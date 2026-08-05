//! Every **record** read entry point takes `&self`, proved by the compiler.
//!
//! The standing policy is "all reads take `&self`; one handle serves concurrent
//! readers". `matrix_concurrent_reads.rs` already proves it for the five matrix
//! entry points across three handle types. Nothing proved it for the record and
//! block side — `blocks`, `keyed_blocks`, `scan`, `metadata`, `block_chain`,
//! `verify_all`, `index_entries` and the rest — where it held only by review.
//!
//! That gap became load-bearing when the record index stopped keeping its
//! entries. `ResidentIndex` now holds 16-byte slots and rebuilds each entry from
//! the record's own header and footer on demand, so **producing an index entry
//! now reads the file**. The two obvious ways to make that faster are a cache
//! and a cursor, and both want `&mut self` or a `RefCell` — which is exactly the
//! change that would silently make one handle stop serving two readers, with no
//! test failing.
//!
//! So this is a compile-time gate, in the same shape as the matrix one: two
//! shared borrows of one handle are alive at once, and every read entry point is
//! driven through both. A receiver that became `&mut self` fails to borrow. A
//! field that became `!Sync` fails `assert_sync`. Neither can be argued with.
//!
//! It is not a behavioural test. It asserts the values it reads only enough to
//! keep the calls from being optimised into nothing; what it is really asserting
//! is that the file below compiles.
//!
//! **Proved discriminating, 2026-08-05.** Changing `VarveReader::scan` from
//! `&self` to `&mut self` and changing nothing else fails *this test binary and
//! only this one*: `error[E0596]: cannot borrow *reader as mutable, as it is
//! behind a & reference`. (The same experiment on `index_entries` is not the
//! proof, because `varve-core` has internal `&self` callers of its own and stops
//! at the library — a gate has to fail where it is, not upstream of itself.)

use varve::{VarveBlock, varve_format};

varve_format! {
    pub format SharedBorrowFormat {
        magic: b"SHRBRW";
        version: 1;
        index: keyed_offset_chain;
        blocks {
            fixed Tick(id = 10) {
                at: u64,
            }
            variable Item(id = 11, key = [id]) {
                id: u64,
                value: u32,
            }
        }
    }
}

fn build(path: &std::path::Path) -> varve::Result<()> {
    let mut writer = SharedBorrowFormat::create(path)?;
    for at in 0..8u64 {
        writer.push(&Tick { at })?;
        writer.push_keyed(&Item {
            id: at,
            value: at as u32 * 3,
        })?;
    }
    writer.write_metadata("owner", b"varve")?;
    writer.flush()?;
    Ok(())
}

/// Every record read entry point on `VarveFile`, driven through a **shared**
/// borrow. A receiver that is `&mut self` on any one of them fails here.
fn read_everything(file: &varve::VarveFile) -> varve::Result<usize> {
    let mut touched = 0usize;

    touched += file.blocks::<Tick>()?.len();
    touched += file.keyed_blocks::<Item>()?.len();
    touched += file.scan().collect::<varve::Result<Vec<_>>>()?.len();
    touched += file.index_entries().len();

    let mut entries = Vec::new();
    file.index_entries_into(&mut entries)?;
    touched += entries.len();

    touched += usize::from(file.metadata("owner")?.is_some());
    touched += file.all_metadata()?.len();
    touched += usize::from(file.schema_manifest()?.is_some());
    touched += file.verify_all()?;
    touched += file
        .block_chain(Tick::ID)?
        .collect::<varve::Result<Vec<_>>>()?
        .len();
    touched += file.key_tail_offsets::<Item>()?.len();
    let _ = file.spec();
    let _ = file.path();

    Ok(touched)
}

