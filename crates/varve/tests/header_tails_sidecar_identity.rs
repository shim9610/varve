//! What a disk-index sidecar may and may not notice about the header.
//!
//! A sidecar binds itself to its primary by hashing the primary's leading
//! bytes, twice over: `primary_identity` hashes the whole header into the
//! identity it stores and compares on every open, and `primary_generation`
//! CRCs the first 4 KiB. Both were written when the header was written exactly
//! once at create, and both said so out loud.
//!
//! `IndexPolicy::header_tails` reserves a region *inside* that header which a
//! commit rewrites in place. Without narrowing the two windows, one commit
//! would make every published sidecar for that file permanently unopenable —
//! the sidecar is not stale, the primary is not damaged, and the check would
//! refuse anyway.
//!
//! The narrowing has to be exactly one region wide, so this file asserts both
//! sides of it: a changed region payload is accepted, and a changed byte just
//! *past* the header — still inside the 4 KiB generation window — is still
//! refused. The second assertion is the one that would catch a fix that simply
//! stopped checking.

#![cfg(all(feature = "integrity", feature = "high-cardinality-dev"))]

use std::path::Path;

use varve::{DiskIndexOptions, varve_format};

varve_format! {
    pub format TailedIndexFormat {
        magic: b"HTSIDE";
        version: 1;
        schema_hash: computed;
        integrity: crc32;
        index: [keyed_offset_chain, header_tails];
        blocks {
            variable Frame(id = 1, key = [scan], key_index = disk) {
                scan: u32,
                payload: Vec<u8>,
            }
        }
    }
}

/// Where the `VBTT` block starts within the file.
///
/// A byte search rather than a re-derivation of the header layout: the point is
/// to find the bytes the writer put on disk.
fn region_offset(bytes: &[u8]) -> usize {
    bytes
        .windows(4)
        .position(|window| window == b"VBTT")
        .filter(|offset| *offset < 512)
        .expect("the format declares the region, so the file carries it")
}

fn build(path: &Path) -> varve::Result<()> {
    let mut writer = TailedIndexFormat::create_indexed_writer(path, DiskIndexOptions::default())?;
    for scan in 0..64u32 {
        writer.push_frame(&Frame {
            scan,
            payload: scan.to_le_bytes().to_vec(),
        })?;
    }
    writer.sync()?;
    Ok(())
}

/// Opens the indexed reader and reads one key back, so a "success" here means
/// the sidecar was accepted and used rather than merely that a handle opened.
fn reopen(path: &Path) -> varve::Result<Option<Vec<u8>>> {
    let reader = TailedIndexFormat::open_indexed_reader(path, DiskIndexOptions::default())?;
    Ok(reader.get_frame(&7)?.map(|frame| frame.payload))
}

#[test]
fn a_rewritten_tail_region_does_not_invalidate_a_published_sidecar() -> varve::Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("tailed.varve");
    build(&path)?;

    // Baseline: the sidecar is there and is accepted.
    assert!(
        varve::disk_index_sidecar_path(&path).exists(),
        "the fixture must actually publish a sidecar, or this test proves nothing",
    );
    assert_eq!(reopen(&path)?, Some(7u32.to_le_bytes().to_vec()));

    // Change the region's payload the way a commit will. Every other byte of
    // the file is untouched, so this is exactly the difference the two windows
    // must stop seeing.
    let original = std::fs::read(&path)?;
    let offset = region_offset(&original);
    let mut rewritten = original.clone();
    for byte in rewritten
        .get_mut(offset + 8..offset + 8 + 32)
        .expect("the region has a payload")
    {
        *byte ^= 0xFF;
    }
    assert_ne!(rewritten, original);
    std::fs::write(&path, &rewritten)?;

    assert_eq!(
        reopen(&path)?,
        Some(7u32.to_le_bytes().to_vec()),
        "a rewritten tail region must not invalidate the sidecar",
    );
    Ok(())
}

#[test]
fn a_change_outside_the_region_is_still_refused() -> varve::Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("tailed.varve");
    build(&path)?;
    assert_eq!(reopen(&path)?, Some(7u32.to_le_bytes().to_vec()));

    let original = std::fs::read(&path)?;
    let offset = region_offset(&original);
    // The first byte after the header — inside the 4 KiB generation window, and
    // one byte past the region. If the fix were "stop hashing the header" this
    // would still pass; if it were "stop hashing the window" it would not.
    let after_header = {
        let declared =
            u32::from_le_bytes(original[offset + 4..offset + 8].try_into().unwrap()) as usize;
        offset + 8 + declared
    };
    assert!(
        after_header < 4096 && after_header < original.len(),
        "the fixture must put the first record inside the generation window",
    );

    let mut damaged = original.clone();
    damaged[after_header] ^= 0xFF;
    std::fs::write(&path, &damaged)?;
    assert!(
        reopen(&path).is_err(),
        "a byte past the header still moves the generation witness",
    );

    // And the region's own framing is not excused either: its declared length
    // is what `append_log_start` and every record offset ride on.
    let mut relengthed = original.clone();
    relengthed[offset + 4] = relengthed[offset + 4].wrapping_add(1);
    std::fs::write(&path, &relengthed)?;
    assert!(
        reopen(&path).is_err(),
        "a region that changed length must not open",
    );
    Ok(())
}
