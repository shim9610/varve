#![allow(unexpected_cfgs)]

#[cfg(not(feature = "scalable-fault-injection"))]
#[test]
#[ignore = "requires main-agent scalable-fault-injection feature and hook wiring"]
fn scalable_crash_faults_require_feature_wiring() {}

#[cfg(feature = "scalable-fault-injection")]
mod enabled {

    use std::collections::{BTreeMap, BTreeSet};
    use std::env;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::process::{Command, ExitStatus, Stdio};
    use std::time::{SystemTime, UNIX_EPOCH};

    use varve::scalable_fault::{
        ArmGuard, FAULT_ENV, REQUIRED_POINTS, TRACE_ENV, arm_from_env, fault_point,
    };
    use varve::{
        BatchOptions, BlockDescriptor, BlockKind, Decoder, DiskIndexBatchOptions, DiskIndexOptions,
        DiskIndexPlan, DiskIndexedBlock, Encoder, Endian, FormatSpec, IndexPolicy, IntegrityPolicy,
        ManifestPolicy, ReadLimits, RecoveryPolicy, Result as VarveResult, StreamOptions,
        VarveBlock, VarveDecode, VarveEncode, VarveIndexedReader, VarveIndexedWriter,
        VarveKeyedBlock, VarveStreamReader, VarveStreamWriter, WireType, WriterLockBreakPolicy,
        bootstrap_stream_checkpoint, clear_stale_writer_lock, rebuild_disk_index,
    };

    const CHILD_ENV: &str = "VARVE_SCALABLE_FAULT_CHILD";
    const SCENARIO_ENV: &str = "VARVE_SCALABLE_FAULT_SCENARIO";
    const ROOT_ENV: &str = "VARVE_SCALABLE_FAULT_ROOT";
    const READY_FILE: &str = "ready";
    const BASE_EOF_FILE: &str = "base-eof";

    const SCENARIOS: &[Scenario] = &[
        Scenario::CreateStream,
        Scenario::CreateIndexed,
        Scenario::AppendStream,
        Scenario::AppendIndexed,
        Scenario::RestoreStream,
        Scenario::RestoreIndexed,
        Scenario::BootstrapStream,
        Scenario::ReplaceIndexed,
    ];

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum Scenario {
        CreateStream,
        CreateIndexed,
        AppendStream,
        AppendIndexed,
        RestoreStream,
        RestoreIndexed,
        BootstrapStream,
        ReplaceIndexed,
    }

    impl Scenario {
        const fn name(self) -> &'static str {
            match self {
                Self::CreateStream => "create-stream",
                Self::CreateIndexed => "create-indexed",
                Self::AppendStream => "append-stream",
                Self::AppendIndexed => "append-indexed",
                Self::RestoreStream => "restore-stream",
                Self::RestoreIndexed => "restore-indexed",
                Self::BootstrapStream => "bootstrap-stream",
                Self::ReplaceIndexed => "replace-indexed",
            }
        }

