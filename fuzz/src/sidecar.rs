use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};

use varve::{BatchOptions, DiskIndexBatchOptions, DiskIndexOptions, StreamOptions};

use crate::{DiskItem, FuzzSidecarFormat, SidecarEvent, StreamItem};

const MAX_BYTES: usize = 1_048_576;
const MAX_OPERATIONS: usize = 64;
const MAX_RECORDS: usize = 256;

#[derive(Clone, Default)]
struct Model {
    records: u64,
    disk: BTreeMap<u64, DiskItem>,
}

pub fn run_arbitrary(data: &[u8]) {
    let Ok(root) = tempfile::tempdir() else {
        return;
    };
    let raw = &data[..data.len().min(MAX_BYTES)];

    let stream_path = root.path().join("arbitrary-stream.varve");
    let stream_model = create_stream_fixture(&stream_path, 0, true);
    if fs::write(companion_path(&stream_path, ".vks"), raw).is_ok() {
        exercise_stream(&stream_path, &stream_model, true);
    }

    let indexed_path = root.path().join("arbitrary-indexed.varve");
    let indexed_model = create_indexed_fixture(&indexed_path, 0, true);
    if fs::write(companion_path(&indexed_path, ".vki"), raw).is_ok() {
        exercise_indexed(&indexed_path, &indexed_model, true);
    }
}

pub fn run_mutation(data: &[u8]) {
    let Ok(root) = tempfile::tempdir() else {
        return;
    };
    let control = data.first().copied().unwrap_or(0);
    let seed = data.get(1).copied().unwrap_or(0);
    let synced = control & 1 == 0;
    let mutations = data.get(2..).unwrap_or_default();

    let stream_path = root.path().join("mutation-stream.varve");
    let stream_model = create_stream_fixture(&stream_path, seed, synced);
    if mutate_companion(&companion_path(&stream_path, ".vks"), control, mutations) {
        exercise_stream(&stream_path, &stream_model, synced);
    }

    let indexed_path = root.path().join("mutation-indexed.varve");
    let indexed_model = create_indexed_fixture(&indexed_path, seed, synced);
    if mutate_companion(&companion_path(&indexed_path, ".vki"), control, mutations) {
        exercise_indexed(&indexed_path, &indexed_model, synced);
    }
}

pub fn run_state_machine(data: &[u8]) {
    let Ok(root) = tempfile::tempdir() else {
        return;
    };
    let operations: Vec<[u8; 4]> = data
        .chunks(4)
        .take(MAX_OPERATIONS)
        .map(|chunk| {
            let mut operation = [0; 4];
            operation[..chunk.len()].copy_from_slice(chunk);
            operation
        })
        .collect();

    run_stream_machine(&root.path().join("machine-stream.varve"), &operations);
    run_indexed_machine(&root.path().join("machine-indexed.varve"), &operations);
}

fn create_stream_fixture(path: &Path, seed: u8, synced: bool) -> Model {
    let mut committed = Model::default();
    let Ok(mut writer) = FuzzSidecarFormat::create_stream_writer(path, stream_options()) else {
        return committed;
    };

    let populated = seed & 1 != 0;
    if populated {
        let count = usize::from(seed % 8) + 2;
        for index in 0..count {
            if writer
                .push_sidecar_event(&SidecarEvent {
                    value: index as u64,
                })
                .is_ok()
            {
                committed.records += 1;
            }
            let key = if seed & 2 == 0 {
                index as u64
            } else {
                (index % 2) as u64
            };
            if writer
                .push_stream_item(&StreamItem {
                    key,
                    value: (index as u64) ^ u64::from(seed),
                    payload: vec![seed; index % 17],
                })
                .is_ok()
            {
                committed.records += 1;
            }
        }
        if seed & 4 != 0 && writer.delete_stream_item(&0).is_ok() {
            committed.records += 1;
        }
    }
    if writer.sync().is_err() {
        return Model::default();
    }

    if !synced {
        let _ = writer.push_sidecar_event(&SidecarEvent { value: u64::MAX });
        let _ = writer.push_stream_item(&StreamItem {
            key: 0,
            value: u64::MAX,
            payload: vec![0xDD; 8],
        });
        let _ = writer.flush();
    }
    committed
}

