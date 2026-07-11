use std::fs::{remove_file, write};
use std::path::{Path, PathBuf};

use varve::{
    DiagnosticDomain, DiagnosticSeverity, Endian, Error, FormatSpec, IndexPolicy, IntegrityPolicy,
    ManifestPolicy, MatrixAuxDescriptor, MatrixBlockDescriptor, MatrixCommitDescriptor,
    MatrixCommitKind, MatrixDimensionDescriptor, MatrixDimensions, MatrixKey, ReadLimits,
    RecoveryPolicy, SelfTestStepStatus, VarveBlock, VarveDecode, VarveEncode, VarveMatrixBlock,
    WireType, varve_format,
};

varve_format! {
    pub format SelfCheckFormat {
        magic: b"SCHK";
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
        manifest: embedded;
        index: [scan_on_open, block_offset_chain, keyed_offset_chain];

        blocks {
            fixed Point(id = 700) {
                x: u32,
                y: u32,
            }

            variable User(id = 701, key = [id]) {
                id: u64,
                name: String,
                flags: u32 = default,
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
struct MatrixCell {
    value: u32,
}

impl VarveEncode for MatrixCell {
    const WIRE_TYPE: WireType = WireType::Bytes;

    fn encode_varve(&self, encoder: &mut varve::Encoder) -> varve::Result<()> {
        self.value.encode_varve(encoder)
    }
}

impl VarveDecode for MatrixCell {
    const WIRE_TYPE: WireType = WireType::Bytes;

    fn decode_varve(decoder: &mut varve::Decoder<'_>) -> varve::Result<Self> {
        Ok(Self {
            value: u32::decode_varve(decoder)?,
        })
    }
}

impl VarveBlock for MatrixCell {
    const ID: u32 = 710;
    const VERSION: u16 = 1;
    const KIND: varve::BlockKind = varve::BlockKind::Matrix;
    const ENDIAN: Option<Endian> = None;
}

impl varve::VarveMatrixBlock for MatrixCell {
    const DIMENSIONS: [&'static str; 2] = ["scan", "ch"];
    const CATEGORY: &'static str = "analysis";
    const SLOT_STRIDE: u64 = 4;
}

fn matrix_spec() -> FormatSpec {
    static BLOCKS: &[varve::BlockDescriptor] = &[varve::BlockDescriptor {
        id: MatrixCell::ID,
        name: "MatrixCell",
        version: MatrixCell::VERSION,
        kind: varve::BlockKind::Matrix,
        fields: MatrixCell::FIELDS,
    }];
    static DIMS: &[MatrixDimensionDescriptor] = &[
        MatrixDimensionDescriptor { name: "scan" },
        MatrixDimensionDescriptor { name: "ch" },
    ];
    static COMMITS: &[MatrixCommitDescriptor] = &[MatrixCommitDescriptor {
        name: MatrixCell::CATEGORY,
        kind: MatrixCommitKind::Cell,
    }];
    static MATRIX_BLOCKS: &[MatrixBlockDescriptor] = &[MatrixBlockDescriptor {
        block_id: MatrixCell::ID,
        dimensions: MatrixCell::DIMENSIONS,
        category: MatrixCell::CATEGORY,
        slot_stride: MatrixCell::SLOT_STRIDE,
    }];
    static AUX: &[MatrixAuxDescriptor] = &[MatrixAuxDescriptor {
        name: "scratch",
        byte_len: 16,
    }];
    FormatSpec::new(
        b"MSCHK",
        1,
        Endian::Little,
        0,
        IndexPolicy::ScanOnOpen,
        IntegrityPolicy::None,
        RecoveryPolicy::Strict,
        ManifestPolicy::None,
        BLOCKS,
    )
    .with_matrix_spec(DIMS, COMMITS, MATRIX_BLOCKS)
    .with_matrix_aux(AUX)
    .with_read_limits(ReadLimits::finite_all(u64::MAX))
}

#[test]
fn generated_format_self_test_roundtrips_append_and_keyed_blocks() {
    let path = temp_path("append");
    cleanup(&path);

    let report = SelfCheckFormat::self_test(&path)
        .with_block(Point { x: 1, y: 2 })
        .with_keyed_block(User {
            id: 42,
            name: "Ada".to_string(),
            flags: 7,
        })
        .cleanup(true)
        .run();

    assert!(report.passed(), "{report:#?}");
    assert!(
        report
            .steps
            .iter()
            .any(|step| step.name.contains("write block 700"))
    );
    cleanup(&path);
}

#[test]
fn matrix_self_test_roundtrips_committed_uncommitted_and_aux_paths() {
    let path = temp_path("matrix");
    cleanup(&path);

    let report = matrix_spec()
        .self_test(&path)
        .with_dims(MatrixDimensions::from_pairs([("scan", 2), ("ch", 2)]))
        .with_matrix_cell(MatrixKey::new(0, 0), MatrixCell { value: 9 })
        .with_uncommitted_matrix_cell(MatrixKey::new(1, 1), MatrixCell { value: 11 })
        .with_matrix_aux("scratch", 4, [1, 2, 3, 4])
        .cleanup(true)
        .run();

    assert!(report.passed(), "{report:#?}");
    cleanup(&path);
}

#[test]
fn matrix_self_test_reports_missing_dims_as_caller_usage() {
    let path = temp_path("missing_dims");
    cleanup(&path);

    let report = matrix_spec()
        .self_test(&path)
        .with_matrix_cell(MatrixKey::new(0, 0), MatrixCell { value: 9 })
        .cleanup(true)
        .run();

    let failure = report.failures().next().expect("failure");
    assert_eq!(failure.status, SelfTestStepStatus::Failed);
    assert_eq!(failure.domain, Some(DiagnosticDomain::CallerUsage));
    assert!(failure.message.contains("matrix dimensions"));
    cleanup(&path);
}

#[test]
fn diagnostics_classify_file_mismatch_and_bad_format_spec() {
    let path = temp_path("bad_file");
    cleanup(&path);
    write(&path, b"not a varve file").expect("write corrupt fixture");

    let report = SelfCheckFormat::diagnose_file(&path);
    assert!(!report.passed());
    assert!(report.items.iter().any(|item| {
        item.severity == DiagnosticSeverity::Error
            && item.domain == DiagnosticDomain::FileData
            && item.code == "file.open_readonly.failed"
    }));

    static DUP_BLOCKS: &[varve::BlockDescriptor] = &[
        varve::BlockDescriptor {
            id: 1,
            name: "A",
            version: 1,
            kind: varve::BlockKind::Fixed,
            fields: &[],
        },
        varve::BlockDescriptor {
            id: 1,
            name: "B",
            version: 1,
            kind: varve::BlockKind::Fixed,
            fields: &[],
        },
    ];
    let bad_spec = FormatSpec::new(
        b"BAD",
        1,
        Endian::Little,
        0,
        IndexPolicy::ScanOnOpen,
        IntegrityPolicy::None,
        RecoveryPolicy::Strict,
        ManifestPolicy::None,
        DUP_BLOCKS,
    );
    let spec_report = bad_spec.diagnostics();
    assert!(!spec_report.passed());
    assert!(spec_report.items.iter().any(|item| {
        item.domain == DiagnosticDomain::FormatDefinition
            && item.code == "format.spec.invalid"
            && item.message.contains("duplicate block id")
    }));

    assert_eq!(
        varve::classify_error(&Error::CompressionFeatureDisabled),
        DiagnosticDomain::FeatureGate
    );
    cleanup(&path);
}

fn temp_path(name: &str) -> PathBuf {
    let mut path = std::env::temp_dir();
    path.push(format!(
        "varve_self_check_{name}_{}.vrv",
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
