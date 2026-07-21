#![cfg(feature = "high-cardinality-dev")]

use std::alloc::{GlobalAlloc, Layout, System};
use std::num::NonZeroU64;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

use varve::{
    BatchOptions, DiskIndexOptions, ScanCancellationToken, ScanOptions, ScanProgressOptions,
    ScanProgressPhase, disk_index_sidecar_path, varve_format,
};

struct CountingAllocator;

static LIVE_BYTES: AtomicUsize = AtomicUsize::new(0);
static PEAK_BYTES: AtomicUsize = AtomicUsize::new(0);

unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let pointer = unsafe { System.alloc(layout) };
        if !pointer.is_null() {
            record_allocation(layout.size());
        }
        pointer
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        LIVE_BYTES.fetch_sub(layout.size(), Ordering::Relaxed);
        record_thread_delta(-(layout.size() as isize));
        unsafe { System.dealloc(pointer, layout) };
    }

    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let replacement = unsafe { System.realloc(pointer, layout, new_size) };
        if !replacement.is_null() {
            if new_size >= layout.size() {
                record_allocation(new_size - layout.size());
            } else {
                LIVE_BYTES.fetch_sub(layout.size() - new_size, Ordering::Relaxed);
                record_thread_delta(-((layout.size() - new_size) as isize));
            }
        }
        replacement
    }
}

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

std::thread_local! {
    /// Live bytes attributable to *this* thread, for measurements that must
    /// survive `cargo test`'s parallelism: the process-global counters above
    /// see every other test binary thread's traffic as well.
    ///
    /// Memory allocated on one thread and released on another skews this, so
    /// it is only meaningful across a window that allocates and frees on the
    /// same thread - which is what a single-threaded append loop does.
    static THREAD_LIVE_BYTES: std::cell::Cell<isize> = const { std::cell::Cell::new(0) };
}

/// Adds `delta` to this thread's live-byte counter, tolerating a thread whose
/// TLS is already being destroyed.
fn record_thread_delta(delta: isize) {
    let _ = THREAD_LIVE_BYTES.try_with(|live| live.set(live.get() + delta));
}

fn thread_live_bytes() -> isize {
    THREAD_LIVE_BYTES.with(std::cell::Cell::get)
}

fn record_allocation(size: usize) {
    record_thread_delta(size as isize);
    let live = LIVE_BYTES.fetch_add(size, Ordering::Relaxed) + size;
    let mut peak = PEAK_BYTES.load(Ordering::Relaxed);
    while live > peak {
        match PEAK_BYTES.compare_exchange_weak(peak, live, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => break,
            Err(actual) => peak = actual,
        }
    }
}

fn begin_allocation_window() -> usize {
    let baseline = LIVE_BYTES.load(Ordering::SeqCst);
    PEAK_BYTES.store(baseline, Ordering::SeqCst);
    baseline
}

fn peak_delta(baseline: usize) -> usize {
    PEAK_BYTES.load(Ordering::SeqCst).saturating_sub(baseline)
}

varve_format! {
    pub format CardinalityFormat {
        magic: b"HCRD";
        version: 1;
        index: keyed_offset_chain;
        blocks {
            fixed Metadata(id = 2) {
                run: u64,
            }
            variable Frame(id = 1, key = [scan, frame], key_index = disk) {
                scan: u32,
                frame: u32,
                payload: Vec<u8>,
            }
        }
    }
}

