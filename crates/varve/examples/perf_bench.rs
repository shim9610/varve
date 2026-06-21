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
    report("encode/decode fixed", records, None, elapsed);

    let user = bench_user(42);
    let elapsed = timed(|| {
        for _ in 0..records {
            let bytes = encode_to_vec(&user, BenchFormat::spec().endian)?;
            let decoded: BenchUser = decode_from_slice(&bytes, BenchFormat::spec().endian)?;
            black_box(decoded);
        }
        Ok(())
    })?;
    report("encode/decode variable", records, None, elapsed);
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
    report("append fixed", records, Some(&path), elapsed);

    let elapsed = timed(|| {
        let file = BenchFormat::open_readonly(&path)?;
        assert_eq!(file.scan().count(), records + 2);
        assert_eq!(file.blocks::<BenchPoint>()?.len(), records);
        Ok(())
    })?;
    report("open/scan fixed", records, Some(&path), elapsed);

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
    report("merge keyed files", records, Some(&merged), elapsed);

    let elapsed = timed(|| {
        compact_keyed_file::<BenchUser, _>(
            BenchFormat::spec(),
            merged.as_path(),
            compacted.as_path(),
        )
    })?;
    report("compact merged", records, Some(&compacted), elapsed);

    let elapsed = timed(|| {
        compact_keyed_files::<BenchUser, _>(
            BenchFormat::spec(),
            base.as_path(),
            &[delta.as_path()],
            direct.as_path(),
        )
    })?;
    report("compact base+deltas", records, Some(&direct), elapsed);

    let expected = records - (records / 3 - records / 4) + records / 5;
    let direct_file = BenchFormat::open_readonly(&direct)?;
    assert_eq!(
        direct_file.materialized_keyed_blocks::<BenchUser>()?.len(),
        expected
    );

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

fn report(label: &str, records: usize, path: Option<&Path>, elapsed: Duration) {
    let seconds = elapsed.as_secs_f64();
    let records_per_sec = if seconds > 0.0 {
        records as f64 / seconds
    } else {
        f64::INFINITY
    };
    let bytes = path
        .and_then(|path| metadata(path).ok())
        .map_or(0, |m| m.len());
    println!(
        "{label:>24}: {:>9.3} ms | {:>12.0} records/sec | {:>10} bytes",
        seconds * 1_000.0,
        records_per_sec,
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
