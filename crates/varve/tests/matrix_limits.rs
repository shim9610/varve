use std::fs::{OpenOptions, metadata};
use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use varve::{
    BlockDescriptor, BlockKind, Endian, Error, FormatSpec, IndexPolicy, MatrixAuxDescriptor,
    MatrixBlockDescriptor, MatrixCommitDescriptor, MatrixCommitKind, MatrixDimensionDescriptor,
    MatrixDimensions, MatrixKey, ReadLimits, VarveBlock, VarveMatrixBlock,
};

const VMAT_HEADER_LEN: u64 = 160;
#[cfg(feature = "integrity")]
const HEADER_U64_OFFSET: u64 = 24;
#[cfg(feature = "integrity")]
const COMMIT_MAP_OFFSET_INDEX: u64 = 6;
/// Resident bitmap bytes held by the 4x4 fixture once a single cell has been
/// written and committed: the commit-map, checksum-validity, and current-write
/// pages, each a 2-byte short page for a 16-cell block. The budget now charges
/// the pages actually materialised rather than the dense worst case of every
/// map, so this constant tracks residency, not the reserved on-disk extents.
#[cfg(feature = "integrity")]
const TEST_BITMAP_BYTES: u64 = 6;
/// Residency of the same fixture after a reopen: the persisted commit-map and
/// checksum-validity pages only. The current-write map is session state and
/// starts empty at every open.
#[cfg(feature = "integrity")]
const TEST_REOPEN_BITMAP_BYTES: u64 = 4;
/// Residency of a single materialised commit-map page of the same fixture.
#[cfg(feature = "integrity")]
const TEST_COMMIT_PAGE_BYTES: u64 = 2;

static NEXT_TEMP_ID: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Debug, PartialEq, Eq, VarveBlock)]
#[varve(id = 740, version = 1, kind = "matrix")]
struct LimitedCell {
    value: u32,
}

