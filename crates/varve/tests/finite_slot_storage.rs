#![cfg(feature = "integrity")]
use std::{fs, path::Path};
use varve::{BatchOptions, DiskIndexOptions, varve_format};
varve_format! {
    pub format Direct {
        magic:b"DIRECT";version:1;schema_hash:computed;index:keyed_offset_chain;integrity:crc32;
        blocks {
            fixed Slot(id=1,key=[scan,frame],key_index=disk,key_domain=[scan=0..64,frame=0..64]) {
                scan:u16,frame:u16,value:u64,
            }
            fixed Legacy(id=2,key=[id],key_index=disk) {id:u64,value:u64}
        }
    }
}
fn options() -> DiskIndexOptions {
    DiskIndexOptions {
        cache_bytes: 1024 * 1024,
        max_key_bytes: 64,
        batch: varve::DiskIndexBatchOptions {
            max_records: 2,
            max_bytes: 1024,
        },
        ..DiskIndexOptions::default()
    }
}
fn key(code: u16) -> SlotKey {
    SlotKey::from_code(code).unwrap()
}
fn checkpoint(path: &Path) -> Vec<u8> {
    let bytes = fs::read(path.with_extension("varve.vki")).unwrap();
    assert_eq!(&bytes[..8], b"VARVEIX5");
    let offset = [4096usize, 8192]
        .into_iter()
        .filter(|s| &bytes[*s..*s + 8] == b"VIXHEAD1")
        .map(|s| u64::from_le_bytes(bytes[s + 8..s + 16].try_into().unwrap()) as usize)
        .max()
        .unwrap();
    bytes[offset..offset + 352].to_vec()
}
fn u64_at(bytes: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes(bytes[offset..offset + 8].try_into().unwrap())
}
#[test]
fn finite_only_has_no_tree_and_flush_does_not_publish_slot_extents() -> varve::Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("data.varve");
    let options = options();
    let mut writer = Direct::create_indexed_writer(&path, options)?;
    writer.push_slot(&Slot {
        key: key(0),
        value: 1,
    })?;
    writer.sync()?;
    let initial = checkpoint(&path);
    assert_eq!(u64_at(&initial, 16), 0);
    assert_eq!(u64_at(&initial, 24), 0);
    let mut reader = Direct::open_indexed_reader(&path, options)?;
    for value in 2..100 {
        writer.push_slot(&Slot {
            key: key((value % 32) as u16),
            value,
        })?;
        writer.flush()?;
        assert_eq!(u64_at(&checkpoint(&path), 336), u64_at(&initial, 336));
        assert_eq!(reader.get_slot(&key(0))?.unwrap().value, 1);
    }
    writer.sync()?;
    reader.follow()?;
    let next = checkpoint(&path);
    assert_eq!(u64_at(&next, 16), 0);
    assert_ne!(u64_at(&next, 336), u64_at(&initial, 336));
    assert_eq!(reader.get_slot(&key(0))?.unwrap().value, 96);
    writer.delete_slot(&key(0))?;
    writer.immediate()?;
    reader.follow()?;
    assert!(reader.get_slot(&key(0))?.is_none());
    writer.compact_index()?;
    reader.follow()?;
    assert_eq!(u64_at(&checkpoint(&path), 16), 0);
    assert!(reader.get_slot(&key(0))?.is_none());
    Ok(())
}
#[test]
fn mixed_tables_restore_rebuild_and_continue_after_compaction() -> varve::Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("data.varve");
    let options = options();
    drop(Direct::create_indexed_writer(&path, options)?);
    // Rebuilding an empty file commits a zero-record batch. Its writer-owned
    // table must remain available to publication and the next write.
    Direct::rebuild_disk_index(&path, options)?;
    let mut writer = Direct::open_indexed_writer(&path, options)?;
    writer.push_slot(&Slot {
        key: key(4095),
        value: 10,
    })?;
    writer.push_legacy(&Legacy { id: 99, value: 10 })?;
    writer.sync()?;
    writer.push_slot(&Slot {
        key: key(4095),
        value: 20,
    })?;
    writer.delete_legacy(&99)?;
    writer.flush()?;
    drop(writer);
    let reader = Direct::open_indexed_reader(&path, options)?;
    assert_eq!(reader.get_slot(&key(4095))?.unwrap().value, 10);
    assert_eq!(reader.get_legacy(&99)?.unwrap().value, 10);
    let mut writer = Direct::restore_indexed_writer(&path, options)?;
    writer.compact_index()?;
    let info = writer.push_slot(&Slot {
        key: key(4095),
        value: 30,
    })?;
    assert!(info.prev_same_key_offset.is_some());
    writer.sync()?;
    drop(writer);
    Direct::rebuild_disk_index(&path, options)?;
    let reader = Direct::open_indexed_reader(&path, options)?;
    assert_eq!(reader.get_slot(&key(4095))?.unwrap().value, 30);
    assert_eq!(reader.get_legacy(&99)?.unwrap().value, 10);
    let root = checkpoint(&path);
    assert_ne!(u64_at(&root, 16), 0);
    assert_ne!(u64_at(&root, 336), 0);
    Ok(())
}
#[test]
fn corrupt_slot_chunk_is_rejected_before_returning_a_record() -> varve::Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("data.varve");
    let options = options();
    let mut writer = Direct::create_indexed_writer(&path, options)?;
    writer.push_slot(&Slot {
        key: key(0),
        value: 10,
    })?;
    writer.sync()?;
    drop(writer);
    let cp = checkpoint(&path);
    let sidecar = path.with_extension("varve.vki");
    let mut bytes = fs::read(&sidecar)?;
    let directory = u64_at(&cp, 336) as usize;
    assert_eq!(&bytes[directory..directory + 8], b"VIXSLOT1");
    let chunk = u64_at(&bytes, directory + 40) as usize;
    bytes[chunk + 6] ^= 1;
    fs::write(&sidecar, bytes)?;
    let reader = Direct::open_indexed_reader(&path, options)?;
    assert!(reader.get_slot(&key(0)).is_err());
    Ok(())
}

