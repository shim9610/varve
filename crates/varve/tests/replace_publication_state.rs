//! DUR-01 regression: when the atomic pathname publication has already
//! succeeded but the post-publication parent-directory sync fails, the writer
//! must be rebound to the published generation (or poisoned) and the caller
//! must receive the typed publication state instead of a plain failure.
//!
//! DUR2-01 regression: `ReplaceFileW` failures must be classified per the
//! Microsoft contract — 1175 is a documented no-mutation failure, while
//! 1176/1177 leave the pathnames in a possibly partially moved state, after
//! which the replacement temp file must be preserved and the writer poisoned.

#![allow(unexpected_cfgs)]

#[cfg(not(feature = "scalable-fault-injection"))]
#[test]
#[ignore = "requires the scalable-fault-injection feature"]
fn replace_publication_state_requires_feature_wiring() {}

/// DUR2-01: the pure `ReplaceFileW` error classification, testable on every
/// host because a real 1176/1177 cannot be reproduced on demand.
mod classification {
    use varve::{ReplacePublicationFailure, classify_replace_publication_error};

    #[test]
    fn error_1175_is_the_documented_no_mutation_failure() {
        // ERROR_UNABLE_TO_REMOVE_REPLACED: the replaced file is intact under
        // its original name.
        assert_eq!(
            classify_replace_publication_error(1175),
            ReplacePublicationFailure::ReplacedFileIntact
        );
    }

    #[test]
    fn errors_1176_and_1177_are_indeterminate_publication() {
        // ERROR_UNABLE_TO_MOVE_REPLACEMENT and
        // ERROR_UNABLE_TO_MOVE_REPLACEMENT_2: names, streams and attributes
        // may be partially moved; the target pathname may already be gone.
        assert_eq!(
            classify_replace_publication_error(1176),
            ReplacePublicationFailure::IndeterminatePublication
        );
        assert_eq!(
            classify_replace_publication_error(1177),
            ReplacePublicationFailure::IndeterminatePublication
        );
    }

    #[test]
    fn every_other_os_error_is_pre_publication() {
        // Sharing violations, access denied, invalid parameter, and the codes
        // adjacent to the documented pair must never be treated as having
        // mutated the target pathname.
        for raw in [0, 2, 3, 5, 32, 87, 1174, 1178, 6, 123] {
            assert_eq!(
                classify_replace_publication_error(raw),
                ReplacePublicationFailure::PrePublication,
                "raw os error {raw}",
            );
        }
    }
}

#[cfg(feature = "scalable-fault-injection")]
mod enabled {
    use std::sync::Mutex;

    use varve::scalable_fault::{FAULT_ENV, TRACE_ENV, arm_from_env};
    use varve::{
        BlockDescriptor, BlockKind, Decoder, Encoder, Endian, Error, FormatSpec, IndexPolicy,
        IntegrityPolicy, ManifestPolicy, ReadLimits, RecoveryPolicy, ReplaceStrategy, Result,
        VarveBlock, VarveDecode, VarveEncode, VarveFile, WireType,
    };

    // The injected-failure counter and the fault registry are process-global;
    // serialize every test that arms either of them.
    static FAULT_GATE: Mutex<()> = Mutex::new(());

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    struct PubRecord(u64);

    impl VarveEncode for PubRecord {
        const WIRE_TYPE: WireType = WireType::U64;

        fn encode_varve(&self, encoder: &mut Encoder) -> Result<()> {
            self.0.encode_varve(encoder)
        }
    }

    impl VarveDecode for PubRecord {
        const WIRE_TYPE: WireType = WireType::U64;

        fn decode_varve(decoder: &mut Decoder<'_>) -> Result<Self> {
            Ok(Self(u64::decode_varve(decoder)?))
        }
    }

    impl VarveBlock for PubRecord {
        const ID: u32 = 62;
        const VERSION: u16 = 1;
        const KIND: BlockKind = BlockKind::Fixed;
        const ENDIAN: Option<Endian> = None;
        const IS_KEYED: bool = false;
        const SCHEMA_FINGERPRINT: u64 = 0x5055_424C_0000_003E;
    }

    static BLOCKS: &[BlockDescriptor] = &[BlockDescriptor {
        id: PubRecord::ID,
        name: "ReplacePublicationRecord",
        version: PubRecord::VERSION,
        kind: PubRecord::KIND,
        fields: &[],
    }];

