//! The file-header extension region, read as a block sequence.
//!
//! What these tests exist to hold: a block whose magic this build does not know
//! is *skipped*, and skipping it correctly means more than not erroring. The
//! append log starts after it, a rewrite writes it back, and a region that
//! cannot be framed is still refused.

use std::path::PathBuf;

use varve::{Error, VarveBlock, varve_format};

#[derive(Clone, Debug, PartialEq, VarveBlock)]
#[varve(id = 1, version = 1, kind = "fixed")]
struct Sample {
    value: u64,
}

varve_format! {
    pub struct PlainFormat {
        magic: b"HDREXT";
        version: 1;
        endian: little;
        blocks: [Sample];
    }
}

/// Bytes before the container marker, and the marker's own width.
const MAGIC_LEN: usize = 6;
const MARKER_LEN: usize = 6;
/// `version(2) + endian(1) + flags(1) + schema_hash(8)`, which is everything
/// between the marker and where the extension length field would sit.
const FIXED_TAIL_LEN: usize = 2 + 1 + 1 + 8;
const V1_HEADER_LEN: usize = MAGIC_LEN + MARKER_LEN + FIXED_TAIL_LEN;

fn temp_path(name: &str) -> PathBuf {
    let mut path = std::env::temp_dir();
    path.push(format!("varve_hdrext_{name}_{}", std::process::id()));
    path
}

fn cleanup(path: &PathBuf) {
    let _ = std::fs::remove_file(path);
}

/// A block with the framing every post-`VCHD` block uses: magic, `u32` length,
/// payload. `XTST` is deliberately not a magic this build knows.
fn unknown_block(payload: &[u8]) -> Vec<u8> {
    let mut block = Vec::new();
    block.extend_from_slice(b"XTST");
    block.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    block.extend_from_slice(payload);
    block
}

/// Rewrite a freshly created `VARVE1` header as a `VARVE2` header carrying
/// `region`, which is what a later release writing an unknown block would
/// produce. Only valid while the file holds no records: it shifts every byte
/// after the header.
fn splice_extension_region(path: &PathBuf, region: &[u8]) -> std::io::Result<()> {
    let original = std::fs::read(path)?;
    assert_eq!(
        original.len(),
        V1_HEADER_LEN,
        "splice assumes a header-only file, so nothing after it moves"
    );
    assert_eq!(&original[MAGIC_LEN..MAGIC_LEN + MARKER_LEN], b"VARVE1");

    let mut spliced = Vec::new();
    spliced.extend_from_slice(&original[..MAGIC_LEN]);
    spliced.extend_from_slice(b"VARVE2");
    spliced.extend_from_slice(&original[MAGIC_LEN + MARKER_LEN..]);
    spliced.extend_from_slice(&(region.len() as u32).to_le_bytes());
    spliced.extend_from_slice(region);
    std::fs::write(path, &spliced)
}

/// The whole point of the walk: a file carrying `XTST` opens, and appends and
/// reads round-trip across it.
///
/// This covers open, which takes its boundary from the header it just read. It
/// does *not* cover `append_log_start_for_file`, which recomputes the boundary
/// and is only reached by the rewrite and segment paths — reverting that fix
/// leaves this test green. `a_rewrite_writes_the_unknown_block_back` is what
/// holds it.
#[test]
fn an_unknown_block_is_skipped_at_open() -> varve::Result<()> {
    let path = temp_path("skip_unknown");
    cleanup(&path);

    {
        let file = PlainFormat::create(&path)?;
        drop(file);
    }
    let block = unknown_block(&[0xAB; 16]);
    splice_extension_region(&path, &block).expect("splice header");

    {
        let mut file = PlainFormat::open(&path)?;
        for value in 0..8u64 {
            file.push(&Sample { value })?;
        }
        file.flush()?;
    }

    let file = PlainFormat::open_readonly(&path)?;
    let blocks = file.blocks::<Sample>()?;
    assert_eq!(blocks.len(), 8);
    for value in 0..8u64 {
        assert_eq!(blocks.get(value as usize)?, Some(Sample { value }));
    }
    drop(file);

    // Still present, and still where it was.
    let bytes = std::fs::read(&path).expect("read back");
    let region_start = V1_HEADER_LEN + 4;
    assert_eq!(&bytes[MAGIC_LEN..MAGIC_LEN + MARKER_LEN], b"VARVE2");
    assert_eq!(&bytes[region_start..region_start + block.len()], &block[..]);

    cleanup(&path);
    Ok(())
}

