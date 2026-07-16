use std::fs::remove_file;
use std::path::{Path, PathBuf};

#[cfg(all(feature = "compression-zstd", feature = "integrity"))]
use varve::decode_from_slice;
#[cfg(feature = "compression-zstd")]
use varve::{
    BlockCompressionDescriptor, BlockDescriptor, BlockKind, CommitPolicy, CompressionAlgorithm,
    CompressionHeaderMode, CompressionLevel, CompressionPolicy, Endian, FormatSpec, IndexPolicy,
    IntegrityPolicy, LayoutPlanFieldSource, LayoutPlanFieldType, LayoutPlanLen, LayoutPlanPartKind,
    ManifestPolicy, ReadLimits, RecoveryPolicy, VariableCompression, encode_to_vec,
};
use varve::{ChunkedBytes, Error, VarveBlock, varve_format};

#[derive(Clone, Debug, PartialEq, VarveBlock)]
#[varve(id = 600, version = 1, kind = "variable")]
struct CompressibleBlock {
    #[varve(field_id = 1)]
    id: u32,
    #[varve(field_id = 2)]
    payload: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, VarveBlock)]
#[varve(id = 601, version = 1, kind = "fixed")]
struct FixedBlock {
    value: u64,
}

#[cfg(feature = "compression-zstd")]
const BLOCK_COMPRESSION_POLICY: VariableCompression = VariableCompression {
    algorithm: CompressionAlgorithm::Zstd,
    level: CompressionLevel::Fast,
    header_mode: CompressionHeaderMode::RecordExplicit,
    min_uncompressed_len: 32,
    only_if_smaller: true,
    max_uncompressed_len: 1024 * 1024,
};

#[cfg(feature = "compression-zstd")]
static MANUAL_COMPRESSION_BLOCKS: &[BlockDescriptor] = &[
    BlockDescriptor {
        id: CompressibleBlock::ID,
        name: "CompressibleBlock",
        version: 1,
        kind: BlockKind::Variable,
        fields: CompressibleBlock::FIELDS,
    },
    BlockDescriptor {
        id: FixedBlock::ID,
        name: "FixedBlock",
        version: 1,
        kind: BlockKind::Fixed,
        fields: FixedBlock::FIELDS,
    },
];

varve_format! {
    pub struct RecordCompressionFormat {
        magic: b"COMPR";
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
        extension: "vcz";
        manifest: embedded;
        compression: variable_blocks(
            zstd,
            level = default,
            header = record_explicit,
            min_len = 32,
            only_if_smaller = true,
            max_len = 1048576,
        );
        blocks: [CompressibleBlock, FixedBlock];
    }
}

varve_format! {
    pub struct FileExplicitCompressionFormat {
        magic: b"COMPF";
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
        compression: variable_blocks(
            zstd,
            level = fast,
            header = file_explicit,
            min_len = 32,
            only_if_smaller = true,
            max_len = 1048576,
        );
        blocks: [CompressibleBlock, FixedBlock];
    }
}

varve_format! {
    pub struct FileExplicitFooterCompressionFormat {
        magic: b"COMPF3";
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
        commit: record_footer;
        compression: variable_blocks(
            zstd,
            level = fast,
            header = file_explicit,
            min_len = 32,
            only_if_smaller = true,
            max_len = 1048576,
        );
        blocks: [CompressibleBlock, FixedBlock];
    }
}

varve_format! {
    pub struct ContractCompressionFormat {
        magic: b"COMPC";
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
        schema_hash: 1;
        compression: variable_blocks(
            zstd,
            level = 3,
            header = format_contract,
            min_len = 32,
            only_if_smaller = true,
            max_len = 1048576,
        );
        blocks: [CompressibleBlock];
    }
}

varve_format! {
    pub struct CheckpointCompressionFormat {
        magic: b"COMPIX";
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
        compression: variable_blocks(
            zstd,
            level = default,
            header = record_explicit,
            min_len = 32,
            only_if_smaller = true,
            max_len = 1048576,
        );
        blocks: [CompressibleBlock];
    }
}

