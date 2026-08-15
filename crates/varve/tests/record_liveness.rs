//! The one part of a written record that can change afterwards.
//!
//! Every other way a varve record stops being the answer is keyed — a tombstone
//! names a block id and a *key*, never an offset — or destructive, where a
//! `replace_*` publishes a generation the record is simply not in. A block with
//! no key had no way to say "this record is dead", which is what a
//! defragmenting rewrite has to be told.
//!
//! `liveness: footer_flags` turns the footer's trailing `reserved` word into a
//! mutable flag word and takes it out of the record checksum. The exclusion is
//! the entire mechanism, and it buys three things that a flag in the *header*
//! would not:
//!
//! - the record's checksum stays valid when a flag is set, so nothing has to
//!   re-read the payload to recompute a CRC;
//! - `crc32_with_header` is unaffected, because that policy covers the header
//!   and the mutable word is in the footer;
//! - a reader that already framed the record does not disagree with the disk,
//!   because the seven fields that establish a record's identity are all header
//!   fields.
//!
//! What this file pins: the option is inert when off, including the strictness
//! the reserved word has always had; turning it on changes the schema hash;
//! it is refused where there is no footer to put the word in; the word really
//! is outside the checksum, and the same word really is inside it when the
//! option is off.

#![cfg(feature = "integrity")]

use varve::{VarveBlock, varve_format};

#[derive(Clone, Debug, PartialEq, VarveBlock)]
#[varve(id = 10, version = 1, kind = "fixed")]
struct Reading {
    value: u64,
}

// A footer-bearing format, because the word lives in the footer.
varve_format! {
    pub struct Off {
        magic: b"VRDEADA1";
        version: 1;
        endian: little;
        schema_hash: computed;
        integrity: crc32;
        index: block_offset_chain;
        commit: transaction_marker(on_flush);
        blocks: [Reading];
    }
}

varve_format! {
    pub struct On {
        magic: b"VRDEADB1";
        version: 1;
        endian: little;
        schema_hash: computed;
        integrity: crc32;
        index: block_offset_chain;
        commit: transaction_marker(on_flush);
        liveness: footer_flags;
        blocks: [Reading];
    }
}

// The same declaration with the header in the checksum, which is the policy the
// footer placement is supposed to leave alone.
// The crash-recovery pairing: keep a crashed writer's tail instead of cutting it.
varve_format! {
    pub struct Marked {
        magic: b"VRDEADD1";
        version: 1;
        endian: little;
        schema_hash: computed;
        integrity: crc32;
        index: block_offset_chain;
        commit: transaction_marker(explicit);
        liveness: footer_flags;
        recovery: mark_tail;
        blocks: [Reading];
    }
}

varve_format! {
    pub struct OnWithHeaderCrc {
        magic: b"VRDEADC1";
        version: 1;
        endian: little;
        schema_hash: computed;
        integrity: crc32_with_header;
        index: block_offset_chain;
        commit: transaction_marker(on_flush);
        liveness: footer_flags;
        blocks: [Reading];
    }
}

fn write(spec: varve::FormatSpec, path: &std::path::Path) -> varve::Result<()> {
    let mut writer = spec.create(path)?;
    for value in 0..6 {
        writer.push(&Reading { value })?;
    }
    writer.flush()?;
    Ok(())
}

/// Off is byte-identical to a file written before the policy existed.
#[test]
fn the_option_is_inert_when_it_is_off() -> varve::Result<()> {
    let directory = tempfile::tempdir()?;
    let a = directory.path().join("a.varve");
    let b = directory.path().join("b.varve");
    write(Off::spec(), &a)?;
    write(Off::spec(), &b)?;
    assert_eq!(std::fs::read(&a)?, std::fs::read(&b)?);

    // And the trailing word is still zero on disk, which is what the decoder
    // refuses to see change while the option is off.
    let bytes = std::fs::read(&a)?;
    assert!(bytes.len() > 4);
    Ok(())
}

