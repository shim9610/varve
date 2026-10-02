//! Confirmed generations stay readable while a writer is Dirty. Following
//! updates a handle explicitly and can resume existing parsers without replay.

use std::sync::{
    Barrier,
    atomic::{AtomicBool, Ordering},
};
use varve::{
    BatchOptions, DiskIndexBatchOptions, DiskIndexOptions, Error, StreamOptions, varve_format,
};

varve_format! {
    pub format Live {
        magic: b"LIVE";
        version: 1;
        schema_hash: computed;
        index: keyed_offset_chain;
        blocks {
            fixed Item(id = 1, key = [key], key_index = disk) { key: u64, value: u64 }
        }
    }
}

varve_format! {
    pub format Feed {
        magic: b"FEED";
        version: 1;
        schema_hash: computed;
        index: block_offset_chain;
        blocks { fixed Sample(id = 1) { value: u64 } }
    }
}

fn options() -> DiskIndexOptions {
    DiskIndexOptions {
        cache_bytes: 1024 * 1024,
        max_key_bytes: 128,
        batch: DiskIndexBatchOptions {
            max_records: 2,
            max_bytes: 1024,
        },
        ..DiskIndexOptions::default()
    }
}

#[test]
fn dirty_overwrites_and_deletes_preserve_the_confirmed_index() -> varve::Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("dirty.varve");
    let mut writer = Live::create_indexed_writer(&path, options())?;
    writer
        .push_items(
            (0..3).map(|key| Item { key, value: 0 }),
            BatchOptions::default(),
        )
        .map_err(|e| e.source)?;
    writer.immediate()?;
    let mut reader = Live::open_indexed_reader(&path, options())?;
    let mut blocks = reader.items()?;
    let frozen = reader.items()?;
    assert_eq!(blocks.by_ref().collect::<varve::Result<Vec<_>>>()?.len(), 3);
    let eof = reader.committed_len();
    writer.push_item(&Item { key: 0, value: 1 })?;
    writer.delete_item(&1)?;
    writer.push_item(&Item { key: 3, value: 1 })?;
    writer.flush()?;
    let mut during = Live::open_indexed_reader(&path, options())?;
    for view in [&reader, &during] {
        assert_eq!(view.committed_len(), eof);
        assert_eq!(view.get_item(&0)?.unwrap().value, 0);
        assert_eq!(view.get_item(&1)?.unwrap().value, 0);
        assert!(view.get_item(&3)?.is_none());
    }
    assert_eq!(reader.follow_blocks(&mut blocks)?, 0);
    assert!(blocks.next().is_none());
    writer.immediate()?;
    // Return to Dirty before either existing reader follows the new checkpoint.
    writer.push_item(&Item { key: 0, value: 2 })?;
    assert_eq!(reader.follow_blocks(&mut blocks)?, 3);
    assert_eq!(during.follow()?, 3);
    for view in [&reader, &during] {
        assert_eq!(view.get_item(&0)?.unwrap().value, 1);
        assert!(view.get_item(&1)?.is_none());
        assert_eq!(view.get_item(&3)?.unwrap().value, 1);
    }
    assert_eq!(
        blocks.collect::<varve::Result<Vec<_>>>()?,
        vec![Item { key: 0, value: 1 }, Item { key: 3, value: 1 }]
    );
    assert_eq!(
        frozen.collect::<varve::Result<Vec<_>>>()?,
        (0..3).map(|key| Item { key, value: 0 }).collect::<Vec<_>>()
    );
    Ok(())
}

