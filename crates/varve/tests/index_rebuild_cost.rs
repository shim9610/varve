#![cfg(feature = "high-cardinality-dev")]

//! Rebuild cost contracts (PERF-04/PERF-05, PERF2-03, DUR-03, DUR2-04,
//! DUR2-01).
//!
//! Rebuild resolves each record's descriptor once by block id and decodes each
//! tombstone key exactly once, so rebuild cost must not scale with the plan's
//! descriptor count. The strict per-record decode counters live in the
//! varve-core unit tests; this file pins the end-to-end contracts: a wide plan
//! rebuilds a tombstone-heavy file in roughly the time of a single-descriptor
//! plan, the published `historical_distinct_keys` metric follows K-ever,
//! rebuild refuses to publish a sidecar when the pathname identity changes
//! after the scan (DUR-03), a CRC rebuild traverses each indexed payload once
//! (PERF2-03), sidecar publication surfaces the pending parent-directory
//! durability state instead of discarding it (DUR2-04), and an indeterminate
//! sidecar publication preserves the replacement temp for reconciliation
//! instead of blind-deleting it (DUR2-01).

use std::fs;
use std::sync::{RwLock, RwLockReadGuard, RwLockWriteGuard};
use std::time::{Duration, Instant};

use varve::{
    BlockDescriptor, BlockKind, Decoder, DiskIndexError, DiskIndexOptions, DiskIndexPlan,
    DiskIndexedBlock, Encoder, Endian, Error, FormatSpec, IndexPolicy, IntegrityPolicy,
    ManifestPolicy, ReadLimits, RecoveryPolicy, Result, ScanOptions, ScanProgressPhase, VarveBlock,
    VarveDecode, VarveEncode, VarveIndexedReader, VarveIndexedWriter, VarveKeyedBlock, WireType,
    disk_index_sidecar_path, rebuild_disk_index, rebuild_disk_index_with_progress,
};

/// The parent-sync fault-injection counter is process-global, so any test in
/// this binary that publishes a sidecar could consume an armed failure meant
/// for a `publication_state` test (or trip over one). Regular tests share the
/// gate; tests that arm injected failures take it exclusively.
static FAULT_ISOLATION: RwLock<()> = RwLock::new(());

fn shared_fault_gate() -> RwLockReadGuard<'static, ()> {
    FAULT_ISOLATION
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg_attr(not(feature = "scalable-fault-injection"), allow(dead_code))]
fn exclusive_fault_gate() -> RwLockWriteGuard<'static, ()> {
    FAULT_ISOLATION
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Target(String);

impl VarveEncode for Target {
    const WIRE_TYPE: WireType = <String as VarveEncode>::WIRE_TYPE;

    fn encode_varve(&self, encoder: &mut Encoder) -> Result<()> {
        self.0.encode_varve(encoder)
    }
}

impl VarveDecode for Target {
    const WIRE_TYPE: WireType = <String as VarveDecode>::WIRE_TYPE;

    fn decode_varve(decoder: &mut Decoder<'_>) -> Result<Self> {
        Ok(Self(String::decode_varve(decoder)?))
    }
}

impl VarveBlock for Target {
    const ID: u32 = 1;
    const VERSION: u16 = 1;
    const KIND: BlockKind = BlockKind::Variable;
    const ENDIAN: Option<Endian> = None;
    const SCHEMA_FINGERPRINT: u64 = 0x5245_4255_494C_4401;
    const IS_KEYED: bool = true;
}

impl VarveKeyedBlock for Target {
    type Key = String;

    fn key(&self) -> String {
        self.0.clone()
    }
}

