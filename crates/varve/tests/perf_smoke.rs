use std::fs::{OpenOptions, metadata, remove_file};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use varve::{
    BinaryCursor, BinaryWriter, BlockDescriptor, BlockKind, ChunkEntry, ChunkIndexBuilder,
    ChunkLayout, Endian, FormatSpec, IndexPolicy, LayoutSegmentInfo, MatrixAuxDescriptor,
    MatrixBlockDescriptor, MatrixCommitDescriptor, MatrixCommitKind, MatrixDimensionDescriptor,
    MatrixDimensions, MatrixKey, ReadLimits, RecoveryPolicy, SegmentReducer, SegmentWrite,
    VarveBlock, VarveDecode, VarveEncode, VarveMatrixBlock, VarveMerge, compact_keyed_file,
    compact_keyed_files, merge_keyed_files, reduce_segments_by_ref, varve_format,
};

#[derive(Clone, Debug, PartialEq, VarveBlock)]
#[varve(id = 100, version = 1, kind = "fixed")]
struct PerfPoint {
    x: u64,
    y: u64,
}

#[repr(C)]
#[derive(Clone, Debug, PartialEq, VarveBlock)]
#[cfg_attr(
    feature = "zero-copy",
    derive(
        varve::zerocopy::FromBytes,
        varve::zerocopy::Immutable,
        varve::zerocopy::KnownLayout
    )
)]
#[varve(id = 103, version = 1, kind = "fixed")]
struct PerfRawPoint {
    bytes: [u8; 16],
}

#[cfg(feature = "zero-copy")]
unsafe impl varve::VarveRawFixedBlock for PerfRawPoint {
    const RAW_ENDIAN: varve::Endian = varve::Endian::Little;
}

#[derive(Clone, Debug, PartialEq)]
struct PerfMatrixCell {
    value: u32,
}

impl VarveEncode for PerfMatrixCell {
    const WIRE_TYPE: varve::WireType = varve::WireType::Nested;

    fn encode_varve(&self, encoder: &mut varve::Encoder) -> varve::Result<()> {
        self.value.encode_varve(encoder)
    }
}

impl VarveDecode for PerfMatrixCell {
    const WIRE_TYPE: varve::WireType = varve::WireType::Nested;

    fn decode_varve(decoder: &mut varve::Decoder<'_>) -> varve::Result<Self> {
        Ok(Self {
            value: u32::decode_varve(decoder)?,
        })
    }
}

impl VarveBlock for PerfMatrixCell {
    const ID: u32 = 104;
    const VERSION: u16 = 1;
    const KIND: BlockKind = BlockKind::Matrix;
    const ENDIAN: Option<Endian> = None;
}

impl VarveMatrixBlock for PerfMatrixCell {
    const DIMENSIONS: [&'static str; 2] = ["scan", "ch"];
    const CATEGORY: &'static str = "analysis";
    const SLOT_STRIDE: u64 = 4;
}

#[derive(Clone, Debug, PartialEq, VarveBlock)]
#[varve(id = 101, version = 1, kind = "variable", key = "user_id")]
struct PerfUser {
    #[varve(field_id = 1)]
    user_id: u64,
    #[varve(field_id = 2)]
    region: u16,
    #[varve(field_id = 3)]
    name: String,
    #[varve(field_id = 4)]
    payload: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, VarveBlock)]
#[varve(id = 102, version = 1, kind = "variable")]
struct PerfUserOp {
    #[varve(field_id = 1)]
    rename_to: String,
}

impl VarveMerge for PerfUser {
    type Op = PerfUserOp;

    fn apply_op(&mut self, op: Self::Op) -> varve::Result<()> {
        self.name = op.rename_to;
        Ok(())
    }
}

varve_format! {
    pub struct PerfFormat {
        magic: b"PERFSMK";
        version: 1;
        endian: little;
        blocks: [PerfPoint, PerfRawPoint, PerfUser, PerfUserOp];
    }
}

fn perf_matrix_spec() -> FormatSpec {
    perf_matrix_spec_with_integrity(varve::IntegrityPolicy::None)
}

fn perf_matrix_aux_spec() -> FormatSpec {
    static AUX: &[MatrixAuxDescriptor] = &[MatrixAuxDescriptor {
        name: "scratch",
        byte_len: 64 * 1024,
    }];
    perf_matrix_spec().with_matrix_aux(AUX)
}

#[cfg(feature = "integrity")]
fn perf_matrix_crc_spec() -> FormatSpec {
    perf_matrix_spec_with_integrity(varve::IntegrityPolicy::Crc32)
}

