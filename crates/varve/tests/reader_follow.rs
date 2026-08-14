//! A reader that catches up without reopening.
//!
//! A handle fixes its snapshot length at open and reads positionally against
//! it, which is what lets one handle serve concurrent readers while a writer
//! appends. The cost was that the handle never saw the appends either: measured
//! before this existed, a handle open on a five-record file that then grew to
//! fifteen went on reporting 6 records while a fresh open reported 17.
//!
//! `follow()` closes that, and the things worth pinning are the ones that are
//! easy to get wrong rather than the happy path: it must adopt exactly what a
//! fresh open would adopt, it must stop at the same commit boundary, it must
//! cost the tail rather than the file, and it must **not** wander onto a
//! different generation when the pathname is replaced under it.

#![cfg(all(feature = "integrity", feature = "scalable-fault-injection"))]

use varve::{VarveBlock, varve_format};

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
    pub struct FollowFormat {
        magic: b"VFOLLOW1";
        version: 1;
        endian: little;
        schema_hash: computed;
        integrity: crc32;
        index: block_offset_chain;
        commit: transaction_marker(on_flush);
        blocks: [Reading, Label];
    }
}

// The same declaration with no commit marker, so "visible" means "framed"
// rather than "committed". The boundary rules differ and both are pinned.
varve_format! {
    pub struct FollowNoMarker {
        magic: b"VFOLLOW2";
        version: 1;
        endian: little;
        schema_hash: computed;
        integrity: crc32;
        index: block_offset_chain;
        commit: none;
        blocks: [Reading, Label];
    }
}

fn spec() -> varve::FormatSpec {
    FollowFormat::spec()
}

fn records(file: &varve::VarveFile) -> varve::Result<usize> {
    let mut buffer = Vec::new();
    let mut walk = file.record_map(&mut buffer)?;
    let mut count = 0;
    while walk.advance()?.is_some() {
        count += 1;
    }
    Ok(count)
}

/// The claim, against the measurement that motivated it.
#[test]
fn a_handle_catches_up_to_records_written_after_it_opened() -> varve::Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("follow.varve");

    let mut writer = spec().create(&path)?;
    for value in 0..5 {
        writer.push(&Reading { value })?;
    }
    writer.flush()?;

    let mut reader = varve::VarveFile::open_readonly(spec(), &path)?;
    let before = records(&reader)?;

    for value in 5..15 {
        writer.push(&Reading { value })?;
    }
    writer.push(&Label {
        text: "after".into(),
    })?;
    writer.flush()?;

    // The premise: without following, the handle is still at its open state.
    assert_eq!(
        records(&reader)?,
        before,
        "a handle must not drift on its own — the snapshot is what bounds it",
    );

    let gained = reader.follow()?;
    assert!(
        gained > 0,
        "the file grew and follow reported {gained} bytes"
    );

    // And now it answers what a fresh open answers, record for record.
    let fresh = varve::VarveFile::open_readonly(spec(), &path)?;
    assert_eq!(records(&reader)?, records(&fresh)?);
    for block in [Reading::ID, Label::ID] {
        assert_eq!(
            reader.block_tail_offset(block),
            fresh.block_tail_offset(block),
            "block {block}'s tail",
        );
    }
    // Not merely equal counts: the newest record reads back through the
    // followed handle, which is the read the old snapshot would have refused.
    let tail = reader.block_tail_offset(Label::ID).expect("a Label");
    let label: Label = reader.read_block_at(tail)?;
    assert_eq!(label.text, "after");
    Ok(())
}

/// Following twice in a row is following once.
///
/// The second call has nothing to adopt and must say so rather than re-framing
/// the run it already holds — that is the difference between a stream reader
/// that costs the tail and one that costs the file every time it checks.
#[test]
fn following_a_file_that_did_not_grow_costs_nothing() -> varve::Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("idle.varve");

    let mut writer = spec().create(&path)?;
    for value in 0..20 {
        writer.push(&Reading { value })?;
    }
    writer.flush()?;

    let mut reader = varve::VarveFile::open_readonly(spec(), &path)?;
    for value in 20..40 {
        writer.push(&Reading { value })?;
    }
    writer.flush()?;

    assert!(reader.follow()? > 0);
    let before = varve::VarveFile::records_framed();
    assert_eq!(reader.follow()?, 0, "nothing was appended in between");
    assert_eq!(
        varve::VarveFile::records_framed() - before,
        0,
        "an idle follow must frame no records at all",
    );
    Ok(())
}

