//! A followed handle must not keep a chunk directory built before the follow.
//!
//! `VarveFile::follow` extends a handle along the append log, and a matrix
//! chunk record is an ordinary append-log record — so a run can bring new
//! chunks. The chunk directory is built once on the first chunked read and
//! cached in a `OnceLock`, which nothing in the tree invalidates. Caching it
//! forever was sound only while a handle's snapshot could not grow.
//!
//! Measured before the fix: the followed handle answered `MatrixNotCommitted`
//! for a cell that a fresh open of the same file read back as `Sample { value:
//! 80 }`. The handle had the chunk's record in its index and no entry for it in
//! its directory.

#![cfg(feature = "integrity")]
use varve::{
    BlockDescriptor, BlockKind, Endian, FormatSpec, IndexPolicy, IntegrityPolicy,
    MatrixBlockDescriptor, MatrixCommitDescriptor, MatrixCommitKind, MatrixDimensionDescriptor,
    MatrixDimensions, MatrixKey, ReadLimits, VarveBlock, VarveMatrixBlock,
};

const CHANNELS: u64 = 4;
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

static BLOCKS: &[BlockDescriptor] = &[BlockDescriptor {
    id: Sample::ID,
    name: "Sample",
    version: Sample::VERSION,
    kind: BlockKind::Matrix,
    fields: &[],
}];
static DIMS: &[MatrixDimensionDescriptor] = &[
    MatrixDimensionDescriptor { name: "scan" },
    MatrixDimensionDescriptor { name: "ch" },
];
static COMMITS: &[MatrixCommitDescriptor] = &[MatrixCommitDescriptor {
    name: Sample::CATEGORY,
    kind: MatrixCommitKind::Cell,
}];
static MATRIX_BLOCKS: &[MatrixBlockDescriptor] = &[MatrixBlockDescriptor {
    block_id: Sample::ID,
    dimensions: Sample::DIMENSIONS,
    category: Sample::CATEGORY,
    slot_stride: Sample::SLOT_STRIDE,
}];

fn spec() -> FormatSpec {
    FormatSpec::new(
        b"PRBCHNK0",
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
    .with_growing_matrix_dimension("scan", ROWS_PER_CHUNK)
}

fn dims() -> MatrixDimensions {
    MatrixDimensions::from_pairs([("scan", ROWS_PER_CHUNK), ("ch", CHANNELS)])
}
fn key(row: u64, ch: u64) -> MatrixKey {
    MatrixKey::new(row, ch)
}

#[test]
fn a_follow_invalidates_a_chunk_directory_built_before_it() -> varve::Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("chunkdir.vrv");

    let mut writer = spec().create_writer_with_dims(&path, dims())?;
    // Rows 0..8 => the region (chunk 0) plus one chunk record (chunk 1).
    for row in 0..ROWS_PER_CHUNK * 2 {
        for ch in 0..CHANNELS {
            writer.write_matrix_cell(
                key(row, ch),
                &Sample {
                    value: (row * 10 + ch) as u32,
                },
            )?;
            writer.commit_matrix_cell::<Sample>(key(row, ch))?;
        }
    }
    writer.flush()?;

    let mut reader = spec().open_reader(&path)?;
    // Touch a chunked cell: this builds and caches the chunk directory.
    // Touching a chunked cell is what builds and caches the directory; without
    // this read the OnceLock is empty and the follow has nothing to invalidate.
    assert_eq!(
        reader.read_matrix_cell::<Sample>(key(ROWS_PER_CHUNK, 0))?,
        Sample {
            value: (ROWS_PER_CHUNK * 10) as u32
        },
    );

    // A new chunk arrives.
    for row in ROWS_PER_CHUNK * 2..ROWS_PER_CHUNK * 3 {
        for ch in 0..CHANNELS {
            writer.write_matrix_cell(
                key(row, ch),
                &Sample {
                    value: (row * 10 + ch) as u32,
                },
            )?;
            writer.commit_matrix_cell::<Sample>(key(row, ch))?;
        }
    }
    writer.flush()?;
    drop(writer);

    assert!(reader.follow()? > 0, "the new chunk record was adopted");

    // A cell in the chunk the follow brought. The standard it is held to is the
    // fresh open, not a literal: the two handles must not disagree about a file
    // they both describe.
    let target = key(ROWS_PER_CHUNK * 2, 0);
    let expected = Sample {
        value: ((ROWS_PER_CHUNK * 2) * 10) as u32,
    };
    assert_eq!(
        spec()
            .open_reader(&path)?
            .read_matrix_cell::<Sample>(target)?,
        expected
    );
    assert_eq!(
        reader.read_matrix_cell::<Sample>(target)?,
        expected,
        "the followed handle has the chunk's record but answered from a \
         directory built before it",
    );

    // And the chunks it already had are still right, so the invalidation
    // rebuilt rather than lost.
    assert_eq!(
        reader.read_matrix_cell::<Sample>(key(ROWS_PER_CHUNK, 0))?,
        Sample {
            value: (ROWS_PER_CHUNK * 10) as u32
        },
    );
    Ok(())
}
