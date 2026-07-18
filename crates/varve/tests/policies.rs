use std::collections::{BTreeMap, HashMap};
use std::fs::{OpenOptions, remove_file};
use std::io::Write;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{
    Arc, Barrier,
    atomic::{AtomicUsize, Ordering},
};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use varve::{
    BlockDescriptor, BlockKind, CommitPolicy, CompressionHeaderMode, CompressionPolicy, Decoder,
    Encoder, Endian, Error, FieldDescriptor, FieldPresence, FormatSpec, INDEX_BLOCK_ID,
    IndexPolicy, IntegrityPolicy, MANIFEST_BLOCK_ID, ManifestPolicy, RecoveryPolicy,
    VariableCompression, VarveBlock, VarveDecode, VarveEncode, VarveMerge, VarveMigration,
    WireType, WriterLockBreakPolicy, decode_from_slice, encode_to_vec, varve_format,
};

#[derive(Clone, Debug, PartialEq, VarveBlock)]
#[varve(id = 20, version = 1, kind = "fixed")]
struct ContractBlock {
    value: u32,
}

#[derive(Clone, Debug, PartialEq, VarveBlock)]
#[varve(id = 20, version = 2, kind = "fixed")]
struct ContractBlockV2 {
    value: u32,
}

#[derive(Clone, Debug, PartialEq, VarveBlock)]
#[varve(id = 22, version = 1, kind = "fixed")]
struct ForeignFixed {
    value: u32,
}

#[derive(Clone, Debug, PartialEq, VarveBlock)]
#[varve(id = 23, version = 1, kind = "variable", key = "id")]
struct ForeignUser {
    #[varve(field_id = 1)]
    id: u32,
    #[varve(field_id = 2)]
    name: String,
}

#[derive(Clone, Debug, PartialEq, VarveBlock)]
#[varve(id = 24, version = 1, kind = "variable")]
struct ForeignUserOp {
    #[varve(field_id = 1)]
    name: String,
}

impl VarveMerge for ForeignUser {
    type Op = ForeignUserOp;

    fn apply_op(&mut self, op: Self::Op) -> varve::Result<()> {
        self.name = op.name;
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, VarveBlock)]
#[varve(id = 21, version = 1, kind = "variable")]
struct RichBlock {
    #[varve(field_id = 1)]
    maybe: Option<u32>,
    #[varve(field_id = 2)]
    fixed: [u16; 3],
    #[varve(field_id = 3)]
    numbers: Vec<u16>,
    #[varve(field_id = 4)]
    names: Vec<String>,
    #[varve(field_id = 5)]
    bytes: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, VarveBlock)]
#[varve(id = 25, version = 1, kind = "variable")]
struct MapBlock {
    #[varve(field_id = 1)]
    values: BTreeMap<String, u32>,
    #[varve(field_id = 2)]
    hash_values: HashMap<String, u32>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PackedPair {
    left: u32,
    right: u32,
}

impl VarveEncode for PackedPair {
    const WIRE_TYPE: WireType = WireType::U64;

    fn encode_varve(&self, encoder: &mut Encoder) -> varve::Result<()> {
        let packed = (u64::from(self.left) << 32) | u64::from(self.right);
        packed.encode_varve(encoder)
    }
}

impl VarveDecode for PackedPair {
    const WIRE_TYPE: WireType = WireType::U64;