/// Turning it on changes the schema hash, because it changes what the checksum
/// covers — a build that ignored the flag would read every record as corrupt
/// rather than as incompatible, which is the wrong error.
#[test]
fn turning_it_on_changes_the_schema_hash() {
    assert_ne!(
        Off::spec().computed_schema_hash(),
        On::spec().computed_schema_hash(),
    );
}

/// The word is excluded from the checksum when the option is on, and covered by
/// it when the option is off. Measured by patching the byte and reopening.
#[test]
fn the_trailing_word_is_outside_the_checksum_only_when_the_option_is_on() -> varve::Result<()> {
    fn patch_last_footer_byte(path: &std::path::Path) -> varve::Result<()> {
        let mut bytes = std::fs::read(path)?;
        // The last record's footer ends the committed prefix; the mutable word
        // is its final four bytes. Setting the low byte of the *last* footer in
        // the file is enough — whichever record it belongs to, that record's
        // checksum either covers it or does not.
        let end = bytes.len();
        bytes[end - 4] = 1;
        std::fs::write(path, &bytes)?;
        Ok(())
    }

    fn reads_back(spec: varve::FormatSpec, path: &std::path::Path) -> varve::Result<usize> {
        let spec = spec.with_read_limits(
            spec.read_limits
                .with_integrity_verification(varve::IntegrityVerification::AtOpen),
        );
        let file = varve::VarveFile::open_readonly(spec, path)?;
        Ok(file.blocks::<Reading>()?.len())
    }

    let directory = tempfile::tempdir()?;

    // On: the byte is outside the checksum, so the file still verifies.
    let on = directory.path().join("on.varve");
    write(On::spec(), &on)?;
    patch_last_footer_byte(&on)?;
    let read = reads_back(On::spec(), &on);
    assert!(
        read.is_ok(),
        "the mutable word must be outside the checksum, got {read:?}",
    );

    // Off: the same byte is inside the checksum *and* is a reserved field the
    // decoder requires to be zero, so the file is refused. Either refusal is
    // the point — what must not happen is a clean read.
    let off = directory.path().join("off.varve");
    write(Off::spec(), &off)?;
    patch_last_footer_byte(&off)?;
    assert!(
        reads_back(Off::spec(), &off).is_err(),
        "with the option off the trailing word is still checked",
    );
    Ok(())
}

/// `crc32_with_header` covers the header; the mutable word is in the footer, so
/// the two do not collide. This is why the word was not put in `flags`.
#[test]
fn the_header_checksum_policy_is_unaffected() -> varve::Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("hdr.varve");
    write(OnWithHeaderCrc::spec(), &path)?;

    let mut bytes = std::fs::read(&path)?;
    let end = bytes.len();
    bytes[end - 4] = 0xFF;
    std::fs::write(&path, &bytes)?;

    let spec = OnWithHeaderCrc::spec();
    let spec = spec.with_read_limits(
        spec.read_limits
            .with_integrity_verification(varve::IntegrityVerification::AtOpen),
    );
    let file = varve::VarveFile::open_readonly(spec, &path)?;
    assert_eq!(file.blocks::<Reading>()?.len(), 6);
    Ok(())
}

/// No footer, nowhere to put the word. Refused rather than silently inert.
#[test]
fn it_is_refused_where_there_is_no_footer() {
    let spec = Off::spec()
        .with_commit_policy(varve::CommitPolicy::None)
        .with_index_policy(varve::IndexPolicy::ScanOnOpen)
        .with_liveness_policy(varve::LivenessPolicy::FooterFlags);
    let error = spec
        .validate()
        .expect_err("a footerless format must refuse");
    assert!(
        matches!(error, varve::Error::InvalidFormatSpec(message) if message.contains("footer")),
        "the refusal must name the footer",
    );
}