#[test]
fn generated_indexed_api_stays_key_map_free_and_preserves_latest_values() -> varve::Result<()> {
    let repeated_peak = allocation_profile(false, 2_000)?;
    let unique_peak = allocation_profile(true, 2_000)?;
    assert!(
        unique_peak <= repeated_peak.saturating_mul(3) + 2 * 1024 * 1024,
        "unique-key peak {unique_peak} grew beyond repeated-key peak {repeated_peak}"
    );

    let directory = tempfile::tempdir()?;
    let stream_path = directory.path().join("chain-unkeyed-stream.varve");
    let mut stream_writer =
        CardinalityFormat::create_stream_writer(&stream_path, varve::StreamOptions::default())?;
    let stream_report = stream_writer
        .push_metadatas(
            [Metadata { run: 1 }, Metadata { run: 2 }],
            BatchOptions::default(),
        )
        .map_err(|error| error.source)?;
    assert_eq!(stream_report.records, 2);
    assert_eq!(stream_report.write_calls, 1);
    stream_writer.sync()?;
    drop(stream_writer);
    let stream_reader =
        CardinalityFormat::open_stream_reader(&stream_path, varve::StreamOptions::default())?;
    let runs = stream_reader
        .metadatas()?
        .map(|value| value.map(|metadata| metadata.run))
        .collect::<varve::Result<Vec<_>>>()?;
    assert_eq!(runs, [1, 2]);

    let path = directory.path().join("cardinality.varve");
    let mut writer = CardinalityFormat::create_indexed_writer(
        &path,
        DiskIndexOptions {
            cache_bytes: 1024 * 1024,
            ..DiskIndexOptions::default()
        },
    )?;
    writer.push_metadata(&Metadata { run: 42 })?;
    let metadata_batch = writer
        .push_metadatas(
            [Metadata { run: 43 }, Metadata { run: 44 }],
            BatchOptions::default(),
        )
        .map_err(|error| error.source)?;
    assert_eq!(metadata_batch.records, 2);
    assert_eq!(metadata_batch.write_calls, 1);

    let report = writer
        .push_frames(
            (0..10_000u32).map(|key| Frame {
                scan: key / 100,
                frame: key,
                payload: key.to_le_bytes().to_vec(),
            }),
            BatchOptions::default(),
        )
        .map_err(|error| error.source)?;
    assert_eq!(report.records, 10_000);
    assert!(report.write_calls < 10_000);
    writer.push_frame(&Frame {
        scan: 49,
        frame: 4_999,
        payload: b"latest".to_vec(),
    })?;
    writer.delete_frame(&(50, 5_000))?;
    let state = writer.resident_state();
    assert_eq!(state.retained_record_entries, 0);
    assert_eq!(state.retained_key_entries, 0);
    writer.sync()?;
    drop(writer);

    let reader = CardinalityFormat::open_indexed_reader(
        &path,
        DiskIndexOptions {
            cache_bytes: 1024 * 1024,
            ..DiskIndexOptions::default()
        },
    )?;
    for key in [0, 1, 4_999, 9_999] {
        let value = reader.get_frame(&(key / 100, key))?.expect("indexed value");
        if key == 4_999 {
            assert_eq!(value.payload, b"latest");
        } else {
            assert_eq!(value.payload, key.to_le_bytes());
        }
    }
    assert!(reader.get_frame(&(50, 5_000))?.is_none());
    let metadata_runs = reader
        .metadatas()?
        .map(|value| value.map(|metadata| metadata.run))
        .collect::<varve::Result<Vec<_>>>()?;
    assert_eq!(metadata_runs, [42, 43, 44]);
    assert_eq!(reader.resident_state().retained_key_entries, 0);
    Ok(())
}

fn allocation_profile(unique: bool, records: u32) -> varve::Result<usize> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join(if unique {
        "unique.varve"
    } else {
        "repeat.varve"
    });
    let mut writer = CardinalityFormat::create_indexed_writer(
        &path,
        DiskIndexOptions {
            cache_bytes: 1024 * 1024,
            ..DiskIndexOptions::default()
        },
    )?;
    let baseline = begin_allocation_window();
    writer
        .push_frames(
            (0..records).map(|ordinal| {
                let key = if unique { ordinal } else { 0 };
                Frame {
                    scan: key / 100,
                    frame: key,
                    payload: ordinal.to_le_bytes().to_vec(),
                }
            }),
            BatchOptions::default(),
        )
        .map_err(|error| error.source)?;
    Ok(peak_delta(baseline))
}

fn state_sidecar_path(path: &Path) -> PathBuf {
    let mut sidecar = path.as_os_str().to_os_string();
    sidecar.push(".vks");
    PathBuf::from(sidecar)
}

fn every_record_scan<'a>(token: Option<&'a ScanCancellationToken>) -> ScanOptions<'a> {
    ScanOptions {
        progress: ScanProgressOptions {
            every_records: NonZeroU64::new(1),
            every_bytes: None,
        },
        cancellation: token,
    }
}

