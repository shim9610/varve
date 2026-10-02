//! Index-only comparison: identical keys, 52-byte descriptors and sync cadence.
//! Unlike the end-to-end oracle, neither side writes a native payload log here.
use super::{LATEST_LEN, tree};
use redb::{Database, Durability, ReadableDatabase, TableDefinition};
use std::{
    collections::BTreeMap,
    fs::OpenOptions,
    sync::Arc,
    time::{Duration, Instant},
};
const TABLE: TableDefinition<&[u8], &[u8]> = TableDefinition::new("offsets");

#[test]
fn randomized_pages_match_redb_across_splits_and_old_roots() {
    for seed in [0u64, 1, 42, u64::MAX] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("native-index");
        let file = Arc::new(
            OpenOptions::new()
                .create_new(true)
                .read(true)
                .write(true)
                .open(&path)
                .unwrap(),
        );
        file.set_len(tree::PAGE as u64).unwrap();
        let db = Database::builder()
            .set_cache_size(1024 * 1024)
            .create(dir.path().join("redb-index"))
            .unwrap();
        let transaction = db.begin_write().unwrap();
        transaction.open_table(TABLE).unwrap();
        transaction.commit().unwrap();
        let mut root = tree::Root::default();
        let mut model = BTreeMap::new();
        let mut random = seed ^ 0x9E3779B97F4A7C15;
        let mut native_write = Duration::ZERO;
        let mut redb_write = Duration::ZERO;
        let mut native_read = Duration::ZERO;
        let mut redb_read = Duration::ZERO;
        let mut checks = 0u64;
        for epoch in 0..48u64 {
            let old_root = root;
            let old_reader = tree::Reader::new(
                file.clone(),
                file.metadata().unwrap().len(),
                4096,
                1024 * 1024,
            );
            let old_redb = db.begin_read().unwrap();
            let old_model = model.clone();
            let mut updates = BTreeMap::new();
            for record in 0..256u64 {
                random ^= random << 13;
                random ^= random >> 7;
                random ^= random << 17;
                let id = if epoch < 24 {
                    epoch * 256 + record
                } else {
                    random % 7000
                };
                let mut key = vec![(id % 4) as u8; if id % 3 == 0 { 128 } else { 4 }];
                key.extend_from_slice(&id.to_be_bytes());
                let mut value = [0u8; LATEST_LEN];
                value[..8].copy_from_slice(&random.to_le_bytes());
                value[8] = u8::from(random % 7 == 0); // retained tombstone descriptor
                updates.insert(key, value);
            }
            let start = Instant::now();
            root = tree::apply(&file, &old_reader, root, &updates).unwrap();
            // Raw native page sync excludes checkpoint publication; redb commits include it.
            file.sync_data().unwrap();
            native_write += start.elapsed();
            let start = Instant::now();
            let mut tx = db.begin_write().unwrap();
            tx.set_durability(Durability::Immediate).unwrap();
            {
                let mut table = tx.open_table(TABLE).unwrap();
                for (key, value) in &updates {
                    table.insert(key.as_slice(), value.as_slice()).unwrap();
                }
            }
            tx.commit().unwrap();
            redb_write += start.elapsed();
            model.extend(updates);
            assert_eq!(root.count, model.len() as u64);
            // Exact old roots remain valid after every split and overwrite.
            let old_table = old_redb.open_table(TABLE).unwrap();
            for (key, value) in old_model.iter().step_by(71) {
                assert_eq!(old_reader.get(old_root, key).unwrap(), Some(*value));
                assert_eq!(
                    old_table.get(key.as_slice()).unwrap().unwrap().value(),
                    value
                );
                checks += 1;
            }
            let reader = tree::Reader::new(
                file.clone(),
                file.metadata().unwrap().len(),
                4096,
                1024 * 1024,
            );
            let tx = db.begin_read().unwrap();
            let table = tx.open_table(TABLE).unwrap();
            for (key, value) in model.iter().step_by(if epoch == 47 { 1 } else { 13 }) {
                let start = Instant::now();
                let actual = reader.get(root, key).unwrap();
                native_read += start.elapsed();
                let start = Instant::now();
                let reference = table.get(key.as_slice()).unwrap();
                redb_read += start.elapsed();
                assert_eq!(actual, Some(*value));
                assert_eq!(reference.unwrap().value(), value);
                checks += 1;
            }
            assert_eq!(reader.get(root, b"absent").unwrap(), None);
        }
        eprintln!(
            "INDEX_ORACLE seed={seed} keys={} checks={checks} native_write_us={} redb_write_us={} native_read_us={} redb_read_us={} native_bytes={} redb_bytes={}",
            model.len(),
            native_write.as_micros(),
            redb_write.as_micros(),
            native_read.as_micros(),
            redb_read.as_micros(),
            file.metadata().unwrap().len(),
            std::fs::metadata(dir.path().join("redb-index"))
                .unwrap()
                .len()
        );
    }
}

