//! What a generated writer's construction costs on an existing file.
//!
//! The generated writer primes one keyed-tail map per keyed block, and each
//! priming pass walks the whole record directory. The resident index stores an
//! offset and a commit bit per record rather than the entry, so *walking* it is
//! not free: every entry is rebuilt by one positional read of that record's
//! header (`fault_record_entry`).
//!
//! Measured at 520 records and four keyed blocks: the untyped open faults
//! **0** entries and the generated writer faults **2080**, which is four walks
//! of the whole file. The open scan does not appear in that number at all —
//! it frames records as it reads forward and never rebuilds an entry — so the
//! primes are not a fraction of the open cost, they are 4N *random* header
//! reads added to a single sequential pass.
//!
//! The instrument is `take_record_entry_faults`, which counts exactly those
//! rebuilds. `take_open_scan_bytes` cannot see this: it charges the open scan,
//! and the priming happens after the scan has finished.

#![cfg(feature = "scalable-fault-injection")]

use varve::{Result, varve_format};

varve_format! {
    pub format FourKeyedFormat {
        magic: b"4KEYED";
        version: 1;
        index: keyed_offset_chain;
        blocks {
            variable Channel(id = 10, key = [index]) {
                index: u32,
                value: u32,
            }
            variable Partition(id = 11, key = [partition_id]) {
                partition_id: u32,
                value: u32,
            }
            variable ProtocolStep(id = 12, key = [step]) {
                step: u32,
                value: u32,
            }
            variable Outcome(id = 13, key = [slot]) {
                slot: u32,
                value: u32,
            }
            fixed Bulk(id = 20) {
                value: u64,
            }
        }
    }
}

/// The distinct-key counts are deliberately tiny and the record count is not:
/// the cost being measured is the directory walk, which is charged per record
/// in the file rather than per key in the map.
const KEYS_PER_BLOCK: u32 = 5;
const BULK_RECORDS: u64 = 500;

fn build(path: &std::path::Path) -> Result<u64> {
    let mut writer = FourKeyedFormat::create_writer(path)?;
    for index in 0..KEYS_PER_BLOCK {
        writer.push_channel(&Channel { index, value: 0 })?;
        writer.push_partition(&Partition {
            partition_id: index,
            value: 0,
        })?;
        writer.push_protocol_step(&ProtocolStep {
            step: index,
            value: 0,
        })?;
        writer.push_outcome(&Outcome {
            slot: index,
            value: 0,
        })?;
    }
    for value in 0..BULK_RECORDS {
        writer.push_bulk(&Bulk { value })?;
    }
    writer.flush()?;
    let records = u64::from(KEYS_PER_BLOCK) * 4 + BULK_RECORDS;
    Ok(records)
}

/// Reopening a generated writer reads every record header once per keyed block.
///
/// The primes run whether or not the caller ever performs a keyed operation on
/// the handle: construction builds all four maps, and a writer that only ever
/// appends to the non-keyed blocks pays for every one of them.
///
/// The assertion is a ratio against the record count rather than an absolute
/// number, because what is under test is that the primes scale with the
/// *records* in the file while the maps they build hold `KEYS_PER_BLOCK`
/// entries each — 5 here, against 520 records walked four times.
#[test]
fn reopening_a_generated_writer_walks_the_directory_once_per_keyed_block() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("prime-cost.varve");
    let records = build(&path)?;

    // The untyped handle does the open work and nothing else: no generated
    // wrapper, so no priming. This is the baseline the primes sit on top of.
    let _ = varve::VarveFile::take_record_entry_faults();
    let untyped = varve::VarveWriter::open(FourKeyedFormat::spec(), &path)?;
    let open_only = varve::VarveFile::take_record_entry_faults();
    drop(untyped);

    // The same open, through the generated writer, which primes four maps.
    let _ = varve::VarveFile::take_record_entry_faults();
    let typed = FourKeyedFormat::open_writer(&path)?;
    let with_primes = varve::VarveFile::take_record_entry_faults();
    drop(typed);

    let primes = with_primes - open_only;
    assert_eq!(
        primes,
        records * 4,
        "four keyed blocks must cost four directory walks of {records} records; \
         open alone faulted {open_only}, the generated writer {with_primes}",
    );
    Ok(())
}
