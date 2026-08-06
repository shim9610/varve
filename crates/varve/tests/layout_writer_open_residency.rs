//! `LayoutWriter::open` must not hold the file's segments in memory.
//!
//! Opening an existing custom-layout file for append walks every segment — it
//! has to, that is how the append offset and the per-name counts are
//! established — but the only things it keeps are the counts and the running
//! index-byte total, both `O(declared segment kinds)`. It used to materialise
//! one `LayoutSegmentInfo` per segment in the file, with its `Vec` of decoded
//! field values, derive the counts by walking that `Vec` a second time, and
//! then drop the whole thing.
//!
//! The discriminator here is **flatness across two file sizes**, not a single
//! number: a change that merely shrank `LayoutSegmentInfo` would still grow
//! with the segment count, and a single measurement cannot tell the two apart.
//! So this measures the peak live bytes during `open` at 1,000 segments and at
//! 50,000 and requires the difference to be small in absolute terms.
//!
//! Deliberately its own test binary: a process gets one `#[global_allocator]`.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::path::{Path, PathBuf};

use varve::{Error, varve_format};

struct CountingAllocator;

std::thread_local! {
    /// Live bytes attributable to *this* thread, and this thread's high-water
    /// mark since the window opened.
    ///
    /// Per-thread rather than process-global on purpose: `cargo test` runs test
    /// functions on several threads at once, and a process-wide high-water mark
    /// would be measuring whichever test happened to be running beside this
    /// one. Memory allocated on one thread and freed on another skews it, which
    /// a single-threaded open loop does not do.
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

/// Opens a measurement window on the calling thread and returns its baseline.
fn begin_window() -> isize {
    let baseline = LIVE_BYTES.with(Cell::get);
    PEAK_BYTES.with(|peak| peak.set(baseline));
    baseline
}

/// Peak live bytes above the window's baseline.
fn peak_above(baseline: isize) -> isize {
    PEAK_BYTES.with(Cell::get) - baseline
}

varve_format! {
    pub format ResidencyLayoutFormat {
        magic: b"RESL";
        version: 1;
        limits {
            file_len: 8_589_934_592;
            records: 4_000_000;
            index_bytes: 536_870_912;
            scan_bytes: 8_589_934_592;
            record_payload: 67_108_864;
            logical_payload: 268_435_456;
            materialized_bytes: 1_073_741_824;
            segments: 4_000_000;
            matrix_dimension: 16_000_000;
            matrix_cells: 16_000_000;
            matrix_bitmap: 64_000_000;
            matrix_crc: 128_000_000;
            matrix_metadata: 268_435_456;
            matrix_slot_region: 8_589_934_592;
            sidecar: 268_435_456;
            mmap: 8_589_934_592;
        }
        endian: little;
        schema_hash: computed;
        extension: "resl";
        preset: none;

        layout {
            segment ControlSegment repeat once {
                lead_in ControlLeadIn {
                    bytes tag = b"CTRL";
                    u32 code;
                    i64 next_segment_offset = finalize(target = segment_end, relative_to = after_lead_in);
                    i64 raw_data_offset = finalize(target = raw_region_start, relative_to = after_lead_in);
                }

                metadata ControlMetadata;
                raw_region ControlRaw;
            }

            segment DataSegment repeat until_eof {
                lead_in DataLeadIn {
                    bytes tag = b"DATA";
                    u32 channel;
                    i64 next_segment_offset = finalize(target = segment_end, relative_to = after_lead_in);
                    i64 raw_data_offset = finalize(target = raw_region_start, relative_to = after_lead_in);
                }

                metadata DataMetadata;
                raw_region DataRaw;
            }
        }
    }
}

fn write_control(writer: &mut ResidencyLayoutFormatLayoutWriter, code: u32) -> varve::Result<()> {
    writer
        .write_control_segment(ResidencyLayoutFormatControlSegmentLayoutWrite {
            fields: ResidencyLayoutFormatControlSegmentLayoutFields { code },
            footer_fields: ResidencyLayoutFormatControlSegmentLayoutFooterFields,
            metadata: b"c",
            raw: b"",
        })
        .map(|_| ())
}

fn write_data(writer: &mut ResidencyLayoutFormatLayoutWriter, channel: u32) -> varve::Result<()> {
    writer
        .write_data_segment(ResidencyLayoutFormatDataSegmentLayoutWrite {
            fields: ResidencyLayoutFormatDataSegmentLayoutFields { channel },
            footer_fields: ResidencyLayoutFormatDataSegmentLayoutFooterFields,
            metadata: b"d",
            raw: b"",
        })
        .map(|_| ())
}

/// One control segment plus `data_segments` data segments.
fn build(path: &Path, data_segments: u32) -> varve::Result<()> {
    let mut writer = ResidencyLayoutFormat::create_layout_writer(path)?;
    write_control(&mut writer, 1)?;
    for channel in 0..data_segments {
        write_data(&mut writer, channel)?;
    }
    writer.flush()?;
    Ok(())
}

/// `(retained, peak)` bytes for an open `LayoutReader`, on this thread.
///
/// The handle is alive when `retained` is read and is dropped afterwards, so a
/// reader that frees its segments on drop cannot hide behind the drop.
fn reader_cost(path: &Path) -> varve::Result<(isize, isize)> {
    let baseline = begin_window();
    let reader = ResidencyLayoutFormat::open_layout_reader(path)?;
    let retained = LIVE_BYTES.with(Cell::get) - baseline;
    let peak = peak_above(baseline);
    drop(reader);
    Ok((retained, peak))
}

/// Peak live bytes during `open_layout_writer`, measured on this thread.
///
/// The handle is dropped *after* the peak is read, so a writer that frees its
/// retained segments on drop cannot hide behind the drop.
fn open_peak(path: &Path) -> varve::Result<isize> {
    let baseline = begin_window();
    let writer = ResidencyLayoutFormat::open_layout_writer(path)?;
    let peak = peak_above(baseline);
    drop(writer);
    Ok(peak)
}

fn path_in(directory: &tempfile::TempDir, name: &str) -> PathBuf {
    directory.path().join(name)
}

#[test]
fn opening_a_layout_writer_costs_the_same_at_1_000_and_50_000_segments() -> varve::Result<()> {
    const SMALL: u32 = 1_000;
    const LARGE: u32 = 50_000;
    /// 49,000 retained `LayoutSegmentInfo`s are >= 5.4 MB of struct alone
    /// before their field `Vec`s, so anything under a quarter of a megabyte of
    /// growth is flat for the purpose of this question — and is far below what
    /// a merely-smaller struct would produce.
    const SLACK: isize = 256 * 1024;

    let directory = tempfile::tempdir()?;
    let small_path = path_in(&directory, "small.resl");
    let large_path = path_in(&directory, "large.resl");
    build(&small_path, SMALL)?;
    build(&large_path, LARGE)?;

    // Warm first: the first open of the process pulls in whatever the format
    // machinery lazily allocates, and that must not land in either measurement.
    open_peak(&small_path)?;

    let small = open_peak(&small_path)?;
    let large = open_peak(&large_path)?;
    let growth = large - small;

    assert!(
        small > 0,
        "the allocator window measured nothing at all ({small} bytes), so the \
         comparison below would pass vacuously"
    );
    assert!(
        growth <= SLACK,
        "peak live bytes during `LayoutWriter::open` grew by {growth} bytes \
         between {SMALL} and {LARGE} segments ({small} -> {large}); open is \
         still holding the file's segments"
    );
    Ok(())
}

/// The correctness net for the counting scan: the per-name tally it produces is
/// what `ensure_segment_can_write` consults, so a sink that miscounts is caught
/// as a wrong *verdict* here rather than as a smaller number above.
#[test]
fn a_reopened_writer_still_knows_which_segments_it_has() -> varve::Result<()> {
    let directory = tempfile::tempdir()?;
    let path = path_in(&directory, "counts.resl");
    build(&path, 3)?;

    {
        let mut writer = ResidencyLayoutFormat::open_layout_writer(&path)?;
        // The `once` segment is already in the file. The reopened writer only
        // knows that from the counts its scan produced.
        assert!(
            matches!(
                write_control(&mut writer, 2),
                Err(Error::LayoutRepeatedOnceSegment { .. })
            ),
            "a reopened writer let a `repeat once` segment be written twice; \
             its segment counts did not survive the scan"
        );
        // ... and an `until_eof` segment is still writable, so the counts are
        // not merely refusing everything.
        write_data(&mut writer, 99)?;
        writer.flush()?;
    }

    let reader = ResidencyLayoutFormat::open_layout_reader(&path)?;
    assert_eq!(reader.segment_count(), 5);
    assert_eq!(
        reader.segment(0)?.expect("first segment").name,
        "ControlSegment"
    );
    assert_eq!(reader.control_segments()?.len(), 1);
    assert_eq!(reader.data_segments()?.len(), 4);
    assert_eq!(
        reader.data_segment(3)?.expect("last segment").channel()?,
        99
    );
    Ok(())
}

/// A `LayoutReader` must not hold the file's segments either.
///
/// The writer stopped retaining them when `CountSegments` was written; the
/// reader kept a `Vec<LayoutSegmentInfo>` — a struct plus two `Vec`s of decoded
/// field values, several heap allocations each — for the life of the handle.
/// For a varve-native spec those segments are records, so this failed the
/// TB-scale requirement the same way the record index did.
///
/// It now keeps a 16-byte directory slot per segment (start offset, kind) and
/// rebuilds the info from the segment's own lead-in and footer on demand. Same
/// discriminator as the writer test above: the slope across two sizes, not one
/// number.
#[test]
fn opening_a_layout_reader_keeps_a_directory_and_not_the_segments() -> varve::Result<()> {
    const SMALL: u32 = 1_000;
    const LARGE: u32 = 50_000;

    let directory = tempfile::tempdir()?;
    let small_path = path_in(&directory, "reader-small.resl");
    let large_path = path_in(&directory, "reader-large.resl");
    build(&small_path, SMALL)?;
    build(&large_path, LARGE)?;

    reader_cost(&small_path)?; // warm

    let (small_retained, small_peak) = reader_cost(&small_path)?;
    let (large_retained, large_peak) = reader_cost(&large_path)?;

    let added = isize::try_from(LARGE - SMALL).expect("segment count fits");
    let retained_per_segment = (large_retained - small_retained) as f64 / added as f64;
    let peak_per_segment = (large_peak - small_peak) as f64 / added as f64;

    // Measured 2026-08-05, Linux/ext4, 1,000 -> 50,000 segments:
    //
    //   before  retained 303,485 -> 17,113,117 B   343.05 B/segment
    //           peak     370,537 -> 17,180,169 B   343.05 B/segment
    //   after   retained  16,441 ->  1,048,633 B    21.07 B/segment
    //           peak       83,657 -> 1,115,849 B    21.07 B/segment
    //
    // 16.3x. The 21.07 is the 16-byte slot plus the directory `Vec`'s doubling
    // slack: 65,536 slots x 16 = 1,048,576, which is the retained figure to
    // within the fixed per-open cost. Peak tracks retained exactly, before and
    // after, because the layout scan accepts one segment at a time and never
    // held the whole array — unlike the record index, whose peak had to be
    // reported separately.

    assert!(
        small_retained > 0,
        "the allocator window measured nothing at all, so the comparison below \
         would pass vacuously"
    );
    assert!(
        retained_per_segment < 32.0,
        "an open reader retains {retained_per_segment:.2} bytes per segment \
         ({small_retained} at {SMALL}, {large_retained} at {LARGE}). A directory slot is 16 \
         bytes; anything above that means the `LayoutSegmentInfo`s are resident again."
    );
    assert!(
        peak_per_segment < 32.0,
        "opening a reader peaks at {peak_per_segment:.2} bytes per segment ({small_peak} at \
         {SMALL}, {large_peak} at {LARGE}). Unlike the record index, the layout scan never \
         materialised the whole array — it accepts one segment at a time — so peak and \
         retained should agree here."
    );
    Ok(())
}