#[test]
fn randomized_full_domain_matches_redb_through_reopen_and_compaction() -> varve::Result<()> {
    use redb::{ReadableDatabase, TableDefinition};
    const TABLE: TableDefinition<u16, u64> = TableDefinition::new("slots");
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("random.varve");
    let opts = options();
    let oracle = redb::Database::create(dir.path().join("oracle.redb")).unwrap();
    let mut writer = Direct::create_indexed_writer(&path, opts)?;
    writer.sync()?;
    let mut reader = Direct::open_indexed_reader(&path, opts)?;
    let mut model = [None; 4096];
    let mut random = 0x5165_9f17_842a_bcd1u64;
    for generation in 0..24 {
        let old = model;
        let transaction = oracle.begin_write().unwrap();
        {
            let mut table = transaction.open_table(TABLE).unwrap();
            for _ in 0..96 {
                random ^= random << 13;
                random ^= random >> 7;
                random ^= random << 17;
                let code = (random % 4096) as u16;
                if random >> 32 & 3 == 0 {
                    writer.delete_slot(&key(code))?;
                    table.remove(code).unwrap();
                    model[code as usize] = None;
                } else {
                    writer.push_slot(&Slot {
                        key: key(code),
                        value: random,
                    })?;
                    table.insert(code, random).unwrap();
                    model[code as usize] = Some(random);
                }
            }
        }
        transaction.commit().unwrap();
        writer.sync()?;
        if generation % 4 == 3 {
            writer.compact_index()?;
            drop(writer);
            writer = Direct::open_indexed_writer(&path, opts)?;
        }
        // Both unchanged chunks and replaced chunks must keep the old snapshot.
        for (code, expected) in old.into_iter().enumerate() {
            assert_eq!(
                reader.get_slot(&key(code as u16))?.map(|row| row.value),
                expected
            );
        }
        reader.follow()?;
        let transaction = oracle.begin_read().unwrap();
        let table = transaction.open_table(TABLE).unwrap();
        for (code, expected) in model.into_iter().enumerate() {
            let actual = reader.get_slot(&key(code as u16))?.map(|row| row.value);
            assert_eq!(actual, expected);
            assert_eq!(
                actual,
                table.get(code as u16).unwrap().map(|value| value.value())
            );
        }
    }
    Ok(())
}
#[test]
fn independent_readers_follow_atomic_slot_generations() -> varve::Result<()> {
    use std::sync::{
        Barrier,
        atomic::{AtomicBool, Ordering},
    };
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("data.varve");
    let options = options();
    let mut writer = Direct::create_indexed_writer(&path, options)?;
    writer
        .push_slots(
            (0..32).map(|code| Slot {
                key: key(code),
                value: 0,
            }),
            BatchOptions::default(),
        )
        .map_err(|e| e.source)?;
    writer.sync()?;
    let ready = Barrier::new(5);
    let done = AtomicBool::new(false);
    std::thread::scope(|scope| {
        let threads: Vec<_> = (0..4)
            .map(|_| {
                scope.spawn(|| {
                    let mut reader = Direct::open_indexed_reader(&path, options).unwrap();
                    ready.wait();
                    let mut reads = 0;
                    loop {
                        reader.follow().unwrap();
                        let generation = reader.get_slot(&key(0)).unwrap().unwrap().value;
                        for code in 1..32 {
                            assert_eq!(
                                reader.get_slot(&key(code)).unwrap().unwrap().value,
                                generation
                            );
                            reads += 1;
                        }
                        if done.load(Ordering::Acquire) {
                            break;
                        }
                    }
                    reads
                })
            })
            .collect();
        ready.wait();
        for generation in 1..=50 {
            writer
                .push_slots(
                    (0..32).map(|code| Slot {
                        key: key(code),
                        value: generation,
                    }),
                    BatchOptions::default(),
                )
                .unwrap();
            writer.sync().unwrap();
            if generation % 10 == 0 {
                writer.compact_index().unwrap();
            }
        }
        done.store(true, Ordering::Release);
        let reads: usize = threads.into_iter().map(|t| t.join().unwrap()).sum();
        assert!(reads > 0);
    });
    Ok(())
}

