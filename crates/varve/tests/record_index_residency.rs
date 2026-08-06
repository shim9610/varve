//! An open handle must not keep a copy of every record's index entry.
//!
//! What a record's index entry holds — block id, version, flags, sequence,
//! payload extent, checksum, footer chain — is already in the record's header
//! and footer. Keeping it in memory is keeping a second copy of the file, and
//! it is the copy that decides whether a TB-scale file opens: it tracks record
//! count with no ceiling.
//!
//! The handle now keeps a **directory** instead: one 16-byte slot per record
//! (its offset, and the writer's committed bit — the two facts that are *not*
//! in the record), and it rebuilds the entry from the record's own bytes when
//! something asks for it.
//!
//! The discriminator is the **slope across two record counts**, not a single
//! number. A single measurement cannot separate "the entries are gone" from
//! "the fixture got smaller"; two counts give the per-record cost directly, and
//! that is the number the change is about.
//!
//! **Retained fell; peak rose.** Both are measured here, and the second is the
//! part that is not finished. Open still materialises the scanned entries into
//! a `Vec` and the directory adopts them from it, so while the adoption runs
//! both are alive. Measured 2026-08-05 on Linux/ext4, 2,000 -> 20,000 records:
//!
//! | | before the store swap | after |
//! | --- | --- | --- |
//! | retained by the open handle | 213,108 -> 3,407,988 B (177.5 B/record) | 32,116 -> 320,116 B (**16.00 B/record**) |
//! | peak during `open_readonly` | 213,192 -> 3,408,072 B (177.5 B/record) | 245,192 -> 3,728,072 B (193.5 B/record) |
//!
//! The retained figure is exact: 20,000 slots x 16 bytes = 320,000. The `177.5`
//! before it is `size_of::<RecordIndexEntry>()` (104) inflated by the scan
//! `Vec`'s doubling — 32,768 x 104 = 3,407,872 — which is also why peak and
//! retained used to be the same number. Peak now is 3,407,872 + 320,000 to the
//! byte: the scan's `Vec` plus the directory built from it.
//!
//! So this proves the *handle* no longer holds the entries. It does not prove
//! open never materialises them, because open still does; making the scan emit
//! slots directly is the remaining work, and the peak assertion below is a
//! non-regression ceiling rather than a claim of improvement.
//!
//! Deliberately its own test binary: a process gets one `#[global_allocator]`.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::path::{Path, PathBuf};

use varve::{VarveBlock, varve_format};

struct CountingAllocator;

std::thread_local! {
    /// Live bytes attributable to *this* thread, and this thread's high-water
    /// mark since the window opened.
    ///
    /// Per-thread rather than process-global on purpose: `cargo test` runs test
    /// functions on several threads at once, and a process-wide figure would be
    /// measuring whichever test happened to be running beside this one.
    static LIVE_BYTES: Cell<isize> = const { Cell::new(0) };
    static PEAK_BYTES: Cell<isize> = const { Cell::new(0) };
    /// Every byte this thread has ever asked for, never decremented. Live and
    /// peak both go back down when a buffer is freed, so neither can tell "one
    /// allocation reused ten times" from "ten allocations freed in turn" — and
    /// that difference is exactly what a caller-supplied buffer buys.
    static TOTAL_BYTES: Cell<isize> = const { Cell::new(0) };
    static TOTAL_ALLOCS: Cell<isize> = const { Cell::new(0) };
}

fn record(delta: isize) {
    if delta > 0 {
        let _ = TOTAL_BYTES.try_with(|total| total.set(total.get() + delta));
        let _ = TOTAL_ALLOCS.try_with(|count| count.set(count.get() + 1));
    }
    let _ = LIVE_BYTES.try_with(|live| {
        let now = live.get() + delta;
        live.set(now);
        if delta > 0 {
            let _ = PEAK_BYTES.try_with(|peak| {
                if now > peak.get() {
                    peak.set(now);
                }
            });
        }
    });
}

unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let pointer = unsafe { System.alloc(layout) };
        if !pointer.is_null() {
            record(layout.size() as isize);
        }
        pointer
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        record(-(layout.size() as isize));
        unsafe { System.dealloc(pointer, layout) };
    }

    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let replacement = unsafe { System.realloc(pointer, layout, new_size) };
        if !replacement.is_null() {
            record(new_size as isize - layout.size() as isize);
        }
        replacement
    }
}

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