        fn parse(value: &str) -> Self {
            SCENARIOS
                .iter()
                .copied()
                .find(|scenario| scenario.name() == value)
                .unwrap_or_else(|| panic!("unknown scalable fault scenario {value:?}"))
        }
    }

    #[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
    struct TraceEvent {
        point: String,
        occurrence: u64,
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    struct StreamRecord(u64);

    impl VarveEncode for StreamRecord {
        const WIRE_TYPE: WireType = WireType::U64;

        fn encode_varve(&self, encoder: &mut Encoder) -> VarveResult<()> {
            self.0.encode_varve(encoder)
        }
    }

    impl VarveDecode for StreamRecord {
        const WIRE_TYPE: WireType = WireType::U64;

        fn decode_varve(decoder: &mut Decoder<'_>) -> VarveResult<Self> {
            Ok(Self(u64::decode_varve(decoder)?))
        }
    }

    impl VarveBlock for StreamRecord {
        const ID: u32 = 41;
        const VERSION: u16 = 1;
        const KIND: BlockKind = BlockKind::Fixed;
        const ENDIAN: Option<Endian> = None;
        const SCHEMA_FINGERPRINT: u64 = 0xF159E1E9A0866168;
        const IS_KEYED: bool = false;
    }

    #[derive(Clone, Debug, PartialEq, Eq)]
    struct IndexedRecord {
        key: u64,
        value: u64,
    }

    impl VarveEncode for IndexedRecord {
        const WIRE_TYPE: WireType = <(u64, u64) as VarveEncode>::WIRE_TYPE;

        fn encode_varve(&self, encoder: &mut Encoder) -> VarveResult<()> {
            (self.key, self.value).encode_varve(encoder)
        }
    }

    impl VarveDecode for IndexedRecord {
        const WIRE_TYPE: WireType = <(u64, u64) as VarveDecode>::WIRE_TYPE;

        fn decode_varve(decoder: &mut Decoder<'_>) -> VarveResult<Self> {
            let (key, value) = <(u64, u64)>::decode_varve(decoder)?;
            Ok(Self { key, value })
        }
    }

    impl VarveBlock for IndexedRecord {
        const ID: u32 = 42;
        const VERSION: u16 = 1;
        const KIND: BlockKind = BlockKind::Fixed;
        const ENDIAN: Option<Endian> = None;
        const SCHEMA_FINGERPRINT: u64 = 0x0F11DD4FBC969473;
        const IS_KEYED: bool = true;
    }

    impl VarveKeyedBlock for IndexedRecord {
        type Key = u64;

        fn key(&self) -> Self::Key {
            self.key
        }
    }

    static STREAM_BLOCKS: &[BlockDescriptor] = &[BlockDescriptor {
        id: StreamRecord::ID,
        name: "ScalableCrashStreamRecord",
        version: StreamRecord::VERSION,
        kind: StreamRecord::KIND,
        fields: &[],
    }];

    static INDEXED_BLOCKS: &[BlockDescriptor] = &[BlockDescriptor {
        id: IndexedRecord::ID,
        name: "ScalableCrashIndexedRecord",
        version: IndexedRecord::VERSION,
        kind: IndexedRecord::KIND,
        fields: &[],
    }];

    #[test]
    fn scalable_crash_fault_matrix() {
        let root_dir = unique_root("matrix");
        let root = root_dir.path().to_path_buf();
        let mut discovered = Vec::new();
        let mut catalog = BTreeSet::new();

        for scenario in SCENARIOS.iter().copied() {
            let run = root.join(format!("trace-{}", scenario.name()));
            let trace = run.join("trace.tsv");
            fs::create_dir_all(&run).expect("create trace scenario root");
            let status = run_child(scenario, &run, "trace", &trace);
            assert!(
                status.success(),
                "trace child {scenario:?} failed: {status}"
            );
            let events = parse_trace(&trace);
            assert_complete_before_after_pairs(scenario, &events);
            for event in events {
                catalog.insert(event.point.clone());
                discovered.push((scenario, event));
            }
        }

        let required: BTreeSet<_> = REQUIRED_POINTS
            .iter()
            .map(|point| (*point).to_owned())
            .collect();
        assert_eq!(
            catalog, required,
            "fault hooks are not fully wired; the trace catalog must exactly match REQUIRED_POINTS"
        );
        assert!(
            !discovered.is_empty(),
            "fault trace contained no wired points"
        );

        for (index, (scenario, event)) in discovered.into_iter().enumerate() {
            let run = root.join(format!(
                "abort-{index:04}-{}-{}-{}",
                scenario.name(),
                event.point,
                event.occurrence
            ));
            let trace = run.join("trace.tsv");
            fs::create_dir_all(&run).expect("create abort scenario root");
            let selection = format!("abort:{}:{}", event.point, event.occurrence);
            let status = run_child(scenario, &run, &selection, &trace);
            assert!(
                !status.success(),
                "selected fault did not abort: {scenario:?} {event:?}"
            );
            assert!(
                run.join(READY_FILE).is_file(),
                "child aborted before explicit arming: {scenario:?} {event:?}"
            );
            assert!(
                parse_trace(&trace).contains(&event),
                "aborting trace did not reach its selected event: {scenario:?} {event:?}"
            );
            assert_boundary_oracle(scenario, &run, &event);
        }

        root_dir.close().expect("remove crash matrix root");
    }

    #[test]
    fn scalable_fault_environment_is_inert_until_armed() {
        let root_dir = unique_root("inert");
        let trace = root_dir.path().join("trace.tsv");
        let status = Command::new(env::current_exe().expect("locate integration test executable"))
            .args([
                "--ignored",
                "--exact",
                "enabled::scalable_fault_inert_child",
            ])
            .env(CHILD_ENV, "1")
            .env(FAULT_ENV, "abort:generation.commit:1")
            .env(TRACE_ENV, &trace)
            .status()
            .expect("run inert child");
        assert!(
            status.success(),
            "environment alone activated a fault: {status}"
        );
        assert!(!trace.exists(), "an unarmed hook created a trace file");
        root_dir.close().expect("remove inert test root");
    }

    #[test]
    #[ignore = "invoked only as a scalable crash-test subprocess"]
    fn scalable_fault_child() {
        if env::var_os(CHILD_ENV).is_none() {
            return;
        }
        suppress_interactive_fault_reporting();
        let scenario = Scenario::parse(&env::var(SCENARIO_ENV).expect("child scenario"));
        let root = PathBuf::from(env::var_os(ROOT_ENV).expect("child root"));
        run_scenario(scenario, &root);
    }

    #[test]
    #[ignore = "invoked only to prove environment variables do not arm hooks"]
    fn scalable_fault_inert_child() {
        if env::var_os(CHILD_ENV).is_none() {
            return;
        }
        suppress_interactive_fault_reporting();
        fault_point("generation.commit");
    }

    /// Keeps a deliberately aborting child process non-interactive.
    ///
    /// The crash matrix aborts one child per traced fault boundary. Without
    /// this, Windows starts `WerFault.exe` for every one of them, which
    /// dominates the suite wall time and can raise error dialogs on a
    /// developer machine. Suppression must happen inside the child, before it
    /// induces the fault, because the abort is `std::process::abort` in the
    /// injected fault point itself.
    #[cfg(windows)]
    fn suppress_interactive_fault_reporting() {
        const SEM_FAILCRITICALERRORS: u32 = 0x0001;
        const SEM_NOGPFAULTERRORBOX: u32 = 0x0002;
        const SEM_NOOPENFILEERRORBOX: u32 = 0x8000;
        const WER_FAULT_REPORTING_FLAG_NOHEAP: u32 = 0x0001;
        const WER_FAULT_REPORTING_NO_UI: u32 = 0x0020;
        const WER_FAULT_REPORTING_FLAG_DISABLE_SNAPSHOT_CRASH: u32 = 0x0040;

        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn SetErrorMode(mode: u32) -> u32;
            fn SetThreadErrorMode(mode: u32, old_mode: *mut u32) -> i32;
            fn WerSetFlags(flags: u32) -> i32;
        }

        let mode = SEM_FAILCRITICALERRORS | SEM_NOGPFAULTERRORBOX | SEM_NOOPENFILEERRORBOX;
        let mut previous_thread_mode = 0_u32;
        // SAFETY: both error-mode setters take documented flag bit sets, and
        // the out parameter points at a live local. `WerSetFlags` only takes a
        // documented flag bit set. All three are process/thread-local settings
        // with no memory effects.
        unsafe {
            SetErrorMode(mode);
            SetThreadErrorMode(mode, &raw mut previous_thread_mode);
            WerSetFlags(
                WER_FAULT_REPORTING_NO_UI
                    | WER_FAULT_REPORTING_FLAG_NOHEAP
                    | WER_FAULT_REPORTING_FLAG_DISABLE_SNAPSHOT_CRASH,
            );
        }
    }

    #[cfg(not(windows))]
    fn suppress_interactive_fault_reporting() {}

    fn run_child(scenario: Scenario, root: &Path, selection: &str, trace: &Path) -> ExitStatus {
        Command::new(env::current_exe().expect("locate integration test executable"))
            .args(["--ignored", "--exact", "enabled::scalable_fault_child"])
            .env(CHILD_ENV, "1")
            .env(SCENARIO_ENV, scenario.name())
            .env(ROOT_ENV, root)
            .env(FAULT_ENV, selection)
            .env(TRACE_ENV, trace)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap_or_else(|error| panic!("run {scenario:?} child: {error}"))
    }

    fn run_scenario(scenario: Scenario, root: &Path) {
        match scenario {
            Scenario::CreateStream => {
                let _fault_guard = mark_ready_and_arm(root);
                write_stream_generation(root, false);
            }
            Scenario::CreateIndexed => {
                let _fault_guard = mark_ready_and_arm(root);
                write_indexed_generation(root, false);
            }
            Scenario::AppendStream => {
                write_stream_base(root);
                let _fault_guard = mark_ready_and_arm(root);
                append_stream_generation(root);
            }
            Scenario::AppendIndexed => {
                write_indexed_base(root);
                let _fault_guard = mark_ready_and_arm(root);
                append_indexed_generation(root);
            }
            Scenario::RestoreStream => {
                write_dirty_stream(root);
                let _fault_guard = mark_ready_and_arm(root);
                let writer = VarveStreamWriter::restore_checkpoint_and_open(
                    stream_spec(),
                    native_path(root),
                    stream_options(),
                )
                .expect("restore stream checkpoint");
                drop(writer);
            }
            Scenario::RestoreIndexed => {
                write_dirty_indexed(root);
                let _fault_guard = mark_ready_and_arm(root);
                let writer = VarveIndexedWriter::restore_checkpoint_and_open(
                    indexed_spec(),
                    native_path(root),
                    disk_options(),
                    index_plan(),
                )
                .expect("restore indexed checkpoint");
                drop(writer);
            }
            Scenario::BootstrapStream => {
                write_stream_generation(root, false);
                fs::remove_file(stream_sidecar_path(root))
                    .expect("remove bootstrap source sidecar");
                write_base_eof(root);
                let _fault_guard = mark_ready_and_arm(root);
                bootstrap_stream_checkpoint(stream_spec(), native_path(root), stream_options())
                    .expect("bootstrap stream sidecar");
            }
            Scenario::ReplaceIndexed => {
                write_indexed_generation(root, true);
                let _fault_guard = mark_ready_and_arm(root);
                rebuild_disk_index(
                    indexed_spec(),
                    native_path(root),
                    disk_options(),
                    index_plan(),
                )
                .expect("rebuild indexed sidecar");
            }
        }
    }

    fn mark_ready_and_arm(root: &Path) -> ArmGuard {
        fs::write(root.join(READY_FILE), b"ready\n").expect("write ready marker");
        arm_from_env().expect("explicitly arm scalable faults")
    }

    fn write_stream_base(root: &Path) {
        let path = native_path(root);
        let mut writer = VarveStreamWriter::create(stream_spec(), &path, stream_options())
            .expect("create stream base");
        for value in base_stream_values() {
            writer.push_info(&value).expect("append stream base value");
        }
        writer.sync().expect("sync stream base");
        drop(writer);
        write_base_eof(root);
    }

    fn write_stream_generation(root: &Path, include_base: bool) {
        let path = native_path(root);
        let mut writer = VarveStreamWriter::create(stream_spec(), &path, stream_options())
            .expect("create stream fixture");
        let values = if include_base {
            base_stream_values()
        } else {
            full_stream_values()
        };
        writer
            .push_iter::<StreamRecord, _>(values, batch_options())
            .map_err(|error| error.source)
            .expect("append deterministic stream chunks");
        writer.sync().expect("sync stream fixture");
    }

    fn append_stream_generation(root: &Path) {
        let path = native_path(root);
        let mut writer = VarveStreamWriter::open(stream_spec(), &path, stream_options())
            .expect("open stream base");
        writer
            .push_iter::<StreamRecord, _>(new_stream_values(), batch_options())
            .map_err(|error| error.source)
            .expect("append deterministic stream chunks");
        writer.sync().expect("publish stream generation");
    }

    fn write_dirty_stream(root: &Path) {
        write_stream_base(root);
        let path = native_path(root);
        let mut writer = VarveStreamWriter::open(stream_spec(), &path, stream_options())
            .expect("open stream for dirty tail");
        writer
            .push_iter::<StreamRecord, _>(new_stream_values(), batch_options())
            .map_err(|error| error.source)
            .expect("append dirty stream tail");
    }

    fn write_indexed_base(root: &Path) {
        write_indexed_generation(root, true);
        write_base_eof(root);
    }

    fn write_indexed_generation(root: &Path, include_base: bool) {
        let path = native_path(root);
        let mut writer =
            VarveIndexedWriter::create(indexed_spec(), &path, disk_options(), index_plan())
                .expect("create indexed fixture");
        let values = if include_base {
            base_indexed_values()
        } else {
            full_indexed_values()
        };
        writer
            .push_iter::<IndexedRecord, _>(values, batch_options())
            .map_err(|error| error.source)
            .expect("append deterministic indexed chunks");
        writer.sync().expect("sync indexed fixture");
    }

    fn append_indexed_generation(root: &Path) {
        let path = native_path(root);
        let mut writer =
            VarveIndexedWriter::open(indexed_spec(), &path, disk_options(), index_plan())
                .expect("open indexed base");
        writer
            .push_iter::<IndexedRecord, _>(new_indexed_values(), batch_options())
            .map_err(|error| error.source)
            .expect("append deterministic indexed chunks");
        writer.sync().expect("publish indexed generation");
    }

    fn write_dirty_indexed(root: &Path) {
        write_indexed_base(root);
        let path = native_path(root);
        let mut writer =
            VarveIndexedWriter::open(indexed_spec(), &path, disk_options(), index_plan())
                .expect("open indexed fixture for dirty tail");
        writer
            .push_iter::<IndexedRecord, _>(new_indexed_values(), batch_options())
            .map_err(|error| error.source)
            .expect("append dirty indexed tail");
    }

    fn assert_boundary_oracle(scenario: Scenario, root: &Path, event: &TraceEvent) {
        clear_stale_writer_lock(
            native_path(root),
            WriterLockBreakPolicy::BreakIfProcessAbsent,
        )
        .unwrap_or_else(|error| {
            panic!("explicit stale-lock recovery failed at {scenario:?} {event:?}: {error}")
        });
        match scenario {
            Scenario::CreateStream => assert_interrupted_stream_creation(root, event),
            Scenario::CreateIndexed => assert_interrupted_indexed_creation(root, event),
            Scenario::AppendStream => assert_stream_append_outcome(root, event),
            Scenario::AppendIndexed => assert_indexed_append_outcome(root, event),
            Scenario::RestoreStream => assert_stream_restore_outcome(root, event),
            Scenario::RestoreIndexed => assert_indexed_restore_outcome(root, event),
            Scenario::BootstrapStream => assert_stream_bootstrap_outcome(root, event),
            Scenario::ReplaceIndexed => assert_indexed_replacement_outcome(root, event),
        }
    }

    fn assert_interrupted_stream_creation(root: &Path, event: &TraceEvent) {
        if let Ok(values) = read_stream(root) {
            assert!(
                values.is_empty() || values == full_stream_values(),
                "partial clean stream at {event:?}: {values:?}"
            );
        }
    }

    fn assert_interrupted_indexed_creation(root: &Path, event: &TraceEvent) {
        if let Ok(values) = read_indexed(root, 0..7) {
            assert!(
                values.is_empty() || values == full_indexed_values(),
                "partial clean index at {event:?}: {values:?}"
            );
        }
    }

    fn assert_stream_append_outcome(root: &Path, event: &TraceEvent) {
        let base_eof = read_base_eof(root);
        match read_stream(root) {
            Ok(values) => {
                assert!(
                    values == base_stream_values() || values == full_stream_values(),
                    "clean stream is neither old nor complete at {event:?}: {values:?}"
                );
                if values == base_stream_values() {
                    assert_eq!(
                        fs::metadata(native_path(root)).unwrap().len(),
                        base_eof,
                        "unchanged clean stream retained a native append at {event:?}"
                    );
                }
            }
            Err(_) => {
                let physical = fs::metadata(native_path(root))
                    .expect("dirty stream native")
                    .len();
                assert!(
                    physical >= base_eof,
                    "dirty stream shrank below base EOF at {event:?}"
                );
                let writer = VarveStreamWriter::restore_checkpoint_and_open(
                    stream_spec(),
                    native_path(root),
                    stream_options(),
                )
                .unwrap_or_else(|error| {
                    panic!("dirty stream did not restore at {event:?}: {error}")
                });
                drop(writer);
                assert_eq!(fs::metadata(native_path(root)).unwrap().len(), base_eof);
                assert_eq!(read_stream(root).unwrap(), base_stream_values());
            }
        }
    }

    fn assert_indexed_append_outcome(root: &Path, event: &TraceEvent) {
        let base_eof = read_base_eof(root);
        match read_indexed(root, 0..7) {
            Ok(values) => {
                assert!(
                    values == base_indexed_values() || values == full_indexed_values(),
                    "clean index is neither old nor complete at {event:?}: {values:?}"
                );
                if values == base_indexed_values() {
                    assert_eq!(
                        fs::metadata(native_path(root)).unwrap().len(),
                        base_eof,
                        "unchanged clean index retained a native append at {event:?}"
                    );
                }
            }
            Err(_) => {
                let physical = fs::metadata(native_path(root))
                    .expect("dirty indexed native")
                    .len();
                assert!(
                    physical >= base_eof,
                    "dirty index shrank below base EOF at {event:?}"
                );
                let writer = VarveIndexedWriter::restore_checkpoint_and_open(
                    indexed_spec(),
                    native_path(root),
                    disk_options(),
                    index_plan(),
                )
                .unwrap_or_else(|error| {
                    panic!("dirty index did not restore at {event:?}: {error}")
                });
                drop(writer);
                assert_eq!(fs::metadata(native_path(root)).unwrap().len(), base_eof);
                assert_eq!(read_indexed(root, 0..2).unwrap(), base_indexed_values());
            }
        }
    }

    fn assert_stream_restore_outcome(root: &Path, event: &TraceEvent) {
        if read_stream(root).is_err() {
            assert!(
                fs::metadata(native_path(root)).unwrap().len() >= read_base_eof(root),
                "interrupted restore truncated below its base EOF at {event:?}"
            );
            let writer = VarveStreamWriter::restore_checkpoint_and_open(
                stream_spec(),
                native_path(root),
                stream_options(),
            )
            .unwrap_or_else(|error| panic!("repeated stream restore failed at {event:?}: {error}"));
            drop(writer);
        }
        assert_eq!(
            fs::metadata(native_path(root)).unwrap().len(),
            read_base_eof(root)
        );
        assert_eq!(read_stream(root).unwrap(), base_stream_values());
    }

    fn assert_indexed_restore_outcome(root: &Path, event: &TraceEvent) {
        if read_indexed(root, 0..2).is_err() {
            assert!(
                fs::metadata(native_path(root)).unwrap().len() >= read_base_eof(root),
                "interrupted restore truncated below its base EOF at {event:?}"
            );
            let writer = VarveIndexedWriter::restore_checkpoint_and_open(
                indexed_spec(),
                native_path(root),
                disk_options(),
                index_plan(),
            )
            .unwrap_or_else(|error| {
                panic!("repeated indexed restore failed at {event:?}: {error}")
            });
            drop(writer);
        }
        assert_eq!(
            fs::metadata(native_path(root)).unwrap().len(),
            read_base_eof(root)
        );
        assert_eq!(read_indexed(root, 0..2).unwrap(), base_indexed_values());
    }

    fn assert_stream_bootstrap_outcome(root: &Path, event: &TraceEvent) {
        assert_eq!(
            fs::metadata(native_path(root)).unwrap().len(),
            read_base_eof(root),
            "bootstrap changed the native file at {event:?}"
        );
        if stream_sidecar_path(root).exists() {
            assert_eq!(
                read_stream(root).unwrap_or_else(|error| {
                    panic!("bootstrap published an invalid sidecar at {event:?}: {error}")
                }),
                full_stream_values()
            );
        }
    }

    fn assert_indexed_replacement_outcome(root: &Path, event: &TraceEvent) {
        assert_eq!(
            read_indexed(root, 0..2).unwrap_or_else(|error| panic!(
                "replacement left invalid target at {event:?}: {error}"
            )),
            base_indexed_values()
        );
    }

    fn parse_trace(path: &Path) -> Vec<TraceEvent> {
        let contents = fs::read_to_string(path)
            .unwrap_or_else(|error| panic!("read fault trace {}: {error}", path.display()));
        let required: BTreeSet<_> = REQUIRED_POINTS.iter().copied().collect();
        let mut next = BTreeMap::<String, u64>::new();
        let mut events = Vec::new();
        for (line_number, line) in contents.lines().enumerate() {
            let fields: Vec<_> = line.split('\t').collect();
            assert_eq!(
                fields.len(),
                3,
                "malformed trace line {}: {line:?}",
                line_number + 1
            );
            assert_eq!(
                fields[0],
                "v1",
                "unknown trace version on line {}",
                line_number + 1
            );
            assert!(
                required.contains(fields[1]),
                "unknown point on line {}: {line:?}",
                line_number + 1
            );
            let occurrence = fields[2].parse::<u64>().unwrap_or_else(|_| {
                panic!("invalid occurrence on line {}: {line:?}", line_number + 1)
            });
            let expected = next.entry(fields[1].to_owned()).or_insert(1);
            assert_eq!(
                occurrence,
                *expected,
                "non-consecutive occurrence on line {}",
                line_number + 1
            );
            *expected += 1;
            events.push(TraceEvent {
                point: fields[1].to_owned(),
                occurrence,
            });
        }
        events
    }

    fn assert_complete_before_after_pairs(scenario: Scenario, events: &[TraceEvent]) {
        let mut counts = BTreeMap::<&str, u64>::new();
        for event in events {
            counts.insert(&event.point, event.occurrence);
        }
        for (point, count) in counts {
            assert_eq!(
                count % 2,
                0,
                "trace scenario {scenario:?} did not record both sides of the last {point:?} primitive"
            );
        }
    }

    fn read_stream(root: &Path) -> VarveResult<Vec<StreamRecord>> {
        VarveStreamReader::open(stream_spec(), native_path(root), stream_options())?
            .blocks::<StreamRecord>()?
            .collect()
    }

    fn read_indexed(
        root: &Path,
        keys: impl IntoIterator<Item = u64>,
    ) -> VarveResult<Vec<IndexedRecord>> {
        let reader = VarveIndexedReader::open(
            indexed_spec(),
            native_path(root),
            disk_options(),
            index_plan(),
        )?;
        let mut values = Vec::new();
        for key in keys {
            if let Some(value) = reader.get::<IndexedRecord>(&key)? {
                values.push(value);
            }
        }
        Ok(values)
    }

    fn stream_spec() -> FormatSpec {
        FormatSpec::new(
            b"VFSTRM",
            1,
            Endian::Little,
            0,
            IndexPolicy::BlockOffsetChain,
            IntegrityPolicy::None,
            RecoveryPolicy::Strict,
            ManifestPolicy::None,
            STREAM_BLOCKS,
        )
        .with_read_limits(ReadLimits::STANDARD)
    }

    fn indexed_spec() -> FormatSpec {
        FormatSpec::new(
            b"VFINDX",
            1,
            Endian::Little,
            0,
            IndexPolicy::KeyedOffsetChain,
            IntegrityPolicy::None,
            RecoveryPolicy::Strict,
            ManifestPolicy::None,
            INDEXED_BLOCKS,
        )
        .with_read_limits(ReadLimits::STANDARD)
    }

    fn index_plan() -> DiskIndexPlan {
        const INDEXED: &[DiskIndexedBlock] = &[DiskIndexedBlock::of::<IndexedRecord>()];
        DiskIndexPlan::canonical(indexed_spec(), INDEXED).expect("canonical crash-test index plan")
    }

    fn disk_options() -> DiskIndexOptions {
        DiskIndexOptions {
            batch: DiskIndexBatchOptions {
                max_records: 2,
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

    const fn batch_options() -> BatchOptions {
        BatchOptions {
            max_records: 2,
            max_bytes: usize::MAX,
        }
    }

    fn base_stream_values() -> Vec<StreamRecord> {
        vec![StreamRecord(10), StreamRecord(20)]
    }

    fn new_stream_values() -> Vec<StreamRecord> {
        (100..105).map(StreamRecord).collect()
    }

    fn full_stream_values() -> Vec<StreamRecord> {
        base_stream_values()
            .into_iter()
            .chain(new_stream_values())
            .collect()
    }

    fn base_indexed_values() -> Vec<IndexedRecord> {
        (0..2).map(indexed_value).collect()
    }

    fn new_indexed_values() -> Vec<IndexedRecord> {
        (2..7).map(indexed_value).collect()
    }

    fn full_indexed_values() -> Vec<IndexedRecord> {
        (0..7).map(indexed_value).collect()
    }

    const fn indexed_value(key: u64) -> IndexedRecord {
        IndexedRecord {
            key,
            value: 10_000 + key * 17,
        }
    }

    fn native_path(root: &Path) -> PathBuf {
        root.join("fixture.varve")
    }

    fn stream_sidecar_path(root: &Path) -> PathBuf {
        let mut path = native_path(root).into_os_string();
        path.push(".vks");
        path.into()
    }

    fn write_base_eof(root: &Path) {
        let eof = fs::metadata(native_path(root))
            .expect("base native metadata")
            .len();
        fs::write(root.join(BASE_EOF_FILE), eof.to_string()).expect("write base EOF");
    }

    fn read_base_eof(root: &Path) -> u64 {
        fs::read_to_string(root.join(BASE_EOF_FILE))
            .expect("read base EOF")
            .parse()
            .expect("parse base EOF")
    }

    fn unique_root(label: &str) -> tempfile::TempDir {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock before epoch")
            .as_nanos();
        tempfile::Builder::new()
            .prefix(&format!(
                "varve-scalable-fault-{label}-{}-{nonce}-",
                std::process::id()
            ))
            .tempdir()
            .expect("create crash test root")
    }
}
