//! Criteria (A) and (B), measured against the declared option that makes them
//! reachable: `MatrixMetadataResidency::Lazy`.
//!
//! Every assertion here is an absolute number, not a ratio between two runs.
//! The ratio-only shape is exactly what let "open is O(1) in file size" pass
//! for eight rounds while open read ~128 KiB for a matrix with one live page:
//! `large <= small + allowance` is satisfied by any large constant. So each
//! test below pins a *bound in bytes and pages* first, and uses the two-fixture
//! comparison only as a second statement.
//!
//! The fixtures are sparse: the payload, checksum and bitmap extents are zero
//! extents, so a nominally 1 MiB-commit-map matrix costs a few pages on disk.
#![cfg(all(feature = "integrity", feature = "scalable-fault-injection"))]

use std::fs::OpenOptions;
use std::io::{Seek, SeekFrom, Write};
use std::path::Path;

use varve::{
    BlockDescriptor, BlockKind, Endian, Error, FormatSpec, IndexPolicy, IntegrityPolicy,
    MatrixBlockDescriptor, MatrixCellStatus, MatrixCommitDescriptor, MatrixCommitKind,
    MatrixDimensionDescriptor, MatrixDimensions, MatrixKey, MatrixMetadataResidency,
    MatrixRecoveryReport, ReadLimits, VarveBlock, VarveMatrixBlock,
};

const PAGE_BYTES: u64 = 4096;
/// Bits per commit-map page, i.e. cells per page.
const CELLS_PER_PAGE: u64 = PAGE_BYTES * 8;
const CHANNELS: u64 = 128;
/// 2_097_152 cells: a 256 KiB commit map, several allocation granules wide.
const SMALL_WIDE_SCANS: u64 = 16_384;
/// 8_388_608 cells: a 1 MiB commit map, 4x `SMALL_WIDE_SCANS`.
const LARGE_WIDE_SCANS: u64 = 65_536;

#[derive(Clone, Debug, PartialEq, Eq, VarveBlock)]
#[varve(id = 762, version = 1, kind = "matrix")]
struct LazyCell {
    value: u32,
}

impl VarveMatrixBlock for LazyCell {
    const DIMENSIONS: [&'static str; 2] = ["scan", "ch"];
    const CATEGORY: &'static str = "analysis";
    const SLOT_STRIDE: u64 = 4;
}

fn spec_with(limits: ReadLimits) -> FormatSpec {
    static BLOCKS: &[BlockDescriptor] = &[BlockDescriptor {
        id: LazyCell::ID,
        name: "LazyCell",
        version: LazyCell::VERSION,
        kind: BlockKind::Matrix,
        fields: LazyCell::FIELDS,
    }];
    static DIMENSIONS: &[MatrixDimensionDescriptor] = &[
        MatrixDimensionDescriptor { name: "scan" },
        MatrixDimensionDescriptor { name: "ch" },
    ];
    static COMMITS: &[MatrixCommitDescriptor] = &[MatrixCommitDescriptor {
        name: LazyCell::CATEGORY,
        kind: MatrixCommitKind::Cell,
    }];
    static MATRIX_BLOCKS: &[MatrixBlockDescriptor] = &[MatrixBlockDescriptor {
        block_id: LazyCell::ID,
        dimensions: LazyCell::DIMENSIONS,
        category: LazyCell::CATEGORY,
        slot_stride: LazyCell::SLOT_STRIDE,
    }];

    FormatSpec::new(
        b"MLZY",
        1,
        Endian::Little,
        0,
        IndexPolicy::ScanOnOpen,
        IntegrityPolicy::Crc32,
        varve::RecoveryPolicy::Strict,
        varve::ManifestPolicy::None,
        BLOCKS,
    )
    .with_read_limits(limits)
    .with_matrix_spec(DIMENSIONS, COMMITS, MATRIX_BLOCKS)
}

fn eager_spec() -> FormatSpec {
    spec_with(ReadLimits::STANDARD)
}

fn lazy_spec(cache_bytes: u64) -> FormatSpec {
    spec_with(
        ReadLimits::STANDARD
            .with_matrix_metadata_residency(MatrixMetadataResidency::Lazy { cache_bytes }),
    )
}

fn dims(scans: u64) -> MatrixDimensions {
    MatrixDimensions::from_pairs([("scan", scans), ("ch", CHANNELS)])
}

fn key(ordinal: u64) -> MatrixKey {
    MatrixKey::new(ordinal / CHANNELS, ordinal % CHANNELS)
}

/// Commits one cell in each named commit-map page, so "live pages" is exactly
/// `pages.len()` however large the matrix is.
fn fill_pages(path: &Path, scans: u64, pages: &[u64]) -> varve::Result<()> {
    let mut writer = eager_spec().create_writer_with_dims(path, dims(scans))?;
    for page in pages {
        let ordinal = page * CELLS_PER_PAGE;
        writer.write_matrix_cell(key(ordinal), &LazyCell { value: 7 })?;
        writer.commit_matrix_cell::<LazyCell>(key(ordinal))?;
    }
    writer.flush()?;
    Ok(())
}

fn temp_dir(name: &str) -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix(&format!("varve-matrix-lazy-{name}-"))
        .tempdir()
        .expect("temp dir")
}

