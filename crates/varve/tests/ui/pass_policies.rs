use varve::{
    varve_format, CompressionAlgorithm, CompressionHeaderMode, CompressionLevel,
    CompressionPolicy, Endian, IndexPolicy, IntegrityPolicy, ManifestPolicy, RecoveryPolicy,
    VarveBlock,
};

#[derive(Clone, VarveBlock)]
#[varve(id = 300, kind = "variable", key = "id")]
struct PolicyBlock {
    #[varve(field_id = 1)]
    id: u64,
    #[varve(field_id = 2)]
    value: String,
}

varve_format! {
    pub struct PolicyFormat {
        magic: b"POLICY";
        version: 2;
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
        endian: big;
        schema_hash: 42;
        extension: "vrv";
        integrity: crc32;
        index: checkpoint_on_flush;
        recovery: truncate_tail;
        manifest: embedded;
        compression: variable_blocks(
            zstd,
            level = default,
            header = record_explicit,
            min_len = 32,
            only_if_smaller = true,
            max_len = 1048576,
        );
        blocks: [PolicyBlock];
    }
}

varve_format! {
    pub struct HeaderCrcPolicyFormat {
        magic: b"HCRC";
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
        integrity: crc32_with_header;
        blocks: [PolicyBlock];
    }
}

fn main() {
    let spec = PolicyFormat::spec();
    assert_eq!(spec.version, 2);
    assert_eq!(spec.endian, Endian::Big);
    assert_eq!(spec.schema_hash, 42);
    assert_eq!(spec.extension, Some("vrv"));
    assert_eq!(spec.integrity_policy, IntegrityPolicy::Crc32);
    assert_eq!(
        HeaderCrcPolicyFormat::spec().integrity_policy,
        IntegrityPolicy::Crc32WithHeader
    );
    assert_eq!(spec.index_policy, IndexPolicy::CheckpointOnFlush);
    assert_eq!(spec.recovery_policy, RecoveryPolicy::TruncateTail);
    assert_eq!(spec.manifest_policy, ManifestPolicy::Embedded);
    match spec.compression_policy {
        CompressionPolicy::VariableBlocks(compression) => {
            assert_eq!(compression.algorithm, CompressionAlgorithm::Zstd);
            assert_eq!(compression.level, CompressionLevel::Default);
            assert_eq!(compression.header_mode, CompressionHeaderMode::RecordExplicit);
            assert_eq!(compression.min_uncompressed_len, 32);
            assert!(compression.only_if_smaller);
            assert_eq!(compression.max_uncompressed_len, 1048576);
        }
        CompressionPolicy::None => panic!("missing compression policy"),
    }
    let _ = PolicyFormat::open_recover_with_report("missing.varve");
}
