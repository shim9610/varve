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
