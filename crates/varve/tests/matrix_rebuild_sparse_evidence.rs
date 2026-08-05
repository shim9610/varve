//! The explicit CRC commit-map rebuild follows its evidence.
//!
//! `rebuild_matrix_commit_from_crc` visits every declared cell — the published
//! recovery model requires the full-keyspace sweep and this does not change it
//! — but for each cell it used to read and hash the whole slot, read the stored
//! CRC, and only *then* consult the CRC-validity bit that decides the outcome
//! on its own. A cell whose validity bit is clear is not committed whatever its
//! slot hashes to, so those reads answered a question already answered.
//!
//! Three things are asserted, and the second is the one that matters:
//!
//! A. the cost follows the set bits, not the cell count;
//! B. the rebuilt map is **identical** — an inverted condition would "improve"
//!    (A) while rebuilding the complement, and a fix that skipped the bit write
//!    along with the reads would leave the map short;
//! C. where every bit is set the reads are genuinely needed, and the counter
//!    must not pretend otherwise.
#![cfg(all(feature = "integrity", feature = "scalable-fault-injection"))]

use std::path::Path;

use varve::{
    BlockDescriptor, BlockKind, Endian, FormatSpec, IndexPolicy, IntegrityPolicy,
    MatrixBlockDescriptor, MatrixCellStatus, MatrixCommitDescriptor, MatrixCommitKind,
    MatrixDimensionDescriptor, MatrixDimensions, MatrixKey, MatrixRecoveryReport, ReadLimits,
    VarveBlock, VarveMatrixBlock,
};

/// Cells per scan.
const CHANNELS: u64 = 128;
/// 32 scans x 128 channels = 4,096 declared cells.
const SCANS: u64 = 32;
const CELLS: u64 = SCANS * CHANNELS;
/// Written and committed in the sparse case: 64 set validity bits, 4,032 clear.
const WRITTEN: u64 = 64;

#[derive(Clone, Debug, PartialEq, Eq, VarveBlock)]
#[varve(id = 761, version = 1, kind = "matrix")]
struct RebuildCell {
    value: u32,
}

impl VarveMatrixBlock for RebuildCell {
    const DIMENSIONS: [&'static str; 2] = ["scan", "ch"];
    const CATEGORY: &'static str = "analysis";
    const SLOT_STRIDE: u64 = 4;
}

fn spec() -> FormatSpec {
    static BLOCKS: &[BlockDescriptor] = &[BlockDescriptor {
        id: RebuildCell::ID,
        name: "RebuildCell",
        version: RebuildCell::VERSION,
        kind: BlockKind::Matrix,
        fields: RebuildCell::FIELDS,
    }];
    static DIMENSIONS: &[MatrixDimensionDescriptor] = &[
        MatrixDimensionDescriptor { name: "scan" },
        MatrixDimensionDescriptor { name: "ch" },
    ];
    static COMMITS: &[MatrixCommitDescriptor] = &[MatrixCommitDescriptor {
        name: RebuildCell::CATEGORY,
        kind: MatrixCommitKind::Cell,
    }];
    static MATRIX_BLOCKS: &[MatrixBlockDescriptor] = &[MatrixBlockDescriptor {
        block_id: RebuildCell::ID,
        dimensions: RebuildCell::DIMENSIONS,
        category: RebuildCell::CATEGORY,
        slot_stride: RebuildCell::SLOT_STRIDE,
    }];

    FormatSpec::new(
        b"MRBD",
        1,
        Endian::Little,
        0,
        IndexPolicy::ScanOnOpen,
        IntegrityPolicy::Crc32,
        varve::RecoveryPolicy::Strict,
        varve::ManifestPolicy::None,
        BLOCKS,
    )
    .with_read_limits(ReadLimits::STANDARD)
    .with_matrix_spec(DIMENSIONS, COMMITS, MATRIX_BLOCKS)
}

fn key(ordinal: u64) -> MatrixKey {
    MatrixKey::new(ordinal / CHANNELS, ordinal % CHANNELS)
}

fn temp_dir(name: &str) -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix(&format!("varve-matrix-rebuild-{name}-"))
        .tempdir()
        .expect("temp dir")
}

