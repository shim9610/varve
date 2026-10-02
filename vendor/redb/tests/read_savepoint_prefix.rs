use redb::{Database, Durability, ReadOnlyDatabase, ReadableDatabase, TableDefinition};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

#[cfg(feature = "experimental-api-5")]
use redb::ReadableTable;

const STATE: TableDefinition<u64, u64> = TableDefinition::new("savepoint-state");

#[test]
fn prefix_is_pinned_and_staged_changes_are_invisible() {
    let file = tempfile::NamedTempFile::new().unwrap();
    let db = Database::create(file.path()).unwrap();
    let empty = db.begin_read().unwrap();
    let write = db.begin_write().unwrap();
    let id = write.persistent_savepoint().unwrap();
    assert_eq!(
        write.persistent_savepoint_prefix().unwrap(),
        [Some(id), None]
    );
    assert_eq!(
        db.begin_read()
            .unwrap()
            .persistent_savepoint_prefix()
            .unwrap(),
        [None; 2]
    );
    write.commit().unwrap();
    let saved = db.begin_read().unwrap();
    assert_eq!(
        saved.persistent_savepoint_prefix().unwrap(),
        [Some(id), None]
    );
    assert_eq!(empty.persistent_savepoint_prefix().unwrap(), [None; 2]);

    let write = db.begin_write().unwrap();
    assert!(write.delete_persistent_savepoint(id).unwrap());
    assert_eq!(
        db.begin_read()
            .unwrap()
            .persistent_savepoint_prefix()
            .unwrap(),
        [Some(id), None]
    );
    write.abort().unwrap();
    let write = db.begin_write().unwrap();
    assert!(write.delete_persistent_savepoint(id).unwrap());
    write.commit().unwrap();
    assert_eq!(
        saved.persistent_savepoint_prefix().unwrap(),
        [Some(id), None]
    );
    assert_eq!(
        db.begin_read()
            .unwrap()
            .persistent_savepoint_prefix()
            .unwrap(),
        [None; 2]
    );
}

#[test]
fn bounded_prefix_survives_read_only_reopen_and_integrity_check() {
    let file = tempfile::NamedTempFile::new().unwrap();
    let mut db = Database::create(file.path()).unwrap();
    let mut ids = Vec::new();
    for _ in 0..4 {
        let write = db.begin_write().unwrap();
        ids.push(write.persistent_savepoint().unwrap());
        write.commit().unwrap();
    }
    let prefix = [Some(ids[0]), Some(ids[1])];
    assert_eq!(
        db.begin_read()
            .unwrap()
            .persistent_savepoint_prefix()
            .unwrap(),
        prefix
    );
    assert!(db.check_integrity().unwrap());
    assert_eq!(
        db.begin_read()
            .unwrap()
            .persistent_savepoint_prefix()
            .unwrap(),
        prefix
    );
    drop(db);
    let db = ReadOnlyDatabase::open(file.path()).unwrap();
    assert_eq!(
        db.begin_read()
            .unwrap()
            .persistent_savepoint_prefix()
            .unwrap(),
        prefix
    );
    drop(db);
    let db = Database::open(file.path()).unwrap();
    assert_eq!(
        db.begin_read()
            .unwrap()
            .persistent_savepoint_prefix()
            .unwrap(),
        prefix
    );
}

#[test]
fn non_durable_data_commits_keep_the_committed_savepoint_prefix() {
    let file = tempfile::NamedTempFile::new().unwrap();
    let mut db = Database::create(file.path()).unwrap();
    let write = db.begin_write().unwrap();
    let id = write.persistent_savepoint().unwrap();
    write.commit().unwrap();
    for value in 0..32 {
        let mut write = db.begin_write().unwrap();
        write.set_durability(Durability::None).unwrap();
        write.open_table(STATE).unwrap().insert(0, value).unwrap();
        write.commit().unwrap();
        let read = db.begin_read().unwrap();
        assert_eq!(
            read.persistent_savepoint_prefix().unwrap(),
            [Some(id), None]
        );
        assert_eq!(
            read.open_table(STATE)
                .unwrap()
                .get(0)
                .unwrap()
                .unwrap()
                .value(),
            value
        );
    }
    assert!(db.check_integrity().unwrap());
    assert_eq!(
        db.begin_read()
            .unwrap()
            .persistent_savepoint_prefix()
            .unwrap(),
        [Some(id), None]
    );
}