/// Filler blocks widen the disk-index plan without ever receiving records:
/// their descriptors exist only so a rebuild that re-decoded tombstone keys
/// per descriptor would multiply its payload work by the plan width.
macro_rules! filler_blocks {
    ($($name:ident = $id:literal),+ $(,)?) => {
        $(
            #[derive(Clone, Debug, PartialEq, Eq)]
            struct $name(String);

            impl VarveEncode for $name {
                const WIRE_TYPE: WireType = <String as VarveEncode>::WIRE_TYPE;

                fn encode_varve(&self, encoder: &mut Encoder) -> Result<()> {
                    self.0.encode_varve(encoder)
                }
            }

            impl VarveDecode for $name {
                const WIRE_TYPE: WireType = <String as VarveDecode>::WIRE_TYPE;

                fn decode_varve(decoder: &mut Decoder<'_>) -> Result<Self> {
                    Ok(Self(String::decode_varve(decoder)?))
                }
            }

            impl VarveBlock for $name {
                const ID: u32 = $id;
                const VERSION: u16 = 1;
                const KIND: BlockKind = BlockKind::Variable;
                const ENDIAN: Option<Endian> = None;
                const SCHEMA_FINGERPRINT: u64 = 0x5245_4255_494C_4400 ^ ($id as u64);
                const IS_KEYED: bool = true;
            }

            impl VarveKeyedBlock for $name {
                type Key = String;

                fn key(&self) -> String {
                    self.0.clone()
                }
            }
        )+

        static SPEC_BLOCKS: &[BlockDescriptor] = &[
            BlockDescriptor {
                id: Target::ID,
                name: "RebuildTarget",
                version: Target::VERSION,
                kind: Target::KIND,
                fields: &[],
            },
            $(
                BlockDescriptor {
                    id: $name::ID,
                    name: stringify!($name),
                    version: $name::VERSION,
                    kind: $name::KIND,
                    fields: &[],
                },
            )+
        ];

        static FULL_INDEXED: &[DiskIndexedBlock] = &[
            DiskIndexedBlock::of::<Target>(),
            $(DiskIndexedBlock::of::<$name>(),)+
        ];
    };
}

filler_blocks!(
    F02 = 2,
    F03 = 3,
    F04 = 4,
    F05 = 5,
    F06 = 6,
    F07 = 7,
    F08 = 8,
    F09 = 9,
    F10 = 10,
    F11 = 11,
    F12 = 12,
    F13 = 13,
    F14 = 14,
    F15 = 15,
    F16 = 16,
    F17 = 17,
    F18 = 18,
    F19 = 19,
    F20 = 20,
    F21 = 21,
    F22 = 22,
    F23 = 23,
    F24 = 24,
    F25 = 25,
    F26 = 26,
    F27 = 27,
    F28 = 28,
    F29 = 29,
    F30 = 30,
    F31 = 31,
    F32 = 32,
    F33 = 33,
    F34 = 34,
    F35 = 35,
    F36 = 36,
    F37 = 37,
    F38 = 38,
    F39 = 39,
    F40 = 40,
    F41 = 41,
    F42 = 42,
    F43 = 43,
    F44 = 44,
    F45 = 45,
    F46 = 46,
    F47 = 47,
    F48 = 48,
    F49 = 49,
    F50 = 50,
    F51 = 51,
    F52 = 52,
    F53 = 53,
    F54 = 54,
    F55 = 55,
    F56 = 56,
    F57 = 57,
    F58 = 58,
    F59 = 59,
    F60 = 60,
    F61 = 61,
    F62 = 62,
    F63 = 63,
    F64 = 64,
    F65 = 65,
);

static SMALL_INDEXED: &[DiskIndexedBlock] = &[DiskIndexedBlock::of::<Target>()];

fn spec() -> FormatSpec {
    FormatSpec::new(
        b"VRBC",
        1,
        Endian::Little,
        0,
        IndexPolicy::KeyedOffsetChain,
        IntegrityPolicy::None,
        RecoveryPolicy::Strict,
        ManifestPolicy::None,
        SPEC_BLOCKS,
    )
    .with_read_limits(ReadLimits::STANDARD)
}

fn small_plan() -> DiskIndexPlan {
    DiskIndexPlan::canonical(spec(), SMALL_INDEXED).unwrap()
}

fn full_plan() -> DiskIndexPlan {
    DiskIndexPlan::canonical(spec(), FULL_INDEXED).unwrap()
}

const TOMBSTONES: u32 = 128;
const PUTS: u32 = 4;
const KEY_LEN: usize = 96 * 1024;

fn big_key(ordinal: u32) -> String {
    let mut key = String::with_capacity(KEY_LEN);
    key.push_str(&format!("{ordinal:08}"));
    while key.len() < KEY_LEN {
        key.push('k');
    }
    key
}

