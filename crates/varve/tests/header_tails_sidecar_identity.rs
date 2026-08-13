//! Why a disk-index format cannot carry a header tail region *today*.
//!
//! This file used to assert that a rewritten region does not invalidate a
//! published sidecar — the narrowing of `primary_identity` and
//! `primary_generation` that landed in 93c64c3. Those two cases could not
//! survive the refusal that followed, and the reason is what this file records.
//!
//! **The region is only ever written for a commit marker.** Nothing else names
//! a commit point, so a format without one leaves the region cold for the life
//! of the file — measured before the refusal existed: five flushes over a
//! hundred records, both slots still at `count = 0`. `FormatSpec::validate`
//! therefore refuses `header_tails` without a `transaction_marker` policy.
//!
//! **The streaming path refuses four internal record kinds, by policy.**
//! `NativeStreamScanner::next_entry` errors with `StreamingUnsupported` on
//! `OP`, `INDEX`, `COMMIT` and `SEGMENT` — and passes `TOMBSTONE`, `METADATA`
//! and the creation nonce straight through. So it is not that the scanner
//! cannot cope with an internal record; those four are the ones a writer it
//! does not use produces, and seeing one means "this is not a file I wrote".
//! It fails loudly rather than reporting a shape it was not designed for.
//!
//! **So the exclusivity is a decision, not a law**, and this file is careful to
//! say so. Mechanically, skipping a commit marker there is `continue` instead
//! of `Err`; whether that is *right* is a separate question — the same loop
//! enforces strictly increasing sequence two lines below, and the sidecar's
//! model assumes it wrote the primary. `header_tails` with a disk index is a
//! feature nobody has built, not an impossibility.
//!
//! What follows from it for 93c64c3: no *currently declarable* format both
//! warms the region and carries a sidecar, so a commit rewriting header bytes
//! underneath a published sidecar cannot happen today. The blanking stays — it
//! is what keeps the two windows honest against a hand-edited file and against
//! any future mutable header block, and it is what would already be right if
//! the scanner ever learns to skip a marker. Its narrowness is asserted by
//! `varve_core::file::tests::blanking_covers_the_mutable_payload_and_nothing_else`,
//! which needs neither half of this combination.
//!
//! Delete this file the day the disk index tolerates a commit marker, and
//! restore the two end-to-end cases from 93c64c3's history.

#![cfg(all(feature = "integrity", feature = "high-cardinality-dev"))]

use varve::{DiskIndexOptions, Error, IndexPolicy, varve_format};

varve_format! {
    pub format TailedIndexFormat {
        magic: b"HTSIDE";
        version: 1;
        schema_hash: computed;
        integrity: crc32;
        index: [keyed_offset_chain, header_tails];
        commit: transaction_marker(on_flush);
        blocks {
            variable Frame(id = 1, key = [scan], key_index = disk) {
                scan: u32,
                payload: Vec<u8>,
            }
        }
    }
}

/// Side one: the region needs a commit marker, so it cannot be declared away.
///
/// A format author reaching for the disk index would want the markerless policy
/// the streaming path requires, and this is what stops them from getting a
/// region that silently never fills.
#[test]
fn the_region_cannot_be_declared_without_a_commit_marker() {
    let error = TailedIndexFormat::spec()
        .with_commit_policy(varve::CommitPolicy::None)
        .validate()
        .expect_err("a markerless policy leaves the region cold forever");
    assert!(
        matches!(
            error,
            Error::InvalidFormatSpec("header_tails requires a transaction_marker commit policy")
        ),
        "{error:?}",
    );
}

/// Side two: the disk index refuses a primary that carries commit markers.
///
/// Asserted against the real path rather than by reading the scanner, and the
/// whole sequence is one `Result` because the refusal does not wait for the
/// read: it arrives as soon as the indexed path frames a marker, which is
/// before this can get a reader open.
///
/// "Refuses", not "cannot" — the scanner passes other internal records through
/// and it is these four it treats as a foreign file shape. This test pins the
/// behaviour so that relaxing it is a deliberate change with a test to update,
/// which is the opposite of the state that produced this file.
#[test]
fn a_disk_index_cannot_handle_a_primary_that_carries_commit_markers() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("tailed.varve");

    let outcome = (|| -> varve::Result<()> {
        let mut writer =
            TailedIndexFormat::create_indexed_writer(&path, DiskIndexOptions::default())?;
        for scan in 0..8u32 {
            writer.push_frame(&Frame {
                scan,
                payload: scan.to_le_bytes().to_vec(),
            })?;
        }
        writer.sync()?;
        TailedIndexFormat::open_indexed_reader(&path, DiskIndexOptions::default())?;
        Ok(())
    })();

    assert!(
        matches!(outcome, Err(Error::StreamingUnsupported)),
        "the disk index reads its primary through the stream scanner, which refuses a \
         commit marker: {outcome:?}",
    );
}

/// And the combination stays refusable rather than merely unusable.
///
/// `header_tails` composes with the two things it does need — the block offset
/// chain and a crc32 policy — so this checks the spec is otherwise sound and
/// the only thing standing between it and a working file is the disk index.
#[test]
fn the_declaration_itself_is_valid() {
    TailedIndexFormat::spec()
        .validate()
        .expect("markers plus the region is a valid declaration; the disk index is what refuses");
    assert!(TailedIndexFormat::spec().index_policy.header_tails);
    assert!(TailedIndexFormat::spec().index_policy.block_offset_chain);
    assert_eq!(
        TailedIndexFormat::spec().index_policy,
        IndexPolicy::KeyedOffsetChain.with_header_tails(true),
    );
}