#[test]
fn explicit_scan_progress_and_cancellation_are_exact() -> varve::Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("scan-progress.varve");
    let mut writer =
        CardinalityFormat::create_stream_writer(&path, varve::StreamOptions::default())?;
    writer
        .push_metadatas((0..4).map(|run| Metadata { run }), BatchOptions::default())
        .map_err(|error| error.source)?;
    writer.sync()?;
    drop(writer);

    let reader = CardinalityFormat::open_stream_reader(&path, varve::StreamOptions::default())?;
    let token = ScanCancellationToken::new();
    let callback_token = token.clone();
    let mut observed = Vec::new();
    let error = reader
        .verify_all_with_progress(every_record_scan(Some(&token)), |progress| {
            observed.push(progress);
            if progress.phase == ScanProgressPhase::Running && progress.records == 2 {
                callback_token.cancel();
            }
        })
        .expect_err("the callback cancellation must stop verification");
    let varve::Error::ScanCancelled { progress } = error else {
        panic!("expected typed scan cancellation, got {error}");
    };
    assert_eq!(progress.records, 2);
    assert_eq!(
        progress.current_offset,
        observed.last().unwrap().current_offset
    );
    assert_eq!(
        progress.scanned_bytes,
        observed.last().unwrap().scanned_bytes
    );
    assert_eq!(progress.phase, ScanProgressPhase::Running);

    let mut phases = Vec::new();
    assert_eq!(
        reader.verify_all_with_progress(every_record_scan(None), |progress| {
            phases.push(progress.phase);
        })?,
        // Three user records plus the internal creation-nonce record (STO-01).
        5
    );
    assert_eq!(phases.first(), Some(&ScanProgressPhase::Started));
    assert_eq!(phases.last(), Some(&ScanProgressPhase::Complete));
    Ok(())
}

#[test]
fn cancelled_bootstrap_and_rebuild_publish_nothing() -> varve::Result<()> {
    let directory = tempfile::tempdir()?;

    let stream_path = directory.path().join("cancel-bootstrap.varve");
    let mut resident = CardinalityFormat::create_writer(&stream_path)?;
    resident.push_metadata(&Metadata { run: 1 })?;
    resident.push_metadata(&Metadata { run: 2 })?;
    resident.sync()?;
    drop(resident);
    assert!(!state_sidecar_path(&stream_path).exists());

    let bootstrap_token = ScanCancellationToken::new();
    let callback_token = bootstrap_token.clone();
    let error = CardinalityFormat::bootstrap_stream_checkpoint_with_progress(
        &stream_path,
        varve::StreamOptions::default(),
        every_record_scan(Some(&bootstrap_token)),
        move |progress| {
            if progress.phase == ScanProgressPhase::Complete {
                callback_token.cancel();
            }
        },
    )
    .expect_err("final callback cancellation must precede publication");
    assert!(matches!(error, varve::Error::ScanCancelled { .. }));
    assert!(!state_sidecar_path(&stream_path).exists());

    let indexed_path = directory.path().join("cancel-rebuild.varve");
    let mut indexed =
        CardinalityFormat::create_indexed_writer(&indexed_path, DiskIndexOptions::default())?;
    indexed.push_frame(&Frame {
        scan: 0,
        frame: 1,
        payload: b"one".to_vec(),
    })?;
    indexed.push_frame(&Frame {
        scan: 0,
        frame: 2,
        payload: b"two".to_vec(),
    })?;
    indexed.sync()?;
    drop(indexed);
    let index_path = disk_index_sidecar_path(&indexed_path);
    let before = std::fs::read(&index_path)?;

    let rebuild_token = ScanCancellationToken::new();
    let callback_token = rebuild_token.clone();
    let error = CardinalityFormat::rebuild_disk_index_with_progress(
        &indexed_path,
        DiskIndexOptions::default(),
        every_record_scan(Some(&rebuild_token)),
        move |progress| {
            if progress.phase == ScanProgressPhase::Running && progress.records == 1 {
                callback_token.cancel();
            }
        },
    )
    .expect_err("cancelled rebuild must retain the prior sidecar");
    assert!(matches!(error, varve::Error::ScanCancelled { .. }));
    assert_eq!(std::fs::read(index_path)?, before);
    Ok(())
}