fn file_len(path: &Path) -> u64 {
    std::fs::metadata(path).expect("metadata").len()
}

/// **Criterion (A), stated as an absolute bound.**
///
/// Two matrices with a 4x cell-count ratio and an identical live set of one
/// commit-map page. Under the default `EagerVerified` policy this reads ~131 KB
/// over ~32 pages for both, because the visit set is the persisted page index
/// unioned with whatever the filesystem's allocation map reports written, and
/// NTFS allocates in ~128 KiB runs. Under `Lazy` nothing is read but the
/// persisted page index, and the allocation map is never queried at all.
#[test]
fn a_lazy_open_reads_a_bounded_number_of_bytes_whatever_the_file_holds() -> varve::Result<()> {
    let dir = temp_dir("open-cost");

    let mut rows = Vec::new();
    for (name, scans) in [("small", SMALL_WIDE_SCANS), ("large", LARGE_WIDE_SCANS)] {
        let path = dir.path().join(format!("{name}.varve"));
        fill_pages(&path, scans, &[0])?;
        let len = file_len(&path);

        MatrixRecoveryReport::reset_matrix_integrity_counters();
        drop(eager_spec().open_readonly(&path)?);
        let eager = (
            MatrixRecoveryReport::matrix_open_bitmap_bytes_read(),
            MatrixRecoveryReport::matrix_open_bitmap_pages_visited(),
            MatrixRecoveryReport::matrix_open_resident_bitmap_bytes(),
        );

        MatrixRecoveryReport::reset_matrix_integrity_counters();
        drop(lazy_spec(64 * PAGE_BYTES).open_readonly(&path)?);
        let lazy = (
            MatrixRecoveryReport::matrix_open_bitmap_bytes_read(),
            MatrixRecoveryReport::matrix_open_bitmap_pages_visited(),
            MatrixRecoveryReport::matrix_resident_bitmap_bytes(),
        );
        rows.push((name, scans * CHANNELS, len, eager, lazy));
    }

    for (name, cells, len, eager, lazy) in &rows {
        println!(
            "(A) {name}: cells={cells} file_len={len} \
             EAGER bytes_read={} pages_visited={} resident={} | \
             LAZY bytes_read={} pages_visited={} resident={}",
            eager.0, eager.1, eager.2, lazy.0, lazy.1, lazy.2
        );
    }

    // The absolute statement. One live page, so the whole of open's bitmap I/O
    // is the persisted page index: an 8-byte occupancy header and one 8-byte
    // entry, plus whatever the index reader rounds its read up to. Two pages'
    // worth is a generous ceiling and still two orders of magnitude below the
    // ~131 KB the eager path reads for the same file.
    for (name, _, _, eager, lazy) in &rows {
        assert!(
            lazy.0 <= 2 * PAGE_BYTES,
            "(A) {name}: a lazy open read {} bytes for a matrix with one live page",
            lazy.0
        );
        assert_eq!(
            lazy.1, 0,
            "(A) {name}: a lazy open visited {} commit-map pages; it must visit none",
            lazy.1
        );
        assert_eq!(
            lazy.2, 0,
            "(B) {name}: a lazy open left {} resident bitmap bytes; it must leave none",
            lazy.2
        );
        assert!(
            lazy.0 * 16 < eager.0,
            "(A) {name}: lazy open ({} B) is not decisively cheaper than eager ({} B)",
            lazy.0,
            eager.0
        );
    }

    // The scale statement, kept as a second-order check: a 4x cell count and a
    // larger physical file must not move any of it.
    let (_, small_cells, small_len, _, small_lazy) = rows[0];
    let (_, large_cells, large_len, _, large_lazy) = rows[1];
    assert_eq!(large_cells / small_cells, 4, "fixtures must differ 4x");
    assert!(large_len > small_len, "the large fixture must be larger");
    assert_eq!(
        small_lazy.0, large_lazy.0,
        "(A) lazy open bytes moved with file size: {} -> {}",
        small_lazy.0, large_lazy.0
    );
    Ok(())
}

