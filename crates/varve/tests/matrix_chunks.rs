//! Growing matrix dimensions (`.internal-docs/matrix-chunk-spec.md`).
//!
//! A matrix's dimensions are otherwise fixed at create, which cannot model a
//! stream whose extent is unknown when the file is made — you learn a grid is
//! full when it fills. Declaring a growing dimension makes rows past the
//! declared extent land in *chunks*: ordinary internal records in the append
//! log, each holding `rows_per_chunk` rows of every matrix block, and each with
//! the matrix region's layout. Chunk 0 **is** the region.
//!
//! What these tests pin, in the order §6 of the spec lists them: the option is
//! inert when off; growth needs no advance knowledge; a value that never
//! arrived stays absent across a seal rather than blocking it; a late write
//! into a sealed chunk is refused rather than dropped; and a cell read does not
//! materialise the chunk it lives in.

use std::path::{Path, PathBuf};

use varve::{
    BlockDescriptor, BlockKind, Endian, Error, FormatSpec, IndexPolicy, IntegrityPolicy,
    MatrixBlockDescriptor, MatrixCellStatus, MatrixCommitDescriptor, MatrixCommitKind,
    MatrixDimensionDescriptor, MatrixDimensions, MatrixKey, ReadLimits, VarveBlock,
    VarveMatrixBlock,
};

const CHANNELS: u64 = 8;
const ROWS_PER_CHUNK: u64 = 4;

#[derive(Clone, Debug, PartialEq, Eq, VarveBlock)]
#[varve(id = 810, version = 1, kind = "matrix")]
struct Sample {
    value: u32,
}

