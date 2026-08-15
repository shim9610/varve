//! A follow is bounded by the same resident ceilings an open is.
//!
//! The scan charges `Records` and `IndexBytes` per entry into the buffer it is
//! filling, and `follow` hands it a fresh buffer — so that charge bounds one
//! run, not the handle. Without a charge against the handle's own total, a
//! handle following a growing file walks past a ceiling that a fresh open of
//! the same file refuses, and ends up holding an index its own `record_map`
//! will not walk.
//!
//! Measured before the fix, with `records: 30`: a handle at 21 entries followed
//! four times and every follow succeeded, while every fresh open answered
//! `LimitExceeded { resource: "record count", actual: 31, limit: 30 }`.
//!
//! `ScanBytes` is deliberately exempt and is not what this is about — that
//! exemption is why a follow costs the tail rather than the file, and
//! `reader_follow.rs` pins it.

#![cfg(all(feature = "integrity", feature = "scalable-fault-injection"))]
use varve::{VarveBlock, varve_format};
#[derive(Clone, Debug, PartialEq, VarveBlock)]
#[varve(id = 10, version = 1, kind = "fixed")]
struct Reading {
    value: u64,
}
varve_format! {
    pub struct L { magic: b"VLIMIT01"; version: 1;
        limits { file_len: 8_589_934_592; records: 4_000_000; index_bytes: 536_870_912;
                 scan_bytes: 8_589_934_592; record_payload: 67_108_864;
                 logical_payload: 268_435_456; materialized_bytes: 1_073_741_824;
                 segments: 4_000_000; matrix_dimension: 16_000_000; matrix_cells: 16_000_000;
                 matrix_bitmap: 64_000_000; matrix_crc: 128_000_000; matrix_metadata: 268_435_456;
                 matrix_slot_region: 8_589_934_592; sidecar: 268_435_456; mmap: 8_589_934_592; }
        endian: little; schema_hash: computed; integrity: crc32;
        index: block_offset_chain; commit: transaction_marker(on_flush);
        blocks: [Reading]; }
}
fn n(f: &varve::VarveFile) -> varve::Result<usize> {
    let mut b = Vec::new();
    let mut w = f.record_map(&mut b)?;
    let mut c = 0;
    while w.advance()?.is_some() {
        c += 1
    }
    Ok(c)
}
#[test]
fn a_follow_is_refused_by_the_ceiling_that_refuses_an_open() -> varve::Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("ceiling.varve");
    let mut writer = L::spec().create(&path)?;
    for value in 0..20 {
        writer.push(&Reading { value })?;
    }
    writer.flush()?;

    // Only the reader is tightened; the writer keeps the declared ceiling, so
    // the file legitimately grows past what this reader may hold.
    let tight = L::spec().with_read_limits(L::spec().read_limits.with_max_records(30));
    let mut reader = varve::VarveFile::open_readonly(tight, &path)?;
    let held = n(&reader)?;
    assert!(
        held < 30,
        "the fixture must start inside the ceiling: {held}"
    );

    for value in 100..120u64 {
        writer.push(&Reading { value })?;
    }
    writer.flush()?;

    // A fresh open refuses. The follow must reach the same verdict.
    assert!(
        matches!(
            varve::VarveFile::open_readonly(tight, &path),
            Err(varve::Error::LimitExceeded { .. })
        ),
        "the fixture must have grown past the ceiling",
    );
    assert!(
        matches!(reader.follow(), Err(varve::Error::LimitExceeded { .. })),
        "a follow past the ceiling must be refused, not granted",
    );

    // And the refusal leaves the handle exactly as it was: charging after
    // installing left entries in the index that the snapshot did not reach.
    // That now holds for every refusal in `follow`, not only this one — the
    // ceilings, the run's index capacity and each entry's physical end are all
    // settled before the first entry is installed, so nothing in the install
    // loop can fail partway through it.
    assert_eq!(n(&reader)?, held);
    assert!(reader.blocks::<Reading>().is_ok());
    Ok(())
}