/// **Criterion (A), the live-page term.**
///
/// Holds cell count and file size fixed and varies only how many pages hold
/// live state, over a 64x range. Under `EagerVerified` both the bytes read and
/// the residency are `O(live pages)`. Under `Lazy` the payload term disappears
/// entirely: only the persisted page index still follows the live set, and it
/// costs 8 bytes per entry rather than 4096.
#[test]
fn a_lazy_open_does_not_read_a_page_per_live_page() -> varve::Result<()> {
    let dir = temp_dir("live-pages");

    let mut rows = Vec::new();
    for live in [1u64, 64] {
        let path = dir.path().join(format!("live-{live}.varve"));
        let pages: Vec<u64> = (0..live).collect();
        fill_pages(&path, LARGE_WIDE_SCANS, &pages)?;

        MatrixRecoveryReport::reset_matrix_integrity_counters();
        drop(eager_spec().open_readonly(&path)?);
        let eager_read = MatrixRecoveryReport::matrix_open_bitmap_bytes_read();
        let eager_resident = MatrixRecoveryReport::matrix_open_resident_bitmap_bytes();

        MatrixRecoveryReport::reset_matrix_integrity_counters();
        drop(lazy_spec(8 * PAGE_BYTES).open_readonly(&path)?);
        let lazy_read = MatrixRecoveryReport::matrix_open_bitmap_bytes_read();
        let lazy_resident = MatrixRecoveryReport::matrix_resident_bitmap_bytes();
        rows.push((live, eager_read, eager_resident, lazy_read, lazy_resident));
    }

    for (live, eager_read, eager_resident, lazy_read, lazy_resident) in &rows {
        println!(
            "(A-residual) live_pages={live} EAGER bytes_read={eager_read} \
             resident={eager_resident} | LAZY bytes_read={lazy_read} resident={lazy_resident}"
        );
    }

    let (_, _, one_eager_resident, one_lazy_read, one_lazy_resident) = rows[0];
    let (_, _, many_eager_resident, many_lazy_read, many_lazy_resident) = rows[1];

    assert_eq!(
        many_eager_resident,
        64 * one_eager_resident,
        "the eager policy is supposed to be exactly O(live pages) resident"
    );
    assert_eq!(one_lazy_resident, 0);
    assert_eq!(
        many_lazy_resident, 0,
        "(B) a lazy open of a 64-live-page matrix left {many_lazy_resident} resident bytes"
    );
    // 64x the live pages costs 63 more 8-byte index entries, not 63 more pages.
    assert!(
        many_lazy_read < one_lazy_read + PAGE_BYTES,
        "(A) lazy open grew by {} bytes across a 64x live-page range",
        many_lazy_read - one_lazy_read
    );
    Ok(())
}