#[test]
#[ignore = "one-million-key RSS/allocator stress probe"]
fn million_unique_keys_keep_varve_resident_maps_empty() -> varve::Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("million.varve");
    let mut writer = CardinalityFormat::create_indexed_writer(
        &path,
        DiskIndexOptions {
            cache_bytes: 8 * 1024 * 1024,
            ..DiskIndexOptions::default()
        },
    )?;
    let baseline = begin_allocation_window();
    let append_started = Instant::now();
    let report = writer
        .push_frames(
            (0..1_000_000u32).map(|key| Frame {
                scan: key / 1000,
                frame: key,
                payload: Vec::new(),
            }),
            BatchOptions::default(),
        )
        .map_err(|error| error.source)?;
    assert_eq!(report.records, 1_000_000);
    assert!(report.write_calls <= 62);
    let append_elapsed = append_started.elapsed();
    assert_eq!(writer.resident_state().retained_key_entries, 0);
    let sync_started = Instant::now();
    writer.sync()?;
    let sync_elapsed = sync_started.elapsed();
    let native_bytes = std::fs::metadata(&path)?.len();
    let sidecar_bytes = std::fs::metadata(disk_index_sidecar_path(&path))?.len();
    drop(writer);

    let open_started = Instant::now();
    let reader = CardinalityFormat::open_indexed_reader(
        &path,
        DiskIndexOptions {
            cache_bytes: 8 * 1024 * 1024,
            ..DiskIndexOptions::default()
        },
    )?;
    let open_elapsed = open_started.elapsed();
    let lookup_started = Instant::now();
    for key in (0..1_000_000u32).step_by(100) {
        assert!(reader.get_frame(&(key / 1000, key))?.is_some());
    }
    let lookup_elapsed = lookup_started.elapsed();

    eprintln!(
        "million-key: append={append_elapsed:?} sync={sync_elapsed:?} open={open_elapsed:?} \
         lookups_10k={lookup_elapsed:?} writes={} native={native_bytes} sidecar={sidecar_bytes} peak={} bytes",
        report.write_calls,
        peak_delta(baseline),
    );
    Ok(())
}

// PERF2-03 (report finding PERF-03): the resident keyed merge/compact family
// retains one map entry per distinct key ever seen, including tombstoned keys,
// so its memory is O(K-ever) and it is deliberately not a PB-scale operation.
// These tests pin the published contract: the cost is predictable up front via
// `estimate_keyed_merge`, and the `*_with_key_limit` entry points fail typed at
// the cardinality boundary instead of exhausting memory.

#[derive(Clone, Debug, PartialEq, varve::VarveBlock)]
#[varve(id = 40, version = 1, kind = "variable", key = "user_id")]
struct MergeUser {
    #[varve(field_id = 1)]
    user_id: u64,
    #[varve(field_id = 2)]
    name: String,
}

#[derive(Clone, Debug, PartialEq, varve::VarveBlock)]
#[varve(id = 41, version = 1, kind = "variable")]
struct MergeUserOp {
    #[varve(field_id = 1)]
    rename_to: String,
}

impl varve::VarveMerge for MergeUser {
    type Op = MergeUserOp;

    fn apply_op(&mut self, op: Self::Op) -> varve::Result<()> {
        self.name = op.rename_to;
        Ok(())
    }
}

varve_format! {
    pub struct ResidentMergeFormat {
        magic: b"HCMRG";
        version: 1;
        endian: little;
        blocks: [MergeUser, MergeUserOp];
    }
}

/// Writes `keys` distinct users and then tombstones the first `deleted` of
/// them, so `K-ever` is `keys` while `K-live` is `keys - deleted`.
fn write_merge_input(path: &Path, keys: u64, deleted: u64) -> varve::Result<()> {
    let mut file = ResidentMergeFormat::create(path)?;
    for key in 0..keys {
        file.push(&MergeUser {
            user_id: key,
            name: format!("user-{key}"),
        })?;
    }
    for key in 0..deleted {
        file.delete::<MergeUser>(&key)?;
    }
    file.flush()?;
    Ok(())
}

#[test]
fn resident_merge_estimate_reports_the_k_ever_bound() -> varve::Result<()> {
    let directory = tempfile::tempdir()?;
    let base = directory.path().join("merge-estimate-base.varve");
    let delta = directory.path().join("merge-estimate-delta.varve");
    write_merge_input(&base, 200, 50)?;
    write_merge_input(&delta, 40, 0)?;

    let estimate = varve::estimate_keyed_merge::<MergeUser, _>(
        ResidentMergeFormat::spec(),
        base.as_path(),
        &[delta.as_path()],
    )?;

    assert_eq!(estimate.input_records, 200 + 50 + 40);
    assert_eq!(estimate.key_bearing_records, 200 + 50 + 40);
    assert!(
        estimate.max_distinct_keys >= 200,
        "the estimate must bound K-ever from above, got {}",
        estimate.max_distinct_keys
    );
    assert!(estimate.largest_input_index_bytes > 0);
    assert!(estimate.max_state_bytes > 0);

    // The bound is proportional to key cardinality, which is exactly why this
    // family is resident-only: a ten-fold key count costs ten-fold state.
    let bigger = directory.path().join("merge-estimate-big.varve");
    write_merge_input(&bigger, 2_000, 0)?;
    let bigger_estimate = varve::estimate_keyed_merge::<MergeUser, _>(
        ResidentMergeFormat::spec(),
        bigger.as_path(),
        &[],
    )?;
    assert!(
        bigger_estimate.max_state_bytes > estimate.max_state_bytes * 5,
        "state bound must scale with K-ever: {} vs {}",
        bigger_estimate.max_state_bytes,
        estimate.max_state_bytes
    );
    Ok(())
}