    fn spec() -> FormatSpec {
        FormatSpec::new(
            b"VSPUB",
            1,
            Endian::Little,
            0,
            IndexPolicy::ScanOnOpen,
            IntegrityPolicy::None,
            RecoveryPolicy::Strict,
            ManifestPolicy::None,
            BLOCKS,
        )
        .with_read_limits(ReadLimits::STANDARD)
    }

    #[test]
    fn parent_sync_failure_after_publication_rebinds_writer_to_new_generation() -> Result<()> {
        let _gate = FAULT_GATE.lock().unwrap();
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("publication.varve");
        let mut writer = VarveFile::create(spec(), &path)?;
        writer.push(&PubRecord(1))?;
        writer.sync()?;

        VarveFile::inject_parent_sync_failures(1);
        match writer.replace(0, &PubRecord(2), ReplaceStrategy::RewriteFile) {
            Err(Error::PublishedButParentSyncPending { .. }) => {}
            other => panic!("expected PublishedButParentSyncPending, got {other:?}"),
        }

        // Publication already happened: the pathname resolves to the new
        // generation even though the caller saw an error.
        assert_eq!(
            VarveFile::open_readonly(spec(), &path)?
                .blocks::<PubRecord>()?
                .get(0)?,
            Some(PubRecord(2))
        );

        // The writer must not silently keep writing to the orphaned
        // pre-publication object: it was rebound to the published generation
        // and stays usable, so later appends are reachable via the pathname.
        writer.push(&PubRecord(3))?;
        writer.sync()?;
        drop(writer);

        let reopened = VarveFile::open_readonly(spec(), &path)?;
        let blocks = reopened.blocks::<PubRecord>()?;
        assert_eq!(blocks.get(0)?, Some(PubRecord(2)));
        assert_eq!(blocks.get(1)?, Some(PubRecord(3)));
        Ok(())
    }

    #[test]
    fn indeterminate_publication_poisons_writer_and_preserves_temp() -> Result<()> {
        let _gate = FAULT_GATE.lock().unwrap();
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("indeterminate.varve");
        let mut writer = VarveFile::create(spec(), &path)?;
        writer.push(&PubRecord(1))?;
        writer.sync()?;

        // Models an unreconciled ReplaceFileW 1176/1177 outcome (DUR2-01):
        // the pathname state is unknown to the caller.
        VarveFile::inject_replace_indeterminate_failures(1);
        match writer.replace(0, &PubRecord(2), ReplaceStrategy::RewriteFile) {
            Err(Error::ReplacePublicationIndeterminate { .. }) => {}
            other => panic!("expected ReplacePublicationIndeterminate, got {other:?}"),
        }

        // The replacement temp file may be the only surviving copy of the new
        // generation, so it must be preserved for out-of-band reconciliation.
        let preserved: Vec<_> = std::fs::read_dir(directory.path())?
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.file_name())
            .filter(|name| name.to_string_lossy().contains(".rewrite."))
            .collect();
        assert!(
            !preserved.is_empty(),
            "the replacement temp file must survive an indeterminate publication",
        );

        // The writer is poisoned: a blind retry could publish over an unknown
        // generation, so retries and further writes are refused.
        assert!(matches!(
            writer.replace(0, &PubRecord(2), ReplaceStrategy::RewriteFile),
            Err(Error::WriterPoisoned(_))
        ));
        assert!(matches!(
            writer.push(&PubRecord(3)),
            Err(Error::WriterPoisoned(_))
        ));
        drop(writer);