/// **Criterion (B), in both directions.**
///
/// Residency after a lazy open is zero; it rises by exactly one page per
/// distinct commit-map page addressed; and it stops rising at the declared
/// ceiling instead of the open being refused. The last clause is the
/// availability half: the live set here is 64 pages against a 4-page cache, a
/// configuration the eager policy cannot open at all.
#[test]
fn b_resident_bytes_track_the_working_set_and_stop_at_the_ceiling() -> varve::Result<()> {
    const LIVE: u64 = 64;
    const CACHE_PAGES: u64 = 4;
    let dir = temp_dir("working-set");
    let path = dir.path().join("ws.varve");
    let pages: Vec<u64> = (0..LIVE).collect();
    fill_pages(&path, LARGE_WIDE_SCANS, &pages)?;

    MatrixRecoveryReport::reset_matrix_integrity_counters();
    let reader = lazy_spec(CACHE_PAGES * PAGE_BYTES).open_readonly(&path)?;
    let after_open = MatrixRecoveryReport::matrix_resident_bitmap_bytes();

    let mut samples = Vec::new();
    for touched in 1..=LIVE {
        let ordinal = (touched - 1) * CELLS_PER_PAGE;
        assert_eq!(
            reader.matrix_cell_status::<LazyCell>(key(ordinal))?,
            MatrixCellStatus::Committed,
            "cell in page {} must read committed through the demand path",
            touched - 1
        );
        samples.push((
            touched,
            MatrixRecoveryReport::matrix_lazy_cached_bitmap_bytes(),
        ));
    }

    let after_1 = samples[0].1;
    let after_4 = samples[3].1;
    let after_64 = samples[63].1;
    let peak = samples.iter().map(|(_, bytes)| *bytes).max().expect("peak");
    println!(
        "(B) after_open={after_open} after_1_page={after_1} after_4_pages={after_4} \
         after_64_pages={after_64} peak={peak} ceiling={}",
        CACHE_PAGES * PAGE_BYTES
    );

    assert_eq!(after_open, 0, "(B) a lazy open must materialise nothing");
    assert_eq!(
        after_1, PAGE_BYTES,
        "(B) K=1 page must cost exactly one page"
    );
    assert_eq!(
        after_4,
        CACHE_PAGES * PAGE_BYTES,
        "(B) K=4 pages must cost exactly four pages"
    );
    assert_eq!(
        peak,
        CACHE_PAGES * PAGE_BYTES,
        "(B) residency exceeded the declared ceiling: {peak}"
    );
    assert_eq!(
        after_64,
        CACHE_PAGES * PAGE_BYTES,
        "(B) residency after touching all 64 live pages must still be the ceiling"
    );

    // Re-reading a cached page costs no I/O; re-reading an evicted one does.
    // This is what makes the ceiling a cache bound rather than a truncation.
    MatrixRecoveryReport::reset_matrix_lazy_counters();
    let last = (LIVE - 1) * CELLS_PER_PAGE;
    reader.matrix_cell_status::<LazyCell>(key(last))?;
    assert_eq!(
        MatrixRecoveryReport::matrix_lazy_fault_bytes_read(),
        0,
        "a still-cached page was re-read from disk"
    );
    reader.matrix_cell_status::<LazyCell>(key(0))?;
    assert!(
        MatrixRecoveryReport::matrix_lazy_fault_bytes_read() >= PAGE_BYTES,
        "an evicted page was answered without reading it back"
    );
    Ok(())
}

/// The eager policy refuses this file outright; the lazy policy serves it.
///
/// This is the "hard availability limit" half of the re-verification finding:
/// `max_matrix_bitmap_bytes` below the live set is an admission limit under
/// `EagerVerified`, so the matrix cannot be opened at all.
#[test]
fn a_live_set_larger_than_the_bitmap_ceiling_is_openable_lazily() -> varve::Result<()> {
    const LIVE: u64 = 16;
    let dir = temp_dir("ceiling");
    let path = dir.path().join("ceiling.varve");
    let pages: Vec<u64> = (0..LIVE).collect();
    fill_pages(&path, LARGE_WIDE_SCANS, &pages)?;

    let ceiling = 4 * PAGE_BYTES;
    let eager = spec_with(ReadLimits::STANDARD.with_max_matrix_bitmap_bytes(ceiling))
        .open_readonly(&path)
        .err();
    println!("(B) eager open under a {ceiling}-byte ceiling: {eager:?}");
    assert!(
        matches!(eager, Some(Error::LimitExceeded { .. })),
        "the eager policy is supposed to refuse a live set over the ceiling"
    );

    let lazy = spec_with(
        ReadLimits::STANDARD
            .with_max_matrix_bitmap_bytes(ceiling)
            .with_matrix_metadata_residency(MatrixMetadataResidency::Lazy {
                cache_bytes: ceiling,
            }),
    )
    .open_readonly(&path)?;
    for page in 0..LIVE {
        assert_eq!(
            lazy.matrix_cell_status::<LazyCell>(key(page * CELLS_PER_PAGE))?,
            MatrixCellStatus::Committed
        );
    }
    assert!(MatrixRecoveryReport::matrix_lazy_cached_bitmap_bytes() <= ceiling);

    // A cache larger than the ceiling would be a way to raise a declared limit,
    // so it is refused at open rather than silently clamped.
    let over = spec_with(
        ReadLimits::STANDARD
            .with_max_matrix_bitmap_bytes(ceiling)
            .with_matrix_metadata_residency(MatrixMetadataResidency::Lazy {
                cache_bytes: ceiling + 1,
            }),
    )
    .open_readonly(&path)
    .err();
    assert!(
        matches!(over, Some(Error::LimitExceeded { .. })),
        "a cache above the declared bitmap ceiling was admitted: {over:?}"
    );
    Ok(())
}

