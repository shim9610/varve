//! The open digest: an open that reads no record at all.
//!
//! Three facts are the entire reason an open walks a file it is not indexing —
//! where the committed file ends, what sequence the next append takes, and
//! where each block's newest record sits. None of them is in any one record, so
//! today they are the by-product of framing all of them.
//!
//! A digest is those three written down at each commit point. It is **constant
//! in the record count**: a header plus twelve bytes per distinct block id,
//! against a segment's entry per record. That is the difference that lets it be
//! on for a file with a billion records.
//!
//! What these tests pin, in order: the option is inert when off; the digest is
//! actually taken and frames nothing; the file does not grow on idle flushes;
//! every way it can fail degrades to the scan with an identical answer; and a
//! digest open plus a `RecordMap` is a complete lazy path.

use std::fs::OpenOptions;
use std::io::{Seek, SeekFrom, Write};
use std::path::Path;

use varve::{
    FormatSpec, IndexPolicy, LazyOpenSource, VarveBlock, VarveFile, VarveWriter, varve_format,
};

#[derive(Clone, Debug, PartialEq, VarveBlock)]
#[varve(id = 70, version = 1, kind = "fixed")]
struct Line {
    value: u32,
}

#[derive(Clone, Debug, PartialEq, VarveBlock)]
#[varve(id = 71, version = 1, kind = "variable")]
struct Note {
    #[varve(field_id = 1)]
    body: String,
}

varve_format! {
    pub struct DigestFormat {
        magic: b"VDIGEST0";
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
        blocks: [Line, Note];
    }
}

fn digest_spec() -> FormatSpec {
    DigestFormat::spec().with_index_policy(
        DigestFormat::spec()
            .index_policy
            .with_open_digest_on_flush(true),
    )
}

fn plain_spec() -> FormatSpec {
    DigestFormat::spec()
}

fn write_lines(spec: FormatSpec, path: &Path, lines: u32, per_flush: u32) -> varve::Result<()> {
    let mut file = spec.create(path)?;
    for value in 0..lines {
        file.push(&Line { value })?;
        if value % 50 == 49 {
            file.push(&Note {
                body: format!("note {value}"),
            })?;
        }
        if per_flush > 0 && (value + 1) % per_flush == 0 {
            file.flush()?;
        }
    }
    file.flush()?;
    Ok(())
}

fn framed<T>(body: impl FnOnce() -> T) -> (T, u64) {
    let before = VarveFile::records_framed();
    let value = body();
    (value, VarveFile::records_framed() - before)
}

/// The index a scan produces, as the thing to compare every other route to.
fn scanned_shape(spec: FormatSpec, path: &Path) -> varve::Result<Vec<(u32, u64, u64)>> {
    let file = VarveFile::open_readonly(spec, path)?;
    Ok(file
        .index_entries()
        .iter()
        .map(|entry| (entry.block_id, entry.record_offset, entry.payload_len))
        .collect())
}

#[test]
fn the_option_is_inert_when_it_is_off() -> varve::Result<()> {
    let directory = tempfile::tempdir()?;
    let off = directory.path().join("off.varve");
    let reference = directory.path().join("reference.varve");
    write_lines(plain_spec(), &off, 200, 25)?;
    write_lines(plain_spec(), &reference, 200, 25)?;

    // Byte-identical, which is the whole of "defaults are inert".
    assert_eq!(std::fs::read(&off)?, std::fs::read(&reference)?);
    assert!(!plain_spec().index_policy.open_digest_on_flush);

    // And a file with no digest opens lazily by falling back, with the same
    // answer the scan gives.
    let (file, source) = VarveFile::open_readonly_lazy_with_report(plain_spec(), &off)?;
    assert_eq!(source, LazyOpenSource::FullScan);
    assert_eq!(
        file.block_tail_offset(Line::ID),
        VarveFile::open_readonly(plain_spec(), &off)?.block_tail_offset(Line::ID)
    );
    Ok(())
}