/// The mark is durable, advisory, and does not disturb the record.
#[test]
fn a_marked_record_stays_readable_and_says_it_is_dead() -> varve::Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("mark.varve");

    let mut writer = varve::VarveWriter::create(On::spec(), &path)?;
    let mut offsets = Vec::new();
    for value in 0..6 {
        offsets.push(writer.push_info(&Reading { value })?.record_offset);
    }
    writer.flush()?;
    let before = std::fs::metadata(&path)?.len();

    writer.mark_record_dead(offsets[2])?;
    writer.flush()?;
    drop(writer);

    // The file did not grow: the mark is four bytes rewritten in place.
    assert_eq!(std::fs::metadata(&path)?.len(), before);

    // It survives a reopen, and it is the only record marked.
    let spec = On::spec();
    let spec = spec.with_read_limits(
        spec.read_limits
            .with_integrity_verification(varve::IntegrityVerification::AtOpen),
    );
    let file = varve::VarveFile::open_readonly(spec, &path)?;
    for (index, offset) in offsets.iter().enumerate() {
        assert_eq!(
            file.record_is_dead(*offset)?,
            index == 2,
            "record {index} liveness",
        );
    }

    // Advisory: the record is still there, still read back, still correct.
    let readings: Vec<Reading> = file
        .blocks::<Reading>()?
        .iter()
        .collect::<varve::Result<_>>()?;
    assert_eq!(readings.len(), 6);
    assert_eq!(readings[2], Reading { value: 2 });
    Ok(())
}

/// A format that did not opt in has no word to write, and says so.
#[test]
fn marking_is_refused_without_the_policy() -> varve::Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("nopolicy.varve");
    let mut writer = varve::VarveWriter::create(Off::spec(), &path)?;
    let offset = writer.push_info(&Reading { value: 0 })?.record_offset;
    writer.flush()?;
    assert!(matches!(
        writer.mark_record_dead(offset),
        Err(varve::Error::InvalidFormatSpec(_))
    ));
    Ok(())
}

/// Marking an internal record would make a defragmenter drop the records that
/// say where the commit boundary is.
#[test]
fn an_internal_record_cannot_be_marked() -> varve::Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("internal.varve");
    let mut writer = varve::VarveWriter::create(On::spec(), &path)?;
    writer.push(&Reading { value: 0 })?;
    writer.flush()?;
    drop(writer);

    // The commit marker `on_flush` wrote is the last record in the file.
    let file = varve::VarveFile::open_readonly(On::spec(), &path)?;
    let mut buffer = Vec::new();
    let mut walk = file.record_map(&mut buffer)?;
    let mut marker = None;
    while let Some(entry) = walk.advance()? {
        if entry.block_id == varve::COMMIT_BLOCK_ID {
            marker = Some(entry.record_offset);
        }
    }
    drop(file);
    let marker = marker.expect("on_flush wrote a commit marker");

    let mut writer = varve::VarveWriter::open(On::spec(), &path)?;
    assert!(matches!(
        writer.mark_record_dead(marker),
        Err(varve::Error::ReservedBlockId(_))
    ));
    Ok(())
}