        // The injected fault fired before the publication was attempted, so
        // the target pathname still resolves to the previous generation.
        let reopened = VarveFile::open_readonly(spec(), &path)?;
        assert_eq!(reopened.blocks::<PubRecord>()?.get(0)?, Some(PubRecord(1)),);
        Ok(())
    }

    #[test]
    fn replace_without_injected_failure_stays_durable() -> Result<()> {
        let _gate = FAULT_GATE.lock().unwrap();
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("durable.varve");
        let mut writer = VarveFile::create(spec(), &path)?;
        writer.push(&PubRecord(4))?;
        writer.sync()?;

        let sequence = writer.replace(0, &PubRecord(5), ReplaceStrategy::RewriteFile)?;
        assert!(sequence > 0);
        writer.push(&PubRecord(6))?;
        writer.sync()?;
        drop(writer);

        let reopened = VarveFile::open_readonly(spec(), &path)?;
        let blocks = reopened.blocks::<PubRecord>()?;
        assert_eq!(blocks.get(0)?, Some(PubRecord(5)));
        assert_eq!(blocks.get(1)?, Some(PubRecord(6)));
        Ok(())
    }

    #[test]
    fn resident_replace_reaches_registered_fault_points() -> Result<()> {
        let _gate = FAULT_GATE.lock().unwrap();
        let directory = tempfile::tempdir()?;
        let trace = directory.path().join("trace.tsv");
        // SAFETY: FAULT_GATE serializes every test that touches these
        // process-global variables, and no other thread reads the environment
        // concurrently in this test binary.
        unsafe {
            std::env::set_var(FAULT_ENV, "trace");
            std::env::set_var(TRACE_ENV, &trace);
        }
        let guard = arm_from_env().expect("arm scalable fault trace mode");

        let run = (|| -> Result<()> {
            let path = directory.path().join("traced.varve");
            let mut writer = VarveFile::create(spec(), &path)?;
            writer.push(&PubRecord(8))?;
            writer.sync()?;
            writer.replace(0, &PubRecord(9), ReplaceStrategy::RewriteFile)?;
            Ok(())
        })();
        drop(guard);
        run?;

        // The resident replace path must cross the registered publication
        // boundaries, proving the DUR-01 hooks are wired for resident files.
        let contents = std::fs::read_to_string(&trace)?;
        assert!(
            contents.lines().any(|line| line.contains("replace.atomic")),
            "resident replace did not cross replace.atomic: {contents:?}"
        );
        assert!(
            contents
                .lines()
                .any(|line| line.contains("replace.parent_sync")),
            "resident replace did not cross replace.parent_sync: {contents:?}"
        );
        Ok(())
    }
}

/// F-01 and F-02 regressions: every replacement entry point that resolves its
/// target by block id alone must refuse a stored record whose version differs
/// from the type being written, and the one entry point that mutates record
/// bytes in place must refuse keyed blocks outright.
///
/// These are long-standing defects, not regressions of a recent patch:
/// `replace_rewrite` has selected by block id since the initial implementation
/// and `replace_fixed_in_place_exclusive` since the 0.2.0 hardening. They
/// survived earlier reviews because reaching them needs `schema_hash = 0` -
/// a *supported* opt-out from header-level schema locking, which is exactly
/// what the specs below use.
mod cross_version_and_keyed_refusals {
    use varve::{
        BlockDescriptor, BlockKind, Decoder, Encoder, Endian, Error, FormatSpec, IndexPolicy,
        IntegrityPolicy, ManifestPolicy, ReadLimits, RecoveryPolicy, ReplaceStrategy, Result,
        VarveBlock, VarveDecode, VarveEncode, VarveFile, VarveKeyedBlock, VarveReplaceBlock,
        WireType,
    };

    const RECORD_ID: u32 = 63;
    const KEYED_ID: u32 = 64;

    macro_rules! versioned_record {
        ($name:ident, $version:literal, $fingerprint:literal) => {
            #[derive(Clone, Copy, Debug, PartialEq, Eq)]
            struct $name(u64);

            impl VarveEncode for $name {
                const WIRE_TYPE: WireType = WireType::U64;

                fn encode_varve(&self, encoder: &mut Encoder) -> Result<()> {
                    self.0.encode_varve(encoder)
                }
            }

            impl VarveDecode for $name {
                const WIRE_TYPE: WireType = WireType::U64;

                fn decode_varve(decoder: &mut Decoder<'_>) -> Result<Self> {
                    Ok(Self(u64::decode_varve(decoder)?))
                }
            }

            impl VarveBlock for $name {
                const ID: u32 = RECORD_ID;
                const VERSION: u16 = $version;
                const KIND: BlockKind = BlockKind::Fixed;
                const ENDIAN: Option<Endian> = None;
                const IS_KEYED: bool = false;
                const SCHEMA_FINGERPRINT: u64 = $fingerprint;
            }

            impl VarveReplaceBlock for $name {
                fn validate_replacement(_old: &Self, _new: &Self) -> Result<()> {
                    Ok(())
                }
            }
        };
    }

