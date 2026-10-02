//! Thread ownership, explicit snapshot retention, and cold-open writer admission.

use std::sync::{
    Barrier,
    atomic::{AtomicBool, Ordering},
};
use varve::{BatchOptions, DiskIndexError, DiskIndexOptions, Error, StreamOptions, varve_format};

varve_format! {
    pub format Managed {
        magic: b"MGED";
        version: 1;
        schema_hash: computed;
        index: keyed_offset_chain;
        blocks { fixed Item(id = 1, key = [key], key_index = disk) { key: u64, value: u64 } }
    }
}
varve_format! {
    pub format Feed {
        magic: b"MGFD";
        version: 1;
        schema_hash: computed;
        index: block_offset_chain;
        blocks { fixed Sample(id = 1) { value: u64 } }
    }
}

#[test]
fn moving_reader_with_live_local_cursors_keeps_positions_independent() -> varve::Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("positions.varve");
    let options = DiskIndexOptions::default();
    let mut writer = Managed::create_indexed_writer(&path, options)?;
    writer
        .push_items(
            (0..10000).map(|key| Item {
                key,
                value: key * 3,
            }),
            BatchOptions::default(),
        )
        .map_err(|e| e.source)?;
    writer.immediate()?;
    let reader = Managed::open_indexed_reader(&path, options)?;
    let mut cursors = (0..8)
        .map(|_| reader.items())
        .collect::<varve::Result<Vec<_>>>()?;
    let barrier = Barrier::new(2);
    std::thread::scope(|scope| -> varve::Result<()> {
        let worker = scope.spawn(|| -> varve::Result<()> {
            // Move the reader, never a cursor or a shared reader reference.
            let reader = reader;
            barrier.wait();
            for key in 0..10000 {
                assert_eq!(
                    reader.get_item(&key)?,
                    Some(Item {
                        key,
                        value: key * 3
                    })
                );
            }
            reader.verify_all()?;
            Ok(())
        });
        barrier.wait();
        for key in 0..10000 {
            for cursor in &mut cursors {
                assert_eq!(
                    cursor.next().transpose()?,
                    Some(Item {
                        key,
                        value: key * 3
                    })
                );
            }
        }
        for cursor in &mut cursors {
            assert!(cursor.next().is_none());
        }
        worker.join().expect("reader panicked")
    })
}

#[test]
fn indexed_snapshot_release_reports_lag_and_reacquires_while_dirty() -> varve::Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("retention.varve");
    let options = DiskIndexOptions::default();
    let mut writer = Managed::create_indexed_writer(&path, options)?;
    writer.push_item(&Item { key: 1, value: 1 })?;
    writer.immediate()?;
    let mut old = Managed::open_indexed_reader(&path, options)?;
    let mut idle = Managed::open_indexed_reader(&path, options)?;
    let cursor = idle.items()?;
    let before = old.snapshot_status()?;
    assert_eq!(writer.snapshot_retention()?.active_snapshots, 2);
    for value in 2..=8 {
        writer.push_item(&Item { key: 1, value })?;
        writer.immediate()?;
    }
    let lag = old.snapshot_status()?;
    assert_eq!(lag.generation, before.generation);
    assert_eq!(lag.confirmed_generation - lag.generation, 7);
    assert_eq!(lag.records_behind, 7);
    assert!(lag.bytes_behind > 0);
    let retention = writer.snapshot_retention()?;
    assert_eq!(retention.oldest_pinned_generation, Some(before.generation));
    assert!(retention.sidecar_bytes > 0);
    assert!(idle.release_snapshot());
    assert!(!idle.release_snapshot());
    assert!(!idle.snapshot_status()?.pinned);
    assert_eq!(writer.snapshot_retention()?.active_snapshots, 1);
    assert!(
        matches!(idle.get_item(&1), Err(Error::DiskIndex(error)) if matches!(*error, DiskIndexError::SnapshotReleased))
    );
    assert_eq!(
        cursor.collect::<varve::Result<Vec<_>>>()?,
        vec![Item { key: 1, value: 1 }]
    );
    writer.push_item(&Item { key: 1, value: 9 })?; // Unconfirmed remains invisible.
    assert_eq!(idle.follow()?, 7);
    assert_eq!(idle.get_item(&1)?.unwrap().value, 8);
    assert!(idle.snapshot_status()?.pinned);
    assert!(idle.release_snapshot());
    assert_eq!(idle.follow()?, 0); // Re-pin even without a new generation.
    assert_eq!(idle.get_item(&1)?.unwrap().value, 8);
    assert_eq!(writer.snapshot_retention()?.active_snapshots, 2);
    assert_eq!(old.follow()?, 7);
    assert_eq!(
        writer.snapshot_retention()?.oldest_pinned_generation,
        Some(lag.confirmed_generation)
    );
    drop(old);
    drop(idle);
    assert_eq!(writer.snapshot_retention()?.active_snapshots, 0);
    writer.immediate()?;
    Ok(())
}

