//! Rewriting a file without the records marked dead.
//!
//! `defragment` publishes the way `replace_*` does — temp file, sync, atomic
//! rename — so a reader open across it keeps its own generation whole. The cost
//! is peak disk: both generations exist at once.
//!
//! The part worth aiming tests at is the chain remap. `replace_block` resizes
//! one record and removes none, so a single subtraction translates every link.
//! Here each survivor moves by a different amount and some predecessors are
//! gone, so the links are remapped through a table per chain.
//!
//! **Measured, with the remap swapped for a uniform delta:** the rewrite is
//! refused at write time with `InvalidRecordFooter { offset: 616 }` and nothing
//! reaches the pathname, because `validate_replacement_predecessors` resolves
//! every link against the prefix already written. Two of these four fail that
//! way. So the remap is required for the operation to work, and the *guard*
//! against a wrong one publishing is the predecessor check rather than these
//! tests — which is a better answer than the one this comment first claimed.
//!
//! The assertions still walk the chain rather than counting records, because a
//! record count cannot tell a correct chain from one that skips a survivor.

#![cfg(feature = "integrity")]

use varve::{VarveBlock, varve_format};

#[derive(Clone, Debug, PartialEq, VarveBlock)]
#[varve(id = 10, version = 1, kind = "fixed")]
struct Reading {
    value: u64,
}

// A second block, so the block chains interleave: a defrag that remapped by a
// single delta would still look right with only one block in the file.
#[derive(Clone, Debug, PartialEq, VarveBlock)]
#[varve(id = 11, version = 1, kind = "variable")]
struct Note {
    #[varve(field_id = 1)]
    text: String,
}

varve_format! {
    pub struct Defrag {
        magic: b"VDEFRAG1";
        version: 1;
        endian: little;
        schema_hash: computed;
        integrity: crc32;
        index: block_offset_chain;
        commit: transaction_marker(on_flush);
        liveness: footer_flags;
        blocks: [Reading, Note];
    }
}

fn spec() -> varve::FormatSpec {
    Defrag::spec().with_read_limits(
        Defrag::spec()
            .read_limits
            .with_integrity_verification(varve::IntegrityVerification::AtOpen),
    )
}

/// Every `Reading` in the file, in order, by walking the block chain backwards
/// from its tail — which is the thing a wrong remap corrupts and a record count
/// does not notice.
fn chain_values(path: &std::path::Path) -> varve::Result<Vec<u64>> {
    let file = varve::VarveFile::open_readonly(spec(), path)?;
    let mut values = Vec::new();
    for entry in file.block_chain(Reading::ID)? {
        let entry = entry?;
        let mut scratch = Vec::new();
        let reading: Reading = file.decode_block_into(&entry, &mut scratch)?;
        values.push(reading.value);
    }
    values.reverse();
    Ok(values)
}

fn read_values(path: &std::path::Path) -> varve::Result<Vec<u64>> {
    let file = varve::VarveFile::open_readonly(spec(), path)?;
    Ok(file
        .blocks::<Reading>()?
        .iter()
        .collect::<varve::Result<Vec<Reading>>>()?
        .into_iter()
        .map(|reading| reading.value)
        .collect())
}

/// Build a file whose two blocks interleave, and hand back every `Reading`'s
/// record offset so the caller can choose what to kill.
fn build(path: &std::path::Path) -> varve::Result<Vec<u64>> {
    let mut writer = varve::VarveWriter::create(Defrag::spec(), path)?;
    let mut offsets = Vec::new();
    for value in 0..10u64 {
        offsets.push(writer.push_info(&Reading { value })?.record_offset);
        writer.push(&Note {
            text: format!("note {value}"),
        })?;
    }
    writer.commit()?;
    drop(writer);
    Ok(offsets)
}

#[test]
fn dead_records_are_dropped_and_the_chain_still_reads() -> varve::Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("defrag.varve");
    let offsets = build(&path)?;

    assert_eq!(read_values(&path)?, (0..10).collect::<Vec<_>>());
    assert_eq!(chain_values(&path)?, (0..10).collect::<Vec<_>>());

    // Kill three, deliberately including two that are adjacent in the chain —
    // a link then has to reach back *past two* dropped records, which is the
    // case a single-step remap gets wrong.
    let mut writer = varve::VarveWriter::open(Defrag::spec(), &path)?;
    for index in [3usize, 4, 7] {
        writer.mark_record_dead(offsets[index])?;
    }
    writer.commit()?;

    let report = writer.defragment()?;
    drop(writer);

    assert_eq!(report.records_dropped, 3);
    assert!(
        report.bytes_after < report.bytes_before,
        "the file must shrink: {} -> {}",
        report.bytes_before,
        report.bytes_after,
    );

    let survivors = vec![0, 1, 2, 5, 6, 8, 9];
    assert_eq!(read_values(&path)?, survivors);
    // The load-bearing one: a record count cannot tell a correct chain from one
    // that skips a survivor, and this can.
    assert_eq!(
        chain_values(&path)?,
        survivors,
        "the block chain must reach exactly the survivors, in order",
    );
    Ok(())
}

/// A file with nothing marked is rewritten to the same content.
#[test]
fn defragmenting_a_file_with_no_dead_records_changes_nothing_readable() -> varve::Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("clean.varve");
    build(&path)?;
    let before = read_values(&path)?;
    let chain_before = chain_values(&path)?;

    let mut writer = varve::VarveWriter::open(Defrag::spec(), &path)?;
    let report = writer.defragment()?;
    drop(writer);

    assert_eq!(report.records_dropped, 0);
    assert_eq!(read_values(&path)?, before);
    assert_eq!(chain_values(&path)?, chain_before);
    Ok(())
}

/// A reader open across the republish keeps its own generation whole — the
/// property that made republish the right shape rather than an in-place rewrite.
#[test]
fn a_reader_open_across_the_republish_keeps_its_generation() -> varve::Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("reader.varve");
    let offsets = build(&path)?;

    let held = varve::VarveFile::open_readonly(spec(), &path)?;
    let before: Vec<Reading> = held
        .blocks::<Reading>()?
        .iter()
        .collect::<varve::Result<_>>()?;
    assert_eq!(before.len(), 10);

    let mut writer = varve::VarveWriter::open(Defrag::spec(), &path)?;
    for index in [1usize, 2, 3] {
        writer.mark_record_dead(offsets[index])?;
    }
    writer.commit()?;
    writer.defragment()?;
    drop(writer);

    // Whole, unchanged, and no error — the old object is unlinked but alive.
    let after: Vec<Reading> = held
        .blocks::<Reading>()?
        .iter()
        .collect::<varve::Result<_>>()?;
    assert_eq!(after, before);
    // And it can be told that it is no longer current.
    assert!(!held.is_current()?);
    assert_eq!(
        held.reopen_readonly()?.blocks::<Reading>()?.len(),
        7,
        "the reopened handle lands on the defragmented generation",
    );
    Ok(())
}

/// Without the liveness word nothing is ever dead, so a defragment would be a
/// whole-file copy that achieves nothing. Refused rather than silently wasteful.
#[test]
fn it_is_refused_without_the_liveness_policy() -> varve::Result<()> {
    let plain = Defrag::spec().with_liveness_policy(varve::LivenessPolicy::None);
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("plain.varve");
    let mut writer = varve::VarveWriter::create(plain, &path)?;
    writer.push(&Reading { value: 0 })?;
    writer.commit()?;
    assert!(matches!(
        writer.defragment(),
        Err(varve::Error::InvalidFormatSpec(_))
    ));
    Ok(())
}
