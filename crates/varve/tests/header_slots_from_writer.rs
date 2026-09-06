//! Reaching the editable header region from a writer.
//!
//! The region's whole point is that the writer is the handle that knows what
//! belongs in it: an append returns its own `record_offset`, and recording that
//! offset in the header is what lets a later reader seed a walk without
//! rebuilding an index. But `write_header_block` lived only on `VarveFile`, and
//! every hop down to it handed out `into_inner(self)` and nothing else:
//!
//! ```text
//! <Format>Writer            -- into_inner(self) -> VarveWriter
//!   VarveWriter             -- into_inner(self) -> VarveFile
//!     VarveFile             -- write_header_block(&mut self)
//! ```
//!
//! So the only route to it ended the writer. The read side had no such problem,
//! which made the gap easy to miss: `<Format>::open_readonly` hands back a bare
//! `VarveFile`, so `read_header_block` was always reachable.
//!
//! What these establish, in order:
//!
//! - `a_generated_writer_writes_the_region_directly` — the gap is closed at the
//!   outermost layer, which is the one a format's users actually hold.
//! - `the_writer_keeps_writing_afterwards` — the write did not consume it.
//! - `an_offset_recorded_at_append_seeds_a_read` — the use case end to end: the
//!   writer records what `push_*` returned, and a fresh reader walks from it
//!   without building an index.
//! - `a_bare_varve_writer_reaches_it_too` — the middle layer, for a caller not
//!   using the generated wrapper.
//! - `the_accessors_agree_across_all_three_layers` — the forwarding is
//!   forwarding, not a second implementation.

use varve::{VarveFile, VarveWriter, varve_format};

// `TailIndex` is what the writer records: where each record starts, so a reader
// can seed a walk from the header instead of rebuilding an index to find it.
varve_format! {
    pub format Tails {
        magic: b"HSWRITER";
        version: 1;
        schema_hash: computed;
        header_slots {
            capacity: 4096;
            integrity: rolling;
            blocks: [TailIndex];
        }
        blocks {
            variable Sample(id = 1) {
                value: u64,
            }
            variable TailIndex(id = 2) {
                offsets: Vec<u64>,
            }
        }
    }
}

#[test]
fn a_generated_writer_writes_the_region_directly() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("w.varve");

    // No `into_inner` anywhere in this test. That is the point.
    let mut writer = Tails::create_writer(&path).expect("create");
    writer
        .write_header_block(&TailIndex {
            offsets: vec![7, 11],
        })
        .expect("write through the generated writer");

    assert_eq!(
        writer.read_header_block::<TailIndex>().expect("read"),
        Some(TailIndex {
            offsets: vec![7, 11]
        }),
        "the writer reads back what it just wrote",
    );
    assert_eq!(writer.header_slots_capacity(), 4096);
    assert!(writer.header_slots_used().expect("used") > 0);
    assert!(!writer.header_slots_sealed().expect("sealed"));
    drop(writer);

    let reader = Tails::open_readonly(&path).expect("reopen");
    assert_eq!(
        reader.read_header_block::<TailIndex>().expect("read"),
        Some(TailIndex {
            offsets: vec![7, 11]
        }),
        "and it reached the disk, not just the cached header",
    );
}

#[test]
fn the_writer_keeps_writing_afterwards() {
    // `into_inner` would have ended it here. A forwarding method must not.
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("w.varve");

    let mut writer = Tails::create_writer(&path).expect("create");
    writer.push_sample(&Sample { value: 1 }).expect("append");
    writer
        .write_header_block(&TailIndex { offsets: vec![1] })
        .expect("write region");
    writer.push_sample(&Sample { value: 2 }).expect("append");
    writer
        .write_header_block(&TailIndex {
            offsets: vec![1, 2],
        })
        .expect("rewrite region");
    writer.push_sample(&Sample { value: 3 }).expect("append");
    drop(writer);

    let reader = Tails::open_readonly(&path).expect("reopen");
    assert_eq!(
        reader.read_header_block::<TailIndex>().expect("read"),
        Some(TailIndex {
            offsets: vec![1, 2]
        }),
    );
    let mut entries = Vec::new();
    reader
        .block_entries_into::<Sample>(&mut entries)
        .expect("entries");
    assert_eq!(
        entries.len(),
        3,
        "all three appends survived the region writes"
    );
}

#[test]
fn an_offset_recorded_at_append_seeds_a_read() {
    // The use case, end to end. The writer records the `record_offset` each
    // append returned; a fresh reader takes those offsets out of the header --
    // no I/O, no index -- and reads the records positionally.
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("w.varve");

    let mut writer = Tails::create_writer(&path).expect("create");
    let mut offsets = Vec::new();
    for value in 0..8u64 {
        let info = writer
            .inner_mut()
            .push_info(&Sample { value })
            .expect("append returns its own offset");
        offsets.push(info.record_offset);
    }
    writer
        .write_header_block(&TailIndex {
            offsets: offsets.clone(),
        })
        .expect("record them in the header");
    drop(writer);

    let reader = Tails::open_readonly(&path).expect("reopen");
    let seeds = reader
        .read_header_block::<TailIndex>()
        .expect("read")
        .expect("present");
    assert_eq!(seeds.offsets, offsets);

    for (value, offset) in seeds.offsets.iter().copied().enumerate() {
        let sample = reader
            .read_block_at::<Sample>(offset)
            .expect("positional read from a header-supplied offset");
        assert_eq!(sample.value, value as u64);
    }
}

#[test]
fn a_bare_varve_writer_reaches_it_too() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("w.varve");

    let mut writer = VarveWriter::create(Tails::spec(), &path).expect("create");
    writer
        .write_header_block(&TailIndex { offsets: vec![42] })
        .expect("write");
    assert_eq!(
        writer.read_header_block::<TailIndex>().expect("read"),
        Some(TailIndex { offsets: vec![42] }),
    );
    // And the escape hatch is a borrow now, not a move.
    assert_eq!(writer.file_mut().header_slots_capacity(), 4096);
    assert_eq!(writer.file().header_slots_capacity(), 4096);
    drop(writer);

    let reader = VarveFile::open_readonly(Tails::spec(), &path).expect("reopen");
    assert_eq!(
        reader.read_header_block::<TailIndex>().expect("read"),
        Some(TailIndex { offsets: vec![42] }),
    );
}

#[test]
fn the_accessors_agree_across_all_three_layers() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("w.varve");

    let mut generated = Tails::create_writer(&path).expect("create");
    generated
        .write_header_block(&TailIndex {
            offsets: vec![1, 2, 3],
        })
        .expect("write");

    let capacity = generated.header_slots_capacity();
    let used = generated.header_slots_used().expect("used");
    let free = generated.header_slots_free_bytes().expect("free");
    let sealed = generated.header_slots_sealed().expect("sealed");

    let inner = generated.inner();
    assert_eq!(inner.header_slots_capacity(), capacity);
    assert_eq!(inner.header_slots_used().expect("used"), used);
    assert_eq!(inner.header_slots_free_bytes().expect("free"), free);
    assert_eq!(inner.header_slots_sealed().expect("sealed"), sealed);

    let file = generated.inner().file();
    assert_eq!(file.header_slots_capacity(), capacity);
    assert_eq!(file.header_slots_used().expect("used"), used);
    assert_eq!(file.header_slots_free_bytes().expect("free"), free);
    assert_eq!(file.header_slots_sealed().expect("sealed"), sealed);

    assert_eq!(used + free, capacity, "used and free partition the region");
}