#[cfg(feature = "compression-zstd")]
#[test]
fn record_explicit_variable_blocks_roundtrip_with_physical_scan() -> varve::Result<()> {
    let path = temp_path("record_explicit");
    cleanup(&path);
    let value = compressible(7, 16 * 1024);
    let fixed = FixedBlock { value: 99 };
    let logical_len = encode_to_vec(&value, Endian::Little)?.len() as u64;

    {
        let mut file = RecordCompressionFormat::create(&path)?;
        file.push(&value)?;
        file.push(&fixed)?;
        let manifest = file.schema_manifest()?.expect("embedded manifest");
        assert_eq!(manifest.payload_version, 5);
        assert_eq!(manifest.commit_policy, CommitPolicy::None);
        assert_eq!(manifest.extension, Some("vcz".to_string()));
        assert!(matches!(
            manifest.compression_policy,
            CompressionPolicy::VariableBlocks(compression)
                if compression.header_mode == CompressionHeaderMode::RecordExplicit
        ));

        let compressed = file
            .index_entries()
            .iter()
            .find(|entry| entry.block_id == CompressibleBlock::ID)
            .expect("compressed block");
        assert!(compressed.is_compressed());
        assert!(compressed.uncompressed_len_hint > 0);
        assert!(compressed.payload_len < logical_len);
        assert_eq!(&compressed.read_payload(&path)?[..4], b"VCMP");

        let fixed_entry = file
            .index_entries()
            .iter()
            .find(|entry| entry.block_id == FixedBlock::ID)
            .expect("fixed block");
        assert!(!fixed_entry.is_compressed());
        assert_eq!(fixed_entry.uncompressed_len_hint, 0);
        assert_eq!(
            file.scan()
                .find(|event| event.block_id == CompressibleBlock::ID)
                .expect("event")
                .payload_len,
            compressed.payload_len
        );
        file.flush()?;
    }

    let file = RecordCompressionFormat::open_readonly(&path)?;
    assert_eq!(file.blocks::<CompressibleBlock>()?.get(0)?, Some(value));
    assert_eq!(file.blocks::<FixedBlock>()?.get(0)?, Some(fixed));

    cleanup(&path);
    Ok(())
}

#[cfg(all(feature = "compression-zstd", feature = "mmap"))]
#[test]
fn mmap_windows_expose_physical_compressed_payloads() -> varve::Result<()> {
    let path = temp_path("mmap_physical");
    cleanup(&path);
    let value = compressible(71, 16 * 1024);

    {
        let mut file = RecordCompressionFormat::create(&path)?;
        file.push(&value)?;
        file.flush()?;
    }

    let file = RecordCompressionFormat::open_readonly(&path)?;
    // SAFETY: The fixture is not modified while the mapping is alive.
    let mmap = unsafe { file.mmap_payloads()? };
    let window = mmap
        .block_payload_window::<CompressibleBlock>(0)?
        .expect("compressed payload window");
    assert_eq!(&window[..4], b"VCMP");
    assert_eq!(file.blocks::<CompressibleBlock>()?.get(0)?, Some(value));

    drop(mmap);
    drop(file);
    cleanup(&path);
    Ok(())
}

#[cfg(feature = "compression-zstd")]
#[test]
fn file_explicit_uses_varve2_header_and_raw_compressed_payload() -> varve::Result<()> {
    let path = temp_path("file_explicit");
    cleanup(&path);
    let value = compressible(8, 16 * 1024);

    {
        let mut file = FileExplicitCompressionFormat::create(&path)?;
        file.push(&value)?;
        let entry = file
            .index_entries()
            .iter()
            .find(|entry| entry.block_id == CompressibleBlock::ID)
            .expect("compressed block");
        assert!(entry.is_compressed());
        assert!(entry.uncompressed_len_hint > 0);
        let physical = entry.read_payload(&path)?;
        assert_ne!(&physical[..4], b"VCMP");
        file.flush()?;
    }

    let bytes = std::fs::read(&path)?;
    assert_eq!(&bytes[5..11], b"VARVE2");
    assert_eq!(u16_at(&bytes, 11), 1);
    assert_eq!(bytes[13], 1);
    assert_eq!(bytes[14], 0);
    assert_eq!(u32_at(&bytes, 23), 28);
    assert_eq!(&bytes[27..31], b"VCHD");

    let plan = FileExplicitCompressionFormat::spec().effective_layout();
    let LayoutPlanPartKind::FileHeader(header) = &plan.parts[0].kind else {
        panic!("expected native file header");
    };
    assert_eq!(
        header.fields[1].source,
        LayoutPlanFieldSource::LiteralBytes(b"VARVE2".to_vec())
    );
    assert!(
        header
            .fields
            .iter()
            .any(|field| field.name == "extension_len"
                && field.ty == LayoutPlanFieldType::U32
                && matches!(field.source, LayoutPlanFieldSource::Native("extension_len")))
    );
    assert!(header.fields.iter().any(|field| field.name == "extensions"
        && field.ty
            == LayoutPlanFieldType::Bytes {
                len: LayoutPlanLen::Fixed(28)
            }
        && matches!(
            field.source,
            LayoutPlanFieldSource::Native("file_explicit_compression_header")
        )));

    let file = FileExplicitCompressionFormat::open_readonly(&path)?;
    assert_eq!(file.blocks::<CompressibleBlock>()?.get(0)?, Some(value));

    cleanup(&path);
    Ok(())
}