fn perf_matrix_spec_with_integrity(integrity: varve::IntegrityPolicy) -> FormatSpec {
    static BLOCKS: &[BlockDescriptor] = &[BlockDescriptor {
        id: PerfMatrixCell::ID,
        name: "PerfMatrixCell",
        version: PerfMatrixCell::VERSION,
        kind: BlockKind::Matrix,
        fields: &[],
    }];
    static DIMS: &[MatrixDimensionDescriptor] = &[
        MatrixDimensionDescriptor { name: "scan" },
        MatrixDimensionDescriptor { name: "ch" },
    ];
    static COMMITS: &[MatrixCommitDescriptor] = &[MatrixCommitDescriptor {
        name: PerfMatrixCell::CATEGORY,
        kind: MatrixCommitKind::Cell,
    }];
    static MATRIX_BLOCKS: &[MatrixBlockDescriptor] = &[MatrixBlockDescriptor {
        block_id: PerfMatrixCell::ID,
        dimensions: PerfMatrixCell::DIMENSIONS,
        category: PerfMatrixCell::CATEGORY,
        slot_stride: PerfMatrixCell::SLOT_STRIDE,
    }];
    FormatSpec::new(
        b"PERFMTX",
        1,
        Endian::Little,
        0,
        IndexPolicy::ScanOnOpen,
        integrity,
        varve::RecoveryPolicy::Strict,
        varve::ManifestPolicy::None,
        BLOCKS,
    )
    .with_matrix_spec(DIMS, COMMITS, MATRIX_BLOCKS)
    .with_read_limits(ReadLimits::finite_all(u64::MAX))
}

varve_format! {
    pub struct PerfCheckpointFormat {
        magic: b"PERFCHK";
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
        blocks: [PerfPoint];
    }
}

varve_format! {
    pub format PerfVarve3Format {
        magic: b"PERFV3";
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
        index: [scan_on_open, block_offset_chain, keyed_offset_chain];
        commit: transaction_marker(on_flush);
        blocks {
            variable PerfV3User(id = 201, key = [user_id]) {
                user_id: u64,
                name: String,
                payload: Vec<u8>,
            }
        }
    }
}

varve_format! {
    pub format PerfPhysicalLayoutFormat {
        magic: b"PERFPHY";
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
        schema_hash: computed;
        preset: none;

        layout {
            file_header PhysicalHeader {
                bytes signature = b"PHY!";
                u16 header_version = 1;
            }

            segment PhysicalSegment repeat until_eof {
                lead_in PhysicalLeadIn {
                    bytes tag = b"PSEG";
                    u32 toc_mask;
                    i64 next_segment_offset = finalize(target = segment_end, relative_to = after_lead_in);
                    i64 raw_data_offset = finalize(target = raw_region_start, relative_to = after_lead_in);
                }

                metadata PhysicalMetadata;
                raw_region PhysicalRaw;

                footer PhysicalFooter {
                    bytes seal = b"DONE";
                    u64 segment_len = finalize(target = segment_end, relative_to = segment_start);
                }
            }
        }
    }
}

#[cfg(feature = "compression-zstd")]
varve_format! {
    pub struct PerfCompressedFormat {
        magic: b"PERFCMP";
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
            level = default,
            header = record_explicit,
            min_len = 128,
            only_if_smaller = true,
            max_len = 1048576,
        );
        blocks: [PerfUser, PerfUserOp];
    }
}

#[test]
#[ignore = "performance smoke test: run with `cargo test -p varve --test perf_smoke -- --ignored --nocapture`"]
fn perf_smoke_core_paths() -> varve::Result<()> {
    for case in [
        PerfCase::new("small", 128),
        PerfCase::new("medium", 2_048),
        PerfCase::new("large", 10_000),
    ] {
        println!("\n== {}: {} records ==", case.name, case.records);
        append_open_and_scan(case)?;
        resized_replacement(case)?;
        #[cfg(feature = "mmap")]
        mmap_payload_window_scan(case)?;
        #[cfg(feature = "zero-copy")]
        zero_copy_raw_fixed_reads(case)?;
        #[cfg(feature = "compression-zstd")]
        compressed_variable_blocks(case)?;
        checkpoint_open(case)?;
        varve3_footer_and_chain(case)?;
        materialized_keyed_state(case)?;
        physical_layout_segments(case)?;
        adapter_toolkit_paths(case)?;
        matrix_direct_access(case)?;
        matrix_aux_region_access(case)?;
        #[cfg(feature = "integrity")]
        matrix_crc_direct_access(case)?;
    }

    merge_and_compact(2_048)?;
    recovery_tail_truncation(2_048)?;
    Ok(())
}

