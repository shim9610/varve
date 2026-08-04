// Internal segments (index-residency spec §4A).
//
// A segment is varve's own lookup unit: `flush`/`commit` appends one segment
// record covering exactly the records that commit point added, and the record
// footer's `prev_same_block_offset` names the previous segment. Open confirms a
// record footer at the end of the file, walks that chain backwards, and never
// reads a data record.
//
// What these tests pin, in order: the option is inert when off; the chain is
// actually taken and frames commit points rather than records; every fallback
// path degrades to the scan and produces the identical index; and the two
// pre-existing checkpoint defects §4A.6 records are fixed.

use std::fs::OpenOptions;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use varve::{
    BlockDescriptor, BlockKind, COMMIT_BLOCK_ID, CommitPolicy, Endian, FormatSpec, IndexPolicy,
    IntegrityPolicy, MatrixBlockDescriptor, MatrixCommitDescriptor, MatrixCommitKind,
    MatrixDimensionDescriptor, MatrixDimensions, MatrixKey, RecordIndexEntry, SEGMENT_BLOCK_ID,
    TransactionMarkerMode, VarveBlock, VarveFile, VarveMatrixBlock, varve_format,
};

#[derive(Clone, Debug, PartialEq, VarveBlock)]
#[varve(id = 60, version = 1, kind = "fixed")]
struct Line {
    value: u32,
}

varve_format! {
    pub struct SegmentFormat {
        magic: b"SEGMENT0";
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
        integrity: crc32;
        index: block_offset_chain;
        commit: transaction_marker(on_flush);
        blocks: [Line];
    }
}

/// The declared format, with varve's segment chain switched on.
///
/// There is deliberately no way to say this in the layout DSL: a block is what
/// a declaration names, and segment granularity is varve's decision.
fn segment_spec() -> FormatSpec {
    SegmentFormat::spec().with_index_policy(
        SegmentFormat::spec()
            .index_policy
            .with_segment_on_flush(true),
    )
}

/// The same format with the chain off, for the byte-for-byte inertness check.
fn plain_spec() -> FormatSpec {
    SegmentFormat::spec()
}

fn write_lines(spec: FormatSpec, path: &Path, lines: u32, per_flush: u32) -> varve::Result<()> {
    let mut file = spec.create(path)?;
    for value in 0..lines {
        file.push(&Line { value })?;
        if (value + 1) % per_flush == 0 {
            file.flush()?;
        }
    }
    file.flush()?;
    Ok(())
}

/// The part of an index entry the two open paths must agree on, entry for
/// entry: what block, which sequence, where, how long, and committed or not.
type EntryShape = (u32, u64, u64, u64, bool);