#[test]
fn a_digest_open_frames_no_record_at_all() -> varve::Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("frames.varve");
    write_lines(digest_spec(), &path, 2_000, 100)?;
    let spec = digest_spec();

    let (_scan, scan_frames) = framed(|| VarveFile::open_readonly(spec, &path).expect("scan open"));
    let ((file, source), digest_frames) =
        framed(|| VarveFile::open_readonly_lazy_with_report(spec, &path).expect("digest open"));

    assert_eq!(source, LazyOpenSource::Digest);
    assert!(
        scan_frames >= 2_000,
        "the scan frames every record: {scan_frames}"
    );
    assert_eq!(
        digest_frames, 1,
        "the digest open frames exactly the digest record and nothing else"
    );

    // It is not cheap by being wrong: the three facts must be the scan's.
    let scanned = VarveFile::open_readonly(spec, &path)?;
    assert_eq!(
        file.block_tail_offset(Line::ID),
        scanned.block_tail_offset(Line::ID)
    );
    assert_eq!(
        file.block_tail_offset(Note::ID),
        scanned.block_tail_offset(Note::ID)
    );
    assert!(file.block_tail_offset(Line::ID).is_some());
    Ok(())
}

#[test]
fn the_digest_open_keeps_no_directory_and_the_map_supplies_one() -> varve::Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("map.varve");
    write_lines(digest_spec(), &path, 600, 100)?;
    let spec = digest_spec();
    let expected = scanned_shape(spec, &path)?;

    let file = VarveFile::open_readonly_lazy(spec, &path)?;
    // No directory, and it says so by name rather than answering as if empty.
    assert!(matches!(
        file.blocks::<Line>(),
        Err(varve::Error::NoResidentDirectory { .. })
    ));

    // A map walked to the end is the scan's index, entry for entry — which is
    // what says the digest open's snapshot end is the scan's snapshot end. A
    // handle that stopped one record short would produce a shorter map here and
    // nothing else in this file would notice.
    let mut buffer = Vec::new();
    let mut map = file.record_map(&mut buffer)?;
    map.fill()?;
    let walked: Vec<_> = map
        .entries()
        .iter()
        .map(|entry| (entry.block_id, entry.record_offset, entry.payload_len))
        .collect();
    assert_eq!(walked, expected);

    // And the reads that were refused above are answered against the map.
    let mut entries = Vec::new();
    file.with_directory(&map)
        .block_entries_into::<Note>(&mut entries)?;
    assert_eq!(entries.len(), 12);
    let mut payload = Vec::new();
    file.read_payload_into(&entries[0], &mut payload)?;
    assert_eq!(payload.len() as u64, entries[0].payload_len);
    Ok(())
}

#[test]
fn the_block_chain_still_walks_from_a_digest_open() -> varve::Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("chain.varve");
    write_lines(digest_spec(), &path, 300, 50)?;
    let spec = digest_spec();

    // The reason the digest carries the tails at all: with no index and no
    // records framed, this is the whole of how a block is reached.
    let file = VarveFile::open_readonly_lazy(spec, &path)?;
    let mut notes = 0usize;
    for step in file.block_chain(Note::ID)? {
        let entry = step?;
        let note: Note = file.read_block_at::<Note>(entry.record_offset)?;
        assert!(note.body.starts_with("note "));
        notes += 1;
    }
    assert_eq!(notes, 6);
    Ok(())
}

#[test]
fn an_idle_flush_does_not_grow_the_file() -> varve::Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("idle.varve");
    let spec = digest_spec();
    let mut file = spec.create(&path)?;
    for value in 0..40 {
        file.push(&Line { value })?;
    }
    file.flush()?;
    let after_first = std::fs::metadata(&path)?.len();

    // The historical failure this guards, inherited from the segment path: the
    // digest is appended after the commit marker, so a naive "is anything
    // uncommitted" test sees the digest itself and writes another one on every
    // idle flush.
    for _ in 0..8 {
        file.flush()?;
    }
    assert_eq!(std::fs::metadata(&path)?.len(), after_first);
    drop(file);

    let (_file, source) = VarveFile::open_readonly_lazy_with_report(spec, &path)?;
    assert_eq!(source, LazyOpenSource::Digest);
    Ok(())
}

