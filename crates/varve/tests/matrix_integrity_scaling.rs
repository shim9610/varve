//! Scaling contracts for the paged matrix integrity representation
//! (PERF-01 and PERF-02).
//!
//! Every assertion here is a counter or a typed error, never wall-clock time:
//! the point is that the *amount of work* per commit-bit mutation, per create,
//! and per open is bounded independently of the total cell count, while
//! corruption detection, rebuild, and stale-artifact rejection are unchanged.
//!
//! All fixtures are small. The matrices differ only by a 4x cell-count ratio,
//! and their payload/checksum extents are sparse zero extents, so nothing here
//! allocates a large physical file.
#![cfg(all(feature = "integrity", feature = "scalable-fault-injection"))]

use std::fs::OpenOptions;
use std::io::{Seek, SeekFrom, Write};
use std::path::Path;

use varve::{
    BlockDescriptor, BlockKind, Endian, Error, FormatSpec, IndexPolicy, IntegrityPolicy,
    MatrixBlockDescriptor, MatrixCellStatus, MatrixCommitDescriptor, MatrixCommitKind,
    MatrixCorruptionKind, MatrixCorruptionSeverity, MatrixDimensionDescriptor, MatrixDimensions,
    MatrixKey, MatrixMetadataVerification, MatrixRecoveryAction, MatrixRecoveryReport, ReadLimits,
    VarveBlock, VarveMatrixBlock,
};

/// Commit-map page size used by the paged integrity representation.
const PAGE_BYTES: u64 = 4096;
/// Native file header, then the 24-byte matrix creation-nonce region (DUR2-03).
const NATIVE_PREFIX_LEN: u64 = 18 + 24;
/// Current on-disk matrix layout version (v4 gives the persisted page index a
/// validated occupancy header and makes it the live set, not the history).
const VMAT_VERSION: u16 = 4;
/// One persisted page-index slot.
const PAGE_INDEX_SLOT_LEN: u64 = 8;

/// `CHANNELS` bits per scan, so a scan count of `n` yields `n * CHANNELS` cells
/// and a commit map of `n * CHANNELS / 8` bytes.
const CHANNELS: u64 = 128;
/// 32_768 cells: a commit map of exactly one 4 KiB page.
const SMALL_SCANS: u64 = 256;
/// 131_072 cells: a commit map of exactly four 4 KiB pages.
const LARGE_SCANS: u64 = 1024;

/// 2_097_152 cells: a commit map of 256 KiB, i.e. several filesystem
/// allocation granules wide. Open-cost measurements need maps wider than one
/// granule, otherwise a single written page allocates the whole region and no
/// skipping is observable.
const SMALL_WIDE_SCANS: u64 = 16_384;
/// 8_388_608 cells: a commit map of 1 MiB, a 4x ratio against
/// `SMALL_WIDE_SCANS`. The payload, checksum, and bitmap extents are sparse, so
/// the physical fixtures stay tiny.
const LARGE_WIDE_SCANS: u64 = 65_536;

#[derive(Clone, Debug, PartialEq, Eq, VarveBlock)]
#[varve(id = 760, version = 1, kind = "matrix")]
struct ScalingCell {
    value: u32,
}