#[derive(Clone, Copy, Debug)]
struct PerfAdapterMetadata {
    value: u64,
}

#[derive(Default)]
struct PerfAdapterState {
    total: u64,
}

struct PerfAdapterReducer;

impl SegmentReducer for PerfAdapterReducer {
    type Metadata = PerfAdapterMetadata;
    type State = PerfAdapterState;

    fn initial() -> Self::State {
        PerfAdapterState::default()
    }

    fn apply_segment(
        state: &mut Self::State,
        _segment: &LayoutSegmentInfo,
        metadata: Self::Metadata,
    ) -> varve::Result<()> {
        state.total = state.total.wrapping_add(metadata.value);
        Ok(())
    }
}

fn adapter_toolkit_paths(case: PerfCase) -> varve::Result<()> {
    let path = temp_path(&format!("{}_adapter_toolkit", case.name));
    let mut segments = Vec::with_capacity(case.records);
    let mut metadata = Vec::with_capacity(case.records);
    let elapsed = timed(|| {
        let mut chunks = ChunkIndexBuilder::new();
        for index in 0..case.records {
            let mut writer = BinaryWriter::new(Endian::Little);
            writer.len_prefixed_string::<u16>("stream")?;
            writer.u64(index as u64)?;
            writer.f64(index as f64 * 0.5)?;
            let bytes = writer.into_inner();

            let mut cursor = BinaryCursor::new(&bytes, Endian::Little);
            assert_eq!(cursor.len_prefixed_string::<u16>()?, "stream");
            let value = cursor.u64()?;
            let _ = cursor.f64()?;
            cursor.finish()?;

            let segment = perf_layout_segment_info(index as u64 * 16, 16);
            chunks.push(
                ChunkEntry {
                    key: (index % 16) as u16,
                    segment_index: index,
                    byte_offset: 0,
                    byte_len: 16,
                    value_count: 2,
                    layout: ChunkLayout::Contiguous,
                },
                &segment,
            )?;
            segments.push(segment);
            metadata.push(PerfAdapterMetadata { value });
        }
        let chunk_index = chunks.finish()?;
        assert_eq!(chunk_index.len(), case.records);
        let reduced =
            reduce_segments_by_ref::<PerfAdapterReducer, _>(segments.iter().zip(metadata.clone()))?;
        assert_eq!(reduced.segments_applied, case.records);
        Ok(())
    })?;
    report("adapter toolkit", case.records, &path, elapsed);
    Ok(())
}

fn physical_layout_segments(case: PerfCase) -> varve::Result<()> {
    let path = temp_path(&format!("{}_physical_layout", case.name));
    cleanup(&path);
    let fields = [varve::LayoutFieldValue {
        name: "toc_mask",
        value: varve::LayoutValue::U32(0x1110),
    }];

    let elapsed = timed(|| {
        let mut writer = PerfPhysicalLayoutFormat::create_layout_writer(&path)?;
        let split = (case.records / 2).max(1);
        for index in 0..split {
            let metadata = (index as u64).to_le_bytes();
            let mut raw = [0u8; 16];
            raw[..8].copy_from_slice(&(index as u64).to_le_bytes());
            raw[8..].copy_from_slice(&((index as u64).wrapping_mul(3)).to_le_bytes());
            writer.write_segment(SegmentWrite {
                name: "PhysicalSegment",
                fields: &fields,
                footer_fields: &[],
                metadata: &metadata,
                raw: &raw,
            })?;
        }
        writer.flush()?;

        drop(writer);
        let mut writer = PerfPhysicalLayoutFormat::open_layout_writer(&path)?;
        for index in split..case.records {
            let metadata = (index as u64).to_le_bytes();
            let mut raw = [0u8; 16];
            raw[..8].copy_from_slice(&(index as u64).to_le_bytes());
            raw[8..].copy_from_slice(&((index as u64).wrapping_mul(3)).to_le_bytes());
            writer.write_segment(SegmentWrite {
                name: "PhysicalSegment",
                fields: &fields,
                footer_fields: &[],
                metadata: &metadata,
                raw: &raw,
            })?;
        }
        writer.flush()?;
        Ok(())
    })?;
    report("layout reopen+append", case.records, &path, elapsed);

    let elapsed = timed(|| {
        let reader = PerfPhysicalLayoutFormat::open_layout_reader(&path)?;
        assert_eq!(reader.file_header_len(), 6);
        assert_eq!(reader.segments().len(), case.records);
        let raw = reader.read_raw(case.records - 1)?;
        assert_eq!(
            u64::from_le_bytes(raw[..8].try_into().expect("raw prefix")),
            (case.records - 1) as u64
        );
        Ok(())
    })?;
    report("layout open/scan", case.records, &path, elapsed);

    cleanup(&path);
    Ok(())
}