/// Differential contract: the lazy policy is a residency decision, never an
/// answer decision.
///
/// Every cell of a matrix with a scattered live set is read through both
/// policies and the two answers are compared. A cache far smaller than the live
/// set is used deliberately, so most reads are served by a fault-in that
/// evicted something.
#[test]
fn a_lazily_opened_matrix_answers_exactly_what_an_eager_one_does() -> varve::Result<()> {
    let dir = temp_dir("differential");
    let path = dir.path().join("diff.varve");
    // Live pages 0, 3, 4, 9, 17 — the gaps are the interesting part: those
    // pages are genuinely unpublished and must answer NotCommitted without
    // being confused with a page that is merely uncached.
    let live = [0u64, 3, 4, 9, 17];
    fill_pages(&path, LARGE_WIDE_SCANS, &live)?;

    let eager = eager_spec().open_readonly(&path)?;
    let lazy = lazy_spec(PAGE_BYTES).open_readonly(&path)?;
    let mut committed = 0u64;
    for page in 0..24u64 {
        for offset in [0u64, 1, 7, CELLS_PER_PAGE - 1] {
            let ordinal = page * CELLS_PER_PAGE + offset;
            let expected = eager.matrix_cell_status::<LazyCell>(key(ordinal))?;
            let actual = lazy.matrix_cell_status::<LazyCell>(key(ordinal))?;
            assert_eq!(
                expected, actual,
                "page {page} offset {offset}: eager said {expected:?}, lazy said {actual:?}"
            );
            if expected == MatrixCellStatus::Committed {
                committed += 1;
                assert_eq!(
                    lazy.read_matrix_cell::<LazyCell>(key(ordinal))?,
                    eager.read_matrix_cell::<LazyCell>(key(ordinal))?
                );
            }
        }
    }
    assert_eq!(
        committed,
        live.len() as u64,
        "the fixture must have exactly one committed cell per live page"
    );
    // The aggregate is not served from the cache either: `resume_signal`
    // counts set bits across the whole map, and under the lazy policy the
    // maintained counter describes only what is cached.
    assert_eq!(
        lazy.matrix_resume_signal(LazyCell::CATEGORY)?,
        eager.matrix_resume_signal(LazyCell::CATEGORY)?
    );
    Ok(())
}

/// F-06 under the demand path, which is where it becomes reachable.
///
/// "Not cached" and "not published" must stay distinct. The persisted page
/// index is loaded in full at open under both policies and is what answers the
/// second question, so a page it does not name reads clear because the file
/// said so — not because nothing was loaded.
#[test]
fn an_unpublished_page_is_not_the_same_as_an_uncached_one() -> varve::Result<()> {
    let dir = temp_dir("f06");
    let path = dir.path().join("f06.varve");
    fill_pages(&path, LARGE_WIDE_SCANS, &[0, 5])?;

    // A one-page cache, so at most one of the two live pages is ever resident.
    let reader = lazy_spec(PAGE_BYTES).open_readonly(&path)?;
    for page in [0u64, 5] {
        assert_eq!(
            reader.matrix_cell_status::<LazyCell>(key(page * CELLS_PER_PAGE))?,
            MatrixCellStatus::Committed,
            "published page {page} read as absent while uncached"
        );
    }
    // Now page 5 is cached and page 0 is not. Page 0 must still answer
    // Committed, and the never-published page 4 must still answer NotCommitted.
    assert_eq!(
        reader.matrix_cell_status::<LazyCell>(key(0))?,
        MatrixCellStatus::Committed
    );
    assert_eq!(
        reader.matrix_cell_status::<LazyCell>(key(4 * CELLS_PER_PAGE))?,
        MatrixCellStatus::NotCommitted
    );
    assert!(matches!(
        reader
            .read_matrix_cell::<LazyCell>(key(4 * CELLS_PER_PAGE))
            .err(),
        Some(Error::MatrixNotCommitted)
    ));
    Ok(())
}

