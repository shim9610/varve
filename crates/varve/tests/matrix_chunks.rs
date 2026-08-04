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
    //
    // `open` is `None` because the flush above sealed chunk 2, so nothing is
    // open — which the error now says, rather than naming the refused chunk as
    // its own opener.
    assert!(matches!(
        writer.write_matrix_cell(key(ROWS_PER_CHUNK, 1), &Sample { value: 3 }),
        Err(Error::MatrixChunkSealed {
            chunk: 1,
            open: None
        }),
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
    // `read_matrix_cell` answered `MatrixNotCommitted` until the next seal —
    // a write-then-read inconsistency that no test which flushes first can see.
    let path = temp_path("open_chunk_readback");
    let cell = key(ROWS_PER_CHUNK + 1, 2);
    let mut writer = growing_spec().create_writer_with_dims(path.path(), dims())?;
    writer.write_matrix_cell(cell, &Sample { value: 5 })?;
    writer.commit_matrix_cell::<Sample>(cell)?;

    assert_eq!(
        writer.read_matrix_cell::<Sample>(cell)?,
        Sample { value: 5 },
        "the writer must see its own committed cell before the seal",
    );
    assert_eq!(
        writer.matrix_cell_payload::<Sample>(cell)?.len(),
        Sample::SLOT_STRIDE as usize,
    );
    assert_eq!(
        writer.matrix_cell_status::<Sample>(cell)?,
        MatrixCellStatus::Committed,
    );

    // And the same three answers after the seal, from the same handle.
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

#[test]
fn clearing_a_cell_in_the_open_chunk_works_and_in_a_sealed_one_is_refused() -> varve::Result<()> {
    let path = temp_path("clear");
    let sealed_cell = key(ROWS_PER_CHUNK, 0);
    let open_cell = key(ROWS_PER_CHUNK * 2, 1);
    let mut writer = growing_spec().create_writer_with_dims(path.path(), dims())?;
    writer.write_matrix_cell(sealed_cell, &Sample { value: 1 })?;
    writer.commit_matrix_cell::<Sample>(sealed_cell)?;
    writer.write_matrix_cell(open_cell, &Sample { value: 2 })?;
    writer.commit_matrix_cell::<Sample>(open_cell)?;

    writer.clear_matrix_cell::<Sample>(open_cell)?;
    assert_eq!(
        writer.matrix_cell_status::<Sample>(open_cell)?,
        MatrixCellStatus::NotCommitted,
    );
    // A sealed chunk is a written record, and a record is not rewritten — the
    // same refusal a late write gets, for the same reason.
    assert!(matches!(
        writer.clear_matrix_cell::<Sample>(sealed_cell),
        Err(Error::MatrixChunkSealed { chunk: 1, .. }),
    ));
    assert!(matches!(
        writer.clear_matrix_cell_by_category(Sample::CATEGORY, sealed_cell),
        Err(Error::MatrixChunkSealed { chunk: 1, .. }),
    ));
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

#[test]
fn a_reopened_writer_appends_to_a_later_chunk_and_still_refuses_the_sealed_one() -> varve::Result<()>
{
    let path = temp_path("reopen_append");
    let first = key(ROWS_PER_CHUNK, 0);
    let later = key(ROWS_PER_CHUNK * 3, 0);
    {
        let mut writer = growing_spec().create_writer_with_dims(path.path(), dims())?;
        writer.write_matrix_cell(first, &Sample { value: 5 })?;
        writer.commit_matrix_cell::<Sample>(first)?;
        writer.flush()?;
    }
    let mut writer = growing_spec().open_writer(path.path())?;
    // The sealed write comes **first**, while no chunk is open. Ordered the
    // other way this test proved nothing: opening chunk 3 first makes the
    // refusal come from the in-memory `open_chunk.index > index` branch, so a
    // build that forgot the watermark across a reopen still passed. Only the
    // crash sweep caught that, which is why the order here is deliberate.
    assert!(matches!(
        writer.write_matrix_cell(first, &Sample { value: 1 }),
        Err(Error::MatrixChunkSealed {
            chunk: 1,
            open: None
        }),
    ));
    writer.write_matrix_cell(later, &Sample { value: 9 })?;
    writer.commit_matrix_cell::<Sample>(later)?;
    assert!(matches!(
        writer.write_matrix_cell(first, &Sample { value: 1 }),
        Err(Error::MatrixChunkSealed {
            chunk: 1,
            open: Some(3)
        }),
    ));
    writer.flush()?;
    drop(writer);

    let reader = growing_spec().open_reader(path.path())?;
    assert_eq!(
        reader.read_matrix_cell::<Sample>(first)?,
        Sample { value: 5 }
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
        // A surviving chunk is sealed: reopening it must be refused, not
        // silently duplicated.
        if let Some(newest) = survived.last() {
            assert!(
                matches!(
                    writer.write_matrix_cell(key(newest * ROWS_PER_CHUNK, 1), &Sample { value: 0 }),
                    Err(Error::MatrixChunkSealed { .. }),
                ),
                "cut {cut}: chunk {newest} survived but was reopenable",
            );
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
    // and the row could become permanently unwritable once a later chunk sealed
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
fn dropping_a_writer_seals_the_open_chunk() -> varve::Result<()> {
    // Every other byte a writer accepts is on disk before the call returns; a
    // chunked cell was the one exception, and dropping the writer lost every
    // committed cell in the open chunk. Best effort — `drop` cannot report a
    // failure — but the ordinary case must not lose data.
    let path = temp_path("drop_seals");
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
fn a_chunk_that_could_never_be_sealed_is_refused_at_create() -> varve::Result<()> {
    // A chunk is buffered against `MatrixSlotRegionLen` and sealed against
    // `RecordPayloadLen`, and nothing reconciled them: a spec passed
    // `validate`, accepted writes, and then died at the first seal with the
    // data already in RAM and no way to get it out. Refused at create now,
    // before a byte is accepted.
    //
    // Closing this also closed the only public route to a failed seal, so the
    // property that a failed seal keeps its chunk moved to a unit test in
    // `file.rs` (`a_failed_seal_keeps_the_chunk`), which says so.
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
fn clearing_a_category_counts_the_open_chunk_and_refuses_a_sealed_one() -> varve::Result<()> {
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

    // Once a chunk is sealed it is a written record, and records are not
    // rewritten. Saying so is the only honest answer.
    writer.write_matrix_cell(chunked, &Sample { value: 3 })?;
    writer.commit_matrix_cell::<Sample>(chunked)?;
    writer.flush()?;
    assert!(matches!(
        writer.clear_matrix_category(Sample::CATEGORY),
        Err(Error::InvalidFormatSpec(message)) if message.contains("sealed chunk"),
    ));
    Ok(())
}

#[test]
fn the_resume_signal_is_not_clean_over_an_unsealed_chunk() -> varve::Result<()> {
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
        "an unsealed chunk is unfinished work",
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
    // alone is satisfied by any large constant — see
    // `.internal-docs/index-residency-spec.md` §4.6.1.
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
fn a_flipped_bit_in_a_sealed_chunk_is_refused_not_returned() -> varve::Result<()> {
    // The review reproduced this: flipping bytes in a sealed chunk's slot
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
    // Find the value in the sealed chunk and corrupt it in place.
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