/// The cost is the tail, not the file.
///
/// This is the property that makes a stream reader viable: a handle that
/// followed a file through `N` appends must have framed `N` records in total,
/// not `N` per call. Measured as the delta of the framing counter across one
/// follow of a fixed-size tail on two files of very different size.
#[test]
fn the_cost_of_a_follow_is_the_tail_and_not_the_file() -> varve::Result<()> {
    fn measure(directory: &std::path::Path, initial: u64) -> varve::Result<u64> {
        let path = directory.join(format!("cost-{initial}.varve"));
        let mut writer = spec().create(&path)?;
        for value in 0..initial {
            writer.push(&Reading { value })?;
        }
        writer.flush()?;

        let mut reader = varve::VarveFile::open_readonly(spec(), &path)?;
        // The same ten records appended to both files.
        for value in initial..initial + 10 {
            writer.push(&Reading { value })?;
        }
        writer.flush()?;

        let before = varve::VarveFile::records_framed();
        assert!(reader.follow()? > 0);
        Ok(varve::VarveFile::records_framed() - before)
    }

    let directory = tempfile::tempdir()?;
    let small = measure(directory.path(), 50)?;
    let large = measure(directory.path(), 2_000)?;
    assert_eq!(
        small, large,
        "the file was fortyfold larger and the follow framed {small} then \
         {large} records; it is reading the whole file",
    );
    // Ten records and the commit marker that made them visible.
    assert!(large <= 12, "one follow framed {large} records");
    Ok(())
}

/// Uncommitted records are not adopted, and the same call adopts them once the
/// marker lands.
///
/// The boundary a follow stops at has to be the boundary an open stops at, or
/// a handle that followed would expose records a fresh reader would not.
#[test]
fn a_follow_stops_at_the_commit_boundary_an_open_stops_at() -> varve::Result<()> {
    let spec = spec().with_commit_policy(varve::CommitPolicy::TransactionMarker(
        varve::TransactionMarkerMode::Explicit,
    ));
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("boundary.varve");

    let mut writer = spec.create(&path)?;
    for value in 0..10 {
        writer.push(&Reading { value })?;
    }
    writer.commit()?;

    let mut reader = varve::VarveFile::open_readonly(spec, &path)?;
    let committed = records(&reader)?;

    // Appended and flushed to disk, but no marker covers them under Explicit.
    for value in 10..20 {
        writer.push(&Reading { value })?;
    }
    writer.flush()?;
    assert!(std::fs::metadata(&path)?.len() > 0);

    assert_eq!(
        reader.follow()?,
        0,
        "records past the last marker must not be adopted",
    );
    assert_eq!(records(&reader)?, committed);
    // And a fresh open agrees, which is the standard this is held to.
    let fresh = varve::VarveFile::open_readonly(spec, &path)?;
    assert_eq!(records(&fresh)?, committed);

    // Now commit, and the same call adopts the whole run.
    writer.commit()?;
    assert!(reader.follow()? > 0);
    let fresh = varve::VarveFile::open_readonly(spec, &path)?;
    assert_eq!(records(&reader)?, records(&fresh)?);
    assert!(records(&reader)? > committed);
    Ok(())
}

/// A markerless format has no commit boundary, so a framed record is a visible
/// one — and the follow has to use *that* rule rather than the marker rule.
#[test]
fn a_markerless_format_follows_to_the_last_complete_record() -> varve::Result<()> {
    let spec = FollowNoMarker::spec();
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("markerless.varve");

    let mut writer = spec.create(&path)?;
    for value in 0..5 {
        writer.push(&Reading { value })?;
    }
    writer.flush()?;

    let mut reader = varve::VarveFile::open_readonly(spec, &path)?;
    for value in 5..15 {
        writer.push(&Reading { value })?;
    }
    writer.flush()?;

    assert!(reader.follow()? > 0);
    let fresh = varve::VarveFile::open_readonly(spec, &path)?;
    assert_eq!(records(&reader)?, records(&fresh)?);
    assert_eq!(records(&reader)?, 15);
    Ok(())
}