/// The same for the read-only facade.
fn read_everything_through_reader(reader: &varve::VarveReader) -> varve::Result<usize> {
    let mut touched = 0usize;
    touched += reader.blocks::<Tick>()?.len();
    touched += reader.keyed_blocks::<Item>()?.len();
    touched += reader.scan().collect::<varve::Result<Vec<_>>>()?.len();
    touched += reader.index_entries().len();
    touched += usize::from(reader.metadata("owner")?.is_some());
    touched += reader.all_metadata()?.len();
    touched += reader.verify_all()?;
    let _ = reader.spec();
    let _ = reader.path();
    Ok(touched)
}

/// And for the writer.
///
/// Its record read surface is deliberately narrow — `blocks`, `keyed_blocks`,
/// `scan` and `metadata` are not on it, which the compiler confirmed when this
/// test was first written against them — but what it does expose reads the file
/// and must take `&self` like the rest. `index_entries` is the one that matters:
/// it is the only way to see the record directory through a writer, and it is
/// now the call that faults every entry off disk.
fn read_everything_through_writer(writer: &varve::VarveWriter) -> varve::Result<usize> {
    let mut touched = 0usize;
    touched += writer.index_entries().len();
    let mut entries = Vec::new();
    writer.index_entries_into(&mut entries)?;
    touched += entries.len();
    touched += writer.verify_all()?;
    touched += writer.key_tail_offsets::<Item>()?.len();
    let _ = writer.spec();
    let _ = writer.path();
    let _ = writer.mode();
    Ok(touched)
}

#[test]
fn every_record_read_entry_point_takes_a_shared_borrow() -> varve::Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("shared.shrbrw");
    build(&path)?;

    // Not `mut`. Two shared borrows are alive across both calls, which is the
    // whole assertion: a `&mut self` receiver anywhere below refuses to borrow.
    let file = SharedBorrowFormat::open_readonly(&path)?;
    let first = &file;
    let second = &file;
    let a = read_everything(first)?;
    let b = read_everything(second)?;
    assert_eq!(a, b, "two shared borrows must see the same file");
    assert!(a > 0, "the fixture must actually contain records");
    drop(file);

    // The generated facades wrap the core handles; reach the core type so the
    // signatures being pinned are varve-core's own and not the macro's.
    let reader = varve::VarveReader::open(SharedBorrowFormat::spec(), &path)?;
    let first = &reader;
    let second = &reader;
    assert_eq!(
        read_everything_through_reader(first)?,
        read_everything_through_reader(second)?
    );
    drop(reader);

    let writer = varve::VarveWriter::open(SharedBorrowFormat::spec(), &path)?;
    let first = &writer;
    let second = &writer;
    assert_eq!(
        read_everything_through_writer(first)?,
        read_everything_through_writer(second)?
    );
    Ok(())
}

/// A shared borrow is worthless if `&Handle` cannot cross a thread boundary, so
/// pin that too — and then actually cross one, because `Sync` is a claim about
/// the type and this is the claim about the code.
///
/// The threads read through **one** handle. Each faults its own index entries
/// out of the same file, which is the path the store swap created: two
/// positional reads per entry, no cursor, nothing shared but an `Arc<File>`.
#[test]
fn one_handle_serves_two_threads_reading_records() -> varve::Result<()> {
    fn assert_sync<T: Sync>() {}
    fn assert_send<T: Send>() {}
    assert_sync::<varve::VarveFile>();
    assert_send::<varve::VarveFile>();

    let directory = tempfile::tempdir()?;
    let path = directory.path().join("threaded.shrbrw");
    build(&path)?;
    let file = SharedBorrowFormat::open_readonly(&path)?;
    let expected = read_everything(&file)?;

    std::thread::scope(|scope| {
        let handles: Vec<_> = (0..4)
            .map(|_| {
                let shared = &file;
                scope.spawn(move || read_everything(shared))
            })
            .collect();
        for handle in handles {
            assert_eq!(
                handle
                    .join()
                    .expect("thread panicked")
                    .expect("read failed"),
                expected,
                "a thread sharing the handle read a different file"
            );
        }
    });
    Ok(())
}
