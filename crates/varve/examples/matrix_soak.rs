//! Timed single-writer / independent-reader generation stress with an arithmetic oracle.
//! Run through scripts/soak_matrix.py for resource monitoring and owned cleanup.
use std::{
    path::Path,
    sync::atomic::{AtomicBool, AtomicU64, Ordering},
    time::Instant,
};
use varve::{MatrixCellStatus, MatrixKey, ReadLimits, VarveReader, varve_format};

varve_format! {
    pub format Soak {
        magic: b"MSOK";
        version: 1;
        integrity: crc32_with_header;
        dims { row: u32, col: u32, }
        commit: cell_bitmap { keyspace = [row, col]; categories = [data]; };
        aux { epoch: 8, }
        blocks { matrix Cell(id = 1, dims = [row, col], category = data) {
            payload: [u8; 4096],
        } }
    }
}
type Result<T = ()> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;
const GROUP: u32 = 256;

fn payload(row: u32, epoch: u64) -> [u8; 4096] {
    let mut bytes = [0; 4096];
    let mut state = epoch.wrapping_mul(0x9e3779b97f4a7c15) ^ u64::from(row);
    for chunk in bytes.as_chunks_mut::<8>().0 {
        state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        chunk.copy_from_slice(&state.to_le_bytes());
    }
    bytes
}
fn version(row: u32, epoch: u64, groups: u64) -> u64 {
    let first = u64::from(row / GROUP) + 1;
    if epoch < first {
        0
    } else {
        epoch - (epoch - first) % groups
    }
}
fn check(reader: &VarveReader, row: u32, epoch: u64, groups: u64) -> Result {
    let v = version(row, epoch, groups);
    let key = MatrixKey::new(u64::from(row), 0);
    if v != 0 && v.is_multiple_of(7) {
        assert_eq!(
            reader.matrix_cell_status::<Cell>(key)?,
            MatrixCellStatus::NotCommitted,
            "deleted row={row} epoch={epoch}"
        );
    } else {
        assert_eq!(
            reader.matrix_cell_status::<Cell>(key)?,
            MatrixCellStatus::Committed
        );
        assert_eq!(
            reader.read_matrix_cell::<Cell>(key)?.payload,
            payload(row, v),
            "mixed/corrupt generation row={row} epoch={epoch} version={v}"
        );
    }
    Ok(())
}
fn epoch(reader: &VarveReader) -> Result<u64> {
    Ok(u64::from_le_bytes(
        reader.read_matrix_aux("epoch", 0, 8)?.try_into().unwrap(),
    ))
}
fn open(path: &Path, cache: u64) -> varve::Result<VarveReader> {
    Soak::spec().open_reader_with_resource_limits(
        path,
        ReadLimits::STANDARD.with_matrix_generation_cache_bytes(cache),
    )
}
fn latency(name: &str, mut values: Vec<u128>) {
    if values.is_empty() {
        return;
    }
    values.sort_unstable();
    println!(
        "{{\"event\":\"latency\",\"operation\":{name:?},\"count\":{},\"p50_us\":{},\"p95_us\":{},\"p99_us\":{},\"max_us\":{}}}",
        values.len(),
        values[(values.len() - 1) / 2],
        values[(values.len() - 1) * 95 / 100],
        values[(values.len() - 1) * 99 / 100],
        values.last().unwrap()
    );
}
fn run() -> Result {
    let args: Vec<_> = std::env::args().collect();
    if args.len() != 5 {
        return Err("usage: matrix_soak PATH SECONDS READERS ROWS".into());
    }
    let path = Path::new(&args[1]);
    let seconds: u64 = args[2].parse()?;
    let readers: usize = args[3].parse()?;
    let rows: u32 = args[4].parse()?;
    if seconds == 0 || readers > 64 || rows == 0 || !rows.is_multiple_of(GROUP) {
        return Err(
            "seconds > 0, readers <= 64 and positive rows divisible by 256 required".into(),
        );
    }
    let groups = u64::from(rows / GROUP);
    let mut writer = Soak::create_writer_with_dims(path, SoakDims { row: rows, col: 1 })?;
    for row in 0..rows {
        let key = CellKey {
            row: u64::from(row),
            col: 0,
        };
        writer.write_cell(
            key,
            &Cell {
                payload: payload(row, 0),
            },
        )?;
        writer.commit_cell(key)?;
        if (row + 1) % GROUP == 0 {
            writer.sync()?;
        }
    }
    writer.sync()?;
    println!(
        "{{\"event\":\"initialized\",\"rows\":{rows},\"live_payload_bytes\":{},\"readers\":{readers},\"seconds\":{seconds}}}",
        u64::from(rows) * 4096
    );
    let stop = AtomicBool::new(false);
    let latest = AtomicU64::new(0);
    let start = Instant::now();
    struct StopOnExit<'a>(&'a AtomicBool);
    impl Drop for StopOnExit<'_> {
        fn drop(&mut self) {
            self.0.store(true, Ordering::Release);
        }
    }
    let result = std::thread::scope(|scope| -> Result {
        let _stop_on_exit = StopOnExit(&stop);
        let mut workers = Vec::new();
        for id in 0..readers {
            let stop = &stop;
            let latest = &latest;
            workers.push(scope.spawn(move || -> Result {
                // Both an uncached pinned descriptor and an independently following reader.
                let _stop_on_exit = StopOnExit(stop);
                let pinned = open(path, 0)?;
                let pinned_epoch = epoch(&pinned)?;
                let cache = [0, 4096, 2*1024*1024][id % 3];
                let mut reader = open(path, cache)?;
                let mut state = id as u64 + 1;
                let mut reads = 0u64;
                let mut previous = 0;
                let mut follows = Vec::new();
                loop {
                    let done = stop.load(Ordering::Acquire);
                    let t = Instant::now();
                    reader.follow()?;
                    // Bound instrumentation memory independently of run duration.
                    if follows.len() < 100_000 { follows.push(t.elapsed().as_micros()); }
                    let e = epoch(&reader)?;
                    assert!(e >= previous, "reader generation regressed");
                    previous = e;
                    for _ in 0..32 {
                        state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
                        let row = (state >> 32) as u32 % rows;
                        check(&reader, row, e, groups)?;
                        check(&pinned, row, pinned_epoch, groups)?;
                        reads += 2;
                    }
                    if reads.is_multiple_of(1024) {
                        let fresh = open(path, cache)?;
                        let e = epoch(&fresh)?;
                        check(&fresh, (state >> 32) as u32 % rows, e, groups)?;
                    }
                    if done {
                        assert_eq!(e, latest.load(Ordering::Acquire));
                        break;
                    }
                }
                latency(&format!("follow-reader-{id}-first-100000"), follows);
                println!("{{\"event\":\"reader_complete\",\"id\":{id},\"checks\":{reads},\"last_epoch\":{previous}}}");
                Ok(())
            }));
        }
        let writer_result = (|| -> Result {
            let mut e = 0;
            let mut syncs = Vec::new();
            let mut compactions = Vec::new();
            while start.elapsed().as_secs() < seconds && !stop.load(Ordering::Acquire) {
                e += 1;
                let base = ((e - 1) % groups) as u32 * GROUP;
                let t = Instant::now();
                for row in base..base + GROUP {
                    let key = CellKey {
                        row: u64::from(row),
                        col: 0,
                    };
                    if e % 7 == 0 {
                        writer
                            .inner_mut()
                            .clear_matrix_cell::<Cell>(MatrixKey::new(u64::from(row), 0))?;
                    } else {
                        writer.write_cell(
                            key,
                            &Cell {
                                payload: payload(row, e),
                            },
                        )?;
                        writer.commit_cell(key)?;
                    }
                }
                writer.write_epoch_aux(0, &e.to_le_bytes())?;
                if e % 8 == 0 {
                    writer.flush()?;
                    let dirty = open(path, 0)?;
                    assert_eq!(epoch(&dirty)?, e - 1);
                    check(&dirty, base, e - 1, groups)?;
                }
                writer.sync()?;
                if syncs.len() < 100_000 {
                    syncs.push(t.elapsed().as_micros());
                }
                latest.store(e, Ordering::Release);
                if e % groups == 0 {
                    let t = Instant::now();
                    writer.compact_matrix()?;
                    compactions.push(t.elapsed().as_micros());
                    println!(
                        "{{\"event\":\"progress\",\"epoch\":{e},\"elapsed_seconds\":{},\"compactions\":{}}}",
                        start.elapsed().as_secs_f64(),
                        compactions.len()
                    );
                }
                if e % 128 == 0 {
                    drop(writer);
                    writer = Soak::open_writer(path)?;
                }
            }
            latency("write-batch-and-sync-first-100000", syncs);
            latency("compaction", compactions);
            println!(
                "{{\"event\":\"writer_complete\",\"epochs\":{e},\"payload_bytes_written\":{}}}",
                (e - e / 7) * u64::from(GROUP) * 4096
            );
            Ok(())
        })();
        stop.store(true, Ordering::Release);
        let mut reader_error = None;
        for worker in workers {
            if let Err(error) = worker.join().map_err(|_| "reader panicked").and_then(|r| {
                r.map_err(|error| {
                    eprintln!("reader: {error}");
                    "reader validation failed"
                })
            }) {
                reader_error = Some(error);
            }
        }
        writer_result?;
        if let Some(error) = reader_error {
            return Err(error.into());
        }
        Ok(())
    });
    result?;
    let reader = open(path, 4096)?;
    let e = epoch(&reader)?;
    for row in 0..rows {
        check(&reader, row, e, groups)?;
    }
    println!(
        "{{\"event\":\"soak_complete\",\"elapsed_seconds\":{},\"final_epoch\":{e},\"verified_rows\":{rows}}}",
        start.elapsed().as_secs_f64()
    );
    Ok(())
}
fn main() {
    if let Err(error) = run() {
        eprintln!("{error}");
        std::process::exit(1);
    }
}