fn append_open_and_scan(case: PerfCase) -> varve::Result<()> {
    let path = temp_path(&format!("{}_append_scan", case.name));
    cleanup(&path);

    let elapsed = timed(|| {
        let mut file = PerfFormat::create(&path)?;
        for index in 0..case.records {
            file.push(&PerfPoint {
                x: index as u64,
                y: (index as u64).wrapping_mul(3),
            })?;
        }
        file.flush()?;
        Ok(())
    })?;
    report("append fixed", case.records, &path, elapsed);

    let elapsed = timed(|| {
        let file = PerfFormat::open_readonly(&path)?;
        assert_eq!(file.scan().count(), case.records);
        let points = file.blocks::<PerfPoint>()?;
        assert_eq!(points.len(), case.records);
        assert_eq!(
            points.get(case.records - 1)?,
            Some(PerfPoint {
                x: (case.records - 1) as u64,
                y: ((case.records - 1) as u64).wrapping_mul(3),
            })
        );
        Ok(())
    })?;
    report("open/scan fixed", case.records, &path, elapsed);

    cleanup(&path);
    Ok(())
}

fn resized_replacement(case: PerfCase) -> varve::Result<()> {
    let path = temp_path(&format!("{}_resized_replace", case.name));
    cleanup(&path);

    {
        let mut file = PerfFormat::create(&path)?;
        for index in 0..case.records {
            file.push(&perf_user(index))?;
        }
        file.flush()?;
    }

    let target = case.records / 2;
    let replacement = PerfUser {
        user_id: target as u64,
        region: 9,
        name: "resized replacement".to_string(),
        payload: vec![0xA5; 256 * 1024],
    };
    let elapsed = timed(|| {
        let mut file = PerfFormat::open(&path)?;
        let info = file.replace_block(target, &replacement)?;
        assert!(info.new_payload_len > info.old_payload_len);
        Ok(())
    })?;
    report("replace resized COW", case.records, &path, elapsed);

    let file = PerfFormat::open_readonly(&path)?;
    assert_eq!(file.blocks::<PerfUser>()?.get(target)?, Some(replacement));
    cleanup(&path);
    Ok(())
}

#[cfg(feature = "mmap")]
fn mmap_payload_window_scan(case: PerfCase) -> varve::Result<()> {
    let path = temp_path(&format!("{}_mmap_windows", case.name));
    cleanup(&path);

    {
        let mut file = PerfFormat::create(&path)?;
        for index in 0..case.records {
            file.push(&perf_user(index))?;
        }
        file.flush()?;
    }

    let elapsed = timed(|| {
        let file = PerfFormat::open_readonly(&path)?;
        // SAFETY: The benchmark does not mutate the fixture while mapped.
        let mmap = unsafe { file.mmap_payloads()? };
        assert_eq!(mmap.len(), case.records);

        let mut total_len = 0usize;
        for entry in mmap.index_entries() {
            total_len += mmap.payload_window(entry)?.len();
        }
        assert!(total_len > case.records * 32);

        let mut total_len = 0usize;
        for index in 0..case.records {
            let Some(payload) = mmap.block_payload_window::<PerfUser>(index)? else {
                panic!("missing mmap payload window for PerfUser index {index}");
            };
            total_len += payload.len();
        }
        assert!(total_len > case.records * 32);
        Ok(())
    })?;
    report("mmap payload scan", case.records, &path, elapsed);

    cleanup(&path);
    Ok(())
}

#[cfg(feature = "zero-copy")]
fn zero_copy_raw_fixed_reads(case: PerfCase) -> varve::Result<()> {
    let path = temp_path(&format!("{}_raw_fixed", case.name));
    cleanup(&path);

    {
        let mut file = PerfFormat::create(&path)?;
        for index in 0..case.records {
            file.push(&perf_raw_point(index))?;
        }
        file.flush()?;
    }

    let elapsed = timed(|| {
        let file = PerfFormat::open_readonly(&path)?;
        // SAFETY: The benchmark does not mutate the fixture while mapped.
        let mmap = unsafe { file.mmap_payloads()? };
        for index in 0..case.records {
            let Some(point) = unsafe { mmap.raw_fixed::<PerfRawPoint>(index) }? else {
                panic!("missing raw fixed block at index {index}");
            };
            assert_eq!(point.bytes[0], index as u8);
            assert_eq!(point.bytes[8], index.wrapping_mul(3) as u8);
        }
        assert!(unsafe { mmap.raw_fixed::<PerfRawPoint>(case.records) }?.is_none());
        Ok(())
    })?;
    report("zero-copy raw fixed", case.records, &path, elapsed);

    cleanup(&path);
    Ok(())
}