#[test]
fn resident_merge_and_compact_fail_typed_at_the_key_ceiling() -> varve::Result<()> {
    let directory = tempfile::tempdir()?;
    let base = directory.path().join("merge-guard-base.varve");
    write_merge_input(&base, 200, 50)?;

    // Tombstoned keys stay in the state, so K-ever is 200, not 150.
    for output_name in ["merge-guard-out.varve", "compact-guard-out.varve"] {
        let output = directory.path().join(output_name);
        let compacting = output_name.starts_with("compact");
        let refused = if compacting {
            varve::compact_keyed_files_with_key_limit::<MergeUser, _>(
                ResidentMergeFormat::spec(),
                base.as_path(),
                &[],
                output.as_path(),
                199,
            )
        } else {
            varve::merge_keyed_files_with_key_limit::<MergeUser, _>(
                ResidentMergeFormat::spec(),
                base.as_path(),
                &[],
                output.as_path(),
                199,
            )
        }
        .expect_err("a K-ever ceiling below the input cardinality must be refused");
        assert!(
            matches!(
                refused,
                varve::Error::LimitExceeded {
                    resource: "merge distinct keys",
                    actual: 200,
                    limit: 199,
                }
            ),
            "{refused:?}"
        );
        assert!(
            !output.exists(),
            "a refused merge must not publish {output_name}"
        );

        let accepted = if compacting {
            varve::compact_keyed_files_with_key_limit::<MergeUser, _>(
                ResidentMergeFormat::spec(),
                base.as_path(),
                &[],
                output.as_path(),
                200,
            )
        } else {
            varve::merge_keyed_files_with_key_limit::<MergeUser, _>(
                ResidentMergeFormat::spec(),
                base.as_path(),
                &[],
                output.as_path(),
                200,
            )
        };
        accepted?;

        let merged = ResidentMergeFormat::open_readonly(&output)?;
        let users = merged.keyed_blocks::<MergeUser>()?;
        assert_eq!(users.len(), 150, "50 of the 200 keys were tombstoned");
    }
    Ok(())
}

/// `compact_keyed_file` is the third public entry point named by the resident
/// scale contract, and it lives in `merge.rs` rather than `file.rs`. It must
/// carry the same caller-usable guards as its base+delta siblings: a pre-flight
/// estimate that predicts the ceiling, a typed refusal at that ceiling that
/// publishes nothing, and an unbounded form that still behaves.
#[test]
fn resident_single_input_compact_fails_typed_at_the_key_ceiling() -> varve::Result<()> {
    let directory = tempfile::tempdir()?;
    let input = directory.path().join("compact-one-input.varve");
    write_merge_input(&input, 200, 50)?;

    // Pre-flight sizing for the single-input path goes through the same
    // estimator with an empty delta slice.
    let estimate = varve::estimate_keyed_merge::<MergeUser, _>(
        ResidentMergeFormat::spec(),
        input.as_path(),
        &[],
    )?;
    assert_eq!(estimate.input_records, 250);
    assert!(
        estimate.max_distinct_keys >= 200,
        "the estimate must bound K-ever from above, got {}",
        estimate.max_distinct_keys
    );

    let refused_output = directory.path().join("compact-one-refused.varve");
    let refused = varve::compact_keyed_file_with_key_limit::<MergeUser, _>(
        ResidentMergeFormat::spec(),
        input.as_path(),
        refused_output.as_path(),
        199,
    )
    .expect_err("a K-ever ceiling below the input cardinality must be refused");
    assert!(
        matches!(
            refused,
            varve::Error::LimitExceeded {
                resource: "merge distinct keys",
                actual: 200,
                limit: 199,
            }
        ),
        "{refused:?}"
    );
    assert!(
        !refused_output.exists(),
        "a refused compact must not publish an output file"
    );

    // The tombstoned keys are counted by the ceiling, so the exact K-ever
    // bound - not the live-key count - is what admits the run.
    let bounded_output = directory.path().join("compact-one-bounded.varve");
    varve::compact_keyed_file_with_key_limit::<MergeUser, _>(
        ResidentMergeFormat::spec(),
        input.as_path(),
        bounded_output.as_path(),
        200,
    )?;
    let bounded = ResidentMergeFormat::open_readonly(&bounded_output)?;
    assert_eq!(
        bounded.keyed_blocks::<MergeUser>()?.len(),
        150,
        "50 of the 200 keys were tombstoned"
    );

    // The unbounded entry point delegates with no ceiling and is unchanged.
    let unbounded_output = directory.path().join("compact-one-unbounded.varve");
    varve::compact_keyed_file::<MergeUser, _>(
        ResidentMergeFormat::spec(),
        input.as_path(),
        unbounded_output.as_path(),
    )?;
    let unbounded = ResidentMergeFormat::open_readonly(&unbounded_output)?;
    assert_eq!(unbounded.keyed_blocks::<MergeUser>()?.len(), 150);
    Ok(())
}