#[cfg(feature = "scalable-fault-injection")]
#[test]
fn slots_crash_child() {
    let Some(path) = std::env::var_os("VARVE_SLOT_CHILD") else {
        return;
    };
    let mut writer = Direct::open_indexed_writer(path, options()).unwrap();
    let _armed = varve::scalable_fault::arm_from_env().unwrap();
    writer
        .push_slot(&Slot {
            key: key(0),
            value: 2,
        })
        .unwrap();
    writer
        .push_slot(&Slot {
            key: key(4095),
            value: 2,
        })
        .unwrap();
    writer.flush().unwrap();
    writer.sync().unwrap();
}
#[cfg(feature = "scalable-fault-injection")]
#[test]
fn interrupted_slot_and_directory_writes_preserve_confirmed_generation() -> varve::Result<()> {
    use std::process::{Command, Stdio};
    fn prepare(root: &Path) -> varve::Result<std::path::PathBuf> {
        fs::create_dir(root)?;
        let path = root.join("data.varve");
        let mut writer = Direct::create_indexed_writer(&path, options())?;
        for code in [0, 4095] {
            writer.push_slot(&Slot {
                key: key(code),
                value: 1,
            })?;
        }
        writer.sync()?;
        Ok(path)
    }
    let dir = tempfile::tempdir()?;
    let trace = dir.path().join("trace.txt");
    let path = prepare(&dir.path().join("trace"))?;
    let run = |path: &Path, trace: &Path, mode: &str| {
        Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "slots_crash_child", "--nocapture"])
            .env("VARVE_SLOT_CHILD", path)
            .env(varve::scalable_fault::TRACE_ENV, trace)
            .env(varve::scalable_fault::FAULT_ENV, mode)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap()
    };
    assert!(run(&path, &trace, "trace").success());
    let events = fs::read_to_string(&trace)?;
    let mut aborts = 0;
    for (i, event) in events.lines().enumerate() {
        let parts: Vec<_> = event.split('\t').collect();
        assert_eq!(parts.len(), 3);
        let path = prepare(&dir.path().join(format!("case-{i}")))?;
        let trace = path.with_extension("trace");
        assert!(!run(&path, &trace, &format!("abort:{}:{}", parts[1], parts[2])).success());
        varve::clear_stale_writer_lock(&path, varve::WriterLockBreakPolicy::BreakIfProcessAbsent)?;
        let reader = Direct::open_indexed_reader(&path, options())?;
        let value = reader.get_slot(&key(0))?.unwrap().value;
        assert!(value == 1 || value == 2);
        assert_eq!(reader.get_slot(&key(4095))?.unwrap().value, value);
        let mut writer = Direct::open_indexed_writer(&path, options())
            .or_else(|_| Direct::restore_indexed_writer(&path, options()))?;
        writer.push_slot(&Slot {
            key: key(0),
            value: 3,
        })?;
        writer.sync()?;
        let reader = Direct::open_indexed_reader(&path, options())?;
        assert_eq!(reader.get_slot(&key(0))?.unwrap().value, 3);
        aborts += 1;
    }
    assert!(aborts > 30);
    eprintln!("FINITE_SLOT_CRASH aborts={aborts}");
    Ok(())
}
