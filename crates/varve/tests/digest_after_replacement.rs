//! A replacement must leave a digest the next lazy writer can resume from.
//!
//! The digest is the one derived record a lazy open **trusts instead of
//! recounting**: it seeds the writer with `sequence_high_water + 1`. A scan
//! recounts, so a scanning open is unaffected by a wrong mark and cannot pin
//! this — every case here goes through `open_lazy` on purpose.
//!
//! Two separate defects landed on the same symptom, both measured on this
//! shape (`crc32 + open_digest_on_flush`, 64 records, replace record 0):
//!
//! - The three republish sites passed `new_index.iter().map(..).max()`, and
//!   `new_index` holds only the records written *before* the digest. The digest
//!   sat at sequence 64 and recorded 63; a lazy writer resumed at 64 and
//!   duplicated it. The append path had already been corrected for exactly this
//!   and the three copies had not — they still carried the comment saying a
//!   digest "never describes itself".
//! - `replace_fixed` rewrites no derived record at all (it copies the
//!   generation byte for byte and patches one record) while assigning the
//!   replacement a *fresh* sequence, raising the file's true maximum above the
//!   mark the copied digest still reports. It gave the replacement 65 against a
//!   digest still reporting 64, and the next lazy append also took 65.
//!
//! Either way the next scan refuses the file with
//! `InvalidCanonicalEncoding("duplicate native record sequence")` — the file
//! stops opening, so this is data loss, not a degraded index.

#![cfg(feature = "integrity")]

use varve::{VarveBlock, varve_format};

#[derive(Clone, Debug, PartialEq, VarveBlock)]
#[varve(id = 1, version = 1, kind = "fixed")]
struct Line {
    value: u32,
}

#[derive(Clone, Debug, PartialEq, VarveBlock)]
#[varve(id = 2, version = 1, kind = "variable")]
struct Note {
    #[varve(field_id = 1)]
    text: String,
}

varve_format! {
    pub struct Digested {
        magic: b"VDIGREPL";
        version: 1;
        endian: little;
        schema_hash: computed;
        integrity: crc32;
        blocks: [Line, Note];
    }
}

/// `open_digest_on_flush` forces `block_offset_chain` on, so this is also a
/// record-footer format — which is why `replace` routes it to `replace_block`.
fn spec() -> varve::FormatSpec {
    Digested::spec().with_index_policy(
        Digested::spec()
            .index_policy
            .with_open_digest_on_flush(true),
    )
}

fn temp_path(name: &str) -> std::path::PathBuf {
    let mut path = std::env::temp_dir();
    path.push(format!(
        "varve_digest_after_replacement_{name}_{}",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&path);
    path
}

fn build(path: &std::path::Path) -> varve::Result<()> {
    let mut file = spec().create(path)?;
    for value in 0..64u32 {
        file.push(&Line { value })?;
    }
    file.push(&Note {
        text: "original".into(),
    })?;
    file.flush()?;
    Ok(())
}

/// Append through a **lazy** writer, then scan. The scan is the oracle: it
/// recounts, so it refuses exactly when the digest sent the writer onto a
/// sequence that was already taken.
fn append_lazily_then_scan(path: &std::path::Path) -> varve::Result<usize> {
    {
        let mut writer = varve::VarveWriter::open_lazy(spec(), path)?;
        writer.push(&Line { value: 7777 })?;
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
    let path = temp_path("control");
    build(&path)?;
    assert_eq!(append_lazily_then_scan(&path)?, 68);
    Ok(())
}

#[test]
fn replace_leaves_a_resumable_digest() -> varve::Result<()> {
    let path = temp_path("replace");
    build(&path)?;
    {
        let mut file = spec().open(&path)?;
        file.replace(0, &Line { value: 4242 })?;
    }
    assert_eq!(append_lazily_then_scan(&path)?, 68);
    Ok(())
}

/// The variable cell, which is the one `replace` could not reach at all before
/// `ReplaceStrategy` was removed — both strategies refused it, so it could not
/// corrupt anything either. It reaches `replace_block` now, so it has to be
/// pinned here rather than assumed to follow from the fixed case.
#[test]
fn replacing_a_variable_record_leaves_a_resumable_digest() -> varve::Result<()> {
    let path = temp_path("variable");
    build(&path)?;
    {
        let mut file = spec().open(&path)?;
        file.replace(
            0,
            &Note {
                text: "a longer replacement".into(),
            },
        )?;
    }
    assert_eq!(append_lazily_then_scan(&path)?, 68);
    Ok(())
}

#[test]
fn replace_block_leaves_a_resumable_digest() -> varve::Result<()> {
    let path = temp_path("block");
    build(&path)?;
    {
        let mut file = spec().open(&path)?;
        file.replace_block(0, &Line { value: 4242 })?;
    }
    assert_eq!(append_lazily_then_scan(&path)?, 68);
    Ok(())
}

/// `replace_fixed` cannot maintain the digest — it rewrites no derived record —
/// so it is refused rather than left to publish a file that stops opening. The
/// capability is not lost: this format is footered, so `replace` routes it to
/// `replace_block`, which the test above pins.
#[test]
fn an_in_place_replacement_is_refused_on_a_digest_format() -> varve::Result<()> {
    let path = temp_path("fixed_refused");
    build(&path)?;
    let mut file = spec().open(&path)?;
    assert!(matches!(
        file.replace_fixed(0, &Line { value: 4242 }),
        Err(varve::Error::InvalidFormatSpec(message))
            if message.contains("open_digest_on_flush"),
    ));
    Ok(())
}

/// The refusal is specific to the digest, and this is why: a checkpoint is
/// re-derived by the scanning open, so a stale one is not observable the way a
/// stale digest is. Measured — all four routes were already clean on a
/// `checkpoint_on_flush` format — so `replace_fixed` keeps working there.
#[test]
fn a_checkpoint_format_keeps_the_in_place_route() -> varve::Result<()> {
    let checkpointed = Digested::spec()
        .with_index_policy(Digested::spec().index_policy.with_checkpoint_on_flush(true));
    let path = temp_path("checkpoint");
    {
        let mut file = checkpointed.create(&path)?;
        for value in 0..64u32 {
            file.push(&Line { value })?;
        }
        file.flush()?;
    }
    let mut file = checkpointed.open(&path)?;
    file.replace_fixed(0, &Line { value: 4242 })?;
    drop(file);

    let reopened = checkpointed.open_readonly(&path)?;
    let blocks = reopened.blocks::<Line>()?;
    assert_eq!(blocks.len(), 64);
    assert_eq!(blocks.get(0)?, Some(Line { value: 4242 }));
    Ok(())
}