#[test]
fn rebuild_cost_is_independent_of_plan_descriptor_count() -> Result<()> {
    let _isolation = shared_fault_gate();
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("plan-width.varve");
    let options = DiskIndexOptions::default();

    {
        let mut writer = VarveIndexedWriter::create(spec(), &path, options, small_plan())?;
        for ordinal in 0..PUTS {
            writer.push_info(&Target(big_key(1_000_000 + ordinal)))?;
        }
        for ordinal in 0..TOMBSTONES {
            writer.delete_info::<Target>(&big_key(ordinal))?;
        }
        writer.sync()?;
    }
    let records = u64::from(PUTS + TOMBSTONES);
    // A rebuild also scans the internal creation-nonce record every
    // stream/indexed primary carries as its first record (STO-01).
    let scanned_records = records + 1;

    fs::remove_file(disk_index_sidecar_path(&path))?;
    let small_started = Instant::now();
    let small_report = rebuild_disk_index(spec(), &path, options, small_plan())?;
    let small_elapsed = small_started.elapsed();
    assert_eq!(small_report.records, scanned_records);
    {
        let reader = VarveIndexedReader::open(spec(), &path, options, small_plan())?;
        assert_eq!(reader.get::<Target>(&big_key(0))?, None);
        assert_eq!(
            reader.get::<Target>(&big_key(1_000_000))?,
            Some(Target(big_key(1_000_000)))
        );
        assert_eq!(reader.historical_distinct_keys()?, records);
    }

    fs::remove_file(disk_index_sidecar_path(&path))?;
    let full_started = Instant::now();
    let full_report = rebuild_disk_index(spec(), &path, options, full_plan())?;
    let full_elapsed = full_started.elapsed();
    assert_eq!(full_report.records, scanned_records);
    assert_eq!(full_report.scanned_bytes, small_report.scanned_bytes);
    {
        let reader = VarveIndexedReader::open(spec(), &path, options, full_plan())?;
        assert_eq!(reader.get::<Target>(&big_key(TOMBSTONES - 1))?, None);
        assert_eq!(
            reader.get::<Target>(&big_key(1_000_000 + PUTS - 1))?,
            Some(Target(big_key(1_000_000 + PUTS - 1)))
        );
        assert_eq!(reader.historical_distinct_keys()?, records);
    }

    eprintln!(
        "rebuild plan-width cost: 1 descriptor {small_elapsed:?}, \
         {} descriptors {full_elapsed:?} over {records} records",
        FULL_INDEXED.len(),
    );
    // The 65-descriptor rebuild covers the same records and must not
    // re-decode each 96 KiB tombstone key per descriptor. The bound is
    // deliberately loose against scheduler noise (the second rebuild also
    // runs page-cache warm); descriptor-proportional payload work would
    // exceed it by an order of magnitude.
    let bound = small_elapsed * 3 + Duration::from_millis(750);
    assert!(
        full_elapsed <= bound,
        "wide-plan rebuild took {full_elapsed:?}, exceeding {bound:?} \
         (single-descriptor rebuild took {small_elapsed:?})"
    );
    Ok(())
}

#[test]
fn historical_distinct_keys_follows_k_ever_across_rebuild() -> Result<()> {
    let _isolation = shared_fault_gate();
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("k-ever.varve");
    let options = DiskIndexOptions::default();

    let mut writer = VarveIndexedWriter::create(spec(), &path, options, small_plan())?;
    for ordinal in 0..32u32 {
        writer.push_info(&Target(format!("key-{ordinal}")))?;
    }
    for ordinal in 0..32u32 {
        writer.delete_info::<Target>(&format!("key-{ordinal}"))?;
    }
    writer.sync()?;
    // Zero live keys, yet capacity follows the historical distinct keys:
    // tombstones replace latest rows without deleting them.
    assert_eq!(writer.historical_distinct_keys()?, 32);
    drop(writer);

    let reader = VarveIndexedReader::open(spec(), &path, options, small_plan())?;
    assert_eq!(reader.historical_distinct_keys()?, 32);
    assert_eq!(reader.get::<Target>(&"key-0".to_string())?, None);
    drop(reader);

    // Rebuild re-creates tombstone rows from the native log, so the metric
    // does not shrink without a native compaction.
    fs::remove_file(disk_index_sidecar_path(&path))?;
    let report = rebuild_disk_index(spec(), &path, options, small_plan())?;
    // 64 user records plus the internal creation-nonce record (STO-01).
    assert_eq!(report.records, 65);
    let reader = VarveIndexedReader::open(spec(), &path, options, small_plan())?;
    assert_eq!(reader.historical_distinct_keys()?, 32);
    Ok(())
}

