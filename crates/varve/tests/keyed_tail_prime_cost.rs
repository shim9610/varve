//! What a generated writer's construction costs on an existing file.
//!
//! The generated writer primes one keyed-tail map per keyed block, and each
//! priming pass walks the whole record directory. The resident index stores an
//! offset, a block id and a commit bit per record rather than the entry, so
//! producing an entry is one positional read of that record's header
//! (`fault_record_entry`) — and a walk that filtered *after* producing them
//! read every record in the file, once per keyed block.
//!
//! Measured at 520 records and four keyed blocks: **2080 faults before, 20
//! after** — four walks of the whole file, against the twenty records that
//! actually carry a key. The open scan appears in neither number: it frames
//! records as it reads forward and never rebuilds an entry, so this cost was
//! never a fraction of the open cost. It was random header reads added to it.
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

/// Distinct keys per block, deliberately tiny. The whole point of the format
/// this mirrors is that its keyed blocks are small and bounded while the
/// non-keyed ones carry the volume.
const KEYS_PER_BLOCK: u32 = 5;
const KEYED_BLOCKS: u64 = 4;

/// Records that carry a key, and are therefore the ones a prime must read.
const KEYED_RECORDS: u64 = KEYS_PER_BLOCK as u64 * KEYED_BLOCKS;

fn build(path: &std::path::Path, bulk: u64) -> Result<()> {
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
    for value in 0..bulk {
        writer.push_bulk(&Bulk { value })?;
    }
    writer.flush()?;
    Ok(())
}

/// Opens a generated writer over a file with `bulk` non-keyed records and
/// returns how many entries the priming rebuilt.
fn prime_faults(directory: &std::path::Path, bulk: u64) -> Result<u64> {
    let path = directory.join(format!("prime-{bulk}.varve"));
    build(&path, bulk)?;

    // The untyped handle does the open work and no priming: no generated
    // wrapper. This is the baseline the primes sit on top of.
    let _ = varve::VarveFile::take_record_entry_faults();
    let untyped = varve::VarveWriter::open(FourKeyedFormat::spec(), &path)?;
    let open_only = varve::VarveFile::take_record_entry_faults();
    drop(untyped);
    assert_eq!(
        open_only, 0,
        "the open scan frames records as it reads and rebuilds no entry; if this \
         moved, the baseline below is measuring something else",
    );

    let _ = varve::VarveFile::take_record_entry_faults();
    let typed = FourKeyedFormat::open_writer(&path)?;
    let faults = varve::VarveFile::take_record_entry_faults();
    drop(typed);
    Ok(faults)
}

/// Priming the keyed-tail maps must cost the keyed records, not the file.
///
/// The primes run whether or not the caller ever performs a keyed operation on
/// the handle — construction builds all four maps, and a writer that only ever
/// appends to `Bulk` pays for every one of them. So what they cost has to be
/// bounded by the keys, which are bounded by the format, rather than by the
/// records, which are bounded by how long the session ran.
///
/// **The instrument is a count of rebuilt entries, and the assertion is that it
/// does not move when the file grows twentyfold.** A ratio against the record
/// count would have passed just as well before this was fixed; only
/// independence from the record count distinguishes the two.
#[test]
fn priming_the_keyed_tails_reads_the_keyed_records_and_not_the_file() -> Result<()> {
    let directory = tempfile::tempdir()?;

    let small = prime_faults(directory.path(), 50)?;
    let large = prime_faults(directory.path(), 1_000)?;

    assert_eq!(
        small, KEYED_RECORDS,
        "priming four maps of {KEYS_PER_BLOCK} keys must rebuild {KEYED_RECORDS} entries",
    );
    assert_eq!(
        large, small,
        "the file grew from 50 to 1,000 non-keyed records and the priming cost \
         moved from {small} to {large}; it is walking the file again",
    );
    Ok(())
}

/// The same defect, on the read path — and this one is paid per call.
///
/// `blocks::<T>()` and its siblings want one block out of the directory. They
/// filtered on `block_id` after producing the entry, which the resident index
/// produces by reading that record's header, so returning five `Channel`s out
/// of a long session read every record in the file.
///
/// Priming is once, at open. This is every call.
#[test]
fn reading_one_block_reads_that_blocks_records_and_not_the_file() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("read-cost.varve");
    build(&path, 1_000)?;

    let reader = FourKeyedFormat::open_reader(&path)?;

    let _ = varve::VarveFile::take_record_entry_faults();
    let channels = reader.channels()?;
    let faults = varve::VarveFile::take_record_entry_faults();

    assert_eq!(channels.len(), KEYS_PER_BLOCK as usize);
    assert_eq!(
        faults,
        KEYS_PER_BLOCK as u64,
        "reading {} channels out of a file of 1,020 records rebuilt {faults} entries",
        channels.len(),
    );
    Ok(())
}

