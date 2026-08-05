#![cfg(feature = "high-cardinality-dev")]
//! F-12 — asking a record "is this your key?" built the key to answer.
//!
//! `VarveKeyedBlock::key` clones every key field, and the disk-index verify
//! path called it purely to compare: `if &value.key() != key`. For a `String`
//! key that is a heap allocation and a copy of the key bytes, per `get`, thrown
//! away the moment the comparison succeeds — which is the case a `get` that
//! finds its record always takes.
//!
//! The fix adds `VarveKeyedBlock::key_eq`, which `varve_format!` generates
//! field by field so it borrows. The default implementation on the trait is the
//! old expression, so a hand-written `impl` that predates the method keeps
//! working.
//!
//! **The key type is the instrument.** A `u32`-keyed block cannot see this
//! defect at all — its `key()` is a `Copy` and allocates nothing, which is why
//! every existing keyed allocation test in this workspace is blind to it. So
//! the measurement is `String`-keyed, and the second arm is the same shape
//! keyed by `u32`: the gap between them is the key clone, and it is the whole
//! of what this fix removes.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use varve::{DiskIndexOptions, varve_format};

struct CountingAllocator;

std::thread_local! {
    /// Allocation calls on this thread since the window opened. Per-thread
    /// because `cargo test` runs test functions concurrently and a
    /// process-wide counter would be counting the neighbours.
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
    pub format StringKeyFormat {
        magic: b"SKEY";
        version: 1;
        blocks {
            variable Doc(id = 1, key = [name], key_index = disk) {
                name: String,
                value: u32,
            }
        }
    }
}

varve_format! {
    pub format ScalarKeyFormat {
        magic: b"UKEY";
        version: 1;
        blocks {
            variable Row(id = 1, key = [id], key_index = disk) {
                id: u32,
                value: u32,
            }
        }
    }
}

const RECORDS: u32 = 500;
const RECORDS_U64: u64 = RECORDS as u64;

fn options() -> DiskIndexOptions {
    DiskIndexOptions {
        cache_bytes: 1024 * 1024,
        ..DiskIndexOptions::default()
    }
}

/// A label long enough that its clone cannot be a small-string optimisation
/// living in the `String` header — `String` has no such optimisation in std,
/// but a 48-byte label also makes the allocation unmistakably payload-sized.
fn label(ordinal: u32) -> String {
    format!("document-key-{ordinal:0>34}")
}

/// Allocations issued by `RECORDS` successful `get`s on a `String`-keyed block.
fn string_key_get_allocations() -> varve::Result<u64> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("string.varve");
    let mut writer = StringKeyFormat::create_indexed_writer(&path, options())?;
    for ordinal in 0..RECORDS {
        writer.push_doc(&Doc {
            name: label(ordinal),
            value: ordinal,
        })?;
    }
    writer.sync()?;
    drop(writer);

    let reader = StringKeyFormat::open_indexed_reader(&path, options())?;
    // Warm whatever the first lookup builds lazily.
    let _ = reader.get_doc(&label(0))?;
    let keys: Vec<String> = (0..RECORDS).map(label).collect();

    let start = allocations();
    for key in &keys {
        let found = reader.get_doc(key)?;
        assert!(found.is_some(), "the fixture lost a record");
    }
    Ok(allocations() - start)
}

/// The same shape keyed by `u32`, whose `key()` allocates nothing at all.
fn scalar_key_get_allocations() -> varve::Result<u64> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("scalar.varve");
    let mut writer = ScalarKeyFormat::create_indexed_writer(&path, options())?;
    for ordinal in 0..RECORDS {
        writer.push_row(&Row {
            id: ordinal,
            value: ordinal,
        })?;
    }
    writer.sync()?;
    drop(writer);

    let reader = ScalarKeyFormat::open_indexed_reader(&path, options())?;
    let _ = reader.get_row(&0)?;

    let start = allocations();
    for ordinal in 0..RECORDS {
        let found = reader.get_row(&ordinal)?;
        assert!(found.is_some(), "the fixture lost a record");
    }
    Ok(allocations() - start)
}