#[test]
fn stream_release_preserves_cursor_and_follow_releases_old_generation() -> varve::Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("stream.varve");
    let options = StreamOptions::default();
    let mut writer = Feed::create_stream_writer(&path, options)?;
    writer.push_sample(&Sample { value: 1 })?;
    writer.immediate()?;
    let mut reader = Feed::open_stream_reader(&path, options)?;
    let mut cursor = reader.samples()?;
    assert_eq!(writer.snapshot_retention()?.active_snapshots, 1);
    assert!(reader.release_snapshot());
    assert_eq!(writer.snapshot_retention()?.active_snapshots, 0);
    assert_eq!(cursor.next().transpose()?, Some(Sample { value: 1 }));
    writer.push_sample(&Sample { value: 2 })?;
    writer.immediate()?;
    writer.push_sample(&Sample { value: 3 })?;
    assert_eq!(reader.snapshot_status()?.records_behind, 1);
    assert_eq!(reader.follow_blocks(&mut cursor)?, 1);
    assert_eq!(cursor.next().transpose()?, Some(Sample { value: 2 }));
    assert!(cursor.next().is_none());
    assert!(reader.release_snapshot());
    assert_eq!(reader.follow()?, 0);
    assert!(reader.snapshot_status()?.pinned);
    Ok(())
}

#[test]
fn cold_reader_retries_never_reject_or_interrupt_the_single_writer() -> varve::Result<()> {
    let dir = tempfile::tempdir()?;
    let options = DiskIndexOptions::default();
    for trial in 0..32 {
        let path = dir.path().join(format!("cold-{trial}.varve"));
        let mut writer = Managed::create_indexed_writer(&path, options)?;
        writer.push_item(&Item { key: 1, value: 0 })?;
        writer.immediate()?;
        drop(writer);
        let start = Barrier::new(9);
        let done = AtomicBool::new(false);
        std::thread::scope(|scope| -> varve::Result<()> {
            let workers: Vec<_> = (0..8)
                .map(|_| {
                    scope.spawn(|| -> varve::Result<()> {
                        start.wait();
                        let deadline =
                            std::time::Instant::now() + std::time::Duration::from_secs(10);
                        loop {
                            match Managed::open_indexed_reader(&path, options) {
                                Ok(mut reader) => {
                                    reader.follow()?;
                                    assert!(reader.get_item(&1)?.unwrap().value <= 16);
                                }
                                Err(Error::IndexBusy) if std::time::Instant::now() < deadline => {
                                    std::thread::yield_now();
                                    continue;
                                }
                                Err(error) => return Err(error),
                            }
                            if done.load(Ordering::Acquire) {
                                return Ok(());
                            }
                        }
                    })
                })
                .collect();
            start.wait();
            let result = (|| -> varve::Result<()> {
                // No application retry around writer open, append, or sync.
                let mut writer = Managed::open_indexed_writer(&path, options)?;
                for value in 1..=16 {
                    writer.push_item(&Item { key: 1, value })?;
                    writer.immediate()?;
                }
                Ok(())
            })();
            done.store(true, Ordering::Release);
            for worker in workers {
                worker.join().expect("reader panicked")?;
            }
            result
        })?;
    }
    Ok(())
}