/// The same format with the open digest turned on.
///
/// The digest is what a lazy open reads its per-block tails from, and it is not
/// reachable from the `index:` clause — `with_open_digest_on_flush` is a spec
/// builder. Both create and open use this, so the schema hash agrees.
fn digest_spec() -> varve::FormatSpec {
    let mut spec = FourKeyedFormat::spec();
    spec.index_policy = spec.index_policy.with_open_digest_on_flush(true);
    spec
}

fn build_untyped(path: &std::path::Path, bulk: u64, deletes: bool) -> Result<()> {
    let mut writer = varve::VarveWriter::create(digest_spec(), path)?;
    for index in 0..KEYS_PER_BLOCK {
        writer.push_keyed(&Channel { index, value: 1 })?;
    }
    if deletes {
        // One key deleted and written again, so its newest record sits after
        // its own tombstone; one key deleted and left deleted.
        writer.delete::<Channel>(&2)?;
        writer.push_keyed(&Channel {
            index: 2,
            value: 99,
        })?;
        writer.delete::<Channel>(&4)?;
    }
    for value in 0..bulk {
        writer.push(&Bulk { value })?;
    }
    writer.flush()?;
    Ok(())
}

/// A lazy handle can build its keyed tails, and builds the same ones.
///
/// `key_tail_offsets` used to require a resident directory, which a lazy handle
/// deliberately has none of — so a generated writer, which primes one keyed-tail
/// map per keyed block at construction, failed before the caller did anything.
///
/// The two paths must not merely both work. The resident builder breaks
/// sequence ties with the record's position in the directory, so a chain walk —
/// which yields records newest-first with no position — has to reconstruct that
/// order or the maps can differ on a tie. This asserts the maps are equal, on a
/// file carrying a tombstone whose key is written again afterwards and a
/// tombstone whose key is not.
#[test]
fn a_lazy_handle_builds_the_same_keyed_tails_as_a_resident_one() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("lazy-tails.varve");
    build_untyped(&path, 200, true)?;

    let eager = varve::VarveWriter::open(digest_spec(), &path)?;
    let resident = eager.key_tail_offsets::<Channel>()?;
    drop(eager);

    let (lazy, source) = varve::VarveWriter::open_lazy_with_report(digest_spec(), &path)?;
    assert_eq!(
        source,
        varve::LazyOpenSource::Digest,
        "the test needs the lazy route, not a full scan"
    );
    let chained = lazy.key_tail_offsets::<Channel>()?;

    assert_eq!(
        chained, resident,
        "the chain-walk build and the resident build must agree exactly",
    );
    assert!(!resident.is_empty(), "the fixture must produce some tails");
    Ok(())
}

/// And it costs the keyed records, not the file.
///
/// The instrument is the same entry-fault counter: a chain walk reads the
/// records of one block plus the tombstones, so growing the non-keyed bulk must
/// not move it.
#[test]
fn a_lazy_keyed_tail_build_reads_the_chain_and_not_the_file() -> Result<()> {
    let directory = tempfile::tempdir()?;

    let measure = |bulk: u64| -> Result<u64> {
        let path = directory.path().join(format!("lazy-cost-{bulk}.varve"));
        build_untyped(&path, bulk, false)?;
        let (lazy, source) = varve::VarveWriter::open_lazy_with_report(digest_spec(), &path)?;
        assert_eq!(source, varve::LazyOpenSource::Digest);
        let _ = varve::VarveFile::take_record_entry_faults();
        let tails = lazy.key_tail_offsets::<Channel>()?;
        let faults = varve::VarveFile::take_record_entry_faults();
        assert_eq!(tails.len(), KEYS_PER_BLOCK as usize);
        Ok(faults)
    };

    let small = measure(50)?;
    let large = measure(1_000)?;
    assert_eq!(
        large, small,
        "the file grew from 50 to 1,000 non-keyed records and the chain build \
         moved from {small} to {large} reads; it is not following the chain",
    );
    assert!(
        small <= u64::from(KEYS_PER_BLOCK) * 2,
        "building {KEYS_PER_BLOCK} tails read {small} entries",
    );
    Ok(())
}
