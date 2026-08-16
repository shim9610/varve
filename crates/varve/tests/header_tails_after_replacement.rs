//! An in-place replacement must leave a header-tails file resumable.
//!
//! The sibling of `digest_after_replacement.rs`, and the same defect on the
//! other lazy route. The two cannot be one file: `header_tails` and
//! `open_digest_on_flush` are refused together — "two answers to the same
//! question; declare one" — so a format exercises exactly one of them, and the
//! fix for the digest could not have covered this one.
//!
//! # The shared defect
//!
//! Both routes hand a lazy open a sequence high-water mark, and a lazy open
//! does **not** recount: it seeds its writer with `mark + 1`. A mark below the
//! file's true maximum therefore sends the next append onto a number some
//! record already used, and the following scan refuses the file with
//! `InvalidCanonicalEncoding("duplicate native record sequence")`. The file
//! stops opening — data loss, not a slow open.
//!
//! # Why this route was missed
//!
//! The digest *stores* its mark, so the fix there was to store the right one.
//! The header region stores none: `corroborate_header_tails` derives it from
//! the commit marker its slot names, plus any segment and digest that follow.
//! That derivation assumes a record's sequence rises with its offset — true of
//! an append log, and false the moment a record is replaced *in place*, which
//! takes a fresh sequence and writes it before the marker.
//!
//! Measured here before the fix: `replace_fixed` took sequence 65 on a
//! 64-record file whose marker still reported 64, and the lazy append after it
//! took 65 as well.
//!
//! # Why retired rather than refused
//!
//! `replace_fixed` is refused on a digest format, because a digest is a record
//! — trusted or absent — and an in-place replacement rewrites no record, so
//! there is no third state to leave it in. The header region has one: cold. So
//! here the capability survives and the region goes out of use, which costs one
//! scanning open and is repaired by the next commit. Both facts are asserted.

#![cfg(feature = "integrity")]

use varve::{FormatSpec, LazyOpenSource, VarveBlock, VarveFile, VarveWriter, varve_format};

#[derive(Clone, Debug, PartialEq, VarveBlock)]
#[varve(id = 90, version = 1, kind = "fixed")]
struct Sample {
    value: u32,
}

varve_format! {
    pub struct TailOn {
        magic: b"VHTREPL0";
        version: 1;
        endian: little;
        schema_hash: computed;
        integrity: crc32;
        index: header_tails;
        commit: transaction_marker(on_flush);
        blocks: [Sample];
    }
}

fn spec() -> FormatSpec {
    TailOn::spec()
}

fn build(path: &std::path::Path) -> varve::Result<()> {
    let mut file = spec().create(path)?;
    for value in 0..64u32 {
        file.push(&Sample { value })?;
    }
    file.flush()?;
    Ok(())
}

/// Append through a **lazy** writer, then scan. The scan is the oracle: it
/// recounts, so it refuses exactly when the mark sent the writer onto a
/// sequence that was already taken.
fn append_lazily_then_scan(path: &std::path::Path) -> varve::Result<usize> {
    {
        let mut writer = VarveWriter::open_lazy(spec(), path)?;
        writer.push(&Sample { value: 7777 })?;
        writer.flush()?;
    }
    let file = spec().open_readonly(path)?;
    let mut buffer = Vec::new();
    let mut walk = file.record_map(&mut buffer)?;
    let mut records = 0;
    while walk.advance()?.is_some() {
        records += 1;
    }
    Ok(records)
}

/// The baseline. Without it, a test that only asserted "replace then lazy
/// append works" could pass on a build where lazy appending never worked.
#[test]
fn a_lazy_append_with_no_replacement_is_the_baseline() -> varve::Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("control.varve");
    build(&path)?;
    assert_eq!(append_lazily_then_scan(&path)?, 67);
    Ok(())
}

#[test]
fn an_in_place_replacement_leaves_the_file_resumable() -> varve::Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("fixed.varve");
    build(&path)?;
    {
        let mut writer = VarveWriter::open(spec(), &path)?;
        writer.replace_fixed(0, &Sample { value: 4242 })?;
    }
    assert_eq!(append_lazily_then_scan(&path)?, 67);
    Ok(())
}

/// The unsafe exclusive-access twin. It shares the refusal gate and now the
/// retirement, but it reaches the file by a different route — it mutates the
/// live bytes instead of publishing a copy — so it is measured, not inferred.
#[test]
fn the_unsafe_in_place_route_leaves_the_file_resumable() -> varve::Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("unsafe.varve");
    build(&path)?;
    {
        let mut writer = VarveWriter::open(spec(), &path)?;
        // SAFETY: this handle is the only one open on the path for the whole
        // call, and no mapping, reference or reader outlives it here.
        unsafe { writer.replace_fixed_in_place_exclusive(0, &Sample { value: 4242 })? };
    }
    assert_eq!(append_lazily_then_scan(&path)?, 67);
    Ok(())
}

/// `replace` routes to the republishing path on this format, which rebuilds the
/// region rather than retiring it. Pinned so the two paths are known to differ
/// and both to be safe.
#[test]
fn the_republishing_route_leaves_the_file_resumable() -> varve::Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("replace.varve");
    build(&path)?;
    {
        let mut writer = VarveWriter::open(spec(), &path)?;
        writer.replace(0, &Sample { value: 4242 })?;
    }
    assert_eq!(append_lazily_then_scan(&path)?, 67);
    Ok(())
}

/// What the fix costs, stated as a measurement rather than as a claim: the fast
/// route is in use, the replacement takes it out of use, and one commit puts it
/// back.
#[test]
fn the_fast_route_pauses_for_exactly_one_open() -> varve::Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("route.varve");
    build(&path)?;
    let source = |path: &std::path::Path| -> varve::Result<LazyOpenSource> {
        Ok(VarveFile::open_readonly_lazy_with_report(spec(), path)?.1)
    };

    assert_eq!(source(&path)?, LazyOpenSource::HeaderTails);
    {
        let mut writer = VarveWriter::open(spec(), &path)?;
        writer.replace_fixed(0, &Sample { value: 4242 })?;
    }
    assert_eq!(source(&path)?, LazyOpenSource::FullScan);
    {
        let mut writer = VarveWriter::open(spec(), &path)?;
        writer.push(&Sample { value: 1 })?;
        writer.flush()?;
    }
    assert_eq!(source(&path)?, LazyOpenSource::HeaderTails);
    Ok(())
}
