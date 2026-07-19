use std::fs::OpenOptions;
#[cfg(feature = "integrity")]
use std::fs::read;
use std::io::{Read, Seek, SeekFrom, Write};
use std::panic::catch_unwind;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use proptest::prelude::*;
use varve::{
    BlockDescriptor, BlockKind, Endian, Error, FormatSpec, IndexPolicy, MatrixBlockDescriptor,
    MatrixCellStatus, MatrixCommitDescriptor, MatrixCommitKind, MatrixDimensionDescriptor,
    MatrixDimensions, MatrixKey, MatrixResumeSignal, ReadLimits, VarveBlock, VarveMatrixBlock,
};
#[cfg(feature = "integrity")]
use varve::{MatrixCorruptionKind, MatrixCorruptionSeverity, MatrixRecoveryAction};

const VMAT_HEADER_LEN: usize = 160;
const BLOCK_ENTRY_LEN: u64 = 44;
const DIMENSION_COUNT_OFFSET: u64 = 12;
const BLOCK_COUNT_OFFSET: u64 = 16;
const COMMIT_COUNT_OFFSET: u64 = 20;
const HEADER_U64_OFFSET: u64 = 24;
const DIMENSION_TABLE_LEN: usize = 1;
const BLOCK_TABLE_OFF: usize = 2;
const BLOCK_TABLE_LEN: usize = 3;
const COMMIT_CATEGORY_OFF: usize = 4;
const COMMIT_CATEGORY_LEN: usize = 5;
const COMMIT_MAP_OFF: usize = 6;
const COMMIT_MAP_LEN: usize = 7;
const SLOT_REGION_OFF: usize = 8;
const SLOT_REGION_LEN: usize = 9;
#[cfg(feature = "integrity")]
const REGION_CRC_LEN: usize = 11;
const APPEND_LOG_START: usize = 12;

static NEXT_TEMP_ID: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Debug, PartialEq, Eq, VarveBlock)]
#[varve(id = 700, version = 1, kind = "matrix")]
struct PrimaryCell {
    value: u32,
}

impl VarveMatrixBlock for PrimaryCell {
    const DIMENSIONS: [&'static str; 2] = ["scan", "ch"];
    const CATEGORY: &'static str = "analysis";
    const SLOT_STRIDE: u64 = 4;
}

#[derive(Clone, Debug, PartialEq, Eq, VarveBlock)]
#[varve(id = 701, version = 1, kind = "matrix")]
struct SecondaryCell {
    value: u32,
}

impl VarveMatrixBlock for SecondaryCell {
    const DIMENSIONS: [&'static str; 2] = ["scan", "ch"];
    const CATEGORY: &'static str = "secondary";
    const SLOT_STRIDE: u64 = 4;
}

fn matrix_spec(integrity: varve::IntegrityPolicy) -> FormatSpec {
    static BLOCKS: &[BlockDescriptor] = &[BlockDescriptor {
        id: PrimaryCell::ID,
        name: "PrimaryCell",
        version: PrimaryCell::VERSION,
        kind: BlockKind::Matrix,
        fields: PrimaryCell::FIELDS,
    }];
    static DIMENSIONS: &[MatrixDimensionDescriptor] = &[
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
        block_id: PrimaryCell::ID,
        dimensions: PrimaryCell::DIMENSIONS,
        category: PrimaryCell::CATEGORY,
        slot_stride: PrimaryCell::SLOT_STRIDE,
    }];