#[test]
fn data_root_and_savepoint_prefix_are_published_together() {
    let file = tempfile::NamedTempFile::new().unwrap();
    let db = Database::create(file.path()).unwrap();
    let write = db.begin_write().unwrap();
    write.open_table(STATE).unwrap().insert(0, 0).unwrap();
    write.commit().unwrap();
    let stop = AtomicBool::new(false);
    let reads = AtomicUsize::new(0);
    let barrier = std::sync::Barrier::new(5);
    std::thread::scope(|scope| {
        for _ in 0..4 {
            scope.spawn(|| {
                barrier.wait();
                while !stop.load(Ordering::Acquire) {
                    let pair = db.begin_read_with_savepoint().unwrap();
                    let read = pair.current;
                    let id = read
                        .open_table(STATE)
                        .unwrap()
                        .get(0)
                        .unwrap()
                        .unwrap()
                        .value();
                    let expected = if id == 0 { [None; 2] } else { [Some(id), None] };
                    assert_eq!(read.persistent_savepoint_prefix().unwrap(), expected);
                    if let Some((saved_id, saved)) = pair.oldest_savepoint {
                        assert_eq!(saved_id, id);
                        assert_eq!(
                            saved
                                .open_table(STATE)
                                .unwrap()
                                .get(0)
                                .unwrap()
                                .unwrap()
                                .value(),
                            0
                        );
                    } else {
                        assert_eq!(id, 0);
                    }
                    reads.fetch_add(1, Ordering::Relaxed);
                }
            });
        }
        barrier.wait();
        for _ in 0..100 {
            let write = db.begin_write().unwrap();
            let id = write.persistent_savepoint().unwrap();
            write.open_table(STATE).unwrap().insert(0, id).unwrap();
            write.commit().unwrap();
            let write = db.begin_write().unwrap();
            write.delete_persistent_savepoint(id).unwrap();
            write.open_table(STATE).unwrap().insert(0, 0).unwrap();
            write.commit().unwrap();
        }
        stop.store(true, Ordering::Release);
    });
    assert!(reads.load(Ordering::Relaxed) > 0);
}

#[test]
fn saved_user_root_remains_readable_after_deletion_and_page_reuse() {
    const DATA: TableDefinition<u64, &[u8]> = TableDefinition::new("saved-data");
    let file = tempfile::NamedTempFile::new().unwrap();
    let db = Database::builder()
        .set_cache_size(128 * 1024)
        .create(file.path())
        .unwrap();
    let write = db.begin_write().unwrap();
    for key in 0..128 {
        write
            .open_table(DATA)
            .unwrap()
            .insert(key, vec![1; 4096].as_slice())
            .unwrap();
    }
    write.commit().unwrap();
    let write = db.begin_write().unwrap();
    let id = write.persistent_savepoint().unwrap();
    write.commit().unwrap();
    let mut write = db.begin_write().unwrap();
    write.set_durability(Durability::None).unwrap();
    write
        .open_table(DATA)
        .unwrap()
        .insert(0, vec![2; 4096].as_slice())
        .unwrap();
    write.commit().unwrap();
    let pair = db.begin_read_with_savepoint().unwrap();
    let (saved_id, saved) = pair.oldest_savepoint.unwrap();
    assert_eq!(id, saved_id);
    let old_table = saved.open_table(DATA).unwrap();
    let current_table = pair.current.open_table(DATA).unwrap();
    drop(saved);
    drop(pair.current);
    let write = db.begin_write().unwrap();
    write.delete_persistent_savepoint(id).unwrap();
    write.commit().unwrap();
    for value in 3..24 {
        let mut write = db.begin_write().unwrap();
        write.set_durability(Durability::None).unwrap();
        for key in 0..128 {
            write
                .open_table(DATA)
                .unwrap()
                .insert(key, vec![value; 4096].as_slice())
                .unwrap();
        }
        write.commit().unwrap();
    }
    for key in 0..128 {
        assert_eq!(old_table.get(key).unwrap().unwrap().value(), &[1; 4096]);
    }
    assert_eq!(current_table.get(0).unwrap().unwrap().value(), &[2; 4096]);
}

#[test]
fn saved_snapshot_reopens_read_only_without_restoring_working_data() {
    let file = tempfile::NamedTempFile::new().unwrap();
    let db = Database::create(file.path()).unwrap();
    let write = db.begin_write().unwrap();
    write.open_table(STATE).unwrap().insert(0, 1).unwrap();
    write.commit().unwrap();
    let write = db.begin_write().unwrap();
    let id = write.persistent_savepoint().unwrap();
    write.commit().unwrap();
    let write = db.begin_write().unwrap();
    write.open_table(STATE).unwrap().insert(0, 2).unwrap();
    write.commit().unwrap();
    drop(db);
    let db = ReadOnlyDatabase::open(file.path()).unwrap();
    let pair = db.begin_read_with_savepoint().unwrap();
    assert_eq!(
        pair.current
            .open_table(STATE)
            .unwrap()
            .get(0)
            .unwrap()
            .unwrap()
            .value(),
        2
    );
    let (saved_id, saved) = pair.oldest_savepoint.unwrap();
    assert_eq!(saved_id, id);
    assert_eq!(
        saved
            .open_table(STATE)
            .unwrap()
            .get(0)
            .unwrap()
            .unwrap()
            .value(),
        1
    );
    assert!(saved.persistent_savepoint_prefix().is_err());
}