/// **The one that must not go wrong.** A follow must never wander onto a
/// different generation.
///
/// The replacement paths publish by renaming a new file over the pathname. This
/// handle's descriptor still names the old object, whose offsets are the ones
/// it holds; framing the *new* file's bytes at those offsets would be reading
/// one file's records through another file's index. So a follow after a
/// republish must answer `0` and leave the handle exactly where it was —
/// pinned to a complete, self-consistent old generation.
#[test]
fn a_follow_does_not_cross_a_republished_generation() -> varve::Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("generation.varve");

    // The Label goes in first, so replacing it with a longer one moves every
    // Reading after it. Replacing a trailing record would move nothing and the
    // test would prove nothing.
    let mut writer = spec().create(&path)?;
    writer.push(&Label {
        text: "original".into(),
    })?;
    for value in 0..20 {
        writer.push(&Reading { value })?;
    }
    writer.flush()?;
    drop(writer);

    let mut reader = varve::VarveFile::open_readonly(spec(), &path)?;
    let held = records(&reader)?;
    let held_tail = reader.block_tail_offset(Reading::ID).expect("a Reading");
    let held_value: Reading = reader.read_block_at(held_tail)?;

    // Republish the pathname with a longer record, so every offset moves.
    let mut writer = varve::VarveWriter::open(spec(), &path)?;
    writer.replace_block(
        0,
        &Label {
            text: "a replacement long enough to move every record after it".into(),
        },
    )?;
    drop(writer);
    let fresh = varve::VarveFile::open_readonly(spec(), &path)?;
    assert_ne!(
        fresh.block_tail_offset(Reading::ID),
        Some(held_tail),
        "the fixture must actually have moved the records",
    );

    assert_eq!(
        reader.follow()?,
        0,
        "the handle follows the object it opened, not the pathname",
    );
    assert_eq!(records(&reader)?, held);
    assert_eq!(reader.block_tail_offset(Reading::ID), Some(held_tail));
    // The decisive read: that offset means something different in the new file,
    // and the handle must still resolve it against the old one.
    let now: Reading = reader.read_block_at(held_tail)?;
    assert_eq!(
        now, held_value,
        "the old generation is still whole and still this handle's",
    );
    let label: Label =
        reader.read_block_at(reader.block_tail_offset(Label::ID).expect("a Label"))?;
    assert_eq!(label.text, "original");
    Ok(())
}

/// A handle that keeps no directory follows too, and gets the same tails.
///
/// This is the pairing that matters for a large file: no resident index, so
/// the follow updates the block tails and the snapshot and nothing else.
#[test]
fn a_directoryless_handle_follows_its_tails() -> varve::Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("nodir.varve");

    let mut writer = spec().create(&path)?;
    for value in 0..10 {
        writer.push(&Reading { value })?;
    }
    writer.flush()?;

    let mut scratch = Vec::new();
    let mut reader =
        varve::VarveFile::open_readonly_without_directory(spec(), &path, &mut scratch)?;
    assert!(matches!(
        reader.blocks::<Reading>(),
        Err(varve::Error::NoResidentDirectory { .. })
    ));

    for value in 10..30 {
        writer.push(&Reading { value })?;
    }
    writer.push(&Label {
        text: "tail".into(),
    })?;
    writer.flush()?;

    assert!(reader.follow()? > 0);
    let fresh = varve::VarveFile::open_readonly(spec(), &path)?;
    for block in [Reading::ID, Label::ID] {
        assert_eq!(
            reader.block_tail_offset(block),
            fresh.block_tail_offset(block),
        );
    }
    // Still directoryless: following must not quietly start retaining one.
    assert!(matches!(
        reader.blocks::<Reading>(),
        Err(varve::Error::NoResidentDirectory { .. })
    ));
    Ok(())
}