#[test]
fn a_record_appended_past_the_digest_falls_back() -> varve::Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("past.varve");
    let spec = digest_spec();
    write_lines(spec, &path, 100, 100)?;
    let expected = scanned_shape(spec, &path)?;

    // A writer that appends past its last flush leaves a data record behind the
    // digest, so the probe at the end of the file no longer finds one.
    {
        let mut writer = VarveWriter::open(spec, &path)?;
        writer.push(&Line { value: 9_999 })?;
        // Dropped without flushing: the record is down, the digest is stale.
        std::mem::forget(writer);
    }

    let (_file, source) = VarveFile::open_readonly_lazy_with_report(spec, &path)?;
    assert_eq!(source, LazyOpenSource::FullScan);
    // The uncommitted record is outside the committed boundary either way, so
    // the fallback answers exactly what a scan of the same file answers.
    assert_eq!(scanned_shape(spec, &path)?, expected);
    Ok(())
}

#[test]
fn a_rotted_digest_falls_back_instead_of_lying() -> varve::Result<()> {
    let directory = tempfile::tempdir()?;
    let spec = digest_spec();

    // Every byte of the digest's payload, one at a time, in a few places that
    // matter: the magic, the sequence, a block id, a tail offset, the trailer.
    // A flipped bit must never produce a *different* answer — only the scan's.
    for (label, from_end) in [
        ("trailer", 8u64 + 32),
        ("a tail offset", 8 + 32 + 4),
        ("a block id", 8 + 32 + 10),
        ("the sequence", 8 + 32 + 24),
    ] {
        let path = directory.path().join(format!("rot-{}.varve", from_end));
        write_lines(spec, &path, 120, 40)?;
        let expected = scanned_shape(spec, &path)?;
        let len = std::fs::metadata(&path)?.len();
        assert!(from_end < len);

        let mut file = OpenOptions::new().read(true).write(true).open(&path)?;
        file.seek(SeekFrom::Start(len - from_end))?;
        let mut byte = [0u8; 1];
        std::io::Read::read_exact(&mut file, &mut byte)?;
        file.seek(SeekFrom::Start(len - from_end))?;
        file.write_all(&[byte[0] ^ 0x01])?;
        file.sync_all()?;
        drop(file);

        let (_handle, source) = VarveFile::open_readonly_lazy_with_report(spec, &path)?;
        assert_eq!(
            source,
            LazyOpenSource::FullScan,
            "flipping {label} must invalidate the digest, not be trusted"
        );
        assert_eq!(
            scanned_shape(spec, &path)?,
            expected,
            "and the fallback answers what the file actually holds"
        );
    }
    Ok(())
}

#[test]
fn the_digest_costs_the_same_bytes_however_many_records_there_are() -> varve::Result<()> {
    let directory = tempfile::tempdir()?;
    let spec = digest_spec();
    let plain = plain_spec();

    // The claim the whole design rests on: the digest is constant in the record
    // count. Measured as the file-size difference against the same file written
    // without it, at two record counts an order of magnitude apart.
    let mut overheads = Vec::new();
    for lines in [200u32, 2_000] {
        let with = directory.path().join(format!("with-{lines}.varve"));
        let without = directory.path().join(format!("without-{lines}.varve"));
        // One flush, so one digest: this measures the digest, not the cadence.
        write_lines(spec, &with, lines, 0)?;
        write_lines(plain, &without, lines, 0)?;
        overheads.push(std::fs::metadata(&with)?.len() - std::fs::metadata(&without)?.len());
    }
    assert_eq!(
        overheads[0], overheads[1],
        "ten times the records must cost the digest nothing: {overheads:?}"
    );
    // Three block ids in this file (Line, Note, the commit marker), so the
    // record is a 32-byte header, a 20-byte prefix, 3 x 12 bytes of tails, an
    // 8-byte trailer and a 32-byte footer.
    assert_eq!(overheads[0], 32 + 20 + 3 * 12 + 8 + 32);
    Ok(())
}