fn create_indexed_fixture(path: &Path, seed: u8, synced: bool) -> Model {
    let mut committed = Model::default();
    let Ok(mut writer) = FuzzSidecarFormat::create_indexed_writer(path, disk_options()) else {
        return committed;
    };

    if seed & 1 != 0 {
        let count = usize::from(seed % 12) + 3;
        for index in 0..count {
            if writer
                .push_sidecar_event(&SidecarEvent {
                    value: index as u64,
                })
                .is_ok()
            {
                committed.records += 1;
            }
            let key = if seed & 2 == 0 {
                index as u64
            } else {
                (index % 3) as u64
            };
            let item = DiskItem {
                key,
                value: (index as u64) ^ u64::from(seed),
                payload: vec![seed; index % 19],
            };
            if writer.push_disk_item(&item).is_ok() {
                committed.records += 1;
                committed.disk.insert(key, item);
            }
            if index % 3 == 0
                && writer
                    .push_stream_item(&StreamItem {
                        key: index as u64,
                        value: index as u64,
                        payload: Vec::new(),
                    })
                    .is_ok()
            {
                committed.records += 1;
            }
        }
        if seed & 4 != 0 && writer.delete_disk_item(&0).is_ok() {
            committed.records += 1;
            committed.disk.remove(&0);
        }
    }
    if writer.sync().is_err() {
        return Model::default();
    }

    if !synced {
        let pending = DiskItem {
            key: 0,
            value: u64::MAX,
            payload: vec![0xDD; 8],
        };
        let _ = writer.push_disk_item(&pending);
        let _ = writer.delete_disk_item(&1);
        let _ = writer.flush();
    }
    committed
}

fn mutate_companion(path: &Path, control: u8, mutations: &[u8]) -> bool {
    let Ok(mut bytes) = fs::read(path) else {
        return false;
    };
    bytes.truncate(MAX_BYTES);
    if control & 0x80 == 0 {
        if mutations.is_empty() {
            if !bytes.is_empty() {
                let index = usize::from(control) % bytes.len();
                bytes[index] ^= 1;
            }
        } else {
            for mutation in mutations.chunks(3).take(MAX_OPERATIONS) {
                if bytes.is_empty() {
                    break;
                }
                let high = usize::from(*mutation.first().unwrap_or(&0));
                let low = usize::from(*mutation.get(1).unwrap_or(&0));
                let index = ((high << 8) | low) % bytes.len();
                bytes[index] ^= mutation.get(2).copied().unwrap_or(1) | 1;
            }
        }
    }
    fs::write(path, &bytes).is_ok()
}

fn run_stream_machine(path: &Path, operations: &[[u8; 4]]) {
    let Ok(writer) = FuzzSidecarFormat::create_stream_writer(path, stream_options()) else {
        return;
    };
    let mut writer = Some(writer);
    let mut working = Model::default();
    let mut committed = Model::default();
    let mut records = 0usize;

    for [opcode, key, value, count] in operations.iter().copied() {
        if records >= MAX_RECORDS {
            break;
        }
        if writer.is_none() {
            writer = FuzzSidecarFormat::open_stream_writer(path, stream_options())
                .or_else(|_| FuzzSidecarFormat::restore_stream_writer(path, stream_options()))
                .ok();
            working = committed.clone();
        }
        let Some(current) = writer.as_mut() else {
            continue;
        };
        match opcode % 8 {
            0 => {
                if current
                    .push_sidecar_event(&SidecarEvent {
                        value: u64::from(value),
                    })
                    .is_ok()
                {
                    working.records += 1;
                    records += 1;
                }
            }
            1 => {
                if current
                    .push_stream_item(&StreamItem {
                        key: u64::from(key % 16),
                        value: u64::from(value),
                        payload: vec![value; usize::from(count % 32)],
                    })
                    .is_ok()
                {
                    working.records += 1;
                    records += 1;
                }
            }
            2 => {
                if current.delete_stream_item(&u64::from(key % 16)).is_ok() {
                    working.records += 1;
                    records += 1;
                }
            }
            3 => {
                let _ = current.flush();
            }
            4 => {
                if current.sync().is_ok() {
                    committed = working.clone();
                }
            }
            5 => {
                writer.take();
                probe_stream(
                    path,
                    &committed,
                    working.records == committed.records,
                    false,
                );
            }
            6 => {
                writer.take();
            }
            _ => {
                let amount = (usize::from(count % 8) + 1).min(MAX_RECORDS - records);
                let values = (0..amount).map(|offset| SidecarEvent {
                    value: u64::from(value) + offset as u64,
                });
                if let Ok(report) = current.push_sidecar_events(values, batch_options()) {
                    working.records += report.records;
                    records += report.records as usize;
                }
            }
        }
    }
    drop(writer);
    exercise_stream_strict(path, &committed, working.records == committed.records);
}