/// `(total bytes, total allocations)` this thread has ever requested.
fn totals() -> (isize, isize) {
    (TOTAL_BYTES.with(Cell::get), TOTAL_ALLOCS.with(Cell::get))
}

fn begin_window() -> isize {
    let baseline = LIVE_BYTES.with(Cell::get);
    PEAK_BYTES.with(|peak| peak.set(baseline));
    baseline
}

varve_format! {
    pub format IndexResidencyFormat {
        magic: b"IDXRES";
        version: 1;
        limits {
            file_len: 8_589_934_592;
            records: 4_000_000;
            index_bytes: 536_870_912;
            scan_bytes: 8_589_934_592;
        }
        blocks {
            fixed Sample(id = 1) {
                value: u64,
            }
        }
    }
}

fn build(path: &Path, records: u32) -> varve::Result<()> {
    let mut writer = IndexResidencyFormat::create(path)?;
    for value in 0..records {
        writer.push(&Sample {
            value: u64::from(value),
        })?;
    }
    writer.flush()?;
    Ok(())
}

/// `(retained, peak)` bytes for an open handle over `path`, on this thread.
///
/// The handle is alive when `retained` is read and is dropped afterwards, so a
/// handle that frees its entries on drop cannot hide behind the drop.
fn open_cost(path: &Path) -> varve::Result<(isize, isize)> {
    let baseline = begin_window();
    let file = IndexResidencyFormat::open_readonly(path)?;
    let retained = LIVE_BYTES.with(Cell::get) - baseline;
    let peak = PEAK_BYTES.with(Cell::get) - baseline;
    drop(file);
    Ok((retained, peak))
}

fn path_in(directory: &tempfile::TempDir, name: &str) -> PathBuf {
    directory.path().join(name)
}

#[test]
fn an_open_handle_keeps_a_record_directory_and_not_the_records() -> varve::Result<()> {
    const SMALL: u32 = 2_000;
    const LARGE: u32 = 20_000;

    let directory = tempfile::tempdir()?;
    let small_path = path_in(&directory, "small.idxres");
    let large_path = path_in(&directory, "large.idxres");
    build(&small_path, SMALL)?;
    build(&large_path, LARGE)?;

    // Warm the process: the first open in a test binary allocates one-time
    // state that would otherwise land in whichever measurement ran first.
    let _ = open_cost(&small_path)?;

    let (small_retained, small_peak) = open_cost(&small_path)?;
    let (large_retained, large_peak) = open_cost(&large_path)?;

    let added_records = isize::try_from(LARGE - SMALL).expect("record count fits");
    let retained_per_record = (large_retained - small_retained) as f64 / added_records as f64;
    let peak_per_record = (large_peak - small_peak) as f64 / added_records as f64;

    assert!(
        retained_per_record < 32.0,
        "an open handle retains {retained_per_record:.2} bytes per record \
         ({small_retained} at {SMALL} records, {large_retained} at {LARGE}). A directory slot \
         is 16 bytes and the reservation is exact; anything above that means the entries \
         themselves are resident again."
    );
    assert!(
        peak_per_record < 256.0,
        "opening peaks at {peak_per_record:.2} bytes per record ({small_peak} at {SMALL}, \
         {large_peak} at {LARGE}). Today's 193.5 is the scan's `Vec` (177.5) plus the \
         directory built from it (16); this ceiling refuses a change that materialises the \
         entries a second time on top of that."
    );
    Ok(())
}