/// **The cost.** A successful `get` does not allocate to check the key it was
/// given against the key it found.
///
/// Measured on this tree at 500 gets:
///
/// |                      | allocations/get |
/// | ---                  | ---             |
/// | `u32` key            | 10.00           |
/// | `String` key, before | **13.00**       |
/// | `String` key, after  | **12.00**       |
///
/// One allocation per `get` disappears, and it is the key clone. Two of the
/// three-allocation gap over the scalar arm remain and are NOT this fix: the
/// `String` the decoded record owns — that record is decoded either way, it is
/// what `get` returns — and the encoded lookup key the index probe builds.
/// Stating the assertion as "equal to the scalar arm" would have failed
/// against correct code.
///
/// The ceiling is a literal captured from the **pre-change build**. A baseline
/// taken inside the post-fix run proves nothing, and that is the pattern that
/// left six proofs in the 2026-08-05 audit round unable to discriminate.
#[test]
fn a_successful_get_does_not_build_the_key_to_compare_it() -> varve::Result<()> {
    let string_key = string_key_get_allocations()?;
    let scalar_key = scalar_key_get_allocations()?;
    let string_per_get = string_key as f64 / RECORDS_U64 as f64;
    let scalar_per_get = scalar_key as f64 / RECORDS_U64 as f64;
    println!(
        "(keyed get) {RECORDS} gets: string_key={string_per_get:.2}/get \
         scalar_key={scalar_per_get:.2}/get"
    );

    let extra = string_per_get - scalar_per_get;
    assert!(
        extra < 3.0,
        "a String-keyed get allocated {extra:.2} times per get more than a u32-keyed one; \
         before this fix it was 3.00, of which one was the key built only to be compared \
         against the key the caller already held"
    );
    // And the scalar arm must not move: it never had the clone to lose, so a
    // "fix" that traded one allocation for another somewhere shared shows up
    // here rather than hiding in the gap.
    assert!(
        scalar_per_get <= 10.0,
        "a u32-keyed get allocated {scalar_per_get:.2} times per get; it was 10.00 before this \
         fix and has no key clone to remove"
    );
    Ok(())
}

/// **The cost.** Staging a record for append does not allocate per record.
///
/// `prepare_stream_record` built a fresh `Vec` for every record — reserve,
/// three `extend_from_slice`, write, drop — so continuous append took one
/// allocation and one full payload copy per record on the path the project's
/// design policy names as sacred. The batch path was worse: that per-record
/// `Vec` was then copied *again* into the chunk buffer and dropped.
///
/// Both now stage straight into a buffer that outlives the record: the writer
/// keeps one for single puts and clears it per record, and the batch loop
/// appends record after record into the chunk it is about to write.
///
/// Measured on this tree at 500 records:
///
/// |                | allocations/record |
/// | ---            | ---                |
/// | single, before | 9.16               |
/// | single, after  | **8.16**           |
/// | batch, before  | 15.26              |
/// | batch, after   | **14.26**          |
///
/// Exactly one per record on each path. The ceilings are literals captured
/// from the pre-change build; what remains is other work — the logical encode's
/// buffer, the disk-index row — that these fixes do not claim.
#[test]
fn staging_a_record_for_append_does_not_allocate_per_record() -> varve::Result<()> {
    const N: u32 = 500;
    let directory = tempfile::tempdir()?;

    let single_path = directory.path().join("single.varve");
    let mut writer = ScalarKeyFormat::create_indexed_writer(&single_path, options())?;
    // Warm: the first record pays for the buffer the rest reuse, which is the
    // whole point, and measuring it would hide the effect in an average.
    writer.push_row(&Row { id: 0, value: 0 })?;
    let start = allocations();
    for id in 1..=N {
        writer.push_row(&Row { id, value: id })?;
    }
    let single = (allocations() - start) as f64 / N as f64;
    writer.sync()?;
    drop(writer);

    let batch_path = directory.path().join("batch.varve");
    let mut writer = ScalarKeyFormat::create_indexed_writer(&batch_path, options())?;
    writer.push_row(&Row { id: 0, value: 0 })?;
    let rows: Vec<Row> = (1..=N).map(|id| Row { id, value: id }).collect();
    let start = allocations();
    writer
        .push_rows(&rows, varve::BatchOptions::default())
        .map_err(|error| error.source)?;
    let batch = (allocations() - start) as f64 / N as f64;
    writer.sync()?;
    drop(writer);
    println!("(staging) single={single:.2}/record batch={batch:.2}/record");

    assert!(
        single < 9.0,
        "a single put allocated {single:.2} times per record; before this fix it was 9.16, of \
         which one was the per-record staging Vec"
    );
    assert!(
        batch < 15.0,
        "a batched put allocated {batch:.2} times per record; before this fix it was 15.26, of \
         which one was the per-record staging Vec that was then copied into the chunk"
    );

    // And the records are all there and readable, so the staging did not lose
    // or reorder anything.
    for path in [&single_path, &batch_path] {
        let reader = ScalarKeyFormat::open_indexed_reader(path, options())?;
        for id in 0..=N {
            assert_eq!(
                reader.get_row(&id)?.expect("record present").value,
                id,
                "record {id} did not survive staging in {path:?}"
            );
        }
    }
    Ok(())
}