#[cfg(feature = "compression-zstd")]
fn compressed_variable_blocks(case: PerfCase) -> varve::Result<()> {
    let path = temp_path(&format!("{}_compressed", case.name));
    cleanup(&path);

    let elapsed = timed(|| {
        let mut file = PerfCompressedFormat::create(&path)?;
        for index in 0..case.records {
            file.push(&perf_compressible_user(index))?;
        }
        let compressed_records = file
            .index_entries()
            .iter()
            .filter(|entry| entry.block_id == PerfUser::ID && entry.is_compressed())
            .count();
        assert_eq!(compressed_records, case.records);
        file.flush()?;
        Ok(())
    })?;
    report("append compressed", case.records, &path, elapsed);

    let elapsed = timed(|| {
        let file = PerfCompressedFormat::open_readonly(&path)?;
        assert_eq!(file.scan().count(), case.records);
        let users = file.blocks::<PerfUser>()?;
        assert_eq!(users.len(), case.records);
        assert_eq!(
            users
                .get(case.records - 1)?
                .expect("last user")
                .payload
                .len(),
            512
        );
        Ok(())
    })?;
    report("open compressed", case.records, &path, elapsed);

    cleanup(&path);
    Ok(())
}

fn checkpoint_open(case: PerfCase) -> varve::Result<()> {
    let path = temp_path(&format!("{}_checkpoint", case.name));
    cleanup(&path);

    {
        let mut file = PerfCheckpointFormat::create(&path)?;
        for index in 0..case.records {
            file.push(&PerfPoint {
                x: index as u64,
                y: index as u64 + 1,
            })?;
        }
        file.flush()?;
    }

    let elapsed = timed(|| {
        let file = PerfCheckpointFormat::open_readonly(&path)?;
        assert!(file.index_entries().len() >= case.records);
        assert_eq!(file.blocks::<PerfPoint>()?.len(), case.records);
        Ok(())
    })?;
    report("checkpoint open", case.records, &path, elapsed);

    cleanup(&path);
    Ok(())
}

fn varve3_footer_and_chain(case: PerfCase) -> varve::Result<()> {
    let path = temp_path(&format!("{}_varve3_chain", case.name));
    cleanup(&path);
    let key_space = (case.records / 2).max(1);

    let elapsed = timed(|| {
        let mut writer = PerfVarve3Format::create_writer(&path)?;
        for index in 0..case.records {
            writer.push_perf_v3_user(&perf_v3_user(index % key_space, index))?;
        }
        writer.flush()?;
        Ok(())
    })?;
    report("append varve3 chain", case.records, &path, elapsed);

    let elapsed = timed(|| {
        let reader = PerfVarve3Format::open_reader(&path)?;
        let users = reader.perf_v3_users()?;
        assert_eq!(users.len(), key_space);
        assert_eq!(
            users
                .get(&((key_space - 1) as u64))?
                .expect("last keyed user")
                .payload
                .len(),
            64
        );
        Ok(())
    })?;
    report("open varve3 chain", case.records, &path, elapsed);

    cleanup(&path);
    Ok(())
}

fn materialized_keyed_state(case: PerfCase) -> varve::Result<()> {
    let path = temp_path(&format!("{}_keyed", case.name));
    cleanup(&path);
    let deletes = case.records / 8;
    let renames = case.records / 4;

    let elapsed = timed(|| {
        let mut file = PerfFormat::create(&path)?;
        for index in 0..case.records {
            file.push(&perf_user(index))?;
        }
        for index in 0..renames {
            file.push_op::<PerfUser>(
                &(index as u64),
                &PerfUserOp {
                    rename_to: format!("renamed-{index}"),
                },
            )?;
        }
        for index in 0..deletes {
            file.delete::<PerfUser>(&(index as u64))?;
        }
        file.flush()?;
        Ok(())
    })?;
    report(
        "append keyed+ops",
        case.records + renames + deletes,
        &path,
        elapsed,
    );

    let elapsed = timed(|| {
        let file = PerfFormat::open_readonly(&path)?;
        let state = file.materialized_keyed_blocks::<PerfUser>()?;
        assert_eq!(state.len(), case.records - deletes);
        assert!(!state.contains_key(&0));
        if renames > deletes {
            assert_eq!(
                state.get(&(deletes as u64)).map(|user| user.name.as_str()),
                Some(format!("renamed-{deletes}").as_str())
            );
        }
        Ok(())
    })?;
    report("materialized keyed", case.records, &path, elapsed);

    cleanup(&path);
    Ok(())
}