/// A write handle answers `0`: it is already at the object's end.
#[test]
fn a_write_handle_has_nothing_to_follow() -> varve::Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("writer.varve");
    let mut writer = spec().create(&path)?;
    for value in 0..10 {
        writer.push(&Reading { value })?;
    }
    writer.flush()?;
    assert_eq!(writer.follow()?, 0);
    Ok(())
}

/// A block with no record in the followed range keeps the tail it had.
///
/// The scan of a *range* reports only the blocks that appear in it, so adopting
/// its tails wholesale would report every other block as having no records at
/// all — and a block tail is the entry point to the offset chain, so losing one
/// makes every record of that block unreachable through this handle. Merging is
/// what the range scan requires and replacing is the natural mistake, so this
/// writes the Label once, before the reader opens, and then appends nothing but
/// Readings.
#[test]
fn a_block_with_no_record_in_the_followed_range_keeps_its_tail() -> varve::Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("merge.varve");

    let mut writer = spec().create(&path)?;
    writer.push(&Label {
        text: "only one, and before the reader opens".into(),
    })?;
    for value in 0..5 {
        writer.push(&Reading { value })?;
    }
    writer.flush()?;

    let mut reader = varve::VarveFile::open_readonly(spec(), &path)?;
    let label_tail = reader.block_tail_offset(Label::ID).expect("a Label");

    // Nothing but Readings from here, so the followed range holds no Label.
    for value in 5..25 {
        writer.push(&Reading { value })?;
    }
    writer.flush()?;
    assert!(reader.follow()? > 0);

    assert_eq!(
        reader.block_tail_offset(Label::ID),
        Some(label_tail),
        "the Label has no record in the followed range and must keep its tail",
    );
    let label: Label = reader.read_block_at(label_tail)?;
    assert_eq!(label.text, "only one, and before the reader opens");

    // And the chain from that tail is still walkable, which is what the tail is
    // for and what losing it would silently break.
    let mut seen = 0;
    for entry in reader.block_chain(Label::ID)? {
        entry?;
        seen += 1;
    }
    assert_eq!(seen, 1);

    let fresh = varve::VarveFile::open_readonly(spec(), &path)?;
    assert_eq!(
        reader.block_tail_offset(Label::ID),
        fresh.block_tail_offset(Label::ID),
    );
    assert_eq!(
        reader.block_tail_offset(Reading::ID),
        fresh.block_tail_offset(Reading::ID),
    );
    Ok(())
}

/// A follow is charged for the tail it framed, not for the file it sits in.
///
/// `ScanBytes` is the ceiling that separates an open which follows an on-disk
/// index from one that walks the file, and a follow that re-charged the bytes
/// its open already paid for would push a long-lived stream reader through that
/// ceiling for reading nothing new. Measured against the counter directly
/// rather than by trying to trip a limit, because the interesting quantity is
/// the charge itself.
#[test]
fn a_follow_is_charged_for_the_tail_and_not_for_the_prefix() -> varve::Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("charge.varve");

    let mut writer = spec().create(&path)?;
    for value in 0..2_000 {
        writer.push(&Reading { value })?;
    }
    writer.flush()?;
    let at_open = std::fs::metadata(&path)?.len();

    let mut reader = varve::VarveFile::open_readonly(spec(), &path)?;
    for value in 2_000..2_010 {
        writer.push(&Reading { value })?;
    }
    writer.flush()?;
    let gained = {
        let _ = varve::VarveFile::take_open_scan_bytes();
        let gained = reader.follow()?;
        assert!(gained > 0);
        gained
    };
    let charged = varve::VarveFile::take_open_scan_bytes();

    assert!(
        charged <= gained,
        "the follow gained {gained} bytes and was charged {charged}; the \
         prefix of {at_open} bytes is being charged twice",
    );
    assert!(
        charged * 4 < at_open,
        "charged {charged} against a {at_open}-byte prefix — that is the file, \
         not the tail",
    );
    Ok(())
}

