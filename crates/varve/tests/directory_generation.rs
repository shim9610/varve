//! A caller-supplied directory belongs to the generation it was built from.
//!
//! `with_directory` trusts the offsets it is handed — that is the whole point,
//! it is how a host that already has the scan buffer avoids paying for a second
//! one. The trust was never bounded to a *file*, though, and `reopen_readonly`
//! made the unbounded version reachable through an ordinary sequence rather
//! than through a mistake:
//!
//! 1. build a directory from a handle on generation N,
//! 2. a `replace_*` publishes generation N+1 by renaming over the pathname,
//! 3. reopen the handle, which lands on N+1,
//! 4. hand it the directory from step 1.
//!
//! Every offset in that directory now names a byte range in a file where the
//! records have moved. Under `integrity: crc32` a checksum refuses most of it;
//! under `integrity: none` nothing does, and the reads answer with whatever is
//! at those offsets.
//!
//! **The refusal has to be derived from the file, not from a tag the directory
//! carries.** The most useful directory of all is `Vec<RecordIndexEntry>` — it
//! is the scan buffer the host already has — and a foreign type cannot be given
//! a provenance field. So the check re-frames the directory's own entries
//! against the file and requires the records to still be there, which is the
//! same shape the header-tail slot uses to decide whether to trust a table.

#![cfg(feature = "scalable-fault-injection")]

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

// No checksum, which is what makes this observable rather than merely wrong:
// with `crc32` the reads refuse, and a refusal is already a safe answer.
varve_format! {
    pub struct Gen {
        magic: b"VGENDIR1";
        version: 1;
        endian: little;
        schema_hash: computed;
        integrity: none;
        index: scan_on_open;
        commit: none;
        blocks: [Reading, Label];
    }
}

fn spec() -> varve::FormatSpec {
    Gen::spec()
}

/// Records 0..COUNT, behind a variable-length label that a replacement can grow
/// so that every record after it moves.
const COUNT: u64 = 8;

fn build(path: &std::path::Path) -> varve::Result<()> {
    let mut writer = spec().create(path)?;
    writer.push(&Label {
        text: "short".into(),
    })?;
    for value in 0..COUNT {
        writer.push(&Reading { value })?;
    }
    writer.flush()?;
    Ok(())
}

#[test]
fn a_directory_from_a_superseded_generation_is_refused() -> varve::Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("generation.varve");
    build(&path)?;

    let old = varve::VarveFile::open_readonly(spec(), &path)?;

    // The directory the host already has, built against this generation.
    let mut held = Vec::new();
    old.index_entries_into(&mut held)?;
    let truth: Vec<Reading> = old
        .blocks::<Reading>()?
        .iter()
        .collect::<varve::Result<_>>()?;
    assert_eq!(truth.len(), COUNT as usize);

    // Generation N+1: a longer label moves every record after it.
    let mut writer = varve::VarveWriter::open(spec(), &path)?;
    writer.replace(
        0,
        &Label {
            text: "a replacement long enough to move every record after it".into(),
        },
        varve::ReplaceStrategy::RewriteFile,
    )?;
    drop(writer);

    let fresh = old.reopen_readonly()?;

    // The premise: the offsets really did move, so the held directory is
    // describing byte ranges that no longer mean what it thinks.
    let mut moved = Vec::new();
    fresh.index_entries_into(&mut moved)?;
    assert_ne!(
        held.iter().map(|e| e.record_offset).collect::<Vec<_>>(),
        moved.iter().map(|e| e.record_offset).collect::<Vec<_>>(),
        "the replacement must move the records for this to be worth refusing",
    );

    // The old handle is untouched and still correct about its own generation:
    // the stale directory is only stale relative to the *reopened* handle.
    let through_old: Vec<Reading> = old
        .with_directory(&held)?
        .blocks::<Reading>()?
        .iter()
        .collect::<varve::Result<_>>()?;
    assert_eq!(through_old, truth, "the directory still fits its own file");

    // And the pairing that is wrong must say so rather than answering. The
    // refusal is at `with_directory`, so it lands once rather than per read.
    let answered = fresh.with_directory(&held);
    assert!(
        matches!(
            answered.as_ref().err(),
            Some(varve::Error::DirectoryDoesNotDescribeThisFile { .. })
        ),
        "a directory from a superseded generation must be refused, got {:?}",
        answered.map(|view| view.record_count()),
    );
    Ok(())
}

/// The refusal must not cost the honest caller anything.
#[test]
fn a_directory_from_this_generation_still_reads() -> varve::Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("same.varve");
    build(&path)?;

    let file = varve::VarveFile::open_readonly(spec(), &path)?;
    let mut held = Vec::new();
    file.index_entries_into(&mut held)?;

    let through: Vec<Reading> = file
        .with_directory(&held)?
        .blocks::<Reading>()?
        .iter()
        .collect::<varve::Result<_>>()?;
    assert_eq!(through.len(), COUNT as usize);

    // A *subset* is a legitimate directory — a caller may hand over the records
    // it cares about — so the check must not be "this is the whole file". Only
    // the entries actually present have to still be where they say they are.
    let subset: Vec<varve::RecordIndexEntry> = held.iter().skip(3).cloned().collect();
    let expected = subset.len();
    let by_hand: Vec<Reading> = file
        .with_directory(subset.as_slice())?
        .blocks::<Reading>()?
        .iter()
        .collect::<varve::Result<_>>()?;
    assert!(by_hand.len() <= expected);

    // An empty directory describes nothing, so there is nothing to disagree
    // with and it must not be refused.
    let empty: Vec<varve::RecordIndexEntry> = Vec::new();
    assert_eq!(file.with_directory(empty.as_slice())?.record_count(), 0);
    Ok(())
}