    versioned_record!(RecordV1, 1, 0x5856_5231_0000_003F);
    versioned_record!(RecordV2, 2, 0x5856_5232_0000_003F);

    static BLOCKS_V1: &[BlockDescriptor] = &[BlockDescriptor {
        id: RECORD_ID,
        name: "CrossVersionRecord",
        version: 1,
        kind: BlockKind::Fixed,
        fields: &[],
    }];

    static BLOCKS_V2: &[BlockDescriptor] = &[BlockDescriptor {
        id: RECORD_ID,
        name: "CrossVersionRecord",
        version: 2,
        kind: BlockKind::Fixed,
        fields: &[],
    }];

    /// `schema_hash` is deliberately 0: header-level schema locking is the
    /// gate that normally stops a v2 program from opening a v1 file, and this
    /// opt-out is what makes the version check inside each replacement path
    /// load-bearing rather than redundant.
    fn spec(blocks: &'static [BlockDescriptor]) -> FormatSpec {
        FormatSpec::new(
            b"VSXVR",
            1,
            Endian::Little,
            0,
            IndexPolicy::ScanOnOpen,
            IntegrityPolicy::None,
            RecoveryPolicy::Strict,
            ManifestPolicy::None,
            blocks,
        )
        .with_read_limits(ReadLimits::STANDARD)
    }

    fn create_v1(path: &std::path::Path) -> Result<()> {
        let mut writer = VarveFile::create(spec(BLOCKS_V1), path)?;
        writer.push(&RecordV1(1))?;
        writer.flush()?;
        Ok(())
    }

    fn assert_version_mismatch(error: Error) {
        match error {
            Error::BlockVersionMismatch {
                block_id,
                expected,
                actual,
            } => {
                assert_eq!(block_id, RECORD_ID);
                assert_eq!(expected, 2);
                assert_eq!(actual, 1);
            }
            other => panic!("expected BlockVersionMismatch, got {other:?}"),
        }
    }

    /// The stored file must still be a well-formed v1 file that a v1 reader
    /// can decode: a refused replacement leaves nothing behind.
    fn assert_still_v1(path: &std::path::Path) -> Result<()> {
        let reader = VarveFile::open_readonly(spec(BLOCKS_V1), path)?;
        assert_eq!(reader.blocks::<RecordV1>()?.get(0)?, Some(RecordV1(1)));
        Ok(())
    }

    #[test]
    fn rewrite_replacement_refuses_a_cross_version_target() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("rewrite.varve");
        create_v1(&path)?;