fn run_indexed_machine(path: &Path, operations: &[[u8; 4]]) {
    let Ok(writer) = FuzzSidecarFormat::create_indexed_writer(path, disk_options()) else {
        return;
    };
    let mut writer = Some(writer);
    let mut working = Model::default();
    let mut committed = Model::default();
    let mut records = 0usize;

    for [opcode, key, value, count] in operations.iter().copied() {
        if records >= MAX_RECORDS {
            break;
        }
        if writer.is_none() {
            writer = FuzzSidecarFormat::open_indexed_writer(path, disk_options())
                .or_else(|_| FuzzSidecarFormat::restore_indexed_writer(path, disk_options()))
                .ok();
            working = committed.clone();
        }
        let Some(current) = writer.as_mut() else {
            continue;
        };
        let key = u64::from(key % 16);
        match opcode % 9 {
            0 => {
                if current
                    .push_sidecar_event(&SidecarEvent {
                        value: u64::from(value),
                    })
                    .is_ok()
                {
                    working.records += 1;
                    records += 1;
                }
            }
            1 | 2 => {
                let item = DiskItem {
                    key,
                    value: u64::from(value),
                    payload: vec![value; usize::from(count % 32)],
                };
                if current.push_disk_item(&item).is_ok() {
                    working.records += 1;
                    working.disk.insert(key, item);
                    records += 1;
                }
            }
            3 => {
                if current.delete_disk_item(&key).is_ok() {
                    working.records += 1;
                    working.disk.remove(&key);
                    records += 1;
                }
            }
            4 => {
                let item = StreamItem {
                    key,
                    value: u64::from(value),
                    payload: Vec::new(),
                };
                if current.push_stream_item(&item).is_ok() {
                    working.records += 1;
                    records += 1;
                }
            }
            5 => {
                if current.sync().is_ok() {
                    committed = working.clone();
                }
            }
            6 => {
                let _ = current.flush();
                writer.take();
                probe_indexed(
                    path,
                    &committed,
                    working.records == committed.records,
                    false,
                );
            }
            7 => {
                writer.take();
            }
            _ => {
                let amount = (usize::from(count % 8) + 1).min(MAX_RECORDS - records);
                let items: Vec<_> = (0..amount)
                    .map(|offset| DiskItem {
                        key: (key + offset as u64) % 16,
                        value: u64::from(value) + offset as u64,
                        payload: vec![count; offset % 8],
                    })
                    .collect();
                if let Ok(report) = current.push_disk_items(&items, batch_options()) {
                    working.records += report.records;
                    records += report.records as usize;
                    for item in items.into_iter().take(report.records as usize) {
                        working.disk.insert(item.key, item);
                    }
                }
            }
        }
    }
    drop(writer);
    exercise_indexed_strict(path, &committed, working.records == committed.records);
}