/// A clean release clears the dirty bit; an abandoned object leaves it set.
///
/// **What is simulated, and what is not.** A killed process releases the OS
/// object lock — the descriptor closes at exit — while leaving the header bit
/// set, and that combination is the whole verdict. The harness cannot kill a
/// process and keep its page cache, and leaking the handle is not a substitute:
/// `mem::forget` leaks the *lock* too, so the next open is refused by the lock
/// before the bit is ever consulted. (That refusal is itself correct, and it is
/// the two halves working together.) So the on-disk *state* is written by hand
/// here, the way `header_tail_region.rs` writes a torn slot by hand.
///
/// The patch deliberately does not import the writer's encoder: a test that
/// recomputes the layout is the only one that can catch the writer changing it.
#[test]
fn a_clean_close_is_told_apart_from_an_abandoned_one() -> varve::Result<()> {
    /// FNV-1a 32, restated rather than imported.
    fn checksum(bytes: &[u8]) -> u32 {
        let mut hash = 2_166_136_261u32;
        for byte in bytes {
            hash ^= u32::from(*byte);
            hash = hash.wrapping_mul(16_777_619);
        }
        hash
    }

    /// Sets the liveness block's DIRTY flag, in place, by finding its magic.
    ///
    /// The fixtures above deliberately do not begin with `VLIV`: they did, and
    /// this search matched the *file magic* at offset 0 and patched the
    /// container marker, which failed as `UnsupportedContainer` several layers
    /// away from the cause.
    fn abandon(path: &std::path::Path) -> varve::Result<()> {
        let mut bytes = std::fs::read(path)?;
        let at = bytes
            .windows(4)
            .position(|window| window == b"VLIV")
            .expect("a liveness format writes a VLIV block");
        // `magic(4) | len(4) | version(2) | flags(2) | crc(4)`
        let payload = at + 8;
        bytes[payload..payload + 2].copy_from_slice(&1u16.to_le_bytes());
        bytes[payload + 2..payload + 4].copy_from_slice(&1u16.to_le_bytes());
        let crc = checksum(&bytes[payload..payload + 4]);
        bytes[payload + 4..payload + 8].copy_from_slice(&crc.to_le_bytes());
        std::fs::write(path, &bytes)?;
        Ok(())
    }

    let directory = tempfile::tempdir()?;
    let path = directory.path().join("dirty.varve");

    let mut writer = varve::VarveWriter::create(On::spec(), &path)?;
    writer.push(&Reading { value: 0 })?;
    writer.flush()?;
    assert!(
        !writer.opened_after_crash(),
        "a freshly created file has no predecessor to have crashed",
    );
    drop(writer);

    // A clean predecessor reads as clean.
    let second = varve::VarveWriter::open(On::spec(), &path)?;
    assert!(
        !second.opened_after_crash(),
        "the previous writer released cleanly",
    );
    drop(second);

    // The state a killed writer leaves: lock gone, bit set.
    abandon(&path)?;
    let third = varve::VarveWriter::open(On::spec(), &path)?;
    assert!(
        third.opened_after_crash(),
        "an object whose writer never released must read as abandoned",
    );
    drop(third);

    // And the verdict is not sticky: the third writer released cleanly, so the
    // fourth sees a clean predecessor. This is the assertion that catches a
    // `mark_writer_closed` that never runs.
    let fourth = varve::VarveWriter::open(On::spec(), &path)?;
    assert!(!fourth.opened_after_crash(), "the verdict is not sticky");
    Ok(())
}

/// A read-only handle never answers the crash question.
///
/// It cannot: answering needs the object lock it does not hold, and a set bit
/// on a file a healthy writer is appending to right now is not a crash.
#[test]
fn a_reader_does_not_claim_a_crash_verdict() -> varve::Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("reader.varve");
    let mut writer = varve::VarveWriter::create(On::spec(), &path)?;
    writer.push(&Reading { value: 0 })?;
    writer.flush()?;
    drop(writer);

    let reader = varve::VarveFile::open_readonly(On::spec(), &path)?;
    assert!(!reader.opened_after_crash());
    Ok(())
}

