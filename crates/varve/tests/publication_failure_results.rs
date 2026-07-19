//! Direct public stream/indexed failure-result contracts.
//!
//! The crash matrix in `scalable_crash_faults` proves the on-disk state after
//! a process boundary, and `replace_publication_state` proves the resident
//! `VarveFile::replace` publication classification. Neither covers what the
//! *public scalable writer API* returns to an in-process caller when a
//! publication or a batch commit fails: `flush`, `sync` and the batch entry
//! points must surface the typed failure with its documented state — the
//! writer is either poisoned or the call is safely retryable, and a
//! publication that already happened is reported as
//! `PublishedButParentSyncPending` rather than promoted to plain success.

#![allow(unexpected_cfgs)]

#[cfg(not(feature = "scalable-fault-injection"))]
#[test]
#[ignore = "requires the scalable-fault-injection feature"]
fn publication_failure_results_require_feature_wiring() {}

#[cfg(feature = "scalable-fault-injection")]
mod enabled {
    use std::cell::Cell;
    use std::path::{Path, PathBuf};
    use std::sync::{Mutex, MutexGuard};

    use varve::{
        BatchOptions, BlockDescriptor, BlockKind, Decoder, DiskIndexBatchOptions, DiskIndexOptions,
        DiskIndexPlan, DiskIndexedBlock, Encoder, Endian, Error, FormatSpec, IndexPolicy,
        IntegrityPolicy, ManifestPolicy, ReadLimits, RecoveryPolicy, Result, StreamOptions,
        VarveBlock, VarveDecode, VarveEncode, VarveFile, VarveIndexedReader, VarveIndexedWriter,
        VarveKeyedBlock, VarveStreamReader, VarveStreamWriter, WireType, disk_index_sidecar_path,
    };

    /// The injected-failure counters are process-global, so any test in this
    /// binary that publishes a sidecar could consume a failure armed by
    /// another one.
    static FAULT_GATE: Mutex<()> = Mutex::new(());

    fn fault_gate() -> MutexGuard<'static, ()> {
        FAULT_GATE
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Encoding this value fails, which is the only deterministic way to stop
    /// a batch part-way through from outside the crate: the failure happens
    /// after the preceding records of the same batch were already appended.
    const POISON_VALUE: u64 = u64::MAX;