#[cfg(feature = "compression-zstd")]
#[test]
fn file_explicit_with_record_footer_uses_varve3_header_and_extensions() -> varve::Result<()> {
    let path = temp_path("file_explicit_footer");
    cleanup(&path);
    let value = compressible(88, 16 * 1024);

    {
        let mut file = FileExplicitFooterCompressionFormat::create(&path)?;
        file.push(&value)?;
        file.flush()?;
    }

    let bytes = std::fs::read(&path)?;
    assert_eq!(&bytes[0..6], b"COMPF3");
    assert_eq!(&bytes[6..12], b"VARVE3");
    assert_eq!(u16_at(&bytes, 12), 1);
    assert_eq!(bytes[14], 1);
    assert_eq!(bytes[15], 0);
    assert_eq!(u32_at(&bytes, 24), 28);
    assert_eq!(&bytes[28..32], b"VCHD");

    let inspected = FileExplicitFooterCompressionFormat::inspect_layout_file(&path)?;
    assert_eq!(inspected.file_header_len, 56);
    assert_eq!(inspected.segments[0].segment_start, 56);

    let file = FileExplicitFooterCompressionFormat::open_readonly(&path)?;
    assert_eq!(file.blocks::<CompressibleBlock>()?.get(0)?, Some(value));

    cleanup(&path);
    Ok(())
}

#[cfg(feature = "compression-zstd")]
#[test]
fn format_contract_requires_computed_schema_hash() -> varve::Result<()> {
    let path = temp_path("format_contract");
    cleanup(&path);
    assert!(matches!(
        ContractCompressionFormat::create(&path),
        Err(Error::InvalidFormatSpec(_))
    ));

    let spec = ContractCompressionFormat::spec().with_computed_schema_hash();
    let value = compressible(9, 16 * 1024);
    {
        let mut file = spec.create(&path)?;
        file.push(&value)?;
        file.flush()?;
    }
    let file = spec.open_readonly(&path)?;
    assert_eq!(file.blocks::<CompressibleBlock>()?.get(0)?, Some(value));

    cleanup(&path);
    Ok(())
}

#[cfg(feature = "compression-zstd")]
#[test]
fn block_specific_record_explicit_compression_overrides_global_none() -> varve::Result<()> {
    static BLOCK_COMPRESSION: &[BlockCompressionDescriptor] = &[BlockCompressionDescriptor {
        block_id: CompressibleBlock::ID,
        compression: BLOCK_COMPRESSION_POLICY,
    }];
    let spec = FormatSpec::new(
        b"BCOMP",
        1,
        Endian::Little,
        0,
        IndexPolicy::ScanOnOpen,
        IntegrityPolicy::None,
        RecoveryPolicy::Strict,
        ManifestPolicy::None,
        MANUAL_COMPRESSION_BLOCKS,
    )
    .with_read_limits(ReadLimits::finite_all(1 << 30))
    .with_block_compression(BLOCK_COMPRESSION);
    let path = temp_path("block_specific");
    cleanup(&path);
    let value = compressible(12, 16 * 1024);

    {
        let mut file = spec.create(&path)?;
        file.push(&value)?;
        file.push(&FixedBlock { value: 99 })?;
        let compressed = file
            .index_entries()
            .iter()
            .find(|entry| entry.block_id == CompressibleBlock::ID)
            .expect("compressed variable block");
        assert!(compressed.is_compressed());
        let fixed = file
            .index_entries()
            .iter()
            .find(|entry| entry.block_id == FixedBlock::ID)
            .expect("fixed block");
        assert!(!fixed.is_compressed());
        file.flush()?;
    }

    let file = spec.open_readonly(&path)?;
    assert_eq!(file.blocks::<CompressibleBlock>()?.get(0)?, Some(value));
    cleanup(&path);
    Ok(())
}