impl VarveMatrixBlock for Sample {
    const DIMENSIONS: [&'static str; 2] = ["scan", "ch"];
    const CATEGORY: &'static str = "sample";
    const SLOT_STRIDE: u64 = 4;
}

/// A second block on the same grid, so "only part of a cell arrived" is
/// expressible: `Sample` may be present where `Marker` is not.
#[derive(Clone, Debug, PartialEq, Eq, VarveBlock)]
#[varve(id = 811, version = 1, kind = "matrix")]
struct Marker {
    flag: u32,
}

impl VarveMatrixBlock for Marker {
    const DIMENSIONS: [&'static str; 2] = ["scan", "ch"];
    const CATEGORY: &'static str = "marker";
    const SLOT_STRIDE: u64 = 4;
}

static BLOCKS: &[BlockDescriptor] = &[
    BlockDescriptor {
        id: Sample::ID,
        name: "Sample",
        version: Sample::VERSION,
        kind: BlockKind::Matrix,
        fields: &[],
    },
    BlockDescriptor {
        id: Marker::ID,
        name: "Marker",
        version: Marker::VERSION,
        kind: BlockKind::Matrix,
        fields: &[],
    },
];
static DIMS: &[MatrixDimensionDescriptor] = &[
    MatrixDimensionDescriptor { name: "scan" },
    MatrixDimensionDescriptor { name: "ch" },
];
static COMMITS: &[MatrixCommitDescriptor] = &[
    MatrixCommitDescriptor {
        name: Sample::CATEGORY,
        kind: MatrixCommitKind::Cell,
    },
    MatrixCommitDescriptor {
        name: Marker::CATEGORY,
        kind: MatrixCommitKind::Cell,
    },
];
static MATRIX_BLOCKS: &[MatrixBlockDescriptor] = &[
    MatrixBlockDescriptor {
        block_id: Sample::ID,
        dimensions: Sample::DIMENSIONS,
        category: Sample::CATEGORY,
        slot_stride: Sample::SLOT_STRIDE,
    },
    MatrixBlockDescriptor {
        block_id: Marker::ID,
        dimensions: Marker::DIMENSIONS,
        category: Marker::CATEGORY,
        slot_stride: Marker::SLOT_STRIDE,
    },
];

fn base_spec() -> FormatSpec {
    FormatSpec::new(
        b"GROWMTX0",
        1,
        Endian::Little,
        0,
        IndexPolicy::ScanOnOpen,
        IntegrityPolicy::Crc32,
        varve::RecoveryPolicy::Strict,
        varve::ManifestPolicy::None,
        BLOCKS,
    )
    .with_read_limits(ReadLimits::finite_all(u64::MAX))
    .with_matrix_spec(DIMS, COMMITS, MATRIX_BLOCKS)
}

/// The same format with the growing dimension declared.
fn growing_spec() -> FormatSpec {
    base_spec().with_growing_matrix_dimension("scan", ROWS_PER_CHUNK)
}

fn dims() -> MatrixDimensions {
    MatrixDimensions::from_pairs([("scan", ROWS_PER_CHUNK), ("ch", CHANNELS)])
}

fn key(row: u64, ch: u64) -> MatrixKey {
    MatrixKey::new(row, ch)
}

// ---------------------------------------------------------------------------
// §6.1 Inertness
// ---------------------------------------------------------------------------

#[test]
fn the_option_defaults_off_and_changes_no_hash() {
    assert!(base_spec().growing_rows_per_chunk().is_none());
    assert_eq!(
        growing_spec().growing_rows_per_chunk(),
        Some(ROWS_PER_CHUNK)
    );
    // A spec that declares no growing dimension must hash to exactly what it
    // hashed to before the declaration existed.
    assert_ne!(
        growing_spec().computed_schema_hash(),
        base_spec().computed_schema_hash(),
        "a file with chunks is not the file the plain spec describes",
    );
}

/// The hash of the plain spec, read off the build **before** this feature
/// existed (`f1f4339`, via a scratch test that printed it and was deleted).
///
/// A literal, not a comparison against a freshly built spec: comparing the new
/// code against itself proves the fold is consistent, not that it is absent.
const BASE_SCHEMA_HASH: u64 = 0x2f14_5461_495b_712b;

#[test]
fn a_spec_that_declares_nothing_hashes_as_it_did_before() {
    assert_eq!(base_spec().computed_schema_hash(), BASE_SCHEMA_HASH);
}

#[test]
fn a_format_without_the_declaration_writes_no_chunk_record() -> varve::Result<()> {
    // Byte-for-byte file comparison is not available here and saying so is the
    // point: a matrix file carries a random per-create nonce (`VMNC`), so two
    // matrix files are never identical however inert the writer is. What is
    // assertable is that the plain writer produces no chunk record and hashes
    // to the literal above.
    let path = temp_path("inert_no_chunks");
    let mut writer = base_spec().create_writer_with_dims(path.path(), dims())?;
    for row in 0..ROWS_PER_CHUNK {
        writer.write_matrix_cell(key(row, 0), &Sample { value: 7 })?;
        writer.commit_matrix_cell::<Sample>(key(row, 0))?;
    }
    writer.flush()?;
    drop(writer);

    let reader = base_spec().open_reader(path.path())?;
    assert!(
        !reader
            .index_entries()
            .iter()
            .any(|entry| entry.block_id == varve::MATRIX_CHUNK_BLOCK_ID),
    );
    Ok(())
}

#[test]
fn a_format_without_the_declaration_refuses_a_row_past_the_extent() -> varve::Result<()> {
    let path = temp_path("no_growth_refuses");
    let mut writer = base_spec().create_writer_with_dims(path.path(), dims())?;
    // This is the limitation the feature exists to lift, pinned so the "before"
    // is not folklore.
    assert!(matches!(
        writer.write_matrix_cell(key(ROWS_PER_CHUNK, 0), &Sample { value: 1 }),
        Err(Error::MatrixKeyOutOfBounds { .. }),
    ));
    Ok(())
}

// ---------------------------------------------------------------------------
// §6.2 Growth without advance knowledge
// ---------------------------------------------------------------------------

#[test]
fn rows_past_the_declared_extent_are_written_and_read_back() -> varve::Result<()> {
    let path = temp_path("growth");
    // Ten times the declared extent, and nothing anywhere named that number
    // before the writes happened.
    let rows = ROWS_PER_CHUNK * 10;
    {
        let mut writer = growing_spec().create_writer_with_dims(path.path(), dims())?;
        for row in 0..rows {
            for ch in 0..CHANNELS {
                let value = (row * CHANNELS + ch) as u32 + 1;
                writer.write_matrix_cell(key(row, ch), &Sample { value })?;
                writer.commit_matrix_cell::<Sample>(key(row, ch))?;
            }
        }
        writer.flush()?;
    }

    let reader = growing_spec().open_reader(path.path())?;
    for row in 0..rows {
        for ch in 0..CHANNELS {
            let expected = (row * CHANNELS + ch) as u32 + 1;
            assert_eq!(
                reader.read_matrix_cell::<Sample>(key(row, ch))?,
                Sample { value: expected },
                "row {row} ch {ch}",
            );
        }
    }
    Ok(())
}

#[test]
fn one_chunk_record_is_written_per_chunk_that_holds_a_value() -> varve::Result<()> {
    let path = temp_path("chunk_count");
    {
        let mut writer = growing_spec().create_writer_with_dims(path.path(), dims())?;
        // Chunk 0 is the region. Rows here reach chunks 1, 2 and 3.
        for row in [ROWS_PER_CHUNK, ROWS_PER_CHUNK * 2, ROWS_PER_CHUNK * 3] {
            writer.write_matrix_cell(key(row, 0), &Sample { value: 5 })?;
            writer.commit_matrix_cell::<Sample>(key(row, 0))?;
        }
        writer.flush()?;
    }
    let reader = growing_spec().open_reader(path.path())?;
    let chunks = reader
        .index_entries()
        .iter()
        .filter(|entry| entry.block_id == varve::MATRIX_CHUNK_BLOCK_ID)
        .count();
    assert_eq!(chunks, 3, "one record per chunk that holds a value");
    Ok(())
}

#[test]
fn a_chunk_nothing_committed_is_not_written() -> varve::Result<()> {
    let path = temp_path("empty_chunk_silent");
    let mut writer = growing_spec().create_writer_with_dims(path.path(), dims())?;
    writer.write_matrix_cell(key(ROWS_PER_CHUNK, 0), &Sample { value: 9 })?;
    writer.commit_matrix_cell::<Sample>(key(ROWS_PER_CHUNK, 0))?;
    writer.flush()?;
    let after_first = std::fs::metadata(path.path())?.len();
    // The failure this guards is the one an empty segment record once had: an
    // idle flush that grows the file every time it runs.
    for _ in 0..8 {
        writer.flush()?;
    }
    assert_eq!(std::fs::metadata(path.path())?.len(), after_first);
    Ok(())
}

// ---------------------------------------------------------------------------
// §6.3 Partial arrival survives sealing
// ---------------------------------------------------------------------------

#[test]
fn a_block_that_never_arrived_reads_as_absent_across_a_seal() -> varve::Result<()> {
    let path = temp_path("partial_arrival");
    let cell = key(ROWS_PER_CHUNK + 1, 3);
    {
        let mut writer = growing_spec().create_writer_with_dims(path.path(), dims())?;
        // `Sample` arrives for this cell. `Marker` never does — and sealing asks
        // no question about that.
        writer.write_matrix_cell(cell, &Sample { value: 42 })?;
        writer.commit_matrix_cell::<Sample>(cell)?;
        // Move past the chunk, which seals it with `Marker` still absent.
        writer.write_matrix_cell(key(ROWS_PER_CHUNK * 2, 0), &Sample { value: 1 })?;
        writer.commit_matrix_cell::<Sample>(key(ROWS_PER_CHUNK * 2, 0))?;
        writer.flush()?;
    }

    let reader = growing_spec().open_reader(path.path())?;
    assert_eq!(
        reader.read_matrix_cell::<Sample>(cell)?,
        Sample { value: 42 }
    );
    assert_eq!(
        reader.matrix_cell_status::<Marker>(cell)?,
        MatrixCellStatus::NotCommitted,
    );
    assert!(matches!(
        reader.read_matrix_cell::<Marker>(cell),
        Err(Error::MatrixNotCommitted),
    ));
    Ok(())
}

#[test]
fn an_uncommitted_cell_in_a_sealed_chunk_stays_uncommitted() -> varve::Result<()> {
    let path = temp_path("uncommitted_cell");
    let written = key(ROWS_PER_CHUNK, 0);
    let never = key(ROWS_PER_CHUNK, 5);
    {
        let mut writer = growing_spec().create_writer_with_dims(path.path(), dims())?;
        writer.write_matrix_cell(written, &Sample { value: 3 })?;
        writer.commit_matrix_cell::<Sample>(written)?;
        writer.flush()?;
    }
    let reader = growing_spec().open_reader(path.path())?;
    assert_eq!(
        reader.matrix_cell_status::<Sample>(written)?,
        MatrixCellStatus::Committed
    );
    assert_eq!(
        reader.matrix_cell_status::<Sample>(never)?,
        MatrixCellStatus::NotCommitted
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// §6.4 A late write into a sealed chunk is refused
// ---------------------------------------------------------------------------

#[test]
fn a_write_into_a_sealed_chunk_is_refused_and_changes_nothing() -> varve::Result<()> {
    let path = temp_path("sealed_refusal");
    let mut writer = growing_spec().create_writer_with_dims(path.path(), dims())?;
    writer.write_matrix_cell(key(ROWS_PER_CHUNK, 0), &Sample { value: 1 })?;
    writer.commit_matrix_cell::<Sample>(key(ROWS_PER_CHUNK, 0))?;
    // Opening chunk 2 seals chunk 1.
    writer.write_matrix_cell(key(ROWS_PER_CHUNK * 2, 0), &Sample { value: 2 })?;
    writer.commit_matrix_cell::<Sample>(key(ROWS_PER_CHUNK * 2, 0))?;
    writer.flush()?;
    let before = std::fs::read(path.path())?;

    // Late data is reported, not dropped: a value that silently does not arrive
    // is indistinguishable from one that was never sent.
    assert!(matches!(
        writer.write_matrix_cell(key(ROWS_PER_CHUNK, 1), &Sample { value: 3 }),
        Err(Error::MatrixChunkSealed { chunk: 1, open: 2 }),
    ));
    writer.flush()?;
    assert_eq!(std::fs::read(path.path())?, before);
    Ok(())
}

// ---------------------------------------------------------------------------
// §6.7 A cell read does not materialise its chunk
// ---------------------------------------------------------------------------

#[cfg(feature = "scalable-fault-injection")]
#[test]
fn reading_one_cell_does_not_read_the_chunk() -> varve::Result<()> {
    use varve::VarveFile;

    // A chunk far larger than a cell, so "reads the cell" and "reads the chunk"
    // are separated by three orders of magnitude and the assertion below can be
    // an absolute byte count rather than a ratio.
    const WIDE_ROWS: u64 = 4_096;
    let spec = base_spec().with_growing_matrix_dimension("scan", WIDE_ROWS);
    let wide_dims = MatrixDimensions::from_pairs([("scan", WIDE_ROWS), ("ch", CHANNELS)]);
    let chunk_slot_bytes = WIDE_ROWS * CHANNELS * Sample::SLOT_STRIDE;

    let path = temp_path("positional_read");
    let cell = key(WIDE_ROWS + 7, 2);
    {
        let mut writer = spec.create_writer_with_dims(path.path(), wide_dims)?;
        writer.write_matrix_cell(cell, &Sample { value: 77 })?;
        writer.commit_matrix_cell::<Sample>(cell)?;
        // Seal by moving on, so the read below goes to a record and not to the
        // writer's own buffer.
        writer.write_matrix_cell(key(WIDE_ROWS * 2, 0), &Sample { value: 1 })?;
        writer.commit_matrix_cell::<Sample>(key(WIDE_ROWS * 2, 0))?;
        writer.flush()?;
    }

    let reader = spec.open_reader(path.path())?;
    let before = VarveFile::chunk_bytes_read();
    assert_eq!(
        reader.read_matrix_cell::<Sample>(cell)?,
        Sample { value: 77 }
    );
    let read = VarveFile::chunk_bytes_read() - before;

    // Measured 2026-08-04: 189 bytes — a 40-byte prefix, 64 bytes of block
    // descriptors, one commit byte, the 4-byte slot, and the header reads
    // around them. Under a probe that materialises the chunk instead it is
    // 131,261, so this bound discriminates rather than merely passing.
    assert!(
        read < 4096,
        "one cell read {read} bytes; the chunk's slot region alone is \
         {chunk_slot_bytes} bytes",
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// Declaration refusals
// ---------------------------------------------------------------------------

#[test]
fn the_growing_dimension_must_be_declared_and_first() {
    assert!(matches!(
        base_spec()
            .with_growing_matrix_dimension("nonexistent", 4)
            .validate(),
        Err(Error::InvalidFormatSpec(
            "growing matrix dimension is not declared"
        )),
    ));
    // `ordinal = scan * ch_count + ch`, so only dimension 0 can grow without
    // renumbering every cell already written.
    assert!(matches!(
        base_spec()
            .with_growing_matrix_dimension("ch", 4)
            .validate(),
        Err(Error::InvalidFormatSpec(
            "the growing dimension must be dimension 0 of every matrix block"
        )),
    ));
    assert!(matches!(
        base_spec()
            .with_growing_matrix_dimension("scan", 0)
            .validate(),
        Err(Error::InvalidFormatSpec(
            "growing matrix rows_per_chunk must be non-zero"
        )),
    ));
    assert!(growing_spec().validate().is_ok());
}

#[test]
fn the_declared_extent_must_equal_the_chunk_height() -> varve::Result<()> {
    // Chunk 0 *is* the region, and the row-to-chunk map is one division. With a
    // declared extent of 100 and chunks of 4, row 50 would route to chunk 12
    // while the region already held it, and the region's rows 4..100 would
    // become unreachable. Refused at create, where the value first exists.
    let path = temp_path("extent_mismatch");
    let wrong = MatrixDimensions::from_pairs([("scan", ROWS_PER_CHUNK * 25), ("ch", CHANNELS)]);
    assert!(matches!(
        growing_spec().create_writer_with_dims(path.path(), wrong),
        Err(Error::MatrixSizeMismatch {
            expected: ROWS_PER_CHUNK,
            actual: 100
        }),
    ));
    Ok(())
}

#[cfg(feature = "scalable-fault-injection")]
#[test]
fn the_segment_chain_still_frames_commit_points_not_chunks() -> varve::Result<()> {
    use varve::VarveFile;

    // A chunk is an ordinary record, which is the whole design — so it must not
    // cost the segment chain anything. Open frames one record per commit point
    // on a file full of chunks, exactly as it does on a file without any.
    let spec = growing_spec()
        .with_index_policy(IndexPolicy::new(true, false, true, false).with_segment_on_flush(true))
        .with_commit_policy(varve::CommitPolicy::TransactionMarker(
            varve::TransactionMarkerMode::OnFlush,
        ));
    let path = temp_path("chain_with_chunks");
    let commit_points = 4u64;
    {
        let mut writer = spec.create_writer_with_dims(path.path(), dims())?;
        for point in 0..commit_points {
            // Each commit point advances two chunks, so the file holds chunks
            // as well as markers and segments.
            for chunk in 1..=2 {
                let row = (point * 2 + chunk) * ROWS_PER_CHUNK;
                writer.write_matrix_cell(key(row, 0), &Sample { value: 1 })?;
                writer.commit_matrix_cell::<Sample>(key(row, 0))?;
            }
            writer.flush()?;
        }
    }

    let before = VarveFile::records_framed();
    let reader = spec.open_reader(path.path())?;
    let framed = VarveFile::records_framed() - before;
    assert_eq!(
        framed, commit_points,
        "the chain must frame one record per commit point, not one per chunk",
    );
    assert!(
        reader
            .index_entries()
            .iter()
            .filter(|entry| entry.block_id == varve::MATRIX_CHUNK_BLOCK_ID)
            .count()
            > commit_points as usize,
        "the file must actually hold more chunks than commit points",
    );
    Ok(())
}

// ---------------------------------------------------------------------------

struct TempPath {
    path: PathBuf,
    _dir: tempfile::TempDir,
}

impl TempPath {
    fn path(&self) -> &Path {
        &self.path
    }
}

fn temp_path(name: &str) -> TempPath {
    let dir = tempfile::tempdir().expect("create per-test temp directory");
    let path = dir
        .path()
        .join(format!("varve_chunk_{name}_{}.vrv", std::process::id()));
    TempPath { path, _dir: dir }
}