/// A rewrite regenerating the header from the spec would drop `XTST` and move
/// the append log out from under the index it just wrote.
#[test]
fn a_rewrite_writes_the_unknown_block_back() -> varve::Result<()> {
    let path = temp_path("rewrite_preserves");
    cleanup(&path);

    {
        let file = PlainFormat::create(&path)?;
        drop(file);
    }
    let block = unknown_block(&[0x5C; 12]);
    splice_extension_region(&path, &block).expect("splice header");

    {
        let mut file = PlainFormat::open(&path)?;
        for value in 0..4u64 {
            file.push(&Sample { value })?;
        }
        file.replace_rewrite(1, &Sample { value: 99 })?;
        file.flush()?;
    }

    let bytes = std::fs::read(&path).expect("read back");
    let region_start = V1_HEADER_LEN + 4;
    assert_eq!(
        &bytes[region_start..region_start + block.len()],
        &block[..],
        "the rewrite regenerated the header instead of preserving it"
    );

    let file = PlainFormat::open_readonly(&path)?;
    let blocks = file.blocks::<Sample>()?;
    assert_eq!(blocks.len(), 4);
    assert_eq!(blocks.get(1)?, Some(Sample { value: 99 }));
    assert_eq!(blocks.get(3)?, Some(Sample { value: 3 }));

    cleanup(&path);
    Ok(())
}

/// Skipping an unknown block is forward compatibility; a region that cannot be
/// framed at all is a corrupt header, and the two must not be confused.
#[test]
fn a_region_that_cannot_be_framed_is_refused() {
    let cases: [(&str, Vec<u8>); 4] = [
        // A magic with no length field behind it.
        ("truncated_magic", b"XTST".to_vec()),
        // A length that runs past the end of the region.
        ("overrun", {
            let mut region = b"XTST".to_vec();
            region.extend_from_slice(&64u32.to_le_bytes());
            region.extend_from_slice(&[0; 4]);
            region
        }),
        // Fewer than four bytes left: not even a magic.
        ("trailing_garbage", {
            let mut region = unknown_block(&[1, 2, 3, 4]);
            region.extend_from_slice(&[0xFF; 2]);
            region
        }),
        // The same magic twice, which makes "does this file carry X" ambiguous.
        ("duplicate_magic", {
            let mut region = unknown_block(&[7; 4]);
            region.extend_from_slice(&unknown_block(&[8; 4]));
            region
        }),
    ];

    for (name, region) in cases {
        let path = temp_path(&format!("malformed_{name}"));
        cleanup(&path);
        {
            let file = PlainFormat::create(&path).expect("create");
            drop(file);
        }
        splice_extension_region(&path, &region).expect("splice header");

        match PlainFormat::open_readonly(&path) {
            Err(Error::InvalidCompressionHeader) => {}
            Err(other) => panic!("{name}: expected InvalidCompressionHeader, got {other:?}"),
            Ok(_) => panic!("{name}: a malformed extension region opened"),
        }
        cleanup(&path);
    }
}

/// The region length is a `u32`, so an untrusted header can name one far larger
/// than any real block sequence. Open must refuse it rather than allocate it.
#[test]
fn an_oversized_region_is_refused_without_reading_it() {
    let path = temp_path("oversized");
    cleanup(&path);
    {
        let file = PlainFormat::create(&path).expect("create");
        drop(file);
    }

    let original = std::fs::read(&path).expect("read");
    let mut spliced = Vec::new();
    spliced.extend_from_slice(&original[..MAGIC_LEN]);
    spliced.extend_from_slice(b"VARVE2");
    spliced.extend_from_slice(&original[MAGIC_LEN + MARKER_LEN..]);
    // 4 GiB claimed, nothing behind it.
    spliced.extend_from_slice(&u32::MAX.to_le_bytes());
    std::fs::write(&path, &spliced).expect("write");

    match PlainFormat::open_readonly(&path) {
        Err(Error::InvalidCompressionHeader) => {}
        Err(other) => panic!("expected InvalidCompressionHeader, got {other:?}"),
        Ok(_) => panic!("an oversized extension region opened"),
    }
    cleanup(&path);
}

/// Inertness: a format that declares no extension-bearing option still writes a
/// `VARVE1` header with no region at all, byte for byte as before this walk.
#[test]
fn a_format_without_extensions_still_writes_varve1() -> varve::Result<()> {
    let path = temp_path("inert");
    cleanup(&path);
    {
        let mut file = PlainFormat::create(&path)?;
        file.push(&Sample { value: 1 })?;
        file.flush()?;
    }

    let bytes = std::fs::read(&path).expect("read");
    assert_eq!(&bytes[MAGIC_LEN..MAGIC_LEN + MARKER_LEN], b"VARVE1");

    cleanup(&path);
    Ok(())
}