#[cfg(feature = "compression-zstd")]
#[test]
fn block_specific_compression_rejects_invalid_descriptors() {
    static FIXED_BLOCK_COMPRESSION: &[BlockCompressionDescriptor] = &[BlockCompressionDescriptor {
        block_id: FixedBlock::ID,
        compression: BLOCK_COMPRESSION_POLICY,
    }];
    static DUPLICATE_BLOCK_COMPRESSION: &[BlockCompressionDescriptor] = &[
        BlockCompressionDescriptor {
            block_id: CompressibleBlock::ID,
            compression: BLOCK_COMPRESSION_POLICY,
        },
        BlockCompressionDescriptor {
            block_id: CompressibleBlock::ID,
            compression: BLOCK_COMPRESSION_POLICY,
        },
    ];
    static FILE_EXPLICIT_BLOCK_COMPRESSION: &[BlockCompressionDescriptor] =
        &[BlockCompressionDescriptor {
            block_id: CompressibleBlock::ID,
            compression: VariableCompression {
                header_mode: CompressionHeaderMode::FileExplicit,
                ..BLOCK_COMPRESSION_POLICY
            },
        }];

    let spec = |compression| {
        FormatSpec::new(
            b"BCMPV",
            1,
            Endian::Little,
            0,
            IndexPolicy::ScanOnOpen,
            IntegrityPolicy::None,
            RecoveryPolicy::Strict,
            ManifestPolicy::None,
            MANUAL_COMPRESSION_BLOCKS,
        )
        .with_block_compression(compression)
    };

    assert!(matches!(
        spec(FIXED_BLOCK_COMPRESSION).validate(),
        Err(Error::InvalidFormatSpec(
            "block compression requires a variable block"
        ))
    ));
    assert!(matches!(
        spec(DUPLICATE_BLOCK_COMPRESSION).validate(),
        Err(Error::InvalidFormatSpec("duplicate block compression"))
    ));
    assert!(matches!(
        spec(FILE_EXPLICIT_BLOCK_COMPRESSION).validate(),
        Err(Error::InvalidFormatSpec(
            "block compression requires record_explicit header"
        ))
    ));
}

#[cfg(feature = "compression-zstd")]
#[test]
fn compressed_checkpoint_and_rewrite_preserve_length_hint() -> varve::Result<()> {
    let path = temp_path("checkpoint_rewrite");
    cleanup(&path);
    let first = compressible(10, 16 * 1024);
    let second = compressible(11, 12 * 1024);

    {
        let mut file = CheckpointCompressionFormat::create(&path)?;
        file.push(&first)?;
        file.flush()?;
    }
    {
        let mut file = CheckpointCompressionFormat::open(&path)?;
        assert_eq!(file.blocks::<CompressibleBlock>()?.get(0)?, Some(first));
        file.replace_rewrite(0, &second)?;
        file.flush()?;
        let entry = file
            .index_entries()
            .iter()
            .find(|entry| entry.block_id == CompressibleBlock::ID)
            .expect("rewritten compressed block");
        assert!(entry.is_compressed());
        assert!(entry.uncompressed_len_hint > 0);
    }

    let file = CheckpointCompressionFormat::open_readonly(&path)?;
    assert_eq!(file.blocks::<CompressibleBlock>()?.get(0)?, Some(second));

    cleanup(&path);
    Ok(())
}