#[test]
fn compaction_keeps_old_readers_and_follow_adopts_new_file() -> varve::Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("compact.varve");
    let options = DiskIndexOptions::default();
    let mut writer = Managed::create_indexed_writer(&path, options)?;
    writer
        .push_items(
            (0..200).map(|key| Item { key, value: 1 }),
            BatchOptions::default(),
        )
        .map_err(|e| e.source)?;
    writer.immediate()?;
    let mut old = Managed::open_indexed_reader(&path, options)?;
    let mut cursor = old.items()?;
    for value in 2..20 {
        writer
            .push_items(
                (0..200).map(|key| Item { key, value }),
                BatchOptions::default(),
            )
            .map_err(|e| e.source)?;
        writer.immediate()?;
    }
    let mut current = Managed::open_indexed_reader(&path, options)?;
    let report = writer.compact_index()?;
    assert_eq!(current.follow()?, 0); // Same logical generation, new physical file.
    assert_eq!(
        current.snapshot_retention()?.sidecar_bytes,
        report.after_bytes
    );
    assert_eq!(writer.snapshot_retention()?.active_snapshots, 1);
    drop(current);
    assert!(report.after_bytes < report.before_bytes);
    assert_eq!(report.historical_distinct_keys, 200);
    for key in 0..200 {
        assert_eq!(old.get_item(&key)?.unwrap().value, 1);
    }
    assert_eq!(cursor.next().transpose()?.unwrap().value, 1);
    writer.push_item(&Item { key: 0, value: 20 })?;
    assert!(writer.compact_index().is_err()); // Never inserts an implicit sync.
    old.follow()?;
    assert_eq!(old.get_item(&0)?.unwrap().value, 19);
    writer.immediate()?;
    old.follow()?;
    assert_eq!(old.get_item(&0)?.unwrap().value, 20);
    drop(writer);
    let mut reopened = Managed::open_indexed_writer(&path, options)?;
    reopened.push_item(&Item { key: 0, value: 21 })?;
    reopened.immediate()?;
    old.follow()?;
    assert_eq!(old.get_item(&0)?.unwrap().value, 21);
    Ok(())
}

#[test]
#[ignore = "cross-process reader subprocess"]
fn independent_process_reader_child() -> varve::Result<()> {
    let path = std::path::PathBuf::from(std::env::var_os("VARVE_NATIVE_READER_CHILD").unwrap());
    let options = DiskIndexOptions::default();
    let mut reader = Managed::open_indexed_reader(&path, options)?;
    let pinned = Managed::open_indexed_reader(&path, options)?;
    assert_eq!(reader.get_item(&0)?.unwrap().value, 1);
    std::fs::write(path.with_extension("ready"), b"ready")?;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    let mut checks = 0;
    loop {
        reader.follow()?;
        let a = reader.get_item(&0)?.unwrap().value;
        let b = reader.get_item(&1)?.unwrap().value;
        assert_eq!(a, b, "mixed confirmed generations");
        assert_eq!(pinned.get_item(&0)?.unwrap().value, 1);
        checks += 1;
        if path.with_extension("done").exists() {
            reader.follow()?;
            assert_eq!(reader.get_item(&0)?.unwrap().value, 257);
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "writer did not finish"
        );
        std::thread::yield_now();
    }
    assert!(checks > 0);
    reader.verify_all()?;
    Ok(())
}

#[test]
fn independent_process_reads_dirty_writer_and_follows_compaction() -> varve::Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("process.varve");
    let options = DiskIndexOptions::default();
    let mut writer = Managed::create_indexed_writer(&path, options)?;
    for key in 0..2 {
        writer.push_item(&Item { key, value: 1 })?;
    }
    writer.immediate()?;
    writer.push_item(&Item { key: 0, value: 2 })?; // Child opens during Dirty.
    let mut child = std::process::Command::new(std::env::current_exe()?)
        .args([
            "--ignored",
            "--exact",
            "independent_process_reader_child",
            "--nocapture",
        ])
        .env("VARVE_NATIVE_READER_CHILD", &path)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()?;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    while !path.with_extension("ready").exists() {
        if let Some(status) = child.try_wait()? {
            panic!("reader exited before open: {status}");
        }
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            panic!("reader open blocked");
        }
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    writer.push_item(&Item { key: 1, value: 2 })?;
    writer.immediate()?;
    for value in 3..=257 {
        for key in 0..2 {
            writer.push_item(&Item { key, value })?;
        }
        writer.immediate()?;
        if value % 64 == 0 {
            writer.compact_index()?;
        }
    }
    std::fs::write(path.with_extension("done"), b"done")?;
    loop {
        if let Some(status) = child.try_wait()? {
            let output = child.wait_with_output()?;
            assert!(
                status.success(),
                "child failed:\n{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            break;
        }
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            panic!("reader did not finish");
        }
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    Ok(())
}
