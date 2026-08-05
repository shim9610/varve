//! F35: the generated layout reader must not re-derive a segment's absolute
//! index by scanning `segments()` from the start on every typed read.
//!
//! `__varve_layout_segment_index` used to be
//! `segments().iter().enumerate().filter(name == ..).nth(ordinal)`, so an
//! ordered pass over N segments of one kind cost `sum(i) = O(N^2)` name
//! comparisons and a past-the-end lookup walked the whole slice. The fix builds
//! a per-name ordinal table on the generated reader, lazily on first typed
//! access, reached through `&self`.
//!
//! Two claims, proved separately:
//!   * behaviour — the table returns exactly what the scan returned, including
//!     `Ok(None)` (not an error) past the end;
//!   * cost — measured as a RATIO inside one process, so it does not depend on
//!     how fast the machine is. A comparison counter would be the better
//!     instrument, but it would have to live in `varve-core`, which is outside
//!     this change's file set.

use std::time::{Duration, Instant};

use varve::{SegmentWrite, varve_format};

varve_format! {
    pub format OrdinalLayoutFormat {
        magic: b"ORDL";
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
        }
        endian: little;
        schema_hash: computed;
        extension: "ordl";
        preset: none;

        layout {
            file_header OrdinalFileHeader {
                bytes signature = b"ORD!";
                u16 header_version = 1;
            }

            segment DataSegment repeat until_eof {
                lead_in DataLeadIn {
                    bytes tag = b"SEGM";
                    u32 kind;
                    i64 next_segment_offset = finalize(target = segment_end, relative_to = after_lead_in);
                    i64 raw_data_offset = finalize(target = raw_region_start, relative_to = after_lead_in);
                }

                metadata DataMetadata;
                raw_region DataRaw;
            }
        }
    }
}

/// Segment count. Large enough that an O(N^2) pass is unmistakable next to an
/// O(N) one, small enough that the file stays under a megabyte.
const SEGMENTS: usize = 20_000;
/// Slice size for the head/tail timing comparison.
const SLICE: usize = SEGMENTS / 8;

fn build(path: &std::path::Path) -> varve::Result<()> {
    let mut writer = OrdinalLayoutFormat::create_layout_writer(path)?;
    for index in 0..SEGMENTS {
        let fields = [varve::LayoutFieldValue {
            name: "kind",
            value: varve::LayoutValue::U32(index as u32),
        }];
        writer.write_segment(SegmentWrite {
            name: "DataSegment",
            fields: &fields,
            footer_fields: &[],
            metadata: b"m",
            raw: &(index as u32).to_le_bytes(),
        })?;
    }
    writer.flush()?;
    Ok(())
}

fn time<F: FnMut()>(iterations: usize, mut body: F) -> Duration {
    let start = Instant::now();
    for _ in 0..iterations {
        body();
    }
    start.elapsed()
}

#[test]
fn typed_layout_segment_lookup_is_not_quadratic() -> varve::Result<()> {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("ordinal_layout.ordl");
    build(&path)?;

    let reader = OrdinalLayoutFormat::open_layout_reader(&path)?;
    assert_eq!(reader.segments().len(), SEGMENTS);

    // ---- behaviour: identical to what the linear scan returned ----
    for index in 0..SEGMENTS {
        let info = reader
            .data_segment(index)?
            .expect("every declared segment is reachable by ordinal");
        assert_eq!(info.kind()?, index as u32);
    }
    // Past the end is `Ok(None)`, not an error: the generated accessor converts
    // the index failure, and a fix that changed that would be a silent API
    // break.
    assert!(reader.data_segment(SEGMENTS)?.is_none());
    assert!(reader.data_segment(SEGMENTS + 5_000)?.is_none());
    // The byte-region readers agree with the ordinal they were handed.
    for index in [0usize, 1, SEGMENTS / 2, SEGMENTS - 1] {
        assert_eq!(
            reader.read_data_segment_raw(index)?,
            (index as u32).to_le_bytes()
        );
        assert_eq!(reader.read_data_segment_metadata(index)?, b"m");
    }

    // ---- cost ----
    // Warm-up: pay the one-time table build (and any page faults) before
    // anything is timed, so the head slice is not charged for it.
    let _ = reader.data_segment(0)?;

    let head = time(1, || {
        for index in 0..SLICE {
            let info = reader.data_segment(index).expect("head lookup");
            assert!(info.is_some());
        }
    });
    let tail = time(1, || {
        for index in (SEGMENTS - SLICE)..SEGMENTS {
            let info = reader.data_segment(index).expect("tail lookup");
            assert!(info.is_some());
        }
    });
    // A scan from the start makes the last eighth cost ~15x the first eighth
    // (mean walk 15N/16 vs N/16). A table makes them equal. 4x leaves a wide
    // margin for timer noise on either side.
    assert!(
        tail.as_nanos() * 100 < head.as_nanos() * 400,
        "tail-of-file lookups cost {tail:?} against {head:?} for the head of the \
         file: the segment index is still being derived by scanning"
    );

    // The miss path must be constant too, or an implementation that builds the
    // table but falls back to the scan when the ordinal is absent shows the old
    // number here while passing the assertion above.
    let miss = time(1, || {
        for _ in 0..SLICE {
            let info = reader.data_segment(SEGMENTS).expect("miss lookup");
            assert!(info.is_none());
        }
    });
    assert!(
        miss.as_nanos() * 100 < head.as_nanos() * 400,
        "past-the-end lookups cost {miss:?} against {head:?} for real lookups: \
         the miss path still walks every segment"
    );

    Ok(())
}