#[cfg(all(feature = "compression-zstd", feature = "integrity"))]
#[test]
fn chunked_bytes_roundtrip_and_chunk_crc_detection() -> varve::Result<()> {
    let payload = (0..8192)
        .map(|index| (index % 251) as u8)
        .collect::<Vec<_>>();
    let chunked = ChunkedBytes::from_zstd_chunks(&payload, 257, CompressionLevel::Fast)?;
    assert_eq!(chunked.decode_to_vec()?, payload);

    let encoded = encode_to_vec(&chunked, Endian::Little)?;
    let decoded: ChunkedBytes = decode_from_slice(&encoded, Endian::Little)?;
    assert_eq!(decoded.decode_to_vec()?, payload);

    let mut corrupt = chunked.into_encoded();
    let first_crc_offset = 32 + 8;
    corrupt[first_crc_offset] ^= 0x55;
    let corrupt = ChunkedBytes::from_encoded(corrupt)?;
    assert!(matches!(
        corrupt.decode_to_vec(),
        Err(Error::ChunkChecksumMismatch { chunk_index: 0, .. })
    ));
    Ok(())
}

#[test]
fn chunked_bytes_rejects_hostile_header_extents_before_decode_allocation() {
    let mut encoded = Vec::new();
    encoded.extend_from_slice(b"VCHK");
    encoded.extend_from_slice(&1u16.to_le_bytes());
    encoded.push(1);
    encoded.push(0);
    encoded.extend_from_slice(&1u64.to_le_bytes());
    encoded.extend_from_slice(&u64::MAX.to_le_bytes());
    encoded.extend_from_slice(&u32::MAX.to_le_bytes());
    encoded.extend_from_slice(&0u32.to_le_bytes());
    assert!(matches!(
        varve::ChunkedBytes::from_encoded(encoded),
        Err(Error::InvalidChunkedBytes)
    ));
}

#[test]
fn chunked_bytes_rejects_inconsistent_count_and_chunk_extents() {
    let mut encoded = Vec::new();
    encoded.extend_from_slice(b"VCHK");
    encoded.extend_from_slice(&1u16.to_le_bytes());
    encoded.push(1);
    encoded.push(0);
    encoded.extend_from_slice(&4u64.to_le_bytes());
    encoded.extend_from_slice(&1u64.to_le_bytes());
    encoded.extend_from_slice(&0u32.to_le_bytes());
    encoded.extend_from_slice(&0u32.to_le_bytes());
    assert!(matches!(
        varve::ChunkedBytes::from_encoded(encoded),
        Err(Error::InvalidChunkedBytes)
    ));
}

#[test]
fn chunked_bytes_default_decode_enforces_standard_logical_limit() -> varve::Result<()> {
    const LOGICAL_LEN: u32 = 256 * 1024 * 1024 + 1;
    let mut encoded = Vec::new();
    encoded.extend_from_slice(b"VCHK");
    encoded.extend_from_slice(&1u16.to_le_bytes());
    encoded.push(1);
    encoded.push(0);
    encoded.extend_from_slice(&u64::from(LOGICAL_LEN).to_le_bytes());
    encoded.extend_from_slice(&u64::from(LOGICAL_LEN).to_le_bytes());
    encoded.extend_from_slice(&1u32.to_le_bytes());
    encoded.extend_from_slice(&0u32.to_le_bytes());
    encoded.extend_from_slice(&LOGICAL_LEN.to_le_bytes());
    encoded.extend_from_slice(&1u32.to_le_bytes());
    encoded.extend_from_slice(&0u32.to_le_bytes());
    encoded.extend_from_slice(&0u32.to_le_bytes());
    encoded.push(0);

    let chunked = ChunkedBytes::from_encoded(encoded)?;
    assert!(matches!(
        chunked.decode_to_vec(),
        Err(Error::DecompressedLengthLimitExceeded {
            actual: 268_435_457,
            limit: 268_435_456,
        })
    ));
    Ok(())
}

#[cfg(all(feature = "compression-zstd", feature = "integrity"))]
#[test]
fn chunked_bytes_limited_decode_checks_logical_len_before_allocation() -> varve::Result<()> {
    let chunked = ChunkedBytes::from_zstd_chunks(&[1, 2, 3, 4], 2, CompressionLevel::Fast)?;
    assert!(matches!(
        chunked.decode_to_vec_limited(3),
        Err(Error::DecompressedLengthLimitExceeded {
            actual: 4,
            limit: 3
        })
    ));
    Ok(())
}

