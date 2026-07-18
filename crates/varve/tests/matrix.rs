use std::fs::{File, OpenOptions, metadata, remove_file};
use std::io::Read;
use std::io::{Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use varve::{
    BlockDescriptor, BlockKind, Endian, Error, FormatSpec, IndexPolicy, MatrixAuxDescriptor,
    MatrixBlockDescriptor, MatrixCellStatus, MatrixCommitDescriptor, MatrixCommitKind,
    MatrixDimensionDescriptor, MatrixDimensions, MatrixDurabilityBarrier, MatrixKey,
    MatrixResumeSignal, PackedBitmap, ReadLimits, VarveBlock, VarveDecode, VarveEncode,
    VarveMatrixBlock, varve_format,
};
#[cfg(feature = "integrity")]
use varve::{MatrixCorruptionKind, MatrixCorruptionSeverity};

varve_format! {
    pub format GeneratedMatrixFormat {
        magic: b"GMTX";
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
        dims {
            scan: u32,
            ch: u32,
        }
        commit: cell_bitmap {
            keyspace = [scan, ch];
            categories = [analysis];
            singles = [master_grid];
            per_channel = [threshold];
        };
        aux {
            thumbnail: 16,
        }
        blocks {
            matrix GeneratedCell(id = 300, dims = [scan, ch], category = analysis) {
                value: u32,
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct MatrixCell {
    value: u32,
}

impl VarveEncode for MatrixCell {
    const WIRE_TYPE: varve::WireType = varve::WireType::Nested;

    fn encode_varve(&self, encoder: &mut varve::Encoder) -> varve::Result<()> {
        self.value.encode_varve(encoder)
    }
}

impl VarveDecode for MatrixCell {
    const WIRE_TYPE: varve::WireType = varve::WireType::Nested;

    fn decode_varve(decoder: &mut varve::Decoder<'_>) -> varve::Result<Self> {
        Ok(Self {
            value: u32::decode_varve(decoder)?,
        })
    }
}

impl VarveBlock for MatrixCell {
    const ID: u32 = 100;
    const VERSION: u16 = 1;
    const KIND: BlockKind = BlockKind::Matrix;
    const ENDIAN: Option<Endian> = None;
    const SCHEMA_FINGERPRINT: u64 = 0x6295E4AA349AD0C2;
    const IS_KEYED: bool = false;
}

impl VarveMatrixBlock for MatrixCell {
    const DIMENSIONS: [&'static str; 2] = ["scan", "ch"];
    const CATEGORY: &'static str = "analysis";
    const SLOT_STRIDE: u64 = 4;
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct BadSizedCell {
    value: u64,
}

impl VarveEncode for BadSizedCell {
    const WIRE_TYPE: varve::WireType = varve::WireType::Nested;

    fn encode_varve(&self, encoder: &mut varve::Encoder) -> varve::Result<()> {
        self.value.encode_varve(encoder)
    }
}

impl VarveDecode for BadSizedCell {
    const WIRE_TYPE: varve::WireType = varve::WireType::Nested;

    fn decode_varve(decoder: &mut varve::Decoder<'_>) -> varve::Result<Self> {
        Ok(Self {
            value: u64::decode_varve(decoder)?,
        })
    }
}

impl VarveBlock for BadSizedCell {
    const ID: u32 = MatrixCell::ID;
    const VERSION: u16 = MatrixCell::VERSION;
    const KIND: BlockKind = BlockKind::Matrix;
    const ENDIAN: Option<Endian> = None;
    const SCHEMA_FINGERPRINT: u64 = 0x6BED1E9E5DD7FAFD;
    const IS_KEYED: bool = false;
}

impl VarveMatrixBlock for BadSizedCell {
    const DIMENSIONS: [&'static str; 2] = MatrixCell::DIMENSIONS;
    const CATEGORY: &'static str = MatrixCell::CATEGORY;
    const SLOT_STRIDE: u64 = MatrixCell::SLOT_STRIDE;
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct OtherMatrixCell {
    value: u32,
}

impl VarveEncode for OtherMatrixCell {
    const WIRE_TYPE: varve::WireType = varve::WireType::Nested;

    fn encode_varve(&self, encoder: &mut varve::Encoder) -> varve::Result<()> {
        self.value.encode_varve(encoder)
    }
}

impl VarveDecode for OtherMatrixCell {
    const WIRE_TYPE: varve::WireType = varve::WireType::Nested;

    fn decode_varve(decoder: &mut varve::Decoder<'_>) -> varve::Result<Self> {
        Ok(Self {
            value: u32::decode_varve(decoder)?,
        })
    }
}

impl VarveBlock for OtherMatrixCell {
    const ID: u32 = 101;
    const VERSION: u16 = 1;
    const KIND: BlockKind = BlockKind::Matrix;
    const ENDIAN: Option<Endian> = None;
    const SCHEMA_FINGERPRINT: u64 = 0xD1E2147D120E3576;
    const IS_KEYED: bool = false;
}

impl VarveMatrixBlock for OtherMatrixCell {
    const DIMENSIONS: [&'static str; 2] = MatrixCell::DIMENSIONS;
    const CATEGORY: &'static str = MatrixCell::CATEGORY;
    const SLOT_STRIDE: u64 = MatrixCell::SLOT_STRIDE;
}

#[derive(Clone, Debug, PartialEq, Eq, VarveBlock)]
#[varve(id = 200, version = 1, kind = "fixed")]
struct LogPoint {
    value: u32,
}

#[cfg(feature = "zero-copy")]
#[repr(C)]
#[derive(
    Clone,
    Copy,
    Debug,
    PartialEq,
    Eq,
    VarveBlock,
    varve::zerocopy::FromBytes,
    varve::zerocopy::Immutable,
    varve::zerocopy::KnownLayout,
)]
#[varve(id = 250, version = 1, kind = "matrix")]
struct RawMatrixCell {
    bytes: [u8; 4],
}

#[cfg(feature = "zero-copy")]
impl VarveMatrixBlock for RawMatrixCell {
    const DIMENSIONS: [&'static str; 2] = ["scan", "ch"];
    const CATEGORY: &'static str = "raw";
    const SLOT_STRIDE: u64 = 4;
}

#[cfg(feature = "zero-copy")]
unsafe impl varve::VarveRawMatrixBlock for RawMatrixCell {
    const RAW_ENDIAN: Endian = Endian::Little;
}

fn matrix_spec() -> FormatSpec {
    matrix_spec_with_integrity(varve::IntegrityPolicy::None)
}

fn matrix_aux_spec() -> FormatSpec {
    static AUX: &[MatrixAuxDescriptor] = &[MatrixAuxDescriptor {
        name: "thumbnail",
        byte_len: 16,
    }];
    matrix_spec().with_matrix_aux(AUX)
}

fn other_matrix_spec() -> FormatSpec {
    static BLOCKS: &[BlockDescriptor] = &[BlockDescriptor {
        id: OtherMatrixCell::ID,
        name: "OtherMatrixCell",
        version: OtherMatrixCell::VERSION,
        kind: BlockKind::Matrix,
        fields: &[],
    }];
    static DIMS: &[MatrixDimensionDescriptor] = &[
        MatrixDimensionDescriptor { name: "scan" },
        MatrixDimensionDescriptor { name: "ch" },
    ];
    static COMMITS: &[MatrixCommitDescriptor] = &[MatrixCommitDescriptor {
        name: OtherMatrixCell::CATEGORY,
        kind: MatrixCommitKind::Cell,
    }];
    static MATRIX_BLOCKS: &[MatrixBlockDescriptor] = &[MatrixBlockDescriptor {
        block_id: OtherMatrixCell::ID,
        dimensions: OtherMatrixCell::DIMENSIONS,
        category: OtherMatrixCell::CATEGORY,
        slot_stride: OtherMatrixCell::SLOT_STRIDE,
    }];
    FormatSpec::new(
        b"OMTX",
        1,
        Endian::Little,
        0,
        IndexPolicy::ScanOnOpen,
        varve::IntegrityPolicy::None,
        varve::RecoveryPolicy::Strict,
        varve::ManifestPolicy::None,
        BLOCKS,
    )
    .with_matrix_spec(DIMS, COMMITS, MATRIX_BLOCKS)
    .with_read_limits(ReadLimits::finite_all(u64::MAX))
}

#[test]
fn matrix_aux_declaration_order_changes_schema_hash() {
    static AUX_AB: &[MatrixAuxDescriptor] = &[
        MatrixAuxDescriptor {
            name: "a",
            byte_len: 8,
        },
        MatrixAuxDescriptor {
            name: "b",
            byte_len: 16,
        },
    ];
    static AUX_BA: &[MatrixAuxDescriptor] = &[
        MatrixAuxDescriptor {
            name: "b",
            byte_len: 16,
        },
        MatrixAuxDescriptor {
            name: "a",
            byte_len: 8,
        },
    ];

    assert_ne!(
        matrix_spec().with_matrix_aux(AUX_AB).computed_schema_hash(),
        matrix_spec().with_matrix_aux(AUX_BA).computed_schema_hash()
    );
}

fn matrix_spec_with_integrity(integrity: varve::IntegrityPolicy) -> FormatSpec {
    static BLOCKS: &[BlockDescriptor] = &[
        BlockDescriptor {
            id: MatrixCell::ID,
            name: "MatrixCell",
            version: MatrixCell::VERSION,
            kind: BlockKind::Matrix,
            fields: &[],
        },
        BlockDescriptor {
            id: LogPoint::ID,
            name: "LogPoint",
            version: LogPoint::VERSION,
            kind: BlockKind::Fixed,
            fields: LogPoint::FIELDS,
        },
    ];
    static DIMS: &[MatrixDimensionDescriptor] = &[
        MatrixDimensionDescriptor { name: "scan" },
        MatrixDimensionDescriptor { name: "ch" },
    ];
    static COMMITS: &[MatrixCommitDescriptor] = &[
        MatrixCommitDescriptor {
            name: "analysis",
            kind: MatrixCommitKind::Cell,
        },
        MatrixCommitDescriptor {
            name: "master_grid",
            kind: MatrixCommitKind::Single,
        },
        MatrixCommitDescriptor {
            name: "threshold",
            kind: MatrixCommitKind::PerChannel,
        },
    ];
    static MATRIX_BLOCKS: &[MatrixBlockDescriptor] = &[MatrixBlockDescriptor {
        block_id: MatrixCell::ID,
        dimensions: MatrixCell::DIMENSIONS,
        category: MatrixCell::CATEGORY,
        slot_stride: MatrixCell::SLOT_STRIDE,
    }];
    FormatSpec::new(
        b"MTX",
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

#[cfg(feature = "zero-copy")]
fn raw_matrix_spec() -> FormatSpec {
    static BLOCKS: &[BlockDescriptor] = &[BlockDescriptor {
        id: RawMatrixCell::ID,
        name: "RawMatrixCell",
        version: RawMatrixCell::VERSION,
        kind: BlockKind::Matrix,
        fields: RawMatrixCell::FIELDS,
    }];
    static DIMS: &[MatrixDimensionDescriptor] = &[
        MatrixDimensionDescriptor { name: "scan" },
        MatrixDimensionDescriptor { name: "ch" },
    ];
    static COMMITS: &[MatrixCommitDescriptor] = &[MatrixCommitDescriptor {
        name: RawMatrixCell::CATEGORY,
        kind: MatrixCommitKind::Cell,
    }];
    static MATRIX_BLOCKS: &[MatrixBlockDescriptor] = &[MatrixBlockDescriptor {
        block_id: RawMatrixCell::ID,
        dimensions: RawMatrixCell::DIMENSIONS,
        category: RawMatrixCell::CATEGORY,
        slot_stride: RawMatrixCell::SLOT_STRIDE,
    }];
    FormatSpec::new(
        b"RMTX",
        1,
        Endian::Little,
        0,
        IndexPolicy::ScanOnOpen,
        varve::IntegrityPolicy::None,
        varve::RecoveryPolicy::Strict,
        varve::ManifestPolicy::None,
        BLOCKS,
    )
    .with_matrix_spec(DIMS, COMMITS, MATRIX_BLOCKS)
    .with_read_limits(ReadLimits::finite_all(u64::MAX))
}

#[cfg(feature = "integrity")]
fn matrix_crc_spec() -> FormatSpec {
    matrix_spec_with_integrity(varve::IntegrityPolicy::Crc32)
}

struct RecordingMatrixBarrier {
    events: Arc<Mutex<Vec<&'static str>>>,
    commit_map_off: u64,
}

impl RecordingMatrixBarrier {
    fn new(events: Arc<Mutex<Vec<&'static str>>>, commit_map_off: u64) -> Self {
        Self {
            events,
            commit_map_off,
        }
    }

    fn commit_byte(&self, file: &mut File) -> varve::Result<u8> {
        let cursor = file.stream_position()?;
        file.seek(SeekFrom::Start(self.commit_map_off))?;
        let mut byte = [0; 1];
        file.read_exact(&mut byte)?;
        file.seek(SeekFrom::Start(cursor))?;
        Ok(byte[0])
    }
}

impl MatrixDurabilityBarrier for RecordingMatrixBarrier {
    fn sync_matrix_data(&mut self, file: &mut File) -> varve::Result<()> {
        assert_eq!(self.commit_byte(file)? & 0b0000_0001, 0);
        self.events.lock().expect("event mutex").push("data_sync");
        Ok(())
    }

    fn sync_matrix_commit(&mut self, file: &mut File) -> varve::Result<()> {
        assert_eq!(self.commit_byte(file)? & 0b0000_0001, 0b0000_0001);
        self.events.lock().expect("event mutex").push("commit_sync");
        Ok(())
    }
}

#[test]
fn matrix_random_order_write_read_commit_and_append_log_coexist() -> varve::Result<()> {
    let path = temp_path("matrix_p0");
    cleanup(&path);
    let spec = matrix_spec();

    assert!(matches!(
        spec.create(&path),
        Err(Error::MatrixDimensionsRequired)
    ));

    {
        let dims = MatrixDimensions::from_pairs([("scan", 3), ("ch", 2)]);
        let mut writer = spec.create_writer_with_dims(&path, dims)?;
        let key_a = MatrixKey::new(2, 1);
        let key_b = MatrixKey::new(0, 0);
        let key_zero = MatrixKey::new(1, 1);
        assert!(matches!(
            writer.commit_matrix_cell::<MatrixCell>(key_a),
            Err(Error::MatrixCellNotWritten)
        ));
        writer.write_matrix_cell(key_a, &MatrixCell { value: 21 })?;
        writer.write_matrix_cell(key_b, &MatrixCell { value: 1 })?;
        writer.write_matrix_cell(key_zero, &MatrixCell { value: 0 })?;
        writer.commit_matrix_cell::<MatrixCell>(key_a)?;
        writer.commit_matrix_cell::<MatrixCell>(key_b)?;
        writer.commit_matrix_cell::<MatrixCell>(key_zero)?;
        writer.write_matrix_cell(MatrixKey::new(1, 0), &MatrixCell { value: 10 })?;
        writer.set_matrix_single_committed("master_grid", true)?;
        writer.set_matrix_channel_committed("threshold", 1, true)?;
        writer.push_info(&LogPoint { value: 99 })?;
        writer.flush()?;
    }

    let file_len = metadata(&path)?.len();
    {
        let mut reader = spec.open_reader(&path)?;
        assert_eq!(
            reader.matrix_cell_status::<MatrixCell>(MatrixKey::new(2, 1))?,
            MatrixCellStatus::Committed
        );
        assert_eq!(
            reader.read_matrix_cell::<MatrixCell>(MatrixKey::new(2, 1))?,
            MatrixCell { value: 21 }
        );
        assert_eq!(
            reader.read_matrix_cell::<MatrixCell>(MatrixKey::new(1, 1))?,
            MatrixCell { value: 0 }
        );
        assert_eq!(
            reader.matrix_cell_status::<MatrixCell>(MatrixKey::new(1, 0))?,
            MatrixCellStatus::NotCommitted
        );
        assert!(matches!(
            reader.read_matrix_cell::<MatrixCell>(MatrixKey::new(1, 0)),
            Err(Error::MatrixNotCommitted)
        ));
        assert!(reader.is_matrix_single_committed("master_grid")?);
        assert!(reader.is_matrix_channel_committed("threshold", 1)?);
        assert!(!reader.is_matrix_channel_committed("threshold", 0)?);
        assert_eq!(
            reader.blocks::<LogPoint>()?.get(0)?,
            Some(LogPoint { value: 99 })
        );
    }

    {
        let mut writer = spec.open_writer(&path)?;
        writer.write_matrix_cell(MatrixKey::new(2, 1), &MatrixCell { value: 22 })?;
        writer.commit_matrix_cell::<MatrixCell>(MatrixKey::new(2, 1))?;
        assert!(matches!(
            writer.write_matrix_cell(MatrixKey::new(0, 1), &BadSizedCell { value: 7 }),
            Err(Error::MatrixSizeMismatch {
                expected: 4,
                actual: 8
            })
        ));
        writer.clear_matrix_cell::<MatrixCell>(MatrixKey::new(0, 0))?;
        writer.flush()?;
    }
    assert_eq!(metadata(&path)?.len(), file_len);

    {
        let mut writer = spec.open_writer(&path)?;
        writer.commit_matrix_cell::<MatrixCell>(MatrixKey::new(0, 0))?;
        writer.flush()?;
    }

    {
        let mut reader = spec.open_reader(&path)?;
        assert_eq!(
            reader.read_matrix_cell::<MatrixCell>(MatrixKey::new(2, 1))?,
            MatrixCell { value: 22 }
        );
        assert_eq!(
            reader.matrix_cell_status::<MatrixCell>(MatrixKey::new(0, 0))?,
            MatrixCellStatus::Committed
        );
    }

    cleanup(&path);
    Ok(())
}

#[test]
fn matrix_spec_rejects_duplicate_cell_categories() {
    static BLOCKS: &[BlockDescriptor] = &[
        BlockDescriptor {
            id: MatrixCell::ID,
            name: "MatrixCell",
            version: MatrixCell::VERSION,
            kind: BlockKind::Matrix,
            fields: &[],
        },
        BlockDescriptor {
            id: OtherMatrixCell::ID,
            name: "OtherMatrixCell",
            version: OtherMatrixCell::VERSION,
            kind: BlockKind::Matrix,
            fields: &[],
        },
    ];
    static DIMS: &[MatrixDimensionDescriptor] = &[
        MatrixDimensionDescriptor { name: "scan" },
        MatrixDimensionDescriptor { name: "ch" },
    ];
    static COMMITS: &[MatrixCommitDescriptor] = &[MatrixCommitDescriptor {
        name: MatrixCell::CATEGORY,
        kind: MatrixCommitKind::Cell,
    }];
    static MATRIX_BLOCKS: &[MatrixBlockDescriptor] = &[
        MatrixBlockDescriptor {
            block_id: MatrixCell::ID,
            dimensions: MatrixCell::DIMENSIONS,
            category: MatrixCell::CATEGORY,
            slot_stride: MatrixCell::SLOT_STRIDE,
        },
        MatrixBlockDescriptor {
            block_id: OtherMatrixCell::ID,
            dimensions: OtherMatrixCell::DIMENSIONS,
            category: OtherMatrixCell::CATEGORY,
            slot_stride: OtherMatrixCell::SLOT_STRIDE,
        },
    ];
    let spec = FormatSpec::new(
        b"DUPM",
        1,
        Endian::Little,
        0,
        IndexPolicy::ScanOnOpen,
        varve::IntegrityPolicy::None,
        varve::RecoveryPolicy::Strict,
        varve::ManifestPolicy::None,
        BLOCKS,
    )
    .with_matrix_spec(DIMS, COMMITS, MATRIX_BLOCKS);
    assert!(matches!(
        spec.validate(),
        Err(Error::InvalidFormatSpec(
            "matrix cell commit category must be unique per block"
        ))
    ));
}

#[test]
fn format_first_matrix_dsl_generates_typed_api() -> varve::Result<()> {
    let path = temp_path("matrix_dsl");
    cleanup(&path);

    {
        let mut writer = GeneratedMatrixFormat::create_writer_with_dims(
            &path,
            GeneratedMatrixFormatDims { scan: 2, ch: 2 },
        )?;
        let key = GeneratedCellKey { scan: 1, ch: 0 };
        assert_eq!(writer.thumbnail_aux_len()?, 16);
        writer.write_thumbnail_aux(2, &[5, 6, 7])?;
        assert_eq!(writer.read_thumbnail_aux(0, 6)?, vec![0, 0, 5, 6, 7, 0]);
        writer.write_generated_cell(key, &GeneratedCell { value: 42 })?;
        assert_eq!(
            writer.generated_cell_status(GeneratedCellKey { scan: 0, ch: 0 })?,
            MatrixCellStatus::NotCommitted
        );
        writer.commit_generated_cell(key)?;
        writer.set_master_grid_committed(true)?;
        writer.set_threshold_committed(1, true)?;
        writer.flush()?;
    }

    {
        let mut reader = GeneratedMatrixFormat::open_reader(&path)?;
        let key = GeneratedCellKey { scan: 1, ch: 0 };
        assert_eq!(reader.thumbnail_aux_len()?, 16);
        assert_eq!(reader.read_thumbnail_aux(2, 3)?, vec![5, 6, 7]);
        assert_eq!(
            reader.generated_cell_status(key)?,
            MatrixCellStatus::Committed
        );
        assert_eq!(reader.generated_cell(key)?, GeneratedCell { value: 42 });
        assert!(reader.is_master_grid_committed()?);
        assert!(reader.is_threshold_committed(1)?);
        assert!(!reader.is_threshold_committed(0)?);
    }

    cleanup(&path);
    Ok(())
}

#[test]
fn matrix_p1_p2_extension_points_are_usable() -> varve::Result<()> {
    let path = temp_path("matrix_extensions");
    cleanup(&path);
    let spec = matrix_spec();
    let events = Arc::new(Mutex::new(Vec::new()));

    {
        let dims = MatrixDimensions::from_pairs([("scan", 2), ("ch", 2)]);
        let mut writer = spec.create_writer_with_dims(&path, dims)?;
        let events_for_hook = Arc::clone(&events);
        writer.write_matrix_cell_durable(
            MatrixKey::new(1, 1),
            &MatrixCell { value: 7 },
            move |event| {
                events_for_hook.lock().expect("event mutex").push((
                    event.block_id,
                    event.key.scan,
                    event.key.ch,
                    event.slot_len,
                ));
                Ok(())
            },
        )?;
        let failed = writer.write_matrix_cell_durable(
            MatrixKey::new(0, 1),
            &MatrixCell { value: 3 },
            |_event| Err(Error::InvalidFormatSpec("hook failed")),
        );
        assert!(matches!(
            failed,
            Err(Error::InvalidFormatSpec("hook failed"))
        ));
        writer.write_matrix_cell(MatrixKey::new(0, 0), &MatrixCell { value: 1 })?;
        writer.commit_matrix_cell::<MatrixCell>(MatrixKey::new(0, 0))?;
        writer.flush()?;
    }

    assert_eq!(
        events.lock().expect("event mutex").as_slice(),
        &[(MatrixCell::ID, 1, 1, 4)]
    );

    {
        let mut reader = spec.open_reader(&path)?;
        assert_eq!(
            reader.matrix_resume_signal("analysis")?,
            MatrixResumeSignal::Partial {
                committed: 3,
                total: 4
            }
        );
        let sidecar = path.with_extension("sidecar");
        let _ = remove_file(&sidecar);
        assert_eq!(
            reader.matrix_sidecar_resume_signal("analysis", &sidecar)?,
            MatrixResumeSignal::RestartRecommended
        );
        std::fs::write(&sidecar, b"partial-progress")?;
        assert_eq!(
            reader.matrix_sidecar_resume_signal("analysis", &sidecar)?,
            MatrixResumeSignal::ResumeAvailable {
                committed: 3,
                total: 4
            }
        );
        remove_file(&sidecar)?;
        let report = reader.matrix_recovery_report();
        assert!(!report.findings.is_empty());
        assert_eq!(
            reader.matrix_cell_payload::<MatrixCell>(MatrixKey::new(1, 1))?,
            7u32.to_le_bytes()
        );
        assert_eq!(
            reader.read_matrix_cell::<MatrixCell>(MatrixKey::new(0, 1))?,
            MatrixCell { value: 3 }
        );
    }

    let mut bitmap = PackedBitmap::new(10)?;
    bitmap.set(0, true)?;
    bitmap.set(9, true)?;
    assert!(bitmap.get(0)?);
    assert!(!bitmap.get(1)?);
    assert!(bitmap.get(9)?);
    let encoded = varve::encode_to_vec(&bitmap, Endian::Little)?;
    let decoded: PackedBitmap = varve::decode_from_slice(&encoded, Endian::Little)?;
    assert_eq!(decoded, bitmap);

    cleanup(&path);
    Ok(())
}

#[test]
fn matrix_durable_barrier_observes_data_then_commit_sync_then_hook() -> varve::Result<()> {
    let path = temp_path("matrix_durable_barrier");
    cleanup(&path);
    let spec = matrix_spec();
    let events = Arc::new(Mutex::new(Vec::new()));

    {
        let dims = MatrixDimensions::from_pairs([("scan", 2), ("ch", 2)]);
        let mut writer = spec.create_writer_with_dims(&path, dims)?;
        let offsets = read_vmat_offsets(&path, spec)?;
        let mut barrier = RecordingMatrixBarrier::new(Arc::clone(&events), offsets.commit_map_off);
        let events_for_hook = Arc::clone(&events);
        writer.write_matrix_cell_durable_with_barrier(
            MatrixKey::new(0, 0),
            &MatrixCell { value: 11 },
            &mut barrier,
            move |event| {
                assert_eq!(event.block_id, MatrixCell::ID);
                assert_eq!(event.key, MatrixKey::new(0, 0));
                events_for_hook.lock().expect("event mutex").push("hook");
                Ok(())
            },
        )?;
    }

    assert_eq!(
        events.lock().expect("event mutex").as_slice(),
        &["data_sync", "commit_sync", "hook"]
    );

    {
        let mut reader = spec.open_reader(&path)?;
        assert_eq!(
            reader.matrix_cell_status::<MatrixCell>(MatrixKey::new(0, 0))?,
            MatrixCellStatus::Committed
        );
        assert_eq!(
            reader.read_matrix_cell::<MatrixCell>(MatrixKey::new(0, 0))?,
            MatrixCell { value: 11 }
        );
    }

    cleanup(&path);
    Ok(())
}

#[test]
fn matrix_aux_region_is_preallocated_noncommit_storage() -> varve::Result<()> {
    let path = temp_path("matrix_aux");
    cleanup(&path);
    let spec = matrix_aux_spec();

    let len_after_create;
    {
        let dims = MatrixDimensions::from_pairs([("scan", 2), ("ch", 2)]);
        let mut writer = spec.create_writer_with_dims(&path, dims)?;
        assert_eq!(writer.matrix_aux_len("thumbnail")?, 16);
        assert_eq!(writer.read_matrix_aux("thumbnail", 0, 16)?, vec![0; 16]);
        len_after_create = metadata(&path)?.len();

        writer.write_matrix_aux("thumbnail", 4, &[1, 2, 3, 4])?;
        writer.flush()?;
        assert_eq!(metadata(&path)?.len(), len_after_create);
        assert_eq!(
            writer.read_matrix_aux("thumbnail", 0, 8)?,
            vec![0, 0, 0, 0, 1, 2, 3, 4]
        );
        assert!(matches!(
            writer.write_matrix_aux("thumbnail", 15, &[9, 10]),
            Err(Error::MatrixAuxOutOfBounds { .. })
        ));

        writer.push(&LogPoint { value: 99 })?;
        writer.flush()?;
        assert!(metadata(&path)?.len() > len_after_create);
    }

    {
        let mut reader = spec.open_reader(&path)?;
        assert_eq!(reader.matrix_aux_len("thumbnail")?, 16);
        assert_eq!(reader.read_matrix_aux("thumbnail", 4, 4)?, vec![1, 2, 3, 4]);
        assert!(matches!(
            reader.read_matrix_aux("missing", 0, 1),
            Err(Error::MatrixAuxMissing(name)) if name == "missing"
        ));
    }

    cleanup(&path);
    Ok(())
}

#[test]
fn matrix_recovery_clear_category_and_cell_actions_are_applied() -> varve::Result<()> {
    let path = temp_path("matrix_recovery_actions");
    cleanup(&path);
    let spec = matrix_spec();

    {
        let dims = MatrixDimensions::from_pairs([("scan", 2), ("ch", 2)]);
        let mut writer = spec.create_writer_with_dims(&path, dims)?;
        for key in [MatrixKey::new(0, 0), MatrixKey::new(1, 1)] {
            writer.write_matrix_cell(key, &MatrixCell { value: 10 })?;
            writer.commit_matrix_cell::<MatrixCell>(key)?;
        }
        assert_eq!(writer.clear_matrix_category("analysis")?, 2);
        assert_eq!(
            writer.matrix_cell_status::<MatrixCell>(MatrixKey::new(0, 0))?,
            MatrixCellStatus::NotCommitted
        );

        let key = MatrixKey::new(0, 1);
        writer.write_matrix_cell(key, &MatrixCell { value: 20 })?;
        writer.commit_matrix_cell::<MatrixCell>(key)?;
        writer.apply_matrix_recovery_action(&varve::MatrixRecoveryAction::ClearCell {
            category: "analysis".to_string(),
            key,
        })?;
        assert_eq!(
            writer.matrix_cell_status::<MatrixCell>(key)?,
            MatrixCellStatus::NotCommitted
        );
        writer.flush()?;
    }

    {
        let reader = spec.open_reader(&path)?;
        assert_eq!(
            reader.matrix_resume_signal("analysis")?,
            MatrixResumeSignal::Clean
        );
    }

    cleanup(&path);
    Ok(())
}

#[test]
fn matrix_byte_copy_migration_scaffold_copies_compatible_committed_slot() -> varve::Result<()> {
    let source_path = temp_path("matrix_migration_source");
    let target_path = temp_path("matrix_migration_target");
    cleanup(&source_path);
    cleanup(&target_path);
    let source_spec = matrix_spec();
    let target_spec = other_matrix_spec();
    let dims = MatrixDimensions::from_pairs([("scan", 2), ("ch", 2)]);
    let key = MatrixKey::new(1, 0);

    {
        let mut writer = source_spec.create_writer_with_dims(&source_path, dims.clone())?;
        writer.write_matrix_cell(key, &MatrixCell { value: 123 })?;
        writer.commit_matrix_cell::<MatrixCell>(key)?;
        writer.flush()?;
    }

    {
        let mut reader = source_spec.open_reader(&source_path)?;
        let mut writer = target_spec.create_writer_with_dims(&target_path, dims)?;
        writer.copy_matrix_cell_bytes_from::<MatrixCell, OtherMatrixCell>(&mut reader, key)?;
        writer.flush()?;
    }

    {
        let mut reader = target_spec.open_reader(&target_path)?;
        assert_eq!(
            reader.read_matrix_cell::<OtherMatrixCell>(key)?,
            OtherMatrixCell { value: 123 }
        );
    }

    cleanup(&source_path);
    cleanup(&target_path);
    Ok(())
}

#[cfg(feature = "integrity")]
#[test]
fn matrix_verified_sidecar_checks_parent_identity_and_payload_crc() -> varve::Result<()> {
    let path = temp_path("matrix_sidecar_manifest");
    let sidecar = path.with_extension("sidecar");
    cleanup(&path);
    let _ = remove_file(&sidecar);
    let spec = matrix_crc_spec();

    {
        let dims = MatrixDimensions::from_pairs([("scan", 2), ("ch", 2)]);
        let mut writer = spec.create_writer_with_dims(&path, dims)?;
        writer.write_matrix_cell(MatrixKey::new(0, 0), &MatrixCell { value: 1 })?;
        writer.commit_matrix_cell::<MatrixCell>(MatrixKey::new(0, 0))?;
        let manifest = writer.write_matrix_sidecar("analysis", &sidecar, 7, b"resume-state-v1")?;
        assert_eq!(manifest.category, "analysis");
        assert_eq!(manifest.generation, 7);
        assert_eq!(manifest.payload_len, b"resume-state-v1".len() as u64);
        writer.flush()?;
    }

    {
        let reader = spec.open_reader(&path)?;
        assert_eq!(
            reader.matrix_verified_sidecar_resume_signal("analysis", &sidecar)?,
            MatrixResumeSignal::ResumeAvailable {
                committed: 1,
                total: 4
            }
        );
        assert_eq!(
            reader
                .matrix_verified_sidecar_resume_signal_with_generation("analysis", &sidecar, 7)?,
            MatrixResumeSignal::ResumeAvailable {
                committed: 1,
                total: 4
            }
        );
        assert_eq!(
            reader
                .matrix_verified_sidecar_resume_signal_with_generation("analysis", &sidecar, 8)?,
            MatrixResumeSignal::DiscardRecommended
        );
        let (manifest, payload) = reader.read_matrix_sidecar("analysis", &sidecar)?;
        assert_eq!(manifest.format_magic, b"MTX");
        assert_eq!(payload, b"resume-state-v1");
        assert!(matches!(
            reader.read_matrix_sidecar_with_generation("analysis", &sidecar, 8),
            Err(Error::MatrixSidecarMismatch("generation"))
        ));
        assert!(matches!(
            reader.read_matrix_sidecar("master_grid", &sidecar),
            Err(Error::MatrixSidecarMismatch("category"))
        ));
    }

    {
        let mut file = OpenOptions::new().read(true).write(true).open(&sidecar)?;
        let len = file.metadata()?.len();
        file.seek(SeekFrom::Start(len - 1))?;
        file.write_all(&[0xFF])?;
    }

    {
        let reader = spec.open_reader(&path)?;
        assert_eq!(
            reader.matrix_verified_sidecar_resume_signal("analysis", &sidecar)?,
            MatrixResumeSignal::DiscardRecommended
        );
        assert!(matches!(
            reader.read_matrix_sidecar("analysis", &sidecar),
            Err(Error::MatrixSidecarChecksumMismatch { .. })
        ));
    }

    remove_file(&sidecar)?;
    cleanup(&path);
    Ok(())
}

#[cfg(feature = "integrity")]
#[test]
fn matrix_sidecar_identity_uses_computed_schema_hash() -> varve::Result<()> {
    let path = temp_path("matrix_sidecar_schema_hash");
    let sidecar = path.with_extension("sidecar");
    cleanup(&path);
    let _ = remove_file(&sidecar);
    let spec = FormatSpec {
        schema_hash: 0xCAFE_BABE,
        ..matrix_crc_spec()
    };
    let computed = spec.computed_schema_hash();
    assert_ne!(spec.schema_hash, computed);

    {
        let dims = MatrixDimensions::from_pairs([("scan", 1), ("ch", 1)]);
        let mut writer = spec.create_writer_with_dims(&path, dims)?;
        let manifest = writer.write_matrix_sidecar("analysis", &sidecar, 1, b"resume")?;
        assert_eq!(manifest.schema_hash, computed);
        assert_ne!(manifest.schema_hash, spec.schema_hash);
        writer.flush()?;
    }

    {
        let reader = spec.open_reader(&path)?;
        let (manifest, payload) = reader.read_matrix_sidecar("analysis", &sidecar)?;
        assert_eq!(manifest.schema_hash, computed);
        assert_eq!(payload, b"resume");
    }

    remove_file(&sidecar)?;
    cleanup(&path);
    Ok(())
}

#[cfg(feature = "mmap")]
#[test]
fn matrix_mmap_payload_window_is_checked_and_snapshot_based() -> varve::Result<()> {
    let path = temp_path("matrix_mmap_payload");
    cleanup(&path);
    let spec = matrix_spec();

    {
        let dims = MatrixDimensions::from_pairs([("scan", 2), ("ch", 2)]);
        let mut writer = spec.create_writer_with_dims(&path, dims)?;
        writer.write_matrix_cell(MatrixKey::new(0, 0), &MatrixCell { value: 77 })?;
        writer.commit_matrix_cell::<MatrixCell>(MatrixKey::new(0, 0))?;
        writer.write_matrix_cell(MatrixKey::new(1, 1), &MatrixCell { value: 88 })?;
        writer.flush()?;
    }

    {
        let reader = spec.open_reader(&path)?;
        // SAFETY: The fixture is not modified while the mapping is alive.
        let mmap = unsafe { reader.mmap_matrix()? };
        assert_eq!(
            mmap.cell_numeric::<MatrixCell, u32>(MatrixKey::new(0, 0))?,
            77
        );
        assert!(matches!(
            mmap.cell_numeric_at::<MatrixCell, u32>(MatrixKey::new(0, 0), 1),
            Err(Error::MatrixNumericOutOfBounds { .. })
        ));
        assert_eq!(
            mmap.cell_payload_window::<MatrixCell>(MatrixKey::new(0, 0))?,
            77u32.to_le_bytes()
        );
        assert!(matches!(
            mmap.cell_payload_window::<MatrixCell>(MatrixKey::new(1, 1)),
            Err(Error::MatrixNotCommitted)
        ));
    }

    cleanup(&path);
    Ok(())
}

#[cfg(feature = "mmap")]
#[test]
fn matrix_mmap_rejects_backing_file_truncated_after_open() -> varve::Result<()> {
    let path = temp_path("matrix_mmap_truncated_snapshot");
    cleanup(&path);
    let spec = matrix_spec();

    let dims = MatrixDimensions::from_pairs([("scan", 1), ("ch", 1)]);
    let mut writer = spec.create_writer_with_dims(&path, dims)?;
    writer.push_info(&LogPoint { value: 99 })?;
    writer.flush()?;
    drop(writer);

    let reader = spec.open_reader(&path)?;
    let append_log_start = reader.index_entries()[0].record_offset;
    OpenOptions::new()
        .write(true)
        .open(&path)?
        .set_len(append_log_start)?;

    let mapped = std::panic::catch_unwind(|| {
        // SAFETY: Mutation is complete before this call and no mapping is returned.
        unsafe { reader.mmap_matrix() }
    });
    assert!(mapped.is_ok(), "truncated matrix snapshot caused a panic");
    assert!(matches!(
        mapped.expect("checked above"),
        Err(Error::MmapPayloadOutOfBounds { .. })
    ));

    drop(reader);
    cleanup(&path);
    Ok(())
}

#[cfg(feature = "zero-copy")]
#[test]
fn matrix_zero_copy_raw_cell_views_committed_slot() -> varve::Result<()> {
    let path = temp_path("matrix_zero_copy_raw");
    cleanup(&path);
    let spec = raw_matrix_spec();
    let key = MatrixKey::new(1, 0);

    {
        let dims = MatrixDimensions::from_pairs([("scan", 2), ("ch", 2)]);
        let mut writer = spec.create_writer_with_dims(&path, dims)?;
        writer.write_matrix_cell(
            key,
            &RawMatrixCell {
                bytes: 1234u32.to_le_bytes(),
            },
        )?;
        writer.commit_matrix_cell::<RawMatrixCell>(key)?;
        writer.flush()?;
    }

    {
        let reader = spec.open_reader(&path)?;
        // SAFETY: The fixture is not modified while the mapping is alive.
        let mmap = unsafe { reader.mmap_matrix()? };
        let raw = unsafe { mmap.raw_cell::<RawMatrixCell>(key) }?;
        assert_eq!(u32::from_le_bytes(raw.bytes), 1234);
    }

    cleanup(&path);
    Ok(())
}

#[test]
fn matrix_rejects_corrupt_append_log_start() -> varve::Result<()> {
    let path = temp_path("matrix_bad_append_start");
    cleanup(&path);
    let spec = matrix_spec();
    {
        let dims = MatrixDimensions::from_pairs([("scan", 2), ("ch", 2)]);
        let mut writer = spec.create_writer_with_dims(&path, dims)?;
        writer.write_matrix_cell(MatrixKey::new(0, 0), &MatrixCell { value: 1 })?;
        writer.commit_matrix_cell::<MatrixCell>(MatrixKey::new(0, 0))?;
        writer.flush()?;
    }

    let mut file = OpenOptions::new().write(true).open(&path)?;
    let header_len = spec.magic.len() as u64 + 18;
    let append_log_start_field = header_len + 28 + 12 * 8;
    file.seek(SeekFrom::Start(append_log_start_field))?;
    file.write_all(&(header_len + 160).to_le_bytes())?;
    drop(file);

    assert!(matches!(
        spec.open_readonly(&path),
        Err(Error::InvalidMatrixLayout)
    ));

    cleanup(&path);
    Ok(())
}

#[cfg(feature = "integrity")]
#[test]
fn matrix_commit_map_crc_corruption_reports_recoverable_region_and_rebuilds() -> varve::Result<()> {
    let path = temp_path("matrix_crc_commit_map");
    cleanup(&path);
    let spec = matrix_crc_spec();

    {
        let dims = MatrixDimensions::from_pairs([("scan", 2), ("ch", 2)]);
        let mut writer = spec.create_writer_with_dims(&path, dims)?;
        writer.write_matrix_cell(MatrixKey::new(1, 0), &MatrixCell { value: 11 })?;
        writer.commit_matrix_cell::<MatrixCell>(MatrixKey::new(1, 0))?;
        writer.write_matrix_cell(MatrixKey::new(0, 1), &MatrixCell { value: 99 })?;
        writer.flush()?;
    }

    let offsets = read_vmat_offsets(&path, spec)?;
    {
        let mut file = OpenOptions::new().write(true).open(&path)?;
        file.seek(SeekFrom::Start(offsets.commit_map_off))?;
        file.write_all(&[0xFF])?;
    }

    {
        let reader = spec.open_reader(&path)?;
        let report = reader.matrix_recovery_report();
        assert!(report.findings.iter().any(|finding| {
            finding.kind == MatrixCorruptionKind::CommitMap
                && finding.severity == MatrixCorruptionSeverity::Recoverable
        }));
    }

    {
        let mut writer = spec.open_writer(&path)?;
        assert_eq!(writer.rebuild_matrix_commit_from_crc::<MatrixCell>()?, 1);
        writer.flush()?;
    }

    {
        let mut reader = spec.open_reader(&path)?;
        assert_eq!(
            reader.matrix_cell_status::<MatrixCell>(MatrixKey::new(1, 0))?,
            MatrixCellStatus::Committed
        );
        assert_eq!(
            reader.read_matrix_cell::<MatrixCell>(MatrixKey::new(1, 0))?,
            MatrixCell { value: 11 }
        );
        assert_eq!(
            reader.matrix_cell_status::<MatrixCell>(MatrixKey::new(0, 1))?,
            MatrixCellStatus::NotCommitted
        );
    }

    cleanup(&path);
    Ok(())
}

#[cfg(feature = "integrity")]
#[test]
fn matrix_slot_crc_corruption_rejects_committed_cell_read() -> varve::Result<()> {
    let path = temp_path("matrix_crc_slot");
    cleanup(&path);
    let spec = matrix_crc_spec();

    {
        let dims = MatrixDimensions::from_pairs([("scan", 2), ("ch", 2)]);
        let mut writer = spec.create_writer_with_dims(&path, dims)?;
        writer.write_matrix_cell(MatrixKey::new(1, 1), &MatrixCell { value: 1234 })?;
        writer.commit_matrix_cell::<MatrixCell>(MatrixKey::new(1, 1))?;
        writer.flush()?;
    }

    let offsets = read_vmat_offsets(&path, spec)?;
    {
        let mut file = OpenOptions::new().write(true).open(&path)?;
        let ordinal = 3;
        file.seek(SeekFrom::Start(
            offsets.slot_region_off + ordinal * MatrixCell::SLOT_STRIDE,
        ))?;
        file.write_all(&9999u32.to_le_bytes())?;
    }

    {
        let mut reader = spec.open_reader(&path)?;
        assert!(matches!(
            reader.read_matrix_cell::<MatrixCell>(MatrixKey::new(1, 1)),
            Err(Error::MatrixChecksumMismatch { .. })
        ));
        assert!(matches!(
            reader.matrix_cell_payload::<MatrixCell>(MatrixKey::new(1, 1)),
            Err(Error::MatrixChecksumMismatch { .. })
        ));
    }

    #[cfg(feature = "mmap")]
    {
        let reader = spec.open_reader(&path)?;
        // SAFETY: The fixture is not modified while the mapping is alive.
        let mmap = unsafe { reader.mmap_matrix()? };
        assert!(matches!(
            mmap.cell_payload_window::<MatrixCell>(MatrixKey::new(1, 1)),
            Err(Error::MatrixChecksumMismatch { .. })
        ));
    }

    cleanup(&path);
    Ok(())
}

#[cfg(feature = "integrity")]
#[test]
fn matrix_crc_rebuild_preserves_committed_zero_payloads() -> varve::Result<()> {
    let path = temp_path("matrix_crc_zero_rebuild");
    cleanup(&path);
    let spec = matrix_crc_spec();
    let zero_key = MatrixKey::new(0, 0);

    {
        let dims = MatrixDimensions::from_pairs([("scan", 2), ("ch", 2)]);
        let mut writer = spec.create_writer_with_dims(&path, dims)?;
        writer.write_matrix_cell(zero_key, &MatrixCell { value: 0 })?;
        writer.commit_matrix_cell::<MatrixCell>(zero_key)?;
        writer.flush()?;
    }

    let offsets = read_vmat_offsets(&path, spec)?;
    {
        let mut file = OpenOptions::new().write(true).open(&path)?;
        file.seek(SeekFrom::Start(offsets.commit_map_off))?;
        file.write_all(&[0])?;
    }

    {
        let mut writer = spec.open_writer(&path)?;
        assert_eq!(writer.rebuild_matrix_commit_from_crc::<MatrixCell>()?, 1);
        writer.flush()?;
    }

    {
        let mut reader = spec.open_reader(&path)?;
        assert_eq!(
            reader.read_matrix_cell::<MatrixCell>(zero_key)?,
            MatrixCell { value: 0 }
        );
    }

    cleanup(&path);
    Ok(())
}

#[derive(Debug)]
struct VmatOffsets {
    commit_map_off: u64,
    #[cfg(feature = "integrity")]
    slot_region_off: u64,
}

fn read_vmat_offsets(path: &PathBuf, spec: FormatSpec) -> varve::Result<VmatOffsets> {
    let mut file = OpenOptions::new().read(true).open(path)?;
    let header_len = spec.magic.len() as u64 + 18;
    file.seek(SeekFrom::Start(header_len))?;
    let mut header = [0; 160];
    file.read_exact(&mut header)?;
    let read_u64 = |index: usize| {
        let start = 24 + index * 8;
        u64::from_le_bytes(header[start..start + 8].try_into().expect("slice"))
    };
    Ok(VmatOffsets {
        commit_map_off: read_u64(6),
        #[cfg(feature = "integrity")]
        slot_region_off: read_u64(8),
    })
}

fn temp_path(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "varve_{name}_{}_{}.vrv",
        std::process::id(),
        std::thread::current().name().unwrap_or("test")
    ))
}

fn cleanup(path: &PathBuf) {
    match remove_file(path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => panic!("failed to remove {}: {error}", path.display()),
    }
    let lock = path.with_extension("vrv.lock");
    match remove_file(&lock) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => panic!("failed to remove {}: {error}", lock.display()),
    }
}