/// The declared consequence of the option, asserted rather than assumed:
/// detection moves from open to the touch that reads the damaged page.
///
/// Both halves matter. The lazy open must *succeed* — that is the point — and
/// the read of the damaged page must fail closed rather than report the
/// corrupted bits. An eager open of the same file still refuses it at open.
#[test]
fn page_corruption_is_detected_at_first_touch_and_never_silently_answered() -> varve::Result<()> {
    let dir = temp_dir("digest");
    let path = dir.path().join("digest.varve");
    fill_pages(&path, LARGE_WIDE_SCANS, &[0, 5])?;

    // Flip a byte inside commit-map page 5 so it disagrees with its digest.
    let map_off = commit_map_off(&path);
    patch_byte(&path, map_off + 5 * PAGE_BYTES + 16, 0x40);

    // Page 0 is undamaged and answers normally; page 5 fails closed.
    let reader = lazy_spec(8 * PAGE_BYTES).open_readonly(&path)?;
    assert_eq!(
        reader.matrix_cell_status::<LazyCell>(key(0))?,
        MatrixCellStatus::Committed,
        "damage in one page must not take the rest of the map with it"
    );
    let touched = reader.matrix_cell_status::<LazyCell>(key(5 * CELLS_PER_PAGE));
    println!("(lazy) first touch of a damaged page: {touched:?}");
    assert!(
        matches!(touched, Err(Error::MatrixFatalCorruption)),
        "a damaged page was answered instead of refused: {touched:?}"
    );
    // Repeating the touch must repeat the refusal, not fall through to zero.
    assert!(matches!(
        reader.matrix_cell_status::<LazyCell>(key(5 * CELLS_PER_PAGE)),
        Err(Error::MatrixFatalCorruption)
    ));
    Ok(())
}

/// Writes through a lazily opened matrix agree with writes through an eagerly
/// opened one, cell for cell.
///
/// The mutation engine reads the byte it is about to change, so every write
/// path faults its page in before preparing. A path that did not would compute
/// its update from an absent page — the "absent means zero" confusion, arriving
/// through the write side instead of the read side.
#[test]
fn writes_through_a_lazy_handle_match_writes_through_an_eager_one() -> varve::Result<()> {
    let dir = temp_dir("writes");
    let ordinals: Vec<u64> = (0..6)
        .flat_map(|page: u64| [page * CELLS_PER_PAGE, page * CELLS_PER_PAGE + 11])
        .collect();

    let mut states = Vec::new();
    for (name, lazy) in [("eager", false), ("lazy", true)] {
        let path = dir.path().join(format!("{name}.varve"));
        fill_pages(&path, LARGE_WIDE_SCANS, &[0, 2])?;
        let spec = if lazy {
            lazy_spec(2 * PAGE_BYTES)
        } else {
            eager_spec()
        };
        {
            let mut writer = spec.open_writer(&path)?;
            for ordinal in &ordinals {
                writer.write_matrix_cell(key(*ordinal), &LazyCell { value: 3 })?;
                writer.commit_matrix_cell::<LazyCell>(key(*ordinal))?;
            }
            // Clear half of them again, which is the path that releases a page
            // when its last set bit goes.
            for ordinal in ordinals.iter().step_by(2) {
                writer.clear_matrix_cell::<LazyCell>(key(*ordinal))?;
            }
            writer.flush()?;
        }
        let reader = eager_spec().open_readonly(&path)?;
        let mut row = Vec::new();
        for page in 0..8u64 {
            for offset in [0u64, 11] {
                row.push(
                    reader.matrix_cell_status::<LazyCell>(key(page * CELLS_PER_PAGE + offset))?,
                );
            }
        }
        let signal = reader.matrix_resume_signal(LazyCell::CATEGORY)?;
        println!("({name}) resume_signal={signal:?}");
        states.push((name, row, signal));
    }

    assert_eq!(
        states[0].1, states[1].1,
        "a lazily written matrix disagrees with an eagerly written one"
    );
    assert_eq!(states[0].2, states[1].2, "committed counts disagree");
    Ok(())
}

