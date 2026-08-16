//! `replace` works on every format, because the format picks the route.
//!
//! It used to take a `ReplaceStrategy`, and that was wrong in a way that was
//! measurable rather than stylistic. On a format declaring an offset chain and
//! a commit policy — the shape this crate is built around — replacing a
//! variable record with a longer one gave:
//!
//! | call | result |
//! | --- | --- |
//! | `replace(.., FixedCopyOnWrite)` | `BlockKindMismatch { expected: Fixed, actual: Variable }` |
//! | `replace(.., RewriteFile)` | `InvalidFormatSpec("replace is not supported for record-footer formats")` |
//! | `replace_block(..)` | `Ok` |
//!
//! **Both strategies refused, and the one that worked was not reachable through
//! the enum at all.** The strategy was never the caller's to pick: which route
//! is legal follows from the declaration. So the argument is gone and `replace`
//! reads the declaration.
//!
//! This file is the table that measurement should have been. Each case names a
//! format shape and asserts the replacement lands *and reads back*, because
//! "did not error" is not the same as "wrote the right record" — on a
//! footer-bearing format the chains have to survive too, so the footered cases
//! walk the chain.

#![cfg(feature = "integrity")]

use varve::{VarveBlock, varve_format};

#[derive(Clone, Debug, PartialEq, VarveBlock)]
#[varve(id = 10, version = 1, kind = "fixed")]
struct Reading {
    value: u64,
}

#[derive(Clone, Debug, PartialEq, VarveBlock)]
#[varve(id = 11, version = 1, kind = "variable")]
struct Note {
    #[varve(field_id = 1)]
    text: String,
}

// Footer-bearing: an offset chain and a commit marker. `replace` must route
// this to `replace_block`, the only route that maintains the chain.
varve_format! {
    pub struct Footered {
        magic: b"VDISPAT1";
        version: 1;
        endian: little;
        schema_hash: computed;
        integrity: crc32;
        index: block_offset_chain;
        commit: transaction_marker(on_flush);
        blocks: [Reading, Note];
    }
}

// No footer at all, so there is no chain to maintain and the cheaper routes
// apply: `replace_fixed` for a fixed block, `replace_rewrite` otherwise.
varve_format! {
    pub struct Plain {
        magic: b"VDISPAT2";
        version: 1;
        endian: little;
        schema_hash: computed;
        integrity: crc32;
        index: scan_on_open;
        commit: none;
        blocks: [Reading, Note];
    }
}

fn notes(spec: varve::FormatSpec, path: &std::path::Path) -> varve::Result<Vec<String>> {
    let file = varve::VarveFile::open_readonly(spec, path)?;
    Ok(file
        .blocks::<Note>()?
        .iter()
        .collect::<varve::Result<Vec<Note>>>()?
        .into_iter()
        .map(|note| note.text)
        .collect())
}

fn readings(spec: varve::FormatSpec, path: &std::path::Path) -> varve::Result<Vec<u64>> {
    let file = varve::VarveFile::open_readonly(spec, path)?;
    Ok(file
        .blocks::<Reading>()?
        .iter()
        .collect::<varve::Result<Vec<Reading>>>()?
        .into_iter()
        .map(|reading| reading.value)
        .collect())
}

