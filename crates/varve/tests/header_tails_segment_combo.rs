//! `segment_on_flush` together with `header_tails` — the only fixture where a
//! commit point appends a record *after* its own commit marker.
//!
//! A reader corroborates the header slot by framing the commit marker it names
//! and then walking forward to the end of the file, and that walk has an arm
//! for a trailing segment and an arm for a trailing digest. Both were dead code
//! before this file: `header_tails` refuses `open_digest_on_flush`, so the
//! digest arm is unreachable by construction, and no fixture declared segments
//! alongside the region, so the segment arm was never entered either. Every
//! other `header_tails` file goes marker → end of file in one step. Here the
//! walk has to step through a segment to get there, and if it could not, the
//! option would be silently inert on such a format — falling back to the full
//! scan at every open while still costing its bytes and its schema hash.
//!
//! The write order inside `close_commit_point` is pinned here too, and it is
//! worth being precise about what it buys, because it is less than it looks.
//! Moving the slot write ahead of the segment does **not** break the reader:
//! the slot carries a commit offset, not a file length, so the walk starts from
//! the same marker and still lands on the end of the file. What it breaks is
//! the slot's own `SEGMENT` entry, which then names the previous commit point's
//! segment — a wrong chain link that corroboration cannot see, because a stale
//! segment offset still frames as a segment record.
//! `the_slot_names_the_segment_from_its_own_commit_point` is the one assertion
//! that moves when the order does; the four resume tests do not.
//!
//! There is deliberately no DSL keyword for `segment_on_flush` — segment
//! granularity is varve's decision, not a declaration's — so the fixture is
//! built rather than declared, exactly as `index_segments.rs` builds its own.

#![cfg(all(feature = "integrity", feature = "scalable-fault-injection"))]

use varve::{Error, FormatSpec, LazyOpenSource, VarveBlock, VarveFile, varve_format};

#[derive(Clone, Debug, PartialEq, VarveBlock)]
#[varve(id = 10, version = 1, kind = "fixed")]
struct Reading {
    value: u64,
}

#[derive(Clone, Debug, PartialEq, VarveBlock)]
#[varve(id = 11, version = 1, kind = "variable")]
struct Label {
    #[varve(field_id = 1)]
    text: String,
}

varve_format! {
    pub struct SegmentTailFormat {
        magic: b"VSEGTAIL";
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
        integrity: crc32;
        index: header_tails;
        commit: transaction_marker(on_flush);
        blocks: [Reading, Label];
    }
}

/// The declared format with varve's segment chain switched on as well.
///
/// `with_index_policy` leaves `schema_hash` as the macro computed it, which is
/// harmless here because every open in this file uses this same spec: the hash
/// is an identity check between a spec and a file, and the region's length
/// comes from the policy, not from the hash.
fn combo_spec() -> FormatSpec {
    SegmentTailFormat::spec().with_index_policy(
        SegmentTailFormat::spec()
            .index_policy
            .with_segment_on_flush(true),
    )
}

/// The region's framing, restated rather than imported — the same choice
/// `header_tail_region.rs` makes, and for the same reason: a test that computed
/// these from the writer's own constants would agree with a wrong change to
/// them.
const SLOTS: usize = 2;
const RESERVED_ENTRIES: usize = 10;
const SLOT_HEADER_LEN: usize = 2 + 2 + 4 + 4 + 8 + 8;
const ENTRY_LEN: usize = 4 + 8;
const SLOT_CRC_LEN: usize = 4;
const BLOCK_FRAMING_LEN: usize = 4 + 4;

/// CRC-32/ISO-HDLC, written out rather than taken from `crc32fast`, so the
/// assertion is "the bytes on disk carry this checksum" and not "two calls into
/// the same crate agree".
fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for byte in bytes {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            let mask = 0u32.wrapping_sub(crc & 1);
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
        }
    }
    !crc
}

struct Slot {
    generation: u64,
    commit_offset: u64,
    tails: Vec<(u32, u64)>,
}