/// The followed run has to continue *this handle's* sequence.
///
/// The scan's own uniqueness check sees only the run it framed, so a run that
/// is internally strictly increasing passes it even when it repeats sequences
/// the handle already holds — and sequences are file-global and gapless, so a
/// repeat means these are not this object's continuation. Nothing reachable
/// through the API produces that, which is exactly why it needs a byte patch:
/// a defence with no test is a defence with no evidence.
///
/// The sequence lives in the record *header*, and under a plain `crc32` policy
/// the checksum covers payload and footer only — so the patch is expressible
/// without also forging a checksum, and the record still frames. The handle is
/// opened on a file holding only the prefix and the tail is then written into
/// the same object, which is how a reader ends up with a snapshot that stops
/// exactly before the patched record.
#[test]
fn a_followed_run_that_repeats_a_sequence_is_refused() -> varve::Result<()> {
    let directory = tempfile::tempdir()?;
    let source = directory.path().join("source.varve");

    let mut writer = spec().create(&source)?;
    for value in 0..10 {
        writer.push(&Reading { value })?;
    }
    writer.flush()?;
    drop(writer);
    let prefix = std::fs::read(&source)?;
    let from = prefix.len();

    let mut writer = varve::VarveWriter::open(spec(), &source)?;
    let first = writer.push_info(&Reading { value: 100 })?;
    assert_eq!(
        first.record_offset, from as u64,
        "the patch below addresses the first record past the prefix",
    );
    for value in 101..110 {
        writer.push(&Reading { value })?;
    }
    writer.flush()?;
    drop(writer);
    let full = std::fs::read(&source)?;
    assert!(full.len() > from && full[..from] == prefix[..]);

    // The field is located by searching the header window for the value the
    // append reported, rather than by restating the header layout here.
    let window = from..(from + 48).min(full.len());
    let wanted = first.sequence.to_le_bytes();
    let at = full[window.clone()]
        .windows(8)
        .position(|candidate| candidate == wanted)
        .map(|offset| window.start + offset)
        .expect("the appended record carries the sequence it reported");
    let mut patched = full.clone();
    patched[at..at + 8].copy_from_slice(&0u64.to_le_bytes());

    // A handle whose snapshot stops at `from`, then the tail arrives.
    let follow_with = |name: &str, tail: &[u8]| -> varve::Result<varve::Result<u64>> {
        let path = directory.path().join(name);
        std::fs::write(&path, &prefix)?;
        let mut reader = varve::VarveFile::open_readonly(spec(), &path)?;
        // Same object: a truncating rewrite keeps the inode, and the prefix is
        // unchanged, so the handle's own bytes never move under it.
        std::fs::write(&path, tail)?;
        Ok(reader.follow())
    };

    assert!(
        follow_with("clean.varve", &full)?? > 0,
        "the unpatched run is adopted, or this proves nothing",
    );
    let refused = follow_with("patched.varve", &patched)?;
    assert!(
        matches!(refused, Err(varve::Error::InvalidCanonicalEncoding(_))),
        "a run repeating a held sequence must be refused, got {refused:?}",
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// Crossing a generation, which `follow` deliberately will not do.
// ---------------------------------------------------------------------------

/// `is_current` says what `follow`'s zero could not.
///
/// A handle bound to a replaced object answers `0` to every follow, forever,
/// and that is indistinguishable from "the writer appended nothing". This is
/// the question that separates them, and it must stay `&self` — a shared reader
/// has to be able to ask it without anybody stopping.
#[test]
fn a_handle_can_tell_that_its_generation_was_replaced() -> varve::Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("current.varve");

    let mut writer = spec().create(&path)?;
    writer.push(&Label {
        text: "original".into(),
    })?;
    for value in 0..10 {
        writer.push(&Reading { value })?;
    }
    writer.flush()?;
    drop(writer);

    let reader = varve::VarveFile::open_readonly(spec(), &path)?;
    assert!(reader.is_current()?, "nothing has replaced it yet");

    // Appending does not replace anything, so the answer must not change: this
    // asks about the object, not about the content.
    let mut writer = varve::VarveWriter::open(spec(), &path)?;
    for value in 10..20 {
        writer.push(&Reading { value })?;
    }
    writer.flush()?;
    assert!(
        reader.is_current()?,
        "an append is not a new generation, and this must not confuse the two",
    );

    // A republish is.
    writer.replace_block(
        0,
        &Label {
            text: "a replacement long enough to move every record after it".into(),
        },
    )?;
    drop(writer);
    assert!(
        !reader.is_current()?,
        "the pathname resolves to a different object now",
    );
    // And the writer that published it is on the new generation itself.
    let published = varve::VarveFile::open_readonly(spec(), &path)?;
    assert!(published.is_current()?);
    Ok(())
}

/// A pathname that no longer exists is not this handle's.
///
/// The handle stays perfectly readable — the object is pinned by the descriptor
/// — so this is exactly the case where "still works" and "still current" come
/// apart.
#[test]
fn a_removed_pathname_is_not_current_and_the_handle_still_reads() -> varve::Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("removed.varve");

    let mut writer = spec().create(&path)?;
    for value in 0..10 {
        writer.push(&Reading { value })?;
    }
    writer.flush()?;
    drop(writer);

    let reader = varve::VarveFile::open_readonly(spec(), &path)?;
    let held = records(&reader)?;
    std::fs::remove_file(&path)?;

    assert!(!reader.is_current()?);
    assert_eq!(
        records(&reader)?,
        held,
        "the descriptor pins the object, so the handle is unharmed",
    );
    Ok(())
}

