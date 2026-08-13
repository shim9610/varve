//! The two halves of this effort, exercised together for the first time.
//!
//! The work started because a consumer format wanted to resume a large,
//! keyed file without a full open scan. That took two changes which until now
//! were only ever tested apart:
//!
//! * **The chain fallback.** `key_tail_offsets` went through the resident
//!   record directory, which a lazy handle deliberately has none of, so it
//!   answered `NoResidentDirectory`. It now rebuilds the map from
//!   `prev_same_key_offset` and the tombstone chain instead, at a cost bounded
//!   by the keyed records rather than by the file
//!   (`keyed_tail_prime_cost.rs`).
//! * **The header tail region.** `index: header_tails` puts each block's newest
//!   record offset in the file header, so a lazy open has somewhere to start
//!   the chain walk from that nothing appended can bury
//!   (`header_tail_region.rs`).
//!
//! Neither file combines them: the first never declares the region and the
//! second has no keyed block. **This one does**, and asserts the thing the
//! effort was for — that a keyed lookup on a lazily opened handle answers what
//! the scanning handle answers, from a starting point the header supplied, at a
//! cost that does not move when the file grows.

#![cfg(all(feature = "integrity", feature = "scalable-fault-injection"))]

use std::collections::HashMap;

use varve::{LazyOpenSource, Result, VarveFile, varve_format};

varve_format! {
    pub format KeyedResumeFormat {
        magic: b"KRESUME";
        version: 1;
        schema_hash: computed;
        integrity: crc32;
        // The region needs the block chain; the keyed lookups need the keyed
        // chain; the region needs a commit marker. All three, declared together
        // for the first time here.
        index: [keyed_offset_chain, header_tails];
        commit: transaction_marker(on_flush);
        blocks {
            variable Channel(id = 10, key = [index]) {
                index: u32,
                value: u32,
            }
            variable Partition(id = 11, key = [partition_id]) {
                partition_id: u32,
                value: u32,
            }
            fixed Bulk(id = 20) {
                value: u64,
            }
        }
    }
}

/// Distinct keys per keyed block, deliberately small: the format this mirrors
/// has a handful of channels and a bulk stream that runs for hours.
const KEYS_PER_BLOCK: u32 = 5;

fn build(path: &std::path::Path, bulk: u64) -> Result<()> {
    let mut writer = KeyedResumeFormat::create_writer(path)?;
    // Two rounds, so each key has an older record behind its newest one and a
    // walk that stopped at the first hit would give a different answer from one
    // that took the newest.
    for round in 0..2u32 {
        for index in 0..KEYS_PER_BLOCK {
            writer.push_channel(&Channel {
                index,
                value: round,
            })?;
            writer.push_partition(&Partition {
                partition_id: index,
                value: round,
            })?;
        }
    }
    for value in 0..bulk {
        writer.push_bulk(&Bulk { value })?;
    }
    writer.flush()?;
    Ok(())
}

/// The keyed maps a handle reports, and how many record entries it rebuilt
/// getting them.
fn keyed_maps(file: &VarveFile) -> Result<(HashMap<u32, u64>, HashMap<u32, u64>)> {
    Ok((
        file.key_tail_offsets::<Channel>()?,
        file.key_tail_offsets::<Partition>()?,
    ))
}

/// The whole point, in one test.
#[test]
fn a_lazy_keyed_resume_answers_what_the_scan_answers() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("resume.varve");
    build(&path, 400)?;

    let (lazy, source) =
        VarveFile::open_readonly_lazy_with_report(KeyedResumeFormat::spec(), &path)?;
    assert_eq!(
        source,
        LazyOpenSource::HeaderTails,
        "the resume must come from the header, not from a scan",
    );

    let scanned = VarveFile::open_readonly(KeyedResumeFormat::spec(), &path)?;
    let (lazy_channels, lazy_partitions) = keyed_maps(&lazy)?;
    let (scanned_channels, scanned_partitions) = keyed_maps(&scanned)?;

    assert_eq!(lazy_channels.len(), KEYS_PER_BLOCK as usize);
    assert_eq!(lazy_channels, scanned_channels);
    assert_eq!(lazy_partitions, scanned_partitions);

    // Not merely equal — the offsets must be the *newest* record per key, which
    // is what the second round exists to make checkable.
    for (key, offset) in &lazy_channels {
        let block: Channel = lazy.read_block_at(*offset)?;
        assert_eq!(block.index, *key);
        assert_eq!(block.value, 1, "the tail must be the second round's record");
    }
    Ok(())
}