/// Opening `N` files through one caller-supplied buffer allocates the scan's
/// entry array **once**, not `N` times.
///
/// This is the half of the owner's rule that live-bytes and peak-bytes cannot
/// see. Both go back down when a buffer is freed, so a library that allocates
/// and frees an `N`-entry array on every open measures the same as one that
/// reuses the caller's. Cumulative bytes is the measurement that separates
/// them, so this is the one the test makes.
///
/// It is deliberately *cumulative and unbounded*: `TOTAL_BYTES` is never
/// decremented.
#[test]
fn opening_many_files_through_one_buffer_allocates_the_entry_array_once() -> varve::Result<()> {
    const RECORDS: u32 = 20_000;
    const OPENS: usize = 8;

    let directory = tempfile::tempdir()?;
    let path = path_in(&directory, "reused.idxres");
    build(&path, RECORDS)?;
    let spec = IndexResidencyFormat::spec();

    // Warm the process before either measurement, so one-time state does not
    // land in whichever ran first.
    drop(varve::VarveFile::open_readonly(spec, &path)?);

    let (bytes_before, allocs_before) = totals();
    for _ in 0..OPENS {
        drop(varve::VarveFile::open_readonly(spec, &path)?);
    }
    let (bytes_after, allocs_after) = totals();
    let owned_bytes = bytes_after - bytes_before;
    let owned_allocs = allocs_after - allocs_before;

    let mut scratch = Vec::new();
    let (bytes_before, allocs_before) = totals();
    for _ in 0..OPENS {
        drop(varve::VarveFile::open_readonly_with_scratch(
            spec,
            &path,
            &mut scratch,
        )?);
    }
    let (bytes_after, allocs_after) = totals();
    let shared_bytes = bytes_after - bytes_before;
    let shared_allocs = allocs_after - allocs_before;

    // Measured 2026-08-05, Linux/ext4, 20,000 records, 8 opens:
    //
    //   open_readonly              29,824,680 B over 168 allocations
    //   open_readonly_with_scratch  5,969,576 B over  70 allocations
    //
    // 5.00x fewer bytes and 2.4x fewer allocations. The difference,
    // 23,855,104 B, is seven further copies of the entry array and its growth
    // steps — 7 x 3,407,872 = 23,855,104 to the byte. The caller's buffer is
    // grown once on the first open and reused by the other seven.
    //
    // The 5,969,576 B that remains on the shared path is that first growth plus
    // everything an open does that is not the entry array. That part is
    // unchanged and is not what this measures.
    assert!(
        shared_bytes * 2 < owned_bytes,
        "reusing one buffer across {OPENS} opens allocated {shared_bytes} bytes against \
         {owned_bytes} for the library-owned form ({shared_allocs} vs {owned_allocs} \
         allocations). If these are close, the scan is no longer filling the caller's buffer."
    );
    Ok(())
}

/// A walk of `N` records allocates **one** payload buffer, not `N`.
///
/// Every typed read bottoms out in
/// `RecordIndexEntry::read_logical_payload_snapshot`, which allocated a fresh
/// buffer per record. The loops now hand it one buffer and reuse it, so what
/// scales with record count is the reads, not the allocations.
///
/// Cumulative again, for the same reason as the test above: each buffer was
/// freed at the end of its iteration, so live and peak bytes never saw this.
#[test]
fn a_walk_of_n_records_allocates_one_payload_buffer_not_n() -> varve::Result<()> {
    const SMALL: u32 = 2_000;
    const LARGE: u32 = 20_000;

    let directory = tempfile::tempdir()?;
    let small_path = path_in(&directory, "walk-small.idxres");
    let large_path = path_in(&directory, "walk-large.idxres");
    build(&small_path, SMALL)?;
    build(&large_path, LARGE)?;
    let spec = IndexResidencyFormat::spec();

    // `blocks().iter()` is the probe: the collection is built BEFORE the
    // window opens, so the only allocations inside it are the per-record
    // payload buffers this test is about. `Sample` is all scalars, so decoding
    // one allocates nothing of its own and cannot mask the result.
    //
    // `verify_all` would have been the cleaner probe, but this fixture declares
    // no integrity policy and `verify_all` returns 0 without reading anything.
    let walk = |path: &Path| -> varve::Result<(isize, isize)> {
        let file = varve::VarveFile::open_readonly(spec, path)?;
        let blocks = file.blocks::<Sample>()?;
        let (bytes_before, allocs_before) = totals();
        let mut seen = 0usize;
        for value in blocks.iter() {
            let _ = value?;
            seen += 1;
        }
        let (bytes_after, allocs_after) = totals();
        assert!(seen > 0, "the fixture must contain records");
        Ok((bytes_after - bytes_before, allocs_after - allocs_before))
    };

    let _ = walk(&small_path)?; // warm
    let (small_bytes, small_allocs) = walk(&small_path)?;
    let (large_bytes, large_allocs) = walk(&large_path)?;

    let added = isize::try_from(LARGE - SMALL).expect("record count fits");
    let allocs_per_record = (large_allocs - small_allocs) as f64 / added as f64;
    let bytes_per_record = (large_bytes - small_bytes) as f64 / added as f64;

    // Measured 2026-08-05, Linux/ext4, iterating `blocks::<Sample>()`:
    //
    //                                2,000 records      20,000 records
    //   before (one Vec per record)  2,000 allocs /     20,000 allocs /
    //                                16,000 B           160,000 B
    //   after  (one Vec per walk)        1 alloc  /          1 alloc  /
    //                                     8 B                  8 B
    //
    // Zero per record, not "fewer": the buffer is grown once by the first
    // record — `Sample` is one `u64`, hence 8 bytes — and never again, so the
    // walk's allocation count does not depend on the record count at all. The
    // before column is 1.00 allocations per record exactly, which is what makes
    // this test discriminating: run against the previous commit it reports
    // 1.00 and fails.
    assert!(
        allocs_per_record < 0.1,
        "verifying {LARGE} records made {allocs_per_record:.2} allocations per record \
         ({small_allocs} at {SMALL}, {large_allocs} at {LARGE}, {bytes_per_record:.1} B/record). \
         Above ~1.0 means the per-record payload buffer is back."
    );
    Ok(())
}

