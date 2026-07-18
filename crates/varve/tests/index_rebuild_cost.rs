#![cfg(feature = "high-cardinality-dev")]

//! Rebuild cost contracts (PERF-04/PERF-05, DUR-03).
//!
//! Rebuild resolves each record's descriptor once by block id and decodes each
//! tombstone key exactly once, so rebuild cost must not scale with the plan's
//! descriptor count. The strict per-record decode counters live in the
//! varve-core unit tests; this file pins the end-to-end contracts: a wide plan
//! rebuilds a tombstone-heavy file in roughly the time of a single-descriptor
//! plan, the published `historical_distinct_keys` metric follows K-ever, and
//! rebuild refuses to publish a sidecar when the pathname identity changes
//! after the scan (DUR-03).

use std::fs;
use std::time::{Duration, Instant};

use varve::{
    BlockDescriptor, BlockKind, Decoder, DiskIndexError, DiskIndexOptions, DiskIndexPlan,
    DiskIndexedBlock, Encoder, Endian, Error, FormatSpec, IndexPolicy, IntegrityPolicy,
    ManifestPolicy, ReadLimits, RecoveryPolicy, Result, ScanOptions, ScanProgressPhase, VarveBlock,
    VarveDecode, VarveEncode, VarveIndexedReader, VarveIndexedWriter, VarveKeyedBlock, WireType,
    disk_index_sidecar_path, rebuild_disk_index, rebuild_disk_index_with_progress,
};

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

    fs::remove_file(disk_index_sidecar_path(&path))?;
    let small_started = Instant::now();
    let small_report = rebuild_disk_index(spec(), &path, options, small_plan())?;
    let small_elapsed = small_started.elapsed();
    assert_eq!(small_report.records, records);
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
    assert_eq!(full_report.records, records);
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
    assert_eq!(report.records, 64);
    let reader = VarveIndexedReader::open(spec(), &path, options, small_plan())?;
    assert_eq!(reader.historical_distinct_keys()?, 32);
    Ok(())
}

#[test]
fn rebuild_aborts_when_pathname_identity_changes_before_publish() -> Result<()> {
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
