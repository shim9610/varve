//! Criteria (A) and (B), measured against the residency model that is now the
//! only one: bounded, demand-filled `MatrixMetadataResidency::Lazy`.
//!
//! Every assertion here is an absolute number, not a ratio between two runs.
//! The ratio-only shape is exactly what let "open is O(1) in file size" pass
//! for eight rounds while open read ~128 KiB for a matrix with one live page:
//! `large <= small + allowance` is satisfied by any large constant. So each
//! test below pins a *bound in bytes and pages* first, and uses the two-fixture
//! comparison only as a second statement.
//!
//! **What the comparison arm is, since 0.5.0.** These tests used to compare the
//! `Lazy` policy against `MatrixMetadataResidency::EagerVerified`. That variant is
//! gone: residency is always bounded, and what the eager variant really provided -
//! authenticating the whole commit map at open - is now
//! `MatrixMetadataVerification`, which retains one page buffer and defaults to
//! running at open. So the arm each test compares against is
//! `MatrixMetadataVerification::AtOpen` (`verified_spec`), and the arm that
//! measures pure open cost declares `OnDemand` (`lazy_spec`). The numbers on both
//! sides are the same numbers as before; what changed is which policy owns them.
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
    MatrixMetadataVerification, MatrixRecoveryReport, ReadLimits, VarveBlock, VarveMatrixBlock,
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

/// Verification at open, declared rather than assumed. This is the comparison arm
/// for every open-cost measurement below: it is what the default does, and until
/// 0.5.0 it was inseparable from eager residency.
fn verified_spec() -> FormatSpec {
    spec_with(
        ReadLimits::STANDARD.with_matrix_metadata_verification(MatrixMetadataVerification::AtOpen),
    )
}

/// What a caller who has never heard of either option gets.
fn default_spec() -> FormatSpec {
    spec_with(ReadLimits::STANDARD)
}

/// A declared cache, with verification moved off the open.
///
/// Both halves are deliberate. The cache is what these tests measure; declaring
/// `OnDemand` is what makes the measurement about *residency* rather than about
/// the verification pass, which reads pages by design. A test that wants the
/// default's full open cost uses `verified_spec`.
fn lazy_spec(cache_bytes: u64) -> FormatSpec {
    spec_with(
        ReadLimits::STANDARD
            .with_matrix_metadata_residency(MatrixMetadataResidency::Lazy { cache_bytes })
            .with_matrix_metadata_verification(MatrixMetadataVerification::OnDemand),
    )
}