/// The pattern the owner asked for, end to end: **the host owns every buffer,
/// and chooses whether to keep the index or throw it away.**
///
/// Two shapes, both measured here:
///
/// * *keep it* — take the index into your own `Vec` once, then read records
///   through those entries as often as you like. After the index is taken,
///   further reads allocate nothing.
/// * *throw it away* — open, read, drop. Reuse one scan buffer and one payload
///   buffer across files and the whole loop allocates a fixed amount however
///   many files or records there are.
///
/// What varve still allocates on its own, and the host cannot decline: the
/// 16-byte-per-record directory the handle keeps so that positions resolve.
/// That is measured by `an_open_handle_keeps_a_record_directory_and_not_the_records`
/// and is NOT claimed to be zero here.
#[test]
fn the_host_owns_the_buffers_and_chooses_whether_to_keep_the_index() -> varve::Result<()> {
    const RECORDS: u32 = 20_000;

    let directory = tempfile::tempdir()?;
    let path = path_in(&directory, "host-owned.idxres");
    build(&path, RECORDS)?;
    let spec = IndexResidencyFormat::spec();

    // --- shape 1: keep the index, read through it ---
    let mut scan_scratch = Vec::new();
    let file = varve::VarveFile::open_readonly_with_scratch(spec, &path, &mut scan_scratch)?;

    let mut index = Vec::new();
    file.index_entries_into(&mut index)?;
    assert_eq!(index.len() as u32, RECORDS, "one entry per record");

    let mut payload = Vec::new();
    // Warm both buffers on the first record, then measure the rest: what is
    // being asserted is that steady-state reads allocate nothing, not that the
    // very first one does.
    let _: Sample = file.decode_block_into(&index[0], &mut payload)?;

    let (bytes_before, allocs_before) = totals();
    let mut decoded = 0usize;
    for entry in &index {
        if entry.block_id != Sample::ID {
            continue;
        }
        let sample: Sample = file.decode_block_into(entry, &mut payload)?;
        decoded += usize::from(sample.value < u64::from(RECORDS));
    }
    let (bytes_after, allocs_after) = totals();
    let read_allocs = allocs_after - allocs_before;
    let read_bytes = bytes_after - bytes_before;

    assert_eq!(decoded as u32, RECORDS, "every record must decode");
    // Measured 2026-08-05, Linux/ext4: 0 allocations, 0 bytes for 20,000 reads.
    assert_eq!(
        read_allocs, 0,
        "reading {RECORDS} records through a cached index made {read_allocs} allocations \
         ({read_bytes} bytes). The host supplied both buffers, so varve must allocate nothing."
    );
    drop(file);

    // --- shape 2: open, read, drop — repeatedly, through the same buffers ---
    let (bytes_before, allocs_before) = totals();
    for _ in 0..4 {
        let file = varve::VarveFile::open_readonly_with_scratch(spec, &path, &mut scan_scratch)?;
        file.index_entries_into(&mut index)?;
        let _: Sample = file.decode_block_into(&index[1], &mut payload)?;
        drop(file);
    }
    let (bytes_after, allocs_after) = totals();
    let loop_allocs = allocs_after - allocs_before;
    let loop_bytes = bytes_after - bytes_before;

    // Measured 2026-08-05, Linux/ext4: 28 allocations / 1,280,868 bytes for
    // four opens — 320,217 bytes each. The record directory alone is
    // 20,000 x 16 = 320,000 of that, so everything else an open does comes to
    // 217 bytes, and the scan buffer, the index snapshot and the payload
    // buffer contribute nothing at all: they are the host's and are reused
    // across all four.
    //
    // The counterfactual is the test above: without `_with_scratch` the same
    // loop re-allocates the scan's entry array every time, which is 3,407,872
    // bytes per open rather than 0.
    //
    // So the honest statement is: everything the host can own, the host owns.
    // What remains is the directory — 16 B per record, per open — and the host
    // cannot decline it, because it is what makes a position resolve.
    let directory_bytes = 4 * 16 * isize::try_from(RECORDS).expect("record count fits");
    assert!(
        loop_bytes < directory_bytes + directory_bytes / 4,
        "four open/read/drop cycles through one set of host buffers allocated {loop_bytes} \
         bytes over {loop_allocs} allocations, against {directory_bytes} for the record \
         directories alone. The excess means a host buffer stopped being reused."
    );
    Ok(())
}

