#![cfg(feature = "high-cardinality-dev")]
//! One composite disk-index key per ingested record, not two.
//!
//! With `index: keyed_offset_chain` the indexed writer looks the previous
//! record for a key up before appending, so it can write the back-pointer. That
//! lookup builds the composite key `(block_id, key length, canonical key)`;
//! staging the new row then built the identical bytes a second time and the
//! first copy was dropped. This is on the per-record append path, so it is a
//! per-record allocation — the thing policy 3 names.
//!
//! The measurement is an allocation **count**, not a byte total, and that is
//! the point: a "fix" that keeps both builds but clones one into the other
//! moves no bytes and would pass a byte-based assertion.
//!
//! The baselines below are checked-in literals captured from the *pre-change*
//! build, because the pre-change count does not exist in a post-change binary
//! and cannot be derived from anything in the same run. How they were taken is
//! written beside each one.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use varve::{BatchOptions, DiskIndexOptions, varve_format};

struct CountingAllocator;

std::thread_local! {
    /// Allocation *calls* on this thread since the window opened.
    ///
    /// Per-thread because `cargo test` runs test functions concurrently and a
    /// process-wide counter would be counting the neighbours. `realloc` counts
    /// as one allocation, matching what a `Vec` growing does.
    static ALLOCATIONS: Cell<u64> = const { Cell::new(0) };
}

fn count_allocation() {
    let _ = ALLOCATIONS.try_with(|count| count.set(count.get() + 1));
}

unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let pointer = unsafe { System.alloc(layout) };
        if !pointer.is_null() {
            count_allocation();
        }
        pointer
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        unsafe { System.dealloc(pointer, layout) };
    }

    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let replacement = unsafe { System.realloc(pointer, layout, new_size) };
        if !replacement.is_null() {
            count_allocation();
        }
        replacement
    }
}

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

fn allocations() -> u64 {
    ALLOCATIONS.with(Cell::get)
}

varve_format! {
    pub format ChainedKeyFormat {
        magic: b"CKEY";
        version: 1;
        index: keyed_offset_chain;
        blocks {
            variable Frame(id = 1, key = [scan, frame], key_index = disk) {
                scan: u32,
                frame: u32,
                payload: Vec<u8>,
            }
        }
    }
}

varve_format! {
    pub format PlainKeyFormat {
        magic: b"PKEY";
        version: 1;
        blocks {
            variable Row(id = 1, key = [scan, frame], key_index = disk) {
                scan: u32,
                frame: u32,
                payload: Vec<u8>,
            }
        }
    }
}

const RECORDS: u32 = 2_000;
/// The same count as `RECORDS`, in the allocation counter's own type.
const RECORDS_U64: u64 = RECORDS as u64;

fn options() -> DiskIndexOptions {
    DiskIndexOptions {
        cache_bytes: 1024 * 1024,
        ..DiskIndexOptions::default()
    }
}

/// Allocations issued while pushing `RECORDS` records one at a time through the
/// keyed-offset-chain writer.
fn chained_single_push_allocations() -> varve::Result<u64> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("chained.varve");
    let mut writer = ChainedKeyFormat::create_indexed_writer(&path, options())?;
    // Warm: the first record pays for whatever the sidecar sets up lazily.
    writer.push_frame(&Frame {
        scan: 0,
        frame: 0,
        payload: vec![0u8; 8],
    })?;
    let start = allocations();
    for ordinal in 1..=RECORDS {
        writer.push_frame(&Frame {
            scan: ordinal / 100,
            frame: ordinal,
            payload: vec![0u8; 8],
        })?;
    }
    let measured = allocations() - start;
    writer.sync()?;
    Ok(measured)
}

/// The same shape with the chain policy off — the default. Nothing here should
/// move at all, in either direction.
fn plain_single_push_allocations() -> varve::Result<u64> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("plain.varve");
    let mut writer = PlainKeyFormat::create_indexed_writer(&path, options())?;
    writer.push_row(&Row {
        scan: 0,
        frame: 0,
        payload: vec![0u8; 8],
    })?;
    let start = allocations();
    for ordinal in 1..=RECORDS {
        writer.push_row(&Row {
            scan: ordinal / 100,
            frame: ordinal,
            payload: vec![0u8; 8],
        })?;
    }
    let measured = allocations() - start;
    writer.sync()?;
    Ok(measured)
}

/// Pre-change allocation count for `chained_single_push_allocations()`, taken
/// on this machine by running this test file against the unmodified
/// `disk_index.rs`/`indexed.rs` (`git stash`, `cargo test`, `git stash pop`).
/// Recorded as a literal because a post-change binary cannot produce it.
const CHAINED_BEFORE: u64 = 52_334;