fn merge_and_compact(records: usize) -> varve::Result<()> {
    let base = temp_path("merge_base");
    let delta = temp_path("merge_delta");
    let merged = temp_path("merge_output");
    let compacted = temp_path("compact_output");
    let direct_compacted = temp_path("compact_direct_output");
    cleanup_many([&base, &delta, &merged, &compacted, &direct_compacted]);

    {
        let mut file = PerfFormat::create(&base)?;
        for index in 0..records {
            file.push(&perf_user(index))?;
        }
        file.flush()?;
    }
    {
        let mut file = PerfFormat::create(&delta)?;
        for index in 0..records / 3 {
            file.push_op::<PerfUser>(
                &(index as u64),
                &PerfUserOp {
                    rename_to: format!("delta-{index}"),
                },
            )?;
        }
        for index in records / 3..records / 2 {
            file.delete::<PerfUser>(&(index as u64))?;
        }
        for index in records..records + records / 4 {
            file.push(&perf_user(index))?;
        }
        file.flush()?;
    }

    let elapsed = timed(|| {
        merge_keyed_files::<PerfUser, _>(
            PerfFormat::spec(),
            base.as_path(),
            &[delta.as_path()],
            merged.as_path(),
        )
    })?;
    report("merge keyed files", records + records / 4, &merged, elapsed);

    let elapsed = timed(|| {
        compact_keyed_file::<PerfUser, _>(PerfFormat::spec(), merged.as_path(), compacted.as_path())
    })?;
    report(
        "compact keyed file",
        records + records / 4,
        &compacted,
        elapsed,
    );

    let elapsed = timed(|| {
        compact_keyed_files::<PerfUser, _>(
            PerfFormat::spec(),
            base.as_path(),
            &[delta.as_path()],
            direct_compacted.as_path(),
        )
    })?;
    report(
        "compact base+deltas",
        records + records / 4,
        &direct_compacted,
        elapsed,
    );

    let file = PerfFormat::open_readonly(&compacted)?;
    assert_eq!(
        file.materialized_keyed_blocks::<PerfUser>()?.len(),
        records - (records / 2 - records / 3) + records / 4
    );
    let file = PerfFormat::open_readonly(&direct_compacted)?;
    assert_eq!(
        file.materialized_keyed_blocks::<PerfUser>()?.len(),
        records - (records / 2 - records / 3) + records / 4
    );

    cleanup_many([&base, &delta, &merged, &compacted, &direct_compacted]);
    Ok(())
}

fn recovery_tail_truncation(records: usize) -> varve::Result<()> {
    let path = temp_path("recovery");
    cleanup(&path);

    {
        let mut file = PerfFormat::create(&path)?;
        for index in 0..records {
            file.push(&PerfPoint {
                x: index as u64,
                y: index as u64,
            })?;
        }
        file.flush()?;
    }
    let clean_len = metadata(&path)?.len();
    append_partial_record_header(&path)?;

    let recovering_spec = PerfFormat::spec().with_recovery_policy(RecoveryPolicy::TruncateTail);
    let elapsed = timed(|| {
        let (file, report) = recovering_spec.open_recover_with_report(&path)?;
        assert!(report.original_len > report.recovered_len);
        assert_eq!(report.recovered_len, clean_len);
        assert_eq!(report.records_preserved, records);
        assert_eq!(file.blocks::<PerfPoint>()?.len(), records);
        Ok(())
    })?;
    report("recover partial tail", records, &path, elapsed);

    cleanup(&path);
    Ok(())
}

fn matrix_direct_access(case: PerfCase) -> varve::Result<()> {
    matrix_direct_access_with_spec(case, perf_matrix_spec(), "matrix", false)
}