/// `MatrixMetadataResidency::DEFAULT`, with the cache a lazy default derives
/// from these limits filled in — i.e. what the unset state must resolve to.
fn resolved_default(limits: ReadLimits) -> MatrixMetadataResidency {
    match MatrixMetadataResidency::DEFAULT {
        MatrixMetadataResidency::Lazy { .. } => MatrixMetadataResidency::Lazy {
            cache_bytes: limits.default_matrix_metadata_cache_bytes(),
        },
        default => default,
    }
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
    let mut writer = verified_spec().create_writer_with_dims(path, dims(scans))?;
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
/// commit-map page. With verification at open this reads tens of KB
/// over ~32 pages for both, because the visit set is the persisted page index
/// unioned with whatever the filesystem's allocation map reports written, and
/// NTFS allocates in ~128 KiB runs. With verification on demand nothing is read
/// but the persisted page index, and the allocation map is never queried at all.
#[test]
fn a_lazy_open_reads_a_bounded_number_of_bytes_whatever_the_file_holds() -> varve::Result<()> {
    let dir = temp_dir("open-cost");

    let mut rows = Vec::new();
    for (name, scans) in [("small", SMALL_WIDE_SCANS), ("large", LARGE_WIDE_SCANS)] {
        let path = dir.path().join(format!("{name}.varve"));
        fill_pages(&path, scans, &[0])?;
        let len = file_len(&path);

        MatrixRecoveryReport::reset_matrix_integrity_counters();
        drop(verified_spec().open_readonly(&path)?);
        let verified = (
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
        rows.push((name, scans * CHANNELS, len, verified, lazy));
    }

    for (name, cells, len, verified, lazy) in &rows {
        println!(
            "(A) {name}: cells={cells} file_len={len} \
             VERIFIED bytes_read={} pages_visited={} resident={} | \
             BARE bytes_read={} pages_visited={} resident={}",
            verified.0, verified.1, verified.2, lazy.0, lazy.1, lazy.2
        );
    }

    // The absolute statement. One live page, so the whole of open's bitmap I/O
    // is the persisted page index: an 8-byte occupancy header and one 8-byte
    // entry, plus whatever the index reader rounds its read up to. Two pages'
    // worth is a generous ceiling and still two orders of magnitude below the
    // ~70 KB the verifying open reads for the same file.
    for (name, _, _, verified, lazy) in &rows {
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
            lazy.0 * 16 < verified.0,
            "(A) {name}: a bare open ({} B) is not much cheaper than verifying ({} B)",
            lazy.0,
            verified.0
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
/// live state, over a 64x range. With verification at open the bytes read are
/// `O(live pages)`; residency is not, at any policy. With verification on demand
/// the payload term disappears entirely: only the persisted page index still
/// follows the live set, and it costs 8 bytes per entry rather than 4096.
#[test]
fn a_lazy_open_does_not_read_a_page_per_live_page() -> varve::Result<()> {
    let dir = temp_dir("live-pages");

    let mut rows = Vec::new();
    for live in [1u64, 64] {
        let path = dir.path().join(format!("live-{live}.varve"));
        let pages: Vec<u64> = (0..live).collect();
        fill_pages(&path, LARGE_WIDE_SCANS, &pages)?;

        MatrixRecoveryReport::reset_matrix_integrity_counters();
        drop(verified_spec().open_readonly(&path)?);
        let verified_read = MatrixRecoveryReport::matrix_open_bitmap_bytes_read();
        let verified_resident = MatrixRecoveryReport::matrix_open_resident_bitmap_bytes();

        MatrixRecoveryReport::reset_matrix_integrity_counters();
        drop(lazy_spec(8 * PAGE_BYTES).open_readonly(&path)?);
        let lazy_read = MatrixRecoveryReport::matrix_open_bitmap_bytes_read();
        let lazy_resident = MatrixRecoveryReport::matrix_resident_bitmap_bytes();
        rows.push((
            live,
            verified_read,
            verified_resident,
            lazy_read,
            lazy_resident,
        ));
    }

    for (live, verified_read, verified_resident, lazy_read, lazy_resident) in &rows {
        println!(
            "(A-residual) live_pages={live} VERIFIED bytes_read={verified_read} \
             resident={verified_resident} | BARE bytes_read={lazy_read} resident={lazy_resident}"
        );
    }

    let (_, _, one_verified_resident, one_lazy_read, one_lazy_resident) = rows[0];
    let (_, _, many_verified_resident, many_lazy_read, many_lazy_resident) = rows[1];

    // Residency is zero at every live-page count and under every policy: it used
    // to be exactly `64 * one` here, which is the term the split removed.
    assert_eq!(
        (one_verified_resident, many_verified_resident),
        (0, 0),
        "an open materialised bitmap payload; residency is demand-filled"
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
/// configuration an eager open could not have opened at all.
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

/// A live set wider than `max_matrix_bitmap_bytes` is served, not refused.
///
/// The "hard availability limit" half of the re-verification finding. Until 0.5.0
/// this ceiling was an admission limit on a matrix's whole live set, because the
/// live set was made resident at open, so this file could not be opened at all
/// (`docs/known-limitations.md` section 1.3). The refusal arm that stood here is
/// gone with the eager variant it declared; what remains is the cure, plus the one
/// refusal that is still deliberate - a *declared* cache above the ceiling.
#[test]
fn a_live_set_larger_than_the_bitmap_ceiling_is_openable_lazily() -> varve::Result<()> {
    const LIVE: u64 = 16;
    let dir = temp_dir("ceiling");
    let path = dir.path().join("ceiling.varve");
    let pages: Vec<u64> = (0..LIVE).collect();
    fill_pages(&path, LARGE_WIDE_SCANS, &pages)?;

    let ceiling = 4 * PAGE_BYTES;
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
fn a_lazily_opened_matrix_answers_exactly_what_a_verified_one_does() -> varve::Result<()> {
    let dir = temp_dir("differential");
    let path = dir.path().join("diff.varve");
    // Live pages 0, 3, 4, 9, 17 — the gaps are the interesting part: those
    // pages are genuinely unpublished and must answer NotCommitted without
    // being confused with a page that is merely uncached.
    let live = [0u64, 3, 4, 9, 17];
    fill_pages(&path, LARGE_WIDE_SCANS, &live)?;

    let verified = verified_spec().open_readonly(&path)?;
    let lazy = lazy_spec(PAGE_BYTES).open_readonly(&path)?;
    let mut committed = 0u64;
    for page in 0..24u64 {
        for offset in [0u64, 1, 7, CELLS_PER_PAGE - 1] {
            let ordinal = page * CELLS_PER_PAGE + offset;
            let expected = verified.matrix_cell_status::<LazyCell>(key(ordinal))?;
            let actual = lazy.matrix_cell_status::<LazyCell>(key(ordinal))?;
            assert_eq!(
                expected, actual,
                "page {page} offset {offset}: verified said {expected:?}, bare said {actual:?}"
            );
            if expected == MatrixCellStatus::Committed {
                committed += 1;
                assert_eq!(
                    lazy.read_matrix_cell::<LazyCell>(key(ordinal))?,
                    verified.read_matrix_cell::<LazyCell>(key(ordinal))?
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
        verified.matrix_resume_signal(LazyCell::CATEGORY)?
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
/// corrupted bits. An open that verifies still reports the same damage at open.
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

/// The verification policy, end to end: the same damage, reported at open, on
/// demand, and gated only by the first.
///
/// This is the contract that made flipping the residency default a loss before
/// 0.5.0, so it is asserted directly rather than inferred from the eight contracts
/// that depend on it. One fixture, one damaged commit-map page, three opens.
#[test]
fn verification_reports_the_same_damage_at_open_and_on_demand() -> varve::Result<()> {
    let dir = temp_dir("verify-timing");
    let path = dir.path().join("verify.varve");
    fill_pages(&path, LARGE_WIDE_SCANS, &[0, 5])?;
    let map_off = commit_map_off(&path);
    patch_byte(&path, map_off + 5 * PAGE_BYTES + 16, 0x40);

    let commit_map_finding = |report: &MatrixRecoveryReport| {
        report
            .findings
            .iter()
            .any(|finding| finding.kind == varve::MatrixCorruptionKind::CommitMap)
    };

    // (1) Verification at open - the default. The report names the damage, the
    // category is quarantined, and the quarantine is what a writer is refused by.
    let verified = default_spec().open_readonly(&path)?;
    assert!(
        commit_map_finding(&verified.matrix_recovery_report()),
        "an open that verifies did not report commit-map damage"
    );
    assert!(matches!(
        verified.matrix_cell_status::<LazyCell>(key(0)),
        Err(Error::MatrixCommitQuarantined(name)) if name == LazyCell::CATEGORY
    ));
    drop(verified);
    let mut writer = default_spec().open_writer(&path)?;
    assert!(
        matches!(
            writer.write_matrix_cell(key(0), &LazyCell { value: 1 }),
            Err(Error::MatrixCommitQuarantined(name)) if name == LazyCell::CATEGORY
        ),
        "a strict-recovery writer was not stopped from mutating a damaged category"
    );
    drop(writer);

    // (2) Verification on demand. The open is silent, and asking produces exactly
    // the same finding - it is the same pass over the same candidate set.
    let bare = lazy_spec(8 * PAGE_BYTES).open_readonly(&path)?;
    assert!(
        !commit_map_finding(&bare.matrix_recovery_report()),
        "an open that does not verify reported commit-map damage anyway"
    );
    MatrixRecoveryReport::reset_matrix_integrity_counters();
    let asked = bare.verify_matrix_metadata()?;
    assert!(
        commit_map_finding(&asked),
        "verifying on demand did not find the damage an open finds"
    );
    println!(
        "(verify) on-demand pass: bytes_read={} pages_visited={} resident={}",
        MatrixRecoveryReport::matrix_open_bitmap_bytes_read(),
        MatrixRecoveryReport::matrix_open_bitmap_pages_visited(),
        MatrixRecoveryReport::matrix_resident_bitmap_bytes()
    );
    // It retained nothing, and it reports the same recommendation the open-time
    // pass does.
    assert_eq!(MatrixRecoveryReport::matrix_resident_bitmap_bytes(), 0);
    assert_eq!(
        asked.recommended_actions,
        default_spec()
            .open_readonly(&path)?
            .matrix_recovery_report()
            .recommended_actions
    );
    // But it does not arm the gate: that is derived at open, and this reader was
    // opened without it. An undamaged page still answers, a damaged one refuses.
    assert_eq!(
        bare.matrix_cell_status::<LazyCell>(key(0))?,
        MatrixCellStatus::Committed
    );
    assert!(matches!(
        bare.matrix_cell_status::<LazyCell>(key(5 * CELLS_PER_PAGE)),
        Err(Error::MatrixFatalCorruption)
    ));
    drop(bare);

    // (3) The same call on a writer handle, because a writer is the caller that
    // most needs it: it is the one the quarantine would have gated.
    let writer = lazy_spec(8 * PAGE_BYTES).open_writer(&path)?;
    assert!(
        commit_map_finding(&writer.verify_matrix_metadata()?),
        "verifying on demand through a writer did not find the damage"
    );
    Ok(())
}

/// Writes through a matrix opened with a tiny cache agree with writes through one
/// opened with the default, cell for cell.
///
/// The mutation engine reads the byte it is about to change, so every write
/// path faults its page in before preparing. A path that did not would compute
/// its update from an absent page — the "absent means zero" confusion, arriving
/// through the write side instead of the read side.
#[test]
fn writes_through_a_lazy_handle_match_writes_through_a_verified_one() -> varve::Result<()> {
    let dir = temp_dir("writes");
    let ordinals: Vec<u64> = (0..6)
        .flat_map(|page: u64| [page * CELLS_PER_PAGE, page * CELLS_PER_PAGE + 11])
        .collect();

    let mut states = Vec::new();
    for (name, lazy) in [("verified", false), ("bare", true)] {
        let path = dir.path().join(format!("{name}.varve"));
        fill_pages(&path, LARGE_WIDE_SCANS, &[0, 2])?;
        let spec = if lazy {
            lazy_spec(2 * PAGE_BYTES)
        } else {
            verified_spec()
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
        let reader = verified_spec().open_readonly(&path)?;
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
        "a matrix written through a tiny cache disagrees with one written through the default"
    );
    assert_eq!(states[0].2, states[1].2, "committed counts disagree");
    Ok(())
}

/// Where the default comes from, and what it currently is.
///
/// This is the successor to `the_default_policy_is_unchanged`, whose two eager
/// assertions became the verification arm at the end: "the open still visits and
/// reads pages" is exactly as load-bearing when it is verification doing it.
///
/// The important half is the first one. No preset declares a policy any more —
/// `Missing` is the unset state, the same one every ceiling starts in — so the
/// default lives in exactly one constant, `MatrixMetadataResidency::DEFAULT`,
/// and the resolution of the unset state lives in exactly one function.
#[test]
fn the_default_policy_is_resolved_in_exactly_one_place() -> varve::Result<()> {
    for (name, limits) in [
        ("STANDARD", ReadLimits::STANDARD),
        ("UNTRUSTED", ReadLimits::UNTRUSTED),
        ("default()", ReadLimits::default()),
        ("MISSING", ReadLimits::MISSING),
        ("TRUSTED_UNBOUNDED", ReadLimits::TRUSTED_UNBOUNDED),
    ] {
        assert_eq!(
            limits.matrix_metadata_residency,
            MatrixMetadataResidency::Missing,
            "{name} declares a residency policy; a preset must leave it unset so an \
             overlay can compose and so the default has one home"
        );
        assert_eq!(
            limits.effective_matrix_metadata_residency(),
            resolved_default(limits),
            "{name} resolves the unset state somewhere other than \
             `MatrixMetadataResidency::DEFAULT`"
        );
    }

    // The cache a lazy default uses is derived from the bitmap ceiling, not
    // chosen: 64 MiB / 32 = 2 MiB = 512 commit-map pages, which is more than the
    // whole commit map of the largest matrix `STANDARD` admits (16,000,000 cells
    // = 1,953,125 bytes = 477 pages), so ordinary work never evicts.
    assert_eq!(
        MatrixMetadataResidency::DEFAULT_CACHE_BYTES,
        2 * 1024 * 1024
    );
    assert_eq!(
        MatrixMetadataResidency::DEFAULT_CACHE_BYTES,
        512 * PAGE_BYTES
    );
    assert_eq!(
        ReadLimits::STANDARD.default_matrix_metadata_cache_bytes(),
        MatrixMetadataResidency::DEFAULT_CACHE_BYTES
    );
    // A bitmap ceiling below that cache clamps it instead of refusing the open:
    // the caller tightened a memory ceiling, they did not ask for every matrix
    // open to fail. A *declared* cache over the ceiling is still refused, which
    // `a_live_set_larger_than_the_bitmap_ceiling_is_openable_lazily` asserts.
    assert_eq!(
        ReadLimits::STANDARD
            .with_max_matrix_bitmap_bytes(16 * PAGE_BYTES)
            .default_matrix_metadata_cache_bytes(),
        16 * PAGE_BYTES
    );
    // A declared policy is never second-guessed, in either direction: a cache
    // smaller than the derived default is kept, and so is a larger one.
    for declared in [
        MatrixMetadataResidency::Lazy {
            cache_bytes: PAGE_BYTES,
        },
        MatrixMetadataResidency::Lazy {
            cache_bytes: 4 * MatrixMetadataResidency::DEFAULT_CACHE_BYTES,
        },
    ] {
        assert_eq!(
            ReadLimits::STANDARD
                .with_matrix_metadata_residency(declared)
                .effective_matrix_metadata_residency(),
            declared
        );
    }

    // The verification policy is the same shape, resolved in its own single
    // place, and it is what the eager residency variant's *behaviour* became.
    for (name, limits) in [
        ("STANDARD", ReadLimits::STANDARD),
        ("UNTRUSTED", ReadLimits::UNTRUSTED),
        ("default()", ReadLimits::default()),
        ("MISSING", ReadLimits::MISSING),
        ("TRUSTED_UNBOUNDED", ReadLimits::TRUSTED_UNBOUNDED),
    ] {
        assert_eq!(
            limits.matrix_metadata_verification,
            MatrixMetadataVerification::Missing,
            "{name} declares a verification policy; a preset must leave it unset"
        );
        assert_eq!(
            limits.effective_matrix_metadata_verification(),
            MatrixMetadataVerification::DEFAULT,
            "{name} resolves the unset verification state somewhere else"
        );
    }
    for declared in [
        MatrixMetadataVerification::AtOpen,
        MatrixMetadataVerification::OnDemand,
    ] {
        assert_eq!(
            ReadLimits::STANDARD
                .with_matrix_metadata_verification(declared)
                .effective_matrix_metadata_verification(),
            declared
        );
    }
    // Verifying at open is the default, because the alternative is losing the
    // recovery report's commit-map half silently. Residency being lazy is a
    // memory decision; verification being lazy would be a behaviour decision.
    assert_eq!(
        MatrixMetadataVerification::DEFAULT,
        MatrixMetadataVerification::AtOpen,
        "the default verification policy moved; the eight contracts depend on it"
    );

    // And the current value of that one constant: the bounded demand cache, with
    // the derived default bound. It used to be `EagerVerified`, and what kept it
    // there was not cost but that the commit-map half of `MatrixRecoveryReport`
    // was a side effect of the eager page load. Splitting verification out of
    // residency removed that reason, so the constant moved and the variant was
    // deleted rather than left as an alias for a residency model it no longer
    // describes.
    assert_eq!(
        MatrixMetadataResidency::DEFAULT,
        MatrixMetadataResidency::Lazy {
            cache_bytes: MatrixMetadataResidency::DEFAULT_CACHE_BYTES,
        },
        "the default residency policy moved"
    );

    let dir = temp_dir("default");
    let path = dir.path().join("default.varve");
    fill_pages(&path, SMALL_WIDE_SCANS, &[0])?;

    // An undeclared open: the default residency *and* the default verification.
    // It retains nothing, and it still visits pages, because verifying at open is
    // what the default does.
    MatrixRecoveryReport::reset_matrix_integrity_counters();
    drop(default_spec().open_readonly(&path)?);
    let visited = MatrixRecoveryReport::matrix_open_bitmap_pages_visited();
    let resident = MatrixRecoveryReport::matrix_resident_bitmap_bytes();
    let read = MatrixRecoveryReport::matrix_open_bitmap_bytes_read();
    println!(
        "(default = {:?} + {:?}) bytes_read={read} pages={visited} resident={resident}",
        MatrixMetadataResidency::DEFAULT,
        MatrixMetadataVerification::DEFAULT
    );
    assert_eq!(
        resident, 0,
        "the default open left {resident} bitmap payload bytes resident"
    );
    assert!(
        visited > 0,
        "the default open verified nothing; damage would not announce itself"
    );

    // And the same file with verification declared off: now open is the page
    // index and nothing else.
    MatrixRecoveryReport::reset_matrix_integrity_counters();
    drop(lazy_spec(8 * PAGE_BYTES).open_readonly(&path)?);
    let bare_visited = MatrixRecoveryReport::matrix_open_bitmap_pages_visited();
    let bare_read = MatrixRecoveryReport::matrix_open_bitmap_bytes_read();
    println!("(no verification) open bytes_read={bare_read} pages_visited={bare_visited}");
    assert_eq!(bare_visited, 0);
    assert!(bare_read <= 2 * PAGE_BYTES, "open read {bare_read} bytes");
    assert_eq!(MatrixRecoveryReport::matrix_resident_bitmap_bytes(), 0);
    Ok(())
}

/// What flipping `MatrixMetadataResidency::DEFAULT` buys, measured on the cache
/// the default would derive rather than on a hand-picked small one.
///
/// The declared-`Lazy` tests above use tiny caches, which is what makes eviction
/// observable there; that leaves the other half of the default's case unmeasured.
/// This test runs the *derived* 2 MiB cache over a 64-page working set, so it
/// states both halves: open costs 32 bytes and no page visits at any live-page
/// count, and residency then tracks the working set exactly, nowhere near the
/// ceiling — a default that thrashed its LRU on ordinary work would fail here.
///
/// Every number printed here becomes the default's number the moment the
/// constant flips, and the assertions are written to hold either way.
#[test]
fn the_derived_default_cache_bounds_open_cost_and_the_working_set() -> varve::Result<()> {
    const LIVE: u64 = 64;
    let derived = ReadLimits::STANDARD.default_matrix_metadata_cache_bytes();
    let dir = temp_dir("derived-default");

    let mut rows = Vec::new();
    for live in [1u64, LIVE] {
        let path = dir.path().join(format!("live-{live}.varve"));
        let pages: Vec<u64> = (0..live).collect();
        fill_pages(&path, LARGE_WIDE_SCANS, &pages)?;

        MatrixRecoveryReport::reset_matrix_integrity_counters();
        drop(verified_spec().open_readonly(&path)?);
        let verified = (
            MatrixRecoveryReport::matrix_open_bitmap_bytes_read(),
            MatrixRecoveryReport::matrix_open_bitmap_pages_visited(),
            MatrixRecoveryReport::matrix_resident_bitmap_bytes(),
        );

        MatrixRecoveryReport::reset_matrix_integrity_counters();
        drop(lazy_spec(derived).open_readonly(&path)?);
        let lazy = (
            MatrixRecoveryReport::matrix_open_bitmap_bytes_read(),
            MatrixRecoveryReport::matrix_open_bitmap_pages_visited(),
            MatrixRecoveryReport::matrix_resident_bitmap_bytes(),
        );
        println!(
            "(derived default cache={derived}) live_pages={live} \
             VERIFIED bytes_read={} pages_visited={} resident={} | \
             BARE bytes_read={} pages_visited={} resident={}",
            verified.0, verified.1, verified.2, lazy.0, lazy.1, lazy.2
        );
        rows.push((live, verified, lazy));
    }

    for (live, verified, lazy) in &rows {
        assert!(
            lazy.0 <= 2 * PAGE_BYTES,
            "(A) the derived default read {} bytes at {live} live pages",
            lazy.0
        );
        assert_eq!(lazy.1, 0, "(A) it visited a commit-map page at open");
        assert_eq!(lazy.2, 0, "(B) it left residency behind at open");
        assert!(
            lazy.0 * 16 < verified.0,
            "(A) the derived default ({} B) is not decisively cheaper than a verifying open ({} B)",
            lazy.0,
            verified.0
        );
    }

    // Residency after open, after touching 4 pages, and after touching all 64.
    let path = dir.path().join(format!("live-{LIVE}.varve"));
    MatrixRecoveryReport::reset_matrix_integrity_counters();
    let reader = lazy_spec(derived).open_readonly(&path)?;
    let after_open = MatrixRecoveryReport::matrix_resident_bitmap_bytes();
    let mut after_4 = 0;
    for touched in 1..=LIVE {
        assert_eq!(
            reader.matrix_cell_status::<LazyCell>(key((touched - 1) * CELLS_PER_PAGE))?,
            MatrixCellStatus::Committed
        );
        if touched == 4 {
            after_4 = MatrixRecoveryReport::matrix_lazy_cached_bitmap_bytes();
        }
    }
    let after_all = MatrixRecoveryReport::matrix_lazy_cached_bitmap_bytes();
    println!(
        "(derived default cache={derived}) resident after_open={after_open} \
         after_4_pages={after_4} after_{LIVE}_pages={after_all}"
    );
    assert_eq!(after_open, 0, "(B) the open must materialise nothing");
    assert_eq!(after_4, 4 * PAGE_BYTES, "(B) K pages must cost K pages");
    assert_eq!(
        after_all,
        LIVE * PAGE_BYTES,
        "(B) residency must follow the working set exactly below the ceiling"
    );
    assert!(
        after_all < derived,
        "(B) a 64-page working set is at the derived ceiling ({derived} B); a default \
         cache this small would be thrashing its LRU on ordinary work"
    );
    Ok(())
}

/// The unopenable-matrix failure of `docs/known-limitations.md` section 1.3, and
/// that it is cured for a caller who declares nothing.
///
/// A live set wider than `max_matrix_bitmap_bytes` could not be opened at all
/// while eager residency was the default, because the ceiling was an admission
/// limit on the whole live set. The cure was never raising the ceiling - it is the
/// ceiling bounding a demand-filled cache, which is what the *derived* default
/// cache does for any ceiling, however small. The arm that asserted the refusal
/// is gone with the variant that produced it; what is asserted here is that the
/// undeclared open succeeds and stays inside the ceiling.
#[test]
fn a_live_set_over_the_bitmap_ceiling_opens_under_the_derived_default_cache() -> varve::Result<()> {
    const LIVE: u64 = 16;
    let ceiling = 4 * PAGE_BYTES;
    let dir = temp_dir("default-ceiling");
    let path = dir.path().join("ceiling.varve");
    let pages: Vec<u64> = (0..LIVE).collect();
    fill_pages(&path, LARGE_WIDE_SCANS, &pages)?;

    // The undeclared open: the section 1.3 failure, asserted as cured rather than
    // described. Every live page is served, through a cache four pages wide.
    let limits = ReadLimits::STANDARD.with_max_matrix_bitmap_bytes(ceiling);
    MatrixRecoveryReport::reset_matrix_integrity_counters();
    let undeclared = spec_with(limits).open_readonly(&path)?;
    for page in 0..LIVE {
        assert_eq!(
            undeclared.matrix_cell_status::<LazyCell>(key(page * CELLS_PER_PAGE))?,
            MatrixCellStatus::Committed,
            "(1.3) page {page} of a matrix whose live set exceeds the ceiling"
        );
    }
    let cached = MatrixRecoveryReport::matrix_lazy_cached_bitmap_bytes();
    println!("(1.3) undeclared open under a {ceiling}-byte ceiling: cached={cached}");
    assert!(
        cached <= ceiling,
        "(1.3) the derived cache ({cached} B) exceeded the ceiling it was clamped to"
    );
    drop(undeclared);

    // Same file, same ceiling, opened with the cache the default derives *from*
    // that ceiling. The clamp is what makes this work: 2 MiB would be refused as
    // a declaration, so a default that was not clamped would refuse every open
    // under a tightened ceiling.
    let derived = limits.default_matrix_metadata_cache_bytes();
    assert_eq!(derived, ceiling, "the derived cache was not clamped");
    MatrixRecoveryReport::reset_matrix_integrity_counters();
    let reader = spec_with(
        limits.with_matrix_metadata_residency(MatrixMetadataResidency::Lazy {
            cache_bytes: derived,
        }),
    )
    .open_readonly(&path)?;
    for page in 0..LIVE {
        assert_eq!(
            reader.matrix_cell_status::<LazyCell>(key(page * CELLS_PER_PAGE))?,
            MatrixCellStatus::Committed,
            "page {page} of a matrix an eager open could not have opened"
        );
    }
    let cached = MatrixRecoveryReport::matrix_lazy_cached_bitmap_bytes();
    println!("(1.3) derived-cache open under the same ceiling: cached={cached} ceiling={ceiling}");
    assert!(
        cached <= ceiling,
        "the derived cache ({cached} B) exceeded the ceiling it was clamped to"
    );
    Ok(())
}

/// Defect 2: a call-time `ReadLimits` that never declared a residency policy
/// must not overwrite the one the format declared.
///
/// `*_with_resource_limits` composes as `format.resolve().overlay(call_time)`,
/// and `overlay` took the call-time residency unconditionally. Since
/// `ReadLimits::STANDARD` carried a concrete policy, every call that raised some
/// unrelated ceiling reverted a spec-declared `Lazy` to the eager one - silently
/// removing the only memory bound the caller had, and then letting the open be
/// refused by the very admission limit they were raising. Both fields are unset
/// (`Missing`) in every preset now, and both compose the same way, which is why
/// this test covers the verification policy alongside the residency one.
#[test]
fn a_resource_limits_overlay_keeps_a_spec_declared_residency() -> varve::Result<()> {
    let cache = 4 * PAGE_BYTES;
    let declared = MatrixMetadataResidency::Lazy { cache_bytes: cache };

    // Composition, stated without any I/O.
    let format = ReadLimits::STANDARD.with_matrix_metadata_residency(declared);
    let call_time = ReadLimits::STANDARD.with_max_matrix_cells(32_000_000);
    assert_eq!(
        format
            .resolve()
            .overlay(call_time)
            .matrix_metadata_residency,
        declared,
        "an overlay that declared no policy overwrote the format's"
    );
    assert_eq!(
        format
            .resolve()
            .tighten(call_time)
            .matrix_metadata_residency,
        declared,
        "a tighten that declared no policy overwrote the format's"
    );
    // A call-time declaration still wins, and still wins over a format one.
    let smaller = MatrixMetadataResidency::Lazy {
        cache_bytes: PAGE_BYTES,
    };
    assert_eq!(
        format
            .resolve()
            .overlay(call_time.with_matrix_metadata_residency(smaller))
            .matrix_metadata_residency,
        smaller,
        "a call-time declaration was ignored"
    );
    // `tighten` fills in a silence but never overrides a declaration.
    assert_eq!(
        ReadLimits::STANDARD
            .tighten(call_time.with_matrix_metadata_residency(declared))
            .matrix_metadata_residency,
        declared,
        "a runtime declaration was dropped even though the format declared none"
    );

    // The same statement as behaviour: the spec declares a 4-page cache and a
    // 4-page bitmap ceiling, the caller raises the ceiling to 8 pages, and the
    // matrix has 16 live pages. Under the reverted policy this open was refused by
    // the very limit the caller raised.
    const LIVE: u64 = 16;
    let dir = temp_dir("overlay");
    let path = dir.path().join("overlay.varve");
    let pages: Vec<u64> = (0..LIVE).collect();
    fill_pages(&path, LARGE_WIDE_SCANS, &pages)?;

    let spec = spec_with(
        ReadLimits::STANDARD
            .with_max_matrix_bitmap_bytes(cache)
            .with_matrix_metadata_residency(declared)
            // Declared too, so that "no page was visited" is a statement about the
            // residency declaration surviving rather than about verification.
            .with_matrix_metadata_verification(MatrixMetadataVerification::OnDemand),
    );
    MatrixRecoveryReport::reset_matrix_integrity_counters();
    let reader = spec.open_reader_with_resource_limits(
        &path,
        ReadLimits::STANDARD.with_max_matrix_bitmap_bytes(8 * PAGE_BYTES),
    )?;
    assert_eq!(
        MatrixRecoveryReport::matrix_open_bitmap_pages_visited(),
        0,
        "the overlay dropped one of the spec's two declarations"
    );
    for page in 0..LIVE {
        assert_eq!(
            reader.matrix_cell_status::<LazyCell>(key(page * CELLS_PER_PAGE))?,
            MatrixCellStatus::Committed
        );
    }
    assert!(MatrixRecoveryReport::matrix_lazy_cached_bitmap_bytes() <= cache);
    drop(reader);

    // The other direction, and the successor of the arm that asserted this for
    // `EagerVerified`: a spec that declared verification *at* open must keep it
    // across an overlay that declares nothing.
    let verified = spec_with(
        ReadLimits::STANDARD.with_matrix_metadata_verification(MatrixMetadataVerification::AtOpen),
    );
    MatrixRecoveryReport::reset_matrix_integrity_counters();
    drop(verified.open_reader_with_resource_limits(
        &path,
        ReadLimits::STANDARD.with_max_matrix_cells(32_000_000),
    )?);
    assert!(
        MatrixRecoveryReport::matrix_open_bitmap_pages_visited() > 0,
        "the overlay dropped the spec's `AtOpen` declaration"
    );
    Ok(())
}

// --- fixture plumbing -------------------------------------------------------

/// Native file header, then the 24-byte matrix creation-nonce region.
const NATIVE_PREFIX_LEN: u64 = 18 + 24;

fn vmat_header_offset() -> u64 {
    NATIVE_PREFIX_LEN + u64::try_from(verified_spec().magic.len()).expect("magic length")
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