/// `reopen_readonly` moves to the current generation, and takes `&self`.
///
/// The `&self` is the point rather than a detail: a handle shared as
/// `Arc<VarveFile>` cannot be advanced in place, so the owner produces a
/// replacement and stores it while every reader keeps reading. This asserts
/// both handles are usable at once and that they disagree, which is what makes
/// the swap meaningful.
#[test]
fn reopen_moves_to_the_current_generation_without_disturbing_the_old() -> varve::Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("reopen.varve");

    let mut writer = spec().create(&path)?;
    writer.push(&Label {
        text: "original".into(),
    })?;
    for value in 0..10 {
        writer.push(&Reading { value })?;
    }
    writer.flush()?;
    drop(writer);

    let old = varve::VarveFile::open_readonly(spec(), &path)?;
    let old_tail = old.block_tail_offset(Reading::ID).expect("a Reading");

    let mut writer = varve::VarveWriter::open(spec(), &path)?;
    writer.replace_block(
        0,
        &Label {
            text: "a replacement long enough to move every record after it".into(),
        },
    )?;
    drop(writer);

    // `&self`, so this is what a shared reader can do — pinned by signature in
    // `detecting_and_reopening_a_generation_take_a_shared_borrow`.
    let new = old.reopen_readonly()?;
    assert!(new.is_current()?);
    assert!(!old.is_current()?);
    assert_ne!(
        new.block_tail_offset(Reading::ID),
        Some(old_tail),
        "the generations must actually differ",
    );

    // Both alive and both correct, each about its own generation.
    assert_eq!(old.block_tail_offset(Reading::ID), Some(old_tail));
    let old_label: Label = old.read_block_at(old.block_tail_offset(Label::ID).unwrap())?;
    assert_eq!(old_label.text, "original");
    let new_label: Label = new.read_block_at(new.block_tail_offset(Label::ID).unwrap())?;
    assert!(new_label.text.starts_with("a replacement"));

    let fresh = varve::VarveFile::open_readonly(spec(), &path)?;
    assert_eq!(
        new.block_tail_offset(Reading::ID),
        fresh.block_tail_offset(Reading::ID),
    );
    Ok(())
}

/// A reopen keeps the route the handle was using.
///
/// A directoryless handle must not come back with a resident directory: the
/// whole reason it has none is that the file is too large to hold one, and a
/// reopen that quietly retained one would allocate `N` slots on a caller that
/// had asked for exactly the opposite.
#[test]
fn a_reopen_keeps_the_route_the_handle_was_using() -> varve::Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("route.varve");

    let mut writer = spec().create(&path)?;
    for value in 0..20 {
        writer.push(&Reading { value })?;
    }
    writer.flush()?;
    drop(writer);

    let retained = varve::VarveFile::open_readonly(spec(), &path)?;
    assert!(retained.blocks::<Reading>().is_ok());
    assert!(retained.reopen_readonly()?.blocks::<Reading>().is_ok());

    let mut scratch = Vec::new();
    let bare = varve::VarveFile::open_readonly_without_directory(spec(), &path, &mut scratch)?;
    assert!(matches!(
        bare.reopen_readonly()?.blocks::<Reading>(),
        Err(varve::Error::NoResidentDirectory { .. })
    ));
    Ok(())
}