fn matrix_aux_region_access(case: PerfCase) -> varve::Result<()> {
    let path = temp_path(&format!("{}_matrix_aux", case.name));
    cleanup(&path);
    let spec = perf_matrix_aux_spec();
    let channels = 8usize;
    let scans = case.records.div_ceil(channels);
    let dims = MatrixDimensions::from_pairs([("scan", scans as u64), ("ch", channels as u64)]);
    let aux_len = spec.matrix_aux[0].byte_len;

    let mut writer = spec.create_writer_with_dims(&path, dims)?;
    let stable_len = metadata(&path)?.len();
    let elapsed = timed(|| {
        for index in 0..case.records {
            let offset = ((index * 4) as u64) % (aux_len - 4);
            writer.write_matrix_aux("scratch", offset, &(index as u32).to_le_bytes())?;
        }
        writer.flush()?;
        Ok(())
    })?;
    assert_eq!(metadata(&path)?.len(), stable_len);
    report("matrix aux write", case.records, &path, elapsed);

    let elapsed = timed(|| {
        for index in 0..case.records {
            let offset = ((index * 4) as u64) % (aux_len - 4);
            let payload = writer.read_matrix_aux("scratch", offset, 4)?;
            let _ = u32::from_le_bytes(payload.try_into().expect("matrix aux u32 payload"));
        }
        Ok(())
    })?;
    assert_eq!(metadata(&path)?.len(), stable_len);
    report("matrix aux read", case.records, &path, elapsed);

    cleanup(&path);
    Ok(())
}

#[cfg(feature = "integrity")]
fn matrix_crc_direct_access(case: PerfCase) -> varve::Result<()> {
    matrix_direct_access_with_spec(case, perf_matrix_crc_spec(), "matrix crc", true)
}

fn matrix_direct_access_with_spec(
    case: PerfCase,
    spec: FormatSpec,
    label_prefix: &str,
    include_rebuild: bool,
) -> varve::Result<()> {
    let path = temp_path(&format!("{}_matrix", case.name));
    cleanup(&path);
    let channels = 8usize;
    let scans = case.records.div_ceil(channels);
    let dims = MatrixDimensions::from_pairs([("scan", scans as u64), ("ch", channels as u64)]);

    let elapsed = timed(|| {
        let mut writer = spec.create_writer_with_dims(&path, dims.clone())?;
        for index in 0..case.records {
            let logical = (index * 37) % case.records;
            let key = MatrixKey::new((logical / channels) as u64, (logical % channels) as u64);
            writer.write_matrix_cell(
                key,
                &PerfMatrixCell {
                    value: logical as u32,
                },
            )?;
            writer.commit_matrix_cell::<PerfMatrixCell>(key)?;
        }
        writer.flush()?;
        Ok(())
    })?;
    report(
        &format!("{label_prefix} write+commit"),
        case.records,
        &path,
        elapsed,
    );
    let stable_len = metadata(&path)?.len();

    let elapsed = timed(|| {
        let mut reader = spec.open_reader(&path)?;
        for index in 0..case.records {
            let key = MatrixKey::new((index / channels) as u64, (index % channels) as u64);
            let cell = reader.read_matrix_cell::<PerfMatrixCell>(key)?;
            assert_eq!(cell.value, index as u32);
        }
        Ok(())
    })?;
    report(
        &format!("{label_prefix} direct read"),
        case.records,
        &path,
        elapsed,
    );

    #[cfg(feature = "mmap")]
    {
        let elapsed = timed(|| {
            let reader = spec.open_reader(&path)?;
            // SAFETY: The benchmark does not mutate the fixture while mapped.
            let mmap = unsafe { reader.mmap_matrix()? };
            for index in 0..case.records {
                let key = MatrixKey::new((index / channels) as u64, (index % channels) as u64);
                let payload = mmap.cell_payload_window::<PerfMatrixCell>(key)?;
                assert_eq!(
                    u32::from_le_bytes(payload.try_into().expect("matrix u32 payload")),
                    index as u32
                );
            }
            Ok(())
        })?;
        report(
            &format!("{label_prefix} mmap read"),
            case.records,
            &path,
            elapsed,
        );

        let elapsed = timed(|| {
            let reader = spec.open_reader(&path)?;
            // SAFETY: The benchmark does not mutate the fixture while mapped.
            let mmap = unsafe { reader.mmap_matrix()? };
            for index in 0..case.records {
                let key = MatrixKey::new((index / channels) as u64, (index % channels) as u64);
                let value = mmap.cell_numeric::<PerfMatrixCell, u32>(key)?;
                assert_eq!(value, index as u32);
            }
            Ok(())
        })?;
        report(
            &format!("{label_prefix} mmap numeric read"),
            case.records,
            &path,
            elapsed,
        );
    }

    let overwrite_count = case.records.min(1_024);
    let elapsed = timed(|| {
        let mut writer = spec.open_writer(&path)?;
        for index in 0..overwrite_count {
            let key = MatrixKey::new((index / channels) as u64, (index % channels) as u64);
            writer.write_matrix_cell(
                key,
                &PerfMatrixCell {
                    value: (index as u32).wrapping_add(1),
                },
            )?;
            writer.commit_matrix_cell::<PerfMatrixCell>(key)?;
        }
        writer.flush()?;
        Ok(())
    })?;
    assert_eq!(metadata(&path)?.len(), stable_len);
    report(
        &format!("{label_prefix} overwrite"),
        overwrite_count,
        &path,
        elapsed,
    );

    if include_rebuild {
        let elapsed = timed(|| {
            let mut writer = spec.open_writer(&path)?;
            assert_eq!(
                writer.rebuild_matrix_commit_from_crc::<PerfMatrixCell>()?,
                case.records as u64
            );
            writer.flush()?;
            Ok(())
        })?;
        report(
            &format!("{label_prefix} rebuild"),
            case.records,
            &path,
            elapsed,
        );
    }

    cleanup(&path);
    Ok(())
}