/// PERF2-03: under CRC policies the rebuild traverses each payload once. The
/// scanner reads frames only, extraction owns the single checksum-verified
/// payload read for indexed records, and payloads outside the plan are never
/// read — a corrupt unindexed payload no longer fails the rebuild (the old
/// double-traversal scanner pre-pass would have), while a corrupt indexed
/// payload is still rejected by the one verified read.
#[cfg(feature = "integrity")]
mod crc_single_traversal {
    use super::*;

    #[derive(Clone, Debug, PartialEq, Eq)]
    struct Blob(String);

    impl VarveEncode for Blob {
        const WIRE_TYPE: WireType = <String as VarveEncode>::WIRE_TYPE;

        fn encode_varve(&self, encoder: &mut Encoder) -> Result<()> {
            self.0.encode_varve(encoder)
        }
    }

    impl VarveDecode for Blob {
        const WIRE_TYPE: WireType = <String as VarveDecode>::WIRE_TYPE;

        fn decode_varve(decoder: &mut Decoder<'_>) -> Result<Self> {
            Ok(Self(String::decode_varve(decoder)?))
        }
    }

    impl VarveBlock for Blob {
        const ID: u32 = 66;
        const VERSION: u16 = 1;
        const KIND: BlockKind = BlockKind::Variable;
        const ENDIAN: Option<Endian> = None;
        const SCHEMA_FINGERPRINT: u64 = 0x424C_4F42_5242_4C42;
        const IS_KEYED: bool = false;
    }

    static CRC_BLOCKS: &[BlockDescriptor] = &[
        BlockDescriptor {
            id: Target::ID,
            name: "RebuildTarget",
            version: Target::VERSION,
            kind: Target::KIND,
            fields: &[],
        },
        BlockDescriptor {
            id: Blob::ID,
            name: "RebuildBlob",
            version: Blob::VERSION,
            kind: Blob::KIND,
            fields: &[],
        },
    ];

    static CRC_INDEXED: &[DiskIndexedBlock] = &[DiskIndexedBlock::of::<Target>()];

    fn crc_spec() -> FormatSpec {
        FormatSpec::new(
            b"VRBI",
            1,
            Endian::Little,
            0,
            IndexPolicy::KeyedOffsetChain,
            IntegrityPolicy::Crc32WithHeader,
            RecoveryPolicy::Strict,
            ManifestPolicy::None,
            CRC_BLOCKS,
        )
        .with_read_limits(ReadLimits::STANDARD)
    }

    fn crc_plan() -> DiskIndexPlan {
        DiskIndexPlan::canonical(crc_spec(), CRC_INDEXED).unwrap()
    }

    fn flip_marker_byte(path: &std::path::Path, marker: &str) -> Result<usize> {
        let mut bytes = fs::read(path)?;
        let position = bytes
            .windows(marker.len())
            .position(|window| window == marker.as_bytes())
            .expect("marker payload must exist in the native file");
        let target = position + marker.len() / 2;
        bytes[target] ^= 0x01;
        fs::write(path, &bytes)?;
        Ok(target)
    }

    #[test]
    fn rebuild_reads_frames_only_outside_the_plan_and_verifies_indexed_payloads() -> Result<()> {
        let _isolation = shared_fault_gate();
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("crc-single-traversal.varve");
        let options = DiskIndexOptions::default();
        let indexed_marker = "indexed-target-payload-0123456789abcdef";
        let blob_marker = "unindexed-blob-payload-0123456789abcdef";

        {
            let mut writer = VarveIndexedWriter::create(crc_spec(), &path, options, crc_plan())?;
            writer.push_info(&Target(indexed_marker.into()))?;
            writer.push_unindexed_info(&Blob(blob_marker.into()))?;
            writer.delete_info::<Target>(&"absent-key".to_string())?;
            writer.sync()?;
        }

        // Baseline: the clean CRC file rebuilds.
        fs::remove_file(disk_index_sidecar_path(&path))?;
        let report = rebuild_disk_index(crc_spec(), &path, options, crc_plan())?;
        // Three user records plus the internal creation-nonce record (STO-01).
        assert_eq!(report.records, 4);

        // Corrupt the payload of the record outside the plan. A rebuild that
        // still ran the scanner's whole-payload checksum pre-pass would fail
        // here; the frame-only scan must succeed without reading the payload.
        let corrupted = flip_marker_byte(&path, blob_marker)?;
        fs::remove_file(disk_index_sidecar_path(&path))?;
        let report = rebuild_disk_index(crc_spec(), &path, options, crc_plan())?;
        // Three user records plus the internal creation-nonce record (STO-01).
        assert_eq!(report.records, 4);
        let reader = VarveIndexedReader::open(crc_spec(), &path, options, crc_plan())?;
        assert_eq!(
            reader.get::<Target>(&indexed_marker.to_string())?,
            Some(Target(indexed_marker.into()))
        );
        // The whole-file integrity scan still owns full corruption coverage.
        assert!(matches!(
            reader.verify_all(),
            Err(Error::ChecksumMismatch { .. })
        ));
        drop(reader);

        // Restore the unindexed payload, then corrupt the indexed payload:
        // the single verified extraction read must reject the rebuild and
        // publish nothing.
        let mut bytes = fs::read(&path)?;
        bytes[corrupted] ^= 0x01;
        fs::write(&path, &bytes)?;
        flip_marker_byte(&path, indexed_marker)?;
        fs::remove_file(disk_index_sidecar_path(&path))?;
        assert!(matches!(
            rebuild_disk_index(crc_spec(), &path, options, crc_plan()),
            Err(Error::ChecksumMismatch { .. })
        ));
        assert!(
            !disk_index_sidecar_path(&path).exists(),
            "no sidecar may be published after a corrupt indexed payload"
        );
        Ok(())
    }
}