/// Separate first-pass and warmed reads; run explicitly for measurements.
#[test]
#[ignore = "release timing profile"]
fn lookup_profiles() {
    for kind in ["short", "long_distinct", "long_common"] {
        let dir = tempfile::tempdir().unwrap();
        let file = Arc::new(
            OpenOptions::new()
                .create_new(true)
                .read(true)
                .write(true)
                .open(dir.path().join("native"))
                .unwrap(),
        );
        file.set_len(tree::PAGE as u64).unwrap();
        let db = Database::builder()
            .set_cache_size(8 * 1024 * 1024)
            .create(dir.path().join("redb"))
            .unwrap();
        let make_key = |id: u64| {
            let mut key = match kind {
                "short" => Vec::new(),
                "long_distinct" => id.to_be_bytes().repeat(16),
                _ => vec![b'x'; 128],
            };
            key.extend_from_slice(&id.to_be_bytes());
            key
        };
        let mut root = tree::Root::default();
        let mut model = BTreeMap::new();
        for batch in 0..48u64 {
            let mut updates = BTreeMap::new();
            for i in 0..256u64 {
                let id = (batch * 256 + i) * 2;
                let mut value = [0; LATEST_LEN];
                value[..8].copy_from_slice(&id.to_le_bytes());
                updates.insert(make_key(id), value);
            }
            let reader = tree::Reader::new(
                file.clone(),
                file.metadata().unwrap().len(),
                4096,
                8 * 1024 * 1024,
            );
            root = tree::apply(&file, &reader, root, &updates).unwrap();
            file.sync_data().unwrap();
            let tx = db.begin_write().unwrap();
            {
                let mut table = tx.open_table(TABLE).unwrap();
                for (key, value) in &updates {
                    table.insert(key.as_slice(), value.as_slice()).unwrap();
                }
            }
            tx.commit().unwrap();
            model.extend(updates);
        }
        let mut queries: Vec<_> = (0..48 * 256 * 2u64).map(make_key).collect();
        let mut random = 42u64;
        for i in (1..queries.len()).rev() {
            random ^= random << 13;
            random ^= random >> 7;
            random ^= random << 17;
            queries.swap(i, random as usize % (i + 1));
        }
        let expected: Vec<_> = queries.iter().map(|key| model.get(key).copied()).collect();
        let reader = tree::Reader::new(
            file.clone(),
            file.metadata().unwrap().len(),
            4096,
            8 * 1024 * 1024,
        );
        let tx = db.begin_read().unwrap();
        let table = tx.open_table(TABLE).unwrap();
        let end = file.metadata().unwrap().len();
        let start = Instant::now();
        for (key, expected) in queries.iter().zip(&expected).step_by(191).take(128) {
            let fresh = tree::Reader::new(file.clone(), end, 4096, 8 * 1024 * 1024);
            assert_eq!(fresh.get(root, key).unwrap(), *expected);
        }
        eprintln!(
            "SELECTIVE_PROFILE kind={kind} readers=128 native_us={}",
            start.elapsed().as_micros()
        );
        for pass in 0..4 {
            let start = Instant::now();
            for (key, expected) in queries.iter().zip(&expected) {
                assert_eq!(reader.get(root, key).unwrap(), *expected);
            }
            let native = start.elapsed();
            let start = Instant::now();
            for (key, expected) in queries.iter().zip(&expected) {
                let actual = table.get(key.as_slice()).unwrap();
                assert_eq!(
                    actual.as_ref().map(|v| v.value()),
                    expected.as_ref().map(|v| v.as_slice())
                );
            }
            let redb = start.elapsed();
            eprintln!(
                "LOOKUP_PROFILE kind={kind} pass={pass} queries={} native_us={} redb_us={} native_bytes={}",
                queries.len(),
                native.as_micros(),
                redb.as_micros(),
                file.metadata().unwrap().len()
            );
        }
    }
}