fn perf_user(index: usize) -> PerfUser {
    PerfUser {
        user_id: index as u64,
        region: (index % 16) as u16,
        name: format!("user-{index}"),
        payload: vec![(index % 251) as u8; 32],
    }
}

fn perf_v3_user(key: usize, index: usize) -> PerfV3User {
    PerfV3User {
        user_id: key as u64,
        name: format!("v3-user-{index}"),
        payload: vec![(index % 251) as u8; 64],
    }
}

#[cfg(feature = "compression-zstd")]
fn perf_compressible_user(index: usize) -> PerfUser {
    PerfUser {
        user_id: index as u64,
        region: (index % 16) as u16,
        name: format!("compressible-{index}"),
        payload: vec![b'Z'; 512],
    }
}

#[cfg(feature = "zero-copy")]
fn perf_raw_point(index: usize) -> PerfRawPoint {
    let mut bytes = [0u8; 16];
    bytes[..8].copy_from_slice(&(index as u64).to_le_bytes());
    bytes[8..].copy_from_slice(&((index as u64).wrapping_mul(3)).to_le_bytes());
    PerfRawPoint { bytes }
}

fn timed(operation: impl FnOnce() -> varve::Result<()>) -> varve::Result<Duration> {
    let start = Instant::now();
    operation()?;
    Ok(start.elapsed())
}

fn report(label: &str, records: usize, path: &Path, elapsed: Duration) {
    let seconds = elapsed.as_secs_f64();
    let records_per_sec = if seconds > 0.0 {
        records as f64 / seconds
    } else {
        f64::INFINITY
    };
    let bytes = metadata(path).map(|metadata| metadata.len()).unwrap_or(0);
    println!(
        "{label:>22}: {:>8.3} ms | {:>12.0} records/sec | {:>10} bytes",
        seconds * 1_000.0,
        records_per_sec,
        bytes
    );
}

fn append_partial_record_header(path: &Path) -> varve::Result<()> {
    let mut file = OpenOptions::new().append(true).open(path)?;
    file.write_all(&[0xAA, 0xBB, 0xCC, 0xDD])?;
    Ok(())
}

fn perf_layout_segment_info(raw_offset: u64, raw_len: u64) -> LayoutSegmentInfo {
    LayoutSegmentInfo {
        name: "PerfAdapterSegment",
        segment_start: raw_offset,
        lead_in_len: 0,
        metadata_offset: raw_offset,
        metadata_len: 0,
        raw_offset,
        raw_len,
        footer_offset: raw_offset + raw_len,
        footer_len: 0,
        segment_end: raw_offset + raw_len,
        fields: Vec::new(),
        footer_fields: Vec::new(),
    }
}

fn temp_path(name: &str) -> PathBuf {
    let mut path = std::env::temp_dir();
    path.push(format!(
        "varve_perf_smoke_{name}_{}_{}.vrv",
        std::process::id(),
        std::thread::current().name().unwrap_or("anon")
    ));
    path
}

fn cleanup_many<'a>(paths: impl IntoIterator<Item = &'a PathBuf>) {
    for path in paths {
        cleanup(path);
    }
}

fn cleanup(path: &PathBuf) {
    let _ = remove_file(path);
    let mut lock = path.as_os_str().to_os_string();
    lock.push(".lock");
    let _ = remove_file(PathBuf::from(lock));
}

#[derive(Clone, Copy)]
struct PerfCase {
    name: &'static str,
    records: usize,
}

impl PerfCase {
    fn new(name: &'static str, records: usize) -> Self {
        Self { name, records }
    }
}