    FormatSpec::new(
        b"MHMX",
        1,
        Endian::Little,
        0,
        IndexPolicy::ScanOnOpen,
        integrity,
        varve::RecoveryPolicy::Strict,
        varve::ManifestPolicy::None,
        BLOCKS,
    )
    .with_matrix_spec(DIMENSIONS, COMMITS, MATRIX_BLOCKS)
    .with_read_limits(ReadLimits::finite_all(u64::MAX))
}

fn two_block_spec() -> FormatSpec {
    static BLOCKS: &[BlockDescriptor] = &[
        BlockDescriptor {
            id: PrimaryCell::ID,
            name: "PrimaryCell",
            version: PrimaryCell::VERSION,
            kind: BlockKind::Matrix,
            fields: PrimaryCell::FIELDS,
        },
        BlockDescriptor {
            id: SecondaryCell::ID,
            name: "SecondaryCell",
            version: SecondaryCell::VERSION,
            kind: BlockKind::Matrix,
            fields: SecondaryCell::FIELDS,
        },
    ];
    static DIMENSIONS: &[MatrixDimensionDescriptor] = &[
        MatrixDimensionDescriptor { name: "scan" },
        MatrixDimensionDescriptor { name: "ch" },
    ];
    static COMMITS: &[MatrixCommitDescriptor] = &[
        MatrixCommitDescriptor {
            name: "analysis",
            kind: MatrixCommitKind::Cell,
        },
        MatrixCommitDescriptor {
            name: "secondary",
            kind: MatrixCommitKind::Cell,
        },
    ];
    static MATRIX_BLOCKS: &[MatrixBlockDescriptor] = &[
        MatrixBlockDescriptor {
            block_id: PrimaryCell::ID,
            dimensions: PrimaryCell::DIMENSIONS,
            category: PrimaryCell::CATEGORY,
            slot_stride: PrimaryCell::SLOT_STRIDE,
        },
        MatrixBlockDescriptor {
            block_id: SecondaryCell::ID,
            dimensions: SecondaryCell::DIMENSIONS,
            category: SecondaryCell::CATEGORY,
            slot_stride: SecondaryCell::SLOT_STRIDE,
        },
    ];

    FormatSpec::new(
        b"MHM2",
        1,
        Endian::Little,
        0,
        IndexPolicy::ScanOnOpen,
        varve::IntegrityPolicy::None,
        varve::RecoveryPolicy::Strict,
        varve::ManifestPolicy::None,
        BLOCKS,
    )
    .with_matrix_spec(DIMENSIONS, COMMITS, MATRIX_BLOCKS)
    .with_read_limits(ReadLimits::finite_all(u64::MAX))
}

#[derive(Clone, Copy, Debug)]
struct VmatHeader {
    offset: u64,
    fields: [u64; 13],
}

struct TempMatrix {
    path: PathBuf,
    _dir: tempfile::TempDir,
}

impl TempMatrix {
    fn new(name: &str) -> Self {
        let id = NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed);
        let dir = tempfile::tempdir().expect("create per-test temp directory");
        let path = dir.path().join(format!(
            "varve_{name}_{}_{}_{}.vrv",
            std::process::id(),
            std::thread::current().name().unwrap_or("test"),
            id
        ));
        Self { path, _dir: dir }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

fn create_empty(spec: FormatSpec, path: &Path) {
    let dimensions = MatrixDimensions::from_pairs([("scan", 2), ("ch", 2)]);
    drop(
        spec.create_writer_with_dims(path, dimensions)
            .expect("create matrix fixture"),
    );
}

#[test]
fn fresh_reader_rejects_uncommitted_matrix_overwrite() {
    let fixture = TempMatrix::new("fresh_reader_overwrite");
    let spec = matrix_spec(varve::IntegrityPolicy::None);
    let key = MatrixKey::new(0, 0);
    let dimensions = MatrixDimensions::from_pairs([("scan", 1), ("ch", 1)]);
    let mut writer = spec
        .create_writer_with_dims(fixture.path(), dimensions)
        .expect("create matrix fixture");
    writer
        .write_matrix_cell(key, &PrimaryCell { value: 11 })
        .expect("write initial cell");
    writer
        .commit_matrix_cell::<PrimaryCell>(key)
        .expect("commit initial cell");

    writer
        .write_matrix_cell(key, &PrimaryCell { value: 22 })
        .expect("write uncommitted replacement");

    let mut reader = spec
        .open_readonly(fixture.path())
        .expect("open reader after overwrite");
    assert_eq!(
        reader
            .matrix_cell_status::<PrimaryCell>(key)
            .expect("read commit status"),
        MatrixCellStatus::NotCommitted
    );
    assert!(matches!(
        reader.read_matrix_cell::<PrimaryCell>(key),
        Err(Error::MatrixNotCommitted)
    ));
}

fn read_vmat_header(path: &Path, spec: FormatSpec) -> VmatHeader {
    // Native file header, then the 24-byte matrix creation-nonce region
    // (DUR2-03), then the VMAT layout header.
    let offset = u64::try_from(spec.magic.len()).expect("magic length") + 18 + 24;
    let mut file = OpenOptions::new()
        .read(true)
        .open(path)
        .expect("open matrix fixture");
    file.seek(SeekFrom::Start(offset))
        .expect("seek matrix header");
    let mut bytes = [0; VMAT_HEADER_LEN];
    file.read_exact(&mut bytes).expect("read matrix header");

    let mut fields = [0; 13];
    for (index, field) in fields.iter_mut().enumerate() {
        let start = 24 + index * 8;
        *field = u64::from_le_bytes(bytes[start..start + 8].try_into().expect("field"));
    }
    VmatHeader { offset, fields }
}

fn patch_u32(path: &Path, offset: u64, value: u32) {
    let mut file = OpenOptions::new()
        .write(true)
        .open(path)
        .expect("open matrix fixture for mutation");
    file.seek(SeekFrom::Start(offset)).expect("seek u32 field");
    file.write_all(&value.to_le_bytes()).expect("patch u32");
}

fn patch_u64(path: &Path, offset: u64, value: u64) {
    let mut file = OpenOptions::new()
        .write(true)
        .open(path)
        .expect("open matrix fixture for mutation");
    file.seek(SeekFrom::Start(offset)).expect("seek u64 field");
    file.write_all(&value.to_le_bytes()).expect("patch u64");
}

fn patch_header_u64(path: &Path, header: VmatHeader, index: usize, value: u64) {
    let index = u64::try_from(index).expect("header field index");
    patch_u64(path, header.offset + HEADER_U64_OFFSET + index * 8, value);
}

fn category_entry_offsets(spec: FormatSpec, header: VmatHeader) -> Vec<u64> {
    let mut next = header.fields[COMMIT_CATEGORY_OFF];
    spec.matrix_commits
        .iter()
        .map(|commit| {
            let current = next;
            next += 30 + u64::try_from(commit.name.len()).expect("commit name length");
            current
        })
        .collect()
}

fn category_bit_count_offset(entry: u64, name: &str) -> u64 {
    entry + 6 + u64::try_from(name.len()).expect("commit name length")
}

fn category_map_offset_offset(entry: u64, name: &str) -> u64 {
    entry + 14 + u64::try_from(name.len()).expect("commit name length")
}

fn assert_invalid_layout(spec: FormatSpec, path: &Path) {
    let opened = catch_unwind(|| spec.open_readonly(path));
    assert!(opened.is_ok(), "hostile matrix extent caused a panic");
    assert!(matches!(
        opened.expect("checked above"),
        Err(Error::InvalidMatrixLayout)
    ));
}

#[test]
fn stored_block_cell_count_must_match_declared_dimensions() {
    let spec = matrix_spec(varve::IntegrityPolicy::None);
    let fixture = TempMatrix::new("matrix_hardening_cell_count");
    create_empty(spec, fixture.path());
    let header = read_vmat_header(fixture.path(), spec);
    let block = header.fields[BLOCK_TABLE_OFF];

    patch_u64(fixture.path(), block + 20, 1);
    patch_u64(fixture.path(), block + 36, PrimaryCell::SLOT_STRIDE);

    assert_invalid_layout(spec, fixture.path());
}

#[test]
fn stored_commit_bit_counts_must_match_descriptor_keyspaces() {
    let spec = matrix_spec(varve::IntegrityPolicy::None);
    for commit_index in 0..spec.matrix_commits.len() {
        let fixture = TempMatrix::new("matrix_hardening_bit_count");
        create_empty(spec, fixture.path());
        let header = read_vmat_header(fixture.path(), spec);
        let entries = category_entry_offsets(spec, header);
        let commit = &spec.matrix_commits[commit_index];
        patch_u64(
            fixture.path(),
            category_bit_count_offset(entries[commit_index], commit.name),
            8,
        );
        assert_invalid_layout(spec, fixture.path());
    }
}

#[test]
fn every_hostile_vmat_header_extent_fails_without_panicking() {
    let spec = matrix_spec(varve::IntegrityPolicy::None);
    for index in 0..=APPEND_LOG_START {
        let fixture = TempMatrix::new("matrix_hardening_header_extent");
        create_empty(spec, fixture.path());
        let header = read_vmat_header(fixture.path(), spec);
        patch_header_u64(fixture.path(), header, index, u64::MAX);

        let opened = catch_unwind(|| spec.open_readonly(fixture.path()));
        assert!(opened.is_ok(), "VMAT header field {index} caused a panic");
        assert!(
            opened.expect("checked above").is_err(),
            "VMAT header field {index} accepted u64::MAX"
        );
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    #[test]
    fn hostile_commit_map_offsets_are_rejected_unless_exactly_contiguous(
        relative_offsets in prop::array::uniform3(0u8..3),
    ) {
        prop_assume!(relative_offsets != [0, 1, 2]);
        let spec = matrix_spec(varve::IntegrityPolicy::None);
        let fixture = TempMatrix::new("matrix_hardening_commit_offsets");
        create_empty(spec, fixture.path());
        let header = read_vmat_header(fixture.path(), spec);
        let entries = category_entry_offsets(spec, header);

        for ((entry, commit), relative) in entries
            .iter()
            .zip(spec.matrix_commits)
            .zip(relative_offsets)
        {
            patch_u64(
                fixture.path(),
                category_map_offset_offset(*entry, commit.name),
                header.fields[COMMIT_MAP_OFF] + u64::from(relative),
            );
        }

        prop_assert!(matches!(
            spec.open_readonly(fixture.path()),
            Err(Error::InvalidMatrixLayout)
        ));
    }
}

#[test]
fn stored_block_slots_must_be_contiguous_in_descriptor_order() {
    let spec = two_block_spec();
    let fixture = TempMatrix::new("matrix_hardening_block_order");
    create_empty(spec, fixture.path());
    let header = read_vmat_header(fixture.path(), spec);
    let first = header.fields[BLOCK_TABLE_OFF];
    let second = first + BLOCK_ENTRY_LEN;
    let slot_base = header.fields[SLOT_REGION_OFF];
    let block_len = 4 * PrimaryCell::SLOT_STRIDE;

    patch_u64(fixture.path(), first + 28, slot_base + block_len);
    patch_u64(fixture.path(), second + 28, slot_base);

    assert_invalid_layout(spec, fixture.path());
}

#[test]
fn aggregate_commit_and_slot_lengths_must_be_descriptor_derived() {
    let spec = matrix_spec(varve::IntegrityPolicy::None);

    let commit_fixture = TempMatrix::new("matrix_hardening_commit_len");
    create_empty(spec, commit_fixture.path());
    let header = read_vmat_header(commit_fixture.path(), spec);
    let shifted_slot = header.fields[SLOT_REGION_OFF] + 1;
    patch_header_u64(
        commit_fixture.path(),
        header,
        COMMIT_MAP_LEN,
        header.fields[COMMIT_MAP_LEN] + 1,
    );
    patch_header_u64(commit_fixture.path(), header, SLOT_REGION_OFF, shifted_slot);
    patch_header_u64(
        commit_fixture.path(),
        header,
        APPEND_LOG_START,
        header.fields[APPEND_LOG_START] + 1,
    );
    patch_u64(
        commit_fixture.path(),
        header.fields[BLOCK_TABLE_OFF] + 28,
        shifted_slot,
    );
    OpenOptions::new()
        .write(true)
        .open(commit_fixture.path())
        .expect("open fixture to extend")
        .set_len(header.fields[APPEND_LOG_START] + 1)
        .expect("extend fixture");
    assert_invalid_layout(spec, commit_fixture.path());

    let slot_fixture = TempMatrix::new("matrix_hardening_slot_len");
    create_empty(spec, slot_fixture.path());
    let header = read_vmat_header(slot_fixture.path(), spec);
    patch_header_u64(
        slot_fixture.path(),
        header,
        SLOT_REGION_LEN,
        header.fields[SLOT_REGION_LEN] + 1,
    );
    patch_header_u64(
        slot_fixture.path(),
        header,
        APPEND_LOG_START,
        header.fields[APPEND_LOG_START] + 1,
    );
    OpenOptions::new()
        .write(true)
        .open(slot_fixture.path())
        .expect("open fixture to extend")
        .set_len(header.fields[APPEND_LOG_START] + 1)
        .expect("extend fixture");
    assert_invalid_layout(spec, slot_fixture.path());
}

#[test]
fn header_counts_and_descriptor_table_lengths_are_exact() {
    let spec = matrix_spec(varve::IntegrityPolicy::None);
    for (offset, actual) in [
        (DIMENSION_COUNT_OFFSET, 2),
        (BLOCK_COUNT_OFFSET, 1),
        (COMMIT_COUNT_OFFSET, 3),
    ] {
        let fixture = TempMatrix::new("matrix_hardening_header_count");
        create_empty(spec, fixture.path());
        let header = read_vmat_header(fixture.path(), spec);
        patch_u32(fixture.path(), header.offset + offset, actual + 1);
        assert_invalid_layout(spec, fixture.path());
    }

    for index in [DIMENSION_TABLE_LEN, BLOCK_TABLE_LEN, COMMIT_CATEGORY_LEN] {
        let fixture = TempMatrix::new("matrix_hardening_table_len");
        create_empty(spec, fixture.path());
        let header = read_vmat_header(fixture.path(), spec);
        patch_header_u64(fixture.path(), header, index, header.fields[index] + 1);
        assert_invalid_layout(spec, fixture.path());
    }
}

#[test]
fn zero_sized_matrix_create_and_open_reject_cell_access_before_slot_io() {
    let spec = matrix_spec(varve::IntegrityPolicy::None);
    let fixture = TempMatrix::new("matrix_hardening_zero_sized");
    {
        let dimensions = MatrixDimensions::from_pairs([("scan", 2), ("ch", 0)]);
        let mut writer = spec
            .create_writer_with_dims(fixture.path(), dimensions)
            .expect("create zero-sized matrix");
        assert!(matches!(
            writer.write_matrix_cell(MatrixKey::new(0, 0), &PrimaryCell { value: 1 }),
            Err(Error::MatrixKeyOutOfBounds { scan: 0, ch: 0 })
        ));
        writer
            .set_matrix_single_committed("master_grid", true)
            .expect("commit independent single");
        writer.flush().expect("flush zero-sized matrix");
    }

    let reader = spec
        .open_reader(fixture.path())
        .expect("reopen zero-sized matrix");
    assert!(matches!(
        reader.matrix_cell_status::<PrimaryCell>(MatrixKey::new(0, 0)),
        Err(Error::MatrixKeyOutOfBounds { scan: 0, ch: 0 })
    ));
    assert!(
        reader
            .is_matrix_single_committed("master_grid")
            .expect("read independent single")
    );
    assert_eq!(
        reader
            .matrix_resume_signal("analysis")
            .expect("read zero-sized resume signal"),
        MatrixResumeSignal::Clean
    );
    assert!(matches!(
        reader.is_matrix_channel_committed("threshold", 0),
        Err(Error::MatrixCommitMissing(category)) if category == "threshold"
    ));
}

#[cfg(feature = "integrity")]
#[test]
fn crc_region_length_must_match_the_dimension_derived_extent() {
    let spec = matrix_spec(varve::IntegrityPolicy::Crc32);
    let fixture = TempMatrix::new("matrix_hardening_crc_len");
    create_empty(spec, fixture.path());
    let header = read_vmat_header(fixture.path(), spec);

    patch_header_u64(
        fixture.path(),
        header,
        REGION_CRC_LEN,
        header.fields[REGION_CRC_LEN] + 1,
    );
    patch_header_u64(
        fixture.path(),
        header,
        APPEND_LOG_START,
        header.fields[APPEND_LOG_START] + 1,
    );
    OpenOptions::new()
        .write(true)
        .open(fixture.path())
        .expect("open fixture to extend")
        .set_len(header.fields[APPEND_LOG_START] + 1)
        .expect("extend fixture");

    assert_invalid_layout(spec, fixture.path());
}

#[cfg(feature = "integrity")]
#[test]
fn quarantine_recommendations_match_available_recovery_paths() {
    let spec = matrix_spec(varve::IntegrityPolicy::Crc32);
    for (category, map_index, expected) in [
        (
            "analysis",
            0u64,
            MatrixRecoveryAction::RebuildCommitMap {
                category: Some("analysis".to_string()),
            },
        ),
        (
            "master_grid",
            1,
            MatrixRecoveryAction::ClearCategory {
                category: "master_grid".to_string(),
            },
        ),
        (
            "threshold",
            2,
            MatrixRecoveryAction::ClearCategory {
                category: "threshold".to_string(),
            },
        ),
    ] {
        let fixture = TempMatrix::new("matrix_hardening_recommendation");
        create_empty(spec, fixture.path());
        let header = read_vmat_header(fixture.path(), spec);
        let mut file = OpenOptions::new()
            .write(true)
            .open(fixture.path())
            .expect("open commit map for corruption");
        file.seek(SeekFrom::Start(header.fields[COMMIT_MAP_OFF] + map_index))
            .expect("seek commit category");
        file.write_all(&[0x80]).expect("corrupt commit category");
        drop(file);

        let reader = spec
            .open_reader(fixture.path())
            .expect("open quarantined category");
        let report = reader.matrix_recovery_report();
        assert_eq!(
            report.recommended_actions,
            vec![expected],
            "unexpected recommendation for {category}"
        );
        match category {
            "analysis" => assert!(matches!(
                reader.matrix_cell_status::<PrimaryCell>(MatrixKey::new(0, 0)),
                Err(Error::MatrixCommitQuarantined(name)) if name == "analysis"
            )),
            "master_grid" => assert!(matches!(
                reader.is_matrix_single_committed("master_grid"),
                Err(Error::MatrixCommitQuarantined(name)) if name == "master_grid"
            )),
            "threshold" => assert!(matches!(
                reader.is_matrix_channel_committed("threshold", 0),
                Err(Error::MatrixCommitQuarantined(name)) if name == "threshold"
            )),
            _ => unreachable!("unexpected category"),
        }
        drop(reader);

        let mut writer = spec
            .open_writer(fixture.path())
            .expect("open quarantined writer");
        let blocked = match category {
            "analysis" => {
                writer.write_matrix_cell(MatrixKey::new(0, 0), &PrimaryCell { value: 99 })
            }
            "master_grid" => writer.set_matrix_single_committed("master_grid", true),
            "threshold" => writer.set_matrix_channel_committed("threshold", 0, true),
            _ => unreachable!("unexpected category"),
        };
        assert!(matches!(
            blocked,
            Err(Error::MatrixCommitQuarantined(name)) if name == category
        ));
        writer
            .clear_matrix_category(category)
            .expect("whole-category recovery remains available");
    }
}

#[cfg(feature = "integrity")]
#[test]
fn corrupt_commit_maps_are_quarantined_until_whole_category_recovery() {
    let spec = matrix_spec(varve::IntegrityPolicy::Crc32);
    let fixture = TempMatrix::new("matrix_hardening_quarantine");
    let committed_key = MatrixKey::new(1, 0);
    let replacement_key = MatrixKey::new(0, 0);

    {
        let dimensions = MatrixDimensions::from_pairs([("scan", 2), ("ch", 2)]);
        let mut writer = spec
            .create_writer_with_dims(fixture.path(), dimensions)
            .expect("create matrix fixture");
        writer
            .write_matrix_cell(committed_key, &PrimaryCell { value: 11 })
            .expect("write committed cell");
        writer
            .commit_matrix_cell::<PrimaryCell>(committed_key)
            .expect("commit cell");
        writer
            .set_matrix_single_committed("master_grid", true)
            .expect("set single commit");
        writer
            .set_matrix_channel_committed("threshold", 1, true)
            .expect("set channel commit");
        writer.flush().expect("flush fixture");
    }

    let header = read_vmat_header(fixture.path(), spec);
    let commit_map_off = header.fields[COMMIT_MAP_OFF];
    {
        let mut file = OpenOptions::new()
            .write(true)
            .open(fixture.path())
            .expect("open commit map for corruption");
        file.seek(SeekFrom::Start(commit_map_off))
            .expect("seek commit map");
        file.write_all(&[0xFF]).expect("corrupt commit map");
    }
    let corrupt_bytes = read(fixture.path()).expect("read corrupt evidence");

    {
        let mut reader = spec
            .open_reader(fixture.path())
            .expect("open corrupt matrix");
        assert!(matches!(
            reader.matrix_cell_status::<PrimaryCell>(committed_key),
            Err(Error::MatrixCommitQuarantined(name)) if name == "analysis"
        ));
        assert!(matches!(
            reader.matrix_cell_status::<PrimaryCell>(MatrixKey::new(0, 1)),
            Err(Error::MatrixCommitQuarantined(name)) if name == "analysis"
        ));
        assert!(matches!(
            reader.read_matrix_cell::<PrimaryCell>(committed_key),
            Err(Error::MatrixCommitQuarantined(name)) if name == "analysis"
        ));
        assert!(
            reader
                .is_matrix_single_committed("master_grid")
                .expect("read unaffected single")
        );
        assert!(
            reader
                .is_matrix_channel_committed("threshold", 1)
                .expect("read unaffected channel")
        );
        let report = reader.matrix_recovery_report();
        assert!(report.findings.iter().any(|finding| {
            finding.kind == MatrixCorruptionKind::CommitMap
                && finding.severity == MatrixCorruptionSeverity::Recoverable
        }));
    }
    assert_eq!(
        read(fixture.path()).expect("reread raw evidence"),
        corrupt_bytes
    );

    {
        let mut writer = spec
            .open_writer(fixture.path())
            .expect("open corrupt writer");
        assert!(matches!(
            writer.write_matrix_cell(replacement_key, &PrimaryCell { value: 22 }),
            Err(Error::MatrixCommitQuarantined(name)) if name == "analysis"
        ));
    }
    assert_eq!(
        read(fixture.path()).expect("reread blocked publication evidence"),
        corrupt_bytes
    );

    {
        let mut writer = spec
            .open_writer(fixture.path())
            .expect("open recovery writer");
        writer
            .clear_matrix_category("analysis")
            .expect("clear quarantined category");
        assert!(
            !writer
                .matrix_recovery_report()
                .findings
                .iter()
                .any(|finding| finding.kind == MatrixCorruptionKind::CommitMap)
        );
        writer
            .write_matrix_cell(replacement_key, &PrimaryCell { value: 22 })
            .expect("write after recovery");
        writer
            .commit_matrix_cell::<PrimaryCell>(replacement_key)
            .expect("commit after recovery");
        writer.flush().expect("flush recovered matrix");
    }

    {
        let mut reader = spec
            .open_reader(fixture.path())
            .expect("open recovered matrix");
        assert_eq!(
            reader
                .read_matrix_cell::<PrimaryCell>(replacement_key)
                .expect("read replacement cell"),
            PrimaryCell { value: 22 }
        );
        assert_eq!(
            reader
                .matrix_cell_status::<PrimaryCell>(committed_key)
                .expect("read cleared status"),
            MatrixCellStatus::NotCommitted
        );
        assert!(
            reader
                .is_matrix_single_committed("master_grid")
                .expect("read preserved single")
        );
        assert!(
            reader
                .is_matrix_channel_committed("threshold", 1)
                .expect("read preserved channel")
        );
    }
}

#[cfg(feature = "integrity")]
#[test]
fn valid_commit_visibility_and_wire_bytes_are_unchanged_by_open() {
    let spec = matrix_spec(varve::IntegrityPolicy::Crc32);
    let fixture = TempMatrix::new("matrix_hardening_valid_visibility");
    let key = MatrixKey::new(1, 1);
    {
        let dimensions = MatrixDimensions::from_pairs([("scan", 2), ("ch", 2)]);
        let mut writer = spec
            .create_writer_with_dims(fixture.path(), dimensions)
            .expect("create matrix fixture");
        writer
            .write_matrix_cell(key, &PrimaryCell { value: 77 })
            .expect("write visible cell");
        writer
            .commit_matrix_cell::<PrimaryCell>(key)
            .expect("commit visible cell");
        writer
            .set_matrix_single_committed("master_grid", true)
            .expect("set single commit");
        writer
            .set_matrix_channel_committed("threshold", 0, true)
            .expect("set channel commit");
        writer.flush().expect("flush fixture");
    }
    let before = read(fixture.path()).expect("capture valid wire bytes");

    {
        let mut reader = spec.open_reader(fixture.path()).expect("open valid matrix");
        assert_eq!(
            reader
                .read_matrix_cell::<PrimaryCell>(key)
                .expect("read visible cell"),
            PrimaryCell { value: 77 }
        );
        assert!(
            reader
                .is_matrix_single_committed("master_grid")
                .expect("read visible single")
        );
        assert!(
            reader
                .is_matrix_channel_committed("threshold", 0)
                .expect("read visible channel")
        );
    }

    assert_eq!(
        read(fixture.path()).expect("capture reopened bytes"),
        before
    );
}
