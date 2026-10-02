#![cfg(feature = "integrity")]

//! Stabilization gates: strict model histories, pinned concurrent snapshots,
//! and repeated external process termination. No operation error is discarded.
//! Reproduce a campaign with VARVE_QUAL_SEED and VARVE_QUAL_EPOCHS.

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::Write;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use varve::{
    BatchOptions, DiskIndexBatchOptions, DiskIndexOptions, WriterLockBreakPolicy,
    clear_stale_writer_lock, varve_format,
};

varve_format! {
    pub format Qualification {
        magic: b"QUAL";
        version: 1;
        schema_hash: computed;
        index: keyed_offset_chain;
        integrity: crc32;
        blocks {
            variable Item(id = 1, key = [tenant, key], key_index = disk) {
                tenant: u32,
                key: u64,
                payload: Vec<u8>,
            }
            fixed Marker(id = 2) {
                value: u64,
            }
        }
    }
}

const KEYS: u64 = 257;
const WAIT: Duration = Duration::from_secs(60);
type Model = BTreeMap<(u32, u64), Vec<u8>>;

fn options() -> DiskIndexOptions {
    DiskIndexOptions {
        cache_bytes: 1024 * 1024,
        max_key_bytes: 1024,
        // Cross sidecar transaction boundaries much more often than native
        // chunks; neither boundary is a durable application checkpoint.
        batch: DiskIndexBatchOptions {
            max_records: 7,
            max_bytes: 2048,
        },
        ..DiskIndexOptions::default()
    }
}

fn key(number: u64) -> (u32, u64) {
    // Composite keys include large integers and the zero boundary.
    (
        (number % 5) as u32,
        number.wrapping_mul(0x0100_0000_0000_0001),
    )
}

fn item(number: u64, value: u64) -> Item {
    let (tenant, key) = key(number);
    let mut payload = vec![(value ^ number) as u8; [0, 1, 31, 255, 4097][value as usize % 5]];
    payload.extend_from_slice(&value.to_le_bytes());
    Item {
        tenant,
        key,
        payload,
    }
}

fn check(reader: &QualificationIndexedReader, model: &Model) -> varve::Result<()> {
    for number in 0..=KEYS {
        let key = key(number);
        assert_eq!(
            reader.get_item(&key)?.map(|item| item.payload),
            model.get(&key).cloned(),
            "lookup mismatch at {key:?}"
        );
    }
    let state = reader.resident_state();
    assert_eq!(state.retained_key_entries, 0);
    assert_eq!(state.retained_record_entries, 0);
    Ok(())
}

