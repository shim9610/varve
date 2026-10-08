//! Matrix readers own private caches and are Send + !Sync. Each reading
//! thread opens its own reader; regular reads remain &self. Tests cover
//! concurrent correctness, cache eviction without I/O under a cache borrow,
//! and separately reported throughput measurements.

use std::path::{Path, PathBuf};
use std::sync::Barrier;
use std::sync::atomic::{AtomicU64, Ordering};

use varve::{
    BlockDescriptor, BlockKind, Endian, FormatSpec, IndexPolicy, MatrixAuxDescriptor,
    MatrixBlockDescriptor, MatrixCommitDescriptor, MatrixCommitKind, MatrixDimensionDescriptor,
    MatrixDimensions, MatrixKey, ReadLimits, VarveBlock, VarveMatrixBlock,
};

static NEXT_TEMP_ID: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Debug, PartialEq, Eq, VarveBlock)]
#[varve(id = 780, version = 1, kind = "matrix")]
struct SharedCell {
    value: u32,
}

impl VarveMatrixBlock for SharedCell {
    const DIMENSIONS: [&'static str; 2] = ["scan", "ch"];
    const CATEGORY: &'static str = "analysis";
    const SLOT_STRIDE: u64 = 4;
}

fn spec(integrity: varve::IntegrityPolicy) -> FormatSpec {
    static BLOCKS: &[BlockDescriptor] = &[BlockDescriptor {
        id: SharedCell::ID,
        name: "SharedCell",
        version: SharedCell::VERSION,
        kind: BlockKind::Matrix,
        fields: SharedCell::FIELDS,
    }];
    static DIMENSIONS: &[MatrixDimensionDescriptor] = &[
        MatrixDimensionDescriptor { name: "scan" },
        MatrixDimensionDescriptor { name: "ch" },
    ];
    static COMMITS: &[MatrixCommitDescriptor] = &[MatrixCommitDescriptor {
        name: SharedCell::CATEGORY,
        kind: MatrixCommitKind::Cell,
    }];
    static MATRIX_BLOCKS: &[MatrixBlockDescriptor] = &[MatrixBlockDescriptor {
        block_id: SharedCell::ID,
        dimensions: SharedCell::DIMENSIONS,
        category: SharedCell::CATEGORY,
        slot_stride: SharedCell::SLOT_STRIDE,
    }];
    static AUX: &[MatrixAuxDescriptor] = &[MatrixAuxDescriptor {
        name: "thumbnail",
        byte_len: 32,
    }];

    FormatSpec::new(
        b"MCON",
        1,
        Endian::Little,
        0,
        IndexPolicy::ScanOnOpen,
        integrity,
        varve::RecoveryPolicy::Strict,
        varve::ManifestPolicy::None,
        BLOCKS,
    )
    .with_block_identities(&[SharedCell::IDENTITY])
    .with_matrix_spec(DIMENSIONS, COMMITS, MATRIX_BLOCKS)
    .with_matrix_aux(AUX)
    .with_read_limits(ReadLimits::finite_all(u64::MAX))
}

struct TempMatrix {
    path: PathBuf,
    _dir: tempfile::TempDir,
}

impl TempMatrix {
    fn new(name: &str) -> Self {
        let id = NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed);
        let dir = tempfile::tempdir().expect("create per-test temp directory");
        let path = dir
            .path()
            .join(format!("varve-{name}-{}-{id}.vrv", std::process::id()));
        Self { path, _dir: dir }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

const SIDE: u64 = 16;

fn cell_value(scan: u64, ch: u64) -> u32 {
    (scan * SIDE + ch) as u32 + 1
}

/// A `SIDE x SIDE` matrix with every cell written and committed.
fn populated(fixture: &TempMatrix, integrity: varve::IntegrityPolicy) -> varve::Result<FormatSpec> {
    let spec = spec(integrity);
    let dimensions = MatrixDimensions::from_pairs([("scan", SIDE), ("ch", SIDE)]);
    let mut writer = spec.create_with_dims(fixture.path(), dimensions)?;
    for scan in 0..SIDE {
        for ch in 0..SIDE {
            let key = MatrixKey::new(scan, ch);
            writer.write_matrix_cell(
                key,
                &SharedCell {
                    value: cell_value(scan, ch),
                },
            )?;
            writer.commit_matrix_cell::<SharedCell>(key)?;
        }
    }
    writer.write_matrix_aux("thumbnail", 0, &[7u8; 32])?;
    writer.sync()?;
    drop(writer);
    Ok(spec)
}

/// Criterion (C), part 1: the handle crosses a thread boundary by shared
/// reference.
///
/// Ownership can move to a thread. Cross-thread sharing is covered by the
/// compile-fail fixtures in compile.rs.
#[test]
fn the_reader_and_writer_handles_are_send() {
    fn assert_send<T: Send>() {}
    assert_send::<varve::VarveReader>();
    assert_send::<varve::VarveFile>();
    assert_send::<varve::VarveWriter>();
}

/// Criterion (C), part 2: the read entry points compile against a shared
/// borrow.
///
/// This would not compile at all before this round. It is written as a
/// free-standing function taking `&VarveReader` precisely so that reintroducing
/// `&mut self` on any of these entry points is a build failure, not a
/// silently-accepted regression.
fn read_everything(reader: &varve::VarveReader, key: MatrixKey) -> varve::Result<u32> {
    let typed = reader.read_matrix_cell::<SharedCell>(key)?;
    let payload = reader.matrix_cell_payload::<SharedCell>(key)?;
    let aux = reader.read_matrix_aux("thumbnail", 0, 8)?;
    let aux_len = reader.matrix_aux_len("thumbnail")?;
    let status = reader.matrix_cell_status::<SharedCell>(key)?;
    assert_eq!(payload.len(), 4);
    assert_eq!(aux, vec![7u8; 8]);
    assert_eq!(aux_len, 32);
    assert_eq!(status, varve::MatrixCellStatus::Committed);
    Ok(typed.value)
}

/// The same five entry points on the *writer*, which is the handle no test
/// pinned before: a `&mut self` reintroduced there would not be caught by the
/// reader-only function above.
fn read_everything_through_writer(
    writer: &varve::VarveWriter,
    key: MatrixKey,
) -> varve::Result<u32> {
    let typed = writer.read_matrix_cell::<SharedCell>(key)?;
    let payload = writer.matrix_cell_payload::<SharedCell>(key)?;
    let aux = writer.read_matrix_aux("thumbnail", 0, 8)?;
    let aux_len = writer.matrix_aux_len("thumbnail")?;
    let status = writer.matrix_cell_status::<SharedCell>(key)?;
    assert_eq!(payload.len(), 4);
    assert_eq!(aux, vec![7u8; 8]);
    assert_eq!(aux_len, 32);
    assert_eq!(status, varve::MatrixCellStatus::Committed);
    Ok(typed.value)
}

/// And on the general handle.
fn read_everything_through_file(file: &varve::VarveFile, key: MatrixKey) -> varve::Result<u32> {
    let typed = file.read_matrix_cell::<SharedCell>(key)?;
    let payload = file.matrix_cell_payload::<SharedCell>(key)?;
    let aux = file.read_matrix_aux("thumbnail", 0, 8)?;
    let aux_len = file.matrix_aux_len("thumbnail")?;
    let status = file.matrix_cell_status::<SharedCell>(key)?;
    assert_eq!(payload.len(), 4);
    assert_eq!(aux, vec![7u8; 8]);
    assert_eq!(aux_len, 32);
    assert_eq!(status, varve::MatrixCellStatus::Committed);
    Ok(typed.value)
}

#[test]
fn every_matrix_read_entry_point_takes_a_shared_borrow() -> varve::Result<()> {
    let fixture = TempMatrix::new("shared-borrow");
    let spec = populated(&fixture, varve::IntegrityPolicy::None)?;
    let reader = spec.open_reader(fixture.path())?;
    // Note: `reader` is not `mut`. Two shared borrows are alive at once.
    let first = &reader;
    let second = &reader;
    assert_eq!(
        read_everything(first, MatrixKey::new(0, 0))?,
        cell_value(0, 0)
    );
    assert_eq!(
        read_everything(second, MatrixKey::new(3, 5))?,
        cell_value(3, 5)
    );
    drop(reader);

    // 5 entry points x 3 handle types = 15 signatures, all `&self`.
    let file = spec.open_readonly(fixture.path())?;
    assert_eq!(
        read_everything_through_file(&file, MatrixKey::new(1, 1))?,
        cell_value(1, 1)
    );
    drop(file);

    let writer = spec.open_writer(fixture.path())?;
    assert_eq!(
        read_everything_through_writer(&writer, MatrixKey::new(2, 4))?,
        cell_value(2, 4)
    );
    Ok(())
}

/// Each thread owns a reader and verifies every cell at interleaved offsets.
#[test]
fn independent_readers_read_every_cell_correctly() -> varve::Result<()> {
    // `Crc32` is only constructible into a working spec when the `integrity`
    // feature is on; without it every checksummed read fails closed with
    // `IntegrityFeatureDisabled`, so the default-feature run covers `None` only.
    let policies: &[varve::IntegrityPolicy] = if cfg!(feature = "integrity") {
        &[varve::IntegrityPolicy::None, varve::IntegrityPolicy::Crc32]
    } else {
        &[varve::IntegrityPolicy::None]
    };
    for &integrity in policies {
        let fixture = TempMatrix::new("concurrent-cells");
        let spec = populated(&fixture, integrity)?;

        const THREADS: usize = 4;
        let barrier = Barrier::new(THREADS);
        let barrier = &barrier;

        std::thread::scope(|scope| {
            for thread in 0..THREADS {
                let reader = spec
                    .open_reader(fixture.path())
                    .expect("independent reader");
                scope.spawn(move || {
                    barrier.wait();
                    // Each thread walks the matrix from a different starting
                    // offset so no two threads are ever addressing the same
                    // slot at the same moment.
                    for step in 0..(SIDE * SIDE) {
                        let ordinal = (step + (thread as u64) * 7) % (SIDE * SIDE);
                        let (scan, ch) = (ordinal / SIDE, ordinal % SIDE);
                        let key = MatrixKey::new(scan, ch);
                        let cell = reader
                            .read_matrix_cell::<SharedCell>(key)
                            .expect("concurrent typed read");
                        assert_eq!(
                            cell.value,
                            cell_value(scan, ch),
                            "thread {thread} read the wrong slot for ({scan},{ch}); \
                             a shared file cursor would produce exactly this"
                        );
                        let payload = reader
                            .matrix_cell_payload::<SharedCell>(key)
                            .expect("concurrent payload read");
                        assert_eq!(
                            u32::from_le_bytes(payload.try_into().expect("4-byte slot")),
                            cell_value(scan, ch)
                        );
                    }
                });
            }
        });
    }
    Ok(())
}

/// Bytes in one commit-map page. Not a public constant, and deliberately not
/// derived from one: the fixture below is built to straddle exactly this
/// boundary, so if the internal page size ever changes the fixture stops being
/// several pages wide and the warm-up assertion says so.
#[cfg(feature = "scalable-fault-injection")]
const PAGE_BYTES: u64 = 4096;

/// Cells per commit-map page: the map holds one bit per cell.
#[cfg(feature = "scalable-fault-injection")]
const CELLS_PER_PAGE: u64 = PAGE_BYTES * 8;

/// Live commit-map pages in the paged fixture. Four is enough for a one-page
/// cache to miss on essentially every read while keeping the fixture cheap to
/// build: four writes and four commits.
#[cfg(feature = "scalable-fault-injection")]
const LIVE_PAGES: u64 = 4;

/// The first cell of commit-map page `page`, i.e. cell ordinal
/// `page * CELLS_PER_PAGE`, given the fixture's `scan x ch` shape.
#[cfg(feature = "scalable-fault-injection")]
fn page_key(page: u64) -> MatrixKey {
    MatrixKey::new(page, 0)
}

/// A matrix whose commit map is `LIVE_PAGES` pages wide, with exactly one
/// committed cell per page.
///
/// Sparse on disk despite naming `LIVE_PAGES * CELLS_PER_PAGE` cells: the
/// payload, checksum and commit-map extents are established by `set_len` and only
/// the four written slots and the four bitmap bytes are ever touched.
#[cfg(feature = "scalable-fault-injection")]
fn paged_fixture(fixture: &TempMatrix) -> varve::Result<FormatSpec> {
    let spec = spec(varve::IntegrityPolicy::None);
    let dimensions = MatrixDimensions::from_pairs([("scan", LIVE_PAGES), ("ch", CELLS_PER_PAGE)]);
    let mut writer = spec.create_with_dims(fixture.path(), dimensions)?;
    for page in 0..LIVE_PAGES {
        let key = page_key(page);
        writer.write_matrix_cell(
            key,
            &SharedCell {
                value: u32::try_from(page).expect("page fits a u32") + 1,
            },
        )?;
        writer.commit_matrix_cell::<SharedCell>(key)?;
    }
    writer.sync()?;
    drop(writer);
    Ok(spec)
}

/// The fixture's limits with a demand cache of exactly `cache_bytes`.
#[cfg(feature = "scalable-fault-injection")]
fn lazy_limits(cache_bytes: u64) -> ReadLimits {
    ReadLimits::finite_all(u64::MAX)
        .with_matrix_metadata_residency(varve::MatrixMetadataResidency::Lazy { cache_bytes })
}

/// Independent readers fault and evict pages without retaining a mutable
/// local cache borrow across I/O. The public audit counters keep their
/// historical `lock` names; caches no longer contain mutexes.
#[cfg(feature = "scalable-fault-injection")]
#[test]
fn no_read_is_issued_while_a_bitmap_page_store_lock_is_held() -> varve::Result<()> {
    use varve::MatrixRecoveryReport as Report;

    // At least two threads even on a single-core host: unlike a timing ratio,
    // this assertion is about one thread's control flow, so it is meaningful —
    // and a violation is still detected — with no real parallelism at all.
    let threads = std::thread::available_parallelism()
        .map(std::num::NonZeroUsize::get)
        .unwrap_or(1)
        .clamp(2, 4);

    let fixture = TempMatrix::new("no-lock-across-io");
    let spec = paged_fixture(&fixture)?;
    // One page of cache against `LIVE_PAGES` live pages: every read of a page
    // the cache is not holding faults it in, and installing it evicts the last.
    let reader = spec
        .with_read_limits(lazy_limits(PAGE_BYTES))
        .open_readonly(fixture.path())?;

    // Phase 1, single-threaded and therefore deterministic: with a one-page
    // cache, walking `LIVE_PAGES` distinct pages must fault every one of them
    // in. This is what makes phase 2's zero mean something.
    Report::reset_matrix_lock_audit_counters();
    for page in 0..LIVE_PAGES {
        assert_eq!(
            reader.matrix_cell_status::<SharedCell>(page_key(page))?,
            varve::MatrixCellStatus::Committed
        );
    }
    let warm_reads = Report::matrix_region_reads();
    assert!(
        warm_reads >= LIVE_PAGES,
        "the warm-up issued {warm_reads} matrix reads for {LIVE_PAGES} distinct commit-map \
         pages against a one-page cache; the guarded fault-in window was not exercised, so \
         the audit below would prove nothing"
    );
    assert_eq!(
        Report::matrix_region_reads_under_bitmap_lock(),
        0,
        "a single-threaded fault-in read was issued while the page store was locked"
    );

    // Phase 2: the same reads, concurrently, through independent readers. The counters
    // are thread-local, so each worker reports its own and the parent sums —
    // which is also why a worker's numbers cannot be attributed to the wrong
    // thread.
    const READS_PER_THREAD: u64 = 4_000;
    let region_reads = AtomicU64::new(0);
    let reads_under_lock = AtomicU64::new(0);
    let barrier = Barrier::new(threads);
    let barrier = &barrier;
    let (region_reads, reads_under_lock) = (&region_reads, &reads_under_lock);

    std::thread::scope(|scope| {
        for thread in 0..threads {
            let reader = spec
                .with_read_limits(lazy_limits(PAGE_BYTES))
                .open_readonly(fixture.path())
                .expect("independent lazy reader");
            scope.spawn(move || {
                Report::reset_matrix_lock_audit_counters();
                barrier.wait();
                for step in 0..READS_PER_THREAD {
                    // Consecutive steps address different pages, so a thread
                    // cannot ride its own cached page.
                    let page = (step + thread as u64) % LIVE_PAGES;
                    assert_eq!(
                        reader
                            .matrix_cell_status::<SharedCell>(page_key(page))
                            .expect("concurrent status read"),
                        varve::MatrixCellStatus::Committed
                    );
                }
                region_reads.fetch_add(Report::matrix_region_reads(), Ordering::Relaxed);
                reads_under_lock.fetch_add(
                    Report::matrix_region_reads_under_bitmap_lock(),
                    Ordering::Relaxed,
                );
                assert_eq!(
                    Report::matrix_bitmap_store_guards_held(),
                    0,
                    "a page-store guard outlived the read that took it"
                );
            });
        }
    });

    let total_reads = region_reads.load(Ordering::Relaxed);
    let under_lock = reads_under_lock.load(Ordering::Relaxed);
    println!(
        "{threads} threads x {READS_PER_THREAD} status reads through independent handles: \
         {total_reads} matrix reads issued, {under_lock} of them under the page-store lock"
    );
    assert!(
        total_reads >= u64::try_from(threads).expect("thread count"),
        "the concurrent phase issued {total_reads} matrix reads across {threads} threads"
    );
    assert_eq!(
        under_lock, 0,
        "{under_lock} of {total_reads} matrix reads were issued while a commit-map page-store \
         borrow was held across I/O"
    );
    Ok(())
}

/// Fixed-work scaling with one independent reader per thread. Timing is
/// printed rather than asserted on shared CI hardware; every value is checked.
#[test]
fn report_the_scaling_of_independent_handles() -> varve::Result<()> {
    let Some((serial, parallel, threads)) = measure_independent_reader_scaling()? else {
        println!("single-core host: nothing to compare, skipping the measurement");
        return Ok(());
    };
    let ratio = parallel.as_secs_f64() / serial.as_secs_f64();
    println!(
        "{TOTAL_READS} reads through independent handles: 1 thread {:.3}s, {threads} threads {:.3}s \
         (ratio {ratio:.2}x, lower is better; MEASUREMENT ONLY — not a gate, see the module \
         header)",
        serial.as_secs_f64(),
        parallel.as_secs_f64()
    );
    Ok(())
}

/// The strict scaling threshold, for a machine whose cores are actually yours.
///
/// `#[ignore]`d because it measures the host as much as the code: it is the gate
/// that CI could not keep. Run it deliberately —
/// `cargo test -p varve --test matrix_concurrent_reads -- --ignored --nocapture`
/// — on an idle host when changing the matrix read path, and expect well under
/// 1.0x. A number above 1.0x on an idle host means N threads owning separate readers
/// are slower than one thread, which is a convoy worth investigating even though
/// it cannot be distinguished from load on a shared runner.
#[test]
#[ignore = "wall-clock scaling: measures the host, so it is a manual benchmark rather than a gate"]
fn independent_reader_scaling_beats_one_thread_on_an_idle_host() -> varve::Result<()> {
    let Some((serial, parallel, threads)) = measure_independent_reader_scaling()? else {
        println!("single-core host: nothing to compare, skipping");
        return Ok(());
    };
    let ratio = parallel.as_secs_f64() / serial.as_secs_f64();
    println!(
        "{TOTAL_READS} reads through independent handles: 1 thread {:.3}s, {threads} threads {:.3}s \
         (ratio {ratio:.2}x)",
        serial.as_secs_f64(),
        parallel.as_secs_f64()
    );
    assert!(
        ratio <= 1.0,
        "{threads} threads with independent readers took {ratio:.2}x as long as one thread for the \
         same {TOTAL_READS} reads. On an idle host that is a convoy; on a shared runner it may \
         only be the runner, which is why this test is `#[ignore]`d"
    );
    Ok(())
}

/// Total work is held constant; only the number of threads it is split across
/// changes. Anything else compares two different amounts of work.
const TOTAL_READS: u64 = 24_000;

/// `(one thread, N threads, N)`, or `None` on a single-core host.
fn measure_independent_reader_scaling()
-> varve::Result<Option<(std::time::Duration, std::time::Duration, usize)>> {
    let threads = std::thread::available_parallelism()
        .map(std::num::NonZeroUsize::get)
        .unwrap_or(1)
        .min(4);
    if threads < 2 {
        return Ok(None);
    }
    let fixture = TempMatrix::new("scaling");
    let spec = populated(&fixture, varve::IntegrityPolicy::None)?;
    // Best-of-three on each configuration: taking the minimum removes some
    // scheduler noise without inventing statistics.
    let serial = best_of_three(spec, fixture.path(), 1, TOTAL_READS);
    let parallel = best_of_three(spec, fixture.path(), threads, TOTAL_READS);
    Ok(Some((serial, parallel, threads)))
}

/// Wall-clock time for `total_reads` cell reads split across `threads` threads,
/// each owning its reader; the fastest of three attempts.
fn best_of_three(
    spec: FormatSpec,
    path: &Path,
    threads: usize,
    total_reads: u64,
) -> std::time::Duration {
    (0..3)
        .map(|_| timed_reads(spec, path, threads, total_reads))
        .min()
        .expect("three attempts")
}

fn timed_reads(
    spec: FormatSpec,
    path: &Path,
    threads: usize,
    total_reads: u64,
) -> std::time::Duration {
    let per_thread = total_reads / threads as u64;
    let ready = Barrier::new(threads + 1);
    let ready = &ready;
    std::thread::scope(|scope| {
        let workers: Vec<_> = (0..threads)
            .map(|thread| {
                let reader = spec.open_reader(path).expect("independent reader");
                scope.spawn(move || {
                    // Every thread is spawned and warm before the clock starts,
                    // so thread creation is not charged to the parallel run.
                    ready.wait();
                    for step in 0..per_thread {
                        let ordinal = (step + (thread as u64) * 13) % (SIDE * SIDE);
                        let key = MatrixKey::new(ordinal / SIDE, ordinal % SIDE);
                        let cell = reader
                            .read_matrix_cell::<SharedCell>(key)
                            .expect("concurrent read");
                        assert_eq!(cell.value, cell_value(ordinal / SIDE, ordinal % SIDE));
                    }
                })
            })
            .collect();
        ready.wait();
        let start = std::time::Instant::now();
        for worker in workers {
            worker.join().expect("reader thread");
        }
        start.elapsed()
    })
}