#[test]
fn a_digest_requires_the_chain_it_hands_out_entry_points_to() {
    let refused = DigestFormat::spec()
        .with_index_policy(
            IndexPolicy::new(true, false, false, false).with_open_digest_on_flush(
                // `with_open_digest_on_flush` turns the chain on itself, so the
                // refusal has to be reached by putting it back off afterwards —
                // which is exactly the shape a hand-built policy can take.
                true,
            ),
        )
        .with_index_policy(IndexPolicy {
            scan_on_open: true,
            checkpoint_on_flush: false,
            block_offset_chain: false,
            keyed_offset_chain: false,
            segment_on_flush: false,
            open_digest_on_flush: true,
        })
        .validate();
    assert!(matches!(
        refused,
        Err(varve::Error::InvalidFormatSpec(
            "open_digest_on_flush requires block_offset_chain"
        ))
    ));
}

/// Both tail records at once.
///
/// The digest is written after the segment and is itself a resident record, so
/// it pushes `index.len()` past the segment cursor — and an unguarded
/// `needs_index_segment` then reads that as "a commit point was left
/// uncovered". It writes a segment, which makes the digest stale, which writes
/// a digest, which pushes the cursor again. The commit that introduced the
/// digest claimed this was fixed and tested only the digest alone; this is the
/// measurement that claim needed.
#[test]
fn both_tail_records_at_once_do_not_grow_an_idle_file() -> varve::Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("both.varve");
    let spec = DigestFormat::spec().with_index_policy(
        DigestFormat::spec()
            .index_policy
            .with_segment_on_flush(true)
            .with_open_digest_on_flush(true),
    );

    let mut file = spec.create(&path)?;
    for value in 0..60 {
        file.push(&Line { value })?;
    }
    file.flush()?;
    let after_first = std::fs::metadata(&path)?.len();

    for _ in 0..8 {
        file.flush()?;
    }
    assert_eq!(
        std::fs::metadata(&path)?.len(),
        after_first,
        "eight idle flushes must write nothing"
    );

    // And a real append after them still closes the commit point once.
    file.push(&Line { value: 999 })?;
    file.flush()?;
    let after_second = std::fs::metadata(&path)?.len();
    assert!(after_second > after_first);
    for _ in 0..8 {
        file.flush()?;
    }
    assert_eq!(std::fs::metadata(&path)?.len(), after_second);
    drop(file);

    // Both routes still work on the same file, and agree.
    let (lazy, source) = VarveFile::open_readonly_lazy_with_report(spec, &path)?;
    assert_eq!(source, LazyOpenSource::Digest);
    let scanned = VarveFile::open_readonly(spec, &path)?;
    assert_eq!(
        lazy.block_tail_offset(Line::ID),
        scanned.block_tail_offset(Line::ID)
    );

    // The segment chain must still describe the file: a chained open frames one
    // record per commit point, and the digest must not have broken that.
    let (chained, frames) = framed(|| VarveFile::open_readonly(spec, &path).expect("chained open"));
    assert!(
        frames < 20,
        "the segment chain must still be taken, not fallen back from: {frames} framed"
    );
    assert_eq!(
        chained
            .index_entries()
            .iter()
            .map(|entry| (entry.block_id, entry.record_offset))
            .collect::<Vec<_>>(),
        scanned
            .index_entries()
            .iter()
            .map(|entry| (entry.block_id, entry.record_offset))
            .collect::<Vec<_>>()
    );
    Ok(())
}