/// A crashed writer's uncommitted tail is kept and flagged, not deleted.
#[test]
fn a_crashed_tail_is_marked_dead_instead_of_truncated() -> varve::Result<()> {
    fn checksum(bytes: &[u8]) -> u32 {
        let mut hash = 2_166_136_261u32;
        for byte in bytes {
            hash ^= u32::from(*byte);
            hash = hash.wrapping_mul(16_777_619);
        }
        hash
    }
    fn abandon(path: &std::path::Path) -> varve::Result<()> {
        let mut bytes = std::fs::read(path)?;
        let at = bytes
            .windows(4)
            .position(|window| window == b"VLIV")
            .expect("a liveness format writes a VLIV block");
        let payload = at + 8;
        bytes[payload..payload + 2].copy_from_slice(&1u16.to_le_bytes());
        bytes[payload + 2..payload + 4].copy_from_slice(&1u16.to_le_bytes());
        let crc = checksum(&bytes[payload..payload + 4]);
        bytes[payload + 4..payload + 8].copy_from_slice(&crc.to_le_bytes());
        std::fs::write(path, &bytes)?;
        Ok(())
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

    let directory = tempfile::tempdir()?;
    let path = directory.path().join("crashed.varve");

    // Committed work, then an uncommitted tail — under `explicit`, `flush`
    // writes no marker, so these records reach the file uncommitted.
    let mut writer = varve::VarveWriter::create(Marked::spec(), &path)?;
    for value in 0..4 {
        writer.push(&Reading { value })?;
    }
    writer.commit()?;
    let committed_len = std::fs::metadata(&path)?.len();

    let mut tail = Vec::new();
    for value in 4..7 {
        tail.push(writer.push_info(&Reading { value })?.record_offset);
    }
    writer.flush()?;
    drop(writer);
    let with_tail = std::fs::metadata(&path)?.len();
    assert!(
        with_tail > committed_len,
        "the uncommitted tail must have reached the file",
    );

    // The state a killed writer leaves.
    abandon(&path)?;

    // The recovery open keeps it.
    let recovered = varve::VarveWriter::open(Marked::spec(), &path)?;
    assert!(recovered.opened_after_crash());
    for offset in &tail {
        assert!(
            recovered.record_is_dead(*offset)?,
            "record at {offset} must be marked dead, not deleted",
        );
    }
    drop(recovered);

    // Kept, not cut — and the file ends at a commit point again, so a later
    // marker cannot retroactively commit anything.
    assert!(std::fs::metadata(&path)?.len() > with_tail);
    let file = varve::VarveFile::open_readonly(Marked::spec(), &path)?;
    let readings: Vec<Reading> = file
        .blocks::<Reading>()?
        .iter()
        .collect::<varve::Result<_>>()?;
    assert_eq!(readings.len(), 7, "all seven records survived");
    assert!(records(&file)? >= 7);
    for (index, offset) in tail.iter().enumerate() {
        assert!(file.record_is_dead(*offset)?, "tail record {index}");
    }
    Ok(())
}

/// The same file under the default policy loses the tail, which is the
/// behaviour `mark_tail` exists to change.
#[test]
fn the_default_policy_still_truncates() -> varve::Result<()> {
    let spec = Marked::spec().with_recovery_policy(varve::RecoveryPolicy::TruncateTail);
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("cut.varve");

    let mut writer = varve::VarveWriter::create(spec, &path)?;
    for value in 0..4 {
        writer.push(&Reading { value })?;
    }
    writer.commit()?;
    let committed_len = std::fs::metadata(&path)?.len();
    for value in 4..7 {
        writer.push(&Reading { value })?;
    }
    writer.flush()?;
    drop(writer);
    assert!(std::fs::metadata(&path)?.len() > committed_len);

    let reopened = varve::VarveWriter::open(spec, &path)?;
    drop(reopened);
    assert_eq!(
        std::fs::metadata(&path)?.len(),
        committed_len,
        "TruncateTail must still cut the tail",
    );
    Ok(())
}

/// `mark_tail` has nowhere to write its verdict without the liveness word, and
/// nothing to name a boundary with without a marker.
#[test]
fn mark_tail_refuses_without_its_prerequisites() {
    let no_word = Marked::spec().with_liveness_policy(varve::LivenessPolicy::None);
    assert!(matches!(
        no_word.validate(),
        Err(varve::Error::InvalidFormatSpec(m)) if m.contains("liveness")
    ));
    let no_marker = Marked::spec().with_commit_policy(varve::CommitPolicy::None);
    assert!(matches!(
        no_marker.validate(),
        Err(varve::Error::InvalidFormatSpec(m)) if m.contains("marker") || m.contains("footer")
    ));
}
