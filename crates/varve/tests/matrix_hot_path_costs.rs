//! Per-operation cost of the matrix cell path, measured with the repository's
//! own counters rather than with wall-clock time.
//!
//! Two costs are pinned here, and both used to follow something other than the
//! work asked for.
//!
//! 1. **The demand cache's recency update.** Every cell read reaches it through
//!    `SparseBitmap::byte`, and so does every cell write, because the mutation
//!    engine reads the byte it is about to change first. It used to be a linear
//!    scan of a `VecDeque` plus a memmove, so its cost followed the *cache
//!    size* — 512 pages by default, 16,384 at the ceiling a 64 MiB
//!    `max_matrix_bitmap_bytes` admits — and not the working set.
//!    [`MatrixRecoveryReport::matrix_lru_touch_steps`] counts map probes.
//!
//! 2. **Bitmap bytes stored back unchanged.** `prepare_bitmap_update` reads the
//!    current byte and computes the new one and never compared them, so the
//!    first write of any matrix cell paid a `seek` and a one-byte `write_all`
//!    to clear a commit bit that was already clear — and a second pair for the
//!    validity bit under a checksum policy.
//!    [`MatrixRecoveryReport::matrix_bitmap_byte_writes`] counts the pairs that
//!    are actually issued.
//!
//! The fixtures are sparse: payload, checksum and bitmap extents are zero
//! extents, so a nominally multi-megabyte matrix costs a few pages on disk.
#![cfg(all(feature = "integrity", feature = "scalable-fault-injection"))]

use std::path::Path;

use varve::{
    BlockDescriptor, BlockKind, Endian, FormatSpec, IndexPolicy, IntegrityPolicy,
    MatrixBlockDescriptor, MatrixCellStatus, MatrixCommitDescriptor, MatrixCommitKind,
    MatrixDimensionDescriptor, MatrixDimensions, MatrixKey, MatrixMetadataResidency,
    MatrixMetadataVerification, MatrixRecoveryReport, ReadLimits, VarveBlock, VarveMatrixBlock,
};

const PAGE_BYTES: u64 = 4096;
/// Bits per commit-map page, i.e. cells per page.
const CELLS_PER_PAGE: u64 = PAGE_BYTES * 8;
const CHANNELS: u64 = 128;

/// Live commit-map pages in the recency fixture. Large enough that a linear
/// scan of the cache and a bounded probe of it are an order of magnitude apart,
/// small enough that building it is 128 cell writes.
const LIVE_PAGES: u64 = 128;
/// Scans that give `LIVE_PAGES` whole commit-map pages of address space.
const RECENCY_SCANS: u64 = LIVE_PAGES * CELLS_PER_PAGE / CHANNELS;

#[derive(Clone, Debug, PartialEq, Eq, VarveBlock)]
#[varve(id = 764, version = 1, kind = "matrix")]
struct HotCell {
    value: u32,
}

