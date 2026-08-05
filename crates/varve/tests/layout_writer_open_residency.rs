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
    assert_eq!(reader.segments().len(), 5);
    assert_eq!(reader.segments()[0].name, "ControlSegment");
    assert_eq!(reader.control_segments()?.len(), 1);
    assert_eq!(reader.data_segments()?.len(), 4);
    assert_eq!(
        reader.data_segment(3)?.expect("last segment").channel()?,
        99
    );
    Ok(())
}
