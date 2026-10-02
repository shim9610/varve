//! Shared by deterministic regression tests and libFuzzer. redb is an oracle
//! for the required logical contract; file layouts and general DB APIs differ.
use redb::{Database, ReadableDatabase, TableDefinition};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::time::{Duration, Instant};
use varve::{DiskIndexBatchOptions, DiskIndexOptions, varve_format};

varve_format! {
    pub format Differential {
        magic: b"DIFF";
        version: 1;
        schema_hash: computed;
        index: keyed_offset_chain;
        integrity: crc32;
        blocks {
            variable Item(id = 1, key = [key], key_index = disk) { key: String, value: u64, payload: Vec<u8> }
            variable Other(id = 2, key = [key], key_index = disk) { key: String, value: u64, payload: Vec<u8> }
        }
    }
}
const TABLE: TableDefinition<&[u8], &[u8]> = TableDefinition::new("latest");
type Model = BTreeMap<Vec<u8>, Vec<u8>>;

#[derive(Default, Debug)]
pub struct Timings {
    pub operations: usize,
    pub comparisons: usize,
    pub native_write: Duration,
    pub redb_write: Duration,
    pub native_sync: Duration,
    pub redb_sync: Duration,
    pub native_read: Duration,
    pub redb_read: Duration,
    pub native_restore: Duration,
    pub redb_abort: Duration,
    pub compactions: usize,
}
fn timed<T>(total: &mut Duration, action: impl FnOnce() -> T) -> T {
    let start = Instant::now();
    let result = action();
    *total += start.elapsed();
    result
}
fn key(id: u16) -> String {
    // Both inline and external keys, including long identical prefixes.
    match id % 4 {
        0 => format!("{id:04x}"),
        1 => format!("identical-prefix-{}-{id:04x}", "x".repeat(128)),
        2 => format!("한글-key-{id:04x}"),
        _ => format!("\0{}-{id:04x}", "y".repeat(25)),
    }
}
fn composite(block: u8, key: &str) -> Vec<u8> {
    let mut bytes = vec![block];
    bytes.extend_from_slice(key.as_bytes());
    bytes
}
fn encoded(value: u64, payload: &[u8]) -> Vec<u8> {
    let mut bytes = value.to_le_bytes().to_vec();
    bytes.extend_from_slice(payload);
    bytes
}
fn verify(
    reader: &DifferentialIndexedReader,
    transaction: &redb::ReadTransaction,
    model: &Model,
    keys: &BTreeSet<Vec<u8>>,
    times: &mut Timings,
) {
    let table = transaction.open_table(TABLE).unwrap();
    for key in keys {
        let name = String::from_utf8(key[1..].to_vec()).unwrap();
        let actual = timed(&mut times.native_read, || {
            if key[0] == 1 {
                reader
                    .get_item(&name)
                    .unwrap()
                    .map(|v| encoded(v.value, &v.payload))
            } else {
                reader
                    .get_other(&name)
                    .unwrap()
                    .map(|v| encoded(v.value, &v.payload))
            }
        });
        let reference = timed(&mut times.redb_read, || {
            table
                .get(key.as_slice())
                .unwrap()
                .map(|v| v.value().to_vec())
        });
        assert_eq!(actual.as_ref(), model.get(key), "native key {key:?}");
        assert_eq!(actual, reference, "redb key {key:?}");
        times.comparisons += 1;
    }
}

