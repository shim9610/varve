//! Growing matrix dimensions (internal design note: the matrix-chunk spec).
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
//! arrived stays absent across a write rather than blocking it; a late write
//! into a written chunk is refused rather than dropped; and a cell read does not
//! materialise the chunk it lives in.

// The shared fixtures here build their specs with `IntegrityPolicy::Crc32`, so
// without the `integrity` feature 35 of the 47 cases fail at create with
// `IntegrityFeatureDisabled`. Gating them individually leaves the shared
// helpers dead, so the file moves as a unit and runs in the `all-features`
// CI job.
#![cfg(feature = "integrity")]

use std::path::{Path, PathBuf};

use varve::{
    BlockDescriptor, BlockKind, Endian, Error, FormatSpec, IndexPolicy, IntegrityPolicy,
    LazyOpenSource, MatrixBlockDescriptor, MatrixCellStatus, MatrixCommitDescriptor,
    MatrixCommitKind, MatrixDimensionDescriptor, MatrixDimensions, MatrixKey, ReadLimits,
    VarveBlock, VarveFile, VarveMatrixBlock,
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
// §6.3 Partial arrival survives writing the chunk
// ---------------------------------------------------------------------------

#[test]
fn a_block_that_never_arrived_reads_as_absent_across_a_write() -> varve::Result<()> {
    let path = temp_path("partial_arrival");
    let cell = key(ROWS_PER_CHUNK + 1, 3);
    {
        let mut writer = growing_spec().create_writer_with_dims(path.path(), dims())?;
        // `Sample` arrives for this cell. `Marker` never does — and writing the chunk asks
        // no question about that.
        writer.write_matrix_cell(cell, &Sample { value: 42 })?;
        writer.commit_matrix_cell::<Sample>(cell)?;
        // Move past the chunk, which writes out it with `Marker` still absent.
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
fn an_uncommitted_cell_in_a_written_chunk_stays_uncommitted() -> varve::Result<()> {
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
// §6.4 A late write into a written chunk edits it where it already sits
// ---------------------------------------------------------------------------

/// A write to a row of an already-written chunk lands, and does not grow the
/// file.
///
/// **This test asserted the opposite refusal until 2026-08-09, and the refusal
/// was the defect.** A writer that can edit a written row of the matrix
/// *region* could not edit a written row of a *chunk*, for no reason in the
/// format: a chunk record is an ordinary record, and rewriting one of those in
/// place is a route `RecordFile` has had since round 12. What the refusal cost
/// was not a corner case — it was every read-modify-write of a growing matrix.
///
/// The file length is the assertion that the edit went back over the original
/// record instead of appending a second one for the same chunk index. A
/// duplicate would not merely waste space: `build_chunk_directory` refuses a
/// file carrying two records for one chunk, so the append would have made the
/// file unopenable.
#[test]
fn a_write_into_a_written_chunk_edits_it_in_place() -> varve::Result<()> {
    let path = temp_path("closed_refusal");
    let first = key(ROWS_PER_CHUNK, 0);
    let late = key(ROWS_PER_CHUNK, 1);
    let mut writer = growing_spec().create_writer_with_dims(path.path(), dims())?;
    writer.write_matrix_cell(first, &Sample { value: 1 })?;
    writer.commit_matrix_cell::<Sample>(first)?;
    // Opening chunk 2 writes out chunk 1.
    writer.write_matrix_cell(key(ROWS_PER_CHUNK * 2, 0), &Sample { value: 2 })?;
    writer.commit_matrix_cell::<Sample>(key(ROWS_PER_CHUNK * 2, 0))?;
    writer.flush()?;
    let before = std::fs::metadata(path.path())?.len();

    writer.write_matrix_cell(late, &Sample { value: 3 })?;
    writer.commit_matrix_cell::<Sample>(late)?;
    writer.flush()?;
    assert_eq!(
        std::fs::metadata(path.path())?.len(),
        before,
        "the edit must rewrite chunk 1's record, not append a second one",
    );
    drop(writer);

    let reader = growing_spec().open_reader(path.path())?;
    // The new cell, and the one that was already there: a rewrite carries the
    // whole chunk, so losing the untouched cell is the way this fails.
    assert_eq!(
        reader.read_matrix_cell::<Sample>(late)?,
        Sample { value: 3 }
    );
    assert_eq!(
        reader.read_matrix_cell::<Sample>(first)?,
        Sample { value: 1 }
    );
    assert_eq!(
        reader.read_matrix_cell::<Sample>(key(ROWS_PER_CHUNK * 2, 0))?,
        Sample { value: 2 }
    );
    Ok(())
}

/// The same cell, written twice: the second value is what the file holds.
///
/// Separate from the test above because "a cell that had no value gets one" and
/// "a cell that had a value gets a different one" fail differently — the second
/// is the one that a rewrite which ORs commit bits without replacing slot bytes
/// would still pass.
#[test]
fn a_written_chunk_cell_can_be_overwritten_with_a_new_value() -> varve::Result<()> {
    let path = temp_path("chunk_cell_rewrite");
    let cell = key(ROWS_PER_CHUNK, 0);
    let mut writer = growing_spec().create_writer_with_dims(path.path(), dims())?;
    writer.write_matrix_cell(cell, &Sample { value: 11 })?;
    writer.commit_matrix_cell::<Sample>(cell)?;
    // Write chunk 1 out by moving past it.
    writer.write_matrix_cell(key(ROWS_PER_CHUNK * 2, 0), &Sample { value: 2 })?;
    writer.commit_matrix_cell::<Sample>(key(ROWS_PER_CHUNK * 2, 0))?;
    writer.flush()?;

    writer.write_matrix_cell(cell, &Sample { value: 22 })?;
    writer.commit_matrix_cell::<Sample>(cell)?;
    writer.flush()?;
    drop(writer);

    let reader = growing_spec().open_reader(path.path())?;
    assert_eq!(
        reader.read_matrix_cell::<Sample>(cell)?,
        Sample { value: 22 }
    );
    Ok(())
}

/// A chunk the writer skipped — nothing committed, so no record — accepts a
/// write later, and the file it produces still opens.
///
/// This is the case that makes the chunk directory's ordering rule a *sorted*
/// one rather than an ascending-file-order one. Chunk 3's record is written
/// first; chunk 1's is appended after it and carries the lower index. Under the
/// old rule the file that produced was refused at open with
/// `InvalidMatrixChunk` — by the writer's own next open.
#[test]
fn a_skipped_chunk_can_be_filled_in_after_a_later_one_was_written() -> varve::Result<()> {
    let path = temp_path("chunk_backfill");
    let late = key(ROWS_PER_CHUNK * 3, 0);
    let skipped = key(ROWS_PER_CHUNK, 0);
    {
        let mut writer = growing_spec().create_writer_with_dims(path.path(), dims())?;
        writer.write_matrix_cell(late, &Sample { value: 33 })?;
        writer.commit_matrix_cell::<Sample>(late)?;
        writer.flush()?;
        // Chunk 1 was never opened, so no record for it exists.
        writer.write_matrix_cell(skipped, &Sample { value: 11 })?;
        writer.commit_matrix_cell::<Sample>(skipped)?;
        writer.flush()?;
    }
    let reader = growing_spec().open_reader(path.path())?;
    assert_eq!(
        reader.read_matrix_cell::<Sample>(skipped)?,
        Sample { value: 11 }
    );
    assert_eq!(
        reader.read_matrix_cell::<Sample>(late)?,
        Sample { value: 33 }
    );
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
// The API sweep: every matrix entry point, against a chunked row
// ---------------------------------------------------------------------------
//
// Written after sweeping them by hand found that only four of thirteen had been
// routed. The tests above all read through a *fresh reader after a flush*, so
// none of them could see the largest defect: a writer could not read back a
// cell it had just written and committed.

#[test]
fn a_writer_reads_back_what_it_just_wrote_into_the_open_chunk() -> varve::Result<()> {
    // The open chunk is in memory, not on disk. Before this was routed,
    // `read_matrix_cell` answered `MatrixNotCommitted` until the next write —
    // a write-then-read inconsistency that no test which flushes first can see.
    let path = temp_path("open_chunk_readback");
    let cell = key(ROWS_PER_CHUNK + 1, 2);
    let mut writer = growing_spec().create_writer_with_dims(path.path(), dims())?;
    writer.write_matrix_cell(cell, &Sample { value: 5 })?;
    writer.commit_matrix_cell::<Sample>(cell)?;

    assert_eq!(
        writer.read_matrix_cell::<Sample>(cell)?,
        Sample { value: 5 },
        "the writer must see its own committed cell before it is written",
    );
    assert_eq!(
        writer.matrix_cell_payload::<Sample>(cell)?.len(),
        Sample::SLOT_STRIDE as usize,
    );
    assert_eq!(
        writer.matrix_cell_status::<Sample>(cell)?,
        MatrixCellStatus::Committed,
    );

    // And the same three answers after the write, from the same handle.
    writer.flush()?;
    assert_eq!(
        writer.read_matrix_cell::<Sample>(cell)?,
        Sample { value: 5 }
    );
    assert_eq!(
        writer.matrix_cell_status::<Sample>(cell)?,
        MatrixCellStatus::Committed,
    );
    Ok(())
}

#[test]
fn an_uncommitted_write_to_the_open_chunk_reads_as_absent() -> varve::Result<()> {
    let path = temp_path("open_chunk_uncommitted");
    let cell = key(ROWS_PER_CHUNK, 0);
    let mut writer = growing_spec().create_writer_with_dims(path.path(), dims())?;
    writer.write_matrix_cell(cell, &Sample { value: 5 })?;
    // Written but not committed: the commit bit is the answer, in a chunk
    // exactly as in the region.
    assert_eq!(
        writer.matrix_cell_status::<Sample>(cell)?,
        MatrixCellStatus::NotCommitted,
    );
    assert!(matches!(
        writer.read_matrix_cell::<Sample>(cell),
        Err(Error::MatrixNotCommitted),
    ));
    Ok(())
}

#[test]
fn the_payload_write_entry_point_routes_like_the_typed_one() -> varve::Result<()> {
    let path = temp_path("payload_write");
    let cell = key(ROWS_PER_CHUNK, 3);
    // `write_matrix_cell_payload` lives on `VarveFile`, not on `VarveWriter` —
    // `VarveWriter::copy_matrix_cell_bytes_from` reaches it, so an unrouted
    // version would have shown up there as a bounds error on a valid key.
    let mut writer = varve::VarveFile::create_with_dims(growing_spec(), path.path(), dims())?;
    writer.write_matrix_cell_payload::<Sample>(cell, &7u32.to_le_bytes())?;
    writer.commit_matrix_cell::<Sample>(cell)?;
    writer.flush()?;
    drop(writer);

    let reader = growing_spec().open_reader(path.path())?;
    assert_eq!(
        reader.read_matrix_cell::<Sample>(cell)?,
        Sample { value: 7 }
    );
    Ok(())
}

/// The two spellings of "clear one cell" accept and refuse the same things.
///
/// `clear_matrix_cell::<T>` and `clear_matrix_cell_by_category` are one
/// operation — one cell, one commit bit, one slot — differing only in whether
/// the block is named by Rust type or by category string. They diverged once:
/// the typed one learned to reopen a written chunk and the by-category one did
/// not, so the same cell cleared under one name and returned
/// `MatrixChunkClosed` under the other. Nothing about a caller can act on that
/// distinction, which is why it is a test and not a documented difference.
///
/// Two chunks, cleared one way each, so the test fails whichever half regresses.
#[test]
fn both_spellings_of_a_cell_clear_reach_a_written_chunk() -> varve::Result<()> {
    let path = temp_path("clear_by_category");
    let typed = key(ROWS_PER_CHUNK, 0);
    let by_category = key(ROWS_PER_CHUNK * 2, 0);
    let mut writer = growing_spec().create_writer_with_dims(path.path(), dims())?;
    for cell in [typed, by_category] {
        writer.write_matrix_cell(cell, &Sample { value: 4 })?;
        writer.commit_matrix_cell::<Sample>(cell)?;
    }
    // Write both chunks out by moving past them.
    let far = key(ROWS_PER_CHUNK * 5, 0);
    writer.write_matrix_cell(far, &Sample { value: 5 })?;
    writer.commit_matrix_cell::<Sample>(far)?;
    writer.flush()?;

    writer.clear_matrix_cell::<Sample>(typed)?;
    writer.clear_matrix_cell_by_category(Sample::CATEGORY, by_category)?;
    writer.flush()?;
    drop(writer);

    let reader = growing_spec().open_reader(path.path())?;
    for cell in [typed, by_category] {
        assert_eq!(
            reader.matrix_cell_status::<Sample>(cell)?,
            MatrixCellStatus::NotCommitted,
        );
    }
    assert_eq!(reader.read_matrix_cell::<Sample>(far)?, Sample { value: 5 });
    Ok(())
}

/// A cell clears in a written chunk exactly as it does in the open one, and the
/// clear survives to the file.
///
/// **The typed clear used to refuse a written chunk and no longer does.** Once
/// `write_matrix_cell` could edit a written chunk, that refusal had nothing
/// behind it: clearing is what a write is for a cell that should go back to
/// having no value, so accepting one and refusing the other left the two halves
/// of one capability disagreeing.
///
/// The by-category clear still refuses, and that is not the same omission: it
/// walks every row of a category, so serving it over written chunks means
/// loading and rewriting every one of them — a different operation with a
/// different cost. It is refused rather than half-done.
#[test]
fn clearing_a_cell_works_in_a_written_chunk_as_in_the_open_one() -> varve::Result<()> {
    let path = temp_path("clear");
    let written_cell = key(ROWS_PER_CHUNK, 0);
    let open_cell = key(ROWS_PER_CHUNK * 2, 1);
    let mut writer = growing_spec().create_writer_with_dims(path.path(), dims())?;
    writer.write_matrix_cell(written_cell, &Sample { value: 1 })?;
    writer.commit_matrix_cell::<Sample>(written_cell)?;
    writer.write_matrix_cell(open_cell, &Sample { value: 2 })?;
    writer.commit_matrix_cell::<Sample>(open_cell)?;

    writer.clear_matrix_cell::<Sample>(open_cell)?;
    assert_eq!(
        writer.matrix_cell_status::<Sample>(open_cell)?,
        MatrixCellStatus::NotCommitted,
    );
    writer.clear_matrix_cell::<Sample>(written_cell)?;
    assert_eq!(
        writer.matrix_cell_status::<Sample>(written_cell)?,
        MatrixCellStatus::NotCommitted,
    );
    writer.flush()?;
    drop(writer);

    // Written out, not just cleared in the buffer: the record on disk still
    // carried the old value until `dirty` learned that a clear on a reloaded
    // chunk is a change.
    let reader = growing_spec().open_reader(path.path())?;
    assert_eq!(
        reader.matrix_cell_status::<Sample>(written_cell)?,
        MatrixCellStatus::NotCommitted,
    );
    Ok(())
}

#[test]
fn the_entry_points_that_do_not_apply_say_so_by_name() -> varve::Result<()> {
    // Both of these used to answer with something misleading: the durable write
    // raised `MatrixKeyOutOfBounds`, which says nothing about why, and the
    // rebuild returned `Ok(0)` — indistinguishable from "the commit map was
    // already correct" on a file whose cells it never looked at.
    let path = temp_path("not_applicable");
    let cell = key(ROWS_PER_CHUNK, 0);
    let mut writer = growing_spec().create_writer_with_dims(path.path(), dims())?;
    assert!(matches!(
        writer.write_matrix_cell_durable(cell, &Sample { value: 1 }, |_| Ok(())),
        Err(Error::InvalidFormatSpec(message))
            if message.contains("does not apply to a chunked row"),
    ));
    assert!(matches!(
        writer.rebuild_matrix_commit_from_crc::<Sample>(),
        Err(Error::InvalidFormatSpec(message))
            if message.contains("matrix region only"),
    ));
    Ok(())
}

/// A chunk written by an earlier handle is editable by a later one, from a cold
/// open and again after another chunk has been opened in between.
///
/// Both orders on purpose. The first edit happens with **no chunk open**, so it
/// can only work by finding the record in the directory built from the file;
/// the second happens while chunk 3 is open, so it also exercises writing that
/// chunk out and loading chunk 1 back in the same call. An implementation that
/// handled only the in-memory branch would pass the second and fail the first.
#[test]
fn a_reopened_writer_edits_a_chunk_written_by_the_previous_handle() -> varve::Result<()> {
    let path = temp_path("reopen_append");
    let first = key(ROWS_PER_CHUNK, 0);
    let second = key(ROWS_PER_CHUNK, 2);
    let later = key(ROWS_PER_CHUNK * 3, 0);
    {
        let mut writer = growing_spec().create_writer_with_dims(path.path(), dims())?;
        writer.write_matrix_cell(first, &Sample { value: 5 })?;
        writer.commit_matrix_cell::<Sample>(first)?;
        writer.flush()?;
    }
    let mut writer = growing_spec().open_writer(path.path())?;
    // Cold: nothing is open, so chunk 1 must come back from its record.
    writer.write_matrix_cell(first, &Sample { value: 1 })?;
    writer.commit_matrix_cell::<Sample>(first)?;
    writer.write_matrix_cell(later, &Sample { value: 9 })?;
    writer.commit_matrix_cell::<Sample>(later)?;
    // Warm: chunk 3 is open and must be written out to make room for chunk 1.
    writer.write_matrix_cell(second, &Sample { value: 7 })?;
    writer.commit_matrix_cell::<Sample>(second)?;
    writer.flush()?;
    drop(writer);

    let reader = growing_spec().open_reader(path.path())?;
    assert_eq!(
        reader.read_matrix_cell::<Sample>(first)?,
        Sample { value: 1 }
    );
    assert_eq!(
        reader.read_matrix_cell::<Sample>(second)?,
        Sample { value: 7 }
    );
    assert_eq!(
        reader.read_matrix_cell::<Sample>(later)?,
        Sample { value: 9 }
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// §6.5 Crash sweep
// ---------------------------------------------------------------------------

#[test]
fn a_truncated_file_loses_no_committed_cell_and_shows_no_half_chunk() -> varve::Result<()> {
    // Cut the file at every 64th offset. At each cut: a read-only open must
    // never show a cell it cannot fully back, and a writer reopen — which
    // truncates the uncommitted tail — must then read back exactly the chunks
    // that survived, and must still refuse to reopen one of them.
    // Transaction markers, because without them there is no commit point to cut
    // back to and a writer reopen answers `CorruptTail` instead of truncating —
    // 144 of 150 cuts, measuring the commit policy rather than the chunk code.
    // The guard at the end of this test caught two versions of that mistake:
    // sweeping from byte 64 (3 of 70 cuts opened, most landing in the matrix
    // region) and sweeping under `CommitPolicy::None`.
    let spec = growing_spec()
        .with_commit_policy(varve::CommitPolicy::TransactionMarker(
            varve::TransactionMarkerMode::OnFlush,
        ))
        .with_recovery_policy(varve::RecoveryPolicy::TruncateTail);
    let path = temp_path("crash_sweep_source");
    let chunks = 6u64;
    {
        let mut writer = spec.create_writer_with_dims(path.path(), dims())?;
        for chunk in 1..=chunks {
            let cell = key(chunk * ROWS_PER_CHUNK, 0);
            writer.write_matrix_cell(
                cell,
                &Sample {
                    value: chunk as u32,
                },
            )?;
            writer.commit_matrix_cell::<Sample>(cell)?;
            // A commit point per chunk, so a cut has something to fall back to
            // rather than destroying the only one.
            writer.flush()?;
        }
    }
    let source = std::fs::read(path.path())?;

    // Sweep the append log, not the whole file. A cut inside the matrix region
    // truncates the header's own structures, so the file does not open at all
    // and the cut proves nothing — the first version of this swept from byte 64
    // and opened 3 files out of ~70, which the guard at the end caught.
    let append_start = {
        let reader = spec.open_readonly(path.path())?;
        let first = reader
            .index_entries()
            .first()
            .expect("the file holds records")
            .record_offset;
        usize::try_from(first).expect("offset")
    };

    let mut opened = 0usize;
    for cut in (append_start..source.len()).step_by(16) {
        let cut_path = temp_path("crash_sweep_cut");
        std::fs::write(cut_path.path(), &source[..cut])?;

        // Read-only: whatever it shows must be internally consistent. A cell it
        // reports as committed must decode. This half opens only when the cut
        // lands on a record boundary — a read-only handle never truncates — so
        // it is not what the guard below counts.
        if let Ok(reader) = spec.open_readonly(cut_path.path()) {
            for chunk in 1..=chunks {
                let cell = key(chunk * ROWS_PER_CHUNK, 0);
                if reader.matrix_cell_status::<Sample>(cell)? == MatrixCellStatus::Committed {
                    assert_eq!(
                        reader.read_matrix_cell::<Sample>(cell)?,
                        Sample {
                            value: chunk as u32
                        },
                        "cut {cut}: chunk {chunk} reported committed but did not read back",
                    );
                }
            }
            drop(reader);
        }

        // Writer reopen truncates the uncommitted tail. What it then reports
        // must survive a further append and another reopen.
        let Ok(mut writer) = spec.open_writer(cut_path.path()) else {
            continue;
        };
        opened += 1;
        let mut survived = Vec::new();
        for chunk in 1..=chunks {
            let cell = key(chunk * ROWS_PER_CHUNK, 0);
            if writer.matrix_cell_status::<Sample>(cell)? == MatrixCellStatus::Committed {
                survived.push(chunk);
            }
        }
        // A surviving chunk is written, and a written chunk is editable: the
        // record is loaded back and rewritten where it sits, never duplicated.
        // The duplicate is what the sweep is watching for here — it would make
        // the file unopenable at the *next* open, several statements below,
        // rather than at the write.
        if let Some(newest) = survived.last() {
            let touched = key(newest * ROWS_PER_CHUNK, 1);
            writer.write_matrix_cell(touched, &Sample { value: 0xABC })?;
            writer.commit_matrix_cell::<Sample>(touched)?;
        }
        let fresh = key((chunks + 4) * ROWS_PER_CHUNK, 0);
        writer.write_matrix_cell(fresh, &Sample { value: 99 })?;
        writer.commit_matrix_cell::<Sample>(fresh)?;
        writer.flush()?;
        drop(writer);

        let reader = spec.open_readonly(cut_path.path())?;
        for chunk in &survived {
            assert_eq!(
                reader.read_matrix_cell::<Sample>(key(chunk * ROWS_PER_CHUNK, 0))?,
                Sample {
                    value: *chunk as u32
                },
                "cut {cut}: chunk {chunk} survived the reopen but not the append",
            );
        }
        assert_eq!(
            reader.read_matrix_cell::<Sample>(fresh)?,
            Sample { value: 99 }
        );
    }
    let cuts = (source.len() - append_start).div_ceil(16);
    assert!(
        opened * 2 > cuts,
        "the writer reopen must succeed on most cuts, not skip them: \
         {opened} of {cuts}",
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// Nothing committed is lost, and nothing is lost quietly
// ---------------------------------------------------------------------------

#[test]
fn a_zero_valued_cell_can_be_committed() -> varve::Result<()> {
    // `commit_matrix_cell` decided "was this written" by testing whether the
    // slot read as all zeros, so `Sample { value: 0 }` — a legitimate value —
    // could never be committed in a chunk while committing fine in the region.
    // The region keeps a write bitmap for exactly this reason; a chunk does now
    // too.
    let path = temp_path("zero_value");
    let chunked = key(ROWS_PER_CHUNK, 0);
    let region = key(0, 0);
    {
        let mut writer = growing_spec().create_writer_with_dims(path.path(), dims())?;
        writer.write_matrix_cell(region, &Sample { value: 0 })?;
        writer.commit_matrix_cell::<Sample>(region)?;
        writer.write_matrix_cell(chunked, &Sample { value: 0 })?;
        writer.commit_matrix_cell::<Sample>(chunked)?;
        writer.flush()?;
    }
    let reader = growing_spec().open_reader(path.path())?;
    assert_eq!(
        reader.read_matrix_cell::<Sample>(region)?,
        Sample { value: 0 }
    );
    assert_eq!(
        reader.read_matrix_cell::<Sample>(chunked)?,
        Sample { value: 0 },
    );
    Ok(())
}

#[test]
fn a_cell_never_written_still_refuses_to_commit() -> varve::Result<()> {
    // The other half of the same rule: replacing the all-zero test with a write
    // bitmap must not make commit accept anything.
    let path = temp_path("uncommitted_refusal");
    let mut writer = growing_spec().create_writer_with_dims(path.path(), dims())?;
    assert!(matches!(
        writer.commit_matrix_cell::<Sample>(key(ROWS_PER_CHUNK, 2)),
        Err(Error::MatrixCellNotWritten),
    ));
    Ok(())
}

#[test]
fn write_then_flush_then_commit_keeps_the_write() -> varve::Result<()> {
    // A chunk with no committed cell used to be *dropped* at flush, so this
    // sequence lost the write on a chunked row while working on a region row —
    // and the row could become permanently unwritable once a later chunk written
    // past it. It stays open now: there is nothing to publish, so a commit
    // point holding it costs nothing.
    let path = temp_path("write_flush_commit");
    let cell = key(ROWS_PER_CHUNK, 1);
    {
        let mut writer = growing_spec().create_writer_with_dims(path.path(), dims())?;
        writer.write_matrix_cell(cell, &Sample { value: 33 })?;
        writer.flush()?;
        writer.commit_matrix_cell::<Sample>(cell)?;
        writer.flush()?;
    }
    let reader = growing_spec().open_reader(path.path())?;
    assert_eq!(
        reader.read_matrix_cell::<Sample>(cell)?,
        Sample { value: 33 }
    );
    Ok(())
}

#[test]
fn dropping_a_writer_writes_the_open_chunk() -> varve::Result<()> {
    // Every other byte a writer accepts is on disk before the call returns; a
    // chunked cell was the one exception, and dropping the writer lost every
    // committed cell in the open chunk. Best effort — `drop` cannot report a
    // failure — but the ordinary case must not lose data.
    let path = temp_path("drop_writes");
    let cell = key(ROWS_PER_CHUNK, 0);
    {
        let mut writer = growing_spec().create_writer_with_dims(path.path(), dims())?;
        writer.write_matrix_cell(cell, &Sample { value: 21 })?;
        writer.commit_matrix_cell::<Sample>(cell)?;
        // No flush, no commit, no sync.
    }
    let reader = growing_spec().open_reader(path.path())?;
    assert_eq!(
        reader.read_matrix_cell::<Sample>(cell)?,
        Sample { value: 21 }
    );
    Ok(())
}

#[test]
fn sync_makes_a_committed_chunked_cell_durable() -> varve::Result<()> {
    // `sync()` returned Ok having made nothing durable for a chunked row, while
    // the same call did for a region row. Read from a *second* handle so this
    // is about the file, not about the writer's memory.
    let path = temp_path("sync_durable");
    let cell = key(ROWS_PER_CHUNK, 0);
    let mut writer = growing_spec().create_writer_with_dims(path.path(), dims())?;
    writer.write_matrix_cell(cell, &Sample { value: 12 })?;
    writer.commit_matrix_cell::<Sample>(cell)?;
    writer.sync()?;

    let reader = growing_spec().open_readonly(path.path())?;
    assert_eq!(
        reader.read_matrix_cell::<Sample>(cell)?,
        Sample { value: 12 }
    );
    Ok(())
}

#[test]
fn a_chunk_that_could_never_be_written_is_refused_at_create() -> varve::Result<()> {
    // A chunk is buffered against `MatrixSlotRegionLen` and written against
    // `RecordPayloadLen`, and nothing reconciled them: a spec passed
    // `validate`, accepted writes, and then died at the first write with the
    // data already in RAM and no way to get it out. Refused at create now,
    // before a byte is accepted.
    //
    // Closing this also closed the only public route to a failed write, so the
    // property that a failed write keeps its chunk moved to a unit test in
    // `file.rs` (`a_failed_chunk_write_keeps_the_chunk`), which says so.
    let path = temp_path("ceiling_mismatch");
    let tight =
        growing_spec().with_read_limits(growing_spec().read_limits.with_max_record_payload_len(64));
    assert!(matches!(
        tight.create_writer_with_dims(path.path(), dims()),
        Err(Error::LimitExceeded {
            resource: "record payload length",
            ..
        }),
    ));
    Ok(())
}

// ---------------------------------------------------------------------------
// Whole-matrix operations must account for chunked rows
// ---------------------------------------------------------------------------

#[test]
fn clearing_a_category_counts_the_open_chunk_and_refuses_a_written_one() -> varve::Result<()> {
    // `clear_matrix_category` cleared the matrix region only, so a caller
    // asking for a clean category got one silently: chunked rows stayed
    // committed, stayed readable, and were not in the count.
    let path = temp_path("clear_category_open");
    let mut writer = growing_spec().create_writer_with_dims(path.path(), dims())?;
    let region = key(0, 0);
    let chunked = key(ROWS_PER_CHUNK, 0);
    writer.write_matrix_cell(region, &Sample { value: 1 })?;
    writer.commit_matrix_cell::<Sample>(region)?;
    writer.write_matrix_cell(chunked, &Sample { value: 2 })?;
    writer.commit_matrix_cell::<Sample>(chunked)?;

    // One region cell and one open-chunk cell.
    assert_eq!(writer.clear_matrix_category(Sample::CATEGORY)?, 2);
    assert_eq!(
        writer.matrix_cell_status::<Sample>(chunked)?,
        MatrixCellStatus::NotCommitted,
    );
    assert_eq!(
        writer.matrix_cell_status::<Sample>(region)?,
        MatrixCellStatus::NotCommitted,
    );

    // A written chunk is reached too, and counted. This asserted a refusal
    // until 2026-08-09: "a written chunk is a written record, and records are
    // not rewritten" stopped being true when a chunk record became rewritable
    // in place, and the cost of reaching them — a reload and a rewrite per
    // chunk — is what clearing a category is, not a reason to refuse it.
    writer.write_matrix_cell(chunked, &Sample { value: 3 })?;
    writer.commit_matrix_cell::<Sample>(chunked)?;
    writer.flush()?;
    assert_eq!(writer.clear_matrix_category(Sample::CATEGORY)?, 1);
    assert_eq!(
        writer.matrix_cell_status::<Sample>(chunked)?,
        MatrixCellStatus::NotCommitted,
    );
    // Idempotent, which is what stands in for atomicity: a call that failed
    // part-way is answered by calling again, and an already-clear category
    // counts nothing.
    assert_eq!(writer.clear_matrix_category(Sample::CATEGORY)?, 0);
    Ok(())
}

/// The bulk clear reaches every written chunk, not just the newest, and the
/// clear reaches the file rather than living in the buffer.
///
/// Three chunks written before the clear, so a walk that stops at the first or
/// only handles the open one fails. The reopen after `drop` is the half that
/// catches the `dirty` landmine: `clear_open_chunk_block` recomputes `dirty`
/// from the commit bits it just zeroed, so a chunk loaded back from a record
/// went *non*-dirty on being cleared and was never written out — leaving the
/// record on disk serving the very cells the call reported clearing.
#[test]
fn the_bulk_clear_reaches_every_written_chunk_and_reaches_the_file() -> varve::Result<()> {
    let path = temp_path("clear_category_written");
    let cells = [
        key(ROWS_PER_CHUNK, 0),
        key(ROWS_PER_CHUNK * 2, 1),
        key(ROWS_PER_CHUNK * 3, 2),
    ];
    {
        let mut writer = growing_spec().create_writer_with_dims(path.path(), dims())?;
        for (value, cell) in cells.iter().enumerate() {
            writer.write_matrix_cell(
                *cell,
                &Sample {
                    value: value as u32,
                },
            )?;
            writer.commit_matrix_cell::<Sample>(*cell)?;
        }
        // Move past the last one so all three have records.
        let far = key(ROWS_PER_CHUNK * 9, 0);
        writer.write_matrix_cell(far, &Sample { value: 9 })?;
        writer.commit_matrix_cell::<Sample>(far)?;
        writer.flush()?;

        // Three chunked cells plus the one that opened chunk 9.
        assert_eq!(writer.clear_matrix_category(Sample::CATEGORY)?, 4);
        writer.flush()?;
    }
    let reader = growing_spec().open_reader(path.path())?;
    for cell in cells {
        assert_eq!(
            reader.matrix_cell_status::<Sample>(cell)?,
            MatrixCellStatus::NotCommitted,
            "a cleared cell came back after a reopen",
        );
    }
    Ok(())
}

#[test]
fn the_resume_signal_is_not_clean_over_an_open_chunk() -> varve::Result<()> {
    // `Clean` told a caller the acquisition had finished. An open chunk is live
    // state this handle holds and the next one will not, so it never is.
    let path = temp_path("resume_signal");
    let mut writer = growing_spec().create_writer_with_dims(path.path(), dims())?;
    assert_eq!(
        writer.matrix_resume_signal(Sample::CATEGORY)?,
        varve::MatrixResumeSignal::Clean,
    );
    writer.write_matrix_cell(key(ROWS_PER_CHUNK, 0), &Sample { value: 1 })?;
    writer.commit_matrix_cell::<Sample>(key(ROWS_PER_CHUNK, 0))?;
    assert_ne!(
        writer.matrix_resume_signal(Sample::CATEGORY)?,
        varve::MatrixResumeSignal::Clean,
        "an open chunk is unfinished work",
    );
    writer.flush()?;
    assert_eq!(
        writer.matrix_resume_signal(Sample::CATEGORY)?,
        varve::MatrixResumeSignal::Clean,
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// What a chunk costs, pinned so the spec's cost table cannot drift back
// ---------------------------------------------------------------------------

#[test]
fn compression_cannot_be_declared_on_a_matrix_block() {
    // The chunk spec §3.2 named per-block compression as the knob for a
    // chunk's lost sparseness. It is not one: `validate` refuses the
    // declaration outright, and a chunk record is written by
    // `write_record_with_prev_key` rather than through
    // `prepare_user_record_payload`, which is the only place compression is
    // applied. Pinned so the claim cannot be made again.
    static COMPRESSED: &[varve::BlockCompressionDescriptor] =
        &[varve::BlockCompressionDescriptor {
            block_id: Sample::ID,
            compression: varve::VariableCompression::zstd(
                varve::CompressionHeaderMode::RecordExplicit,
            ),
        }];
    assert!(matches!(
        growing_spec().with_block_compression(COMPRESSED).validate(),
        Err(Error::InvalidFormatSpec(
            "block compression requires a variable block"
        )),
    ));
}

#[test]
fn an_unfilled_chunk_costs_its_whole_size() -> varve::Result<()> {
    // The spec's cost table said an unfilled grid "compresses to almost
    // nothing". Measured 2026-08-04 on ext4: a chunk 5% filled and a chunk 100%
    // filled produce byte-identical file sizes, apparent *and* allocated. A
    // chunk is written as contiguous bytes and nothing punches holes in them,
    // so the waste is total, not partial — and the only knob that bounds it is
    // `rows_per_chunk`.
    let mut sizes = Vec::new();
    for fill in [ROWS_PER_CHUNK, ROWS_PER_CHUNK / 2, 1] {
        let path = temp_path(&format!("fill_{fill}"));
        {
            let mut writer = growing_spec().create_writer_with_dims(path.path(), dims())?;
            for row in 0..fill {
                let cell = key(ROWS_PER_CHUNK + row, 0);
                writer.write_matrix_cell(cell, &Sample { value: 1 })?;
                writer.commit_matrix_cell::<Sample>(cell)?;
            }
            writer.flush()?;
        }
        sizes.push(std::fs::metadata(path.path())?.len());
    }
    assert_eq!(
        sizes[0], sizes[1],
        "a half-filled chunk must cost what a full one does — it does, and that \
         is the cost this feature has",
    );
    assert_eq!(sizes[1], sizes[2]);
    Ok(())
}

#[test]
fn a_smaller_chunk_height_bounds_the_waste() -> varve::Result<()> {
    // The one real knob, since compression is not available: halving
    // `rows_per_chunk` halves what a single touched row drags along.
    fn one_row(rows_per_chunk: u64) -> varve::Result<u64> {
        let spec = base_spec().with_growing_matrix_dimension("scan", rows_per_chunk);
        let dims = MatrixDimensions::from_pairs([("scan", rows_per_chunk), ("ch", CHANNELS)]);
        let path = temp_path(&format!("height_{rows_per_chunk}"));
        {
            let mut writer = spec.create_writer_with_dims(path.path(), dims)?;
            let cell = key(rows_per_chunk, 0);
            writer.write_matrix_cell(cell, &Sample { value: 1 })?;
            writer.commit_matrix_cell::<Sample>(cell)?;
            writer.flush()?;
        }
        Ok(std::fs::metadata(path.path())?.len())
    }
    let tall = one_row(64)?;
    let short = one_row(8)?;
    assert!(
        short < tall,
        "a shorter chunk must drag less along: {short} vs {tall}",
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// The cost of a chunked read must not scale with the file
// ---------------------------------------------------------------------------

#[cfg(feature = "scalable-fault-injection")]
#[test]
fn a_chunked_read_does_not_pay_for_records_it_does_not_touch() -> varve::Result<()> {
    use std::time::Instant;
    use varve::VarveFile;

    // The defect this pins: `find_chunk_record` filtered the whole resident
    // index and collected it into a fresh `Vec` on **every** chunked cell read
    // — `O(total records)` plus a heap allocation per read — behind a doc
    // comment claiming `O(log chunks)` and "no state built at open". It is the
    // same shape `CheckpointCadence`, `BlockTails` and `SegmentCursor` each
    // exist to remove, in a file where all three are already written down.
    //
    // Measured as a ratio *and* as an absolute byte count, because a ratio
    // alone is satisfied by any large constant (internal design note: the
    // index-residency spec, section 4.6.1).
    fn build(path: &Path, chunks: u64) -> varve::Result<()> {
        let mut writer = growing_spec().create_writer_with_dims(path, dims())?;
        for chunk in 1..=chunks {
            let cell = key(chunk * ROWS_PER_CHUNK, 0);
            writer.write_matrix_cell(
                cell,
                &Sample {
                    value: chunk as u32,
                },
            )?;
            writer.commit_matrix_cell::<Sample>(cell)?;
        }
        writer.flush()?;
        Ok(())
    }

    fn one_read_cost(path: &Path, reads: u32) -> varve::Result<(u64, f64)> {
        let reader = growing_spec().open_reader(path)?;
        let cell = key(ROWS_PER_CHUNK, 0);
        // Warm the directory so this measures the steady state, which is what
        // a per-read walk would dominate.
        reader.read_matrix_cell::<Sample>(cell)?;
        let before = VarveFile::chunk_bytes_read();
        let start = Instant::now();
        for _ in 0..reads {
            reader.read_matrix_cell::<Sample>(cell)?;
        }
        let elapsed = start.elapsed().as_secs_f64();
        Ok((VarveFile::chunk_bytes_read() - before, elapsed))
    }

    let small = temp_path("cost_small");
    let large = temp_path("cost_large");
    build(small.path(), 8)?;
    build(large.path(), 256)?;

    let reads = 200u32;
    let (small_bytes, _) = one_read_cost(small.path(), reads)?;
    let (large_bytes, _) = one_read_cost(large.path(), reads)?;

    // Absolute first: a steady-state read touches the prefix, the descriptors,
    // one commit byte and one slot. Nothing per chunk, nothing per record.
    let per_read = large_bytes / u64::from(reads);
    assert!(
        per_read < 256,
        "a chunked read must not scale with the file: {per_read} bytes per read \
         over {reads} reads on a 256-chunk file",
    );
    // And the 32x larger file must not cost more per read than the small one.
    assert_eq!(
        small_bytes, large_bytes,
        "32x the chunks must cost the same per read",
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// Chunk compression: the knob for the sparse cost
// ---------------------------------------------------------------------------

fn compressed_spec() -> FormatSpec {
    growing_spec().with_chunk_compression(varve::VariableCompression::zstd(
        varve::CompressionHeaderMode::RecordExplicit,
    ))
}

#[test]
fn chunk_compression_is_inert_when_not_declared() {
    assert!(growing_spec().chunk_compression.is_none());
    // A spec that declares no growing dimension still hashes to what it hashed
    // to before either option existed.
    assert_eq!(base_spec().computed_schema_hash(), BASE_SCHEMA_HASH);
    assert_ne!(
        compressed_spec().computed_schema_hash(),
        growing_spec().computed_schema_hash(),
        "a compressed chunk is not the file the plain spec describes",
    );
}

#[test]
fn chunk_compression_requires_a_growing_dimension() {
    assert!(matches!(
        base_spec()
            .with_chunk_compression(varve::VariableCompression::zstd(
                varve::CompressionHeaderMode::RecordExplicit,
            ))
            .validate(),
        Err(Error::InvalidFormatSpec(
            "chunk compression requires a growing matrix dimension"
        )),
    ));
}

#[test]
fn compressed_chunks_read_back_every_cell() -> varve::Result<()> {
    let path = temp_path("compressed_roundtrip");
    let rows = ROWS_PER_CHUNK * 6;
    {
        let mut writer = compressed_spec().create_writer_with_dims(path.path(), dims())?;
        for row in 0..rows {
            for ch in 0..CHANNELS {
                let cell = key(row, ch);
                writer.write_matrix_cell(
                    cell,
                    &Sample {
                        value: (row * CHANNELS + ch) as u32 + 1,
                    },
                )?;
                writer.commit_matrix_cell::<Sample>(cell)?;
            }
        }
        writer.flush()?;
    }
    let reader = compressed_spec().open_reader(path.path())?;
    for row in 0..rows {
        for ch in 0..CHANNELS {
            assert_eq!(
                reader.read_matrix_cell::<Sample>(key(row, ch))?,
                Sample {
                    value: (row * CHANNELS + ch) as u32 + 1
                },
                "row {row} ch {ch}",
            );
        }
    }
    Ok(())
}

#[test]
fn a_partly_filled_chunk_costs_less_when_compressed() -> varve::Result<()> {
    // The point of the knob. Uncompressed, a chunk 1/4096 filled costs exactly
    // what a full one costs — `an_unfilled_chunk_costs_its_whole_size` pins
    // that. Compressed, the untouched cells are zeros and collapse.
    //
    // A tall chunk, so the slot region dominates the file rather than the
    // header: at ROWS_PER_CHUNK the same comparison is 3,644 vs 2,836 bytes and
    // says more about the matrix region than about this mechanism.
    const TALL: u64 = 4_096;
    fn write(spec: FormatSpec, path: &Path) -> varve::Result<u64> {
        let dims = MatrixDimensions::from_pairs([("scan", TALL), ("ch", CHANNELS)]);
        {
            let mut writer = spec.create_writer_with_dims(path, dims)?;
            for chunk in 1..=8u64 {
                for ch in 0..CHANNELS {
                    let cell = key(chunk * TALL, ch);
                    writer.write_matrix_cell(cell, &Sample { value: 7 })?;
                    writer.commit_matrix_cell::<Sample>(cell)?;
                }
            }
            writer.flush()?;
        }
        Ok(std::fs::metadata(path)?.len())
    }

    let plain_path = temp_path("sparse_plain");
    let comp_path = temp_path("sparse_compressed");
    let plain = write(
        growing_spec().with_growing_matrix_dimension("scan", TALL),
        plain_path.path(),
    )?;
    let compressed = write(
        compressed_spec().with_growing_matrix_dimension("scan", TALL),
        comp_path.path(),
    )?;
    println!("PROBE: one filled row of {TALL} — plain {plain} B, compressed {compressed} B");
    // The floor is the matrix region — chunk 0, which lives in the declared
    // extent and is not a record, so this knob does not reach it. With eight
    // chunks beyond it the plain file is nine regions' worth and the compressed
    // one is barely more than the region itself.
    assert!(
        compressed * 4 < plain,
        "eight chunks with one filled row of {TALL} must collapse: {compressed} \
         vs {plain}",
    );
    Ok(())
}

#[test]
fn a_corrupted_compressed_sub_block_is_refused() -> varve::Result<()> {
    // The per-cell checksum is computed over the *uncompressed* cell bytes and
    // stored plain, so it still guards a compressed chunk: a damaged sub-block
    // either fails to decode or decodes to bytes whose checksum does not match.
    let path = temp_path("compressed_corrupt");
    let cell = key(ROWS_PER_CHUNK, 0);
    {
        let mut writer = compressed_spec().create_writer_with_dims(path.path(), dims())?;
        for ch in 0..CHANNELS {
            let at = key(ROWS_PER_CHUNK, ch);
            writer.write_matrix_cell(at, &Sample { value: 0xABCD })?;
            writer.commit_matrix_cell::<Sample>(at)?;
        }
        writer.flush()?;
    }
    // Corrupt a byte inside the first compressed sub-block. zstd frames start
    // with the magic 0x28 0xB5 0x2F 0xFD, and the first one after the chunk
    // prefix belongs to `Sample` — the lower block id, so the first block
    // region in the payload.
    let bytes = std::fs::read(path.path())?;
    let (_, at) = first_chunk_payload(path.path())?;
    let frame = bytes[at..]
        .windows(4)
        .position(|window| window == [0x28, 0xB5, 0x2F, 0xFD])
        .map(|offset| at + offset)
        .expect("a zstd frame inside the chunk record");
    // Past the frame header, in its compressed body.
    patch(path.path(), frame + 8, &[bytes[frame + 8] ^ 0xFF])?;

    let reader = compressed_spec().open_reader(path.path())?;
    assert!(
        reader.read_matrix_cell::<Sample>(cell).is_err(),
        "a corrupted compressed sub-block must not decode to data",
    );
    Ok(())
}

#[cfg(feature = "scalable-fault-injection")]
#[test]
fn a_compressed_read_costs_one_sub_block_not_the_chunk() -> varve::Result<()> {
    use varve::VarveFile;

    // The trade this knob makes, stated as a number rather than as a promise:
    // a plain chunked read touches the cell, a compressed one touches its
    // sub-block. Both must stay far below the chunk.
    const WIDE_ROWS: u64 = 4_096;
    let plain = growing_spec().with_growing_matrix_dimension("scan", WIDE_ROWS);
    let compressed = plain.with_chunk_compression(varve::VariableCompression::zstd(
        varve::CompressionHeaderMode::RecordExplicit,
    ));
    let wide_dims = MatrixDimensions::from_pairs([("scan", WIDE_ROWS), ("ch", CHANNELS)]);
    let chunk_slot_bytes = WIDE_ROWS * CHANNELS * Sample::SLOT_STRIDE;

    fn cost(
        spec: FormatSpec,
        dims: MatrixDimensions,
        path: &Path,
        rows: u64,
    ) -> varve::Result<u64> {
        let cell = MatrixKey::new(rows + 7, 2);
        {
            let mut writer = spec.create_writer_with_dims(path, dims)?;
            writer.write_matrix_cell(cell, &Sample { value: 77 })?;
            writer.commit_matrix_cell::<Sample>(cell)?;
            writer.write_matrix_cell(MatrixKey::new(rows * 2, 0), &Sample { value: 1 })?;
            writer.commit_matrix_cell::<Sample>(MatrixKey::new(rows * 2, 0))?;
            writer.flush()?;
        }
        let reader = spec.open_reader(path)?;
        reader.read_matrix_cell::<Sample>(cell)?;
        let before = VarveFile::chunk_bytes_read();
        assert_eq!(
            reader.read_matrix_cell::<Sample>(cell)?,
            Sample { value: 77 }
        );
        Ok(VarveFile::chunk_bytes_read() - before)
    }

    let plain_path = temp_path("cost_plain");
    let comp_path = temp_path("cost_compressed");
    let plain_cost = cost(plain, wide_dims.clone(), plain_path.path(), WIDE_ROWS)?;
    let comp_cost = cost(compressed, wide_dims, comp_path.path(), WIDE_ROWS)?;

    println!(
        "PROBE: plain {plain_cost} B, compressed {comp_cost} B, chunk slots {chunk_slot_bytes} B"
    );
    assert!(
        plain_cost < 4096,
        "a plain chunked read must stay small: {plain_cost}",
    );
    assert!(
        comp_cost < chunk_slot_bytes / 8,
        "a compressed read must touch its sub-block, not the chunk: {comp_cost} of \
         {chunk_slot_bytes}",
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// Hostile input: a chunk record's own extent is the wall
// ---------------------------------------------------------------------------
//
// Every offset the chunk decode computes comes from bytes in the file. Both
// defects below were found by an adversarial review of this feature and both
// reproduced under `IntegrityPolicy::Crc32` — a positional cell read never
// verifies a record footer under the default `IntegrityVerification::OnDemand`,
// so the checksum is not the guard here. The record's `payload_len`, which the
// framing established and already charged, is.

/// Overwrites `count` bytes at the byte offset `at` inside the file.
fn patch(path: &Path, at: usize, bytes: &[u8]) -> varve::Result<()> {
    use std::io::{Seek, SeekFrom, Write};
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)?;
    file.seek(SeekFrom::Start(at as u64))?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}

/// Byte offset of the first `VMCK` chunk payload in the file.
fn first_chunk_payload(path: &Path) -> varve::Result<(Vec<u8>, usize)> {
    let bytes = std::fs::read(path)?;
    let at = bytes
        .windows(4)
        .position(|window| window == b"VMCK")
        .expect("the file holds a chunk record");
    Ok((bytes, at))
}

fn two_chunk_file(path: &Path) -> varve::Result<()> {
    let mut writer = growing_spec().create_writer_with_dims(path, dims())?;
    writer.write_matrix_cell(key(ROWS_PER_CHUNK, 0), &Sample { value: 5 })?;
    writer.commit_matrix_cell::<Sample>(key(ROWS_PER_CHUNK, 0))?;
    writer.write_matrix_cell(key(ROWS_PER_CHUNK * 2, 0), &Sample { value: 6 })?;
    writer.commit_matrix_cell::<Sample>(key(ROWS_PER_CHUNK * 2, 0))?;
    writer.flush()?;
    Ok(())
}

#[test]
fn a_flipped_bit_in_a_written_chunk_is_refused_not_returned() -> varve::Result<()> {
    // The review reproduced this: flipping bytes in a written chunk's slot
    // region made `read_matrix_cell` return the corrupted value with no error,
    // while `verify_all()` on the same handle reported ChecksumMismatch — the
    // record CRC existed and covered those bytes and was never consulted. The
    // identical flip one row earlier, in the matrix region, was refused, so the
    // same API call verified for row 3 and returned corrupt data for row 4.
    //
    // The record footer's CRC is verified on a *record* read; a positional cell
    // read is not one. A chunk carries per-cell checksums now, as the region
    // already did.
    let path = temp_path("flipped_bit");
    let cell = key(ROWS_PER_CHUNK, 0);
    {
        let mut writer = growing_spec().create_writer_with_dims(path.path(), dims())?;
        writer.write_matrix_cell(cell, &Sample { value: 0x1234_5678 })?;
        writer.commit_matrix_cell::<Sample>(cell)?;
        writer.flush()?;
    }
    // Find the value in the written chunk and corrupt it in place.
    let bytes = std::fs::read(path.path())?;
    let (_, at) = first_chunk_payload(path.path())?;
    let needle = 0x1234_5678u32.to_le_bytes();
    let value_at = bytes[at..]
        .windows(4)
        .position(|window| window == needle)
        .map(|offset| at + offset)
        .expect("the value is in the chunk record");
    patch(path.path(), value_at, &0x9999_9999u32.to_le_bytes())?;

    let reader = growing_spec().open_reader(path.path())?;
    assert!(
        matches!(
            reader.read_matrix_cell::<Sample>(cell),
            Err(Error::MatrixChecksumMismatch { .. }),
        ),
        "a corrupted chunk cell must be refused, not returned",
    );
    Ok(())
}

#[test]
fn a_format_without_integrity_stores_no_chunk_checksums() -> varve::Result<()> {
    // Inertness for the other direction: a format that declares no integrity
    // policy must not start paying four bytes per cell for a table nothing
    // reads.
    let with_crc = temp_path("crc_on");
    let without = temp_path("crc_off");
    let plain = growing_spec().with_integrity_policy(IntegrityPolicy::None);
    for (spec, path) in [(growing_spec(), &with_crc), (plain, &without)] {
        let mut writer = spec.create_writer_with_dims(path.path(), dims())?;
        writer.write_matrix_cell(key(ROWS_PER_CHUNK, 0), &Sample { value: 1 })?;
        writer.commit_matrix_cell::<Sample>(key(ROWS_PER_CHUNK, 0))?;
        writer.flush()?;
    }
    let crc_len = std::fs::metadata(with_crc.path())?.len();
    let plain_len = std::fs::metadata(without.path())?.len();
    assert!(
        crc_len > plain_len,
        "the checksum table must cost something when on: {crc_len} vs {plain_len}",
    );
    // Both must still read back.
    let reader = growing_spec()
        .with_integrity_policy(IntegrityPolicy::None)
        .open_reader(without.path())?;
    assert_eq!(
        reader.read_matrix_cell::<Sample>(key(ROWS_PER_CHUNK, 0))?,
        Sample { value: 1 },
    );
    Ok(())
}

#[test]
fn a_crafted_block_count_cannot_make_the_reader_allocate() -> varve::Result<()> {
    // `block_count` is a `u32` at payload+32. Unbounded, `0xFFFF_FFFF` asked for
    // a **137,438,953,440-byte** reservation out of a 1 KB file — `try_reserve`
    // refused it on this host, and a host with overcommit would have accepted
    // it and then faulted the pages in. Nothing charged a `ReadLimits` ceiling,
    // including under `STANDARD`, which is the setting for untrusted input.
    let path = temp_path("hostile_block_count");
    two_chunk_file(path.path())?;
    let (_, at) = first_chunk_payload(path.path())?;
    patch(path.path(), at + 32, &u32::MAX.to_le_bytes())?;

    let reader = growing_spec().open_reader(path.path())?;
    assert!(matches!(
        reader.read_matrix_cell::<Sample>(key(ROWS_PER_CHUNK, 0)),
        Err(Error::InvalidMatrixChunk),
    ));
    Ok(())
}

#[test]
fn a_crafted_commit_length_cannot_read_outside_the_record() -> varve::Result<()> {
    // `commit_len` is the last field of block descriptor 0, at payload+40+24.
    // Growing it moves that block's slot region forward while leaving the
    // commit map — and so the commit bit — where it was. Unbounded, the read
    // returned a *neighbouring record's* bytes decoded as a cell value: the
    // cell holding 5 answered `Sample { value: 1 }`, with no error at all.
    let path = temp_path("hostile_commit_len");
    two_chunk_file(path.path())?;
    let (bytes, at) = first_chunk_payload(path.path())?;
    let commit_len_at = at + 40 + 24;
    let real = u64::from_le_bytes(
        bytes[commit_len_at..commit_len_at + 8]
            .try_into()
            .expect("slice"),
    );
    patch(path.path(), commit_len_at, &(real + 232).to_le_bytes())?;

    let reader = growing_spec().open_reader(path.path())?;
    assert!(matches!(
        reader.read_matrix_cell::<Sample>(key(ROWS_PER_CHUNK, 0)),
        Err(Error::InvalidMatrixChunk),
    ));
    Ok(())
}

#[test]
fn a_crafted_cell_count_cannot_outrun_its_commit_map() -> varve::Result<()> {
    // `cells` at payload+40+16. Grown, the ordinal bound widens while the
    // commit map does not, so a high ordinal's commit bit is read out of the
    // slot region — a value byte answering a "was this committed" question.
    let path = temp_path("hostile_cells");
    two_chunk_file(path.path())?;
    let (_, at) = first_chunk_payload(path.path())?;
    patch(path.path(), at + 40 + 16, &1_000_000u64.to_le_bytes())?;

    let reader = growing_spec().open_reader(path.path())?;
    assert!(matches!(
        reader.read_matrix_cell::<Sample>(key(ROWS_PER_CHUNK, 0)),
        Err(Error::InvalidMatrixChunk),
    ));
    Ok(())
}

#[test]
fn a_chunk_written_under_a_different_rows_per_chunk_is_refused() -> varve::Result<()> {
    // `read_chunk_prefix` decoded `rows` and `first_row` and every caller threw
    // them away, and the row width came from the record's `cells` divided by
    // *this spec's* `rows_per_chunk`. So a file written under a different chunk
    // height was not refused, it was reinterpreted — and a read returned
    // another row's bytes with no error anywhere.
    let path = temp_path("geometry_mismatch");
    {
        let mut writer = growing_spec().create_writer_with_dims(path.path(), dims())?;
        writer.write_matrix_cell(key(ROWS_PER_CHUNK, 0), &Sample { value: 5 })?;
        writer.commit_matrix_cell::<Sample>(key(ROWS_PER_CHUNK, 0))?;
        writer.flush()?;
    }
    // Same format, different chunk height. Opening is fine — the header does
    // not carry it — but reading a chunk must not reinterpret it. The key is
    // chosen to route to a *chunk* under the reader's own arithmetic: with
    // twice the chunk height, row `2 * ROWS_PER_CHUNK` is chunk 1 there.
    let other = base_spec().with_growing_matrix_dimension("scan", ROWS_PER_CHUNK * 2);
    let reader = other.open_reader(path.path())?;
    assert!(matches!(
        reader.read_matrix_cell::<Sample>(key(ROWS_PER_CHUNK * 2, 0)),
        Err(Error::InvalidMatrixChunk),
    ));
    Ok(())
}

#[test]
fn a_crafted_stride_or_cell_count_is_refused() -> varve::Result<()> {
    // `stride` and `cells` were taken from the record and never compared to the
    // block descriptor, so a widened `cells` changed the row pitch and every
    // key resolved to a different cell.
    for (name, field_offset, value) in [
        ("stride", 40 + 8, 8u64),
        ("cells", 40 + 16, ROWS_PER_CHUNK * CHANNELS * 2),
    ] {
        let path = temp_path(&format!("crafted_{name}"));
        two_chunk_file(path.path())?;
        let (_, at) = first_chunk_payload(path.path())?;
        patch(path.path(), at + field_offset, &value.to_le_bytes())?;

        let reader = growing_spec().open_reader(path.path())?;
        assert!(
            matches!(
                reader.read_matrix_cell::<Sample>(key(ROWS_PER_CHUNK, 0)),
                Err(Error::InvalidMatrixChunk),
            ),
            "a crafted {name} must be refused",
        );
    }
    Ok(())
}

#[test]
fn a_truncated_chunk_payload_is_refused() -> varve::Result<()> {
    // The extent itself, made too small to hold even the prefix.
    let path = temp_path("hostile_short_payload");
    two_chunk_file(path.path())?;
    let (bytes, at) = first_chunk_payload(path.path())?;
    // Header layout (native_layout.rs:88): block_id u32, block_version u16,
    // flags u16, sequence u64, payload_len u64 — so payload_len is at +16.
    let header_at = at - 32;
    let real = u64::from_le_bytes(
        bytes[header_at + 16..header_at + 24]
            .try_into()
            .expect("slice"),
    );
    assert!(real > 0, "the chunk record has a payload");
    patch(path.path(), header_at + 16, &8u64.to_le_bytes())?;

    // Open may refuse outright (the framing no longer adds up) or may open and
    // refuse the read. Either is acceptable; returning a value is not.
    if let Ok(reader) = growing_spec().open_reader(path.path()) {
        assert!(
            reader
                .read_matrix_cell::<Sample>(key(ROWS_PER_CHUNK, 0))
                .is_err(),
        );
    }
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

// ---------------------------------------------------------------------------
// The gates, on the chunk path
//
// The eight invariants this file's earlier cases pin were all driven in on the
// write/write side. A later review found the read side had none of them: the
// only caller of the chunk access gate was `chunk_block_slice`, which only the
// write paths use, so a quarantined category refused a region row and handed
// back the bytes for a chunked one -- from the same call. This file had no
// quarantine or fatal-gate case at all, which is exactly why that survived.
//
// A note on what these cost to write, because it is the reason they were
// missing: a quarantine cannot be asked for through the API. It is a *finding*,
// produced by the recovery pass when it reads damage off the disk, so the
// fixture has to corrupt a commit map by hand and reopen. The three helpers at
// the bottom do that.
// ---------------------------------------------------------------------------

/// The same block id and shape as `Sample`, one version later.
///
/// The descriptor table says block 810 is version 1, and the spec's schema hash
/// ties that table to the file -- so this type disagrees with the bytes on
/// disk. The region path has always refused it. The chunk path addressed the
/// block by id alone and would decode v2 semantics over v1 cells, returning a
/// confidently wrong value rather than an error.
#[derive(Clone, Debug, PartialEq, Eq, VarveBlock)]
#[varve(id = 810, version = 2, kind = "matrix")]
struct SampleV2 {
    value: u32,
}

impl VarveMatrixBlock for SampleV2 {
    const DIMENSIONS: [&'static str; 2] = ["scan", "ch"];
    const CATEGORY: &'static str = "sample";
    const SLOT_STRIDE: u64 = 4;
}

#[test]
fn a_type_that_disagrees_with_the_schema_is_refused_on_a_chunked_row() -> varve::Result<()> {
    let path = temp_path("version_mismatch");
    let mut writer = growing_spec().create_writer_with_dims(path.path(), dims())?;
    // One region row and one chunked row, both committed through the honest type.
    for row in [0, ROWS_PER_CHUNK] {
        writer.write_matrix_cell(key(row, 0), &Sample { value: 7 })?;
        writer.commit_matrix_cell::<Sample>(key(row, 0))?;
    }
    writer.flush()?;
    drop(writer);

    let reader = growing_spec().open_reader(path.path())?;
    // The region row is the control: this is the refusal that already worked.
    assert!(
        matches!(
            reader.read_matrix_cell::<SampleV2>(key(0, 0)),
            Err(Error::BlockVersionMismatch { block_id, .. }) if block_id == Sample::ID
        ),
        "the region row accepted a type that disagrees with the schema"
    );
    // The chunked row must answer the same. Before the gate was added it
    // returned `Ok(SampleV2 { .. })`.
    assert!(
        matches!(
            reader.read_matrix_cell::<SampleV2>(key(ROWS_PER_CHUNK, 0)),
            Err(Error::BlockVersionMismatch { block_id, .. }) if block_id == Sample::ID
        ),
        "a chunked row decoded a type the file's schema does not describe"
    );
    assert!(
        matches!(
            reader.matrix_cell_status::<SampleV2>(key(ROWS_PER_CHUNK, 0)),
            Err(Error::BlockVersionMismatch { .. })
        ),
        "the status query answered for a type the schema does not describe"
    );
    Ok(())
}

#[test]
fn a_quarantined_category_refuses_a_chunked_row_as_it_refuses_a_region_row() -> varve::Result<()> {
    let path = temp_path("quarantine_chunked_read");
    let mut writer = growing_spec().create_writer_with_dims(path.path(), dims())?;
    for row in [0, ROWS_PER_CHUNK] {
        writer.write_matrix_cell(key(row, 0), &Sample { value: 11 })?;
        writer.commit_matrix_cell::<Sample>(key(row, 0))?;
        writer.write_matrix_cell(key(row, 1), &Marker { flag: 1 })?;
        writer.commit_matrix_cell::<Marker>(key(row, 1))?;
    }
    writer.flush()?;
    drop(writer);

    // Damage the commit map so the recovery pass quarantines the category. This
    // is the only way in: a quarantine is a finding, not a setting.
    let map_off = commit_map_offset(path.path());
    patch_byte(path.path(), map_off, 0x40);

    let reader = growing_spec().open_readonly(path.path())?;
    let quarantined = |result: varve::Result<Sample>| matches!(result, Err(Error::MatrixCommitQuarantined(name)) if name == Sample::CATEGORY);
    assert!(
        quarantined(reader.read_matrix_cell::<Sample>(key(0, 0))),
        "the fixture did not produce a quarantine on the region row"
    );
    // The whole finding, in one line: this used to return the cell's bytes.
    assert!(
        quarantined(reader.read_matrix_cell::<Sample>(key(ROWS_PER_CHUNK, 0))),
        "a quarantined category was still readable through a chunked row"
    );
    assert!(
        matches!(
            reader.matrix_cell_status::<Sample>(key(ROWS_PER_CHUNK, 0)),
            Err(Error::MatrixCommitQuarantined(name)) if name == Sample::CATEGORY
        ),
        "a quarantined category still answered a status query on a chunked row"
    );
    // An untouched category is unaffected -- the gate is per-category, and a
    // gate that refused everything would pass the assertions above for the
    // wrong reason.
    assert!(
        reader
            .matrix_cell_status::<Marker>(key(ROWS_PER_CHUNK, 1))
            .is_ok(),
        "the quarantine spread to a category the damage did not touch"
    );
    Ok(())
}

/// The category clear consults the gate, which it did not.
///
/// The defect this replaces was an *ordering* one: `clear_open_chunk_category`
/// zeroed the open chunk's commit/written/slot/crc arrays, and only then did
/// `matrix::clear_category` reach its first gate — so a refused clear returned
/// an error to a caller entitled to believe nothing had happened, after the
/// chunk's cells were already gone.
///
/// **That ordering is no longer reachable through this trigger, and the honest
/// consequence is that this case cannot pin it.** The gate now runs first, so
/// a quarantined category is refused before anything is touched; and writing
/// the chunked cell the old bug would have destroyed is itself refused now,
/// which is what the first version of this test discovered by failing. Pinning
/// the ordering directly would need a fault injected into the region clear
/// after the gate passes.
///
/// What is assertable, and is new: the clear is refused at all. `clear_category`
/// checks only the layout-wide fatal gate, never the per-category quarantine,
/// so before this change a quarantined category was cleared — region and open
/// chunk both — without complaint.
#[test]
fn a_quarantined_category_refuses_the_category_clear() -> varve::Result<()> {
    let path = temp_path("quarantine_clear");
    let mut writer = growing_spec().create_writer_with_dims(path.path(), dims())?;
    writer.write_matrix_cell(key(0, 0), &Sample { value: 5 })?;
    writer.commit_matrix_cell::<Sample>(key(0, 0))?;
    writer.flush()?;
    drop(writer);

    let map_off = commit_map_offset(path.path());
    patch_byte(path.path(), map_off, 0x40);

    let mut writer = growing_spec().open_writer(path.path())?;
    assert!(
        matches!(
            writer.clear_matrix_category(Sample::CATEGORY),
            Err(Error::MatrixCommitQuarantined(name)) if name == Sample::CATEGORY
        ),
        "a quarantined category was cleared without complaint"
    );
    // An untouched category still clears, so the refusal is the quarantine and
    // not the clear having become inoperable.
    assert_eq!(writer.clear_matrix_category(Marker::CATEGORY)?, 0);
    Ok(())
}

#[test]
fn clearing_a_category_does_not_write_a_dead_chunk() -> varve::Result<()> {
    let path = temp_path("clear_then_write");
    let mut writer = growing_spec().create_writer_with_dims(path.path(), dims())?;
    // One committed cell in chunk 1, then clear it away and flush.
    writer.write_matrix_cell(key(ROWS_PER_CHUNK, 0), &Sample { value: 3 })?;
    writer.commit_matrix_cell::<Sample>(key(ROWS_PER_CHUNK, 0))?;
    let cleared = writer.clear_matrix_category(Sample::CATEGORY)?;
    assert_eq!(cleared, 1, "the clear did not account for the chunked row");
    writer.flush()?;

    // `dirty` used to survive the clear, so the flush written an
    // all-uncommitted chunk record -- and a written chunk refuses every later
    // write to its rows, permanently.
    writer.write_matrix_cell(key(ROWS_PER_CHUNK, 1), &Sample { value: 4 })?;
    writer.commit_matrix_cell::<Sample>(key(ROWS_PER_CHUNK, 1))?;
    writer.flush()?;
    drop(writer);

    let reader = growing_spec().open_reader(path.path())?;
    assert_eq!(
        reader.read_matrix_cell::<Sample>(key(ROWS_PER_CHUNK, 1))?,
        Sample { value: 4 },
        "the row written after the clear did not survive"
    );
    assert_eq!(
        reader.matrix_cell_status::<Sample>(key(ROWS_PER_CHUNK, 0))?,
        MatrixCellStatus::NotCommitted,
        "the cleared cell came back"
    );
    Ok(())
}

/// Where the matrix header records the commit map. Ported from
/// `matrix_lazy_residency.rs`: 18 + 24 bytes of native prefix, the magic, then
/// 24 bytes into the `VMAT` header, field 6.
fn commit_map_offset(path: &Path) -> u64 {
    use std::io::Read;
    let prefix = 18 + 24 + u64::try_from(growing_spec().magic.len()).expect("magic length");
    let mut file = std::fs::File::open(path).expect("open matrix");
    std::io::Seek::seek(&mut file, std::io::SeekFrom::Start(prefix + 24 + 6 * 8))
        .expect("seek header field");
    let mut bytes = [0; 8];
    file.read_exact(&mut bytes).expect("read header field");
    u64::from_le_bytes(bytes)
}

fn patch_byte(path: &Path, offset: u64, value: u8) {
    use std::io::Write;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .open(path)
        .expect("open matrix for mutation");
    std::io::Seek::seek(&mut file, std::io::SeekFrom::Start(offset)).expect("seek");
    file.write_all(&[value]).expect("patch byte");
}

/// The growing spec with the open digest on. `with_open_digest_on_flush` turns
/// the block-offset chain on itself, which the digest needs to be reachable.
fn digest_growing_spec() -> FormatSpec {
    let spec = growing_spec();
    spec.with_index_policy(spec.index_policy.with_open_digest_on_flush(true))
}

/// `sync()` must leave the digest where a lazy open can still find it.
///
/// Sealing the open chunk is an ordinary record append, so it lands past any
/// digest the file already ends with — and a digest that is not the last
/// record is one no lazy open can use. `flush` writes out *before* it closes the
/// commit point for exactly that reason; `sync` written and stopped, so this
/// sequence silently demoted every later `open_readonly_lazy` to a full scan.
///
/// Nothing is wrong with the file afterwards, which is what makes it worth a
/// test: the fallback is exact, and the only symptom is the eleven-syscall
/// open quietly becoming a hundred-thousand-syscall one.
#[test]
fn sync_after_a_chunk_write_leaves_the_digest_usable() -> varve::Result<()> {
    let spec = digest_growing_spec();
    let path = temp_path("sync_digest");
    let mut writer = spec.create_writer_with_dims(path.path(), dims())?;
    writer.write_matrix_cell(key(0, 0), &Sample { value: 1 })?;
    writer.commit_matrix_cell::<Sample>(key(0, 0))?;
    writer.flush()?;

    // A committed chunked cell lives in RAM until its chunk is written, and
    // `sync` is what writes out it here.
    writer.write_matrix_cell(key(ROWS_PER_CHUNK, 0), &Sample { value: 2 })?;
    writer.commit_matrix_cell::<Sample>(key(ROWS_PER_CHUNK, 0))?;
    writer.sync()?;
    drop(writer);

    let (_file, source) = VarveFile::open_readonly_lazy_with_report(spec, path.path())?;
    assert_eq!(
        source,
        LazyOpenSource::Digest,
        "writing the chunk left the digest buried, so the lazy open fell back to the scan",
    );
    Ok(())
}