#[test]
fn stream_cursors_resume_from_eof_across_dirty_generations() -> varve::Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("feed.varve");
    let opts = StreamOptions::default();
    let mut writer = Feed::create_stream_writer(&path, opts)?;
    writer.push_sample(&Sample { value: 0 })?;
    let mut reader = Feed::open_stream_reader(&path, opts)?;
    let mut blocks = reader.samples()?;
    let mut events = reader.events()?;
    let initial_events = events.by_ref().collect::<varve::Result<Vec<_>>>()?.len();
    assert!(blocks.next().is_none());
    let mut all = Vec::new();
    let mut event_count = initial_events;
    for value in 0..40 {
        assert_eq!(reader.follow_blocks(&mut blocks)?, 0);
        assert!(blocks.next().is_none());
        writer.immediate()?;
        writer.push_sample(&Sample { value: value + 1 })?;
        assert_eq!(reader.follow_blocks(&mut blocks)?, 1);
        all.extend(blocks.by_ref().collect::<varve::Result<Vec<_>>>()?);
        // A cursor can catch up after follow() already advanced the reader.
        assert_eq!(reader.follow_events(&mut events)?, 0);
        event_count += events.by_ref().collect::<varve::Result<Vec<_>>>()?.len();
    }
    assert_eq!(
        all,
        (0..40).map(|value| Sample { value }).collect::<Vec<_>>()
    );
    assert_eq!(event_count, initial_events + 40);
    assert_eq!(reader.verify_all()? as usize, event_count);
    Ok(())
}

#[test]
fn readers_open_and_follow_inside_an_active_write_batch() -> varve::Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("batch.varve");
    let mut writer = Live::create_indexed_writer(&path, options())?;
    writer.push_item(&Item { key: 0, value: 7 })?;
    writer.immediate()?;
    let mut retained = Live::open_indexed_reader(&path, options())?;
    let values = (0..25).map(|key| {
        let reader = Live::open_indexed_reader(&path, options()).unwrap();
        assert_eq!(reader.get_item(&0).unwrap().unwrap().value, 7);
        assert!(reader.get_item(&1).unwrap().is_none());
        assert_eq!(retained.follow().unwrap(), 0);
        Item { key, value: 8 }
    });
    writer
        .push_items(
            values,
            BatchOptions {
                max_records: 1,
                max_bytes: 1,
            },
        )
        .map_err(|e| e.source)?;
    assert_eq!(retained.follow()?, 0);
    writer.immediate()?;
    assert_eq!(retained.follow()?, 25);
    assert_eq!(retained.get_item(&0)?.unwrap().value, 8);
    Ok(())
}

#[test]
fn dirty_reopen_and_restore_do_not_invalidate_existing_readers() -> varve::Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("recover.varve");
    let mut writer = Live::create_indexed_writer(&path, options())?;
    writer.push_item(&Item { key: 0, value: 1 })?;
    writer.immediate()?;
    writer.push_item(&Item { key: 0, value: 2 })?;
    writer.flush()?;
    drop(writer);
    let dirty_length = std::fs::metadata(&path)?.len();
    let mut reader = Live::open_indexed_reader(&path, options())?;
    assert_eq!(reader.get_item(&0)?.unwrap().value, 1);
    assert_eq!(std::fs::metadata(&path)?.len(), dirty_length);
    let mut cursor = reader.items()?;
    assert_eq!(cursor.next().unwrap()?.value, 1);
    assert!(cursor.next().is_none());
    let mut writer = Live::restore_indexed_writer(&path, options())?;
    assert!(std::fs::metadata(&path)?.len() < dirty_length);
    assert_eq!(reader.follow()?, 0);
    writer.push_item(&Item { key: 0, value: 3 })?;
    writer.immediate()?;
    assert_eq!(reader.follow_blocks(&mut cursor)?, 1);
    assert_eq!(cursor.next().unwrap()?.value, 3);
    assert!(cursor.next().is_none());
    assert_eq!(reader.get_item(&0)?.unwrap().value, 3);
    Ok(())
}

#[test]
fn foreign_cursor_is_rejected_before_the_reader_changes() -> varve::Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("foreign.varve");
    let mut writer = Feed::create_stream_writer(&path, StreamOptions::default())?;
    let mut reader = Feed::open_stream_reader(&path, StreamOptions::default())?;
    let other = Feed::open_stream_reader(&path, StreamOptions::default())?;
    let mut cursor = other.samples()?;
    let eof = reader.committed_len();
    writer.push_sample(&Sample { value: 1 })?;
    writer.immediate()?;
    assert!(matches!(
        reader.follow_blocks(&mut cursor),
        Err(Error::InvalidFollowCursor(_))
    ));
    assert_eq!(reader.committed_len(), eof);
    Ok(())
}