// SplitMix64 is specified here, so seed replay is independent of dependency
// PRNG changes. The oracle is an ordinary map, not another Varve read path.
fn random(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9e37_79b9_7f4a_7c15);
    let mut value = *state;
    value = (value ^ (value >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    value = (value ^ (value >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    value ^ (value >> 31)
}

fn configured(name: &str, default: u64) -> u64 {
    match std::env::var(name) {
        Ok(value) => value
            .parse()
            .unwrap_or_else(|_| panic!("invalid {name}={value:?}")),
        Err(std::env::VarError::NotPresent) => default,
        Err(error) => panic!("{name}: {error}"),
    }
}

fn history(seed: u64, epochs: u64) -> varve::Result<()> {
    assert!(
        epochs >= 4,
        "at least four epochs must exercise all lifecycle paths"
    );
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("history.varve");
    let mut writer = Qualification::create_indexed_writer(&path, options())?;
    writer.sync()?;
    let mut committed = Model::new();
    let mut markers = Vec::new();
    let mut state = seed;
    let mut operations = [0u64; 6];

    for epoch in 0..epochs {
        eprintln!("qualification-history seed={seed} epoch={epoch}/{epochs}");
        let old = Qualification::open_indexed_reader(&path, options())?;
        let old_eof = fs::metadata(&path)?.len();
        let mut pending = committed.clone();
        let mut pending_markers = markers.clone();
        for step in 0..128 {
            // Each epoch necessarily covers every operation, then randomizes.
            let operation = if step < 6 {
                step
            } else {
                random(&mut state) % 6
            };
            operations[operation as usize] += 1;
            let number = random(&mut state) % KEYS;
            let value = random(&mut state);
            match operation {
                0 | 1 => {
                    let item = item(number, value);
                    writer.push_item(&item)?;
                    pending.insert(key(number), item.payload);
                }
                2 => {
                    writer.delete_item(&key(number))?;
                    pending.remove(&key(number));
                }
                3 => {
                    // Includes repeated keys within the same native chunk.
                    let items: Vec<_> = (0..19)
                        .map(|offset| {
                            item((number + offset % 11) % KEYS, value.wrapping_add(offset))
                        })
                        .collect();
                    let report = writer
                        .push_items(
                            &items,
                            BatchOptions {
                                max_records: 3,
                                max_bytes: 512,
                            },
                        )
                        .map_err(|error| error.source)?;
                    assert_eq!(report.records, items.len() as u64);
                    for item in items {
                        pending.insert((item.tenant, item.key), item.payload);
                    }
                }
                4 => {
                    writer.push_marker(&Marker { value })?;
                    pending_markers.push(value);
                }
                5 => writer.flush()?,
                _ => unreachable!(),
            }
        }
        // Readers opened before the mutation must still see the old roots,
        // including deletes and updates whose physical bytes now exist.
        check(&old, &committed)?;
        drop(old);
        if epoch % 3 == 2 {
            writer.flush()?;
            drop(writer);
            check_dirty_reader(&path, &committed);
            writer = Qualification::restore_indexed_writer(&path, options())?;
            assert_eq!(
                fs::metadata(&path)?.len(),
                old_eof,
                "restore EOF seed={seed} epoch={epoch}"
            );
        } else {
            writer.sync()?;
            committed = pending;
            markers = pending_markers;
            drop(writer);
            writer = Qualification::open_indexed_writer(&path, options())?;
        }
        let reader = Qualification::open_indexed_reader(&path, options())?;
        check(&reader, &committed)?;
        assert_eq!(
            reader
                .markers()?
                .map(|m| m.map(|m| m.value))
                .collect::<varve::Result<Vec<_>>>()?,
            markers
        );
        reader.verify_all()?;
        drop(reader);
        if epoch % 4 == 3 {
            drop(writer);
            Qualification::rebuild_disk_index(&path, options())?;
            check(
                &Qualification::open_indexed_reader(&path, options())?,
                &committed,
            )?;
            writer = Qualification::open_indexed_writer(&path, options())?;
        }
    }
    assert!(operations.into_iter().all(|count| count > 0));
    eprintln!("QUAL_HISTORY seed={seed} epochs={epochs} operations={operations:?}");
    Ok(())
}

fn check_dirty_reader(path: &Path, committed: &Model) {
    // Reader access does not recover or publish the unfinished generation.
    let mut reader = Qualification::open_indexed_reader(path, options()).unwrap();
    check(&reader, committed).unwrap();
    reader.verify_all().unwrap();
    assert_eq!(reader.follow().unwrap(), 0);
}

#[test]
fn model_history_regression() -> varve::Result<()> {
    for seed in [0, 1, 0xdead_beef, u64::MAX] {
        history(seed, 4)?;
    }
    Ok(())
}

#[test]
#[ignore = "long deterministic model campaign; scripts/qualify_scalable.py"]
fn model_history_stress() -> varve::Result<()> {
    history(
        configured("VARVE_QUAL_SEED", 42),
        configured("VARVE_QUAL_EPOCHS", 64),
    )
}

#[test]
fn concurrent_snapshots_survive_many_publications() -> varve::Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("concurrent.varve");
    let mut writer = Qualification::create_indexed_writer(&path, options())?;
    let initial: Vec<_> = (0..KEYS).map(|number| item(number, 0)).collect();
    writer
        .push_items(&initial, BatchOptions::default())
        .map_err(|e| e.source)?;
    writer.sync()?;
    let original: Model = initial
        .into_iter()
        .map(|i| ((i.tenant, i.key), i.payload))
        .collect();
    std::thread::scope(|scope| -> varve::Result<()> {
        let mut workers = Vec::new();
        let mut channels = Vec::new();
        for _ in 0..4 {
            let reader = Qualification::open_indexed_reader(&path, options())?;
            let model = &original;
            let (start, receive) = mpsc::channel();
            let (done, ack) = mpsc::channel();
            workers.push(scope.spawn(move || -> varve::Result<()> {
                while receive.recv_timeout(WAIT).is_ok() {
                    check(&reader, model)?;
                    done.send(())
                        .expect("coordinator dropped acknowledgement receiver");
                }
                Ok(())
            }));
            channels.push((start, ack));
        }
        for epoch in 1..=24 {
            for (start, _) in &channels {
                start.send(()).unwrap();
            }
            let next: Vec<_> = (0..KEYS).map(|number| item(number, epoch)).collect();
            writer
                .push_items(&next, BatchOptions::default())
                .map_err(|e| e.source)?;
            writer.sync()?;
            let expected: Model = next
                .into_iter()
                .map(|i| ((i.tenant, i.key), i.payload))
                .collect();
            check(
                &Qualification::open_indexed_reader(&path, options())?,
                &expected,
            )?;
            for (_, ack) in &channels {
                ack.recv_timeout(WAIT)
                    .expect("snapshot worker failed or timed out");
            }
        }
        drop(channels);
        for worker in workers {
            worker.join().expect("snapshot worker panicked")?;
        }
        Ok(())
    })
}

// External kill deliberately bypasses Drop and fault hooks. The ready file is
// written and fsynced only AFTER flush/sync returned. Its contents distinguish
// acknowledged durable generations from deliberately unacknowledged updates.
#[test]
#[ignore = "subprocess entry point, only invoked by kill/recovery tests"]
fn qualification_kill_child() -> varve::Result<()> {
    let root = std::env::var_os("VARVE_QUAL_CHILD_ROOT").expect("parent root required");
    let root = Path::new(&root);
    let epoch = configured("VARVE_QUAL_CHILD_EPOCH", 0);
    assert!(epoch > 0);
    let mut writer = Qualification::open_indexed_writer(root.join("killed.varve"), options())?;
    let items: Vec<_> = (0..KEYS).map(|number| item(number, epoch)).collect();
    writer
        .push_items(&items, BatchOptions::default())
        .map_err(|e| e.source)?;
    for number in (0..KEYS).step_by(3) {
        writer.delete_item(&key(number))?;
    }
    if epoch.is_multiple_of(2) {
        writer.sync()?;
    } else {
        writer.flush()?;
    }
    let mut ready = File::create(root.join("ready"))?;
    writeln!(ready, "{epoch}")?;
    ready.sync_all()?;
    loop {
        std::thread::park();
    }
}

struct KillOnDrop(Child);
impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn kill_cycles(cycles: u64) -> varve::Result<()> {
    assert!(cycles >= 4);
    let directory = tempfile::tempdir()?;
    let root = directory.path();
    let path = root.join("killed.varve");
    Qualification::create_indexed_writer(&path, options())?.sync()?;
    let mut expected = Model::new();
    for epoch in 1..=cycles {
        let old_eof = fs::metadata(&path)?.len();
        let log = File::create(root.join(format!("child-{epoch}.log")))?;
        let mut child = KillOnDrop(
            Command::new(std::env::current_exe()?)
                .args([
                    "--exact",
                    "qualification_kill_child",
                    "--ignored",
                    "--nocapture",
                ])
                .env("VARVE_QUAL_CHILD_ROOT", root)
                .env("VARVE_QUAL_CHILD_EPOCH", epoch.to_string())
                .stdout(Stdio::from(log.try_clone()?))
                .stderr(Stdio::from(log))
                .spawn()?,
        );
        let started = Instant::now();
        while !root.join("ready").exists()
            || fs::read_to_string(root.join("ready"))? != format!("{epoch}\n")
        {
            assert!(
                child.0.try_wait()?.is_none(),
                "kill child exited: {}",
                fs::read_to_string(root.join(format!("child-{epoch}.log")))?
            );
            assert!(
                started.elapsed() < WAIT,
                "kill child timeout at epoch={epoch}"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        child.0.kill()?;
        assert!(
            !child.0.wait()?.success(),
            "child must be terminated externally"
        );
        clear_stale_writer_lock(&path, WriterLockBreakPolicy::BreakIfProcessAbsent)?;
        if epoch.is_multiple_of(2) {
            expected = (0..KEYS)
                .filter(|n| n % 3 != 0)
                .map(|n| (key(n), item(n, epoch).payload))
                .collect();
        } else {
            check_dirty_reader(&path, &expected);
            drop(Qualification::restore_indexed_writer(&path, options())?);
            assert_eq!(fs::metadata(&path)?.len(), old_eof);
        }
        let reader = Qualification::open_indexed_reader(&path, options())?;
        check(&reader, &expected)?;
        reader.verify_all()?;
        drop(reader);
        fs::remove_file(root.join("ready"))?;
        eprintln!(
            "QUAL_KILL epoch={epoch} durable={}",
            epoch.is_multiple_of(2)
        );
    }
    Ok(())
}

#[test]
fn repeated_external_kill_preserves_acknowledged_generations() -> varve::Result<()> {
    kill_cycles(4)
}

#[test]
#[ignore = "long external kill/recovery campaign; scripts/qualify_scalable.py"]
fn external_kill_stress() -> varve::Result<()> {
    kill_cycles(configured("VARVE_QUAL_KILL_CYCLES", 64))
}

#[test]
fn opening_readers_does_not_interrupt_the_single_writer() -> varve::Result<()> {
    use std::sync::atomic::{AtomicBool, Ordering};
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("reader-churn.varve");
    let mut writer = Qualification::create_indexed_writer(&path, options())?;
    writer.push_item(&item(0, 0))?;
    writer.sync()?;
    let stop = AtomicBool::new(false);
    std::thread::scope(|scope| -> varve::Result<()> {
        let workers: Vec<_> = (0..8)
            .map(|_| {
                let path = &path;
                let stop = &stop;
                scope.spawn(move || {
                    let mut opens = 0;
                    while !stop.load(Ordering::Acquire) {
                        match Qualification::open_indexed_reader(path, options()) {
                            Ok(reader) => {
                                assert_eq!(
                                    reader
                                        .get_item(&key(0))
                                        .expect("opened snapshot must remain readable")
                                        .map(|value| value.payload),
                                    Some(item(0, 0).payload),
                                );
                                opens += 1;
                            }
                            Err(error) => panic!("unexpected reader-open failure: {error:?}"),
                        }
                    }
                    opens
                })
            })
            .collect();
        let result = (|| -> varve::Result<()> {
            for epoch in 1..=100 {
                writer.push_item(&item(epoch % KEYS, epoch))?;
                writer.sync()?;
            }
            Ok(())
        })();
        stop.store(true, Ordering::Release);
        let opened: usize = workers
            .into_iter()
            .map(|w| w.join().expect("reader panicked"))
            .sum();
        eprintln!("concurrent reader opens={opened}, writer result={result:?}");
        assert!(opened > 0, "reader-open contention was not exercised");
        result
    })
}