/// And the cost of that resume does not move with the file.
///
/// Two numbers, and both matter. The **open** must frame a handful of records
/// whatever the file's size — that is the region's job. The **keyed build**
/// must rebuild entries proportional to the keyed records, not to the bulk —
/// that is the chain fallback's job. Measuring them separately is what keeps a
/// regression in one from hiding behind the other.
#[test]
fn the_resume_cost_is_bounded_by_the_keys_not_by_the_file() -> Result<()> {
    fn measure(directory: &std::path::Path, bulk: u64) -> Result<(u64, u64)> {
        let path = directory.join(format!("cost-{bulk}.varve"));
        build(&path, bulk)?;

        // `records_framed` is a running total, not a take: the delta is the
        // measurement. Reading it as an absolute is how the first version of
        // this test reported 5 and 10 for two identical opens.
        let before = VarveFile::records_framed();
        let (file, source) =
            VarveFile::open_readonly_lazy_with_report(KeyedResumeFormat::spec(), &path)?;
        let framed = VarveFile::records_framed() - before;
        assert_eq!(source, LazyOpenSource::HeaderTails);

        let _ = VarveFile::take_record_entry_faults();
        let _ = keyed_maps(&file)?;
        let faults = VarveFile::take_record_entry_faults();
        Ok((framed, faults))
    }

    let directory = tempfile::tempdir()?;
    let (small_framed, small_faults) = measure(directory.path(), 100)?;
    let (large_framed, large_faults) = measure(directory.path(), 2_000)?;

    assert_eq!(
        small_framed, large_framed,
        "the file grew twentyfold and the open framed {small_framed} then \
         {large_framed} records; the header route is reading the file",
    );
    assert_eq!(
        small_faults, large_faults,
        "the keyed build rebuilt {small_faults} entries then {large_faults}; it \
         is walking the bulk records",
    );
    // Pinned, so a change that starts reading more has to say so here.
    assert!(
        large_framed <= 8,
        "the open frames the commit marker and one record per block tail: {large_framed}",
    );
    Ok(())
}

/// A scanning open of the same file is the comparison that gives those numbers
/// meaning.
#[test]
fn the_scanning_open_really_does_read_the_whole_file() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("scan.varve");
    build(&path, 2_000)?;

    let before = VarveFile::records_framed();
    let scanned = VarveFile::open_readonly(KeyedResumeFormat::spec(), &path)?;
    let framed = VarveFile::records_framed() - before;
    drop(scanned);

    assert!(
        framed >= 2_000,
        "the scan frames every record, which is the cost the header route \
         avoids: {framed}",
    );
    Ok(())
}

/// A tombstone is a block with a tail like any other, and the region carries it.
///
/// Deleting a key appends a tombstone record on the shared tombstone chain, and
/// the keyed rebuild walks *two* chains — the block's own and the tombstone's —
/// because a tombstone is what makes a key's newest record a deletion. So the
/// region has to hand out both entry points; a table missing the tombstone tail
/// would leave a lazily opened handle answering the pre-delete offset for every
/// deleted key, and the next append would chain to a record the delete had
/// already superseded.
///
/// Note what `key_tail_offsets` does *not* do: it does not drop the key. The
/// map is the newest record offset per key and the tombstone is that record —
/// which is exactly what `prev_same_key_offset` has to point at on the next
/// append. So the assertion is that the deleted key's tail *moved onto the
/// tombstone*, not that it disappeared.
///
/// `TOMBSTONE_BLOCK_ID` is one of the ten internal ids the region reserves
/// capacity for; this is the test that a reserved entry is used rather than
/// merely counted.
#[test]
fn a_deleted_key_points_at_its_tombstone_through_the_header_route() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("deleted.varve");
    build(&path, 200)?;

    let before = {
        let (file, source) =
            VarveFile::open_readonly_lazy_with_report(KeyedResumeFormat::spec(), &path)?;
        assert_eq!(source, LazyOpenSource::HeaderTails);
        keyed_maps(&file)?.0
    };

    {
        let mut writer = VarveFile::open(KeyedResumeFormat::spec(), &path)?;
        writer.delete::<Channel>(&1)?;
        writer.delete::<Partition>(&2)?;
        writer.flush()?;
    }

    let (lazy, source) =
        VarveFile::open_readonly_lazy_with_report(KeyedResumeFormat::spec(), &path)?;
    assert_eq!(source, LazyOpenSource::HeaderTails);
    let tombstone_tail = lazy.block_tail_offset(varve::TOMBSTONE_BLOCK_ID).expect(
        "the region must hand out the tombstone chain's entry point, or the \
             keyed rebuild has no way to learn a key was deleted",
    );

    let scanned = VarveFile::open_readonly(KeyedResumeFormat::spec(), &path)?;
    let (lazy_channels, lazy_partitions) = keyed_maps(&lazy)?;
    let (scanned_channels, scanned_partitions) = keyed_maps(&scanned)?;
    assert_eq!(lazy_channels, scanned_channels);
    assert_eq!(lazy_partitions, scanned_partitions);

    // The deleted key moved onto its tombstone; every other key stayed put.
    for key in 0..KEYS_PER_BLOCK {
        let now = lazy_channels[&key];
        if key == 1 {
            assert_ne!(now, before[&key], "channel 1's tail did not move");
            assert!(
                now <= tombstone_tail,
                "channel 1's tail {now} is past the newest tombstone \
                 {tombstone_tail}",
            );
            // And it really is a tombstone, not a Channel record: the two
            // deletes are the only records appended after the build, so the
            // pre-delete tails bound where a Channel record can be.
            let highest_channel = before.values().copied().max().expect("a tail");
            assert!(now > highest_channel);
        } else {
            assert_eq!(now, before[&key], "channel {key}'s tail moved");
        }
    }
    assert_eq!(lazy_channels.len(), KEYS_PER_BLOCK as usize);
    Ok(())
}
