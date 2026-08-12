//! Why a disk-index format cannot carry a header tail region, from both sides.
//!
//! This file used to assert that a rewritten region does not invalidate a
//! published sidecar — the narrowing of `primary_identity` and
//! `primary_generation` that landed in 93c64c3. Those two cases could not
//! survive the refusal that followed, and the reason is the finding this file
//! now records.
//!
//! **The region is only ever written for a commit marker.** Nothing else names
//! a commit point, so a format without one leaves the region cold for the life
//! of the file — measured before the refusal existed: five flushes over a
//! hundred records, both slots still at `count = 0`. `FormatSpec::validate`
//! therefore refuses `header_tails` without a `transaction_marker` policy.
//!
//! **The streaming path refuses a commit marker.** `NativeStreamScanner`
//! errors with `StreamingUnsupported` the moment it frames a `COMMIT_BLOCK_ID`
//! record, and the disk index reads its primary through that scanner. So a
//! disk-index format cannot have markers.
//!
//! The two together are exclusive, and that is the whole content of this file.
//! It matters beyond bookkeeping: it means the hazard 93c64c3 was written for —
//! a commit rewriting header bytes underneath a published sidecar — **cannot
//! arise from a writer**, because no declarable format both warms the region
//! and carries a sidecar. The blanking stays, as the thing that keeps the two
//! windows honest against any future mutable header block and against a
//! hand-edited file; its narrowness is asserted by
//! `varve_core::file::tests::blanking_covers_the_mutable_payload_and_nothing_else`,
//! which does not need either half of this combination.
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

/// Side two: the disk index cannot handle a primary that carries commit
/// markers.
///
/// Asserted against the real path rather than by reading the scanner, and the
/// whole sequence is one `Result` because the refusal does not wait for the
/// read: it arrives as soon as the indexed path frames a marker, which is
/// before this can get a reader open.
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
