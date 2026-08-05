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

use varve::varve_format;

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
}

fn record(delta: isize) {
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