        let mut writer = VarveFile::open(spec(BLOCKS_V2), &path)?;
        assert_version_mismatch(
            writer
                .replace_rewrite(0, &RecordV2(2))
                .expect_err("a v2 rewrite over a v1 record must be refused"),
        );
        drop(writer);
        assert_still_v1(&path)
    }

    #[test]
    fn fixed_replacement_refuses_a_cross_version_target() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("fixed.varve");
        create_v1(&path)?;

        let mut writer = VarveFile::open(spec(BLOCKS_V2), &path)?;
        assert_version_mismatch(
            writer
                .replace_fixed(0, &RecordV2(2))
                .expect_err("a v2 fixed replacement over a v1 record must be refused"),
        );
        drop(writer);
        assert_still_v1(&path)
    }

    #[test]
    fn block_replacement_refuses_a_cross_version_target() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("block.varve");
        create_v1(&path)?;

        let mut writer = VarveFile::open(spec(BLOCKS_V2), &path)?;
        assert_version_mismatch(
            writer
                .replace_block(0, &RecordV2(2))
                .expect_err("a v2 block replacement over a v1 record must be refused"),
        );
        drop(writer);
        assert_still_v1(&path)
    }

    #[test]
    fn in_place_replacement_refuses_a_cross_version_target() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("in-place.varve");
        create_v1(&path)?;

        let mut writer = VarveFile::open(spec(BLOCKS_V2), &path)?;
        // SAFETY: this process holds the only handle to the path, and the call
        // is expected to refuse before it touches the file at all.
        let error = unsafe { writer.replace_fixed_in_place_exclusive(0, &RecordV2(2)) }
            .expect_err("a v2 in-place replacement over a v1 record must be refused");
        assert_version_mismatch(error);
        drop(writer);
        assert_still_v1(&path)
    }

    /// F-01, the re-verification harness, kept as a regression.
    ///
    /// Round 12 added one test per entry point at ordinal 0. Re-verification
    /// went wider — every entry point including both `replace` strategy
    /// wrappers, at every ordinal of a two-record file — and additionally
    /// asserted that the file is *byte-identical* after each refusal, which the
    /// per-path tests did not: `assert_still_v1` proves the file still decodes,
    /// not that nothing was written. This test is that harness. It also pins
    /// the negative half: an out-of-range ordinal must still fail as an ordinal
    /// (`UnexpectedEof`), so the version refusal is not masking resolution.
    #[test]
    fn every_replacement_entry_point_refuses_a_cross_version_target_at_every_ordinal() -> Result<()>
    {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("every-entry-point.varve");
        {
            let mut writer = VarveFile::create(spec(BLOCKS_V1), &path)?;
            writer.push(&RecordV1(1))?;
            writer.push(&RecordV1(2))?;
            writer.flush()?;
        }
        let original = std::fs::read(&path)?;

        for ordinal in [0, 1] {
            let mut writer = VarveFile::open(spec(BLOCKS_V2), &path)?;
            let mut refusals: Vec<Error> = vec![
                writer
                    .replace_rewrite(ordinal, &RecordV2(9))
                    .expect_err("rewrite must refuse a cross-version target"),
                writer
                    .replace_fixed(ordinal, &RecordV2(9))
                    .expect_err("fixed must refuse a cross-version target"),
                writer
                    .replace_block(ordinal, &RecordV2(9))
                    .expect_err("block must refuse a cross-version target"),
                writer
                    .replace(ordinal, &RecordV2(9), ReplaceStrategy::FixedCopyOnWrite)
                    .expect_err("the FixedCopyOnWrite wrapper must refuse it too"),
                writer
                    .replace(ordinal, &RecordV2(9), ReplaceStrategy::RewriteFile)
                    .expect_err("the RewriteFile wrapper must refuse it too"),
            ];
            // SAFETY: this process holds the only handle to the path, and the
            // call is expected to refuse before it touches the file at all.
            refusals.push(
                unsafe { writer.replace_fixed_in_place_exclusive(ordinal, &RecordV2(9)) }
                    .expect_err("in-place must refuse a cross-version target"),
            );
            for error in refusals {
                assert_version_mismatch(error);
            }
            // An ordinal that does not exist must still fail as an ordinal.
            assert!(
                matches!(
                    writer.replace_fixed(2, &RecordV2(9)),
                    Err(Error::UnexpectedEof)
                ),
                "an out-of-range ordinal must resolve to UnexpectedEof, not to the version                  refusal, or the refusal would be hiding a resolution failure"
            );
            drop(writer);

            assert_eq!(
                std::fs::read(&path)?,
                original,
                "ordinal {ordinal}: a refused replacement must leave the file byte-identical -                  nothing may be written before the refusal (invariant 3)"
            );
        }

        let reader = VarveFile::open_readonly(spec(BLOCKS_V1), &path)?;
        assert_eq!(reader.blocks::<RecordV1>()?.get(0)?, Some(RecordV1(1)));
        assert_eq!(reader.blocks::<RecordV1>()?.get(1)?, Some(RecordV1(2)));
        Ok(())
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    struct KeyedRecord {
        key: u64,
    }

    impl VarveEncode for KeyedRecord {
        const WIRE_TYPE: WireType = WireType::U64;

        fn encode_varve(&self, encoder: &mut Encoder) -> Result<()> {
            self.key.encode_varve(encoder)
        }
    }

    impl VarveDecode for KeyedRecord {
        const WIRE_TYPE: WireType = WireType::U64;

        fn decode_varve(decoder: &mut Decoder<'_>) -> Result<Self> {
            Ok(Self {
                key: u64::decode_varve(decoder)?,
            })
        }
    }

    impl VarveBlock for KeyedRecord {
        const ID: u32 = KEYED_ID;
        const VERSION: u16 = 1;
        const KIND: BlockKind = BlockKind::Fixed;
        const ENDIAN: Option<Endian> = None;
        const IS_KEYED: bool = true;
        const SCHEMA_FINGERPRINT: u64 = 0x4B45_5931_0000_0040;
    }

    impl VarveKeyedBlock for KeyedRecord {
        type Key = u64;

        fn key(&self) -> Self::Key {
            self.key
        }
    }

    static KEYED_BLOCKS: &[BlockDescriptor] = &[BlockDescriptor {
        id: KEYED_ID,
        name: "KeyedInPlaceRecord",
        version: 1,
        kind: BlockKind::Fixed,
        fields: &[],
    }];

    fn keyed_spec() -> FormatSpec {
        FormatSpec::new(
            b"VSXVK",
            1,
            Endian::Little,
            0,
            IndexPolicy::new(true, true, true, true),
            IntegrityPolicy::None,
            RecoveryPolicy::Strict,
            ManifestPolicy::None,
            KEYED_BLOCKS,
        )
        .with_read_limits(ReadLimits::STANDARD)
    }

    /// F-02: a resident keyed-tail cache must never outlive an in-place
    /// record mutation.
    ///
    /// The cache maps canonical key bytes to tail offsets and maintained
    /// keyed pushes read their predecessor from it. In-place replacement
    /// rewrites the stored payload under the existing header, so the record
    /// may no longer carry the key the cache filed it under - and unlike the
    /// copy-on-write paths there is no rebind to invalidate it. This models
    /// the review harness exactly: a record's key is changed from 1 to 2 while
    /// exclusivity is satisfied.
    ///
    /// Pre-fix, the next key-2 push reported no predecessor and a later key-1
    /// push reported the stale offset, so the physical keyed chains stopped
    /// representing the values actually stored. The cache is now dropped
    /// before the first byte is written, so both pushes see the truth.
    #[test]
    fn an_in_place_mutation_cannot_leave_a_stale_keyed_tail() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("keyed-in-place.varve");
        let mut writer = VarveFile::create(keyed_spec(), &path)?;
        let first = writer.push_keyed_info(&KeyedRecord { key: 1 })?;
        assert_eq!(first.prev_same_key_offset, None);
        writer.flush()?;

        // SAFETY: this process holds the only handle to the path. The call
        // deliberately violates the documented same-key part of the contract,
        // which is the point: the cache must not survive it either way.
        unsafe { writer.replace_fixed_in_place_exclusive(0, &KeyedRecord { key: 2 })? };

        // The record at that offset now stores key 2, so key 2's predecessor
        // is that record and key 1 no longer has one.
        let second = writer.push_keyed_info(&KeyedRecord { key: 2 })?;
        assert_eq!(
            second.prev_same_key_offset,
            Some(first.record_offset),
            "a key-2 push must see the mutated record as its predecessor"
        );
        let third = writer.push_keyed_info(&KeyedRecord { key: 1 })?;
        assert_eq!(
            third.prev_same_key_offset, None,
            "a key-1 push must not be linked to a record that no longer stores key 1"
        );
        writer.flush()?;
        Ok(())
    }

    /// The same-key in-place replacement that the unsafe contract does allow
    /// must keep working, and must keep the chain correct afterwards.
    #[test]
    fn a_same_key_in_place_replacement_keeps_the_chain_correct() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("keyed-in-place-same.varve");
        let mut writer = VarveFile::create(keyed_spec(), &path)?;
        let first = writer.push_keyed_info(&KeyedRecord { key: 5 })?;
        writer.flush()?;

        // SAFETY: this process holds the only handle to the path, and the key
        // is unchanged, so the documented contract is satisfied in full.
        unsafe { writer.replace_fixed_in_place_exclusive(0, &KeyedRecord { key: 5 })? };

        let second = writer.push_keyed_info(&KeyedRecord { key: 5 })?;
        assert_eq!(second.prev_same_key_offset, Some(first.record_offset));
        writer.flush()?;
        drop(writer);

        let reader = VarveFile::open_readonly(keyed_spec(), &path)?;
        let blocks = reader.blocks::<KeyedRecord>()?;
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks.get(0)?, Some(KeyedRecord { key: 5 }));
        assert_eq!(blocks.get(1)?, Some(KeyedRecord { key: 5 }));
        Ok(())
    }
}
