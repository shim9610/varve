//! `verify_all()` and a block declared `resident: false`.
//!
//! A non-resident block's records are deliberately absent from the resident
//! index; the footer chain is the only way back to them (`block_chain`).
//! `verify_all()` walked the index alone, so for such a format it silently
//! omitted every record of that block — and a file whose only appends were
//! non-resident reported `Ok(0)`, which is exactly what an empty file reports.
//!
//! The four assertions here fail for four different wrong implementations:
//! the count (a), damage in a non-resident record (b), damage in a resident
//! record (c) — which fails an implementation that *replaced* the index pass
//! with the chain pass rather than adding to it — and an empty chain (d).

// Every case here declares `integrity: crc32`, so without the `integrity`
// feature the format is refused at create with `IntegrityFeatureDisabled`.
#![cfg(feature = "integrity")]

use std::fs::OpenOptions;
use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use varve::{BlockResidencyDescriptor, Error, FormatSpec, VarveBlock, varve_format};

#[derive(Clone, Debug, PartialEq, VarveBlock)]
#[varve(id = 70, version = 1, kind = "variable")]
struct Line {
    #[varve(field_id = 1)]
    payload: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, VarveBlock)]
#[varve(id = 71, version = 1, kind = "variable")]
struct Summary {
    #[varve(field_id = 1)]
    first_line: u64,
}

varve_format! {
    pub struct ChainVerifyFormat {
        magic: b"CHVFY";
        version: 1;
        limits {
            file_len: 1_073_741_824;
            records: 1_000_000;
            index_bytes: 134_217_728;
            scan_bytes: 1_073_741_824;
            record_payload: 16_777_216;
            logical_payload: 16_777_216;
            materialized_bytes: 268_435_456;
            segments: 1_000_000;
            sidecar: 16_777_216;
            mmap: 1_073_741_824;
        }
        endian: little;
        integrity: crc32;
        index: block_offset_chain;
        commit: transaction_marker(on_flush);
        blocks: [Line, Summary];
    }
}

/// `format.rs` refuses a non-resident block without `block_offset_chain`, and
/// `known-limitations.md` pairs it with a checksum; both are declared above.
const LINES_NON_RESIDENT: &[BlockResidencyDescriptor] = &[BlockResidencyDescriptor {
    block_id: 70,
    resident: false,
}];

/// The handle under test: `Line` is not mirrored in the resident index.
fn lean_spec() -> FormatSpec {
    ChainVerifyFormat::spec().with_block_residency(LINES_NON_RESIDENT)
}

/// The same file read by a spec that mirrors everything. Its `verify_all()` is
/// the reference answer, because for it the resident index alone is the whole
/// file.
fn full_spec() -> FormatSpec {
    ChainVerifyFormat::spec()
}

static NEXT: AtomicU64 = AtomicU64::new(0);

fn temp_path(tag: &str) -> PathBuf {
    let id = NEXT.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "varve-chvfy-{tag}-{}-{id}.varve",
        std::process::id()
    ))
}

fn cleanup(path: &Path) {
    let _ = std::fs::remove_file(path);
    let mut lock = path.as_os_str().to_os_string();
    lock.push(".lock");
    let _ = std::fs::remove_file(PathBuf::from(lock));
}

fn write(path: &Path, lines: u32, summaries: u64) -> varve::Result<()> {
    let mut file = lean_spec().create(path)?;
    for value in 0..lines {
        file.push(&Line {
            payload: vec![(value % 251) as u8; 48],
        })?;
    }
    for first_line in 0..summaries {
        file.push(&Summary { first_line })?;
    }
    file.flush()?;
    file.sync()?;
    Ok(())
}

/// Flips one byte of the `nth` record of `block_id`, located through the spec
/// that keeps everything resident.
fn corrupt_payload(path: &Path, block_id: u32, nth: usize) {
    let offset = {
        let reader = full_spec().open_readonly(path).expect("open to locate");
        reader
            .index_entries()
            .iter()
            .filter(|entry| entry.block_id == block_id)
            .nth(nth)
            .expect("record exists")
            .payload_offset
    };
    let mut file = OpenOptions::new().write(true).open(path).expect("open rw");
    file.seek(SeekFrom::Start(offset)).expect("seek");
    file.write_all(&[0xFF]).expect("write");
    file.sync_all().expect("sync");
}

#[test]
fn verify_all_visits_the_records_of_a_non_resident_block() -> varve::Result<()> {
    let path = temp_path("count");
    cleanup(&path);
    write(&path, 12, 6)?;

    let lean = lean_spec().open_readonly(&path)?;
    let full = full_spec().open_readonly(&path)?;

    // (a) The same bytes must verify to the same count whichever way the
    // records are held. Before the fix the lean handle answered 12 short: its
    // index has the 6 summaries and the internal records, and nothing else.
    let resident_only = lean.index_entries().len();
    assert_eq!(
        lean.index_entries()
            .iter()
            .filter(|entry| entry.block_id == 70)
            .count(),
        0,
        "the fixture is pointless unless Line really is non-resident",
    );
    let verified = lean.verify_all()?;
    assert_eq!(
        verified,
        full.verify_all()?,
        "a non-resident block's records must be verified too",
    );
    assert_eq!(
        verified,
        resident_only + 12,
        "exactly the 12 chained Line records are added to the resident pass",
    );
    assert!(verified >= 18);

    cleanup(&path);
    Ok(())
}

#[test]
fn verify_all_finds_damage_in_a_non_resident_record() -> varve::Result<()> {
    let path = temp_path("damaged_chain");
    cleanup(&path);
    write(&path, 12, 6)?;
    // (b) The assertion that matters: counting chain entries without reading
    // their payloads passes (a) and fails this.
    corrupt_payload(&path, 70, 6);

    let lean = lean_spec().open_readonly(&path)?;
    let result = lean.verify_all();
    assert!(
        matches!(result, Err(Error::ChecksumMismatch { .. })),
        "verify_all must refuse a damaged non-resident record, got {result:?}",
    );

    cleanup(&path);
    Ok(())
}

#[test]
fn verify_all_still_finds_damage_in_a_resident_record() -> varve::Result<()> {
    let path = temp_path("damaged_resident");
    cleanup(&path);
    write(&path, 12, 6)?;
    // (c) The guard against swapping the resident pass out for the chain pass.
    corrupt_payload(&path, 71, 3);

    let lean = lean_spec().open_readonly(&path)?;
    let result = lean.verify_all();
    assert!(
        matches!(result, Err(Error::ChecksumMismatch { .. })),
        "verify_all must keep refusing a damaged resident record, got {result:?}",
    );

    cleanup(&path);
    Ok(())
}

#[test]
fn an_empty_chain_verifies_rather_than_errors() -> varve::Result<()> {
    let path = temp_path("empty_chain");
    cleanup(&path);
    // (d) The block is declared non-resident and holds no records at all: the
    // walk must start from an absent tail and simply add nothing.
    write(&path, 0, 6)?;

    let lean = lean_spec().open_readonly(&path)?;
    let full = full_spec().open_readonly(&path)?;
    assert_eq!(lean.verify_all()?, full.verify_all()?);
    assert_eq!(lean.verify_all()?, lean.index_entries().len());

    cleanup(&path);
    Ok(())
}
