//! SEC-05: a matrix metadata CRC mismatch is recorded as a `Fatal` recovery
//! finding and must fail closed: default safe accessors return a typed error,
//! while the explicit `with_matrix_fatal_forensics` opt-in still reads through
//! and surfaces the full recovery report.
#![cfg(feature = "integrity")]

use std::fs::{OpenOptions, read, remove_file};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use varve::{
    BlockDescriptor, BlockKind, Endian, Error, FormatSpec, IndexPolicy, MatrixBlockDescriptor,
    MatrixCommitDescriptor, MatrixCommitKind, MatrixCorruptionKind, MatrixCorruptionSeverity,
    MatrixDimensionDescriptor, MatrixDimensions, MatrixKey, ReadLimits, VarveBlock,
    VarveMatrixBlock,
};

const VMAT_HEADER_LEN: usize = 160;
const HEADER_U64_OFFSET: usize = 24;
const REGION_CRC_OFF: usize = 10;
const MCRC_METADATA_CRC_OFFSET: u64 = 8;

static NEXT_TEMP_ID: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Debug, PartialEq, Eq, VarveBlock)]
#[varve(id = 710, version = 1, kind = "matrix")]
struct GateCell {
    value: u32,
}

impl VarveMatrixBlock for GateCell {
    const DIMENSIONS: [&'static str; 2] = ["scan", "ch"];
    const CATEGORY: &'static str = "analysis";
    const SLOT_STRIDE: u64 = 4;
}

fn matrix_spec() -> FormatSpec {
    static BLOCKS: &[BlockDescriptor] = &[BlockDescriptor {
        id: GateCell::ID,
        name: "GateCell",
        version: GateCell::VERSION,
        kind: BlockKind::Matrix,
        fields: GateCell::FIELDS,
    }];
    static DIMENSIONS: &[MatrixDimensionDescriptor] = &[
        MatrixDimensionDescriptor { name: "scan" },
        MatrixDimensionDescriptor { name: "ch" },
    ];
    static COMMITS: &[MatrixCommitDescriptor] = &[MatrixCommitDescriptor {
        name: "analysis",
        kind: MatrixCommitKind::Cell,
    }];
    static MATRIX_BLOCKS: &[MatrixBlockDescriptor] = &[MatrixBlockDescriptor {
        block_id: GateCell::ID,
        dimensions: GateCell::DIMENSIONS,
        category: GateCell::CATEGORY,
        slot_stride: GateCell::SLOT_STRIDE,
    }];

    FormatSpec::new(
        b"MFGT",
        1,
        Endian::Little,
        0,
        IndexPolicy::ScanOnOpen,
        varve::IntegrityPolicy::Crc32,
        varve::RecoveryPolicy::Strict,
        varve::ManifestPolicy::None,
        BLOCKS,
    )
    .with_matrix_spec(DIMENSIONS, COMMITS, MATRIX_BLOCKS)
    .with_read_limits(ReadLimits::finite_all(u64::MAX))
}

struct TempMatrix {
    path: PathBuf,
}

impl TempMatrix {
    fn new(name: &str) -> Self {
        let id = NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "varve_{name}_{}_{}_{}.vrv",
            std::process::id(),
            std::thread::current().name().unwrap_or("test"),
            id
        ));
        let _ = remove_file(&path);
        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempMatrix {
    fn drop(&mut self) {
        let _ = remove_file(&self.path);
    }
}

fn write_committed_cell(spec: FormatSpec, path: &Path, key: MatrixKey, value: u32) {
    let dimensions = MatrixDimensions::from_pairs([("scan", 2), ("ch", 2)]);
    let mut writer = spec
        .create_writer_with_dims(path, dimensions)
        .expect("create matrix fixture");
    writer
        .write_matrix_cell(key, &GateCell { value })
        .expect("write cell");
    writer
        .commit_matrix_cell::<GateCell>(key)
        .expect("commit cell");
    writer.flush().expect("flush fixture");
}

fn region_crc_off(path: &Path, spec: FormatSpec) -> u64 {
    let offset = u64::try_from(spec.magic.len()).expect("magic length") + 18;
    let mut file = OpenOptions::new()
        .read(true)
        .open(path)
        .expect("open matrix fixture");
    file.seek(SeekFrom::Start(offset))
        .expect("seek matrix header");
    let mut bytes = [0; VMAT_HEADER_LEN];
    file.read_exact(&mut bytes).expect("read matrix header");
    let start = HEADER_U64_OFFSET + REGION_CRC_OFF * 8;
    u64::from_le_bytes(bytes[start..start + 8].try_into().expect("field"))
}

/// Flips the stored matrix metadata CRC inside the MCRC header, so the
/// recomputed metadata CRC no longer matches while every structural field of
/// the layout still validates.
fn corrupt_stored_metadata_crc(path: &Path, spec: FormatSpec) {
    let crc_offset = region_crc_off(path, spec) + MCRC_METADATA_CRC_OFFSET;
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .expect("open matrix fixture for corruption");
    file.seek(SeekFrom::Start(crc_offset))
        .expect("seek stored metadata crc");
    let mut stored = [0u8; 4];
    file.read_exact(&mut stored).expect("read stored crc");
    for byte in &mut stored {
        *byte ^= 0xFF;
    }
    file.seek(SeekFrom::Start(crc_offset))
        .expect("seek stored metadata crc for rewrite");
    file.write_all(&stored).expect("flip stored crc");
}

fn has_fatal_header_finding(report: &varve::MatrixRecoveryReport) -> bool {
    report.findings.iter().any(|finding| {
        finding.kind == MatrixCorruptionKind::Header
            && finding.severity == MatrixCorruptionSeverity::Fatal
    })
}

#[test]
fn fatal_metadata_crc_blocks_default_reads_and_writes() {
    let spec = matrix_spec();
    let fixture = TempMatrix::new("matrix_fatal_gate_default");
    let key = MatrixKey::new(1, 0);
    write_committed_cell(spec, fixture.path(), key, 11);
    corrupt_stored_metadata_crc(fixture.path(), spec);
    let corrupt_bytes = read(fixture.path()).expect("read corrupt evidence");

    {
        let mut reader = spec
            .open_reader(fixture.path())
            .expect("default open still surfaces the recovery report");
        assert!(matches!(
            reader.read_matrix_cell::<GateCell>(key),
            Err(Error::MatrixFatalCorruption)
        ));
        assert!(matches!(
            reader.matrix_cell_status::<GateCell>(key),
            Err(Error::MatrixFatalCorruption)
        ));
        assert!(matches!(
            reader.matrix_resume_signal("analysis"),
            Err(Error::MatrixFatalCorruption)
        ));
        assert!(
            has_fatal_header_finding(&reader.matrix_recovery_report()),
            "default reader must still expose the fatal finding"
        );
    }

    {
        let mut writer = spec
            .open_writer(fixture.path())
            .expect("open default writer");
        assert!(matches!(
            writer.write_matrix_cell(MatrixKey::new(0, 0), &GateCell { value: 22 }),
            Err(Error::MatrixFatalCorruption)
        ));
        assert!(matches!(
            writer.clear_matrix_category("analysis"),
            Err(Error::MatrixFatalCorruption)
        ));
    }

    assert_eq!(
        read(fixture.path()).expect("reread blocked evidence"),
        corrupt_bytes,
        "blocked accessors must not mutate the fatal-state file"
    );
}

#[test]
fn forensic_opt_in_reads_through_and_surfaces_fatal_finding() {
    let spec = matrix_spec();
    let fixture = TempMatrix::new("matrix_fatal_gate_forensic");
    let key = MatrixKey::new(1, 0);
    write_committed_cell(spec, fixture.path(), key, 11);
    corrupt_stored_metadata_crc(fixture.path(), spec);

    let forensic = spec.with_matrix_fatal_forensics();
    let mut reader = forensic
        .open_reader(fixture.path())
        .expect("open forensic reader");
    assert_eq!(
        reader
            .read_matrix_cell::<GateCell>(key)
            .expect("forensic read of committed cell"),
        GateCell { value: 11 }
    );
    assert!(
        has_fatal_header_finding(&reader.matrix_recovery_report()),
        "forensic access must still surface the fatal finding"
    );
}

#[test]
fn uncorrupted_file_is_unaffected_by_the_fatal_gate() {
    let spec = matrix_spec();
    let fixture = TempMatrix::new("matrix_fatal_gate_clean");
    let key = MatrixKey::new(1, 0);
    write_committed_cell(spec, fixture.path(), key, 11);

    let mut reader = spec.open_reader(fixture.path()).expect("open clean reader");
    assert_eq!(
        reader
            .read_matrix_cell::<GateCell>(key)
            .expect("read clean cell"),
        GateCell { value: 11 }
    );
    let report = reader.matrix_recovery_report();
    assert!(
        !has_fatal_header_finding(&report),
        "clean file must not report fatal findings"
    );
}