/// The option is inert unless declared: `ReadLimits::STANDARD` must still be
/// `EagerVerified`, and an eager open must still visit and read pages.
#[test]
fn the_default_policy_is_unchanged() -> varve::Result<()> {
    assert_eq!(
        ReadLimits::STANDARD.matrix_metadata_residency,
        MatrixMetadataResidency::EagerVerified
    );
    assert_eq!(
        ReadLimits::UNTRUSTED.matrix_metadata_residency,
        MatrixMetadataResidency::EagerVerified
    );
    assert_eq!(
        ReadLimits::default().matrix_metadata_residency,
        MatrixMetadataResidency::EagerVerified
    );

    let dir = temp_dir("default");
    let path = dir.path().join("default.varve");
    fill_pages(&path, SMALL_WIDE_SCANS, &[0])?;
    MatrixRecoveryReport::reset_matrix_integrity_counters();
    drop(eager_spec().open_readonly(&path)?);
    assert!(
        MatrixRecoveryReport::matrix_open_bitmap_pages_visited() > 0
            && MatrixRecoveryReport::matrix_open_resident_bitmap_bytes() > 0,
        "the default policy stopped reading pages at open; the option is not inert"
    );
    Ok(())
}

// --- fixture plumbing -------------------------------------------------------

/// Native file header, then the 24-byte matrix creation-nonce region.
const NATIVE_PREFIX_LEN: u64 = 18 + 24;

fn vmat_header_offset() -> u64 {
    NATIVE_PREFIX_LEN + u64::try_from(eager_spec().magic.len()).expect("magic length")
}

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

/// Criterion (C), under the new option: demand loading must not put back the
/// read convoy round 16 removed.
///
/// The lazy path introduced the first lock the matrix read path has ever had —
/// a `Mutex` over each bitmap's page map, so a fault-in can happen under
/// `&self`. A first cut held it across the fault-in `pread`, which would have
/// serialised every reader of a category behind whichever one missed the cache.
/// The contract asserted here is the observable one, in wall clock and with the
/// total work held fixed, exactly as `matrix_concurrent_reads.rs` does for the
/// eager path: N threads must not take *longer* than one.
///
/// The configuration is the hostile one on purpose — a one-page cache against a
/// many-page live set, so nearly every read is a miss that also evicts.
#[test]
fn concurrent_lazy_readers_are_not_serialised_behind_the_page_store() -> varve::Result<()> {
    let threads = std::thread::available_parallelism()
        .map(std::num::NonZeroUsize::get)
        .unwrap_or(1)
        .min(4);
    if threads < 2 {
        println!("single-core host: nothing to serialise, skipping");
        return Ok(());
    }

    const LIVE: u64 = 32;
    const READS: u64 = 40_960;
    let dir = temp_dir("no-convoy");
    let path = dir.path().join("convoy.varve");
    let pages: Vec<u64> = (0..LIVE).collect();
    fill_pages(&path, LARGE_WIDE_SCANS, &pages)?;

    // One page of cache for 32 live pages: every read misses and evicts.
    let reader = lazy_spec(PAGE_BYTES).open_readonly(&path)?;
    let reader = &reader;

    let run = |workers: u64| -> std::time::Duration {
        let each = READS / workers;
        let started = std::time::Instant::now();
        std::thread::scope(|scope| {
            for worker in 0..workers {
                scope.spawn(move || {
                    for step in 0..each {
                        let page = (step + worker * 7) % LIVE;
                        reader
                            .matrix_cell_status::<LazyCell>(key(page * CELLS_PER_PAGE))
                            .expect("concurrent lazy status read");
                    }
                });
            }
        });
        started.elapsed()
    };

    // Warm the page cache of the OS, so the comparison is about our locking.
    run(1);
    let one = run(1);
    let many = run(threads as u64);
    let ratio = many.as_secs_f64() / one.as_secs_f64();
    println!(
        "{READS} lazy fault-in reads: 1 thread {:.3}s, {threads} threads {:.3}s (ratio {ratio:.2}x, \
         lower is better)",
        one.as_secs_f64(),
        many.as_secs_f64()
    );
    assert!(
        ratio <= 1.0,
        "lazy readers convoy: {threads} threads took {ratio:.2}x as long as one for the same work"
    );
    Ok(())
}

fn patch_byte(path: &Path, offset: u64, value: u8) {
    let mut file = OpenOptions::new()
        .write(true)
        .open(path)
        .expect("open matrix for mutation");
    file.seek(SeekFrom::Start(offset)).expect("seek");
    file.write_all(&[value]).expect("patch byte");
}