/// DUR2-04: sidecar publication must not discard the replacement durability
/// state. When the pathname publication succeeded but the parent-directory
/// sync failed, the caller receives the typed
/// `PublishedButParentSyncPending` state and the already-published sidecar is
/// preserved.
///
/// DUR2-01: when publication fails in the indeterminate state (an
/// unreconciled `ReplaceFileW` 1176/1177), the replacement temp file may be
/// the only surviving copy of the new sidecar and must be preserved for
/// out-of-band reconciliation — no caller may blind-delete it, including via
/// a tempfile RAII guard dropping on the error path.
#[cfg(feature = "scalable-fault-injection")]
mod publication_state {
    use varve::{
        StreamOptions, VarveFile, VarveStreamReader, VarveStreamWriter, bootstrap_stream_checkpoint,
    };

    use super::*;

    fn state_sidecar_path(path: &std::path::Path) -> std::path::PathBuf {
        let mut sidecar = path.as_os_str().to_os_string();
        sidecar.push(".vks");
        std::path::PathBuf::from(sidecar)
    }

    #[test]
    fn rebuild_surfaces_parent_sync_pending_and_preserves_the_sidecar() -> Result<()> {
        // Held for the whole test (setup included): a publication from this
        // test's own un-gated setup — or any concurrent test's — must not
        // consume the armed injection.
        let _gate = exclusive_fault_gate();
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("rebuild-pending.varve");
        let options = DiskIndexOptions::default();
        {
            let mut writer = VarveIndexedWriter::create(spec(), &path, options, small_plan())?;
            writer.push_info(&Target("pending-key".into()))?;
            writer.sync()?;
        }

        VarveFile::inject_parent_sync_failures(1);
        match rebuild_disk_index(spec(), &path, options, small_plan()) {
            Err(Error::PublishedButParentSyncPending { .. }) => {}
            other => panic!("expected PublishedButParentSyncPending, got {other:?}"),
        }
        // Publication already happened: the rebuilt sidecar is preserved and
        // serves reads.
        assert!(disk_index_sidecar_path(&path).exists());
        let reader = VarveIndexedReader::open(spec(), &path, options, small_plan())?;
        assert_eq!(
            reader.get::<Target>(&"pending-key".to_string())?,
            Some(Target("pending-key".into()))
        );
        Ok(())
    }

    #[test]
    fn create_surfaces_parent_sync_pending_and_preserves_the_sidecar() -> Result<()> {
        let _gate = exclusive_fault_gate();
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("create-pending.varve");
        let options = DiskIndexOptions::default();

        VarveFile::inject_parent_sync_failures(1);
        match VarveIndexedWriter::create(spec(), &path, options, small_plan()) {
            Err(Error::PublishedButParentSyncPending { .. }) => {}
            other => panic!(
                "expected PublishedButParentSyncPending, got {:?}",
                other.map(|_| "writer")
            ),
        }
        // The native file and the published sidecar are both intact, so a
        // normal open resumes writing.
        assert!(disk_index_sidecar_path(&path).exists());
        let mut writer = VarveIndexedWriter::open(spec(), &path, options, small_plan())?;
        writer.push_info(&Target("after-pending".into()))?;
        writer.sync()?;
        drop(writer);
        let reader = VarveIndexedReader::open(spec(), &path, options, small_plan())?;
        assert_eq!(
            reader.get::<Target>(&"after-pending".to_string())?,
            Some(Target("after-pending".into()))
        );
        Ok(())
    }

