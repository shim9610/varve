//! A follow reaches the same verdict on a damaged tail that an open reaches.
//!
//! Whether a record whose checksum fails is a recoverable tail the walk stops
//! at, or a hard `ChecksumMismatch`, is decided by `checksum_boundary` — which
//! is the last commit end the scan has seen. A scan of the whole file has seen
//! the last marker by the time it reaches anything after it. A *range* that
//! starts after that marker had not, so it errored where the open it is
//! supposed to agree with returned `Ok`.
//!
//! Measured before the fix, under `IntegrityVerification::AtOpen` with the
//! first uncommitted record byte-flipped: open `Ok`, follow
//! `Err(ChecksumMismatch { offset: 818 })`.

#![cfg(all(feature = "integrity", feature = "scalable-fault-injection"))]

use varve::{VarveBlock, varve_format};

#[derive(Clone, Debug, PartialEq, VarveBlock)]
#[varve(id = 10, version = 1, kind = "fixed")]
struct Reading {
    value: u64,
}

varve_format! {
    pub struct C {
        magic: b"VCKB0001";
        version: 1;
        endian: little;
        schema_hash: computed;
        integrity: crc32;
        index: block_offset_chain;
        // Explicit, so `flush` writes no marker and records can reach the file
        // outside the committed prefix without a crash harness.
        commit: transaction_marker(explicit);
        blocks: [Reading];
    }
}

/// Checksums verified at open, which is what makes the boundary matter: without
/// it neither route looks at the payload and both answer the same trivially.
fn at_open() -> varve::FormatSpec {
    C::spec().with_read_limits(
        C::spec()
            .read_limits
            .with_integrity_verification(varve::IntegrityVerification::AtOpen),
    )
}

fn records(file: &varve::VarveFile) -> varve::Result<usize> {
    let mut buffer = Vec::new();
    let mut walk = file.record_map(&mut buffer)?;
    let mut count = 0;
    while walk.advance()?.is_some() {
        count += 1;
    }
    Ok(count)
}

#[test]
fn a_follow_answers_a_damaged_tail_the_way_an_open_does() -> varve::Result<()> {
    let directory = tempfile::tempdir()?;
    let source = directory.path().join("source.varve");

    let mut writer = at_open().create(&source)?;
    for value in 0..10 {
        writer.push(&Reading { value })?;
    }
    writer.commit()?;
    drop(writer);
    let prefix = std::fs::read(&source)?;
    let from = prefix.len();

    // Uncommitted records past that marker: under Explicit, `flush` writes none.
    let mut writer = varve::VarveWriter::open(at_open(), &source)?;
    let first = writer.push_info(&Reading { value: 99 })?;
    for value in 100..105 {
        writer.push(&Reading { value })?;
    }
    writer.flush()?;
    drop(writer);
    let mut full = std::fs::read(&source)?;
    assert_eq!(
        first.record_offset, from as u64,
        "the corruption below must land on the first record past the boundary",
    );
    assert!(full.len() > from, "the tail must have reached the file");

    // Flip a payload byte so that record's checksum fails.
    full[first.payload_offset as usize] ^= 0xFF;

    // A handle whose snapshot stops at the committed prefix, and then the
    // damaged tail arrives in the same object.
    let path = directory.path().join("damaged.varve");
    std::fs::write(&path, &prefix)?;
    let mut reader = varve::VarveFile::open_readonly(at_open(), &path)?;
    let held = records(&reader)?;
    std::fs::write(&path, &full)?;

    // The standard: a fresh open of the damaged file succeeds, stopping at the
    // committed prefix rather than refusing the file.
    let fresh = varve::VarveFile::open_readonly(at_open(), &path)
        .expect("an open stops at the committed prefix rather than refusing");
    assert_eq!(records(&fresh)?, held);

    // The follow must reach the same verdict: nothing new is committed, so `0`
    // — not an error about a record the open was content to stop before.
    let followed = reader.follow();
    assert!(
        matches!(followed, Ok(0)),
        "a follow must not refuse a file its own open accepts, got {followed:?}",
    );
    assert_eq!(records(&reader)?, held);
    Ok(())
}
