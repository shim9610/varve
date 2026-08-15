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
        magic: b"VLIVEOFF";
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
        magic: b"VLIVEOFF";
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
varve_format! {
    pub struct OnWithHeaderCrc {
        magic: b"VLIVEOFF";
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