/// The reader wrapper carries both, and `reopen` there is `&self` too.
#[test]
fn the_reader_wrapper_exposes_the_pair() -> varve::Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("wrapper.varve");

    let mut writer = spec().create(&path)?;
    for value in 0..10 {
        writer.push(&Reading { value })?;
    }
    writer.flush()?;
    drop(writer);

    let reader = varve::VarveReader::open(spec(), &path)?;
    assert!(reader.is_current()?);
    let again = reader.reopen()?;
    assert!(again.is_current()?);
    assert_eq!(
        again.blocks::<Reading>()?.len(),
        reader.blocks::<Reading>()?.len()
    );
    Ok(())
}

/// The read policy, held by the compiler rather than by a comment.
///
/// The standing requirement is that no read entry point needs `&mut self` —
/// one handle serves concurrent readers through `&self`. `follow` is the one
/// operation that is not a read and takes `&mut self` accordingly, so the pair
/// added beside it had to stay `&self` or the whole point of `reopen` (produce
/// a replacement *without* anybody stopping) would be gone.
///
/// This is written as a shared borrow held across both calls: if either method
/// took `&mut self`, this would not compile.
#[test]
fn detecting_and_reopening_a_generation_take_a_shared_borrow() -> varve::Result<()> {
    fn through_a_shared_reference(file: &varve::VarveFile) -> varve::Result<varve::VarveFile> {
        assert!(file.is_current()?);
        // Reads stay available on the same shared borrow, which is the property
        // the policy is about.
        let _ = file.block_tail_offset(Reading::ID);
        file.reopen_readonly()
    }

    let directory = tempfile::tempdir()?;
    let path = directory.path().join("shared.varve");
    let mut writer = spec().create(&path)?;
    for value in 0..10 {
        writer.push(&Reading { value })?;
    }
    writer.flush()?;
    drop(writer);

    let reader = varve::VarveFile::open_readonly(spec(), &path)?;
    let borrowed = &reader;
    let replacement = through_a_shared_reference(borrowed)?;
    // The original is still borrowable and still readable afterwards.
    assert_eq!(
        borrowed.block_tail_offset(Reading::ID),
        replacement.block_tail_offset(Reading::ID),
    );
    Ok(())
}

/// A handle across threads, which is what the `&self` policy exists for.
///
/// Not a stress test — a demonstration that the pair is usable in the shape the
/// design assumes: readers hold an `Arc` and read concurrently, and the owner
/// produces the next generation through the same shared handle.
#[test]
fn readers_share_a_handle_while_the_next_generation_is_produced() -> varve::Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("threads.varve");
    let mut writer = spec().create(&path)?;
    for value in 0..50 {
        writer.push(&Reading { value })?;
    }
    writer.flush()?;
    drop(writer);

    let shared = std::sync::Arc::new(varve::VarveFile::open_readonly(spec(), &path)?);
    let expected = shared.block_tail_offset(Reading::ID);

    let readers: Vec<_> = (0..4)
        .map(|_| {
            let handle = std::sync::Arc::clone(&shared);
            std::thread::spawn(move || -> varve::Result<()> {
                for _ in 0..25 {
                    assert_eq!(handle.block_tail_offset(Reading::ID), expected);
                    assert!(handle.is_current()?);
                }
                Ok(())
            })
        })
        .collect();

    // The owner produces the replacement through the same shared handle, with
    // every reader still running against it.
    let next = shared.reopen_readonly()?;
    assert_eq!(next.block_tail_offset(Reading::ID), expected);

    for reader in readers {
        reader.join().expect("no reader panicked")?;
    }
    Ok(())
}