// F-01 (round 9): the generated keyed writers used to hold their own
// `HashMap<T::Key, u64>` tail map. Two consequences of that design are asserted
// here, both of which the byte-keyed resident cache changes.
//
// The first is a budget consequence. Charging a `HashMap<T::Key, u64>` can only
// count inline `(T::Key, u64)` storage: for `T::Key = String` the charge was
// 32 bytes per entry no matter how long the key was, so a file of long keys
// allocated without limit while every configured ceiling reported room. The
// cache is now keyed by the canonical internal key payload, and the charge adds
// the payload bytes the map actually owns - which is what makes a long-key
// format refusable at all.
//
// The second is an append-path consequence: the step that runs *after* the
// authoritative append must allocate nothing, so a steady-state append over a
// bounded key set must not grow resident memory.

varve_format! {
    pub format LongKeyBudgetFormat {
        magic: b"LKEYBG";
        version: 1;
        limits {
            keyed_tail: 200;
        }
        index: keyed_offset_chain;
        blocks {
            variable Labelled(id = 61, key = [label]) {
                label: String,
                value: u32,
            }
        }
    }
}

/// A `String`-keyed generated writer is charged for the key bytes it retains.
///
/// One entry costs `size_of::<(Vec<u8>, u64)>()` (32) plus the internal key
/// payload, which is a 12-byte envelope plus the encoded `String` (an 8-byte
/// length plus its bytes). A 12-byte label therefore charges 64 and fits the
/// 200-byte ceiling; a 200-byte label charges 252 and cannot. Against the old
/// inline-only charge both were 32 bytes and both were admitted, so this test
/// fails there.
#[test]
fn a_generated_keyed_writer_charges_the_key_bytes_it_retains() -> varve::Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("long-key-budget.varve");
    let mut writer = LongKeyBudgetFormat::create_writer(&path)?;

    writer.push_labelled(&Labelled {
        label: "short-label".to_string(),
        value: 1,
    })?;
    // Repeating a retained key overwrites its slot and is charged nothing.
    writer.push_labelled(&Labelled {
        label: "short-label".to_string(),
        value: 2,
    })?;

    let long = "L".repeat(200);
    let error = writer
        .push_labelled(&Labelled {
            label: long.clone(),
            value: 3,
        })
        .expect_err("a 200-byte key cannot fit a 200-byte keyed-tail ceiling");
    assert!(
        matches!(
            error,
            varve::Error::LimitExceeded {
                resource: "keyed tail bytes",
                ..
            }
        ),
        "expected a typed keyed-tail budget rejection, got {error:?}"
    );
    // The tombstone path is charged identically, and refuses before appending.
    let error = writer
        .delete_labelled(&long)
        .expect_err("a tombstone for that key needs the same retained slot");
    assert!(
        matches!(
            error,
            varve::Error::LimitExceeded {
                resource: "keyed tail bytes",
                ..
            }
        ),
        "expected a typed keyed-tail budget rejection, got {error:?}"
    );
    writer.flush()?;
    drop(writer);

    // Invariant 3: both refusals happened before the append, so neither the
    // record nor the tombstone exists.
    let reopened = LongKeyBudgetFormat::open(&path)?;
    let records: Vec<Labelled> = reopened
        .blocks::<Labelled>()?
        .iter()
        .collect::<varve::Result<_>>()?;
    assert_eq!(
        records,
        vec![
            Labelled {
                label: "short-label".to_string(),
                value: 1,
            },
            Labelled {
                label: "short-label".to_string(),
                value: 2,
            },
        ],
        "the refused mutations must not have appended anything"
    );
    Ok(())
}