    thread_local! {
        static ENCODE_CALLS: Cell<u64> = const { Cell::new(0) };
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    struct FaultRecord {
        key: u64,
        value: u64,
    }

    impl FaultRecord {
        const fn ok(key: u64) -> Self {
            Self {
                key,
                value: 1_000 + key,
            }
        }

        const fn poison(key: u64) -> Self {
            Self {
                key,
                value: POISON_VALUE,
            }
        }
    }

    impl VarveEncode for FaultRecord {
        const WIRE_TYPE: WireType = <(u64, u64) as VarveEncode>::WIRE_TYPE;

        fn encode_varve(&self, encoder: &mut Encoder) -> Result<()> {
            ENCODE_CALLS.set(ENCODE_CALLS.get() + 1);
            if self.value == POISON_VALUE {
                return Err(Error::InvalidCanonicalEncoding(
                    "publication-failure test encode fault",
                ));
            }
            (self.key, self.value).encode_varve(encoder)
        }
    }

    impl VarveDecode for FaultRecord {
        const WIRE_TYPE: WireType = <(u64, u64) as VarveDecode>::WIRE_TYPE;

        fn decode_varve(decoder: &mut Decoder<'_>) -> Result<Self> {
            let (key, value) = <(u64, u64)>::decode_varve(decoder)?;
            Ok(Self { key, value })
        }
    }

    impl VarveBlock for FaultRecord {
        const ID: u32 = 51;
        const VERSION: u16 = 1;
        const KIND: BlockKind = BlockKind::Fixed;
        const ENDIAN: Option<Endian> = None;
        const SCHEMA_FINGERPRINT: u64 = 0x5055_4246_0000_0033;
        const IS_KEYED: bool = true;
    }

    impl VarveKeyedBlock for FaultRecord {
        type Key = u64;

        fn key(&self) -> u64 {
            self.key
        }
    }

    static BLOCKS: &[BlockDescriptor] = &[BlockDescriptor {
        id: FaultRecord::ID,
        name: "PublicationFailureRecord",
        version: FaultRecord::VERSION,
        kind: FaultRecord::KIND,
        fields: &[],
    }];

    fn stream_spec() -> FormatSpec {
        spec(b"VPFSTR", IndexPolicy::BlockOffsetChain)
    }

    fn indexed_spec() -> FormatSpec {
        spec(b"VPFIDX", IndexPolicy::KeyedOffsetChain)
    }

    fn spec(magic: &'static [u8], index_policy: IndexPolicy) -> FormatSpec {
        FormatSpec::new(
            magic,
            1,
            Endian::Little,
            0,
            index_policy,
            IntegrityPolicy::None,
            RecoveryPolicy::Strict,
            ManifestPolicy::None,
            BLOCKS,
        )
        .with_read_limits(ReadLimits::STANDARD)
    }

    fn index_plan() -> DiskIndexPlan {
        const INDEXED: &[DiskIndexedBlock] = &[DiskIndexedBlock::of::<FaultRecord>()];
        DiskIndexPlan::canonical(indexed_spec(), INDEXED).expect("canonical failure-result plan")
    }

    fn disk_options() -> DiskIndexOptions {
        DiskIndexOptions {
            batch: DiskIndexBatchOptions {
                max_records: 4,
                max_bytes: 1024 * 1024,
            },
            ..DiskIndexOptions::default()
        }
    }

    fn stream_options() -> StreamOptions {
        StreamOptions {
            state: disk_options(),
            ..StreamOptions::default()
        }
    }

    /// One record per chunk, so every record before the poisoned one is
    /// physically committed before the batch stops.
    const fn one_record_chunks() -> BatchOptions {
        BatchOptions {
            max_records: 1,
            max_bytes: usize::MAX,
        }
    }

    fn state_sidecar_path(native: &Path) -> PathBuf {
        let mut sidecar = native.as_os_str().to_os_string();
        sidecar.push(".vks");
        PathBuf::from(sidecar)
    }

    fn read_stream(path: &Path) -> Result<Vec<FaultRecord>> {
        VarveStreamReader::open(stream_spec(), path, stream_options())?
            .blocks::<FaultRecord>()?
            .collect()
    }

    /// Reads the durable stream contents, restoring the checkpoint first when
    /// the writer left an uncommitted tail behind.
    fn read_stream_after_recovery(path: &Path) -> Result<Vec<FaultRecord>> {
        if let Ok(values) = read_stream(path) {
            return Ok(values);
        }
        let writer =
            VarveStreamWriter::restore_checkpoint_and_open(stream_spec(), path, stream_options())?;
        drop(writer);
        read_stream(path)
    }

    // --- publication already happened: the caller must see the pending state.

    #[test]
    fn stream_create_reports_parent_sync_pending_instead_of_success() -> Result<()> {
        let _gate = fault_gate();
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("stream-create-pending.varve");

        VarveFile::inject_parent_sync_failures(1);
        match VarveStreamWriter::create(stream_spec(), &path, stream_options()) {
            Err(Error::PublishedButParentSyncPending { path: reported, .. }) => {
                assert!(
                    reported.ends_with(".vks"),
                    "the pending publication must name the state sidecar, got {reported}"
                );
            }
            other => panic!(
                "expected PublishedButParentSyncPending, got {:?}",
                other.map(|_| "writer")
            ),
        }

        // The state sidecar is published: only its parent-directory entry is
        // not yet known to be durable, so a plain open resumes writing.
        assert!(state_sidecar_path(&path).exists());
        let mut writer = VarveStreamWriter::open(stream_spec(), &path, stream_options())?;
        writer.push_info(&FaultRecord::ok(1))?;
        writer.sync()?;
        drop(writer);
        assert_eq!(read_stream(&path)?, vec![FaultRecord::ok(1)]);
        Ok(())
    }

    #[test]
    fn indexed_create_reports_parent_sync_pending_instead_of_success() -> Result<()> {
        let _gate = fault_gate();
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("indexed-create-pending.varve");

        VarveFile::inject_parent_sync_failures(1);
        match VarveIndexedWriter::create(indexed_spec(), &path, disk_options(), index_plan()) {
            Err(Error::PublishedButParentSyncPending { .. }) => {}
            other => panic!(
                "expected PublishedButParentSyncPending, got {:?}",
                other.map(|_| "writer")
            ),
        }

        assert!(disk_index_sidecar_path(&path).exists());
        let mut writer =
            VarveIndexedWriter::open(indexed_spec(), &path, disk_options(), index_plan())?;
        writer.push_info(&FaultRecord::ok(2))?;
        writer.sync()?;
        drop(writer);
        let reader = VarveIndexedReader::open(indexed_spec(), &path, disk_options(), index_plan())?;
        assert_eq!(reader.get::<FaultRecord>(&2)?, Some(FaultRecord::ok(2)));
        Ok(())
    }

    // --- batch commit failures: typed result, documented writer state.

    #[test]
    fn stream_partial_batch_poisons_the_writer_and_flush_sync_stay_typed() -> Result<()> {
        let _gate = fault_gate();
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("stream-partial-batch.varve");
        let mut writer = VarveStreamWriter::create(stream_spec(), &path, stream_options())?;
        let durable = vec![FaultRecord::ok(0), FaultRecord::ok(1)];
        writer
            .push_iter::<FaultRecord, _>(durable.clone(), one_record_chunks())
            .map_err(|error| error.source)?;
        writer.sync()?;

        let batch = vec![
            FaultRecord::ok(2),
            FaultRecord::poison(3),
            FaultRecord::ok(4),
        ];
        let error = writer
            .push_iter::<FaultRecord, _>(batch, one_record_chunks())
            .expect_err("a mid-batch encode failure must not be reported as success");
        assert!(
            matches!(error.source, Error::InvalidCanonicalEncoding(_)),
            "batch failure must carry the typed source, got {:?}",
            error.source
        );
        assert_eq!(
            error.written.records, 1,
            "the records committed before the fault must be reported to the caller"
        );

        // Records were already appended, so the writer cannot be silently
        // reused: every further public entry point reports the poisoned state
        // instead of a partial success.
        assert!(matches!(
            writer.flush(),
            Err(Error::WriterPoisoned("stream"))
        ));
        assert!(matches!(
            writer.sync(),
            Err(Error::WriterPoisoned("stream"))
        ));
        assert!(matches!(
            writer.push_info(&FaultRecord::ok(5)),
            Err(Error::WriterPoisoned("stream"))
        ));
        assert!(matches!(
            writer
                .push_iter::<FaultRecord, _>(vec![FaultRecord::ok(6)], one_record_chunks())
                .map_err(|error| error.source),
            Err(Error::WriterPoisoned("stream"))
        ));
        drop(writer);

        // Nothing beyond the last successful `sync` is durable.
        assert_eq!(read_stream_after_recovery(&path)?, durable);
        Ok(())
    }

    #[test]
    fn stream_batch_that_wrote_nothing_stays_retryable() -> Result<()> {
        let _gate = fault_gate();
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("stream-retryable-batch.varve");
        let mut writer = VarveStreamWriter::create(stream_spec(), &path, stream_options())?;

        let error = writer
            .push_iter::<FaultRecord, _>(
                vec![FaultRecord::poison(0), FaultRecord::ok(1)],
                one_record_chunks(),
            )
            .expect_err("the first record cannot encode, so the batch must fail");
        assert!(matches!(error.source, Error::InvalidCanonicalEncoding(_)));
        assert_eq!(
            error.written.records, 0,
            "no record was committed, so the batch is safely retryable"
        );

        // Safely retryable: the writer is not poisoned and a corrected batch
        // commits normally.
        let retried = vec![FaultRecord::ok(0), FaultRecord::ok(1)];
        writer
            .push_iter::<FaultRecord, _>(retried.clone(), one_record_chunks())
            .map_err(|error| error.source)?;
        writer.flush()?;
        writer.sync()?;
        drop(writer);
        assert_eq!(read_stream(&path)?, retried);
        Ok(())
    }

    #[test]
    fn indexed_batch_failure_poisons_the_writer_conservatively() -> Result<()> {
        let _gate = fault_gate();
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("indexed-partial-batch.varve");
        let mut writer =
            VarveIndexedWriter::create(indexed_spec(), &path, disk_options(), index_plan())?;
        writer
            .push_iter::<FaultRecord, _>(
                vec![FaultRecord::ok(0), FaultRecord::ok(1)],
                one_record_chunks(),
            )
            .map_err(|error| error.source)?;
        writer.sync()?;

        let error = writer
            .push_iter::<FaultRecord, _>(
                vec![FaultRecord::ok(2), FaultRecord::poison(3)],
                one_record_chunks(),
            )
            .expect_err("a failed indexed batch must not be reported as success");
        assert!(
            matches!(error.source, Error::InvalidCanonicalEncoding(_)),
            "batch failure must carry the typed source, got {:?}",
            error.source
        );

        // The indexed writer poisons on any batch failure, because a partial
        // batch can leave the native file ahead of the sidecar transaction.
        assert!(matches!(
            writer.flush(),
            Err(Error::WriterPoisoned("indexed"))
        ));
        assert!(matches!(
            writer.sync(),
            Err(Error::WriterPoisoned("indexed"))
        ));
        assert!(matches!(
            writer.push_info(&FaultRecord::ok(4)),
            Err(Error::WriterPoisoned("indexed"))
        ));
        drop(writer);

        // The synced generation survives; the poisoned batch did not publish.
        let recovered = VarveIndexedWriter::restore_checkpoint_and_open(
            indexed_spec(),
            &path,
            disk_options(),
            index_plan(),
        )?;
        drop(recovered);
        let reader = VarveIndexedReader::open(indexed_spec(), &path, disk_options(), index_plan())?;
        assert_eq!(reader.get::<FaultRecord>(&0)?, Some(FaultRecord::ok(0)));
        assert_eq!(reader.get::<FaultRecord>(&1)?, Some(FaultRecord::ok(1)));
        assert_eq!(reader.get::<FaultRecord>(&3)?, None);
        Ok(())
    }

    // --- control: without an armed fault the same calls must report success.

    #[test]
    fn unfaulted_flush_and_sync_report_success() -> Result<()> {
        let _gate = fault_gate();
        let directory = tempfile::tempdir()?;
        let stream_path = directory.path().join("control-stream.varve");
        let indexed_path = directory.path().join("control-indexed.varve");

        let mut stream = VarveStreamWriter::create(stream_spec(), &stream_path, stream_options())?;
        stream
            .push_iter::<FaultRecord, _>(vec![FaultRecord::ok(0)], one_record_chunks())
            .map_err(|error| error.source)?;
        stream.flush()?;
        stream.sync()?;
        drop(stream);

        let mut indexed = VarveIndexedWriter::create(
            indexed_spec(),
            &indexed_path,
            disk_options(),
            index_plan(),
        )?;
        indexed
            .push_iter::<FaultRecord, _>(vec![FaultRecord::ok(0)], one_record_chunks())
            .map_err(|error| error.source)?;
        indexed.flush()?;
        indexed.sync()?;
        drop(indexed);

        assert_eq!(read_stream(&stream_path)?, vec![FaultRecord::ok(0)]);
        let reader =
            VarveIndexedReader::open(indexed_spec(), &indexed_path, disk_options(), index_plan())?;
        assert_eq!(reader.get::<FaultRecord>(&0)?, Some(FaultRecord::ok(0)));
        assert!(
            ENCODE_CALLS.get() > 0,
            "the fault-capable encoder must actually be exercised"
        );
        Ok(())
    }
}