fn exercise_stream(path: &Path, committed: &Model, should_be_clean: bool) {
    probe_stream(path, committed, should_be_clean, true);
    let restored = FuzzSidecarFormat::restore_stream_writer(path, stream_options());
    drop(restored);
    probe_stream(path, committed, true, true);
    let reopened = FuzzSidecarFormat::open_stream_writer(path, stream_options());
    drop(reopened);
}

fn exercise_indexed(path: &Path, committed: &Model, should_be_clean: bool) {
    probe_indexed(path, committed, should_be_clean, true);
    let restored = FuzzSidecarFormat::restore_indexed_writer(path, disk_options());
    drop(restored);
    probe_indexed(path, committed, true, true);
    let reopened = FuzzSidecarFormat::open_indexed_writer(path, disk_options());
    drop(reopened);
}

fn exercise_stream_strict(path: &Path, committed: &Model, should_be_clean: bool) {
    probe_stream(path, committed, should_be_clean, false);
    if !should_be_clean {
        let restored = FuzzSidecarFormat::restore_stream_writer(path, stream_options());
        drop(restored.expect("valid stream recovery"));
        probe_stream(path, committed, true, false);
    }
    drop(
        FuzzSidecarFormat::open_stream_writer(path, stream_options())
            .expect("valid stream writer reopen"),
    );
}

fn exercise_indexed_strict(path: &Path, committed: &Model, should_be_clean: bool) {
    probe_indexed(path, committed, should_be_clean, false);
    if !should_be_clean {
        let restored = FuzzSidecarFormat::restore_indexed_writer(path, disk_options());
        drop(restored.expect("valid disk index recovery"));
        probe_indexed(path, committed, true, false);
    }
    drop(
        FuzzSidecarFormat::open_indexed_writer(path, disk_options())
            .expect("valid indexed writer reopen"),
    );
}

fn probe_stream(path: &Path, committed: &Model, should_be_clean: bool, permit_rejection: bool) {
    match FuzzSidecarFormat::open_stream_reader(path, stream_options()) {
        Ok(reader) => {
            assert!(should_be_clean, "dirty stream companion opened as clean");
            assert_eq!(reader.verify_all().ok(), Some(committed.records));
        }
        Err(_typed_error) => assert!(
            permit_rejection || !should_be_clean,
            "valid clean stream companion was rejected"
        ),
    }
}

fn probe_indexed(path: &Path, committed: &Model, should_be_clean: bool, permit_rejection: bool) {
    match FuzzSidecarFormat::open_indexed_reader(path, disk_options()) {
        Ok(reader) => {
            assert!(should_be_clean, "dirty disk index opened as clean");
            assert_eq!(reader.verify_all().ok(), Some(committed.records));
            for key in 0..16 {
                let actual = reader.get_disk_item(&key).expect("accepted index lookup");
                match (actual, committed.disk.get(&key)) {
                    (None, None) => {}
                    (Some(actual), Some(expected)) => {
                        assert_eq!(actual.key, expected.key);
                        assert_eq!(actual.value, expected.value);
                        assert_eq!(actual.payload, expected.payload);
                    }
                    _ => panic!("accepted disk index disagrees with committed model"),
                }
            }
        }
        Err(_typed_error) => assert!(
            permit_rejection || !should_be_clean,
            "valid clean disk index was rejected"
        ),
    }
}

fn companion_path(path: &Path, suffix: &str) -> PathBuf {
    let mut value: OsString = path.as_os_str().to_owned();
    value.push(suffix);
    PathBuf::from(value)
}

fn disk_options() -> DiskIndexOptions {
    DiskIndexOptions {
        batch: DiskIndexBatchOptions {
            max_records: 2,
            max_bytes: 64 * 1024,
        },
        ..DiskIndexOptions::default()
    }
}

fn stream_options() -> StreamOptions {
    StreamOptions {
        state: disk_options(),
        ..StreamOptions::default()
    }
}

const fn batch_options() -> BatchOptions {
    BatchOptions {
        max_records: 2,
        max_bytes: 64 * 1024,
    }
}