/// Pre-change count for `plain_single_push_allocations()`, taken the same way
/// and **against this fix alone**: 28,333, in a worktree that carried only the
/// composite-key change.
///
/// The merged tree measures 26,333, exactly `RECORDS` fewer, and the reason is
/// not this fix. F-52 removed the `logical_payload.to_vec()` in
/// `uncompressed_user_payload`, and the scalable prepare path
/// (`file.rs:11460`) goes through it once per record — so a one-allocation
/// saving times 2,000 records lands on this same measurement. The two fixes
/// were built in separate worktrees, which is why neither baseline could see
/// the other.
///
/// The assertion below was therefore rewritten from "must not move in either
/// direction" to "must not regress". The direction that matters is up: the
/// composite-key change must not add an allocation to the policy-off path, and
/// a drop that another accepted fix accounts for arithmetically is not a
/// failure. The floor guard keeps a collapsed workload from passing.
const PLAIN_BEFORE: u64 = 28_333;

/// Allocation counts wobble a little with redb's own bookkeeping across batch
/// boundaries, so the assertions carry this much slack. It is far below the
/// `RECORDS` allocations the fix removes.
const SLACK: u64 = RECORDS_U64 / 8;

#[test]
fn the_keyed_chain_builds_one_composite_key_per_record() -> varve::Result<()> {
    let measured = chained_single_push_allocations()?;
    assert!(
        measured + RECORDS_U64 <= CHAINED_BEFORE + SLACK,
        "{measured} allocations for {RECORDS} keyed-chain records; the pre-change \
         count was {CHAINED_BEFORE}, so at least {RECORDS} fewer were expected"
    );
    // Guards the direction: a measurement that collapsed (a writer that stopped
    // doing the work) would satisfy the bound above for the wrong reason.
    assert!(
        measured >= RECORDS_U64,
        "only {measured} allocations for {RECORDS} records; the workload did not run"
    );
    Ok(())
}

#[test]
fn the_default_policy_path_does_not_move() -> varve::Result<()> {
    let measured = plain_single_push_allocations()?;
    assert!(
        measured <= PLAIN_BEFORE + SLACK,
        "{measured} allocations for {RECORDS} records with the chain policy off; \
         the pre-change count was {PLAIN_BEFORE}. With the policy off no composite \
         key is built early, so this path must not have grown"
    );
    // A measurement that collapsed - a writer that stopped doing the work -
    // would satisfy the bound above for the wrong reason.
    assert!(
        measured >= RECORDS_U64,
        "only {measured} allocations for {RECORDS} records; the workload did not run"
    );
    Ok(())
}

/// The correctness net, and the one that catches the real hazard: handing the
/// *canonical* key where a composite is required would remap every row without
/// failing. Values written through the keyed-chain writer must still be
/// readable by their keys, and a deleted key must still read as absent.
#[test]
fn the_keyed_chain_still_stores_rows_under_the_key_they_are_read_by() -> varve::Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("roundtrip.varve");
    let mut writer = ChainedKeyFormat::create_indexed_writer(&path, options())?;
    for ordinal in 0..64u32 {
        writer.push_frame(&Frame {
            scan: ordinal / 8,
            frame: ordinal,
            payload: ordinal.to_le_bytes().to_vec(),
        })?;
    }
    // Overwrite one key: the second row must shadow the first, which is exactly
    // the property a wrong composite key would break silently.
    writer.push_frame(&Frame {
        scan: 1,
        frame: 9,
        payload: b"latest".to_vec(),
    })?;
    // ... and a batch push, which threads the key through a different call path.
    writer
        .push_frames(
            (64..128u32).map(|ordinal| Frame {
                scan: ordinal / 8,
                frame: ordinal,
                payload: ordinal.to_le_bytes().to_vec(),
            }),
            BatchOptions::default(),
        )
        .map_err(|error| error.source)?;
    writer.delete_frame(&(0, 3))?;
    writer.sync()?;
    drop(writer);

    let reader = ChainedKeyFormat::open_indexed_reader(&path, options())?;
    for ordinal in [0u32, 1, 9, 63, 64, 127] {
        let value = reader
            .get_frame(&(ordinal / 8, ordinal))?
            .expect("every written key is readable");
        if ordinal == 9 {
            assert_eq!(value.payload, b"latest");
        } else {
            assert_eq!(value.payload, ordinal.to_le_bytes());
        }
    }
    assert!(reader.get_frame(&(0, 3))?.is_none(), "the delete was lost");
    assert!(
        reader.get_frame(&(0, 200))?.is_none(),
        "a key that was never written was found"
    );
    Ok(())
}
