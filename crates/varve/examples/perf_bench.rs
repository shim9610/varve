use std::fs::{metadata, remove_file};
use std::hint::black_box;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use varve::{
    VarveBlock, VarveMerge, compact_keyed_file, compact_keyed_files, decode_from_slice,
    encode_to_vec, merge_keyed_files, varve_format,
};

#[derive(Clone, Debug, PartialEq, VarveBlock)]
#[varve(id = 200, version = 1, kind = "fixed")]
struct BenchPoint {
    x: u64,
    y: u64,
}

#[derive(Clone, Debug, PartialEq, VarveBlock)]
#[varve(id = 201, version = 1, kind = "variable", key = "id")]
struct BenchUser {
    #[varve(field_id = 1)]
    id: u64,
    #[varve(field_id = 2)]
    name: String,
    #[varve(field_id = 3)]
    payload: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, VarveBlock)]
#[varve(id = 202, version = 1, kind = "variable")]
struct BenchUserOp {
    #[varve(field_id = 1)]
    name: String,
}

impl VarveMerge for BenchUser {
    type Op = BenchUserOp;

    fn apply_op(&mut self, op: Self::Op) -> varve::Result<()> {
        self.name = op.name;
        Ok(())
    }
}

varve_format! {
    pub struct BenchFormat {
        magic: b"VBENCH";
        version: 1;
        limits {
            file_len: 8_589_934_592;
            records: 4_000_000;
            index_bytes: 536_870_912;
            scan_bytes: 8_589_934_592;
            record_payload: 67_108_864;
            logical_payload: 268_435_456;
            materialized_bytes: 1_073_741_824;
            segments: 4_000_000;
            matrix_dimension: 16_000_000;
            matrix_cells: 16_000_000;
            matrix_bitmap: 64_000_000;
            matrix_crc: 128_000_000;
            matrix_metadata: 268_435_456;
            matrix_slot_region: 8_589_934_592;
            sidecar: 268_435_456;
            mmap: 8_589_934_592;
        }
        endian: little;
        index: checkpoint_on_flush;
        manifest: embedded;
        blocks: [BenchPoint, BenchUser, BenchUserOp];
    }
}

fn main() -> varve::Result<()> {
    let records = std::env::args()
        .nth(1)
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(10_000)
        .max(1);

    println!("varve perf bench: {records} records");
    codec_bench(records)?;
    append_open_scan_bench(records)?;
    merge_compact_bench(records)?;
    Ok(())
}

fn codec_bench(records: usize) -> varve::Result<()> {
    let point = BenchPoint { x: 1, y: 3 };
    let elapsed = timed(|| {
        for _ in 0..records {
            let bytes = encode_to_vec(&point, BenchFormat::spec().endian)?;
            let decoded: BenchPoint = decode_from_slice(&bytes, BenchFormat::spec().endian)?;
            black_box(decoded);
        }
        Ok(())
    })?;
    report("encode/decode fixed", Work::records(records), None, elapsed);

    let user = bench_user(42);
    let elapsed = timed(|| {
        for _ in 0..records {
            let bytes = encode_to_vec(&user, BenchFormat::spec().endian)?;
            let decoded: BenchUser = decode_from_slice(&bytes, BenchFormat::spec().endian)?;
            black_box(decoded);
        }
        Ok(())
    })?;
    report(
        "encode/decode variable",
        Work::records(records),
        None,
        elapsed,
    );
    Ok(())
}

fn append_open_scan_bench(records: usize) -> varve::Result<()> {
    let path = temp_path("append_open_scan");
    cleanup(&path);

    let elapsed = timed(|| {
        let mut file = BenchFormat::create(&path)?;
        for index in 0..records {
            file.push(&BenchPoint {
                x: index as u64,
                y: (index as u64).wrapping_mul(3),
            })?;
        }
        file.flush()?;
        file.sync()
    })?;
    report("append fixed", Work::records(records), Some(&path), elapsed);

    let elapsed = timed(|| {
        let file = BenchFormat::open_readonly(&path)?;
        assert_eq!(
            file.scan().collect::<varve::Result<Vec<_>>>()?.len(),
            records + 2
        );
        assert_eq!(file.blocks::<BenchPoint>()?.len(), records);
        Ok(())
    })?;
    report(
        "open/scan fixed",
        Work::records(records),
        Some(&path),
        elapsed,
    );

    cleanup(&path);
    Ok(())
}