    fn decode_varve(decoder: &mut Decoder<'_>) -> varve::Result<Self> {
        let packed = u64::decode_varve(decoder)?;
        Ok(Self {
            left: (packed >> 32) as u32,
            right: packed as u32,
        })
    }
}

#[derive(Clone, Debug, PartialEq, VarveBlock)]
#[varve(id = 27, version = 1, kind = "fixed")]
struct CustomCodecBlock {
    packed: PackedPair,
}

#[derive(Clone, Debug, PartialEq, VarveBlock)]
#[varve(id = 26, version = 1, kind = "variable")]
struct DefaultedBlock {
    #[varve(field_id = 1, default)]
    value: u32,
}

varve_format! {
    pub struct ContractFormat {
        magic: b"CONTRACT";
        version: 1;
        limits {
            file_len: 8_589_934_592;
            records: 4_000_000;
            index_bytes: 536_870_912;
            scan_bytes: 8_589_934_592;
            record_payload: 67_108_864;
            logical_payload: 268_435_456;
            materialized_bytes: 1_073_741_824;
            segments: 4_000_000;
            matrix_dimension: 16_000_000;
            matrix_cells: 16_000_000;
            matrix_bitmap: 64_000_000;
            matrix_crc: 128_000_000;
            matrix_metadata: 268_435_456;
            matrix_slot_region: 8_589_934_592;
            sidecar: 268_435_456;
            mmap: 8_589_934_592;
        }
        endian: little;
        schema_hash: 99;
        blocks: [ContractBlock, RichBlock, MapBlock, CustomCodecBlock];
    }
}

varve_format! {
    pub struct CheckpointFormat {
        magic: b"CHECKPT";
        version: 1;
        limits {
            file_len: 8_589_934_592;
            records: 4_000_000;
            index_bytes: 536_870_912;
            scan_bytes: 8_589_934_592;
            record_payload: 67_108_864;
            logical_payload: 268_435_456;
            materialized_bytes: 1_073_741_824;
            segments: 4_000_000;
            matrix_dimension: 16_000_000;
            matrix_cells: 16_000_000;
            matrix_bitmap: 64_000_000;
            matrix_crc: 128_000_000;
            matrix_metadata: 268_435_456;
            matrix_slot_region: 8_589_934_592;
            sidecar: 268_435_456;
            mmap: 8_589_934_592;
        }
        endian: little;
        index: checkpoint_on_flush;
        blocks: [ContractBlock];
    }
}

varve_format! {
    pub struct ManifestFormat {
        magic: b"MANIFEST";
        version: 1;
        limits {
            file_len: 8_589_934_592;
            records: 4_000_000;
            index_bytes: 536_870_912;
            scan_bytes: 8_589_934_592;
            record_payload: 67_108_864;
            logical_payload: 268_435_456;
            materialized_bytes: 1_073_741_824;
            segments: 4_000_000;
            matrix_dimension: 16_000_000;
            matrix_cells: 16_000_000;
            matrix_bitmap: 64_000_000;
            matrix_crc: 128_000_000;
            matrix_metadata: 268_435_456;
            matrix_slot_region: 8_589_934_592;
            sidecar: 268_435_456;
            mmap: 8_589_934_592;
        }
        endian: little;
        schema_hash: 77;
        manifest: embedded;
        blocks: [ContractBlock, RichBlock, DefaultedBlock];
    }
}

struct ContractMigration;

impl VarveMigration<ContractBlock, ContractBlockV2> for ContractMigration {
    fn migrate(from: ContractBlock) -> varve::Result<ContractBlockV2> {
        Ok(ContractBlockV2 { value: from.value })
    }
}

#[cfg(feature = "integrity")]
varve_format! {
    pub struct CrcFormat {
        magic: b"CRC";
        version: 1;
        limits {
            file_len: 8_589_934_592;
            records: 4_000_000;
            index_bytes: 536_870_912;
            scan_bytes: 8_589_934_592;
            record_payload: 67_108_864;
            logical_payload: 268_435_456;
            materialized_bytes: 1_073_741_824;
            segments: 4_000_000;
            matrix_dimension: 16_000_000;
            matrix_cells: 16_000_000;
            matrix_bitmap: 64_000_000;
            matrix_crc: 128_000_000;
            matrix_metadata: 268_435_456;
            matrix_slot_region: 8_589_934_592;
            sidecar: 268_435_456;
            mmap: 8_589_934_592;
        }
        endian: little;
        integrity: crc32;
        blocks: [ContractBlock];
    }
}

/// Mirrors the private `INDEX_CHECKPOINT_MIN_RECORDS` floor in varve-core's
/// `file.rs`. PERF-02 geometric checkpoint spacing writes the first full index
/// checkpoint only once the live tail has grown by at least this many records,
/// so checkpoint-mechanism tests must push at least this many blocks.
const CHECKPOINT_MIN_RECORDS: u32 = 16;

#[test]
fn header_contract_rejects_version_endian_and_schema_mismatch() -> varve::Result<()> {
    let path = temp_path("header");
    cleanup(&path);

    {
        let mut file = ContractFormat::create(&path)?;
        file.push(&ContractBlock { value: 7 })?;
        file.flush()?;
    }

    let mut bad_version = ContractFormat::spec();
    bad_version.version = 2;
    assert!(matches!(
        bad_version.open_readonly(&path),
        Err(Error::FormatVersionMismatch {
            expected: 2,
            actual: 1
        })
    ));

    let mut bad_endian = ContractFormat::spec();
    bad_endian.endian = Endian::Big;
    assert!(matches!(
        bad_endian.open_readonly(&path),
        Err(Error::EndianMismatch { .. })
    ));

    let mut bad_schema = ContractFormat::spec();
    bad_schema.schema_hash = 100;
    assert!(matches!(
        bad_schema.open_readonly(&path),
        Err(Error::SchemaHashMismatch {
            expected: 100,
            actual: 99
        })
    ));

    cleanup(&path);
    Ok(())
}

#[test]
fn typed_read_rejects_unexpected_block_version() -> varve::Result<()> {
    let path = temp_path("block_version");
    cleanup(&path);

    {
        let mut file = ContractFormat::create(&path)?;
        file.push(&ContractBlock { value: 42 })?;
        file.flush()?;
    }

    let file = ContractFormat::open_readonly(&path)?;
    assert!(matches!(
        file.blocks::<ContractBlockV2>(),
        Err(Error::BlockVersionMismatch {
            block_id: 20,
            expected: 1,
            actual: 2
        })
    ));

    cleanup(&path);
    Ok(())
}

#[test]
fn write_apis_enforce_registry_membership_and_version() -> varve::Result<()> {
    let path = temp_path("write_registry");
    cleanup(&path);

    {
        let mut file = ContractFormat::create(&path)?;
        file.push(&ContractBlock { value: 1 })?;

        assert!(matches!(
            file.push(&ForeignFixed { value: 2 }),
            Err(Error::UnregisteredBlock(22))
        ));
        assert!(matches!(
            file.push(&ContractBlockV2 { value: 2 }),
            Err(Error::BlockVersionMismatch {
                block_id: 20,
                expected: 1,
                actual: 2
            })
        ));
        assert!(matches!(
            file.replace_fixed(0, &ContractBlockV2 { value: 3 }),
            Err(Error::BlockVersionMismatch {
                block_id: 20,
                expected: 1,
                actual: 2
            })
        ));
        assert!(matches!(
            file.replace_rewrite(0, &ForeignFixed { value: 3 }),
            Err(Error::UnregisteredBlock(22))
        ));
        assert!(matches!(
            file.delete::<ForeignUser>(&1),
            Err(Error::UnregisteredBlock(23))
        ));
        assert!(matches!(
            file.push_op::<ForeignUser>(
                &1,
                &ForeignUserOp {
                    name: "x".to_string()
                }
            ),
            Err(Error::UnregisteredBlock(23))
        ));
    }

    cleanup(&path);
    Ok(())
}

#[test]
fn rich_codec_roundtrips_options_arrays_sequences_and_bytes() -> varve::Result<()> {
    let path = temp_path("rich_codec");
    cleanup(&path);
    let value = RichBlock {
        maybe: Some(9),
        fixed: [1, 2, 3],
        numbers: vec![5, 8, 13],
        names: vec!["alpha".to_string(), "beta".to_string()],
        bytes: vec![1, 3, 3, 7],
    };

    {
        let mut file = ContractFormat::create(&path)?;
        file.push(&value)?;
        file.flush()?;
    }

    let file = ContractFormat::open_readonly(&path)?;
    assert_eq!(file.blocks::<RichBlock>()?.get(0)?, Some(value));

    cleanup(&path);
    Ok(())
}

#[test]
fn map_codec_roundtrips_in_canonical_key_order() -> varve::Result<()> {
    let path = temp_path("map_codec");
    cleanup(&path);
    let value = MapBlock {
        values: BTreeMap::from([
            ("bravo".to_string(), 2),
            ("alpha".to_string(), 1),
            ("charlie".to_string(), 3),
        ]),
        hash_values: HashMap::from([
            ("bravo".to_string(), 2),
            ("alpha".to_string(), 1),
            ("charlie".to_string(), 3),
        ]),
    };
    let mut left = HashMap::new();
    left.insert("bravo".to_string(), 2u32);
    left.insert("alpha".to_string(), 1u32);
    left.insert("charlie".to_string(), 3u32);
    let mut right = HashMap::new();
    right.insert("charlie".to_string(), 3u32);
    right.insert("alpha".to_string(), 1u32);
    right.insert("bravo".to_string(), 2u32);
    assert_eq!(
        encode_to_vec(&left, Endian::Little)?,
        encode_to_vec(&right, Endian::Little)?
    );

    {
        let mut file = ContractFormat::create(&path)?;
        file.push(&value)?;
        file.flush()?;
    }

    let file = ContractFormat::open_readonly(&path)?;
    assert_eq!(file.blocks::<MapBlock>()?.get(0)?, Some(value));

    cleanup(&path);
    Ok(())
}

#[test]
fn user_defined_codec_type_roundtrips_as_block_field() -> varve::Result<()> {
    let path = temp_path("custom_codec");
    cleanup(&path);
    let value = CustomCodecBlock {
        packed: PackedPair {
            left: 0xCAFE_BABE,
            right: 0x0123_4567,
        },
    };

    {
        let mut file = ContractFormat::create(&path)?;
        file.push(&value)?;
        file.flush()?;
    }

    let file = ContractFormat::open_readonly(&path)?;
    assert_eq!(file.blocks::<CustomCodecBlock>()?.get(0)?, Some(value));
    assert_eq!(
        <CustomCodecBlock as VarveBlock>::FIELDS[0].wire_type,
        WireType::U64
    );

    cleanup(&path);
    Ok(())
}

#[test]
fn variable_unknown_field_policy_is_skip_known_wire_reject_unknown_wire() -> varve::Result<()> {
    let value = RichBlock {
        maybe: None,
        fixed: [1, 2, 3],
        numbers: vec![10, 20],
        names: vec!["known".to_string()],
        bytes: vec![7, 8, 9],
    };
    let mut encoded = encode_to_vec(&value, Endian::Little)?;
    let future_payload = encode_to_vec(&"future".to_string(), Endian::Little)?;
    append_raw_field(&mut encoded, 9_999, WireType::String, &future_payload);
    assert_eq!(
        decode_from_slice::<RichBlock>(&encoded, Endian::Little)?,
        value
    );

    append_raw_field_header(&mut encoded, 10_000, u16::MAX, 0);
    assert!(matches!(
        decode_from_slice::<RichBlock>(&encoded, Endian::Little),
        Err(Error::UnknownWireType(u16::MAX))
    ));
    Ok(())
}

#[test]
fn metadata_and_checkpoint_are_written_as_internal_blocks() -> varve::Result<()> {
    let path = temp_path("metadata_checkpoint");
    cleanup(&path);

    {
        let mut file = CheckpointFormat::create(&path)?;
        file.write_metadata("creator", b"varve")?;
        file.write_metadata("creator", b"varve-2")?;
        // Geometric checkpoint spacing (PERF-02) writes the first full checkpoint
        // only once the tail grows past INDEX_CHECKPOINT_MIN_RECORDS (16) records.
        for value in 0..CHECKPOINT_MIN_RECORDS {
            file.push(&ContractBlock { value })?;
        }
        file.flush()?;
    }

    let file = CheckpointFormat::open_readonly(&path)?;
    assert_eq!(file.metadata("creator")?, Some(b"varve-2".to_vec()));
    assert_eq!(file.all_metadata()?.len(), 2);
    assert!(
        file.index_entries()
            .iter()
            .any(|entry| entry.block_id == INDEX_BLOCK_ID)
    );

    cleanup(&path);
    Ok(())
}

#[test]
fn embedded_manifest_is_optional_and_retrievable() -> varve::Result<()> {
    let absent_path = temp_path("manifest_absent");
    let present_path = temp_path("manifest_present");
    cleanup(&absent_path);
    cleanup(&present_path);

    {
        let mut file = ContractFormat::create(&absent_path)?;
        file.push(&ContractBlock { value: 3 })?;
        file.flush()?;
    }
    let file = ContractFormat::open_readonly(&absent_path)?;
    assert_eq!(file.schema_manifest()?, None);

    {
        let mut file = ManifestFormat::create(&present_path)?;
        assert!(
            file.index_entries()
                .iter()
                .any(|entry| entry.block_id == MANIFEST_BLOCK_ID)
        );
        file.push(&ContractBlock { value: 4 })?;
        file.flush()?;
    }
    let file = ManifestFormat::open_readonly(&present_path)?;
    let manifest = file.schema_manifest()?.expect("embedded manifest");
    assert_eq!(manifest.payload_version, 5);
    assert_eq!(manifest.format_version, 1);
    assert_eq!(manifest.endian, Endian::Little);
    assert_eq!(manifest.schema_hash, 77);
    assert_eq!(manifest.extension, None);
    assert_eq!(manifest.manifest_policy, ManifestPolicy::Embedded);
    assert_eq!(manifest.commit_policy, CommitPolicy::None);
    assert_eq!(manifest.compression_policy, CompressionPolicy::None);
    assert_eq!(manifest.blocks.len(), 3);
    assert_eq!(manifest.blocks[0].id, ContractBlock::ID);
    assert_eq!(manifest.blocks[0].version, ContractBlock::VERSION);
    assert_eq!(manifest.blocks[0].fields.len(), 1);
    assert_eq!(manifest.blocks[0].fields[0].id, 1);
    assert_eq!(manifest.blocks[0].fields[0].name, "value");
    assert_eq!(manifest.blocks[0].fields[0].wire_type, WireType::U32);
    assert_eq!(
        manifest.blocks[0].fields[0].presence,
        FieldPresence::Required
    );
    let defaulted = manifest
        .blocks
        .iter()
        .find(|block| block.id == DefaultedBlock::ID)
        .expect("defaulted block manifest entry");
    assert_eq!(defaulted.fields.len(), 1);
    assert_eq!(defaulted.fields[0].presence, FieldPresence::Defaulted);
    assert_eq!(
        file.blocks::<ContractBlock>()?.get(0)?,
        Some(ContractBlock { value: 4 })
    );

    cleanup(&absent_path);
    cleanup(&present_path);
    Ok(())
}

#[test]
fn schema_debug_dump_and_hash_are_stable_and_field_aware() {
    let spec = ManifestFormat::spec();
    let hash = spec.computed_schema_hash();
    assert_ne!(hash, 0);
    assert_eq!(hash, ManifestFormat::spec().computed_schema_hash());

    let mut repinned = spec;
    repinned.schema_hash = hash.wrapping_add(1);
    assert_eq!(repinned.computed_schema_hash(), hash);

    let mut versioned = spec;
    versioned.version = spec.version + 1;
    assert_ne!(versioned.computed_schema_hash(), hash);

    let dump = spec.schema_debug_dump();
    assert!(dump.contains("block 20 ContractBlock v1 Fixed"));
    assert!(dump.contains("field 1 value U32 Required"));
    assert!(dump.contains("field 1 value U32 Defaulted"));
}

#[test]
fn explicit_migration_reads_source_version_while_normal_read_rejects_mismatch() -> varve::Result<()>
{
    let path = temp_path("migration");
    cleanup(&path);

    {
        let mut file = ContractFormat::create(&path)?;
        file.push(&ContractBlock { value: 15 })?;
        file.flush()?;
    }

    let file = ContractFormat::open_readonly(&path)?;
    assert!(matches!(
        file.blocks::<ContractBlockV2>(),
        Err(Error::BlockVersionMismatch {
            block_id: 20,
            expected: 1,
            actual: 2
        })
    ));
    assert_eq!(
        file.blocks_migrated::<ContractBlock, ContractBlockV2, ContractMigration>()?,
        vec![ContractBlockV2 { value: 15 }]
    );

    cleanup(&path);
    Ok(())
}

#[test]
fn checkpoint_open_uses_latest_checkpoint_and_scans_tail() -> varve::Result<()> {
    let path = temp_path("checkpoint_tail");
    cleanup(&path);

    // Push enough records to force a full checkpoint on flush (PERF-02 geometric
    // spacing), then append a few more before sync so the reopen must both use
    // the checkpoint and scan the tail that follows it.
    let checkpointed = CHECKPOINT_MIN_RECORDS;
    let tail = 3u32;
    let total = checkpointed + tail;
    {
        let mut file = CheckpointFormat::create(&path)?;
        for value in 0..checkpointed {
            file.push(&ContractBlock { value })?;
        }
        file.flush()?;
        for value in checkpointed..total {
            file.push(&ContractBlock { value })?;
        }
        file.sync()?;
    }

    let file = CheckpointFormat::open_readonly(&path)?;
    let blocks = file.blocks::<ContractBlock>()?;
    assert_eq!(blocks.len(), total as usize);
    assert_eq!(blocks.get(0)?, Some(ContractBlock { value: 0 }));
    assert_eq!(
        blocks.get(total as usize - 1)?,
        Some(ContractBlock { value: total - 1 })
    );
    assert!(
        file.index_entries()
            .iter()
            .any(|entry| entry.block_id == INDEX_BLOCK_ID)
    );

    cleanup(&path);
    Ok(())
}

#[test]
fn corrupt_checkpoint_falls_back_to_full_scan_without_hiding_data() -> varve::Result<()> {
    let path = temp_path("checkpoint_corrupt");
    cleanup(&path);

    // Push enough records to force a full checkpoint on flush (PERF-02), then
    // corrupt it: the reopen must fall back to a full native scan and still
    // surface every record.
    let record_count = CHECKPOINT_MIN_RECORDS;
    let checkpoint_payload_offset = {
        let mut file = CheckpointFormat::create(&path)?;
        for value in 0..record_count {
            file.push(&ContractBlock { value })?;
        }
        file.flush()?;
        file.index_entries()
            .iter()
            .find(|entry| entry.block_id == INDEX_BLOCK_ID)
            .expect("checkpoint")
            .payload_offset
    };

    tamper_byte(&path, checkpoint_payload_offset)?;
    let file = CheckpointFormat::open_readonly(&path)?;
    let blocks = file.blocks::<ContractBlock>()?;
    assert_eq!(blocks.len(), record_count as usize);
    for value in 0..record_count {
        assert_eq!(blocks.get(value as usize)?, Some(ContractBlock { value }));
    }

    cleanup(&path);
    Ok(())
}

#[test]
fn strict_open_rejects_and_recover_truncates_partial_tail() -> varve::Result<()> {
    let path = temp_path("recover_tail");
    cleanup(&path);

    {
        let mut file = ContractFormat::create(&path)?;
        file.push(&ContractBlock { value: 1 })?;
        file.push(&ContractBlock { value: 2 })?;
        file.flush()?;
    }

    let original_len = std::fs::metadata(&path)?.len();
    append_partial_record_header(&path)?;
    let corrupt_len = std::fs::metadata(&path)?.len();
    let recovering_spec = ContractFormat::spec().with_recovery_policy(RecoveryPolicy::TruncateTail);
    assert!(matches!(
        recovering_spec.open(&path),
        Err(Error::CorruptTail { .. })
    ));
    assert_eq!(std::fs::metadata(&path)?.len(), corrupt_len);
    let readonly = recovering_spec.open_readonly(&path)?;
    assert_eq!(readonly.blocks::<ContractBlock>()?.len(), 2);
    drop(readonly);
    assert_eq!(std::fs::metadata(&path)?.len(), corrupt_len);

    {
        let (file, report) = recovering_spec.open_recover_with_report(&path)?;
        assert_eq!(report.original_len, original_len + 4);
        assert_eq!(report.recovered_len, original_len);
        assert_eq!(report.records_preserved, 2);
        assert_eq!(file.blocks::<ContractBlock>()?.len(), 2);
    }

    append_incomplete_payload(&path)?;
    assert!(matches!(
        ContractFormat::open_readonly(&path),
        Err(Error::CorruptTail { .. })
    ));
    let (_file, report) = recovering_spec.open_recover_with_report(&path)?;
    assert_eq!(report.recovered_len, original_len);

    cleanup(&path);
    Ok(())
}

#[cfg(feature = "integrity")]
#[test]
fn crc_integrity_detects_payload_tampering() -> varve::Result<()> {
    let path = temp_path("crc");
    cleanup(&path);

    let payload_offset = {
        let mut file = CrcFormat::create(&path)?;
        file.push(&ContractBlock { value: 123 })?;
        file.flush()?;
        file.index_entries()[0].payload_offset
    };

    tamper_byte(&path, payload_offset)?;
    assert!(matches!(
        CrcFormat::open_readonly(&path),
        Err(Error::ChecksumMismatch { .. })
    ));
    let recovering_spec = CrcFormat::spec().with_recovery_policy(RecoveryPolicy::TruncateTail);
    assert!(matches!(
        recovering_spec.open_recover_with_report(&path),
        Err(Error::ChecksumMismatch { .. })
    ));

    cleanup(&path);
    Ok(())
}

#[test]
fn format_spec_validation_rejects_bad_specs() {
    static DUPLICATE_FIELDS: &[FieldDescriptor] = &[
        FieldDescriptor {
            id: 1,
            name: "a",
            wire_type: WireType::U32,
            presence: FieldPresence::Required,
        },
        FieldDescriptor {
            id: 1,
            name: "b",
            wire_type: WireType::U64,
            presence: FieldPresence::Required,
        },
    ];
    static BLOCKS: &[BlockDescriptor] = &[BlockDescriptor {
        id: 0xFFFF_FF00,
        name: "bad",
        version: 1,
        kind: BlockKind::Fixed,
        fields: &[],
    }];
    static FIELD_BLOCKS: &[BlockDescriptor] = &[BlockDescriptor {
        id: 30,
        name: "bad-fields",
        version: 1,
        kind: BlockKind::Variable,
        fields: DUPLICATE_FIELDS,
    }];

    assert!(matches!(
        FormatSpec::new(
            b"",
            1,
            Endian::Little,
            0,
            IndexPolicy::ScanOnOpen,
            IntegrityPolicy::None,
            RecoveryPolicy::Strict,
            ManifestPolicy::None,
            &[],
        )
        .validate(),
        Err(Error::InvalidFormatSpec(_))
    ));

    assert!(matches!(
        FormatSpec::new(
            b"BAD",
            1,
            Endian::Little,
            0,
            IndexPolicy::ScanOnOpen,
            IntegrityPolicy::None,
            RecoveryPolicy::Strict,
            ManifestPolicy::None,
            BLOCKS,
        )
        .validate(),
        Err(Error::InvalidFormatSpec(_))
    ));

    assert!(matches!(
        FormatSpec::new(
            b"BADFIELD",
            1,
            Endian::Little,
            0,
            IndexPolicy::ScanOnOpen,
            IntegrityPolicy::None,
            RecoveryPolicy::Strict,
            ManifestPolicy::None,
            FIELD_BLOCKS,
        )
        .validate(),
        Err(Error::InvalidFormatSpec("duplicate field id"))
    ));

    assert!(matches!(
        FormatSpec::new(
            b"BADEXT",
            1,
            Endian::Little,
            0,
            IndexPolicy::ScanOnOpen,
            IntegrityPolicy::None,
            RecoveryPolicy::Strict,
            ManifestPolicy::None,
            &[],
        )
        .with_extension(Some(""))
        .validate(),
        Err(Error::InvalidFormatSpec("extension must not be empty"))
    ));

    assert!(matches!(
        FormatSpec::new(
            b"BADCONTRACT",
            1,
            Endian::Little,
            1,
            IndexPolicy::ScanOnOpen,
            IntegrityPolicy::None,
            RecoveryPolicy::Strict,
            ManifestPolicy::None,
            &[],
        )
        .with_compression_policy(CompressionPolicy::VariableBlocks(
            VariableCompression::zstd(CompressionHeaderMode::FormatContract,)
        ))
        .validate(),
        Err(Error::InvalidFormatSpec(
            "format contract compression requires schema_hash to equal computed_schema_hash"
        ))
    ));
}

#[test]
fn format_spec_builder_matches_direct_spec_construction() -> varve::Result<()> {
    static BLOCKS: &[BlockDescriptor] = &[BlockDescriptor {
        id: ContractBlock::ID,
        name: "ContractBlock",
        version: ContractBlock::VERSION,
        kind: BlockKind::Fixed,
        fields: ContractBlock::FIELDS,
    }];

    let spec = FormatSpec::builder()
        .magic(b"BUILDER")
        .version(7)
        .endian(Endian::Big)
        .schema_hash(1234)
        .extension(Some("vbf"))
        .index_policy(IndexPolicy::CheckpointOnFlush)
        .integrity_policy(IntegrityPolicy::None)
        .recovery_policy(RecoveryPolicy::TruncateTail)
        .manifest_policy(ManifestPolicy::Embedded)
        .blocks(BLOCKS)
        .build()?;

    assert_eq!(spec.magic, b"BUILDER");
    assert_eq!(spec.version, 7);
    assert_eq!(spec.endian, Endian::Big);
    assert_eq!(spec.schema_hash, 1234);
    assert_eq!(spec.extension, Some("vbf"));
    assert_eq!(spec.index_policy, IndexPolicy::CheckpointOnFlush);
    assert_eq!(spec.recovery_policy, RecoveryPolicy::TruncateTail);
    assert_eq!(spec.manifest_policy, ManifestPolicy::Embedded);
    assert_eq!(spec.blocks[0].id, ContractBlock::ID);
    assert!(matches!(
        FormatSpec::builder().build(),
        Err(Error::InvalidFormatSpec("missing magic"))
    ));

    Ok(())
}

#[test]
fn writer_lock_metadata_is_inspectable_and_default_open_refuses() -> varve::Result<()> {
    let path = temp_path("writer_lock_metadata");
    cleanup(&path);

    assert_eq!(ContractFormat::spec().inspect_writer_lock(&path)?, None);

    let file = ContractFormat::create(&path)?;
    let info = ContractFormat::spec()
        .inspect_writer_lock(&path)?
        .expect("writer lock");
    assert_eq!(info.path, writer_lock_path(&path));
    assert_eq!(info.target_path, absolute_test_path(&path)?);
    assert_eq!(info.process_id, std::process::id());
    assert!(info.created_unix_ms > 0);
    assert!(matches!(
        ContractFormat::open(&path),
        Err(Error::WriterLockHeld(_))
    ));
    assert!(matches!(
        ContractFormat::create(&path),
        Err(Error::WriterLockHeld(_))
    ));
    assert!(matches!(
        ContractFormat::open_recover(&path),
        Err(Error::WriterLockHeld(_))
    ));
    assert!(matches!(
        ContractFormat::spec().open_with_lock_policy(&path, WriterLockBreakPolicy::Refuse),
        Err(Error::WriterLockHeld(_))
    ));
    assert!(matches!(
        ContractFormat::spec().open_with_lock_policy(
            &path,
            WriterLockBreakPolicy::BreakIfOlderThan(Duration::ZERO)
        ),
        Err(Error::WriterLockBreakRefused(_))
    ));

    drop(file);
    assert_eq!(ContractFormat::spec().inspect_writer_lock(&path)?, None);
    cleanup(&path);
    Ok(())
}

#[test]
fn explicit_age_based_lock_break_is_opt_in() -> varve::Result<()> {
    let path = temp_path("writer_lock_break_age");
    cleanup(&path);

    {
        let mut file = ContractFormat::create(&path)?;
        file.push(&ContractBlock { value: 88 })?;
        file.flush()?;
    }

    write_test_lock(&path, std::process::id(), current_unix_time_ms())?;
    assert!(matches!(
        ContractFormat::spec().open_with_lock_policy(
            &path,
            WriterLockBreakPolicy::BreakIfOlderThan(Duration::MAX)
        ),
        Err(Error::WriterLockBreakRefused(_))
    ));
    assert!(writer_lock_path(&path).exists());

    write_test_lock(&path, 0, 0)?;
    {
        let file = ContractFormat::spec().open_with_lock_policy(
            &path,
            WriterLockBreakPolicy::BreakIfOlderThan(Duration::ZERO),
        )?;
        assert_eq!(
            file.blocks::<ContractBlock>()?.get(0)?,
            Some(ContractBlock { value: 88 })
        );
        let info = ContractFormat::spec()
            .inspect_writer_lock(&path)?
            .expect("replacement writer lock");
        assert_eq!(info.process_id, std::process::id());
    }

    cleanup(&path);
    Ok(())
}

#[test]
fn stale_lock_can_be_cleared_without_opening_the_data_file() -> varve::Result<()> {
    let path = temp_path("writer_lock_clear_without_open");
    cleanup(&path);

    {
        let mut file = ContractFormat::create(&path)?;
        file.push(&ContractBlock { value: 144 })?;
        file.flush()?;
    }

    write_test_lock(&path, std::process::id(), current_unix_time_ms())?;
    assert!(matches!(
        ContractFormat::clear_stale_writer_lock(&path, WriterLockBreakPolicy::BreakIfProcessAbsent),
        Err(Error::WriterLockBreakRefused(_))
    ));
    assert!(writer_lock_path(&path).exists());

    let mut child = Command::new(std::env::current_exe()?)
        .arg("--list")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    let exited_process = child.id();
    assert!(child.wait()?.success());
    write_test_lock(&path, exited_process, current_unix_time_ms())?;
    ContractFormat::clear_stale_writer_lock(&path, WriterLockBreakPolicy::BreakIfProcessAbsent)?;
    drop(child);
    assert_eq!(ContractFormat::spec().inspect_writer_lock(&path)?, None);
    assert_eq!(
        ContractFormat::open_readonly(&path)?
            .blocks::<ContractBlock>()?
            .get(0)?,
        Some(ContractBlock { value: 144 })
    );

    cleanup(&path);
    Ok(())
}

#[test]
fn concurrent_stale_lock_recovery_allows_exactly_one_writer() -> varve::Result<()> {
    const WORKERS: usize = 16;

    let path = temp_path("writer_lock_concurrent_recovery");
    cleanup(&path);
    drop(ContractFormat::create(&path)?);
    write_test_lock(&path, 0, 0)?;

    let start = Arc::new(Barrier::new(WORKERS + 1));
    let finish = Arc::new(Barrier::new(WORKERS + 1));
    let successes = Arc::new(AtomicUsize::new(0));
    let mut workers = Vec::with_capacity(WORKERS);
    for _ in 0..WORKERS {
        let path = path.clone();
        let start = Arc::clone(&start);
        let finish = Arc::clone(&finish);
        let successes = Arc::clone(&successes);
        workers.push(std::thread::spawn(move || {
            start.wait();
            let writer = ContractFormat::spec().open_with_lock_policy(
                &path,
                WriterLockBreakPolicy::BreakIfOlderThan(Duration::ZERO),
            );
            match &writer {
                Ok(_) => {
                    successes.fetch_add(1, Ordering::SeqCst);
                }
                Err(error) => assert!(matches!(error, Error::WriterLockBreakRefused(_))),
            }
            finish.wait();
            drop(writer);
        }));
    }

    start.wait();
    finish.wait();
    for worker in workers {
        worker.join().expect("lock recovery worker");
    }
    assert_eq!(successes.load(Ordering::SeqCst), 1);
    assert_eq!(ContractFormat::spec().inspect_writer_lock(&path)?, None);

    cleanup(&path);
    Ok(())
}

#[test]
fn malformed_writer_lock_is_reported_and_not_broken() -> varve::Result<()> {
    let path = temp_path("writer_lock_malformed");
    cleanup(&path);

    {
        let mut file = ContractFormat::create(&path)?;
        file.push(&ContractBlock { value: 1 })?;
        file.flush()?;
    }

    let lock = writer_lock_path(&path);
    std::fs::write(&lock, b"not a varve lock")?;
    assert!(matches!(
        ContractFormat::spec().inspect_writer_lock(&path),
        Err(Error::WriterLockMalformed(_))
    ));
    assert!(matches!(
        ContractFormat::spec().open_with_lock_policy(
            &path,
            WriterLockBreakPolicy::BreakIfOlderThan(Duration::ZERO)
        ),
        Err(Error::WriterLockMalformed(_))
    ));
    assert!(lock.exists());

    cleanup(&path);
    Ok(())
}

fn append_partial_record_header(path: &Path) -> varve::Result<()> {
    let mut file = OpenOptions::new().append(true).open(path)?;
    file.write_all(&[1, 2, 3, 4])?;
    Ok(())
}

fn append_incomplete_payload(path: &Path) -> varve::Result<()> {
    let mut file = OpenOptions::new().append(true).open(path)?;
    file.write_all(&20u32.to_le_bytes())?;
    file.write_all(&1u16.to_le_bytes())?;
    file.write_all(&0u16.to_le_bytes())?;
    file.write_all(&99u64.to_le_bytes())?;
    file.write_all(&100u64.to_le_bytes())?;
    file.write_all(&0u32.to_le_bytes())?;
    file.write_all(&0u32.to_le_bytes())?;
    file.write_all(&[0xAA])?;
    Ok(())
}

fn tamper_byte(path: &Path, offset: u64) -> varve::Result<()> {
    let mut file = OpenOptions::new().read(true).write(true).open(path)?;
    file.seek(SeekFrom::Start(offset))?;
    let mut byte = [0; 1];
    file.read_exact(&mut byte)?;
    file.seek(SeekFrom::Start(offset))?;
    file.write_all(&[byte[0] ^ 0xFF])?;
    Ok(())
}

fn temp_path(name: &str) -> PathBuf {
    let mut path = std::env::temp_dir();
    path.push(format!("varve_policy_{name}_{}.vrv", std::process::id()));
    path
}

fn writer_lock_path(path: &Path) -> PathBuf {
    let mut lock = path.as_os_str().to_os_string();
    lock.push(".lock");
    PathBuf::from(lock)
}

fn write_test_lock(path: &Path, process_id: u32, created_unix_ms: u64) -> varve::Result<()> {
    std::fs::write(
        writer_lock_path(path),
        format!(
            "varve-lock-v1\npid={process_id}\ncreated_unix_ms={created_unix_ms}\ntarget={}\n",
            absolute_test_path(path)?.display()
        ),
    )?;
    Ok(())
}

fn absolute_test_path(path: &Path) -> varve::Result<PathBuf> {
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        Ok(std::env::current_dir()?.join(path))
    }
}

fn current_unix_time_ms() -> u64 {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    u64::try_from(millis).unwrap_or(u64::MAX)
}

fn append_raw_field(output: &mut Vec<u8>, field_id: u32, wire_type: WireType, payload: &[u8]) {
    append_raw_field_header(output, field_id, wire_type as u16, payload.len() as u64);
    output.extend_from_slice(payload);
}

fn append_raw_field_header(output: &mut Vec<u8>, field_id: u32, wire_type: u16, payload_len: u64) {
    output.extend_from_slice(&field_id.to_le_bytes());
    output.extend_from_slice(&wire_type.to_le_bytes());
    output.extend_from_slice(&0u16.to_le_bytes());
    output.extend_from_slice(&payload_len.to_le_bytes());
}

fn cleanup(path: &Path) {
    let _ = remove_file(path);
    let _ = remove_file(writer_lock_path(path));
}
