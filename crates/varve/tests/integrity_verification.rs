//! When record integrity is verified, and what must never move with it.
//!
//! Open used to verify every record while scanning, which made open read every
//! payload byte in the file — measured at 7,096 ms against 3,621 ms on one
//! 2,052 MB file of 50,000 records. The check is not removed by this policy; it
//! is left where it already was, on the read that returns the record.
//!
//! Two properties have to hold together, and this file exists because losing
//! either one silently is exactly how the matrix version of this defect was
//! nearly reintroduced in 0.5.0:
//!
//! 1. `OnDemand` still detects every corruption — on the read, and on
//!    `verify_all()`.
//! 2. Recovery verifies regardless of the policy, because a checksum mismatch
//!    is the evidence `truncate_tail` truncates on.

// Every case here declares `integrity: crc32`, so without the `integrity`
// feature the format is refused at create with `IntegrityFeatureDisabled`.
#![cfg(feature = "integrity")]

use std::fs::OpenOptions;
use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use varve::{Error, IntegrityVerification, ReadLimits, VarveBlock, VarveFile, varve_format};

varve_format! {
    pub format Guarded {
        magic: b"IVFY";
        version: 1;
        endian: little;
        schema_hash: computed;
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
        index: [scan_on_open];
        commit: transaction_marker(on_flush);
        integrity: crc32;
        recovery: truncate_tail;
        blocks {
            variable Line(id = 1) {
                payload: Vec<u8>,
            }
        }
    }
}

static NEXT: AtomicU64 = AtomicU64::new(0);

fn temp_path() -> PathBuf {
    let id = NEXT.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("varve-ivfy-{}-{id}.varve", std::process::id()))
}

fn write_lines(path: &Path, count: usize) {
    let mut writer = VarveFile::create(Guarded::spec(), path).expect("create");
    for i in 0..count {
        writer
            .push(&Line {
                payload: vec![(i % 251) as u8; 64],
            })
            .expect("push");
    }
    // transaction_marker(on_flush): without a marker every record reads as
    // uncommitted and the index truncates to empty.
    writer.flush().expect("flush");
    writer.sync().expect("sync");
}

/// Flips one bit inside the payload of the `nth` user record.
fn corrupt_payload(path: &Path, nth: usize) -> u64 {
    let reader = VarveFile::open_readonly(Guarded::spec(), path).expect("open to locate");
    let offset = reader
        .index_entries()
        .iter()
        .filter(|entry| entry.block_id == Line::ID)
        .nth(nth)
        .expect("record exists")
        .payload_offset;
    drop(reader);

    let mut file = OpenOptions::new().write(true).open(path).expect("open rw");
    file.seek(SeekFrom::Start(offset)).expect("seek");
    file.write_all(&[0xFF]).expect("write");
    file.sync_all().expect("sync");
    offset
}

fn cleanup(path: &Path) {
    let _ = std::fs::remove_file(path);
    let mut lock = path.as_os_str().to_os_string();
    lock.push(".lock");
    let _ = std::fs::remove_file(PathBuf::from(lock));
}

#[test]
fn the_default_is_on_demand() {
    // The one line that decides how much work an undeclared open does.
    assert_eq!(
        IntegrityVerification::DEFAULT,
        IntegrityVerification::OnDemand
    );
    assert_eq!(
        ReadLimits::MISSING.effective_integrity_verification(),
        IntegrityVerification::OnDemand,
        "an undeclared open must not verify the whole file"
    );
    assert_eq!(
        ReadLimits::MISSING
            .with_integrity_verification(IntegrityVerification::AtOpen)
            .effective_integrity_verification(),
        IntegrityVerification::AtOpen
    );
}

#[test]
fn at_open_still_refuses_a_corrupt_file_at_open() {
    let path = temp_path();
    write_lines(&path, 40);
    corrupt_payload(&path, 7);

    let spec = Guarded::spec().with_read_limits(
        ReadLimits::MISSING.with_integrity_verification(IntegrityVerification::AtOpen),
    );
    let result = VarveFile::open_readonly(spec, &path);
    assert!(
        matches!(result, Err(Error::ChecksumMismatch { .. })),
        "AtOpen must announce damage while opening, got {result:?}"
    );
    cleanup(&path);
}

#[test]
fn on_demand_opens_the_same_corrupt_file_and_still_refuses_the_bad_record() {
    // This is the whole trade: the file opens, every intact record reads, and
    // the damaged one is refused when it is read. Nothing is accepted that
    // AtOpen would have rejected -- only the moment of the refusal moves.
    let path = temp_path();
    write_lines(&path, 40);
    corrupt_payload(&path, 7);

    let reader = VarveFile::open_readonly(Guarded::spec(), &path)
        .expect("OnDemand must open a file with a damaged record");

    let blocks = reader.blocks::<Line>().expect("blocks");
    let mut refused = 0;
    let mut read_ok = 0;
    for index in 0..blocks.len() {
        match blocks.get(index) {
            Ok(Some(_)) => read_ok += 1,
            Err(Error::ChecksumMismatch { .. }) => refused += 1,
            other => panic!("unexpected result at {index}: {other:?}"),
        }
    }
    assert_eq!(refused, 1, "exactly the damaged record must be refused");
    assert_eq!(read_ok, 39);
    cleanup(&path);
}

#[test]
fn verify_all_recovers_the_announcement_that_on_demand_defers() {
    // OnDemand defers the announcement; it must not lose it. This is the
    // counterpart of `verify_matrix_metadata()`.
    let path = temp_path();
    write_lines(&path, 40);

    let clean = VarveFile::open_readonly(Guarded::spec(), &path).expect("open clean");
    let verified = clean.verify_all().expect("clean file verifies");
    assert!(
        verified >= 40,
        "verify_all must visit every record, saw {verified}"
    );
    drop(clean);

    corrupt_payload(&path, 3);
    let damaged = VarveFile::open_readonly(Guarded::spec(), &path).expect("open damaged");
    assert!(
        matches!(damaged.verify_all(), Err(Error::ChecksumMismatch { .. })),
        "verify_all must find what OnDemand did not announce at open"
    );
    cleanup(&path);
}

#[test]
fn an_intact_file_reads_identically_under_both_policies() {
    let path = temp_path();
    write_lines(&path, 64);

    let lazy = VarveFile::open_readonly(Guarded::spec(), &path).expect("open lazy");
    let eager = VarveFile::open_readonly(
        Guarded::spec().with_read_limits(
            ReadLimits::MISSING.with_integrity_verification(IntegrityVerification::AtOpen),
        ),
        &path,
    )
    .expect("open eager");

    assert_eq!(lazy.index_entries().len(), eager.index_entries().len());
    assert_eq!(
        lazy.index_entries(),
        eager.index_entries(),
        "the policy must change when bytes are checked, not what is indexed"
    );
    cleanup(&path);
}