#[cfg(all(feature = "compression-zstd", not(feature = "integrity")))]
#[test]
fn chunked_bytes_requires_integrity_for_chunk_crc() {
    assert!(matches!(
        varve::ChunkedBytes::from_zstd_chunks(&[1, 2, 3, 4], 2, varve::CompressionLevel::Fast),
        Err(Error::IntegrityFeatureDisabled)
    ));
}

#[cfg(all(feature = "integrity", not(feature = "compression-zstd")))]
#[test]
fn chunked_bytes_requires_zstd_backend_for_chunk_compression() {
    assert!(matches!(
        varve::ChunkedBytes::from_zstd_chunks(&[1, 2, 3, 4], 2, varve::CompressionLevel::Fast),
        Err(Error::CompressionFeatureDisabled)
    ));
}

#[cfg(not(feature = "compression-zstd"))]
#[test]
fn compression_feature_disabled_is_reported_on_write() -> varve::Result<()> {
    let path = temp_path("feature_disabled");
    cleanup(&path);
    let value = compressible(12, 16 * 1024);
    let mut file = RecordCompressionFormat::create(&path)?;
    assert!(matches!(
        file.push(&value),
        Err(Error::CompressionFeatureDisabled)
    ));
    cleanup(&path);
    Ok(())
}

#[cfg(not(feature = "compression-zstd"))]
#[test]
fn compression_feature_disabled_is_reported_on_existing_compressed_record() -> varve::Result<()> {
    let path = temp_path("feature_disabled_read");
    cleanup(&path);
    write_disabled_backend_fixture(&path)?;
    assert!(matches!(
        RecordCompressionFormat::open_readonly(&path),
        Err(Error::CompressionFeatureDisabled)
    ));
    cleanup(&path);
    Ok(())
}

fn compressible(id: u32, len: usize) -> CompressibleBlock {
    CompressibleBlock {
        id,
        payload: vec![b'A'; len],
    }
}

#[cfg(not(feature = "compression-zstd"))]
fn write_disabled_backend_fixture(path: &Path) -> std::io::Result<()> {
    let physical_payload = b"compressed-by-a-backend-not-enabled-here";
    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"COMPR");
    bytes.extend_from_slice(b"VARVE1");
    bytes.extend_from_slice(&1u16.to_le_bytes());
    bytes.push(1);
    bytes.push(0);
    bytes.extend_from_slice(&0u64.to_le_bytes());
    bytes.extend_from_slice(&600u32.to_le_bytes());
    bytes.extend_from_slice(&1u16.to_le_bytes());
    bytes.extend_from_slice(&1u16.to_le_bytes());
    bytes.extend_from_slice(&0u64.to_le_bytes());
    bytes.extend_from_slice(&(physical_payload.len() as u64).to_le_bytes());
    bytes.extend_from_slice(&0u32.to_le_bytes());
    bytes.extend_from_slice(&64u32.to_le_bytes());
    bytes.extend_from_slice(physical_payload);
    std::fs::write(path, bytes)
}

fn temp_path(name: &str) -> PathBuf {
    let mut path = std::env::temp_dir();
    path.push(format!(
        "varve_compression_{name}_{}.vrv",
        std::process::id()
    ));
    path
}

fn cleanup(path: &Path) {
    let _ = remove_file(path);
    let mut lock = path.as_os_str().to_os_string();
    lock.push(".lock");
    let _ = remove_file(PathBuf::from(lock));
}

#[cfg(feature = "compression-zstd")]
fn u16_at(bytes: &[u8], offset: usize) -> u16 {
    let mut value = [0; 2];
    value.copy_from_slice(&bytes[offset..offset + 2]);
    u16::from_le_bytes(value)
}

#[cfg(feature = "compression-zstd")]
fn u32_at(bytes: &[u8], offset: usize) -> u32 {
    let mut value = [0; 4];
    value.copy_from_slice(&bytes[offset..offset + 4]);
    u32::from_le_bytes(value)
}