/// The digest next to a checkpoint.
///
/// `checkpoint_on_flush` fires on a count of records written since the last
/// checkpoint. A digest is written because a commit point closed, not because a
/// record was added, so counting it in that tail would make the cadence a
/// function of how often the file is flushed. It is exempted along with the
/// commit marker and the segment record.
///
/// What it is *not* exempted from, and must not be: the threshold is geometric
/// in the checkpoint's own index position, and a digest is a resident record, so
/// it enlarges the index and widens the next threshold. That is the same effect
/// a commit marker has and is the correct one — the checkpoint has to serialize
/// the larger index too. So the direction here is one-sided on purpose: digests
/// may make checkpoints rarer, never more frequent.
#[test]
fn a_digest_does_not_advance_the_checkpoint_cadence() -> varve::Result<()> {
    let directory = tempfile::tempdir()?;
    let with = directory.path().join("cadence-with.varve");
    let without = directory.path().join("cadence-without.varve");

    let checkpointed = DigestFormat::spec().with_index_policy(
        DigestFormat::spec()
            .index_policy
            .with_checkpoint_on_flush(true),
    );
    let both =
        checkpointed.with_index_policy(checkpointed.index_policy.with_open_digest_on_flush(true));

    // Flush per record, so the cadence sees the maximum number of digests it
    // could possibly be confused by.
    let count = |spec: FormatSpec, path: &Path| -> varve::Result<usize> {
        let mut file = spec.create(path)?;
        for value in 0..300 {
            file.push(&Line { value })?;
            file.flush()?;
        }
        drop(file);
        Ok(VarveFile::open_readonly(spec, path)?
            .index_entries()
            .iter()
            .filter(|entry| entry.block_id == varve::INDEX_BLOCK_ID)
            .count())
    };
    let (checkpoints_with, checkpoints_without) =
        (count(both, &with)?, count(checkpointed, &without)?);
    assert!(
        checkpoints_without > 0,
        "the fixture must checkpoint at all"
    );
    assert!(
        checkpoints_with <= checkpoints_without,
        "a digest must never make checkpoints more frequent: \
         {checkpoints_with} with, {checkpoints_without} without"
    );
    // And the digest is still usable on a file that also checkpoints.
    let (_handle, source) = VarveFile::open_readonly_lazy_with_report(both, &with)?;
    assert_eq!(source, LazyOpenSource::Digest);
    Ok(())
}

/// A writer reopening a file that ends in a digest.
#[test]
fn a_reopened_writer_does_not_pile_up_digests() -> varve::Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("reopen.varve");
    let spec = digest_spec();
    write_lines(spec, &path, 40, 0)?;

    let digests = |path: &Path| -> varve::Result<usize> {
        Ok(VarveFile::open_readonly(spec, path)?
            .index_entries()
            .iter()
            .filter(|entry| entry.block_id == varve::OPEN_DIGEST_BLOCK_ID)
            .count())
    };
    assert_eq!(digests(&path)?, 1);

    // Reopen and flush without writing anything: the seeded
    // `last_resident_block_id` must say the file already ends in a digest.
    for _ in 0..3 {
        let mut writer = VarveWriter::open(spec, &path)?;
        writer.flush()?;
    }
    assert_eq!(digests(&path)?, 1, "an idle reopen must write no digest");

    // A reopen that actually appends closes its own commit point once.
    {
        let mut writer = VarveWriter::open(spec, &path)?;
        writer.push(&Line { value: 77 })?;
        writer.flush()?;
        writer.flush()?;
    }
    assert_eq!(digests(&path)?, 2);
    let (_handle, source) = VarveFile::open_readonly_lazy_with_report(spec, &path)?;
    assert_eq!(source, LazyOpenSource::Digest);
    Ok(())
}

/// Recovery over a file whose last record is a digest.
#[test]
fn open_recover_accepts_a_file_that_ends_in_a_digest() -> varve::Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("recover.varve");
    let spec = digest_spec();
    write_lines(spec, &path, 80, 40)?;
    let before = scanned_shape(spec, &path)?;
    let before_len = std::fs::metadata(&path)?.len();

    // The digest sits after the commit marker, so a recovery that treated it as
    // an uncommitted tail would truncate the file back past it.
    {
        let recovered = VarveFile::open_recover(spec, &path)?;
        drop(recovered);
    }
    assert_eq!(std::fs::metadata(&path)?.len(), before_len);
    assert_eq!(scanned_shape(spec, &path)?, before);
    let (_handle, source) = VarveFile::open_readonly_lazy_with_report(spec, &path)?;
    assert_eq!(source, LazyOpenSource::Digest);
    Ok(())
}