impl VarveMatrixBlock for HotCell {
    const DIMENSIONS: [&'static str; 2] = ["scan", "ch"];
    const CATEGORY: &'static str = "analysis";
    const SLOT_STRIDE: u64 = 4;
}

fn spec_with(integrity: IntegrityPolicy, limits: ReadLimits) -> FormatSpec {
    static BLOCKS: &[BlockDescriptor] = &[BlockDescriptor {
        id: HotCell::ID,
        name: "HotCell",
        version: HotCell::VERSION,
        kind: BlockKind::Matrix,
        fields: HotCell::FIELDS,
    }];
    static DIMENSIONS: &[MatrixDimensionDescriptor] = &[
        MatrixDimensionDescriptor { name: "scan" },
        MatrixDimensionDescriptor { name: "ch" },
    ];
    static COMMITS: &[MatrixCommitDescriptor] = &[MatrixCommitDescriptor {
        name: HotCell::CATEGORY,
        kind: MatrixCommitKind::Cell,
    }];
    static MATRIX_BLOCKS: &[MatrixBlockDescriptor] = &[MatrixBlockDescriptor {
        block_id: HotCell::ID,
        dimensions: HotCell::DIMENSIONS,
        category: HotCell::CATEGORY,
        slot_stride: HotCell::SLOT_STRIDE,
    }];

    FormatSpec::new(
        b"MHOT",
        1,
        Endian::Little,
        0,
        IndexPolicy::ScanOnOpen,
        integrity,
        varve::RecoveryPolicy::Strict,
        varve::ManifestPolicy::None,
        BLOCKS,
    )
    .with_read_limits(limits)
    .with_matrix_spec(DIMENSIONS, COMMITS, MATRIX_BLOCKS)
}

/// What a caller who has never heard of either matrix option gets.
fn default_spec(integrity: IntegrityPolicy) -> FormatSpec {
    spec_with(integrity, ReadLimits::STANDARD)
}

/// A declared demand cache, with verification moved off the open so the
/// measurement is about the cache and not about the verification pass.
fn lazy_spec(cache_bytes: u64) -> FormatSpec {
    spec_with(
        IntegrityPolicy::Crc32,
        ReadLimits::STANDARD
            .with_matrix_metadata_residency(MatrixMetadataResidency::Lazy { cache_bytes })
            .with_matrix_metadata_verification(MatrixMetadataVerification::OnDemand),
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
fn fill_pages(path: &Path, scans: u64, pages: impl IntoIterator<Item = u64>) -> varve::Result<()> {
    let mut writer =
        default_spec(IntegrityPolicy::Crc32).create_writer_with_dims(path, dims(scans))?;
    for page in pages {
        let ordinal = page * CELLS_PER_PAGE;
        writer.write_matrix_cell(key(ordinal), &HotCell { value: 7 })?;
        writer.commit_matrix_cell::<HotCell>(key(ordinal))?;
    }
    writer.flush()?;
    Ok(())
}

fn temp_dir(name: &str) -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix(&format!("varve-matrix-hot-{name}-"))
        .tempdir()
        .expect("temp dir")
}

// The recency update ---------------------------------------------------------

/// **The cost.** Reading cells out of a full demand cache costs a bounded
/// number of recency probes per read, whatever the cache holds.
///
/// Every page is resident before the measurement starts, so no read below
/// faults, and the only work each one performs beyond the lookup is the recency
/// update.
///
/// **The access order is load-bearing and it is not round-robin.** A strict
/// sweep `0, 1, 2, …` is the *cheapest* order for a linear scan, not the
/// dearest: the deque was kept least-recently-used first, and a sweep always
/// asks for the page that was used longest ago, which sits at the front. It
/// finds every page in one comparison and hides the defect completely
/// (measured: 2.00 probes per read against the unfixed code). So the order here
/// is one hot page alternating with a sweep of the rest, which is both an
/// ordinary shape — an index page consulted between scans — and one where the
/// hot page is always near the *far* end of that scan.
///
/// The bound is eight probes. `note_used` spends at most six: one to find the
/// page, three to unlink it from between its neighbours, two to relink it at
/// the tail — and none at all when the page is already the most recently used.
/// A linear scan over this 128-page cache averages about 63 comparisons per
/// touch under the same access order.
#[test]
fn a_cell_read_spends_a_bounded_number_of_recency_probes() -> varve::Result<()> {
    const READS: u64 = 4_096;
    let dir = temp_dir("recency-cost");
    let path = dir.path().join("matrix.varve");
    fill_pages(&path, RECENCY_SCANS, 0..LIVE_PAGES)?;

    // A cache that holds every live page, so the reads below all hit.
    let reader = lazy_spec(LIVE_PAGES * PAGE_BYTES).open_readonly(&path)?;
    for page in 0..LIVE_PAGES {
        assert_eq!(
            reader.matrix_cell_status::<HotCell>(key(page * CELLS_PER_PAGE))?,
            MatrixCellStatus::Committed
        );
    }
    assert_eq!(
        MatrixRecoveryReport::matrix_lazy_cached_bitmap_bytes(),
        LIVE_PAGES * PAGE_BYTES,
        "the fixture did not warm the whole cache, so this measures faulting and not recency"
    );

    MatrixRecoveryReport::reset_matrix_integrity_counters();
    for read in 0..READS {
        // Hot page, cold page, hot page, cold page…
        let page = if read % 2 == 0 {
            0
        } else {
            1 + (read / 2) % (LIVE_PAGES - 1)
        };
        assert_eq!(
            reader.matrix_cell_status::<HotCell>(key(page * CELLS_PER_PAGE))?,
            MatrixCellStatus::Committed
        );
    }
    let steps = MatrixRecoveryReport::matrix_lru_touch_steps();
    let faulted = MatrixRecoveryReport::matrix_lazy_fault_bytes_read();
    println!(
        "(recency) reads={READS} cached_pages={LIVE_PAGES} probes={steps} \
         probes_per_read={:.2} fault_bytes={faulted}",
        steps as f64 / READS as f64
    );
    assert_eq!(
        faulted, 0,
        "the reads faulted, so the probe count is not a measurement of cache hits"
    );
    assert!(
        steps <= 8 * READS,
        "{READS} reads across a {LIVE_PAGES}-page cache spent {steps} recency probes; the \
         bound is {}. A count that follows the cache size is the linear scan back.",
        8 * READS
    );
    Ok(())
}

/// **The behaviour.** The cheapest way to satisfy the bound above — stop
/// tracking recency at all — is excluded, because a re-read really does keep a
/// page resident.
///
/// Eight cache pages against nine live ones. Reading pages `0..8` fills the
/// cache; re-reading page 0 makes page 1 the least recently used; reading page
/// 8 then evicts page 1 and not page 0. Page 0 must still answer without
/// touching the file.
#[test]
fn a_re_read_keeps_a_page_resident_that_arrival_order_would_have_evicted() -> varve::Result<()> {
    const CACHE_PAGES: u64 = 8;
    let dir = temp_dir("recency-behaviour");
    let path = dir.path().join("matrix.varve");
    fill_pages(&path, RECENCY_SCANS, 0..=CACHE_PAGES)?;

    let reader = lazy_spec(CACHE_PAGES * PAGE_BYTES).open_readonly(&path)?;
    let status = |page: u64| reader.matrix_cell_status::<HotCell>(key(page * CELLS_PER_PAGE));
    for page in 0..CACHE_PAGES {
        assert_eq!(status(page)?, MatrixCellStatus::Committed);
    }
    // Page 0 arrived first; this makes page 1 the least recently used.
    assert_eq!(status(0)?, MatrixCellStatus::Committed);
    // A ninth page does not fit, so exactly one page leaves.
    assert_eq!(status(CACHE_PAGES)?, MatrixCellStatus::Committed);

    MatrixRecoveryReport::reset_matrix_integrity_counters();
    assert_eq!(status(0)?, MatrixCellStatus::Committed);
    assert_eq!(
        MatrixRecoveryReport::matrix_lazy_fault_bytes_read(),
        0,
        "the re-read page was evicted anyway, so the recency update is not being made"
    );

    // And the page that arrival order would have kept is the one that went.
    MatrixRecoveryReport::reset_matrix_integrity_counters();
    assert_eq!(status(1)?, MatrixCellStatus::Committed);
    assert!(
        MatrixRecoveryReport::matrix_lazy_fault_bytes_read() > 0,
        "the least recently used page was still resident, so nothing was evicted and this \
         fixture proves nothing"
    );
    Ok(())
}

// Bitmap bytes stored back unchanged -----------------------------------------

/// A matrix cell write does not store a bitmap byte back over the value it
/// already holds.
///
/// Both halves matter. The first is the saving: writing a cell that was never
/// written clears a commit bit that is already clear, and under a checksum
/// policy a validity bit that is already clear, so *no* bitmap byte moves. The
/// second is the discriminator against a short-circuit keyed on the wrong
/// condition: committing those same cells performs real `0 -> 1` transitions,
/// one per cell under `IntegrityPolicy::None` and two under `Crc32` — the
/// validity bit and the commit bit, both through the single `write_bitmap_byte`
/// site — and every one of them must still be issued.
#[test]
fn writing_a_fresh_cell_stores_no_bitmap_byte_and_committing_it_stores_every_one()
-> varve::Result<()> {
    const CELLS: u64 = 100;
    let dir = temp_dir("redundant-writes");

    for (integrity, bytes_per_commit) in [(IntegrityPolicy::None, 1), (IntegrityPolicy::Crc32, 2)] {
        let path = dir.path().join(format!("{integrity:?}.varve"));
        let mut writer = default_spec(integrity).create_writer_with_dims(&path, dims(8))?;

        MatrixRecoveryReport::reset_matrix_integrity_counters();
        for ordinal in 0..CELLS {
            writer.write_matrix_cell(
                key(ordinal),
                &HotCell {
                    value: u32::try_from(ordinal + 1).expect("ordinal fits u32"),
                },
            )?;
        }
        let on_write = MatrixRecoveryReport::matrix_bitmap_byte_writes();

        MatrixRecoveryReport::reset_matrix_integrity_counters();
        for ordinal in 0..CELLS {
            writer.commit_matrix_cell::<HotCell>(key(ordinal))?;
        }
        let on_commit = MatrixRecoveryReport::matrix_bitmap_byte_writes();
        println!(
            "(bitmap bytes) {integrity:?}: writes={on_write} commits={on_commit} for {CELLS} cells"
        );

        assert_eq!(
            on_write, 0,
            "{integrity:?}: writing {CELLS} never-written cells stored {on_write} bitmap bytes; \
             every one of them was the value already on disk"
        );
        assert_eq!(
            on_commit,
            CELLS * bytes_per_commit,
            "{integrity:?}: committing {CELLS} cells stored {on_commit} bitmap bytes; a \
             short-circuit that also drops the real transitions reports fewer"
        );

        // The transitions that were skipped changed nothing, and the ones that
        // were made are visible on a fresh open of the file.
        writer.flush()?;
        drop(writer);
        let reader = default_spec(integrity).open_readonly(&path)?;
        for ordinal in 0..CELLS {
            assert_eq!(
                reader.matrix_cell_status::<HotCell>(key(ordinal))?,
                MatrixCellStatus::Committed,
                "{integrity:?}: cell {ordinal} lost its commit bit"
            );
            assert_eq!(
                reader.read_matrix_cell::<HotCell>(key(ordinal))?,
                HotCell {
                    value: u32::try_from(ordinal + 1).expect("ordinal fits u32")
                }
            );
        }
    }
    Ok(())
}

/// **The cost.** A bitmap byte that does not change hashes no page.
///
/// The skip that landed with `write_bitmap_byte` saved the cheapest half of a
/// redundant mutation — one `seek` and one byte — and left the dear half in
/// place. Under a checksum policy the same no-op mutation still read a whole
/// 4096-byte page, ran CRC-32 over all of it, and wrote the 8-byte digest, so a
/// first cell write cost a page hash to store a commit bit that was already
/// clear. The write phase below is exactly that case: the companion test pins
/// its bitmap-byte stores at zero, which is what makes every one of these
/// mutations a no-op.
///
/// The digest skip is safe for the reason the byte skip is. The page keeps the
/// bytes its recorded digest was taken over, so the digest still describes it —
/// and a page that was never written stays a hole, reads back as zero, and
/// authenticates against `PAGE_STATE_UNINITIALIZED`, which is what the state
/// asserts. Writing `PAGE_STATE_INITIALIZED` over it, as this used to, spent an
/// I/O to record a checksum of zeros for a page nobody had touched.
#[test]
fn a_bitmap_byte_that_does_not_change_hashes_no_page() -> varve::Result<()> {
    const CELLS: u64 = 100;
    const SCANS: u64 = 8;
    /// The whole commit map here is one partial page: `SCANS * CHANNELS` cells,
    /// one bit each. A real transition rehashes that page and nothing wider,
    /// which is the `BITMAP_PAGE_BYTES` bound stated at its narrow end.
    const MAP_BYTES: u64 = SCANS * CHANNELS / 8;
    let dir = temp_dir("redundant-hashes");
    let path = dir.path().join("matrix.varve");
    let mut writer =
        default_spec(IntegrityPolicy::Crc32).create_writer_with_dims(&path, dims(SCANS))?;

    MatrixRecoveryReport::reset_matrix_integrity_counters();
    for ordinal in 0..CELLS {
        writer.write_matrix_cell(
            key(ordinal),
            &HotCell {
                value: u32::try_from(ordinal + 1).expect("ordinal fits u32"),
            },
        )?;
    }
    let on_write = MatrixRecoveryReport::matrix_bitmap_bytes_hashed();
    let stored = MatrixRecoveryReport::matrix_bitmap_byte_writes();

    MatrixRecoveryReport::reset_matrix_integrity_counters();
    for ordinal in 0..CELLS {
        writer.commit_matrix_cell::<HotCell>(key(ordinal))?;
    }
    let on_commit = MatrixRecoveryReport::matrix_bitmap_bytes_hashed();
    println!(
        "(bitmap hashing) {CELLS} cells: writes hashed {on_write} bytes, commits hashed \
         {on_commit}"
    );

    assert_eq!(
        stored, 0,
        "the fixture stored {stored} bitmap bytes, so these were not the no-op mutations \
         this test is about"
    );
    assert_eq!(
        on_write, 0,
        "writing {CELLS} never-written cells hashed {on_write} bitmap bytes for mutations \
         that changed nothing"
    );
    assert_eq!(
        on_commit,
        CELLS * MAP_BYTES,
        "committing {CELLS} cells hashed {on_commit} bytes; each is one real transition and \
         must still rehash its page, so a skip keyed on anything but the byte reports fewer"
    );

    // The commits are durable, so the skipped hashing was not skipped work.
    writer.flush()?;
    drop(writer);
    let reader = default_spec(IntegrityPolicy::Crc32).open_readonly(&path)?;
    for ordinal in 0..CELLS {
        assert_eq!(
            reader.matrix_cell_status::<HotCell>(key(ordinal))?,
            MatrixCellStatus::Committed,
            "cell {ordinal} lost its commit bit"
        );
    }
    Ok(())
}

/// **The cost.** What committing a cell reads back off the disk, pinned.
///
/// `commit_cell` has two reasons to re-read the slot it is committing:
///
/// 1. the **zero probe**, which separates "never written" from "written zeros"
///    — skipped entirely for a cell this session wrote, on the session
///    write-tracking bit; and
/// 2. the **checksum pass**, which is how the cell's CRC is recorded.
///
/// They used to be two independent seek-and-stream passes over the same bytes,
/// so a commit of a cell this session did not write read the slot back twice.
/// `scan_slot` answers both in one pass — it hashes and tracks all-zero over
/// the same buffer — so both cases now read the slot exactly once. Measured
/// before the fusion on this fixture: 256 bytes for the other-session case
/// against 128 now.
///
/// The refusal still happens before any commit state is prepared, so a slot
/// about to be refused never has a CRC written for it — the ordering the
/// separate probe used to give for free.
///
/// What remains, and is not this: `write_cell` holds the payload and could hand
/// its checksum forward, so a cell this session wrote need not be read back at
/// all. That is the 128 below, and it is still there.
///
/// The read-back is per *slot*, so it scales with `SLOT_STRIDE` and not with the
/// cell count — which is why the assertions below are written as multiples of
/// the stride.
#[test]
fn committing_a_cell_reads_its_slot_back_once_per_pass_that_needs_it() -> varve::Result<()> {
    const CELLS: u64 = 32;
    const STRIDE: u64 = HotCell::SLOT_STRIDE;
    let dir = temp_dir("slot-read-back");
    let path = dir.path().join("matrix.varve");
    let mut writer =
        default_spec(IntegrityPolicy::Crc32).create_writer_with_dims(&path, dims(8))?;

    MatrixRecoveryReport::reset_matrix_integrity_counters();
    for ordinal in 0..CELLS {
        writer.write_matrix_cell(key(ordinal), &HotCell { value: 7 })?;
    }
    let on_write = MatrixRecoveryReport::matrix_slot_bytes_read_back();

    MatrixRecoveryReport::reset_matrix_integrity_counters();
    for ordinal in 0..CELLS {
        writer.commit_matrix_cell::<HotCell>(key(ordinal))?;
    }
    let same_session = MatrixRecoveryReport::matrix_slot_bytes_read_back();
    writer.flush()?;
    drop(writer);

    // A fresh handle has no session write bits, so the zero probe runs too.
    let mut reopened = default_spec(IntegrityPolicy::Crc32).open_writer(&path)?;
    MatrixRecoveryReport::reset_matrix_integrity_counters();
    for ordinal in 0..CELLS {
        reopened.commit_matrix_cell::<HotCell>(key(ordinal))?;
    }
    let other_session = MatrixRecoveryReport::matrix_slot_bytes_read_back();
    println!(
        "(slot read-back) {CELLS} cells of {STRIDE} bytes: writes={on_write} \
         commit_same_session={same_session} commit_other_session={other_session}"
    );

    assert_eq!(
        on_write, 0,
        "writing a cell read {on_write} slot bytes back; the payload is in hand there"
    );
    assert_eq!(
        same_session,
        CELLS * STRIDE,
        "a cell this session wrote was read back {same_session} bytes; the zero probe is \
         skipped on the session write bit, so this must be the checksum pass alone"
    );
    assert_eq!(
        other_session, same_session,
        "a cell this session did not write was read back {other_session} bytes against \
         {same_session} for one it did; the zero probe and the checksum pass read the same \
         slot and must do it in one pass, not two"
    );
    assert_eq!(
        other_session,
        CELLS * STRIDE,
        "a cell this session did not write was read back {other_session} bytes; one pass over \
         {CELLS} slots of {STRIDE} bytes is {}, and it was twice that before the fusion",
        CELLS * STRIDE
    );
    Ok(())
}

/// Rewriting an already-committed cell is a real `1 -> 0` transition, so the
/// skip must not swallow it.
///
/// This is the case the first test cannot see: there the bit was already clear,
/// here it is set, and a fix keyed on "is this a write path?" rather than on
/// "does this byte change?" would drop the clear and leave the cell reporting
/// `Committed` with a payload that no longer matches its checksum.
#[test]
fn rewriting_a_committed_cell_still_clears_its_commit_bit() -> varve::Result<()> {
    const CELLS: u64 = 16;
    let dir = temp_dir("rewrite");
    let path = dir.path().join("matrix.varve");
    let mut writer =
        default_spec(IntegrityPolicy::Crc32).create_writer_with_dims(&path, dims(8))?;
    for ordinal in 0..CELLS {
        writer.write_matrix_cell(key(ordinal), &HotCell { value: 1 })?;
        writer.commit_matrix_cell::<HotCell>(key(ordinal))?;
    }

    MatrixRecoveryReport::reset_matrix_integrity_counters();
    for ordinal in 0..CELLS {
        writer.write_matrix_cell(key(ordinal), &HotCell { value: 2 })?;
    }
    let rewritten = MatrixRecoveryReport::matrix_bitmap_byte_writes();
    println!("(bitmap bytes) rewrite of {CELLS} committed cells: {rewritten} stores");
    assert_eq!(
        rewritten,
        CELLS * 2,
        "rewriting a committed cell must clear both its commit bit and its validity bit"
    );

    for ordinal in 0..CELLS {
        assert_eq!(
            writer.matrix_cell_status::<HotCell>(key(ordinal))?,
            MatrixCellStatus::NotCommitted,
            "cell {ordinal} kept a commit bit the rewrite should have cleared"
        );
    }
    writer.flush()?;
    drop(writer);

    let reader = default_spec(IntegrityPolicy::Crc32).open_readonly(&path)?;
    for ordinal in 0..CELLS {
        assert_eq!(
            reader.matrix_cell_status::<HotCell>(key(ordinal))?,
            MatrixCellStatus::NotCommitted,
            "cell {ordinal}'s cleared commit bit did not reach the file"
        );
    }
    Ok(())
}