/// A directory the **host** supplies answers every position-based read exactly
/// as varve's own does.
///
/// This is the contract that makes "varve stops keeping the directory" a
/// capability with a cost rather than a capability withdrawn. `blocks`, `scan`,
/// `keyed_blocks`, `metadata`, `verify_all` and the rest do not disappear when
/// the handle has no directory — they ask for one.
///
/// Asserted as *equality against the resident answer*, not as "it returns
/// something": a supplied directory that silently answered a shorter file
/// would pass any weaker check.
#[test]
fn a_host_supplied_directory_answers_every_read_the_same() -> varve::Result<()> {
    const RECORDS: u32 = 500;

    let directory = tempfile::tempdir()?;
    let path = path_in(&directory, "supplied.idxres");
    build(&path, RECORDS)?;
    let spec = IndexResidencyFormat::spec();

    // The scan buffer IS the index after open — the host already has it, for
    // free, without a second pass over the file.
    let mut index = Vec::new();
    let file = varve::VarveFile::open_readonly_with_scratch(spec, &path, &mut index)?;
    assert_eq!(index.len() as u32, RECORDS);

    let supplied = file.with_directory(&index);
    assert_eq!(supplied.record_count(), index.len());

    // scan
    // `BlockEvent` is not `PartialEq`, so compare the fields that identify a
    // record: its block id and where it sits.
    let project = |events: Vec<varve::BlockEvent>| -> Vec<(u32, u64, u64)> {
        events
            .into_iter()
            .map(|event| (event.block_id, event.record_offset, event.payload_len))
            .collect()
    };
    let resident_scan = project(file.scan().collect::<varve::Result<Vec<_>>>()?);
    let supplied_scan = project(supplied.scan().collect::<varve::Result<Vec<_>>>()?);
    assert_eq!(resident_scan.len(), RECORDS as usize);
    assert_eq!(resident_scan, supplied_scan);

    // blocks, decoded through both
    let mut resident_blocks: Vec<Sample> = Vec::new();
    let mut supplied_blocks: Vec<Sample> = Vec::new();
    file.decode_blocks_into(&mut resident_blocks)?;
    supplied.decode_blocks_into(&mut supplied_blocks)?;
    assert_eq!(resident_blocks.len(), RECORDS as usize);
    assert_eq!(resident_blocks, supplied_blocks);

    // block entries
    let mut resident_entries = Vec::new();
    let mut supplied_entries = Vec::new();
    file.block_entries_into::<Sample>(&mut resident_entries)?;
    supplied.block_entries_into::<Sample>(&mut supplied_entries)?;
    assert_eq!(resident_entries, supplied_entries);

    // the index itself, round-tripped through the supplied directory
    let mut round_tripped = Vec::new();
    supplied.index_entries_into(&mut round_tripped)?;
    assert_eq!(round_tripped, index);

    // and the lazy collection resolves positions identically
    let resident_vec = file.blocks::<Sample>()?;
    let supplied_vec = supplied.blocks::<Sample>()?;
    assert_eq!(resident_vec.len(), supplied_vec.len());
    assert_eq!(
        resident_vec.get(RECORDS as usize - 1)?,
        supplied_vec.get(RECORDS as usize - 1)?
    );

    // metadata and manifest, which walk the same directory for a different
    // block id
    assert_eq!(file.metadata("absent")?, supplied.metadata("absent")?);
    assert_eq!(
        file.schema_manifest()?.is_some(),
        supplied.schema_manifest()?.is_some()
    );

    // A *shorter* directory must be answered as a shorter file, not silently
    // padded from the handle's own. This is what proves the reads really go
    // through the parameter.
    let truncated: &[varve::RecordIndexEntry] = &index[..10];
    let short = file.with_directory(truncated);
    assert_eq!(short.record_count(), 10);
    assert_eq!(short.scan().collect::<varve::Result<Vec<_>>>()?.len(), 10);
    Ok(())
}