/// Writes and commits ordinals `0..count` of a `CELLS`-cell matrix.
fn fill(path: &Path, count: u64) -> varve::Result<()> {
    let mut writer = spec().create_writer_with_dims(
        path,
        MatrixDimensions::from_pairs([("scan", SCANS), ("ch", CHANNELS)]),
    )?;
    for ordinal in 0..count {
        writer.write_matrix_cell(
            key(ordinal),
            &RebuildCell {
                value: u32::try_from(ordinal + 1).expect("ordinal fits u32"),
            },
        )?;
        writer.commit_matrix_cell::<RebuildCell>(key(ordinal))?;
    }
    writer.flush()?;
    Ok(())
}

/// Runs the explicit rebuild and returns `(committed count, slot bytes read)`.
fn rebuild(path: &Path) -> varve::Result<(u64, u64)> {
    let mut writer = spec().open_writer(path)?;
    MatrixRecoveryReport::reset_matrix_integrity_counters();
    let committed = writer.rebuild_matrix_commit_from_crc::<RebuildCell>()?;
    let read = MatrixRecoveryReport::matrix_rebuild_slot_bytes_read();
    writer.flush()?;
    Ok((committed, read))
}

#[test]
fn a_sparse_rebuild_reads_only_the_slots_its_evidence_names() -> varve::Result<()> {
    let dir = temp_dir("sparse");
    let path = dir.path().join("matrix.varve");
    fill(&path, WRITTEN)?;

    let (committed, slot_bytes) = rebuild(&path)?;

    // Half A: the cost.
    assert_eq!(
        slot_bytes,
        WRITTEN * RebuildCell::SLOT_STRIDE,
        "the rebuild read {slot_bytes} slot bytes for {WRITTEN} set validity bits \
         out of {CELLS} declared cells; the whole-keyspace figure is {}",
        CELLS * RebuildCell::SLOT_STRIDE
    );

    // Half B: the map. This is what an inverted condition fails.
    assert_eq!(committed, WRITTEN, "the rebuild published the wrong count");
    let reader = spec().open_reader(&path)?;
    for ordinal in 0..CELLS {
        let expected = if ordinal < WRITTEN {
            MatrixCellStatus::Committed
        } else {
            MatrixCellStatus::NotCommitted
        };
        assert_eq!(
            reader.matrix_cell_status::<RebuildCell>(key(ordinal))?,
            expected,
            "ordinal {ordinal} came back from the rebuild with the wrong status"
        );
    }
    // The cells that were written must still read back their values, so "the
    // map is identical" is not satisfied by a rebuild that lost the payload.
    for ordinal in [0u64, 1, WRITTEN - 1] {
        assert_eq!(
            reader.read_matrix_cell::<RebuildCell>(key(ordinal))?,
            RebuildCell {
                value: u32::try_from(ordinal + 1).expect("ordinal fits u32"),
            }
        );
    }
    Ok(())
}

#[test]
fn a_dense_rebuild_claims_no_saving_it_does_not_make() -> varve::Result<()> {
    let dir = temp_dir("dense");
    let path = dir.path().join("matrix.varve");
    fill(&path, CELLS)?;

    let (committed, slot_bytes) = rebuild(&path)?;

    // Half C: every validity bit is set, so every slot read is needed and the
    // counter must show the full-keyspace figure — the same value the eager
    // version produced. A "fix" that skipped reads here would be skipping reads
    // whose result it needs.
    assert_eq!(
        slot_bytes,
        CELLS * RebuildCell::SLOT_STRIDE,
        "with every validity bit set the rebuild must still read every slot"
    );
    assert_eq!(committed, CELLS);

    let reader = spec().open_reader(&path)?;
    for ordinal in [0u64, CELLS / 2, CELLS - 1] {
        assert_eq!(
            reader.matrix_cell_status::<RebuildCell>(key(ordinal))?,
            MatrixCellStatus::Committed
        );
    }
    Ok(())
}