/// Every `Reading` reached by walking the block chain backwards from its tail.
///
/// Only meaningful on a footered format, and it is the assertion that separates
/// "the replacement did not error" from "the chain still describes the file".
fn chain(spec: varve::FormatSpec, path: &std::path::Path) -> varve::Result<Vec<u64>> {
    let file = varve::VarveFile::open_readonly(spec, path)?;
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

/// A variable record on a footer format: the case where **both** old strategies
/// refused and the working route was reachable only by name.
#[test]
fn a_footered_format_replaces_a_variable_record_and_keeps_the_chain() -> varve::Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("footered-var.varve");

    let mut writer = varve::VarveWriter::create(Footered::spec(), &path)?;
    writer.push(&Note { text: "aa".into() })?;
    for value in 0..4u64 {
        writer.push(&Reading { value })?;
    }
    writer.commit()?;

    // Longer than what it replaces, so every record after it moves.
    writer.replace(
        0,
        &Note {
            text: "a much longer replacement".into(),
        },
    )?;
    drop(writer);

    assert_eq!(
        notes(Footered::spec(), &path)?,
        ["a much longer replacement"]
    );
    assert_eq!(readings(Footered::spec(), &path)?, [0, 1, 2, 3]);
    // The chain has to have moved with the records. A record count cannot tell
    // a correct chain from one pointing at stale offsets.
    assert_eq!(chain(Footered::spec(), &path)?, [0, 1, 2, 3]);
    Ok(())
}

/// A fixed record on a footer format. Same route, and the chain still matters.
#[test]
fn a_footered_format_replaces_a_fixed_record() -> varve::Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("footered-fixed.varve");

    let mut writer = varve::VarveWriter::create(Footered::spec(), &path)?;
    for value in 0..4u64 {
        writer.push(&Reading { value })?;
    }
    writer.commit()?;
    writer.replace(1, &Reading { value: 99 })?;
    drop(writer);

    assert_eq!(readings(Footered::spec(), &path)?, [0, 99, 2, 3]);
    assert_eq!(chain(Footered::spec(), &path)?, [0, 99, 2, 3]);
    Ok(())
}

/// A fixed record with no footer — the cheap route, and the one case the old
/// `FixedCopyOnWrite` already served.
#[test]
fn a_footerless_format_replaces_a_fixed_record() -> varve::Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("plain-fixed.varve");

    let mut writer = varve::VarveWriter::create(Plain::spec(), &path)?;
    for value in 0..4u64 {
        writer.push(&Reading { value })?;
    }
    writer.flush()?;
    writer.replace(2, &Reading { value: 77 })?;
    drop(writer);

    assert_eq!(readings(Plain::spec(), &path)?, [0, 1, 77, 3]);
    Ok(())
}

/// A variable record with no footer — the whole-file republish, which is the
/// one case the old `RewriteFile` served.
#[test]
fn a_footerless_format_replaces_a_variable_record() -> varve::Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("plain-var.varve");

    let mut writer = varve::VarveWriter::create(Plain::spec(), &path)?;
    writer.push(&Note { text: "aa".into() })?;
    for value in 0..4u64 {
        writer.push(&Reading { value })?;
    }
    writer.flush()?;
    writer.replace(
        0,
        &Note {
            text: "a much longer replacement".into(),
        },
    )?;
    drop(writer);

    assert_eq!(notes(Plain::spec(), &path)?, ["a much longer replacement"]);
    assert_eq!(readings(Plain::spec(), &path)?, [0, 1, 2, 3]);
    Ok(())
}

/// The named routes stay public, and still refuse loudly where the format
/// cannot serve them. `replace` is the door for a caller who wants the
/// replacement; these are for a caller who wants a particular mechanism and
/// would rather be told than silently rerouted.
#[test]
fn the_named_routes_still_refuse_where_they_do_not_apply() -> varve::Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("refusals.varve");

    let mut writer = varve::VarveWriter::create(Footered::spec(), &path)?;
    writer.push(&Note { text: "aa".into() })?;
    writer.push(&Reading { value: 0 })?;
    writer.commit()?;

    let longer = Note {
        text: "a much longer replacement".into(),
    };
    assert!(
        matches!(
            writer.replace_rewrite(0, &longer),
            Err(varve::Error::InvalidFormatSpec(_))
        ),
        "the footerless rewrite must still refuse a footer format",
    );
    assert!(
        matches!(
            writer.replace_fixed(0, &longer),
            Err(varve::Error::BlockKindMismatch { .. })
        ),
        "the fixed route must still refuse a variable block",
    );
    // And the route `replace` would have picked works.
    assert!(writer.replace(0, &longer).is_ok());
    Ok(())
}