#[test]
fn eight_followers_observe_atomic_generations_during_single_writer_updates() -> varve::Result<()> {
    const KEYS: u64 = 16;
    const EPOCHS: u64 = 100;
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("concurrent.varve");
    let mut writer = Live::create_indexed_writer(&path, options())?;
    writer
        .push_items(
            (0..KEYS).map(|key| Item { key, value: 0 }),
            BatchOptions::default(),
        )
        .map_err(|e| e.source)?;
    writer.immediate()?;
    let stop = AtomicBool::new(false);
    let barrier = Barrier::new(9);
    struct StopOnDrop<'a>(&'a AtomicBool);
    impl Drop for StopOnDrop<'_> {
        fn drop(&mut self) {
            self.0.store(true, Ordering::Release);
        }
    }
    std::thread::scope(|scope| -> varve::Result<()> {
        let _stop_on_panic = StopOnDrop(&stop);
        let workers: Vec<_> = (0..8)
            .map(|_| {
                let path = &path;
                let stop = &stop;
                let barrier = &barrier;
                scope.spawn(move || -> varve::Result<u64> {
                    let mut reader = Live::open_indexed_reader(path, options())?;
                    let mut cursor = reader.items()?;
                    let mut parsed = 0;
                    let mut reads = 0;
                    let mut previous = 0;
                    barrier.wait();
                    loop {
                        let done = stop.load(Ordering::Acquire);
                        reader.follow_blocks(&mut cursor)?;
                        let generation = reader.get_item(&0)?.unwrap().value;
                        assert!(generation >= previous);
                        for key in 0..KEYS {
                            assert_eq!(reader.get_item(&key)?.unwrap().value, generation);
                        }
                        for value in cursor.by_ref() {
                            let value = value?;
                            assert_eq!(value.key, parsed % KEYS);
                            assert_eq!(value.value, parsed / KEYS);
                            parsed += 1;
                        }
                        assert_eq!(parsed, (generation + 1) * KEYS);
                        let fresh = Live::open_indexed_reader(path, options())?;
                        let fresh_generation = fresh.get_item(&0)?.unwrap().value;
                        for key in 1..KEYS {
                            assert_eq!(fresh.get_item(&key)?.unwrap().value, fresh_generation);
                        }
                        previous = generation;
                        reads += 1;
                        if done {
                            assert_eq!(generation, EPOCHS);
                            break;
                        }
                    }
                    Ok(reads)
                })
            })
            .collect();
        barrier.wait();
        let result = (|| -> varve::Result<()> {
            for value in 1..=EPOCHS {
                writer
                    .push_items(
                        (0..KEYS).map(|key| Item { key, value }),
                        BatchOptions {
                            max_records: 1,
                            max_bytes: 1,
                        },
                    )
                    .map_err(|e| e.source)?;
                writer.immediate()?;
            }
            Ok(())
        })();
        stop.store(true, Ordering::Release);
        let mut reads = 0;
        for worker in workers {
            reads += worker.join().expect("reader panicked")?;
        }
        result?;
        assert!(reads >= 8);
        eprintln!("FOLLOW_CONCURRENT readers=8 generations={EPOCHS} coherent_reads={reads}");
        Ok(())
    })
}

#[test]
fn follow_retains_scan_limits_and_cannot_revive_a_failed_cursor() -> varve::Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("limit.varve");
    let mut writer = Feed::create_stream_writer(&path, StreamOptions::default())?;
    let opts = StreamOptions {
        limits: varve::ResourceLimits::MISSING.with_max_records(3),
        ..StreamOptions::default()
    };
    let mut reader = Feed::open_stream_reader(&path, opts)?;
    let mut cursor = reader.events()?;
    let mut seen = cursor.by_ref().collect::<varve::Result<Vec<_>>>()?.len();
    while seen < 3 {
        writer.push_sample(&Sample { value: seen as u64 })?;
        writer.immediate()?;
        reader.follow_events(&mut cursor)?;
        seen += cursor.by_ref().collect::<varve::Result<Vec<_>>>()?.len();
    }
    writer.push_sample(&Sample { value: 4 })?;
    writer.immediate()?;
    reader.follow_events(&mut cursor)?;
    assert!(cursor.next().unwrap().is_err());
    assert!(matches!(
        reader.follow_events(&mut cursor),
        Err(Error::InvalidFollowCursor(_))
    ));
    Ok(())
}