varve_format! {
    pub format SteadyAppendFormat {
        magic: b"STDYAP";
        version: 1;
        index: keyed_offset_chain;
        blocks {
            variable Sample(id = 62, key = [id]) {
                id: u64,
                value: u64,
            }
        }
    }
}

varve_format! {
    pub format SteadyAppendUnkeyedFormat {
        magic: b"STDYAU";
        version: 1;
        // The same index policy, so records carry the same footer and the
        // checkpoint cadence sees the same byte totals: the only difference
        // between the two loops is that one maintains a keyed tail.
        index: keyed_offset_chain;
        blocks {
            variable Plain(id = 62) {
                id: u64,
                value: u64,
            }
        }
    }
}

/// The generated keyed append adds no resident memory of its own in the steady
/// state.
///
/// Every key in the measured loop is already retained, so the tail cache
/// neither grows nor reserves: the pre-append reservation short-circuits and
/// the post-append commit overwrites a slot in place. Anything the append
/// allocates transiently - the encoded record, the encoded key payload - is
/// released before the call returns.
///
/// What an append *does* legitimately retain is one resident index entry, which
/// has nothing to do with keying. The measurement is therefore differential:
/// the same number of appends of the same shape through an unkeyed generated
/// writer retains exactly that index, and the difference between the two is
/// what the keyed machinery costs. It must be flat.
///
/// This is the measurement behind the claim that the post-publication step
/// allocates nothing. A design that inserted an owned key after the append - or
/// that grew the map there - would show a per-append residue here.
#[test]
fn steady_state_generated_keyed_appends_do_not_grow_resident_memory() -> varve::Result<()> {
    const KEYS: u64 = 64;
    const APPENDS: u64 = 20_000;

    let directory = tempfile::tempdir()?;

    let keyed_path = directory.path().join("steady-append-keyed.varve");
    let mut keyed = SteadyAppendFormat::create_writer(&keyed_path)?;
    // Retain every key first, so the measured loop only overwrites slots.
    for id in 0..KEYS {
        keyed.push_sample(&Sample { id, value: 0 })?;
    }
    let keyed_baseline = thread_live_bytes();
    for round in 1..=APPENDS {
        keyed.push_sample(&Sample {
            id: round % KEYS,
            value: round,
        })?;
    }
    let keyed_growth = thread_live_bytes() - keyed_baseline;
    keyed.flush()?;

    let plain_path = directory.path().join("steady-append-plain.varve");
    let mut plain = SteadyAppendUnkeyedFormat::create_writer(&plain_path)?;
    for id in 0..KEYS {
        plain.push_plain(&Plain { id, value: 0 })?;
    }
    let plain_baseline = thread_live_bytes();
    for round in 1..=APPENDS {
        plain.push_plain(&Plain {
            id: round % KEYS,
            value: round,
        })?;
    }
    let plain_growth = thread_live_bytes() - plain_baseline;
    plain.flush()?;

    // Measured on the round-9 fix: exactly 0. The tolerance is there for
    // allocator and checkpoint-cadence jitter on other hosts, and is still two
    // orders of magnitude below the ~32-48 bytes per append that retaining one
    // owned key payload per record would cost.
    let keyed_overhead = keyed_growth - plain_growth;
    assert!(
        keyed_overhead < 4096,
        "{APPENDS} steady-state keyed appends retained {keyed_overhead} bytes more than the          same unkeyed appends ({keyed_growth} vs {plain_growth}); the post-append tail          commit must not allocate and the cache must not grow",
    );

    drop(keyed);
    drop(plain);
    let reopened = SteadyAppendFormat::open(&keyed_path)?;
    assert_eq!(
        reopened.keyed_blocks::<Sample>()?.len(),
        KEYS as usize,
        "every key must still resolve after the steady-state run",
    );
    Ok(())
}
