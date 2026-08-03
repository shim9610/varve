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
    COMMIT_BLOCK_ID, FormatSpec, IndexPolicy, RecordIndexEntry, SEGMENT_BLOCK_ID, VarveBlock,
    VarveFile, varve_format,
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