    #[test]
    fn bootstrap_surfaces_parent_sync_pending_and_preserves_the_sidecar() -> Result<()> {
        let _gate = exclusive_fault_gate();
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("bootstrap-pending.varve");
        {
            let writer = VarveStreamWriter::create(spec(), &path, StreamOptions::default())?;
            drop(writer);
        }
        fs::remove_file(state_sidecar_path(&path))?;

        VarveFile::inject_parent_sync_failures(1);
        match bootstrap_stream_checkpoint(spec(), &path, StreamOptions::default()) {
            Err(Error::PublishedButParentSyncPending { .. }) => {}
            other => panic!("expected PublishedButParentSyncPending, got {other:?}"),
        }
        // The bootstrapped state sidecar is preserved and readable.
        assert!(state_sidecar_path(&path).exists());
        let reader = VarveStreamReader::open(spec(), &path, StreamOptions::default())?;
        drop(reader);
        Ok(())
    }

    /// Destructures an indeterminate publication error, asserting it names
    /// `expected_target` and that the replacement temp it reports survives on
    /// disk for out-of-band reconciliation (DUR2-01). Returns the preserved
    /// temp path.
    fn assert_preserved_replacement(
        error: Error,
        expected_target: &std::path::Path,
    ) -> std::path::PathBuf {
        match error {
            Error::ReplacePublicationIndeterminate {
                path, replacement, ..
            } => {
                // The error may carry the canonical (`\\?\`-prefixed) form of
                // the pathname; compare canonicalized parents plus the name.
                assert_eq!(
                    canonical_pathname(std::path::Path::new(&path)),
                    canonical_pathname(expected_target)
                );
                let temp = std::path::PathBuf::from(replacement);
                assert!(
                    temp.exists(),
                    "replacement temp {} must be preserved for reconciliation",
                    temp.display()
                );
                temp
            }
            other => panic!("expected ReplacePublicationIndeterminate, got {other:?}"),
        }
    }

    /// Canonicalizes the (existing) parent directory and rejoins the file
    /// name, so pathnames compare equal whether or not they carry the
    /// Windows verbatim prefix and whether or not the file itself exists.
    fn canonical_pathname(path: &std::path::Path) -> std::path::PathBuf {
        let parent = path.parent().expect("sidecar paths have a parent");
        let name = path.file_name().expect("sidecar paths have a name");
        fs::canonicalize(parent)
            .expect("sidecar parent directory must exist")
            .join(name)
    }