/// The warm slot a reader would pick: checksum intact, highest generation.
fn newest_valid_slot(bytes: &[u8]) -> Option<Slot> {
    let region = bytes
        .windows(4)
        .position(|window| window == b"VBTT")
        .filter(|offset| *offset < 512)?;
    let capacity = SegmentTailFormat::spec().blocks.len() + RESERVED_ENTRIES;
    let slot_len = SLOT_HEADER_LEN + capacity * ENTRY_LEN + SLOT_CRC_LEN;
    (0..SLOTS)
        .filter_map(|index| {
            let start = region + BLOCK_FRAMING_LEN + index * slot_len;
            let slot = &bytes[start..start + slot_len];
            let stored = u32::from_le_bytes(slot[slot_len - SLOT_CRC_LEN..].try_into().ok()?);
            if stored != crc32(&slot[..slot_len - SLOT_CRC_LEN]) {
                return None;
            }
            let count = u32::from_le_bytes(slot[8..12].try_into().ok()?) as usize;
            let tails = (0..count)
                .map(|entry| {
                    let at = SLOT_HEADER_LEN + entry * ENTRY_LEN;
                    (
                        u32::from_le_bytes(slot[at..at + 4].try_into().unwrap()),
                        u64::from_le_bytes(slot[at + 4..at + 12].try_into().unwrap()),
                    )
                })
                .collect();
            Some(Slot {
                generation: u64::from_le_bytes(slot[12..20].try_into().ok()?),
                commit_offset: u64::from_le_bytes(slot[20..28].try_into().ok()?),
                tails,
            })
        })
        .max_by_key(|slot| slot.generation)
}

/// Several commit points, so the segment the last one writes has earlier ones
/// behind it and the slot has to describe the file as it is *after* the last.
fn build(path: &std::path::Path, readings: u64) -> varve::Result<()> {
    let mut file = combo_spec().create(path)?;
    for value in 0..readings {
        file.push(&Reading { value })?;
        if value % 40 == 39 {
            file.push(&Label {
                text: format!("mark {value}"),
            })?;
            file.flush()?;
        }
    }
    file.flush()?;
    Ok(())
}

/// The claim: a file carrying segments still resumes from the header.
#[test]
fn a_file_with_segments_still_resumes_from_the_header() -> varve::Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("segments.varve");
    build(&path, 200)?;

    let (lazy, source) = VarveFile::open_readonly_lazy_with_report(combo_spec(), &path)?;
    assert_eq!(
        source,
        LazyOpenSource::HeaderTails,
        "the segment appended at the commit point must not put the end of the \
         file past where the slot says it is",
    );

    // And the tails it adopted are the ones the scan finds. Equality is the
    // assertion; the route above is what makes it interesting.
    let scanned = VarveFile::open_readonly(combo_spec(), &path)?;
    for block_id in [Reading::ID, Label::ID] {
        assert_eq!(
            lazy.block_tail_offset(block_id),
            scanned.block_tail_offset(block_id),
            "block {block_id}'s tail",
        );
    }
    // Not merely equal to the other handle's numbers — each frames a record of
    // its own block, read back through the header route's own handle.
    let reading: Reading = lazy.read_block_at(lazy.block_tail_offset(Reading::ID).unwrap())?;
    assert_eq!(reading.value, 199);
    let label: Label = lazy.read_block_at(lazy.block_tail_offset(Label::ID).unwrap())?;
    assert_eq!(label.text, "mark 199");
    Ok(())
}

/// The forward walk really does traverse a segment on this file.
///
/// Without this the test above would pass just as well if the last commit point
/// happened to append no segment, and the `[SEGMENT]` arm would still be dead.
/// The segment record has to be the thing sitting between the commit marker and
/// the end of the file.
#[test]
fn a_segment_is_what_sits_between_the_marker_and_the_end_of_the_file() -> varve::Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("walk.varve");
    build(&path, 200)?;

    let (lazy, source) = VarveFile::open_readonly_lazy_with_report(combo_spec(), &path)?;
    assert_eq!(source, LazyOpenSource::HeaderTails);

    let marker = lazy
        .block_tail_offset(varve::COMMIT_BLOCK_ID)
        .expect("a transaction_marker format that flushed has a marker");
    let segment = lazy
        .block_tail_offset(varve::SEGMENT_BLOCK_ID)
        .expect("segment_on_flush writes one at each commit point");
    assert!(
        segment > marker,
        "the segment is appended after the marker it closes: marker {marker}, \
         segment {segment}",
    );
    // Nothing starts after the segment, so it is the last record — and the walk
    // that reached the end of the file from `marker` therefore went through the
    // `[SEGMENT]` arm rather than breaking out at the marker's own end.
    for block_id in [Reading::ID, Label::ID] {
        let tail = lazy.block_tail_offset(block_id).expect("a tail");
        assert!(tail < segment, "block {block_id} starts after the segment");
    }
    Ok(())
}