/// A bounded, deterministic operation stream. Every error on valid input is a
/// test failure. libFuzzer stores the exact bytes; seeded tests save them on panic.
pub fn run(data: &[u8]) -> Timings {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("native.varve");
    let options = DiskIndexOptions {
        cache_bytes: 1024 * 1024,
        max_key_bytes: 1024,
        batch: DiskIndexBatchOptions {
            max_records: 17,
            max_bytes: 32768,
        },
        ..DiskIndexOptions::default()
    };
    let mut writer = Some(Differential::create_indexed_writer(&path, options).unwrap());
    let db_path = dir.path().join("oracle.redb");
    let mut db = Database::builder()
        .set_cache_size(options.cache_bytes)
        .create(&db_path)
        .unwrap();
    let tx = db.begin_write().unwrap();
    tx.open_table(TABLE).unwrap();
    tx.commit().unwrap();
    let mut tx = Some(db.begin_write().unwrap());
    let mut working = Model::new();
    let mut committed = Model::new();
    let mut keys = BTreeSet::new();
    // Missing lookups are verified even before any insert.
    keys.insert(composite(1, &key(u16::MAX)));
    keys.insert(composite(2, &key(u16::MAX)));
    let mut reader = Differential::open_indexed_reader(&path, options).unwrap();
    let mut pins = VecDeque::new();
    let mut times = Timings::default();
    let mut dirty = false;
    for (step, op) in data.chunks(8).take(4096).enumerate() {
        let mut bytes = [0; 8];
        bytes[..op.len()].copy_from_slice(op);
        let id = u16::from_le_bytes([bytes[1], bytes[2]]);
        let name = key(id);
        let block = 1 + (bytes[3] & 1);
        let lookup_key = composite(block, &name);
        keys.insert(lookup_key.clone());
        match bytes[0] % 12 {
            0..=4 => {
                let value = u64::from_le_bytes(bytes).wrapping_add(step as u64);
                let payload = vec![bytes[4]; usize::from(bytes[5])];
                timed(&mut times.native_write, || {
                    if block == 1 {
                        writer
                            .as_mut()
                            .unwrap()
                            .push_item(&Item {
                                key: name,
                                value,
                                payload: payload.clone(),
                            })
                            .unwrap();
                    } else {
                        writer
                            .as_mut()
                            .unwrap()
                            .push_other(&Other {
                                key: name,
                                value,
                                payload: payload.clone(),
                            })
                            .unwrap();
                    }
                });
                let encoded = encoded(value, &payload);
                timed(&mut times.redb_write, || {
                    tx.as_ref()
                        .unwrap()
                        .open_table(TABLE)
                        .unwrap()
                        .insert(lookup_key.as_slice(), encoded.as_slice())
                        .unwrap();
                });
                working.insert(lookup_key, encoded);
                dirty = true;
            }
            5 => {
                timed(&mut times.native_write, || {
                    if block == 1 {
                        writer.as_mut().unwrap().delete_item(&name).unwrap();
                    } else {
                        writer.as_mut().unwrap().delete_other(&name).unwrap();
                    }
                });
                timed(&mut times.redb_write, || {
                    tx.as_ref()
                        .unwrap()
                        .open_table(TABLE)
                        .unwrap()
                        .remove(lookup_key.as_slice())
                        .unwrap();
                });
                working.remove(&lookup_key);
                dirty = true;
            }
            6 | 8 => {
                pins.push_back((
                    Differential::open_indexed_reader(&path, options).unwrap(),
                    db.begin_read().unwrap(),
                    committed.clone(),
                ));
                if pins.len() > 4 {
                    pins.pop_front();
                }
                timed(&mut times.native_sync, || {
                    writer.as_mut().unwrap().immediate().unwrap()
                });
                timed(&mut times.redb_sync, || {
                    tx.take().unwrap().commit().unwrap()
                });
                committed = working.clone();
                dirty = false;
                if bytes[0] % 12 == 8 {
                    drop(writer.take());
                    writer = Some(Differential::open_indexed_writer(&path, options).unwrap());
                    // Outstanding redb transactions own its shared state; release them before reopen.
                    pins.clear();
                    drop(db);
                    db = Database::builder()
                        .set_cache_size(options.cache_bytes)
                        .open(&db_path)
                        .unwrap();
                }
                tx = Some(db.begin_write().unwrap());
            }
            7 => {
                // Native rollback is recovery to the confirmed checkpoint;
                // redb rolls back the corresponding uncommitted transaction.
                timed(&mut times.native_restore, || {
                    drop(writer.take());
                    writer = Some(if dirty {
                        Differential::restore_indexed_writer(&path, options).unwrap()
                    } else {
                        Differential::open_indexed_writer(&path, options).unwrap()
                    });
                });
                timed(&mut times.redb_abort, || {
                    tx.take().unwrap().abort().unwrap()
                });
                working = committed.clone();
                dirty = false;
                tx = Some(db.begin_write().unwrap());
            }
            9 => {
                reader.follow().unwrap();
            }
            10 => {
                if dirty {
                    assert!(writer.as_mut().unwrap().compact_index().is_err());
                } else {
                    writer.as_mut().unwrap().compact_index().unwrap();
                    times.compactions += 1;
                }
            }
            _ => {
                writer.as_mut().unwrap().flush().unwrap();
            }
        }
        // Opening while Dirty must see the confirmed root and must not take a writer lock.
        let fresh = Differential::open_indexed_reader(&path, options).unwrap();
        // Check all keys periodically and current/missing keys on every operation.
        let current_keys = if step % 31 == 0 {
            keys.clone()
        } else {
            [composite(block, &key(id)), composite(1, &key(u16::MAX))]
                .into_iter()
                .collect()
        };
        verify(
            &fresh,
            &db.begin_read().unwrap(),
            &committed,
            &current_keys,
            &mut times,
        );
        for (pin, redb_pin, model) in &pins {
            verify(pin, redb_pin, model, &current_keys, &mut times);
        }
        times.operations += 1;
    }
    writer.as_mut().unwrap().immediate().unwrap();
    tx.take().unwrap().commit().unwrap();
    reader.follow().unwrap();
    verify(
        &reader,
        &db.begin_read().unwrap(),
        &working,
        &keys,
        &mut times,
    );
    reader.verify_all().unwrap();
    for (pin, redb_pin, model) in &pins {
        verify(pin, redb_pin, model, &keys, &mut times);
    }
    times
}

#[allow(dead_code)] // Shared fuzz interpreter does not generate its own inputs.
pub fn seeded(seed: u64, operations: usize) -> Vec<u8> {
    let mut state = seed ^ 0x9E3779B97F4A7C15;
    let mut data = Vec::with_capacity(operations * 8);
    for i in 0..operations {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        let mut bytes = state.to_le_bytes();
        // Keep overwrite/delete pressure while retaining enough keys to split pages.
        bytes[2] %= 4;
        if i < operations / 3 {
            bytes[0] %= 5;
        }
        data.extend_from_slice(&bytes);
    }
    data
}