impl VarveMatrixBlock for ScalingCell {
    const DIMENSIONS: [&'static str; 2] = ["scan", "ch"];
    const CATEGORY: &'static str = "analysis";
    const SLOT_STRIDE: u64 = 4;
}

fn spec() -> FormatSpec {
    static BLOCKS: &[BlockDescriptor] = &[BlockDescriptor {
        id: ScalingCell::ID,
        name: "ScalingCell",
        version: ScalingCell::VERSION,
        kind: BlockKind::Matrix,
        fields: ScalingCell::FIELDS,
    }];
    static DIMENSIONS: &[MatrixDimensionDescriptor] = &[
        MatrixDimensionDescriptor { name: "scan" },
        MatrixDimensionDescriptor { name: "ch" },
    ];
    static COMMITS: &[MatrixCommitDescriptor] = &[MatrixCommitDescriptor {
        name: ScalingCell::CATEGORY,
        kind: MatrixCommitKind::Cell,
    }];
    static MATRIX_BLOCKS: &[MatrixBlockDescriptor] = &[MatrixBlockDescriptor {
        block_id: ScalingCell::ID,
        dimensions: ScalingCell::DIMENSIONS,
        category: ScalingCell::CATEGORY,
        slot_stride: ScalingCell::SLOT_STRIDE,
    }];

    FormatSpec::new(
        b"MSCL",
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

fn dims(scans: u64) -> MatrixDimensions {
    MatrixDimensions::from_pairs([("scan", scans), ("ch", CHANNELS)])
}

fn key(ordinal: u64) -> MatrixKey {
    MatrixKey::new(ordinal / CHANNELS, ordinal % CHANNELS)
}

/// Writes and commits `count` cells starting at ordinal zero.
fn fill(path: &Path, scans: u64, count: u64) -> varve::Result<()> {
    let mut writer = spec().create_writer_with_dims(path, dims(scans))?;
    for ordinal in 0..count {
        writer.write_matrix_cell(
            key(ordinal),
            &ScalingCell {
                value: u32::try_from(ordinal + 1).expect("ordinal fits u32"),
            },
        )?;
        writer.commit_matrix_cell::<ScalingCell>(key(ordinal))?;
    }
    writer.flush()?;
    Ok(())
}

fn vmat_header_offset() -> u64 {
    NATIVE_PREFIX_LEN + u64::try_from(spec().magic.len()).expect("magic length")
}

/// Reads a `u64` layout-header field by index (the header's `u64` block starts
/// 24 bytes into the VMAT header).
fn header_u64(path: &Path, index: u64) -> u64 {
    use std::io::Read;
    let mut file = OpenOptions::new()
        .read(true)
        .open(path)
        .expect("open matrix");
    file.seek(SeekFrom::Start(vmat_header_offset() + 24 + index * 8))
        .expect("seek header field");
    let mut bytes = [0; 8];
    file.read_exact(&mut bytes).expect("read header field");
    u64::from_le_bytes(bytes)
}

fn commit_map_off(path: &Path) -> u64 {
    header_u64(path, 6)
}

/// Base of the persisted page-index region. The first category's array starts
/// here: slot 0 is the occupancy header, slot `k + 1` is entry `k`.
fn page_index_off(path: &Path) -> u64 {
    header_u64(path, 13)
}

fn patch_u64(path: &Path, offset: u64, value: u64) {
    let mut file = OpenOptions::new()
        .write(true)
        .open(path)
        .expect("open matrix for mutation");
    file.seek(SeekFrom::Start(offset)).expect("seek");
    file.write_all(&value.to_le_bytes()).expect("patch u64");
}

fn read_u64_at(path: &Path, offset: u64) -> u64 {
    use std::io::Read;
    let mut file = OpenOptions::new()
        .read(true)
        .open(path)
        .expect("open matrix");
    file.seek(SeekFrom::Start(offset)).expect("seek");
    let mut bytes = [0; 8];
    file.read_exact(&mut bytes).expect("read u64");
    u64::from_le_bytes(bytes)
}

/// Opens with filesystem allocation-map discovery disabled, which is also what
/// a file fragmented past the tracked-extent cap produces. The persisted page
/// index is then the *only* way to find a written page.
fn open_without_allocation_map(path: &Path, forensics: bool) -> varve::Result<varve::VarveReader> {
    let spec = if forensics {
        spec().with_matrix_fatal_forensics()
    } else {
        spec()
    };
    MatrixRecoveryReport::force_matrix_allocation_map_unavailable(true);
    let opened = spec.open_reader(path);
    MatrixRecoveryReport::force_matrix_allocation_map_unavailable(false);
    opened
}

fn patch_byte(path: &Path, offset: u64, value: u8) {
    let mut file = OpenOptions::new()
        .write(true)
        .open(path)
        .expect("open matrix for mutation");
    file.seek(SeekFrom::Start(offset)).expect("seek");
    file.write_all(&[value]).expect("patch byte");
}

fn temp_dir(name: &str) -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix(&format!("varve-matrix-scaling-{name}-"))
        .tempdir()
        .expect("temp dir")
}

// PERF-01 -------------------------------------------------------------------

/// A commit-bit mutation must hash one page, not the whole category bitmap, so
/// quadrupling the cell count must not change the per-mutation hashing cost.
#[test]
fn commit_bit_mutation_hashing_cost_is_independent_of_cell_count() -> varve::Result<()> {
    const CELLS: u64 = 64;
    let dir = temp_dir("hash-cost");

    let mut measured = Vec::new();
    for (name, scans) in [("small", SMALL_SCANS), ("large", LARGE_SCANS)] {
        let path = dir.path().join(format!("{name}.varve"));
        MatrixRecoveryReport::reset_matrix_integrity_counters();
        fill(&path, scans, CELLS)?;
        measured.push(MatrixRecoveryReport::matrix_bitmap_bytes_hashed());
    }

    let (small, large) = (measured[0], measured[1]);
    assert_eq!(
        small, large,
        "per-mutation hashing scaled with cell count: {small} vs {large}"
    );
    // Writing clears the commit bit and committing sets it, so each cell costs
    // exactly two page hashes.
    assert_eq!(small, CELLS * 2 * PAGE_BYTES);
    Ok(())
}

/// The bounded cost also holds for a mutation in a high page: only the page
/// containing the mutated byte is rehashed.
#[test]
fn a_single_mutation_hashes_exactly_one_page_anywhere_in_the_map() -> varve::Result<()> {
    let dir = temp_dir("single-page");
    let path = dir.path().join("matrix.varve");
    let mut writer = spec().create_writer_with_dims(&path, dims(LARGE_SCANS))?;

    // The last cell lives in the final commit-map page; the first lives in the
    // first page. Both must cost the same.
    for ordinal in [0, LARGE_SCANS * CHANNELS - 1] {
        MatrixRecoveryReport::reset_matrix_integrity_counters();
        writer.write_matrix_cell(key(ordinal), &ScalingCell { value: 7 })?;
        assert_eq!(
            MatrixRecoveryReport::matrix_bitmap_bytes_hashed(),
            PAGE_BYTES,
            "mutation at ordinal {ordinal} hashed more than one page"
        );
    }
    Ok(())
}

// PERF-02 -------------------------------------------------------------------

/// Creation must not explicitly initialise per-cell metadata: the commit maps,
/// per-cell checksums, and validity bitmaps are a sparse zero extent.
#[test]
fn create_metadata_bytes_do_not_scale_with_cell_count() -> varve::Result<()> {
    let dir = temp_dir("create-cost");

    let mut measured = Vec::new();
    for (name, scans) in [("small", SMALL_SCANS), ("large", LARGE_SCANS)] {
        let path = dir.path().join(format!("{name}.varve"));
        MatrixRecoveryReport::reset_matrix_integrity_counters();
        drop(spec().create_writer_with_dims(&path, dims(scans))?);
        measured.push(MatrixRecoveryReport::matrix_create_metadata_bytes_written());
    }

    let (small, large) = (measured[0], measured[1]);
    assert_eq!(
        small, large,
        "create metadata writes scaled with cell count: {small} vs {large}"
    );
    // Descriptor tables plus the checksum-region header only.
    assert!(small < 1024, "create wrote {small} metadata bytes");
    Ok(())
}

/// Opening must make resident only the bitmap pages that carry state, so a 4x
/// larger matrix holding the same committed cells must cost the same memory.
#[test]
fn resident_bitmap_bytes_after_open_do_not_scale_with_cell_count() -> varve::Result<()> {
    const CELLS: u64 = 64;
    let dir = temp_dir("residency");

    let mut measured = Vec::new();
    for (name, scans) in [("small", SMALL_SCANS), ("large", LARGE_SCANS)] {
        let path = dir.path().join(format!("{name}.varve"));
        fill(&path, scans, CELLS)?;
        MatrixRecoveryReport::reset_matrix_integrity_counters();
        drop(spec().open_readonly(&path)?);
        measured.push(MatrixRecoveryReport::matrix_open_resident_bitmap_bytes());
    }

    let (small, large) = (measured[0], measured[1]);
    assert_eq!(
        small, large,
        "resident bitmap bytes scaled with cell count: {small} vs {large}"
    );
    // Zero, not "one commit-map page plus one checksum-validity page" as it was
    // before 0.5.0. Residency is demand-filled: an open materialises no bitmap
    // payload at all, so the scale-free half of this contract now holds for the
    // strongest possible reason. The absolute number is asserted because the
    // equality above is satisfied by any constant — the shape that let "open is
    // O(1)" stand while open read 128 KiB.
    assert_eq!(
        small, 0,
        "an open materialised {small} bitmap payload bytes; demand residency takes none"
    );
    Ok(())
}

/// A freshly created matrix touches no page at all, whatever its cell count.
#[test]
fn untouched_matrix_holds_no_resident_bitmap_pages() -> varve::Result<()> {
    let dir = temp_dir("untouched");
    let path = dir.path().join("matrix.varve");
    drop(spec().create_writer_with_dims(&path, dims(LARGE_SCANS))?);

    MatrixRecoveryReport::reset_matrix_integrity_counters();
    drop(spec().open_readonly(&path)?);
    assert_eq!(MatrixRecoveryReport::matrix_open_resident_bitmap_bytes(), 0);
    Ok(())
}

/// Opening must not *read* the never-written pages either.
///
/// Residency was already sparse, but authenticating a
/// `PAGE_STATE_UNINITIALIZED` page as still-zero used to require streaming
/// every page of every commit map and validity bitmap: `Theta(cell_count / 8)`
/// of sequential I/O per open, 32 GiB for a 2^38-cell block. The proof now
/// comes from the filesystem allocation map, so a 4x larger matrix holding the
/// same committed cells must read the same number of bytes.
#[test]
fn open_bitmap_bytes_read_do_not_scale_with_cell_count() -> varve::Result<()> {
    const CELLS: u64 = 64;
    let dir = temp_dir("open-io");

    let mut measured = Vec::new();
    for (name, scans) in [("small", SMALL_WIDE_SCANS), ("large", LARGE_WIDE_SCANS)] {
        let path = dir.path().join(format!("{name}.varve"));
        fill(&path, scans, CELLS)?;
        MatrixRecoveryReport::reset_matrix_integrity_counters();
        drop(spec().open_readonly(&path)?);
        assert!(
            MatrixRecoveryReport::matrix_open_allocation_map_available(),
            "no filesystem allocation map for {name}: open cannot be bounded here"
        );
        measured.push(MatrixRecoveryReport::matrix_open_bitmap_bytes_read());
    }

    let (small, large) = (measured[0], measured[1]);
    // The page-byte term must not grow at all. The one term that may is the
    // 8-byte page digest, which a hole page still reads so that a digest
    // recorded without its page (a torn commit) stays detectable: 8 bytes per
    // 4 KiB of map, i.e. 1/4096 of a bit per cell, and only inside allocation
    // granules the filesystem already reports as written.
    let digest_bytes = |scans: u64| (scans * CHANNELS / 8).div_ceil(PAGE_BYTES) * 8;
    let allowance = digest_bytes(LARGE_WIDE_SCANS) - digest_bytes(SMALL_WIDE_SCANS);
    assert!(
        large <= small + allowance,
        "open-time bitmap reads scaled with cell count: {small} vs {large} \
         (digest allowance {allowance})"
    );
    // The dense cost is the commit map plus the validity bitmap, i.e. one
    // quarter of the cell count in bytes. The large matrix must read less than
    // even the *small* matrix's dense cost.
    let small_dense = SMALL_WIDE_SCANS * CHANNELS / 4;
    assert!(
        large < small_dense,
        "open read {large} bytes, not meaningfully below the smaller dense cost {small_dense}"
    );
    Ok(())
}

/// The loop bound itself must not follow the logical page count.
///
/// Bytes read were always the weaker witness: a `0..page_count` loop that skips
/// every page still runs `page_count` times, which is 4.29 billion iterations
/// per bitmap for a 1 PiB payload of 8-byte cells. Open now visits the union of
/// the persisted page index and the pages the allocation map reports, so two
/// matrices whose logical sizes differ by 4x but that hold the same live pages
/// must visit exactly the same number of pages.
#[test]
fn open_bitmap_pages_visited_do_not_scale_with_cell_count() -> varve::Result<()> {
    const CELLS: u64 = 64;
    let dir = temp_dir("pages-visited");

    let mut measured = Vec::new();
    for (name, scans) in [("small", SMALL_WIDE_SCANS), ("large", LARGE_WIDE_SCANS)] {
        let path = dir.path().join(format!("{name}.varve"));
        fill(&path, scans, CELLS)?;
        MatrixRecoveryReport::reset_matrix_integrity_counters();
        drop(spec().open_readonly(&path)?);
        measured.push(MatrixRecoveryReport::matrix_open_bitmap_pages_visited());
    }

    let (small, large) = (measured[0], measured[1]);
    // With an allocation map available the visit set also covers the pages the
    // filesystem reports as written, and a filesystem allocates in runs rather
    // than in 4 KiB pages, so the two fixtures may differ by up to one run.
    // What must not happen is the 4x logical ratio appearing in the counts.
    // `open_stays_bounded_without_an_allocation_map` asserts exact equality on
    // the allocation-map-free path, where the count is purely index-driven.
    const ALLOCATION_RUN_PAGES: u64 = 64;
    assert!(
        large.abs_diff(small) <= ALLOCATION_RUN_PAGES,
        "open page visits scaled with cell count: {small} vs {large}"
    );
    // The whole fixture lives in the first page of each map, so the visit count
    // must stay far below the map width.
    let large_logical_pages = (LARGE_WIDE_SCANS * CHANNELS / 8).div_ceil(PAGE_BYTES);
    assert!(
        large < large_logical_pages,
        "open visited {large} pages, not meaningfully below the logical page count \
         {large_logical_pages}"
    );
    // The small fixture's own logical page count is the honest scale-free
    // witness: a loop that followed logical size would have visited at least
    // four times as many pages for the large one.
    let small_logical_pages = (SMALL_WIDE_SCANS * CHANNELS / 8).div_ceil(PAGE_BYTES);
    assert!(
        large < small_logical_pages,
        "open visited {large} pages for the 4x matrix, not below the smaller \
         matrix's logical page count {small_logical_pages}"
    );
    Ok(())
}

/// The bound must survive the loss of the allocation map.
///
/// A platform that cannot answer, or a file fragmented past the tracked-extent
/// cap, used to fall back to treating every logical page as readable data —
/// turning a sparse but fragmented PB file into a dense one. The persisted page
/// index is the fallback now, so the visit count is still the published pages
/// and still identical across a 4x difference in logical size.
#[test]
fn open_stays_bounded_without_an_allocation_map() -> varve::Result<()> {
    const CELLS: u64 = 64;
    let dir = temp_dir("no-alloc-map");

    let mut measured = Vec::new();
    let mut read_bytes = Vec::new();
    for (name, scans) in [("small", SMALL_WIDE_SCANS), ("large", LARGE_WIDE_SCANS)] {
        let path = dir.path().join(format!("{name}.varve"));
        fill(&path, scans, CELLS)?;
        MatrixRecoveryReport::reset_matrix_integrity_counters();
        MatrixRecoveryReport::force_matrix_allocation_map_unavailable(true);
        let opened = spec().open_readonly(&path);
        MatrixRecoveryReport::force_matrix_allocation_map_unavailable(false);
        drop(opened?);
        assert!(
            !MatrixRecoveryReport::matrix_open_allocation_map_available(),
            "the allocation map was still available for {name}"
        );
        measured.push(MatrixRecoveryReport::matrix_open_bitmap_pages_visited());
        read_bytes.push(MatrixRecoveryReport::matrix_open_bitmap_bytes_read());
    }

    let (small, large) = (measured[0], measured[1]);
    assert_eq!(
        small, large,
        "open page visits scaled with cell count without an allocation map: {small} vs {large}"
    );
    let (small_bytes, large_bytes) = (read_bytes[0], read_bytes[1]);
    assert_eq!(
        small_bytes, large_bytes,
        "open reads scaled with cell count without an allocation map: \
         {small_bytes} vs {large_bytes}"
    );
    // Without an allocation map the old loader read the complete commit map and
    // validity bitmap: a quarter of the cell count in bytes.
    let large_dense = LARGE_WIDE_SCANS * CHANNELS / 4;
    assert!(
        large_bytes < large_dense,
        "open read {large_bytes} bytes, not meaningfully below the dense cost {large_dense}"
    );
    // The absolute companion, added because the two equalities above are
    // satisfied by *any* constant, however large — the shape that let criterion
    // (A)'s absolute half stay unmet for eight rounds while its scaling half
    // passed. All 64 committed cells live in page 0, so the visit set with no
    // allocation map is the persisted page index alone: one page for the commit
    // map and one for the validity bitmap.
    println!(
        "(no-alloc-map) pages_visited={large} bytes_read={large_bytes} \
         for 1 live page per bitmap"
    );
    assert!(
        large <= 4 && large_bytes <= 4 * PAGE_BYTES + 1024,
        "open visited {large} pages / read {large_bytes} bytes for one live page per bitmap"
    );
    Ok(())
}

/// Index-driven open must keep detecting corruption in a page it does visit,
/// and rebuild must still recover the category — with no allocation map to fall
/// back on.
#[test]
fn published_page_corruption_is_detected_without_an_allocation_map() -> varve::Result<()> {
    const CELLS: u64 = 8;
    let dir = temp_dir("no-alloc-map-corruption");
    let path = dir.path().join("matrix.varve");
    fill(&path, SMALL_WIDE_SCANS, CELLS)?;

    patch_byte(&path, commit_map_off(&path), 0x00);

    MatrixRecoveryReport::force_matrix_allocation_map_unavailable(true);
    let opened = spec().open_reader(&path);
    MatrixRecoveryReport::force_matrix_allocation_map_unavailable(false);
    let reader = opened?;
    assert!(
        !MatrixRecoveryReport::matrix_open_allocation_map_available(),
        "the allocation map was still available"
    );
    assert!(matches!(
        reader.matrix_cell_status::<ScalingCell>(key(0)),
        Err(Error::MatrixCommitQuarantined(name)) if name == ScalingCell::CATEGORY
    ));
    assert!(
        reader
            .matrix_recovery_report()
            .findings
            .iter()
            .any(|finding| finding.kind == MatrixCorruptionKind::CommitMap),
        "index-driven open missed corruption in a published page"
    );
    drop(reader);

    let mut writer = spec().open_writer(&path)?;
    assert_eq!(
        writer.rebuild_matrix_commit_from_crc::<ScalingCell>()?,
        CELLS
    );
    assert_eq!(
        writer.read_matrix_cell::<ScalingCell>(key(3))?,
        ScalingCell { value: 4 }
    );
    drop(writer);

    // The rebuilt map must reopen cleanly through the index alone.
    MatrixRecoveryReport::force_matrix_allocation_map_unavailable(true);
    let opened = spec().open_reader(&path);
    MatrixRecoveryReport::force_matrix_allocation_map_unavailable(false);
    let reader = opened?;
    assert!(
        !reader
            .matrix_recovery_report()
            .findings
            .iter()
            .any(|finding| finding.kind == MatrixCorruptionKind::CommitMap),
        "rebuild did not restore a cleanly readable commit map"
    );
    assert!(matches!(
        reader.matrix_cell_status::<ScalingCell>(key(0)),
        Ok(varve::MatrixCellStatus::Committed)
    ));
    Ok(())
}

/// A page whose final set bit is cleared must be released, not retained.
///
/// Set/clear churn used to hold residency proportional to the pages a writer
/// had historically touched, even at `ones == 0`, because only a whole-map
/// clear, rebuild, or reopen released a page. Residency must instead return to
/// the value it had before the page was touched.
#[test]
fn cleared_pages_are_evicted_and_residency_returns_to_baseline() -> varve::Result<()> {
    let dir = temp_dir("page-eviction");
    let path = dir.path().join("matrix.varve");
    let mut writer = spec().create_writer_with_dims(&path, dims(LARGE_SCANS))?;

    // All three cells fall in the first page of every map, so a cycle over them
    // materialises and then empties exactly one commit page and one validity
    // page. The session's write-tracking map is not part of the cycle: it
    // records that this writer wrote the slots, which stays true.
    const ORDINALS: [u64; 3] = [0, 1, 2];
    for ordinal in ORDINALS {
        writer.write_matrix_cell(key(ordinal), &ScalingCell { value: 9 })?;
        writer.commit_matrix_cell::<ScalingCell>(key(ordinal))?;
        writer.clear_matrix_cell::<ScalingCell>(key(ordinal))?;
    }
    let baseline = MatrixRecoveryReport::matrix_resident_bitmap_bytes();
    assert_eq!(
        baseline, PAGE_BYTES,
        "residency after clearing every committed cell is not the single \
         session write-tracking page"
    );

    // Repeating the cycle must return to that baseline every time rather than
    // retaining a page per historically touched page.
    for round in 0..4 {
        for ordinal in ORDINALS {
            writer.commit_matrix_cell::<ScalingCell>(key(ordinal))?;
        }
        let peak = MatrixRecoveryReport::matrix_resident_bitmap_bytes();
        assert!(
            peak > baseline,
            "round {round} committed cells without making a page resident \
             ({peak} vs {baseline})"
        );
        for ordinal in ORDINALS {
            writer.clear_matrix_cell::<ScalingCell>(key(ordinal))?;
        }
        let after = MatrixRecoveryReport::matrix_resident_bitmap_bytes();
        assert_eq!(
            after, baseline,
            "round {round} left cleared bitmap pages resident: {after} vs {baseline}"
        );
    }
    writer.flush()?;
    Ok(())
}

// F-03: page-index residency and enumeration follow live state ---------------

/// The persisted page index must track the pages a matrix *currently* holds
/// state in, not every page it has ever published.
///
/// Before F-03 the tracking set was append-only: a page whose final bit cleared
/// released its 4 KiB payload but kept its id forever, so resident memory was
/// `Theta(4096 * live + set(historically touched))` and the second term was
/// bounded only by the total page count. Payload accounting alone could not see
/// it, which is why the existing eviction test passed. This measures the index
/// term directly, and the same quantity is charged to
/// `ReadLimits::max_matrix_bitmap_bytes`.
#[test]
fn page_index_residency_returns_to_baseline_across_page_churn() -> varve::Result<()> {
    // One cell in each of eight distinct commit-map pages.
    const PAGE_ORDINALS: [u64; 8] = [
        0, 32_768, 65_536, 98_304, 131_072, 163_840, 196_608, 229_376,
    ];
    // 262_144 cells, i.e. eight commit-map pages.
    const CHURN_SCANS: u64 = 2048;

    let dir = temp_dir("index-churn");
    let path = dir.path().join("matrix.varve");
    let mut writer = spec().create_writer_with_dims(&path, dims(CHURN_SCANS))?;

    MatrixRecoveryReport::reset_matrix_integrity_counters();
    let baseline = MatrixRecoveryReport::matrix_resident_page_index_bytes();
    assert_eq!(baseline, 0, "a fresh matrix already tracks index entries");

    let mut peaks = Vec::new();
    for round in 0..4 {
        for ordinal in PAGE_ORDINALS {
            writer.write_matrix_cell(key(ordinal), &ScalingCell { value: 7 })?;
            writer.commit_matrix_cell::<ScalingCell>(key(ordinal))?;
        }
        peaks.push(MatrixRecoveryReport::matrix_resident_page_index_bytes());
        for ordinal in PAGE_ORDINALS {
            writer.clear_matrix_cell::<ScalingCell>(key(ordinal))?;
        }
        let after = MatrixRecoveryReport::matrix_resident_page_index_bytes();
        assert_eq!(
            after, baseline,
            "round {round} retained page-index entries for pages that are now empty"
        );
    }
    assert!(
        peaks[0] > 0,
        "publishing eight distinct pages tracked no index entry at all"
    );
    assert!(
        peaks.iter().all(|peak| *peak == peaks[0]),
        "page-index residency grew with historical churn: {peaks:?}"
    );
    writer.flush()?;
    Ok(())
}

/// F-01: a whole-category clear must refund **every** counter it releases.
///
/// A cell-category clear drops four resident structures: the commit bitmap's
/// payload pages, the validity bitmap's payload pages, and the persisted
/// page-index tracking of both. Only the commit payload was refunded, so each
/// populate/clear cycle left the validity payload and both index charges
/// standing for memory that had already been freed. Nothing on disk was lost —
/// the failure mode is `Error::LimitExceeded` for residency the matrix no longer
/// holds, after enough cycles.
///
/// The assertion is per cycle, not just at the end: a leak that is refunded
/// late still refuses admissions in between.
#[test]
fn category_clear_refunds_payload_and_page_index_residency_every_cycle() -> varve::Result<()> {
    // One cell in each of eight distinct commit-map pages, so both the commit
    // bitmap and the validity bitmap materialise eight pages and index eight
    // entries per cycle.
    const PAGE_ORDINALS: [u64; 8] = [
        0, 32_768, 65_536, 98_304, 131_072, 163_840, 196_608, 229_376,
    ];
    /// 262_144 cells, i.e. eight commit-map pages.
    const CHURN_SCANS: u64 = 2048;

    let dir = temp_dir("category-clear-refund");
    let path = dir.path().join("matrix.varve");
    let mut writer = spec().create_writer_with_dims(&path, dims(CHURN_SCANS))?;

    // Cycle zero fixes the baseline. What survives a clear is the session
    // write-tracking map, which `clear_matrix_category` deliberately does not
    // touch: this writer really did write those slots, and that stays true.
    // It has no persisted page index, so the index baseline is zero.
    for ordinal in PAGE_ORDINALS {
        writer.write_matrix_cell(key(ordinal), &ScalingCell { value: 5 })?;
        writer.commit_matrix_cell::<ScalingCell>(key(ordinal))?;
    }
    assert_eq!(
        writer.clear_matrix_category(ScalingCell::CATEGORY)?,
        PAGE_ORDINALS.len() as u64
    );
    let payload_baseline = MatrixRecoveryReport::matrix_resident_bitmap_bytes();
    let index_baseline = MatrixRecoveryReport::matrix_resident_page_index_bytes();
    assert_eq!(
        payload_baseline,
        PAGE_ORDINALS.len() as u64 * PAGE_BYTES,
        "residency after a whole-category clear is not the session \
         write-tracking pages alone"
    );
    assert_eq!(
        index_baseline, 0,
        "a cleared category still tracks persisted page-index entries"
    );

    for round in 0..4 {
        for ordinal in PAGE_ORDINALS {
            writer.write_matrix_cell(key(ordinal), &ScalingCell { value: 6 })?;
            writer.commit_matrix_cell::<ScalingCell>(key(ordinal))?;
        }
        let payload_peak = MatrixRecoveryReport::matrix_resident_bitmap_bytes();
        let index_peak = MatrixRecoveryReport::matrix_resident_page_index_bytes();
        assert!(
            payload_peak > payload_baseline,
            "round {round} committed cells without making a page resident \
             ({payload_peak} vs {payload_baseline})"
        );
        assert!(
            index_peak > index_baseline,
            "round {round} published eight pages without tracking an index entry"
        );

        assert_eq!(
            writer.clear_matrix_category(ScalingCell::CATEGORY)?,
            PAGE_ORDINALS.len() as u64
        );
        let payload_after = MatrixRecoveryReport::matrix_resident_bitmap_bytes();
        let index_after = MatrixRecoveryReport::matrix_resident_page_index_bytes();
        assert_eq!(
            payload_after, payload_baseline,
            "round {round} left payload residency charged after a whole-category \
             clear: {payload_after} vs {payload_baseline}"
        );
        assert_eq!(
            index_after, index_baseline,
            "round {round} left page-index residency charged after a \
             whole-category clear: {index_after} vs {index_baseline}"
        );
    }

    // The refunds must not have desynchronised the layout: the category is
    // still usable and still counts what it holds.
    for ordinal in PAGE_ORDINALS {
        writer.commit_matrix_cell::<ScalingCell>(key(ordinal))?;
    }
    assert_eq!(
        writer.clear_matrix_category(ScalingCell::CATEGORY)?,
        PAGE_ORDINALS.len() as u64
    );
    writer.flush()?;
    Ok(())
}

/// F-01, the same contract under a runtime ceiling: repeated populate/clear
/// cycles must not exhaust `max_matrix_bitmap_bytes`.
///
/// This is the failure a caller actually sees. The counters above prove the
/// accounting; this proves the admission decision made from it, on a ceiling
/// sized for a little over one cycle's peak.
#[test]
fn repeated_populate_clear_cycles_do_not_exhaust_the_bitmap_ceiling() -> varve::Result<()> {
    const PAGE_ORDINALS: [u64; 4] = [0, 32_768, 65_536, 98_304];
    const CHURN_SCANS: u64 = 2048;
    // Three bitmaps (commit, validity, session write tracking) of four pages
    // each, plus generous room for page-index tracking. A single cycle fits;
    // four cycles only fit if every clear refunds what it released.
    const CEILING: u64 = 3 * 4 * PAGE_BYTES + 4096;

    let dir = temp_dir("category-clear-ceiling");
    let path = dir.path().join("matrix.varve");
    let bounded =
        spec().with_read_limits(ReadLimits::STANDARD.with_max_matrix_bitmap_bytes(CEILING));
    let mut writer = bounded.create_writer_with_dims(&path, dims(CHURN_SCANS))?;

    for round in 0..4 {
        for ordinal in PAGE_ORDINALS {
            writer
                .write_matrix_cell(key(ordinal), &ScalingCell { value: 4 })
                .unwrap_or_else(|err| panic!("round {round} write refused: {err:?}"));
            writer
                .commit_matrix_cell::<ScalingCell>(key(ordinal))
                .unwrap_or_else(|err| panic!("round {round} commit refused: {err:?}"));
        }
        assert_eq!(
            writer.clear_matrix_category(ScalingCell::CATEGORY)?,
            PAGE_ORDINALS.len() as u64
        );
    }
    writer.flush()?;
    Ok(())
}

/// Reopen must materialise and visit the *live* pages, not every page the file
/// has ever published — including where no allocation map is available, which
/// is exactly the case the persisted index exists to serve.
#[test]
fn reopen_after_churn_visits_only_live_pages() -> varve::Result<()> {
    const CHURN_SCANS: u64 = 2048;
    const HISTORIC: [u64; 7] = [32_768, 65_536, 98_304, 131_072, 163_840, 196_608, 229_376];

    let dir = temp_dir("index-reopen");
    let path = dir.path().join("matrix.varve");
    {
        let mut writer = spec().create_writer_with_dims(&path, dims(CHURN_SCANS))?;
        // One cell that stays committed, in page 0.
        writer.write_matrix_cell(key(0), &ScalingCell { value: 1 })?;
        writer.commit_matrix_cell::<ScalingCell>(key(0))?;
        // Seven more pages that are published and then emptied again.
        for ordinal in HISTORIC {
            writer.write_matrix_cell(key(ordinal), &ScalingCell { value: 2 })?;
            writer.commit_matrix_cell::<ScalingCell>(key(ordinal))?;
            writer.clear_matrix_cell::<ScalingCell>(key(ordinal))?;
        }
        writer.flush()?;
    }

    MatrixRecoveryReport::reset_matrix_integrity_counters();
    let reader = open_without_allocation_map(&path, false)?;
    let visited = MatrixRecoveryReport::matrix_open_bitmap_pages_visited();
    let index_bytes = MatrixRecoveryReport::matrix_resident_page_index_bytes();
    assert!(
        !MatrixRecoveryReport::matrix_open_allocation_map_available(),
        "the allocation map was not actually disabled for this fixture"
    );
    // The commit map contributes its one live page and the validity bitmap its
    // own; the seven historically touched pages contribute nothing.
    assert!(
        visited <= 4,
        "reopen visited {visited} pages for two live pages of state"
    );
    assert!(
        index_bytes <= 4 * 48,
        "reopen materialised {index_bytes} bytes of page-index tracking for two live pages"
    );
    assert!(matches!(
        reader.matrix_cell_status::<ScalingCell>(key(0)),
        Ok(varve::MatrixCellStatus::Committed)
    ));
    for ordinal in HISTORIC {
        assert!(matches!(
            reader.matrix_cell_status::<ScalingCell>(key(ordinal)),
            Ok(varve::MatrixCellStatus::NotCommitted)
        ));
    }
    Ok(())
}

// F-06: damaged page-index entries are reported, never silently truncating ---

/// A damaged page-index entry used to be read as a successful end-of-array, so
/// every *later* published page went unvisited, its cells answered
/// `NotCommitted`, and no finding was produced. With no filesystem allocation
/// map there was no second way to find them: corruption hid data instead of
/// being reported.
///
/// The occupancy header now states how many entries exist, so a zeroed entry
/// inside that prefix is provable damage: it is reported as a fatal finding and
/// the scan continues to the pages after it.
#[test]
fn damaged_page_index_entry_is_reported_and_later_pages_still_load() -> varve::Result<()> {
    const SECOND_PAGE_ORDINAL: u64 = 32_768;
    let dir = temp_dir("index-damage");
    let path = dir.path().join("matrix.varve");
    {
        let mut writer = spec().create_writer_with_dims(&path, dims(LARGE_SCANS))?;
        for ordinal in [0, SECOND_PAGE_ORDINAL] {
            writer.write_matrix_cell(key(ordinal), &ScalingCell { value: 3 })?;
            writer.commit_matrix_cell::<ScalingCell>(key(ordinal))?;
        }
        writer.flush()?;
    }

    let base = page_index_off(&path);
    assert_eq!(
        read_u64_at(&path, base) & ((1u64 << 48) - 1),
        2,
        "fixture did not publish exactly two commit-map pages"
    );
    // Destroy the *first* entry, which is the one an end-of-array scan would
    // have stopped on.
    patch_u64(&path, base + PAGE_INDEX_SLOT_LEN, 0);

    let reader = open_without_allocation_map(&path, true)?;
    let report = reader.matrix_recovery_report();
    assert!(
        report.findings.iter().any(
            |finding| finding.severity == MatrixCorruptionSeverity::Fatal
                && finding.kind == MatrixCorruptionKind::CommitMap
        ),
        "a damaged page-index entry produced no fatal finding: {:?}",
        report.findings
    );
    // And the page named by the entry *after* the damaged one is still found.
    assert!(
        matches!(
            reader.matrix_cell_status::<ScalingCell>(key(SECOND_PAGE_ORDINAL)),
            Ok(varve::MatrixCellStatus::Committed)
        ),
        "the page after the damaged entry was silently truncated away"
    );
    Ok(())
}

/// The occupancy header itself carries redundancy, so a torn or flipped header
/// is damage rather than an authoritative "empty index".
#[test]
fn damaged_page_index_header_is_reported_not_read_as_empty() -> varve::Result<()> {
    let dir = temp_dir("index-header-damage");
    let path = dir.path().join("matrix.varve");
    fill(&path, LARGE_SCANS, 4)?;

    let base = page_index_off(&path);
    // A plausible-looking count with no matching check bits.
    patch_u64(&path, base, 1);

    let reader = open_without_allocation_map(&path, true)?;
    let report = reader.matrix_recovery_report();
    assert!(
        report.findings.iter().any(
            |finding| finding.severity == MatrixCorruptionSeverity::Fatal
                && finding.kind == MatrixCorruptionKind::CommitMap
        ),
        "a damaged page-index header produced no fatal finding: {:?}",
        report.findings
    );
    Ok(())
}

/// A never-written index region is all zeros, which is the encoding for "no
/// entries" and must stay a clean, finding-free open.
#[test]
fn zero_page_index_region_is_an_empty_index_not_damage() -> varve::Result<()> {
    let dir = temp_dir("index-empty");
    let path = dir.path().join("matrix.varve");
    drop(spec().create_writer_with_dims(&path, dims(LARGE_SCANS))?);

    let reader = open_without_allocation_map(&path, false)?;
    assert!(
        reader.matrix_recovery_report().findings.is_empty(),
        "an untouched page index was reported as damaged: {:?}",
        reader.matrix_recovery_report().findings
    );
    Ok(())
}

// F-08: the streaming-zero fallback is detectable ----------------------------

/// Whole-category clear removes a byte range where the platform and filesystem
/// can, and streams zeros where they cannot. The second path is
/// `Theta(bitmap bytes)`, so a caller must be able to detect that it is on it
/// rather than having to infer it from the target triple.
#[test]
fn whole_category_clear_reports_whether_it_streamed_zeros() -> varve::Result<()> {
    let dir = temp_dir("clear-qualification");
    let path = dir.path().join("matrix.varve");
    fill(&path, SMALL_WIDE_SCANS, 8)?;

    let mut writer = spec().open_writer(&path)?;
    let before = MatrixRecoveryReport::matrix_total_zero_range_streamed_bytes();
    assert_eq!(writer.clear_matrix_category(ScalingCell::CATEGORY)?, 8);
    let streamed = MatrixRecoveryReport::matrix_last_zero_range_streamed_bytes();
    let total = MatrixRecoveryReport::matrix_total_zero_range_streamed_bytes();
    if streamed == 0 {
        assert!(
            MatrixRecoveryReport::matrix_sparse_zeroing_supported(),
            "a range was removed on a target with no range-removal call"
        );
    } else {
        // The slow path is allowed, but it must be visible and it must agree
        // with the cumulative byte counter.
        assert!(
            total >= streamed,
            "the streaming fallback ran without being counted"
        );
    }

    // The whole-operation contract, which the last-request counter cannot
    // express: a clear issues several range requests, so the qualification a
    // caller is told to use is the delta of the cumulative counter over the
    // operation. On a target with no range-removal call at all, every one of
    // those requests must have streamed, so the delta cannot be zero.
    let delta = total - before;
    if !MatrixRecoveryReport::matrix_sparse_zeroing_supported() {
        assert!(
            delta > 0,
            "a clear on a target without range removal reported no streamed bytes"
        );
    }
    assert!(
        delta >= streamed,
        "the cumulative counter did not account for the last range request"
    );
    Ok(())
}

/// F-08 accounting completeness: every range a whole-category clear zeroes is
/// accounted for, including the page-digest array.
///
/// The digest array is not zeroed through `zero_range`: the clear punches it
/// directly and falls back to an explicit per-page digest loop. That loop used
/// to record nothing, so a clear whose digest array streamed while its final
/// range was removed reported zero streamed bytes and a caller would have
/// concluded it took the cheap path.
///
/// The assertion that does not depend on which path the host filesystem takes:
/// whatever the clear streamed, the cumulative counter must account for at
/// least as many bytes as the counter of explicitly written clear bytes, which
/// is charged by *both* the `zero_range` fallback and the digest loop. A range
/// zeroed by writing but not recorded breaks this inequality.
#[test]
fn whole_category_clear_accounts_for_every_range_it_zeroes() -> varve::Result<()> {
    let dir = temp_dir("clear-accounting");
    let path = dir.path().join("matrix.varve");
    fill(&path, SMALL_WIDE_SCANS, 8)?;

    let streamed_before = MatrixRecoveryReport::matrix_total_zero_range_streamed_bytes();
    let written_before = MatrixRecoveryReport::matrix_category_clear_bytes_written();

    let mut writer = spec().open_writer(&path)?;
    assert_eq!(writer.clear_matrix_category(ScalingCell::CATEGORY)?, 8);

    let streamed = MatrixRecoveryReport::matrix_total_zero_range_streamed_bytes() - streamed_before;
    let written = MatrixRecoveryReport::matrix_category_clear_bytes_written() - written_before;
    assert!(
        streamed >= written,
        "the clear wrote {written} zero bytes but only accounted for {streamed}: \
         some range was zeroed by writing without recording it"
    );
    if written == 0 {
        assert_eq!(
            streamed, 0,
            "no zero byte was written, so nothing may be reported as streamed"
        );
        assert!(
            MatrixRecoveryReport::matrix_sparse_zeroing_supported(),
            "every range was removed on a target with no range-removal call"
        );
    }
    Ok(())
}

/// The skipped ranges must not become a blind spot: a stray byte written into a
/// page that no commit ever touched allocates that page, so it is still read
/// and still quarantines the category — at a cell count where hole skipping is
/// definitely active.
#[test]
fn stray_bytes_in_a_skipped_region_are_still_detected() -> varve::Result<()> {
    let dir = temp_dir("skipped-region");
    let path = dir.path().join("matrix.varve");
    fill(&path, LARGE_WIDE_SCANS, 1)?;

    // A byte deep inside the commit map, far past every page any write touched
    // and past the first allocation granule.
    patch_byte(&path, commit_map_off(&path) + 512 * 1024 + 9, 0x40);

    let reader = spec().open_reader(&path)?;
    assert!(
        MatrixRecoveryReport::matrix_open_allocation_map_available(),
        "allocation map unavailable: this fixture no longer exercises skipping"
    );
    assert!(matches!(
        reader.matrix_cell_status::<ScalingCell>(key(0)),
        Err(Error::MatrixCommitQuarantined(name)) if name == ScalingCell::CATEGORY
    ));
    assert!(
        reader
            .matrix_recovery_report()
            .findings
            .iter()
            .any(|finding| finding.kind == MatrixCorruptionKind::CommitMap),
        "corruption in a skipped region went unreported"
    );
    Ok(())
}

/// A whole-category clear restores the post-create zero extent, so it must not
/// write `Theta(cell_count / 8)` zero bytes to do it.
#[test]
fn whole_category_clear_cost_does_not_scale_with_cell_count() -> varve::Result<()> {
    let dir = temp_dir("clear-cost");

    let mut measured = Vec::new();
    for (name, scans) in [("small", SMALL_WIDE_SCANS), ("large", LARGE_WIDE_SCANS)] {
        let path = dir.path().join(format!("{name}.varve"));
        fill(&path, scans, 8)?;
        let mut writer = spec().open_writer(&path)?;
        MatrixRecoveryReport::reset_matrix_integrity_counters();
        assert_eq!(writer.clear_matrix_category(ScalingCell::CATEGORY)?, 8);
        measured.push(MatrixRecoveryReport::matrix_category_clear_bytes_written());
        // The cleared category must reopen exactly like a fresh matrix.
        drop(writer);
        let reader = spec().open_reader(&path)?;
        assert!(
            reader
                .matrix_recovery_report()
                .findings
                .iter()
                .all(|finding| finding.kind != MatrixCorruptionKind::CommitMap),
            "cleared category was not restored to the uninitialized encoding"
        );
    }

    let (small, large) = (measured[0], measured[1]);
    assert_eq!(
        small, large,
        "whole-category clear scaled with cell count: {small} vs {large}"
    );
    assert_eq!(
        small, 0,
        "clear wrote {small} zero bytes instead of punching"
    );
    Ok(())
}

// Unchanged correctness -----------------------------------------------------

/// Corruption inside a *published* commit-map page is still detected and the
/// category is still quarantined until whole-category recovery or rebuild.
#[test]
fn published_page_corruption_is_detected_and_rebuild_recovers() -> varve::Result<()> {
    const SCANS: u64 = 2;
    const CELLS: u64 = 8;
    let dir = temp_dir("rebuild");
    let path = dir.path().join("matrix.varve");
    fill(&path, SCANS, CELLS)?;

    patch_byte(&path, commit_map_off(&path), 0x00);

    {
        let reader = spec().open_reader(&path)?;
        assert!(matches!(
            reader.matrix_cell_status::<ScalingCell>(key(0)),
            Err(Error::MatrixCommitQuarantined(name)) if name == ScalingCell::CATEGORY
        ));
        assert!(
            reader
                .matrix_recovery_report()
                .findings
                .iter()
                .any(|finding| finding.kind == MatrixCorruptionKind::CommitMap)
        );
    }

    let mut writer = spec().open_writer(&path)?;
    assert_eq!(
        writer.rebuild_matrix_commit_from_crc::<ScalingCell>()?,
        CELLS
    );
    assert_eq!(
        writer.read_matrix_cell::<ScalingCell>(key(3))?,
        ScalingCell { value: 4 }
    );
    assert!(
        !writer
            .matrix_recovery_report()
            .findings
            .iter()
            .any(|finding| finding.kind == MatrixCorruptionKind::CommitMap)
    );
    Ok(())
}

/// A page that was never written is authenticated too: its digest asserts the
/// page is still zero, so "never written" can never be confused with "written
/// zeros". Both cases are exercised here against the same fixture.
#[test]
fn never_written_and_zero_written_pages_are_distinguishable() -> varve::Result<()> {
    let dir = temp_dir("uninitialized");

    // A published page whose bits are all cleared again must reopen cleanly.
    let cleared = dir.path().join("cleared.varve");
    {
        let mut writer = spec().create_writer_with_dims(&cleared, dims(LARGE_SCANS))?;
        writer.write_matrix_cell(key(0), &ScalingCell { value: 5 })?;
        writer.commit_matrix_cell::<ScalingCell>(key(0))?;
        writer.clear_matrix_cell::<ScalingCell>(key(0))?;
        writer.flush()?;
    }
    let reader = spec().open_reader(&cleared)?;
    assert!(
        reader
            .matrix_recovery_report()
            .findings
            .iter()
            .all(|finding| finding.kind != MatrixCorruptionKind::CommitMap),
        "an all-zero published page was mistaken for corruption"
    );
    drop(reader);

    // A never-written page that is not zero on disk is corruption.
    let hostile = dir.path().join("hostile.varve");
    {
        let mut writer = spec().create_writer_with_dims(&hostile, dims(LARGE_SCANS))?;
        writer.write_matrix_cell(key(0), &ScalingCell { value: 5 })?;
        writer.commit_matrix_cell::<ScalingCell>(key(0))?;
        writer.flush()?;
    }
    // Byte inside the fourth commit-map page, which no write has ever touched.
    patch_byte(
        &hostile,
        commit_map_off(&hostile) + 3 * PAGE_BYTES + 17,
        0x40,
    );
    let reader = spec().open_reader(&hostile)?;
    assert!(matches!(
        reader.matrix_cell_status::<ScalingCell>(key(0)),
        Err(Error::MatrixCommitQuarantined(name)) if name == ScalingCell::CATEGORY
    ));
    Ok(())
}

/// A slot payload that no longer matches its recorded checksum is still
/// rejected on read.
#[test]
fn slot_corruption_still_rejects_a_committed_cell_read() -> varve::Result<()> {
    let dir = temp_dir("slot-crc");
    let path = dir.path().join("matrix.varve");
    fill(&path, 2, 4)?;

    let slot_region_off = header_u64(&path, 8);
    patch_byte(&path, slot_region_off, 0xEE);

    let reader = spec().open_reader(&path)?;
    assert!(matches!(
        reader.read_matrix_cell::<ScalingCell>(key(0)),
        Err(Error::MatrixChecksumMismatch { .. })
    ));
    Ok(())
}

/// A matrix written by the previous layout version is stale-regenerable and
/// must be rejected with a typed version error rather than reinterpreted.
#[test]
fn previous_layout_version_artifact_is_rejected_typed() -> varve::Result<()> {
    let dir = temp_dir("stale-version");
    let path = dir.path().join("matrix.varve");
    fill(&path, 2, 4)?;

    {
        let mut file = OpenOptions::new().write(true).open(&path)?;
        file.seek(SeekFrom::Start(vmat_header_offset() + 4))?;
        file.write_all(&2u16.to_le_bytes())?;
    }

    assert!(matches!(
        spec().open_readonly(&path),
        Err(Error::FormatVersionMismatch {
            expected,
            actual: 2
        }) if expected == VMAT_VERSION
    ));
    Ok(())
}

/// The version 3 page index has no occupancy header and a different entry base,
/// so a v3 artifact is stale-regenerable rather than reinterpretable.
#[test]
fn version_three_page_index_artifact_is_rejected_typed() -> varve::Result<()> {
    let dir = temp_dir("stale-v3");
    let path = dir.path().join("matrix.varve");
    fill(&path, 2, 4)?;

    {
        let mut file = OpenOptions::new().write(true).open(&path)?;
        file.seek(SeekFrom::Start(vmat_header_offset() + 4))?;
        file.write_all(&3u16.to_le_bytes())?;
    }

    assert!(matches!(
        spec().open_readonly(&path),
        Err(Error::FormatVersionMismatch {
            expected,
            actual: 3
        }) if expected == VMAT_VERSION
    ));
    Ok(())
}

// F-02: interrupted whole-map page-index republication ----------------------

/// 66_560 cells: a commit map of 8_320 bytes, i.e. exactly three 4 KiB pages,
/// so a rebuild has three page-index entries to republish. Deliberately the
/// smallest matrix that spans three pages: a CRC rebuild reads every cell, so a
/// wider fixture buys nothing and costs a full extra scan per interruption.
const REBUILD_SCANS: u64 = 520;

/// Environment handshake for the child half of the process-interruption test.
const REBUILD_CHILD_ENV: &str = "VARVE_MATRIX_REBUILD_CHILD";
const REBUILD_STAGE_ENV: &str = "VARVE_MATRIX_REBUILD_STAGE";
const REBUILD_PATH_ENV: &str = "VARVE_MATRIX_REBUILD_PATH";

/// Creates a matrix holding one committed cell on each named commit-map page,
/// so a rebuild has several page-index entries to republish.
fn fill_pages(path: &Path, scans: u64, pages: &[u64]) -> varve::Result<()> {
    let mut writer = spec().create_writer_with_dims(path, dims(scans))?;
    for page in pages {
        let ordinal = page * PAGE_BYTES * 8;
        writer.write_matrix_cell(key(ordinal), &ScalingCell { value: 7 })?;
        writer.commit_matrix_cell::<ScalingCell>(key(ordinal))?;
    }
    writer.flush()?;
    Ok(())
}

/// The first byte of the commit map is nonzero once ordinal zero is committed,
/// so clearing it makes the page disagree with its digest and quarantines the
/// category: the state whose only recovery is a whole-map rebuild.
fn quarantine_commit_map(path: &Path) {
    patch_byte(path, commit_map_off(path), 0x00);
}

/// Asserts that reopening `path` refuses the persisted page index rather than
/// reading a half-republished one as the authoritative published set.
///
/// Both halves matter. Forensic access must *report* the damage as fatal and
/// name the rebuild that repairs it; ordinary access must refuse to answer at
/// all. The failure this guards against is neither of those: a valid short
/// occupancy count that answers `NotCommitted` for committed cells in silence.
fn assert_page_index_fails_closed(path: &Path, context: &str) -> varve::Result<()> {
    {
        let reader = open_without_allocation_map(path, true)?;
        let report = reader.matrix_recovery_report();
        assert!(
            report
                .findings
                .iter()
                .any(|finding| finding.kind == MatrixCorruptionKind::CommitMap
                    && finding.severity == MatrixCorruptionSeverity::Fatal),
            "{context}: an interrupted page-index republication produced no fatal finding: {:?}",
            report.findings
        );
        assert!(
            report
                .recommended_actions
                .contains(&MatrixRecoveryAction::RebuildCommitMap { category: None }),
            "{context}: fatal page-index damage recommended no rebuild: {:?}",
            report.recommended_actions
        );
    }
    let reader = open_without_allocation_map(path, false)?;
    assert!(
        matches!(
            reader.matrix_cell_status::<ScalingCell>(key(0)),
            Err(Error::MatrixFatalCorruption)
        ),
        "{context}: cell access was answered from a page index known to be incomplete"
    );
    Ok(())
}

/// F-02: a rebuild whose page-index *entry* write fails after the index has
/// been emptied must not leave a valid short index behind.
///
/// The allocation map is forced unavailable throughout the verification,
/// because that is the configuration in which the persisted index is the only
/// way to find a published page and a short index therefore hides committed
/// cells with no finding at all.
#[test]
fn an_interrupted_rebuild_entry_write_fails_closed_and_rebuilds() -> varve::Result<()> {
    let dir = temp_dir("rebuild-entry-fault");
    let path = dir.path().join("matrix.varve");
    fill_pages(&path, REBUILD_SCANS, &[0, 1, 2])?;
    quarantine_commit_map(&path);

    {
        let mut writer = spec().open_writer(&path)?;
        // The second entry write: the index region has been cleared and only
        // partly refilled.
        MatrixRecoveryReport::inject_matrix_page_index_entry_write_failure(2);
        let outcome = writer.rebuild_matrix_commit_from_crc::<ScalingCell>();
        MatrixRecoveryReport::inject_matrix_page_index_entry_write_failure(0);
        assert!(
            outcome.is_err(),
            "the injected page-index entry write failure did not surface"
        );
        // The same session must refuse to keep mutating a map whose persisted
        // index no longer matches its in-memory mirror.
        assert!(matches!(
            writer.matrix_cell_status::<ScalingCell>(key(0)),
            Err(Error::MatrixFatalCorruption)
        ));
    }

    assert_page_index_fails_closed(&path, "entry-write interruption")?;

    // Recovery: reopen with forensic access and run the rebuild again.
    {
        let mut writer = spec().with_matrix_fatal_forensics().open_writer(&path)?;
        assert_eq!(writer.rebuild_matrix_commit_from_crc::<ScalingCell>()?, 3);
        writer.flush()?;
    }
    let reader = open_without_allocation_map(&path, false)?;
    for page in [0u64, 1, 2] {
        assert_eq!(
            reader.matrix_cell_status::<ScalingCell>(key(page * PAGE_BYTES * 8))?,
            MatrixCellStatus::Committed,
            "page {page} did not survive the interrupted rebuild"
        );
    }
    Ok(())
}

/// F-02: the same contract at the other end of the republication, the final
/// occupancy-count write, which is the publication itself.
#[test]
fn an_interrupted_rebuild_header_write_fails_closed() -> varve::Result<()> {
    let dir = temp_dir("rebuild-header-fault");
    let path = dir.path().join("matrix.varve");
    fill_pages(&path, REBUILD_SCANS, &[0, 1, 2])?;
    quarantine_commit_map(&path);

    {
        let mut writer = spec().open_writer(&path)?;
        // Header write 1 is the rebuild marker, write 2 the publication.
        MatrixRecoveryReport::inject_matrix_page_index_header_write_failure(2);
        let outcome = writer.rebuild_matrix_commit_from_crc::<ScalingCell>();
        MatrixRecoveryReport::inject_matrix_page_index_header_write_failure(0);
        assert!(outcome.is_err(), "the injected header write did not fail");
    }

    assert_page_index_fails_closed(&path, "header-write interruption")?;
    Ok(())
}

/// F-02: failing the *marker* write destroys nothing, so the complete old index
/// must still be there and the reopen must be exactly what it was before.
///
/// This is the other admissible outcome of an interrupted rebuild: either the
/// complete old index, or a fail-closed one. Never a silently short one.
#[test]
fn a_rebuild_that_never_started_leaves_the_old_page_index_intact() -> varve::Result<()> {
    let dir = temp_dir("rebuild-marker-fault");
    let path = dir.path().join("matrix.varve");
    fill_pages(&path, REBUILD_SCANS, &[0, 1, 2])?;
    let index_base = page_index_off(&path);
    let before: Vec<u64> = (0..4)
        .map(|slot| read_u64_at(&path, index_base + slot * PAGE_INDEX_SLOT_LEN))
        .collect();
    quarantine_commit_map(&path);

    {
        let mut writer = spec().open_writer(&path)?;
        MatrixRecoveryReport::inject_matrix_page_index_header_write_failure(1);
        let outcome = writer.rebuild_matrix_commit_from_crc::<ScalingCell>();
        MatrixRecoveryReport::inject_matrix_page_index_header_write_failure(0);
        assert!(outcome.is_err(), "the injected marker write did not fail");
        // Nothing was destroyed, so the session is not poisoned.
        assert!(matches!(
            writer.matrix_cell_status::<ScalingCell>(key(0)),
            Err(Error::MatrixCommitQuarantined(name)) if name == ScalingCell::CATEGORY
        ));
    }

    let after: Vec<u64> = (0..4)
        .map(|slot| read_u64_at(&path, index_base + slot * PAGE_INDEX_SLOT_LEN))
        .collect();
    assert_eq!(
        before, after,
        "a rebuild that failed before its marker still altered the page index"
    );
    let reader = open_without_allocation_map(&path, true)?;
    assert!(
        !reader
            .matrix_recovery_report()
            .findings
            .iter()
            .any(|finding| finding.kind == MatrixCorruptionKind::CommitMap
                && finding.severity == MatrixCorruptionSeverity::Fatal),
        "a rebuild that never started was reported as an interrupted one"
    );
    Ok(())
}

/// Child half of [`an_interrupted_rebuild_process_fails_closed_at_every_stage`].
///
/// Ignored so it never runs as part of an ordinary suite; the parent invokes it
/// by exact name with the handshake environment set.
#[test]
#[ignore = "child process driven by the rebuild interruption test"]
fn matrix_rebuild_interruption_child() {
    if std::env::var(REBUILD_CHILD_ENV).is_err() {
        return;
    }
    let path =
        std::path::PathBuf::from(std::env::var(REBUILD_PATH_ENV).expect("child fixture path"));
    let stage = std::env::var(REBUILD_STAGE_ENV)
        .expect("child stage")
        .parse::<u64>()
        .expect("child stage is a number");
    let mut writer = spec().open_writer(&path).expect("child opens the fixture");
    MatrixRecoveryReport::abort_process_at_matrix_rebuild_stage(stage);
    let _ = writer.rebuild_matrix_commit_from_crc::<ScalingCell>();
    // Unreachable: the armed stage aborts inside the republication.
    std::process::exit(9);
}

fn run_rebuild_interruption_child(path: &Path, stage: u64) -> std::process::ExitStatus {
    std::process::Command::new(std::env::current_exe().expect("locate the test executable"))
        .args(["--ignored", "--exact", "matrix_rebuild_interruption_child"])
        .env(REBUILD_CHILD_ENV, "1")
        .env(REBUILD_PATH_ENV, path)
        .env(REBUILD_STAGE_ENV, stage.to_string())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .expect("run the rebuild interruption child")
}

/// F-02, process level: a rebuild killed at any point between marking the
/// persisted page index and publishing its replacement must fail closed.
///
/// A returned error also unwinds in-memory state, so only killing the process
/// reproduces what a power loss leaves on disk. Every stage is checked with the
/// allocation map forced unavailable, which is the configuration in which the
/// persisted index is the sole witness of a published page.
#[test]
fn an_interrupted_rebuild_process_fails_closed_at_every_stage() -> varve::Result<()> {
    for stage in 1u64..=4 {
        let dir = temp_dir(&format!("rebuild-abort-{stage}"));
        let path = dir.path().join("matrix.varve");
        fill_pages(&path, REBUILD_SCANS, &[0, 1, 2])?;
        quarantine_commit_map(&path);

        let status = run_rebuild_interruption_child(&path, stage);
        assert!(
            !status.success(),
            "stage {stage} did not interrupt the rebuild: {status}"
        );

        assert_page_index_fails_closed(&path, &format!("process abort at stage {stage}"))?;
    }
    Ok(())
}

// F-03: allocation failure can no longer follow persistence -----------------

/// F-03: a first-touch bitmap page allocation failure must be refused *before*
/// anything reaches the file, so disk and memory can never disagree.
///
/// The failing allocation used to run after the page-index entry, the page
/// digest and the bitmap byte were all durable. Disk then held the new bit
/// while memory held the old byte, the writer was not poisoned, and the next
/// mutation of the same byte derived its value and its checksum from stale
/// memory, silently removing the committed bit behind a consistent digest.
///
/// Committing a cell on a fresh page performs two first-touch allocations, in
/// order: the block validity bitmap and then the commit bitmap. Both are
/// exercised, because both used to install memory after their own bitmap byte
/// was durable (RULE B).
///
/// The assertions are exactly the reported sequence: the failure, a reopen
/// proving disk agrees with memory, and a same-byte mutation followed by a
/// final reopen proving no commit was removed.
#[test]
fn a_failed_page_allocation_cannot_leave_a_bit_on_disk() -> varve::Result<()> {
    for (label, attempt) in [("validity bitmap", 1u64), ("commit bitmap", 2)] {
        assert_failed_allocation_persists_nothing(label, attempt)?;
    }
    Ok(())
}

fn assert_failed_allocation_persists_nothing(label: &str, attempt: u64) -> varve::Result<()> {
    const FIRST: u64 = 0;
    const FAILING: u64 = PAGE_BYTES * 8;
    const SAME_BYTE: u64 = FAILING + 1;

    let dir = temp_dir(&format!("post-persistence-alloc-{attempt}"));
    let path = dir.path().join("matrix.varve");
    {
        let mut writer = spec().create_writer_with_dims(&path, dims(REBUILD_SCANS))?;
        writer.write_matrix_cell(key(FIRST), &ScalingCell { value: 1 })?;
        writer.commit_matrix_cell::<ScalingCell>(key(FIRST))?;

        // The payload write materialises the session write-tracking page for
        // this ordinal, so it happens before the countdown is armed and only the
        // commit path is under test.
        writer.write_matrix_cell(key(FAILING), &ScalingCell { value: 2 })?;
        MatrixRecoveryReport::inject_matrix_bitmap_page_allocation_failure(attempt);
        let outcome = writer.commit_matrix_cell::<ScalingCell>(key(FAILING));
        MatrixRecoveryReport::inject_matrix_bitmap_page_allocation_failure(0);
        assert!(
            matches!(outcome, Err(Error::AllocationFailed { .. })),
            "{label}: expected the injected page allocation failure, got {outcome:?}"
        );
        // Memory must still say the cell is uncommitted.
        assert_eq!(
            writer.matrix_cell_status::<ScalingCell>(key(FAILING))?,
            MatrixCellStatus::NotCommitted,
            "{label}: memory recorded a commit the mutation refused"
        );
        writer.flush()?;
    }

    // Disk must agree with memory: nothing was persisted for the failed page.
    let map_byte = FAILING / 8;
    assert_eq!(
        read_u64_at(&path, commit_map_off(&path) + map_byte) & 0xFF,
        0,
        "{label}: a failed page allocation left a commit bit on disk"
    );
    {
        let reader = open_without_allocation_map(&path, false)?;
        assert_eq!(
            reader.matrix_cell_status::<ScalingCell>(key(FIRST))?,
            MatrixCellStatus::Committed
        );
        assert_eq!(
            reader.matrix_cell_status::<ScalingCell>(key(FAILING))?,
            MatrixCellStatus::NotCommitted,
            "{label}: disk held a commit bit the writer had refused"
        );
    }

    // A later mutation of the *same byte* must not be derived from state that
    // disagrees with the file.
    {
        let mut writer = spec().open_writer(&path)?;
        writer.write_matrix_cell(key(SAME_BYTE), &ScalingCell { value: 3 })?;
        writer.commit_matrix_cell::<ScalingCell>(key(SAME_BYTE))?;
        writer.flush()?;
    }

    let reader = open_without_allocation_map(&path, true)?;
    assert!(
        !reader
            .matrix_recovery_report()
            .findings
            .iter()
            .any(|finding| finding.severity != MatrixCorruptionSeverity::Advisory),
        "{label}: the same-byte mutation left the page disagreeing with its digest: {:?}",
        reader.matrix_recovery_report().findings
    );
    assert_eq!(
        reader.matrix_cell_status::<ScalingCell>(key(FIRST))?,
        MatrixCellStatus::Committed,
        "{label}: an unrelated committed cell was removed"
    );
    assert_eq!(
        reader.matrix_cell_status::<ScalingCell>(key(SAME_BYTE))?,
        MatrixCellStatus::Committed
    );
    Ok(())
}

// F-03: page-index mutation is prepared before it is persisted ---------------

/// Cell count giving the commit map exactly three 4 KiB pages, which is the
/// smallest width at which a compaction can reorder the *counted prefix* of the
/// persisted array. `768 * 128 = 98_304` cells, i.e. `12_288` bitmap bytes.
const THREE_PAGE_SCANS: u64 = 768;
/// Ordinals landing in commit-map pages 0, 1 and 2 respectively.
const PAGE_ORDINALS: [u64; 3] = [0, 32_768, 65_536];

/// Reads the persisted page index's occupancy count and its first `entries`
/// entries, as raw `page + 1` values.
fn page_index_state(path: &Path, entries: u64) -> (u64, Vec<u64>) {
    let base = page_index_off(path);
    let header = read_u64_at(path, base) & ((1u64 << 48) - 1);
    let values = (0..entries)
        .map(|slot| read_u64_at(path, base + (slot + 1) * PAGE_INDEX_SLOT_LEN))
        .collect();
    (header, values)
}

/// F-03. Page-index compaction used to write the new sorted entry order to disk
/// and only then perform the fallible `try_reserve` for the in-memory slot map
/// that mirrors it. A typed `AllocationFailed` there returned an `Err` from a
/// writer that ordinary matrix mutation handling does **not** poison — it
/// poisons only `Error::Io` — so the session continued with a slot map that no
/// longer described the array on disk, and the next removal overwrote a live
/// page's only entry.
///
/// The fixture starts from the state that makes the divergence lethal: a
/// duplicate-entry array left by a crash-interrupted removal, `[2, 1, 2]`, which
/// is also full, so the next publication has to compact. Compaction sorts that
/// to `[1, 2]`, moving page 1 into slot 0 — a slot the stale mirror still
/// believes holds page 2. Releasing page 2 through the stale mirror then wrote
/// page 2 over slot 0 and shortened the count to two, and the only entry naming
/// page 1 was gone. With no allocation map there is no second way to find it.
///
/// Every fallible step of a page-index mutation is now a precondition of its
/// first disk byte, so the refusal happens with the array byte-for-byte
/// untouched and the mirror still describing it exactly.
#[test]
fn a_refused_page_index_compaction_leaves_disk_and_mirror_in_the_same_order() -> varve::Result<()> {
    let dir = temp_dir("index-compaction-refusal");
    let path = dir.path().join("matrix.varve");

    // (1) Publish one page each into commit-map pages 0, 1 and 2. The array is
    // then [0, 1, 2] with a count of three, which is its full capacity.
    {
        let mut writer = spec().create_writer_with_dims(&path, dims(THREE_PAGE_SCANS))?;
        for ordinal in PAGE_ORDINALS {
            writer.write_matrix_cell(key(ordinal), &ScalingCell { value: 7 })?;
            writer.commit_matrix_cell::<ScalingCell>(key(ordinal))?;
        }
        // (2) Empty page 0 with the removal's *header* write failing. The swap
        // lands and the shorter count does not, which is exactly what a crash
        // between the two leaves: entries [2, 1, 2] under a count of three.
        MatrixRecoveryReport::inject_matrix_page_index_header_write_failure(1);
        writer.clear_matrix_cell::<ScalingCell>(key(PAGE_ORDINALS[0]))?;
        MatrixRecoveryReport::inject_matrix_page_index_header_write_failure(0);
        writer.flush()?;
    }
    let duplicate_state = page_index_state(&path, 3);
    assert_eq!(
        duplicate_state,
        (3, vec![3, 2, 3]),
        "the fixture did not produce the interrupted-removal duplicate state"
    );

    // (3) Republishing page 0 has to compact first. Fail the mirror reservation
    // that compaction needs.
    {
        let mut writer = spec().open_writer(&path)?;
        writer.write_matrix_cell(key(PAGE_ORDINALS[0]), &ScalingCell { value: 9 })?;
        // Committing publishes the validity bit before the commit bit, and the
        // validity bitmap keeps its own page index, so the compaction this test
        // is aiming at is the *second* mirror reservation of the call.
        MatrixRecoveryReport::inject_matrix_page_index_mirror_reservation_failure(2);
        let refused = writer.commit_matrix_cell::<ScalingCell>(key(PAGE_ORDINALS[0]));
        MatrixRecoveryReport::inject_matrix_page_index_mirror_reservation_failure(0);
        assert!(
            matches!(refused, Err(Error::AllocationFailed { .. })),
            "the injected mirror reservation failure did not surface as a typed \
             allocation failure: {refused:?}"
        );
        writer.flush()?;
        assert_eq!(
            page_index_state(&path, 3),
            duplicate_state,
            "a refused page-index mutation rewrote the persisted array"
        );

        // (4) The divergence only becomes data loss at the *next* removal, and
        // it has to happen in the *same* session: a reopen would rebuild the
        // mirror from disk and hide the disagreement. Emptying page 2 releases
        // its entry through whatever slot positions the mirror believes in.
        writer.clear_matrix_cell::<ScalingCell>(key(PAGE_ORDINALS[2]))?;
        writer.flush()?;
    }
    assert_eq!(
        page_index_state(&path, 2).1[1],
        2,
        "a removal trusting a stale mirror overwrote the live page entry"
    );

    // (5) Reopen with the persisted index as the only way to find a page.
    let reader = open_without_allocation_map(&path, false)?;
    assert!(
        !MatrixRecoveryReport::matrix_open_allocation_map_available(),
        "the allocation map was not actually disabled for this fixture"
    );
    assert!(
        !reader
            .matrix_recovery_report()
            .findings
            .iter()
            .any(|finding| finding.severity == MatrixCorruptionSeverity::Fatal),
        "the surviving index was reported as fatally damaged: {:?}",
        reader.matrix_recovery_report().findings
    );
    assert_eq!(
        reader.matrix_cell_status::<ScalingCell>(key(PAGE_ORDINALS[1]))?,
        MatrixCellStatus::Committed,
        "the live page that compaction moved was lost from the persisted index"
    );
    assert_eq!(
        reader.matrix_cell_status::<ScalingCell>(key(PAGE_ORDINALS[0]))?,
        MatrixCellStatus::NotCommitted,
        "the refused commit was published anyway"
    );
    assert_eq!(
        reader.matrix_cell_status::<ScalingCell>(key(PAGE_ORDINALS[2]))?,
        MatrixCellStatus::NotCommitted,
        "the cleared cell is still committed"
    );
    Ok(())
}

// F-04: CRC rebuild refuses incomplete validity evidence ---------------------

/// Base of a block's CRC-validity page-index array.
///
/// The page-index region holds one array per commit category, in category
/// order, then one per matrix block. This format has a single category and a
/// single block, so the block's array starts one whole commit array in.
fn validity_page_index_off(path: &Path, scans: u64) -> u64 {
    let bitmap_bytes = scans * CHANNELS / 8;
    let pages = bitmap_bytes.div_ceil(PAGE_BYTES);
    // One occupancy header slot plus one entry per page.
    page_index_off(path) + (pages + 1) * PAGE_INDEX_SLOT_LEN
}

/// F-04. CRC commit-map rebuild derives every bit from the block's CRC-validity
/// bitmap: a cell is committed only where the validity bit says the stored CRC
/// is meaningful *and* the recomputed CRC matches. A damaged validity page index
/// is reported as a fatal finding, but the pages it omits simply stay absent
/// from the loaded bitmap, and every cell in them then reads as "not
/// meaningful". Default open is fail-closed on the finding; explicit forensic
/// mode removes that gate, and rebuild then published those absences as
/// `NotCommitted` — erasing the previous commit view using evidence the file
/// never supplied.
///
/// Completeness is now tracked per validity map and rebuild refuses when it is
/// missing. Discarding unverifiable visibility on purpose has to be a different,
/// explicitly named operation; it is not what recovery does by default.
#[test]
fn crc_rebuild_refuses_an_incompletely_loaded_validity_index() -> varve::Result<()> {
    const FIRST: u64 = 0;
    const SECOND: u64 = 32_768;
    let dir = temp_dir("validity-evidence");
    let path = dir.path().join("matrix.varve");
    {
        let mut writer = spec().create_writer_with_dims(&path, dims(LARGE_SCANS))?;
        for ordinal in [FIRST, SECOND] {
            writer.write_matrix_cell(key(ordinal), &ScalingCell { value: 5 })?;
            writer.commit_matrix_cell::<ScalingCell>(key(ordinal))?;
        }
        writer.flush()?;
    }

    let validity_base = validity_page_index_off(&path, LARGE_SCANS);
    assert_eq!(
        read_u64_at(&path, validity_base) & ((1u64 << 48) - 1),
        2,
        "the fixture did not publish exactly two validity-bitmap pages"
    );
    // Damage the entry naming the *second* validity page. Its cells' validity
    // bits are then unreadable, not false.
    patch_u64(&path, validity_base + 2 * PAGE_INDEX_SLOT_LEN, 0);

    MatrixRecoveryReport::force_matrix_allocation_map_unavailable(true);
    let opened = spec().with_matrix_fatal_forensics().open_writer(&path);
    MatrixRecoveryReport::force_matrix_allocation_map_unavailable(false);
    let mut writer = opened?;
    assert!(
        writer
            .matrix_recovery_report()
            .findings
            .iter()
            .any(|finding| finding.severity == MatrixCorruptionSeverity::Fatal),
        "the damaged validity page index produced no fatal finding"
    );

    let refused = writer.rebuild_matrix_commit_from_crc::<ScalingCell>();
    assert!(
        matches!(refused, Err(Error::MatrixFatalCorruption)),
        "rebuild published a commit map derived from evidence it never read: {refused:?}"
    );

    // Round 14, the enforcement half. The round-12 fix was a tracked boolean
    // plus one added check, so the next function was free to skip it. The
    // bitmap now lives behind `CrcValidEvidence`, whose only bit-returning
    // operation demands the completeness witness, so the refusal is a property
    // of the *type* and the three assertions below are its observable edges.
    //
    // (1) The refusal is not consumed by being reported once. A caller that
    //     retries — the natural reaction to a fatal finding in forensic mode —
    //     is refused again rather than succeeding on the second attempt.
    let retried = writer.rebuild_matrix_commit_from_crc::<ScalingCell>();
    assert!(
        matches!(retried, Err(Error::MatrixFatalCorruption)),
        "a retried rebuild published from the same incomplete evidence: {retried:?}"
    );

    // (2) Writing cells does not launder incomplete evidence into complete
    //     evidence. A cell write sets a validity bit and publishes a
    //     page-index entry, so afterwards the *persisted* index names more
    //     pages than the loaded map did — but the loaded map is still a subset
    //     of what the file once held, and nothing has re-enumerated it. The
    //     completeness fact travels with the bitmap and stays false for the
    //     life of the session, so rebuild keeps refusing.
    writer.write_matrix_cell(key(FIRST), &ScalingCell { value: 9 })?;
    writer.commit_matrix_cell::<ScalingCell>(key(FIRST))?;
    let after_write = writer.rebuild_matrix_commit_from_crc::<ScalingCell>();
    assert!(
        matches!(after_write, Err(Error::MatrixFatalCorruption)),
        "a cell write turned incomplete validity evidence into a licence to \
         republish the commit map: {after_write:?}"
    );
    writer.flush()?;
    drop(writer);

    // (3) The other consumer of the validity bitmap still fails *closed*, and
    //     does so without the completeness witness — which is why it is allowed
    //     to read a bit at all. `verify_cell_crc` turns an absent validity bit
    //     into a typed checksum refusal; it can never turn one into published
    //     state, because the evidence type hands it no bit to publish.
    let reader = open_without_allocation_map(&path, true)?;
    let unreadable = reader.read_matrix_cell::<ScalingCell>(key(SECOND));
    assert!(
        matches!(unreadable, Err(Error::MatrixChecksumMismatch { .. })),
        "a cell whose validity page was never loaded was read as if its stored \
         checksum had been established: {unreadable:?}"
    );
    assert_eq!(
        reader.read_matrix_cell::<ScalingCell>(key(FIRST))?,
        ScalingCell { value: 9 },
        "the fail-closed reader refused a cell whose validity page *was* loaded"
    );

    // The previous commit view is untouched: both cells are still committed.
    for ordinal in [FIRST, SECOND] {
        assert_eq!(
            reader.matrix_cell_status::<ScalingCell>(key(ordinal))?,
            MatrixCellStatus::Committed,
            "rebuild refusal still erased the commit bit for ordinal {ordinal}"
        );
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// The matrix access model: measured criteria (A) and (B).
//
// These tests are not new contracts so much as *instruments*. The two criteria
// below were asserted in earlier rounds but never reported as numbers, and the
// recurring failure of this repo has been closing a finding while the actual
// requirement stayed unmet. Each of these prints the number it measured so a
// reviewer reads the value rather than a boolean.
// ---------------------------------------------------------------------------

/// Criterion (A): **open is O(1) in file size**, and where the remaining cost of
/// the default open goes.
///
/// The criterion asks for the bytes read and pages visited at open, for two
/// matrices differing 4x in cell count with identical live state: they must be
/// "equal and small". Two independent halves hide behind that phrase and this
/// test separates them, because the two answers have different causes.
///
/// **Equal - holds.** The 4x cell-count ratio does not appear in either number.
/// That is what the paged representation of round 3 bought.
///
/// **Small - holds exactly where verification is not asked for.** Both fixtures
/// hold 64 committed cells, living in a *single* commit-map page. The default
/// open still reads tens of KiB over tens of pages of each map, and this test's
/// predecessor recorded that as an unmet criterion. Its diagnosis was right and
/// has become the design: the cost is the second term of the verification
/// candidate set - the persisted page index *union the pages the filesystem
/// allocation map reports as written* - and a filesystem allocates in runs, so
/// one written 4 KiB page inside a 128 KiB run puts the whole run into the set.
/// That term exists to catch stray bytes written into a page the index does not
/// name (`stray_bytes_in_a_skipped_region_are_still_detected`), so it is an
/// *integrity scan*, not something a read needs - and since 0.5.0 it is not part
/// of residency at all. It is `MatrixMetadataVerification::AtOpen`, it retains
/// one 4096-byte buffer, and declaring `OnDemand` removes it from open.
///
/// So the third measurement below is the same open with `OnDemand`: same file,
/// same residency, no verification pass. The gap between it and the first two is
/// the price of detecting commit-map damage at open, in bytes, printed rather
/// than described.
///
/// Why this had to be measured rather than trusted: the round-3 test
/// `open_bitmap_bytes_read_do_not_scale_with_cell_count` passes, and it asserts
/// `large <= small + allowance` - a relation a 128 KiB *constant* satisfies
/// perfectly. A ratio assertion cannot see an absolute cost.
#[test]
fn measured_open_cost_is_scale_free_and_its_size_is_verification() -> varve::Result<()> {
    const CELLS: u64 = 64;
    let dir = temp_dir("measure-open-cost");

    let mut rows = Vec::new();
    for (name, scans) in [("small", SMALL_WIDE_SCANS), ("large", LARGE_WIDE_SCANS)] {
        let path = dir.path().join(format!("{name}.varve"));
        fill(&path, scans, CELLS)?;
        MatrixRecoveryReport::reset_matrix_integrity_counters();
        drop(spec().open_readonly(&path)?);
        rows.push((
            name,
            scans * CHANNELS,
            MatrixRecoveryReport::matrix_open_bitmap_bytes_read(),
            MatrixRecoveryReport::matrix_open_bitmap_pages_visited(),
        ));
    }

    for (name, cells, bytes, pages) in &rows {
        println!("(A) {name}: cells={cells} open_bytes_read={bytes} open_pages_visited={pages}");
    }
    let (_, small_cells, small_bytes, small_pages) = rows[0];
    let (_, large_cells, large_bytes, large_pages) = rows[1];
    assert_eq!(large_cells / small_cells, 4, "fixtures must differ 4x");

    // The half that holds: no 4x ratio in either number. Stated as a ratio
    // bound rather than exact equality because the visit set includes whole
    // filesystem allocation runs, whose boundaries the test does not control.
    assert!(
        large_pages * 2 < small_pages * 4 && large_bytes * 2 < small_bytes * 4,
        "(A) open cost picked up the 4x cell-count ratio: {small_pages}p/{small_bytes}B \
         vs {large_pages}p/{large_bytes}B"
    );

    // What the default's remaining cost is: the live state is one page and the
    // verification pass visits tens of pages. Both bounds are deliberately
    // generous; the point is that the gap is two orders of magnitude.
    let live_pages = 1u64;
    assert!(
        large_pages >= 4 * live_pages && large_bytes >= 16 * PAGE_BYTES,
        "(A) the default open stopped scanning ({large_pages} pages / {large_bytes} bytes \
         for {live_pages} live page); stray bytes in a page nothing published would then \
         never be examined - see `stray_bytes_in_a_skipped_region_are_still_detected`."
    );

    // And where that cost lives. Same fixture, same residency, verification moved
    // off the open: what is left is the persisted page index, 8 bytes per live
    // page instead of 4096.
    let path = dir.path().join("large.varve");
    MatrixRecoveryReport::reset_matrix_integrity_counters();
    drop(
        spec()
            .with_read_limits(
                ReadLimits::STANDARD
                    .with_matrix_metadata_verification(MatrixMetadataVerification::OnDemand),
            )
            .open_readonly(&path)?,
    );
    let bare_bytes = MatrixRecoveryReport::matrix_open_bitmap_bytes_read();
    let bare_pages = MatrixRecoveryReport::matrix_open_bitmap_pages_visited();
    let map_queried = MatrixRecoveryReport::matrix_open_allocation_map_available();
    println!(
        "(A) verification at open: {large_bytes} bytes / {large_pages} pages | on demand: \
         {bare_bytes} bytes / {bare_pages} pages (allocation map queried: {map_queried})"
    );
    assert_eq!(
        bare_pages, 0,
        "(A) an open that does not verify visited {bare_pages} commit-map pages"
    );
    assert!(
        bare_bytes * 16 < large_bytes,
        "(A) moving verification off the open saved almost nothing: {large_bytes} -> {bare_bytes}"
    );
    assert!(
        !map_queried,
        "(A) an open that does not verify queried the allocation map, whose only reader \
         is the verification candidate set"
    );
    Ok(())
}

/// The term criterion (A) still has, and the term it no longer has.
///
/// Successor to `open_cost_still_follows_the_live_page_count`, whose subject was
/// that every page the persisted index named was read, digest-checked **and made
/// resident** before open returned. Two of those three still happen and one does
/// not, so the test now separates them. Cell count and file size are held fixed
/// and only the number of pages holding live state varies.
///
/// * **Bytes read still follow live pages.** The persisted index costs 8 bytes
///   per live page, and the verification pass reads each candidate page. That is
///   `O(live pages)` of I/O and is what verification is.
/// * **Residency does not follow them at all.** No payload page is materialised,
///   at any live-page count. What an open still holds is the page-index mirror,
///   which is what makes "not published" answerable without I/O; it is
///   `O(live pages)` and is measured here so it cannot grow silently.
#[test]
fn open_reads_follow_live_pages_but_residency_does_not() -> varve::Result<()> {
    let dir = temp_dir("measure-live-pages");

    let mut rows = Vec::new();
    for pages in [1u64, 4] {
        let path = dir.path().join(format!("live-{pages}.varve"));
        // One cell in each of `pages` distinct commit-map pages of the same
        // fixture: cell count and file size are identical across the two runs.
        let live: Vec<u64> = (0..pages).collect();
        fill_pages(&path, LARGE_WIDE_SCANS, &live)?;
        MatrixRecoveryReport::reset_matrix_integrity_counters();
        drop(spec().open_readonly(&path)?);
        rows.push((
            pages,
            MatrixRecoveryReport::matrix_open_bitmap_bytes_read(),
            MatrixRecoveryReport::matrix_open_bitmap_pages_visited(),
            MatrixRecoveryReport::matrix_open_resident_bitmap_bytes(),
            MatrixRecoveryReport::matrix_resident_page_index_bytes(),
        ));
    }

    for (pages, bytes, visited, resident, index) in &rows {
        println!(
            "(A-residual) live_pages={pages} open_bytes_read={bytes} pages_visited={visited} \
             resident_bitmap={resident} resident_page_index={index}"
        );
    }
    let (_, one_bytes, _, one_resident, one_index) = rows[0];
    let (_, four_bytes, _, four_resident, four_index) = rows[1];
    assert!(
        four_bytes > one_bytes,
        "the default open no longer reads per live page ({one_bytes} -> {four_bytes}); the \
         verification pass reads every candidate page and the index costs 8 bytes per live page"
    );
    // The term that was removed, asserted as an absolute rather than a ratio.
    assert_eq!(
        (one_resident, four_resident),
        (0, 0),
        "an open materialised bitmap payload ({one_resident} -> {four_resident}); residency is \
         demand-filled and materialises none"
    );
    // The term that remains, and must stay proportional to live state rather than
    // to the file.
    assert!(
        four_index > one_index,
        "the page-index mirror stopped tracking live pages ({one_index} -> {four_index}); it \
         is what makes an unpublished page answerable without I/O"
    );
    assert!(
        four_index <= 4 * one_index,
        "the page-index mirror grew faster than the live set ({one_index} -> {four_index})"
    );
    Ok(())
}

/// Criterion (B): **memory is bounded by the working set.**
///
/// Measured on a matrix whose live page set is far wider than the configured
/// `max_matrix_bitmap_bytes` ceiling. The criterion asks for three numbers -
/// resident bytes after open, after touching K pages, and after touching 4K -
/// and this prints them.
///
/// Its predecessor recorded the opposite of the criterion: residency was fixed at
/// open by the live page count, touching pages moved it neither way, and a
/// ceiling below the live set refused the open outright instead of bounding
/// anything. All three are now the other way round, and the third is the one that
/// mattered most - the ceiling was an availability limit that could leave a
/// matrix unopenable (`docs/known-limitations.md` section 1.3).
#[test]
fn measured_resident_bitmap_bytes_against_the_ceiling() -> varve::Result<()> {
    let dir = temp_dir("measure-ceiling");
    let path = dir.path().join("wide.varve");

    // Sixteen live commit-map pages, one committed cell in each.
    const LIVE_PAGES: u64 = 16;
    let live: Vec<u64> = (0..LIVE_PAGES).collect();
    fill_pages(&path, LARGE_WIDE_SCANS, &live)?;

    MatrixRecoveryReport::reset_matrix_integrity_counters();
    let reader = spec().open_readonly(&path)?;
    let after_open = MatrixRecoveryReport::matrix_lazy_cached_bitmap_bytes();
    let charged_after_open = MatrixRecoveryReport::matrix_open_resident_bitmap_bytes();
    let index_after_open = MatrixRecoveryReport::matrix_resident_page_index_bytes();
    println!(
        "(B) live_pages={LIVE_PAGES} cached_after_open={after_open} \
         charged_after_open={charged_after_open} resident_page_index={index_after_open}"
    );

    // Touch K = 1 page, then 4K = 4 pages, through `&self` reads.
    let touch = |count: u64| -> varve::Result<u64> {
        for page in 0..count {
            let ordinal = page * PAGE_BYTES * 8;
            assert_eq!(
                reader.matrix_cell_status::<ScalingCell>(key(ordinal))?,
                MatrixCellStatus::Committed
            );
            let _cell = reader.read_matrix_cell::<ScalingCell>(key(ordinal))?;
        }
        Ok(MatrixRecoveryReport::matrix_lazy_cached_bitmap_bytes())
    };
    let after_k = touch(1)?;
    let after_4k = touch(4)?;
    println!("(B) resident_after_touching_K=1_pages={after_k}");
    println!("(B) resident_after_touching_4K=4_pages={after_4k}");

    // Residency starts at nothing and then follows the working set, one page per
    // distinct commit-map page addressed. The open charged nothing to the budget
    // either: a demand-cached page is bounded by the cache rather than admitted
    // through the budget.
    assert_eq!(after_open, 0, "(B) the open cached {after_open} bytes");
    assert_eq!(charged_after_open, 0);
    assert_eq!(after_k, PAGE_BYTES, "(B) K=1 page must cost one page");
    assert_eq!(
        after_4k,
        4 * PAGE_BYTES,
        "(B) 4K pages must cost four pages"
    );
    drop(reader);

    // And the ceiling is a cache bound, not an admission limit: a limit below the
    // live page set opens, serves every live page, and stays inside the bound. The
    // matrix that could not be opened at all is openable.
    let ceiling = LIVE_PAGES * PAGE_BYTES / 2;
    let tight = spec().with_read_limits(ReadLimits::STANDARD.with_max_matrix_bitmap_bytes(ceiling));
    MatrixRecoveryReport::reset_matrix_integrity_counters();
    let bounded = tight.open_readonly(&path)?;
    for page in 0..LIVE_PAGES {
        assert_eq!(
            bounded.matrix_cell_status::<ScalingCell>(key(page * PAGE_BYTES * 8))?,
            MatrixCellStatus::Committed,
            "(B) page {page} of a matrix whose live set exceeds the ceiling"
        );
    }
    let cached = MatrixRecoveryReport::matrix_lazy_cached_bitmap_bytes();
    println!("(B) open with max_matrix_bitmap_bytes={ceiling}: cached={cached}");
    assert!(
        cached <= ceiling,
        "(B) residency {cached} exceeded the ceiling {ceiling} it is derived from"
    );
    Ok(())
}