fn entry_shape(entries: &[RecordIndexEntry]) -> Vec<EntryShape> {
    entries
        .iter()
        .map(|entry| {
            (
                entry.block_id,
                entry.sequence,
                entry.record_offset,
                entry.payload_len,
                entry.committed,
            )
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Inertness
// ---------------------------------------------------------------------------

#[test]
fn segment_option_defaults_off_and_changes_no_hash() {
    let plain = plain_spec();
    assert!(!plain.index_policy.segment_on_flush);
    // Bit 4 of the index-policy hash byte is new; a spec that leaves it off
    // must hash to exactly what it hashed to before the bit existed.
    assert_eq!(
        plain.computed_schema_hash(),
        SegmentFormat::spec().computed_schema_hash(),
    );
    assert_ne!(
        segment_spec().computed_schema_hash(),
        plain.computed_schema_hash(),
        "a file written with segments is not the file the plain spec describes",
    );
}

#[test]
fn all_off_writes_the_same_bytes_as_before() -> varve::Result<()> {
    let with_option = temp_path("inert_with_option");
    let without = temp_path("inert_without");
    // `IndexPolicy::new` cannot even name the new field, so this compares the
    // declared policy against one rebuilt through the builder with the segment
    // chain explicitly off.
    let rebuilt = plain_spec()
        .with_index_policy(IndexPolicy::new(true, false, true, false).with_segment_on_flush(false));
    write_lines(plain_spec(), &without, 64, 8)?;
    write_lines(rebuilt, &with_option, 64, 8)?;
    assert_eq!(std::fs::read(&*without)?, std::fs::read(&*with_option)?);
    Ok(())
}

#[test]
fn a_format_without_segments_writes_no_segment_record() -> varve::Result<()> {
    let path = temp_path("no_segment_records");
    write_lines(plain_spec(), &path, 64, 8)?;
    let file = plain_spec().open_readonly(&path)?;
    assert!(
        !file
            .index_entries()
            .iter()
            .any(|entry| entry.block_id == SEGMENT_BLOCK_ID),
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// The mechanism
// ---------------------------------------------------------------------------

#[test]
fn each_flush_appends_one_segment_record_last() -> varve::Result<()> {
    let path = temp_path("one_segment_per_flush");
    write_lines(segment_spec(), &path, 32, 8)?;

    let file = segment_spec().open_readonly(&path)?;
    let entries = file.index_entries();
    let segments: Vec<&RecordIndexEntry> = entries
        .iter()
        .filter(|entry| entry.block_id == SEGMENT_BLOCK_ID)
        .collect();
    // Four flushes of eight, and the trailing `flush()` adds nothing because
    // nothing was appended after the fourth.
    assert_eq!(segments.len(), 4, "one segment record per commit point");
    assert_eq!(
        entries.last().map(|entry| entry.block_id),
        Some(SEGMENT_BLOCK_ID),
        "the segment record must be the last record in the file",
    );
    // Every segment but the first names its predecessor; the first names none.
    assert_eq!(segments[0].prev_same_block_offset, None);
    for pair in segments.windows(2) {
        assert_eq!(
            pair[1].prev_same_block_offset,
            Some(pair[0].record_offset),
            "the footer chain must name the previous segment",
        );
    }
    Ok(())
}

#[test]
fn a_flush_that_added_nothing_writes_no_segment() -> varve::Result<()> {
    let path = temp_path("empty_flush_is_silent");
    let mut file = segment_spec().create(&path)?;
    file.push(&Line { value: 0 })?;
    file.flush()?;
    let after_first = std::fs::metadata(&*path)?.len();
    // The historical failure this guards: with the segment appended after the
    // commit marker, a naive "is the newest entry a marker" test sees the
    // segment, writes another marker, and the file grows on every idle flush.
    for _ in 0..8 {
        file.flush()?;
    }
    assert_eq!(std::fs::metadata(&*path)?.len(), after_first);
    Ok(())
}

#[cfg(feature = "scalable-fault-injection")]
#[test]
fn open_frames_commit_points_not_records() -> varve::Result<()> {
    let lines = 512u32;
    let per_flush = 16u32;

    let scanned = temp_path("frames_scanned");
    let chained = temp_path("frames_chained");
    write_lines(plain_spec(), &scanned, lines, per_flush)?;
    write_lines(segment_spec(), &chained, lines, per_flush)?;

    let before_scan = VarveFile::records_framed();
    let scan_index = entry_shape(plain_spec().open_readonly(&scanned)?.index_entries());
    let scan_frames = VarveFile::records_framed() - before_scan;

    let before_chain = VarveFile::records_framed();
    let chain_index = entry_shape(segment_spec().open_readonly(&chained)?.index_entries());
    let chain_frames = VarveFile::records_framed() - before_chain;

    // The scan frames every record it indexes.
    assert!(
        scan_frames >= u64::from(lines),
        "a scan must frame at least every data record: {scan_frames}",
    );
    // The chain frames one record per commit point and no data record at all.
    let commit_points = u64::from(lines / per_flush);
    assert_eq!(
        chain_frames, commit_points,
        "the chain must frame exactly the segment records: {chain_frames}",
    );
    // And it must not have got there by indexing less.
    assert_eq!(chain_index.len(), scan_index.len() + commit_points as usize);
    Ok(())
}

#[test]
fn the_chain_and_the_scan_produce_the_same_index() -> varve::Result<()> {
    let path = temp_path("chain_equals_scan");
    write_lines(segment_spec(), &path, 96, 7)?;

    let chained = entry_shape(segment_spec().open_readonly(&path)?.index_entries());
    // `AtOpen` verification is one of the two cases that must take the scan;
    // it indexes the same file through the other path.
    let scanned_spec = segment_spec().with_read_limits(
        segment_spec()
            .read_limits
            .with_integrity_verification(varve::IntegrityVerification::AtOpen),
    );
    let scanned = entry_shape(scanned_spec.open_readonly(&path)?.index_entries());
    assert_eq!(chained, scanned);
    Ok(())
}

#[test]
fn every_record_survives_the_chained_open() -> varve::Result<()> {
    let path = temp_path("records_survive");
    let lines = 200u32;
    write_lines(segment_spec(), &path, lines, 13)?;

    let file = segment_spec().open_readonly(&path)?;
    let blocks = file.blocks::<Line>()?;
    assert_eq!(blocks.len() as u32, lines);
    for value in 0..lines {
        assert_eq!(blocks.get(value as usize)?, Some(Line { value }));
    }
    Ok(())
}

#[test]
fn a_writer_reopen_keeps_appending_a_valid_chain() -> varve::Result<()> {
    let path = temp_path("writer_reopen");
    write_lines(segment_spec(), &path, 24, 8)?;

    // Reopen as a writer: the trailing segment record must survive the
    // uncommitted-tail truncation, and the next segment must chain onto it.
    {
        let mut file = segment_spec().open(&path)?;
        for value in 24..48 {
            file.push(&Line { value })?;
        }
        file.flush()?;
    }

    let file = segment_spec().open_readonly(&path)?;
    let segments: Vec<&RecordIndexEntry> = file
        .index_entries()
        .iter()
        .filter(|entry| entry.block_id == SEGMENT_BLOCK_ID)
        .collect();
    assert_eq!(segments.len(), 4);
    for pair in segments.windows(2) {
        assert_eq!(pair[1].prev_same_block_offset, Some(pair[0].record_offset));
    }
    let blocks = file.blocks::<Line>()?;
    assert_eq!(blocks.len(), 48);
    for value in 0..48u32 {
        assert_eq!(blocks.get(value as usize)?, Some(Line { value }));
    }
    Ok(())
}

#[test]
fn the_segment_record_sits_inside_the_commit_boundary() -> varve::Result<()> {
    let path = temp_path("inside_commit_boundary");
    write_lines(segment_spec(), &path, 16, 8)?;

    let file = segment_spec().open_readonly(&path)?;
    let entries = file.index_entries();
    let last_marker = entries
        .iter()
        .rposition(|entry| entry.block_id == COMMIT_BLOCK_ID)
        .expect("a marker-on-flush format writes markers");
    // Exactly one record follows the last marker, and it is the segment.
    assert_eq!(entries.len(), last_marker + 2);
    assert_eq!(entries[last_marker + 1].block_id, SEGMENT_BLOCK_ID);
    assert!(
        entries.iter().all(|entry| entry.committed),
        "a reopened index reports its whole committed prefix as committed",
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// Degradation: every one of these must fall back to the scan, not refuse
// ---------------------------------------------------------------------------

/// The index a scan of this file produces, for comparing a fallback against.
fn scanned_shape(path: &Path) -> varve::Result<Vec<EntryShape>> {
    let spec = plain_spec();
    Ok(entry_shape(spec.open_readonly(path)?.index_entries()))
}

#[test]
fn a_file_written_before_segments_were_enabled_falls_back() -> varve::Result<()> {
    let path = temp_path("preexisting_file");
    write_lines(plain_spec(), &path, 40, 8)?;

    // No chain at all: the last record is a commit marker, not a segment.
    let expected = scanned_shape(&path)?;
    let actual = entry_shape(segment_spec().open_readonly(&path)?.index_entries());
    assert_eq!(actual, expected);
    Ok(())
}

#[test]
fn a_record_appended_after_the_chain_falls_back() -> varve::Result<()> {
    let path = temp_path("record_after_chain");
    write_lines(segment_spec(), &path, 16, 8)?;

    // A writer that appends past its last flush leaves a data record at the
    // end of the file, so the probe at EOF finds no segment.
    {
        let mut file = segment_spec().open(&path)?;
        file.push(&Line { value: 999 })?;
    }
    let actual = entry_shape(segment_spec().open_readonly(&path)?.index_entries());
    assert_eq!(actual, scanned_shape(&path)?);
    // The trailing data record is uncommitted and stops at the boundary, but
    // the segment in front of it does not: it describes only records the marker
    // already committed, so it stays. Losing it here is what made the next
    // writer restart coverage and grow every later segment.
    assert_eq!(
        actual.last().map(|entry| entry.0),
        Some(SEGMENT_BLOCK_ID),
        "the segment behind an uncommitted record is still committed",
    );
    assert_eq!(
        actual.iter().filter(|entry| entry.0 == 60).count(),
        16,
        "every committed line is still indexed",
    );
    Ok(())
}

// COVERAGE, not a regression test: this still passes against the unfixed
// `committed_prefix_len`, so it does not discriminate. The defect it describes
// was reproduced by the review's own probe (`probe_b_segment_growth`), which
// read the raw `VSEG` entry counts off disk rather than through the resident
// index - the index drops the trailing segment under the unfixed code, so
// reading the growth through it measures the wrong record. Pinning this
// properly needs a disk-level reader the test file does not have.
//
// Review finding 3. `open; push; flush; push; drop` needs no crash
// - the writer has no flushing `Drop` - and each cycle used to take the newest
// segment down with the uncommitted record, so coverage restarted from the
// older segment and every later segment re-covered everything since. That is an
// O(N) payload and an O(N) allocation per flush on a workload that flushes per
// line, and it ended by exceeding `max_record_payload_len`, after which
// `write_index_segment_if_needed` swallowed the failure and no segment was ever
// written again.
#[test]
fn segments_do_not_grow_when_a_writer_stops_uncommitted() -> varve::Result<()> {
    let path = temp_path("no_segment_growth");
    {
        let mut file = segment_spec().create(&path)?;
        file.push(&Line { value: 0 })?;
        file.flush()?;
    }
    let mut covered = Vec::new();
    for cycle in 1..12u32 {
        {
            let mut file = segment_spec().open(&path)?;
            file.push(&Line { value: cycle * 10 })?;
            file.flush()?;
            // The uncommitted record that used to cost the segment.
            file.push(&Line {
                value: cycle * 10 + 1,
            })?;
        }
        let file = segment_spec().open_readonly(&path)?;
        let largest = file
            .index_entries()
            .iter()
            .filter(|entry| entry.block_id == SEGMENT_BLOCK_ID)
            .map(|entry| entry.payload_len)
            .max()
            .expect("the chain must survive an uncommitted tail");
        covered.push(largest);
    }
    // Every segment covers one line plus one marker, whatever the cycle.
    let first = covered[0];
    assert!(
        covered.iter().all(|len| *len == first),
        "segment payloads must not grow per cycle: {covered:?}",
    );
    Ok(())
}

// Regression, review finding 1. A writer never charges `Segments`, so a chain
// longer than the declared ceiling is a file the writer produced and its own
// reader refused - while a scan of the same bytes reads it perfectly. The
// refusal must fall back, not propagate.
#[test]
fn a_chain_past_the_segment_ceiling_still_opens() -> varve::Result<()> {
    let path = temp_path("segment_ceiling");
    let capped = segment_spec().with_read_limits(segment_spec().read_limits.with_max_segments(4));
    {
        let mut file = capped.create(&path)?;
        for value in 0..12u32 {
            file.push(&Line { value })?;
            file.flush()?;
        }
    }
    // Twelve commit points against a ceiling of four.
    let file = capped.open_readonly(&path)?;
    let blocks = file.blocks::<Line>()?;
    assert_eq!(
        blocks.len(),
        12,
        "the scan fallback must index every record"
    );
    for value in 0..12u32 {
        assert_eq!(blocks.get(value as usize)?, Some(Line { value }));
    }
    // And a writer open must not refuse it either.
    let mut writer = capped.open(&path)?;
    writer.push(&Line { value: 12 })?;
    writer.flush()?;
    Ok(())
}

// COVERAGE, not a regression test: an 8-byte, 8-mutation sweep does not reach
// the 4 of 22,208 single-bit flips the review's exhaustive sweep found, so this
// passes against the unfixed propagate list too. It pins that a mangled trailer
// falls back, which is worth having; it does not pin the limit-refusal path.
//
// Review finding 1, second reachability. `read_segment_tip_offset`
// returns an *unconfirmed* offset, so the framing read charges
// `RecordPayloadLen` against whatever `payload_len` sits there. A flipped bit in
// the eight trailer bytes at `file_len - 40` used to turn a scannable file into
// a hard `LimitExceeded`.
#[test]
fn a_corrupt_trailer_falls_back_rather_than_refusing() -> varve::Result<()> {
    let path = temp_path("corrupt_trailer");
    write_lines(segment_spec(), &path, 24, 8)?;
    let expected = scanned_shape(&path)?;
    let file_len = std::fs::metadata(&*path)?.len();

    // Sweep every bit of the trailer; each must open, and open as the scan does.
    for byte in 0..8u64 {
        let mut bytes = std::fs::read(&*path)?;
        let at = (file_len - 40 + byte) as usize;
        bytes[at] ^= 0xFF;
        let probe = temp_path("corrupt_trailer_probe");
        std::fs::write(&*probe, &bytes)?;
        let actual = entry_shape(segment_spec().open_readonly(&probe)?.index_entries());
        assert_eq!(actual, expected, "trailer byte {byte} must fall back");
    }
    Ok(())
}

// Regression, review finding 2. A chain can tile the whole append log and still
// contain no commit marker - structurally perfect, and a lie, because
// `truncate_uncommitted_tail_if_needed` runs next and cuts the file to the
// header. Accepting it left the handle holding an index of records no longer on
// disk, after which the next append landed on an offset the index already
// claimed and the file became unreadable by either path.
#[test]
fn a_chain_with_no_commit_marker_falls_back() -> varve::Result<()> {
    let path = temp_path("chain_without_marker");
    // Written by a policy that writes no markers, read by one that requires
    // them. Both declare the same schema hash, so the header check passes.
    let unmarked = segment_spec().with_commit_policy(varve::CommitPolicy::None);
    write_lines(unmarked, &path, 8, 4)?;

    let before = std::fs::read(&*path)?;
    // Read-only never truncates, and must agree with the scan.
    let actual = entry_shape(segment_spec().open_readonly(&path)?.index_entries());
    assert_eq!(actual, scanned_shape(&path)?);
    assert!(actual.is_empty(), "nothing in it is committed");
    assert_eq!(std::fs::read(&*path)?, before, "a read must not write");

    // A writer open truncates the uncommitted log - that is the pre-existing
    // transaction-marker contract - but the index it keeps must match what is
    // left, so the handle stays usable and the file stays readable.
    {
        let mut file = segment_spec().open(&path)?;
        assert!(file.index_entries().is_empty());
        file.push(&Line { value: 1 })?;
        file.flush()?;
    }
    let reopened = segment_spec().open_readonly(&path)?;
    assert_eq!(reopened.blocks::<Line>()?.len(), 1);
    Ok(())
}

#[test]
fn a_truncated_tail_falls_back() -> varve::Result<()> {
    let path = temp_path("truncated_tail");
    write_lines(segment_spec(), &path, 24, 8)?;
    let full = std::fs::metadata(&*path)?.len();

    // Cut the file inside the last segment record. The probe at EOF lands on
    // payload bytes rather than a footer.
    truncate_to(&path, full - 16)?;
    let expected = scanned_shape(&path)?;
    let actual = entry_shape(segment_spec().open_readonly(&path)?.index_entries());
    assert_eq!(actual, expected);
    assert!(!actual.is_empty());
    Ok(())
}

#[test]
fn a_corrupt_segment_payload_falls_back() -> varve::Result<()> {
    let path = temp_path("corrupt_segment");
    write_lines(segment_spec(), &path, 24, 8)?;
    let expected = scanned_shape(&path)?;

    let file = segment_spec().open_readonly(&path)?;
    let tip = file
        .index_entries()
        .iter()
        .rfind(|entry| entry.block_id == SEGMENT_BLOCK_ID)
        .cloned()
        .expect("a segment record");
    drop(file);
    // Flip a byte inside the tip's serialized entries: the record checksum
    // stops matching, so the walk refuses the link.
    flip_byte(&path, tip.payload_offset + 40)?;

    let actual = entry_shape(segment_spec().open_readonly(&path)?.index_entries());
    assert_eq!(actual, expected);
    Ok(())
}

#[test]
fn a_broken_chain_link_falls_back() -> varve::Result<()> {
    let path = temp_path("broken_link");
    write_lines(segment_spec(), &path, 32, 8)?;
    let expected = scanned_shape(&path)?;

    let file = segment_spec().open_readonly(&path)?;
    let tip = file
        .index_entries()
        .iter()
        .rfind(|entry| entry.block_id == SEGMENT_BLOCK_ID)
        .cloned()
        .expect("a segment record");
    drop(file);
    // The tip's footer names its predecessor; point it somewhere else. The
    // record checksum covers the footer, so this is refused at the tip.
    let footer = tip.footer_offset.expect("segments carry a record footer");
    flip_byte(&path, footer + 8)?;

    let actual = entry_shape(segment_spec().open_readonly(&path)?.index_entries());
    assert_eq!(actual, expected);
    Ok(())
}

#[test]
fn a_recovery_open_still_scans() -> varve::Result<()> {
    let path = temp_path("recovery_scans");
    write_lines(segment_spec(), &path, 24, 8)?;
    // Recovery's contract is to verify every record and truncate on the
    // mismatch; the chain reads no data record and cannot produce that
    // evidence, so recovery must not take it.
    let expected = scanned_shape(&path)?;
    let recovered = segment_spec().open_recover(&path)?;
    assert_eq!(entry_shape(recovered.index_entries()), expected);
    Ok(())
}

// ---------------------------------------------------------------------------
// A matrix file: the append log does not start at the header
// ---------------------------------------------------------------------------
//
// Every other test here writes a file whose append log begins immediately after
// the file header. A matrix file does not: the matrix region — commit map,
// bitmaps, slot region — sits between them, and the append log starts at
// `MatrixLayout::append_log_start()`.
//
// That offset is load-bearing on both sides of the chain and in a way that is
// invisible if it is only ever `header_len`. The writer resolves the first
// segment's `covered_start` from it, and the walk demands that the oldest link
// reach it exactly — `expected_end != append_start` refuses the chain. So a
// mismatch does not corrupt anything and does not raise anything: the walk
// fails, `load_index_from_segments` answers `Ok(None)`, and open falls back to
// the full scan. A matrix file would silently never take the chain, and the
// only symptom would be the speed.
//
// Every existing matrix test declares `IndexPolicy::ScanOnOpen`, so nothing
// crossed these two features before this test.

#[derive(Clone, Debug, PartialEq, Eq, VarveBlock)]
#[varve(id = 61, version = 1, kind = "matrix")]
struct Cell {
    value: u32,
}

impl VarveMatrixBlock for Cell {
    const DIMENSIONS: [&'static str; 2] = ["scan", "ch"];
    const CATEGORY: &'static str = "analysis";
    const SLOT_STRIDE: u64 = 4;
}

/// A matrix format that also carries the append-log block, with the chain on.
///
/// There is no DSL for this pairing — `with_matrix_spec` is a `FormatSpec`
/// builder and so is `with_segment_on_flush` — so it is spelled out here.
fn matrix_segment_spec() -> FormatSpec {
    static BLOCKS: &[BlockDescriptor] = &[
        BlockDescriptor {
            id: Cell::ID,
            name: "Cell",
            version: Cell::VERSION,
            kind: BlockKind::Matrix,
            fields: &[],
        },
        BlockDescriptor {
            id: Line::ID,
            name: "Line",
            version: Line::VERSION,
            kind: BlockKind::Fixed,
            fields: Line::FIELDS,
        },
    ];
    static DIMS: &[MatrixDimensionDescriptor] = &[
        MatrixDimensionDescriptor { name: "scan" },
        MatrixDimensionDescriptor { name: "ch" },
    ];
    static COMMITS: &[MatrixCommitDescriptor] = &[MatrixCommitDescriptor {
        name: Cell::CATEGORY,
        kind: MatrixCommitKind::Cell,
    }];
    static MATRIX_BLOCKS: &[MatrixBlockDescriptor] = &[MatrixBlockDescriptor {
        block_id: Cell::ID,
        dimensions: Cell::DIMENSIONS,
        category: Cell::CATEGORY,
        slot_stride: Cell::SLOT_STRIDE,
    }];
    FormatSpec::new(
        b"SEGMTX00",
        1,
        Endian::Little,
        0,
        IndexPolicy::new(true, false, true, false).with_segment_on_flush(true),
        IntegrityPolicy::Crc32,
        varve::RecoveryPolicy::Strict,
        varve::ManifestPolicy::None,
        BLOCKS,
    )
    .with_commit_policy(CommitPolicy::TransactionMarker(
        TransactionMarkerMode::OnFlush,
    ))
    // Borrowed from `SegmentFormat`, which declares every matrix ceiling; the
    // dimensions below are 4x4, so the region they size is a few pages.
    .with_read_limits(SegmentFormat::spec().read_limits)
    .with_matrix_spec(DIMS, COMMITS, MATRIX_BLOCKS)
}

fn matrix_dims() -> MatrixDimensions {
    MatrixDimensions::from_pairs([("scan", 4), ("ch", 4)])
}

/// Writes a matrix file with one committed cell and `lines` append-log records,
/// flushing every `per_flush`.
fn write_matrix_lines(
    spec: FormatSpec,
    path: &Path,
    lines: u32,
    per_flush: u32,
) -> varve::Result<()> {
    let mut writer = spec.create_writer_with_dims(path, matrix_dims())?;
    writer.write_matrix_cell(MatrixKey::new(1, 1), &Cell { value: 7 })?;
    writer.commit_matrix_cell::<Cell>(MatrixKey::new(1, 1))?;
    for value in 0..lines {
        writer.push(&Line { value })?;
        if (value + 1) % per_flush == 0 {
            writer.flush()?;
        }
    }
    writer.flush()?;
    Ok(())
}

#[test]
fn a_matrix_file_puts_the_append_log_after_the_matrix_region() -> varve::Result<()> {
    // The premise of the two tests below. If this ever stops holding, they stop
    // discriminating and say so here rather than passing quietly.
    let path = temp_path("matrix_region_is_between");
    write_matrix_lines(matrix_segment_spec(), &path, 16, 8)?;
    let reader = matrix_segment_spec().open_reader(&path)?;
    let first = reader
        .index_entries()
        .first()
        .expect("the file holds records")
        .record_offset;

    let plain = temp_path("matrix_region_is_between_plain");
    write_lines(segment_spec(), &plain, 16, 8)?;
    let plain_first = segment_spec()
        .open_readonly(&plain)?
        .index_entries()
        .first()
        .expect("the file holds records")
        .record_offset;

    assert!(
        first > plain_first,
        "a matrix file's first record must sit past the matrix region: \
         {first} vs {plain_first} without one",
    );
    Ok(())
}

#[test]
fn a_matrix_file_builds_and_walks_a_segment_chain() -> varve::Result<()> {
    let path = temp_path("matrix_chain");
    write_matrix_lines(matrix_segment_spec(), &path, 32, 8)?;

    let reader = matrix_segment_spec().open_reader(&path)?;
    let entries = reader.index_entries();
    let segments: Vec<&RecordIndexEntry> = entries
        .iter()
        .filter(|entry| entry.block_id == SEGMENT_BLOCK_ID)
        .collect();
    assert_eq!(segments.len(), 4, "one segment record per commit point");
    assert_eq!(
        entries.last().map(|entry| entry.block_id),
        Some(SEGMENT_BLOCK_ID),
        "the segment record must be the last record in the file",
    );

    // The chain the writer built must describe the same file the scan reads.
    // `AtOpen` verification is one of the two cases that must take the scan, so
    // this indexes the same bytes through the other path.
    let scanned_spec = matrix_segment_spec().with_read_limits(
        matrix_segment_spec()
            .read_limits
            .with_integrity_verification(varve::IntegrityVerification::AtOpen),
    );
    assert_eq!(
        entry_shape(entries),
        entry_shape(scanned_spec.open_reader(&path)?.index_entries()),
    );
    Ok(())
}

#[cfg(feature = "scalable-fault-injection")]
#[test]
fn a_matrix_file_open_frames_commit_points_not_records() -> varve::Result<()> {
    // The one that discriminates. The assertion above would still hold if the
    // chain were refused, because the fallback scan produces the identical
    // index — that is the whole point of the fallback. This counts framed
    // records instead: taking the chain frames one per commit point, and
    // falling back to the scan frames every data record in the file.
    let lines = 128u32;
    let per_flush = 16u32;
    let path = temp_path("matrix_frames");
    write_matrix_lines(matrix_segment_spec(), &path, lines, per_flush)?;

    let before = VarveFile::records_framed();
    let indexed = matrix_segment_spec()
        .open_reader(&path)?
        .index_entries()
        .len();
    let framed = VarveFile::records_framed() - before;

    let commit_points = u64::from(lines / per_flush);
    assert_eq!(
        framed, commit_points,
        "a matrix file must take the chain, not fall back to the scan: \
         {framed} records framed for {commit_points} commit points",
    );
    // And it must not have got there by indexing less: the data records, the
    // commit markers and the segment records.
    assert_eq!(
        indexed,
        lines as usize + 2 * commit_points as usize,
        "the chain must still account for every record",
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// §4A.6 defect 1: an oversized checkpoint must be skipped, not fatal
// ---------------------------------------------------------------------------

varve_format! {
    pub struct TinyRecordFormat {
        magic: b"TINYREC0";
        version: 1;
        limits {
            file_len: 8_589_934_592;
            records: 4_000_000;
            index_bytes: 536_870_912;
            scan_bytes: 8_589_934_592;
            // 73 bytes per checkpoint entry plus a 22-byte prefix, so a full
            // checkpoint stops fitting at four entries.
            record_payload: 320;
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
        index: checkpoint_on_flush;
        blocks: [Line];
    }
}

#[test]
fn flush_survives_a_checkpoint_that_cannot_fit() -> varve::Result<()> {
    let path = temp_path("oversized_checkpoint");
    // The floor for a fresh file is 16 eligible records, and the ceiling here
    // is four, so the checkpoint the cadence asks for can never be written.
    // Before the fix, `write_index_checkpoint` answered `LimitExceeded` and
    // `flush` propagated it: the file simply stopped being flushable.
    let mut file = TinyRecordFormat::create(&path)?;
    for value in 0..64u32 {
        file.push(&Line { value })?;
        file.flush()?;
    }
    drop(file);

    let file = TinyRecordFormat::open_readonly(&path)?;
    let blocks = file.blocks::<Line>()?;
    assert_eq!(blocks.len(), 64);
    for value in 0..64u32 {
        assert_eq!(blocks.get(value as usize)?, Some(Line { value }));
    }
    // Skipped, not written badly: no checkpoint record is in the file at all.
    assert!(
        !file
            .index_entries()
            .iter()
            .any(|entry| entry.block_id == varve::INDEX_BLOCK_ID),
    );
    Ok(())
}

#[test]
fn a_checkpoint_that_fits_is_still_written() -> varve::Result<()> {
    // The skip must be the ceiling talking, not the predicate going quiet.
    let path = temp_path("checkpoint_still_written");
    let spec = TinyRecordFormat::spec().with_read_limits(
        TinyRecordFormat::spec()
            .read_limits
            .with_max_record_payload_len(65_536),
    );
    let mut file = spec.create(&path)?;
    for value in 0..64u32 {
        file.push(&Line { value })?;
        file.flush()?;
    }
    drop(file);

    let file = spec.open_readonly(&path)?;
    assert!(
        file.index_entries()
            .iter()
            .any(|entry| entry.block_id == varve::INDEX_BLOCK_ID),
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// §4A.6 defect 2: a checkpoint must not leave an offset gap
// ---------------------------------------------------------------------------

varve_format! {
    pub struct GapFormat {
        magic: b"GAPCHECK";
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
        index: checkpoint_on_flush;
        blocks: [Line];
    }
}

// The tightened predicate itself is pinned by unit tests in `varve-core`
// (`a_checkpoint_that_skips_records_is_refused` and its far-end twin), which
// can hand it a checkpoint with a hole - no public API can produce one. What
// this pins is the other direction: the strict validation a replacement
// generation runs must still accept a checkpoint that tiles its coverage.
#[test]
fn a_re_encoded_checkpoint_still_validates_strictly() -> varve::Result<()> {
    use varve::ReplaceStrategy;

    let path = temp_path("checkpoint_gap");
    let mut file = GapFormat::create(&path)?;
    for value in 0..64u32 {
        file.push(&Line { value })?;
        file.flush()?;
    }
    // The generation this publishes is validated with strict checkpoint
    // checking, which is the caller of the tightened predicate.
    file.replace(0, &Line { value: 4242 }, ReplaceStrategy::FixedCopyOnWrite)?;
    drop(file);

    let file = GapFormat::open_readonly(&path)?;
    let blocks = file.blocks::<Line>()?;
    assert_eq!(blocks.len(), 64);
    assert_eq!(blocks.get(0)?, Some(Line { value: 4242 }));
    for value in 1..64u32 {
        assert_eq!(blocks.get(value as usize)?, Some(Line { value }));
    }
    Ok(())
}

#[test]
fn an_in_place_replacement_is_refused() -> varve::Result<()> {
    use varve::ReplaceStrategy;

    // An in-place replacement restamps the sequence and checksum of a record a
    // segment already describes, and nothing rewrites the segment behind it.
    // The chain reads no data record, so open could not notice. The capability
    // and the chain are alternatives, and this is where that is said.
    let path = temp_path("in_place_refused");
    write_lines(segment_spec(), &path, 16, 8)?;
    let mut file = segment_spec().open(&path)?;
    assert!(matches!(
        file.replace(0, &Line { value: 4242 }, ReplaceStrategy::FixedCopyOnWrite),
        Err(varve::Error::InvalidFormatSpec(_)),
    ));
    // The same format without the chain keeps the capability.
    let plain = temp_path("in_place_allowed");
    write_lines(plain_spec(), &plain, 16, 8)?;
    let mut file = plain_spec().open(&plain)?;
    file.replace(0, &Line { value: 4242 }, ReplaceStrategy::FixedCopyOnWrite)?;
    Ok(())
}

#[test]
fn a_replacement_generation_re_encodes_the_chain() -> varve::Result<()> {
    // A segment payload is record offsets, and a replacement generation moves
    // them. Copying the payload bytes would publish a file whose chain
    // describes the generation it replaced.
    let path = temp_path("replace_rewrites_chain");
    write_lines(segment_spec(), &path, 24, 8)?;

    {
        let mut file = segment_spec().open(&path)?;
        file.replace_block(0, &Line { value: 4242 })?;
        // The rebind rebuilt the writer's segment cursor from the new
        // generation; the next commit point must chain onto it.
        file.push(&Line { value: 24 })?;
        file.flush()?;
    }

    let file = segment_spec().open_readonly(&path)?;
    let segments: Vec<&RecordIndexEntry> = file
        .index_entries()
        .iter()
        .filter(|entry| entry.block_id == SEGMENT_BLOCK_ID)
        .collect();
    assert_eq!(segments.len(), 4);
    for pair in segments.windows(2) {
        assert_eq!(pair[1].prev_same_block_offset, Some(pair[0].record_offset));
    }
    // And the chained open agrees with a scan of the published generation.
    assert_eq!(
        entry_shape(file.index_entries()),
        scanned_shape(&path)?,
        "the re-encoded chain must describe the generation it was published in",
    );
    let blocks = file.blocks::<Line>()?;
    assert_eq!(blocks.len(), 25);
    assert_eq!(blocks.get(0)?, Some(Line { value: 4242 }));
    for value in 1..25u32 {
        assert_eq!(blocks.get(value as usize)?, Some(Line { value }));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

struct TempPath {
    path: PathBuf,
    _dir: tempfile::TempDir,
}

impl std::ops::Deref for TempPath {
    type Target = PathBuf;

    fn deref(&self) -> &PathBuf {
        &self.path
    }
}

impl AsRef<Path> for TempPath {
    fn as_ref(&self) -> &Path {
        &self.path
    }
}

fn temp_path(name: &str) -> TempPath {
    let dir = tempfile::tempdir().expect("create per-test temp directory");
    let path = dir
        .path()
        .join(format!("varve_segment_{name}_{}.vrv", std::process::id()));
    TempPath { path, _dir: dir }
}

fn truncate_to(path: &Path, len: u64) -> varve::Result<()> {
    let file = OpenOptions::new().write(true).open(path)?;
    file.set_len(len)?;
    Ok(())
}

fn flip_byte(path: &Path, offset: u64) -> varve::Result<()> {
    let mut file = OpenOptions::new().read(true).write(true).open(path)?;
    let mut byte = [0u8; 1];
    file.seek(SeekFrom::Start(offset))?;
    file.read_exact(&mut byte)?;
    byte[0] ^= 0xFF;
    file.seek(SeekFrom::Start(offset))?;
    file.write_all(&byte)?;
    file.sync_all()?;
    Ok(())
}

// §6.2, settled 2026-08-03. The spec asked whether `VBTL` supersedes
// `checkpoint_on_flush` or coexists with it. `VBTL` never shipped - the segment
// chain replaced it - so the live question was whether the *chain* supersedes
// the checkpoint, and it does on every axis: the checkpoint serialises the
// whole index from scratch on a geometric cadence, the chain carries the delta
// per commit point; the checkpoint stops fitting in a record past ~919,299
// entries, the chain is sized by the commit point; and open reads the chain
// while it merely validates a checkpoint and throws its entries away. With the
// chain on, a checkpoint is a periodic full copy of the index that nothing
// reads. Refused rather than silently cleared, so a format that declared it
// finds out.
#[test]
fn a_checkpoint_and_a_segment_chain_cannot_both_be_declared() {
    let both = segment_spec()
        .with_index_policy(segment_spec().index_policy.with_checkpoint_on_flush(true));
    assert!(matches!(
        both.validate(),
        Err(varve::Error::InvalidFormatSpec(
            "segment_on_flush supersedes checkpoint_on_flush; declare one"
        )),
    ));
    // Either alone is fine.
    assert!(segment_spec().validate().is_ok());
    assert!(
        plain_spec()
            .with_index_policy(plain_spec().index_policy.with_checkpoint_on_flush(true))
            .validate()
            .is_ok()
    );
}