impl VarveMatrixBlock for LimitedCell {
    const DIMENSIONS: [&'static str; 2] = ["scan", "ch"];
    const CATEGORY: &'static str = "analysis";
    const SLOT_STRIDE: u64 = 4;
}

#[derive(Clone, Debug, PartialEq, Eq, VarveBlock)]
#[varve(id = 741, version = 1, kind = "matrix")]
struct SecondLimitedCell {
    value: u32,
}

impl VarveMatrixBlock for SecondLimitedCell {
    const DIMENSIONS: [&'static str; 2] = ["scan", "ch"];
    const CATEGORY: &'static str = "secondary";
    const SLOT_STRIDE: u64 = 4;
}

fn matrix_spec(limits: ReadLimits, integrity: varve::IntegrityPolicy) -> FormatSpec {
    static BLOCKS: &[BlockDescriptor] = &[BlockDescriptor {
        id: LimitedCell::ID,
        name: "LimitedCell",
        version: LimitedCell::VERSION,
        kind: BlockKind::Matrix,
        fields: LimitedCell::FIELDS,
    }];
    static DIMENSIONS: &[MatrixDimensionDescriptor] = &[
        MatrixDimensionDescriptor { name: "scan" },
        MatrixDimensionDescriptor { name: "ch" },
    ];
    static COMMITS: &[MatrixCommitDescriptor] = &[MatrixCommitDescriptor {
        name: LimitedCell::CATEGORY,
        kind: MatrixCommitKind::Cell,
    }];
    static MATRIX_BLOCKS: &[MatrixBlockDescriptor] = &[MatrixBlockDescriptor {
        block_id: LimitedCell::ID,
        dimensions: LimitedCell::DIMENSIONS,
        category: LimitedCell::CATEGORY,
        slot_stride: LimitedCell::SLOT_STRIDE,
    }];
    static AUX: &[MatrixAuxDescriptor] = &[MatrixAuxDescriptor {
        name: "thumbnail",
        byte_len: 16,
    }];

    FormatSpec::new(
        b"MLIM",
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
    .with_matrix_aux(AUX)
    .with_read_limits(limits)
}

fn two_block_spec(limits: ReadLimits) -> FormatSpec {
    static BLOCKS: &[BlockDescriptor] = &[
        BlockDescriptor {
            id: LimitedCell::ID,
            name: "LimitedCell",
            version: LimitedCell::VERSION,
            kind: BlockKind::Matrix,
            fields: LimitedCell::FIELDS,
        },
        BlockDescriptor {
            id: SecondLimitedCell::ID,
            name: "SecondLimitedCell",
            version: SecondLimitedCell::VERSION,
            kind: BlockKind::Matrix,
            fields: SecondLimitedCell::FIELDS,
        },
    ];
    static DIMENSIONS: &[MatrixDimensionDescriptor] = &[
        MatrixDimensionDescriptor { name: "scan" },
        MatrixDimensionDescriptor { name: "ch" },
    ];
    static COMMITS: &[MatrixCommitDescriptor] = &[
        MatrixCommitDescriptor {
            name: LimitedCell::CATEGORY,
            kind: MatrixCommitKind::Cell,
        },
        MatrixCommitDescriptor {
            name: SecondLimitedCell::CATEGORY,
            kind: MatrixCommitKind::Cell,
        },
    ];
    static MATRIX_BLOCKS: &[MatrixBlockDescriptor] = &[
        MatrixBlockDescriptor {
            block_id: LimitedCell::ID,
            dimensions: LimitedCell::DIMENSIONS,
            category: LimitedCell::CATEGORY,
            slot_stride: LimitedCell::SLOT_STRIDE,
        },
        MatrixBlockDescriptor {
            block_id: SecondLimitedCell::ID,
            dimensions: SecondLimitedCell::DIMENSIONS,
            category: SecondLimitedCell::CATEGORY,
            slot_stride: SecondLimitedCell::SLOT_STRIDE,
        },
    ];

    FormatSpec::new(
        b"MLM2",
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
    .with_read_limits(limits)
}

struct TempMatrix {
    path: PathBuf,
    _dir: tempfile::TempDir,
}

impl TempMatrix {
    fn new(name: &str) -> Self {
        let id = NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed);
        let dir = tempfile::tempdir().expect("create per-test temp directory");
        let path = dir
            .path()
            .join(format!("varve-{name}-{}-{id}.vrv", std::process::id()));
        Self { path, _dir: dir }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

fn high_limits() -> ReadLimits {
    ReadLimits::finite_all(u64::MAX)
}

fn expect_limit(error: Error, resource: &'static str, limit: u64) {
    assert!(matches!(
        error,
        Error::LimitExceeded {
            resource: actual_resource,
            limit: actual_limit,
            ..
        } if actual_resource == resource && actual_limit == limit
    ));
}

fn create_error(limits: ReadLimits, name: &str) -> (Error, u64) {
    let fixture = TempMatrix::new(name);
    let dimensions = MatrixDimensions::from_pairs([("scan", 4), ("ch", 4)]);
    let error = match matrix_spec(limits, varve::IntegrityPolicy::None)
        .create_with_dims(fixture.path(), dimensions)
    {
        Ok(_) => panic!("matrix creation unexpectedly passed its resource limit"),
        Err(error) => error,
    };
    let file_len = metadata(fixture.path())
        .map(|value| value.len())
        .unwrap_or(0);
    (error, file_len)
}

#[test]
fn create_checks_all_non_crc_matrix_limits_before_preallocation() {
    let cases = [
        (
            high_limits().with_max_matrix_dimension(3),
            "matrix dimension",
            3,
        ),
        (high_limits().with_max_matrix_cells(15), "matrix cells", 15),
        (
            high_limits().with_max_matrix_metadata_bytes(0),
            "matrix metadata bytes",
            0,
        ),
        (
            high_limits().with_max_matrix_slot_region_len(63),
            "matrix slot region length",
            63,
        ),
        (high_limits().with_max_file_len(1), "file length", 1),
    ];

    for (index, (limits, resource, limit)) in cases.into_iter().enumerate() {
        let (error, file_len) = create_error(limits, &format!("create-limit-{index}"));
        expect_limit(error, resource, limit);
        assert!(
            file_len < 1024,
            "failed create unexpectedly grew to {file_len}"
        );
    }
}

#[test]
fn aggregate_cell_and_slot_limits_cover_every_matrix_block() {
    let dimensions = MatrixDimensions::from_pairs([("scan", 4), ("ch", 4)]);

    let cell_fixture = TempMatrix::new("aggregate-cells");
    let error = match two_block_spec(high_limits().with_max_matrix_cells(20))
        .create_with_dims(cell_fixture.path(), dimensions.clone())
    {
        Ok(_) => panic!("aggregate cell limit unexpectedly passed"),
        Err(error) => error,
    };
    expect_limit(error, "matrix cells", 20);

    let slot_fixture = TempMatrix::new("aggregate-slots");
    let error = match two_block_spec(high_limits().with_max_matrix_slot_region_len(100))
        .create_with_dims(slot_fixture.path(), dimensions)
    {
        Ok(_) => panic!("aggregate slot limit unexpectedly passed"),
        Err(error) => error,
    };
    expect_limit(error, "matrix slot region length", 100);
}

#[cfg(feature = "integrity")]
#[test]
fn create_checks_crc_region_and_resident_validity_bytes_before_allocation() -> varve::Result<()> {
    let fixture = TempMatrix::new("crc-limit");
    let dimensions = MatrixDimensions::from_pairs([("scan", 4), ("ch", 4)]);
    let error = match matrix_spec(
        high_limits().with_max_matrix_crc_bytes(0),
        varve::IntegrityPolicy::Crc32,
    )
    .create_with_dims(fixture.path(), dimensions)
    {
        Ok(_) => panic!("CRC-limited matrix creation unexpectedly passed"),
        Err(error) => error,
    };
    expect_limit(error, "matrix checksum bytes", 0);
    assert!(metadata(fixture.path())?.len() < 1024);
    Ok(())
}

#[test]
fn hostile_dimension_is_rejected_before_derived_bitmap_or_slot_work() -> varve::Result<()> {
    let fixture = TempMatrix::new("hostile-dimension");
    let dimensions = MatrixDimensions::from_pairs([("scan", 2), ("ch", 2)]);
    drop(
        matrix_spec(high_limits(), varve::IntegrityPolicy::None)
            .create_with_dims(fixture.path(), dimensions)?,
    );

    // Native file header + 24-byte matrix creation-nonce region (DUR2-03).
    let header_offset = 18 + 24 + u64::try_from(b"MLIM".len()).expect("magic length");
    let dimension_value_offset = header_offset + VMAT_HEADER_LEN + 2 + 4;
    let mut file = OpenOptions::new().write(true).open(fixture.path())?;
    file.seek(SeekFrom::Start(dimension_value_offset))?;
    file.write_all(&1_000_000u64.to_le_bytes())?;
    drop(file);

    let error = match matrix_spec(
        high_limits().with_max_matrix_dimension(100),
        varve::IntegrityPolicy::None,
    )
    .open_readonly(fixture.path())
    {
        Ok(_) => panic!("hostile dimension unexpectedly opened"),
        Err(error) => error,
    };
    expect_limit(error, "matrix dimension", 100);
    Ok(())
}

#[test]
fn aux_reads_check_file_and_payload_limits_before_allocation() -> varve::Result<()> {
    let fixture = TempMatrix::new("aux-limit");
    let spec = matrix_spec(high_limits(), varve::IntegrityPolicy::None);
    let dimensions = MatrixDimensions::from_pairs([("scan", 2), ("ch", 2)]);
    drop(spec.create_with_dims(fixture.path(), dimensions)?);

    let runtime = high_limits().with_max_record_payload_len(3);
    let mut reader = spec.open_reader_with_limits(fixture.path(), runtime)?;
    let error = reader
        .read_matrix_aux("thumbnail", 0, 4)
        .expect_err("oversized aux read must fail");
    expect_limit(error, "record payload length", 3);
    drop(reader);

    let mut writer = spec.open_writer_with_limits(fixture.path(), runtime)?;
    let error = writer
        .write_matrix_aux("thumbnail", 0, &[1, 2, 3, 4])
        .expect_err("oversized aux write must fail");
    expect_limit(error, "record payload length", 3);
    Ok(())
}

#[test]
fn cell_reads_check_materialization_limit_before_allocating_slot_bytes() -> varve::Result<()> {
    let fixture = TempMatrix::new("cell-materialization-limit");
    let spec = matrix_spec(high_limits(), varve::IntegrityPolicy::None);
    let dimensions = MatrixDimensions::from_pairs([("scan", 1), ("ch", 1)]);
    let key = MatrixKey::new(0, 0);
    let mut writer = spec.create_with_dims(fixture.path(), dimensions)?;
    writer.write_matrix_cell(key, &LimitedCell { value: 7 })?;
    writer.commit_matrix_cell::<LimitedCell>(key)?;
    drop(writer);

    let runtime = high_limits().with_max_materialized_bytes(3);
    let mut reader = spec.open_reader_with_limits(fixture.path(), runtime)?;
    let error = reader
        .matrix_cell_payload::<LimitedCell>(key)
        .expect_err("slot payload must be rejected before its four-byte allocation");
    expect_limit(error, "materialized bytes", 3);

    let error = reader
        .read_matrix_cell::<LimitedCell>(key)
        .expect_err("typed cell must be rejected before its four-byte allocation");
    expect_limit(error, "materialized bytes", 3);
    Ok(())
}

#[test]
fn nonzero_vmat_flags_are_rejected_as_noncanonical() -> varve::Result<()> {
    let fixture = TempMatrix::new("header-flags");
    let spec = matrix_spec(high_limits(), varve::IntegrityPolicy::None);
    let dimensions = MatrixDimensions::from_pairs([("scan", 2), ("ch", 2)]);
    drop(spec.create_with_dims(fixture.path(), dimensions)?);

    // Native file header + 24-byte matrix creation-nonce region (DUR2-03).
    let header_offset = 18 + 24 + u64::try_from(spec.magic.len()).expect("magic length");
    let mut file = OpenOptions::new().write(true).open(fixture.path())?;
    file.seek(SeekFrom::Start(header_offset + 6))?;
    file.write_all(&1u16.to_le_bytes())?;
    drop(file);

    assert!(matches!(
        spec.open_readonly(fixture.path()),
        Err(Error::InvalidMatrixLayout)
    ));
    Ok(())
}

#[cfg(feature = "integrity")]
#[test]
fn quarantined_commit_map_is_charged_to_the_resident_budget_as_it_is_read() -> varve::Result<()> {
    let fixture = TempMatrix::new("quarantine-bitmap-limit");
    let spec = matrix_spec(high_limits(), varve::IntegrityPolicy::Crc32);
    let dimensions = MatrixDimensions::from_pairs([("scan", 4), ("ch", 4)]);
    drop(spec.create_with_dims(fixture.path(), dimensions)?);

    // Native file header + 24-byte matrix creation-nonce region (DUR2-03).
    let header_offset = 18 + 24 + u64::try_from(spec.magic.len()).expect("magic length");
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(fixture.path())?;
    file.seek(SeekFrom::Start(
        header_offset + HEADER_U64_OFFSET + COMMIT_MAP_OFFSET_INDEX * 8,
    ))?;
    let mut offset = [0; 8];
    std::io::Read::read_exact(&mut file, &mut offset)?;
    file.seek(SeekFrom::Start(u64::from_le_bytes(offset)))?;
    file.write_all(&[1])?;
    drop(file);

    // The corrupted byte makes the first commit-map page carry state, so the
    // page is materialised and charged as it is read, before the quarantined
    // map is retained. A budget below one page must refuse the open.
    let error = match matrix_spec(
        high_limits().with_max_matrix_bitmap_bytes(TEST_COMMIT_PAGE_BYTES - 1),
        varve::IntegrityPolicy::Crc32,
    )
    .open_readonly(fixture.path())
    {
        Ok(_) => panic!("quarantined bitmap unexpectedly fit the resident budget"),
        Err(error) => error,
    };
    expect_limit(error, "matrix bitmap bytes", TEST_COMMIT_PAGE_BYTES - 1);

    // A budget that does cover the page admits the same file, so the refusal
    // above is the budget and not the corruption.
    drop(
        matrix_spec(
            high_limits().with_max_matrix_bitmap_bytes(TEST_COMMIT_PAGE_BYTES),
            varve::IntegrityPolicy::Crc32,
        )
        .open_readonly(fixture.path())?,
    );
    Ok(())
}

/// The resident bitmap budget is charged where residency is actually taken: as
/// commit-map and checksum-validity pages are materialised by writes. Creation
/// itself makes nothing resident, whatever the cell count.
#[cfg(feature = "integrity")]
#[test]
fn resident_bitmap_budget_is_charged_as_pages_are_materialized() -> varve::Result<()> {
    let dimensions = MatrixDimensions::from_pairs([("scan", 4), ("ch", 4)]);

    // A budget of zero still admits creation, because creation is free.
    let tight = TempMatrix::new("resident-bitmap-tight");
    let mut writer = matrix_spec(
        high_limits().with_max_matrix_bitmap_bytes(0),
        varve::IntegrityPolicy::Crc32,
    )
    .create_writer_with_dims(tight.path(), dimensions.clone())?;
    // ...but the first write, which materialises the current-write page, must
    // be refused by that budget.
    let error = match writer.write_matrix_cell(MatrixKey::new(0, 0), &LimitedCell { value: 1 }) {
        Ok(()) => panic!("write unexpectedly fit a zero resident bitmap budget"),
        Err(error) => error,
    };
    expect_limit(error, "matrix bitmap bytes", 0);
    drop(writer);

    // A budget that covers both pages admits the same write and commit, and the
    // reopen is charged the same residency.
    let roomy = TempMatrix::new("resident-bitmap-roomy");
    let spec = matrix_spec(
        high_limits().with_max_matrix_bitmap_bytes(TEST_BITMAP_BYTES),
        varve::IntegrityPolicy::Crc32,
    );
    let mut writer = spec.create_writer_with_dims(roomy.path(), dimensions)?;
    writer.write_matrix_cell(MatrixKey::new(0, 0), &LimitedCell { value: 1 })?;
    writer.commit_matrix_cell::<LimitedCell>(MatrixKey::new(0, 0))?;
    writer.flush()?;
    drop(writer);
    drop(spec.open_readonly(roomy.path())?);

    // One byte less than the persisted pages actually hold refuses the reopen.
    let error = match matrix_spec(
        high_limits().with_max_matrix_bitmap_bytes(TEST_REOPEN_BITMAP_BYTES - 1),
        varve::IntegrityPolicy::Crc32,
    )
    .open_readonly(roomy.path())
    {
        Ok(_) => panic!("reopen unexpectedly fit an under-sized resident budget"),
        Err(error) => error,
    };
    expect_limit(error, "matrix bitmap bytes", TEST_REOPEN_BITMAP_BYTES - 1);
    Ok(())
}