/// **The cost.** A run of scalable appends issues no metadata syscall per
/// record.
///
/// `SnapshotFile::with_len` ran an `fstat` to learn the file's physical length
/// — the very fact a `write_all` that just returned `Ok` proves. The scalable
/// append path now rebinds through `with_written_len`, which takes that proof
/// as an argument, exactly as the non-scalable path already did.
///
/// **The seek is still there, deliberately.** The other half of the plan's
/// entry was to shadow the file's offset and skip the `seek` before each
/// write, on the reasoning that a successful write of `bytes` from `eof`
/// leaves the offset at `eof + bytes.len()`, which is the next record's `eof`.
/// Measured against the real offset on this fixture, the shadow was wrong on
/// **every** append, by exactly 16 bytes each time: this handle is not the only
/// writer to the native file, and the other one does not move this handle's
/// offset. Skipping the seek wrote each record 16 bytes early and the file came
/// out short — `NativeTooShort { required: 14542, actual: 14526 }`. The seek is
/// what makes the append independent of that, and it stays until the second
/// writer is part of the accounting.
#[test]
fn a_run_of_scalable_appends_pays_no_per_record_metadata_syscall() -> varve::Result<()> {
    const N: u32 = 200;
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("appends.varve");
    let mut writer = ScalarKeyFormat::create_indexed_writer(&path, options())?;

    // Open and the first record are not what this measures: open binds a
    // snapshot, and the first append is the one that has to find the eof.
    writer.push_row(&Row { id: 0, value: 0 })?;
    let _ = varve::VarveFile::take_snapshot_bounds_fstats();

    for id in 1..=N {
        writer.push_row(&Row { id, value: id })?;
    }
    let fstats = varve::VarveFile::take_snapshot_bounds_fstats();
    println!("(append syscalls) {N} records: snapshot fstats={fstats}");
    assert_eq!(
        fstats, 0,
        "{N} appends ran {fstats} snapshot fstats; the write that returned Ok is the proof \
         they were issued to obtain"
    );

    // Every record readable at its own key, which is what a snapshot bound to
    // the wrong length would break.
    writer.sync()?;
    drop(writer);
    let reader = ScalarKeyFormat::open_indexed_reader(&path, options())?;
    for id in 0..=N {
        assert_eq!(
            reader.get_row(&id)?.expect("record present").value,
            id,
            "record {id} did not survive the witness-bound snapshot"
        );
    }
    Ok(())
}
