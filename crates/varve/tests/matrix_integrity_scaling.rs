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
    MatrixBlockDescriptor, MatrixCommitDescriptor, MatrixCommitKind, MatrixCorruptionKind,
    MatrixDimensionDescriptor, MatrixDimensions, MatrixKey, MatrixRecoveryReport, ReadLimits,
    VarveBlock, VarveMatrixBlock,
};

/// Commit-map page size used by the paged integrity representation.
const PAGE_BYTES: u64 = 4096;
/// Native file header, then the 24-byte matrix creation-nonce region (DUR2-03).
const NATIVE_PREFIX_LEN: u64 = 18 + 24;
/// Current on-disk matrix layout version (v3 adds the persisted page index).
const VMAT_VERSION: u16 = 3;

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
    // One commit-map page plus one checksum-validity page; the never-touched
    // pages of the large matrix cost nothing.
    assert_eq!(small, 2 * PAGE_BYTES);
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

    let mut reader = spec().open_reader(&path)?;
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