/// `replace_rewrite` is not the rewrite a digest format can reach.
///
/// A digest requires `block_offset_chain`, which requires a record footer, and
/// `replace_rewrite` refuses record-footer formats outright. Pinned here
/// because the digest branch in that loop would otherwise look like tested code
/// and is in fact unreachable: it is there so the two rewrite loops do not
/// disagree if the refusal is ever lifted.
#[test]
fn replace_rewrite_is_unreachable_for_a_digest_format() -> varve::Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("no-rewrite.varve");
    let spec = digest_spec();
    write_lines(spec, &path, 10, 0)?;
    let mut file = spec.open(&path)?;
    assert!(matches!(
        file.replace_rewrite(0, &Line { value: 1 }),
        Err(varve::Error::InvalidFormatSpec(
            "replace is not supported for record-footer formats"
        ))
    ));
    Ok(())
}

/// The rewrite a digest format *can* reach, and a digest is nothing but offsets.
///
/// `replace_block` republishes a whole generation. Its loop already re-encodes
/// segment payloads because they are record offsets; a digest payload is block
/// tails and its own start offset, so it needs the same.
///
/// **Two things had to be right for this to test anything**, and neither was
/// obvious — the first two versions of it passed against an implementation that
/// copied the digest verbatim:
///
/// 1. The replacement has to change a length. A same-size one moves no offset,
///    so a copied digest stays valid.
/// 2. The published generation has to be read *as published*. Appending and
///    flushing after the replacement writes a fresh, correct digest at the end
///    of the file, which is what the next open finds — the stale one is still
///    in there and no longer the last record, so nothing looks at it.
#[test]
fn a_replacement_generation_re_encodes_the_digest() -> varve::Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("replace.varve");
    let spec = digest_spec();

    let mut file = spec.create(&path)?;
    for value in 0..24 {
        file.push(&Line { value })?;
        if value == 4 {
            file.push(&Note {
                body: "x".repeat(512),
            })?;
        }
    }
    file.flush()?;
    // The `Line` tail, not the `Note` tail: the note sits near the front, so
    // shortening it moves everything *behind* it and leaves its own offset
    // where it was.
    let old_line_tail = VarveFile::open_readonly(spec, &path)?
        .block_tail_offset(Line::ID)
        .expect("a line tail");

    // Block-relative: the first (and only) `Note`, replaced with a much shorter
    // body so every record behind it moves.
    file.replace_block(
        0,
        &Note {
            body: "y".to_string(),
        },
    )?;
    // Closed here, with no further append: the last record in the published
    // generation is the digest the rewrite produced, and that is the one under
    // test.
    drop(file);

    let scanned = VarveFile::open_readonly(spec, &path)?;
    assert_ne!(
        scanned.block_tail_offset(Line::ID),
        Some(old_line_tail),
        "the fixture must actually move offsets, or a copied digest would \
         still be valid and this proves nothing"
    );

    let (handle, source) = VarveFile::open_readonly_lazy_with_report(spec, &path)?;
    assert_eq!(
        source,
        LazyOpenSource::Digest,
        "the digest must describe the generation it was published in"
    );
    assert_eq!(
        handle.block_tail_offset(Line::ID),
        scanned.block_tail_offset(Line::ID)
    );
    assert_eq!(
        handle.block_tail_offset(Note::ID),
        scanned.block_tail_offset(Note::ID)
    );
    // And the tails point into the rewritten generation, not the old one.
    let note: Note =
        handle.read_block_at::<Note>(handle.block_tail_offset(Note::ID).expect("a note tail"))?;
    assert_eq!(note.body, "y");

    // The writer's own state survived the rebind: the next commit point closes
    // onto the new generation rather than repeating it.
    let mut file = spec.open(&path)?;
    file.push(&Line { value: 24 })?;
    file.flush()?;
    drop(file);
    let (_handle, source) = VarveFile::open_readonly_lazy_with_report(spec, &path)?;
    assert_eq!(source, LazyOpenSource::Digest);
    Ok(())
}