/// The open cost does not move with the file, which is the point of the option.
///
/// Measured on this fixture specifically, because a segment is `O(records)`
/// bytes: an open that read *it* would grow with the file even though it never
/// framed a data record.
#[test]
fn the_header_route_does_not_read_the_segment() -> varve::Result<()> {
    fn measure(directory: &std::path::Path, readings: u64) -> varve::Result<u64> {
        let path = directory.join(format!("cost-{readings}.varve"));
        build(&path, readings)?;
        let before = VarveFile::records_framed();
        let (_file, source) = VarveFile::open_readonly_lazy_with_report(combo_spec(), &path)?;
        let framed = VarveFile::records_framed() - before;
        assert_eq!(source, LazyOpenSource::HeaderTails);
        Ok(framed)
    }

    let directory = tempfile::tempdir()?;
    let small = measure(directory.path(), 80)?;
    let large = measure(directory.path(), 1_600)?;
    assert_eq!(
        small, large,
        "the file grew twentyfold and the open framed {small} then {large} \
         records",
    );
    // The marker, the segment behind it, and one record per block tail.
    assert!(large <= 8, "the header open frames {large} records");
    Ok(())
}

/// The slot a commit point writes names *that* commit point's segment.
///
/// This is what the write order at the end of `close_commit_point` actually
/// buys, and it is narrower than it looks. Moving the slot write ahead of the
/// segment does **not** break the reader's corroboration: the slot carries a
/// commit offset, not a file length, so the forward walk still starts at the
/// same marker and still lands on the end of the file — through the segment,
/// which is the arm this fixture exists to run. What breaks is the slot's own
/// `SEGMENT` entry, which then names the *previous* commit point's segment. A
/// stale offset like that survives corroboration untouched, because it does
/// frame as a segment record; it is only wrong as a chain link.
///
/// So the assertion has to read the slot directly, and it has to read it while
/// the writer is still open: `sync` re-closes the commit point on drop and the
/// region write has no "something new" predicate, so a clean close rewrites the
/// slot with the current tails and repairs the staleness before any later open
/// could see it. Only a file that stopped at the commit point keeps it.
#[test]
fn the_slot_names_the_segment_from_its_own_commit_point() -> varve::Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("slot.varve");

    let mut file = combo_spec().create(&path)?;
    // Two commit points, so there is an older segment for a stale entry to name.
    for round in 0..2u64 {
        for value in 0..40 {
            file.push(&Reading {
                value: round * 40 + value,
            })?;
        }
        file.flush()?;
    }

    // Read the bytes the writer has already put down, without closing it.
    let bytes = std::fs::read(&path)?;
    let slot = newest_valid_slot(&bytes).expect("a commit point warmed a slot");
    let slot_segment = slot
        .tails
        .iter()
        .find(|(block_id, _)| *block_id == varve::SEGMENT_BLOCK_ID)
        .map(|(_, offset)| *offset)
        .expect("segment_on_flush wrote a segment before the slot");

    // The truth, from the handle that did the appending.
    let live_segment = file
        .block_tail_offset(varve::SEGMENT_BLOCK_ID)
        .expect("the writer tracked the segment it just appended");
    assert_eq!(
        slot_segment, live_segment,
        "the slot names segment {slot_segment} while the commit point it \
         describes ended with segment {live_segment}",
    );
    assert!(
        slot_segment > slot.commit_offset,
        "and that segment is the one appended after this slot's marker: \
         segment {slot_segment}, marker {}",
        slot.commit_offset,
    );
    Ok(())
}

/// A lazy *writer* is still refused, and says which policy did it.
///
/// `header_tails` supplies the block tails a lazy writer needs, but a segment
/// cursor is a position in the resident index and a lazy handle has none. The
/// refusal is the honest answer, and it must survive the region being able to
/// answer the other half.
#[test]
fn a_lazy_writer_is_refused_by_the_segment_policy_not_by_the_region() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("writer.varve");
    build(&path, 80).unwrap();

    match VarveFile::open_lazy(combo_spec(), &path) {
        Err(Error::LazyWriterIndexPolicy { policy }) => {
            assert_eq!(policy, "segment_on_flush");
        }
        other => panic!("expected a policy refusal, got {other:?}"),
    }
}