fn merge_compact_bench(records: usize) -> varve::Result<()> {
    let base = temp_path("base");
    let delta = temp_path("delta");
    let merged = temp_path("merged");
    let compacted = temp_path("compacted");
    let direct = temp_path("direct_compacted");
    cleanup_many([&base, &delta, &merged, &compacted, &direct]);

    // BENCH-01: the exact shape of the workload, so each report below can name
    // its own denominator instead of borrowing the base record count.
    let updates = records / 4;
    let deletes = records / 3 - records / 4;
    let inserts = records / 5;
    let delta_records = updates + deletes + inserts;
    // Every merge/compact input record is read and applied: the base file plus
    // the whole delta file.
    let base_plus_delta_events = records + delta_records;
    // `merge_keyed_files` and `compact_keyed_files` both write the surviving
    // live values, so this is what comes out of either of them — and, because
    // `merged` holds exactly these values, it is also what the second compact
    // both reads and writes.
    let live_values = records - deletes + inserts;

    {
        let mut file = BenchFormat::create(&base)?;
        for index in 0..records {
            file.push(&bench_user(index))?;
        }
        file.flush()?;
        file.sync()?;
    }

    {
        let mut file = BenchFormat::create(&delta)?;
        for index in 0..records / 4 {
            file.push_op::<BenchUser>(
                &(index as u64),
                &BenchUserOp {
                    name: format!("delta-{index}"),
                },
            )?;
        }
        for index in records / 4..records / 3 {
            file.delete::<BenchUser>(&(index as u64))?;
        }
        for index in records..records + records / 5 {
            file.push(&bench_user(index))?;
        }
        file.flush()?;
        file.sync()?;
    }

    let elapsed = timed(|| {
        merge_keyed_files::<BenchUser, _>(
            BenchFormat::spec(),
            base.as_path(),
            &[delta.as_path()],
            merged.as_path(),
        )
    })?;
    report(
        "merge keyed files",
        Work::events(base_plus_delta_events, live_values),
        Some(&merged),
        elapsed,
    );

    let elapsed = timed(|| {
        compact_keyed_file::<BenchUser, _>(
            BenchFormat::spec(),
            merged.as_path(),
            compacted.as_path(),
        )
    })?;
    // The already-merged input holds only live values, so this compact both
    // reads and writes exactly `live_values`.
    report(
        "compact merged",
        Work::events(live_values, live_values),
        Some(&compacted),
        elapsed,
    );

    let elapsed = timed(|| {
        compact_keyed_files::<BenchUser, _>(
            BenchFormat::spec(),
            base.as_path(),
            &[delta.as_path()],
            direct.as_path(),
        )
    })?;
    report(
        "compact base+deltas",
        Work::events(base_plus_delta_events, live_values),
        Some(&direct),
        elapsed,
    );

    // BENCH-01: every line that prints a live-value denominator is checked
    // against the file that line actually produced, not just the last one. The
    // contract this loop upholds: for each of the three merge and compact lines,
    // the run reopens the file that line produced and asserts the live-value
    // count it printed against the blocks actually materialized in that file.
    // Checking only one of the three left the other two unverified.
    for (label, path) in [
        ("merge keyed files", &merged),
        ("compact merged", &compacted),
        ("compact base+deltas", &direct),
    ] {
        let file = BenchFormat::open_readonly(path)?;
        let emitted = file.materialized_keyed_blocks::<BenchUser>()?.len();
        assert_eq!(
            emitted, live_values,
            "{label}: the reported live-value denominator must be the count actually emitted"
        );
    }

    cleanup_many([&base, &delta, &merged, &compacted, &direct]);
    Ok(())
}

fn bench_user(index: usize) -> BenchUser {
    BenchUser {
        id: index as u64,
        name: format!("user-{index}"),
        payload: vec![(index % 251) as u8; 64],
    }
}

fn timed(operation: impl FnOnce() -> varve::Result<()>) -> varve::Result<Duration> {
    let start = Instant::now();
    operation()?;
    Ok(start.elapsed())
}

/// What a throughput number is actually per (BENCH-01).
///
/// The merge and compact operations do not process `records` items: their input
/// is the base file *plus* every delta record (updates, deletes, and inserts),
/// and their output is the set of surviving live values, which is smaller than
/// both. Reporting all three operations against the original base record count
/// produced a rate that was neither the events consumed nor the values emitted.
/// Each call site now states its own denominator.
struct Work {
    /// Items the operation actually consumed, and the unit to print.
    processed: usize,
    unit: &'static str,
    /// Live values written out, where that differs from what was consumed.
    emitted: Option<usize>,
}

impl Work {
    fn records(count: usize) -> Self {
        Self {
            processed: count,
            unit: "records",
            emitted: None,
        }
    }

    fn events(processed: usize, emitted: usize) -> Self {
        Self {
            processed,
            unit: "input events",
            emitted: Some(emitted),
        }
    }
}

fn report(label: &str, work: Work, path: Option<&Path>, elapsed: Duration) {
    let seconds = elapsed.as_secs_f64();
    let per_sec = if seconds > 0.0 {
        work.processed as f64 / seconds
    } else {
        f64::INFINITY
    };
    let bytes = path
        .and_then(|path| metadata(path).ok())
        .map_or(0, |m| m.len());
    let unit = work.unit;
    let emitted = match work.emitted {
        Some(count) => format!(" | {count:>9} live values out"),
        None => String::new(),
    };
    println!(
        "{label:>24}: {:>9.3} ms | {:>12.0} {unit}/sec | {:>10} bytes{emitted}",
        seconds * 1_000.0,
        per_sec,
        bytes
    );
}

fn temp_path(name: &str) -> PathBuf {
    let mut path = std::env::temp_dir();
    path.push(format!(
        "varve_perf_bench_{name}_{}.vrv",
        std::process::id()
    ));
    path
}

fn cleanup_many<'a>(paths: impl IntoIterator<Item = &'a PathBuf>) {
    for path in paths {
        cleanup(path);
    }
}

fn cleanup(path: &PathBuf) {
    let _ = remove_file(path);
    let mut lock = path.as_os_str().to_os_string();
    lock.push(".lock");
    let _ = remove_file(PathBuf::from(lock));
}