    fn temp_name_ends_with(temp: &std::path::Path, suffix: &str) -> bool {
        temp.file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.ends_with(suffix))
    }

    #[test]
    fn rebuild_preserves_the_replacement_temp_on_indeterminate_publication() -> Result<()> {
        let _gate = exclusive_fault_gate();
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("rebuild-indeterminate.varve");
        let options = DiskIndexOptions::default();
        {
            let mut writer = VarveIndexedWriter::create(spec(), &path, options, small_plan())?;
            writer.push_info(&Target("indeterminate-key".into()))?;
            writer.sync()?;
        }

        VarveFile::inject_replace_indeterminate_failures(1);
        let error = rebuild_disk_index(spec(), &path, options, small_plan())
            .expect_err("armed indeterminate publication must fail the rebuild");
        let temp = assert_preserved_replacement(error, &disk_index_sidecar_path(&path));
        assert!(
            temp_name_ends_with(&temp, ".vki.tmp"),
            "preserved temp {} must be the rebuilt sidecar replacement",
            temp.display()
        );
        // The injected failure fires before the target pathname is touched,
        // so the previous sidecar generation still serves reads.
        assert!(disk_index_sidecar_path(&path).exists());
        let reader = VarveIndexedReader::open(spec(), &path, options, small_plan())?;
        assert_eq!(
            reader.get::<Target>(&"indeterminate-key".to_string())?,
            Some(Target("indeterminate-key".into()))
        );
        Ok(())
    }

    #[test]
    fn create_preserves_the_replacement_temp_on_indeterminate_publication() -> Result<()> {
        let _gate = exclusive_fault_gate();
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("create-indeterminate.varve");
        let options = DiskIndexOptions::default();

        VarveFile::inject_replace_indeterminate_failures(1);
        let error = match VarveIndexedWriter::create(spec(), &path, options, small_plan()) {
            Err(error) => error,
            Ok(_) => panic!("armed indeterminate publication must fail sidecar creation"),
        };
        let temp = assert_preserved_replacement(error, &disk_index_sidecar_path(&path));
        assert!(
            temp_name_ends_with(&temp, ".vki.tmp"),
            "preserved temp {} must be the created sidecar replacement",
            temp.display()
        );
        // The injected failure fires before the target pathname is touched,
        // so no sidecar was published under its target name...
        assert!(!disk_index_sidecar_path(&path).exists());
        // ...and the native file remains recoverable: an unarmed rebuild
        // publishes a fresh sidecar generation alongside the preserved temp.
        let report = rebuild_disk_index(spec(), &path, options, small_plan())?;
        // No user records, but the internal creation-nonce record is there.
        assert_eq!(report.records, 1);
        assert!(disk_index_sidecar_path(&path).exists());
        assert!(
            temp.exists(),
            "recovery must not consume the preserved replacement temp"
        );
        Ok(())
    }

    #[test]
    fn bootstrap_preserves_the_replacement_temp_on_indeterminate_publication() -> Result<()> {
        let _gate = exclusive_fault_gate();
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("bootstrap-indeterminate.varve");
        {
            let writer = VarveStreamWriter::create(spec(), &path, StreamOptions::default())?;
            drop(writer);
        }
        fs::remove_file(state_sidecar_path(&path))?;

        VarveFile::inject_replace_indeterminate_failures(1);
        let error = match bootstrap_stream_checkpoint(spec(), &path, StreamOptions::default()) {
            Err(error) => error,
            Ok(_) => panic!("armed indeterminate publication must fail the bootstrap"),
        };
        let temp = assert_preserved_replacement(error, &state_sidecar_path(&path));
        assert!(
            temp_name_ends_with(&temp, ".vks.tmp"),
            "preserved temp {} must be the state sidecar replacement",
            temp.display()
        );
        assert!(!state_sidecar_path(&path).exists());
        // Recovery: an unarmed bootstrap publishes a fresh state sidecar
        // without consuming the preserved replacement temp.
        bootstrap_stream_checkpoint(spec(), &path, StreamOptions::default())?;
        assert!(state_sidecar_path(&path).exists());
        assert!(
            temp.exists(),
            "recovery must not consume the preserved replacement temp"
        );
        let reader = VarveStreamReader::open(spec(), &path, StreamOptions::default())?;
        drop(reader);
        Ok(())
    }
}

#[test]
fn rebuild_aborts_when_pathname_identity_changes_before_publish() -> Result<()> {
    let _isolation = shared_fault_gate();
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("swap.varve");
    let decoy = directory.path().join("decoy.varve");
    let moved = directory.path().join("moved.varve");
    let options = DiskIndexOptions::default();

    for (target, seed) in [(&path, 0u32), (&decoy, 100u32)] {
        let mut writer = VarveIndexedWriter::create(spec(), target, options, small_plan())?;
        for ordinal in seed..seed + 3 {
            writer.push_info(&Target(format!("key-{ordinal}")))?;
        }
        writer.sync()?;
        drop(writer);
        fs::remove_file(disk_index_sidecar_path(target))?;
    }

    let error = rebuild_disk_index_with_progress(
        spec(),
        &path,
        options,
        small_plan(),
        ScanOptions::default(),
        |progress| {
            // The scan is complete but the sidecar is not yet published:
            // swap another same-spec generation into the scanned pathname.
            if progress.phase == ScanProgressPhase::Complete {
                fs::rename(&path, &moved).expect("move scanned generation aside");
                fs::rename(&decoy, &path).expect("swap decoy into the pathname");
            }
        },
    )
    .expect_err("rebuild must not publish a sidecar for a swapped pathname");
    assert!(matches!(
        &error,
        Error::DiskIndex(source) if matches!(**source, DiskIndexError::IdentityMismatch)
    ));
    assert!(
        !disk_index_sidecar_path(&path).exists(),
        "no sidecar may be published after the identity mismatch"
    );
    Ok(())
}
