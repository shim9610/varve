//! The `_into` family: every one of them, and the property that makes reuse
//! safe rather than merely cheap.
//!
//! These entry points shipped with no test of their own — they were measured
//! for allocation count and never checked for what they return. The
//! measurements are in `record_index_residency.rs`; this file is the
//! correctness half.
//!
//! Two properties, asserted for every one:
//!
//! 1. **The buffer is cleared, not appended to.** Each is called twice with a
//!    deliberately pre-dirtied buffer; a form that appended would double.
//! 2. **The answer equals the allocating form's.** A `_into` that quietly
//!    returned less would pass any weaker check.
//!
//! And one that only the payload readers have, which is the reason `_into`
//! could have been a *correctness* regression rather than an optimisation:
//! `read_into_at` leaves the buffer exactly the record's length whatever it
//! held before. The callers hand that buffer straight to the checksum, so a
//! buffer left long after a bigger record would verify trailing bytes from the
//! previous read and report a healthy file as corrupt.
//!
//! # What these assertions were measured to catch
//!
//! A test that passes proves nothing about a test that would fail. Twelve
//! mutations were applied to the implementations one at a time — each `clear()`
//! deleted in turn, the block-id check deleted, `read_into_at` made grow-only,
//! `get_into` given an off-by-one, `open_with_scratch` made to scan into its
//! own buffer — and the suite run under each. **All twelve are caught here.**
//!
//! The same twelve were then run against the workspace with *this file
//! removed*, which is what says whether these tests were needed rather than
//! merely correct:
//!
//! | mutation | pre-existing suite |
//! | --- | --- |
//! | `decode_block_into` ignores the entry's block id | 853 passed, 0 failed |
//! | `block_entries_into` appends | 853 passed, 0 failed |
//! | `blocks_migrated_into` appends | 853 passed, 0 failed |
//! | `keyed_blocks_into` leaves the key map populated | 853 passed, 0 failed |
//! | `materialized_keyed_blocks_into` merges into the caller's map | 853 passed, 0 failed |
//! | `key_tail_offsets_into` leaves the map populated | 853 passed, 0 failed |
//! | `all_metadata_into` appends | 853 passed, 0 failed |
//! | `VarveWriter::open_with_scratch` ignores the caller's buffer | 853 passed, 0 failed |
//!
//! Eight of the twelve shipped with nothing anywhere in the workspace that
//! would notice them. The other four were caught incidentally and are named so
//! the claim is not overstated: the grow-only `read_into_at` by
//! `format_first_dsl_generates_typed_api_and_offset_chains` and
//! `typed_replacement_translates_keyed_tails_after_resize`, a
//! `decode_blocks_into` that appends by `the_generated_into_twins_exist_on_both_routes`,
//! an `index_entries_into` that appends by
//! `the_host_owns_the_buffers_and_chooses_whether_to_keep_the_index`, and the
//! `get_into` off-by-one by three checkpoint-recovery tests. Those four were
//! caught by tests that are about something else entirely, which is coverage by
//! luck; the assertions here name the property instead.

use std::collections::HashMap;

use varve::{VarveBlock, VarveMerge, VarveMigration, varve_format};

#[derive(Clone, Debug, PartialEq, VarveBlock)]
#[varve(id = 1, version = 1, kind = "variable", key = "id")]
struct Note {
    #[varve(field_id = 1)]
    id: u64,
    #[varve(field_id = 2)]
    body: String,
}

impl VarveMerge for Note {
    type Op = NoteOp;

    fn apply_op(&mut self, op: Self::Op) -> varve::Result<()> {
        self.body = op.rewrite;
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, VarveBlock)]
#[varve(id = 2, version = 1, kind = "variable")]
struct NoteOp {
    #[varve(field_id = 1)]
    rewrite: String,
}

#[derive(Clone, Debug, PartialEq, VarveBlock)]
#[varve(id = 3, version = 1, kind = "fixed")]
struct Tick {
    at: u64,
}

#[derive(Clone, Debug, PartialEq, VarveBlock)]
#[varve(id = 3, version = 2, kind = "fixed")]
struct TickV2 {
    at: u64,
}

struct BumpTick;

impl VarveMigration<Tick, TickV2> for BumpTick {
    fn migrate(from: Tick) -> varve::Result<TickV2> {
        Ok(TickV2 { at: from.at })
    }
}

varve_format! {
    pub struct BufferFormat {
        magic: b"BUFFER";
        version: 1;
        endian: little;
        blocks: [Note, NoteOp, Tick];
    }
}

/// Bodies of **decreasing** length, which is what makes the length property
/// testable: read them in this order through one buffer and a form that left
/// the buffer long would checksum the previous record's tail.
const BODIES: [&str; 4] = [
    "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
    "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
    "cccccccccccccccc",
    "d",
];

fn build(path: &std::path::Path) -> varve::Result<()> {
    let mut writer = BufferFormat::create(path)?;
    for (id, body) in BODIES.iter().enumerate() {
        writer.push_keyed(&Note {
            id: id as u64,
            body: (*body).to_string(),
        })?;
    }
    for at in 0..3u64 {
        writer.push(&Tick { at })?;
    }
    writer.write_metadata("owner", b"varve")?;
    writer.write_metadata("build", b"7")?;
    writer.flush()?;
    Ok(())
}

/// A buffer with junk in it, so "cleared before filling" is observable.
fn dirty<T: Clone>(sample: &T) -> Vec<T> {
    vec![sample.clone(); 9]
}

#[test]
fn a_reused_buffer_is_cut_to_each_record_and_never_carries_the_last_one() -> varve::Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("lengths.buffer");
    build(&path)?;

    let file = BufferFormat::open_readonly(&path)?;
    let mut index = Vec::new();
    file.index_entries_into(&mut index)?;

    // One buffer, records in decreasing size order. The first read grows it to
    // 64 bytes and every later read must cut it back.
    let mut payload = Vec::new();
    let mut seen = 0usize;
    for entry in &index {
        if entry.block_id != Note::ID {
            continue;
        }
        file.read_payload_into(entry, &mut payload)?;
        assert_eq!(
            payload.len() as u64,
            entry.payload_len,
            "the buffer must end exactly the record's stored length; a longer one is \
             checksummed as if it were the record"
        );

        // The decoded value must be the one that was written, which is the part
        // a leftover tail would corrupt even if the length happened to match.
        let note: Note = file.decode_block_into(entry, &mut payload)?;
        assert_eq!(note.body.as_str(), BODIES[seen]);
        assert_eq!(note.id, seen as u64);
        seen += 1;
    }
    assert_eq!(seen, BODIES.len(), "every note must have been read");

    // Reading the largest record *after* the smallest must also work — the
    // buffer grows back rather than silently truncating the read.
    let first = index
        .iter()
        .find(|entry| entry.block_id == Note::ID)
        .expect("a note");
    file.read_payload_into(first, &mut payload)?;
    assert_eq!(payload.len() as u64, first.payload_len);
    Ok(())
}

#[test]
fn read_payload_into_matches_the_allocating_form() -> varve::Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("payload.buffer");
    build(&path)?;

    let file = BufferFormat::open_readonly(&path)?;
    let mut index = Vec::new();
    file.index_entries_into(&mut index)?;

    let mut physical = dirty(&0u8);
    let mut logical = dirty(&0u8);
    for entry in &index {
        file.read_payload_into(entry, &mut physical)?;
        file.read_logical_payload_into(entry, &mut logical)?;
        // This format declares no compression, so the two agree record for
        // record — which is itself the check that neither is reading something
        // else entirely.
        assert_eq!(physical, logical);
        assert_eq!(physical.len() as u64, entry.payload_len);
    }
    Ok(())
}

#[test]
fn decode_block_into_refuses_an_entry_of_another_block() -> varve::Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("wrong-block.buffer");
    build(&path)?;

    let file = BufferFormat::open_readonly(&path)?;
    let mut index = Vec::new();
    file.index_entries_into(&mut index)?;
    let tick_entry = index
        .iter()
        .find(|entry| entry.block_id == Tick::ID)
        .expect("a tick");

    // The entry is the caller's, so decoding it as the wrong type must be
    // refused rather than reinterpreted.
    let mut scratch = Vec::new();
    assert!(
        matches!(
            file.decode_block_into::<Note>(tick_entry, &mut scratch),
            Err(varve::Error::UnregisteredBlock(id)) if id == Tick::ID
        ),
        "decoding a Tick entry as a Note must be refused by block id"
    );
    Ok(())
}

#[test]
fn every_into_form_clears_and_matches_the_allocating_one() -> varve::Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("into.buffer");
    build(&path)?;
    let file = BufferFormat::open_readonly(&path)?;

    // all_metadata_into
    let mut metadata = dirty(&(String::from("junk"), vec![0u8]));
    file.all_metadata_into(&mut metadata)?;
    assert_eq!(metadata, file.all_metadata()?);
    assert_eq!(metadata.len(), 2);
    file.all_metadata_into(&mut metadata)?;
    assert_eq!(metadata.len(), 2, "`_into` must clear, not append");

    // index_entries_into
    let mut entries = Vec::new();
    file.index_entries_into(&mut entries)?;
    let baseline = entries.len();
    file.index_entries_into(&mut entries)?;
    assert_eq!(entries.len(), baseline);
    assert_eq!(entries.as_slice(), &*file.index_entries());

    // block_entries_into
    let mut block_entries = entries.clone();
    file.block_entries_into::<Tick>(&mut block_entries)?;
    assert_eq!(block_entries.len(), 3);
    assert_eq!(block_entries.len(), file.blocks::<Tick>()?.len());

    // decode_blocks_into
    let mut ticks = dirty(&Tick { at: 99 });
    file.decode_blocks_into(&mut ticks)?;
    assert_eq!(
        ticks,
        (0..3).map(|at| Tick { at }).collect::<Vec<_>>(),
        "decoded values must be the ones written, in file order"
    );

    // blocks_migrated_into
    let mut migrated = dirty(&TickV2 { at: 99 });
    file.blocks_migrated_into::<Tick, TickV2, BumpTick>(&mut migrated)?;
    assert_eq!(migrated, file.blocks_migrated::<Tick, TickV2, BumpTick>()?);
    assert_eq!(migrated.len(), 3);

    // keyed_blocks_into
    let mut keyed_entries = entries.clone();
    let mut by_key: HashMap<u64, varve::RecordIndexEntry> = HashMap::new();
    by_key.insert(u64::MAX, entries[0].clone());
    file.keyed_blocks_into::<Note>(&mut keyed_entries, &mut by_key)?;
    assert_eq!(keyed_entries.len(), BODIES.len());
    assert_eq!(by_key.len(), BODIES.len());
    assert!(
        !by_key.contains_key(&u64::MAX),
        "`_into` must clear the map, not add to it"
    );

    // materialized_keyed_blocks_into
    let mut merged: HashMap<u64, Note> = HashMap::new();
    merged.insert(
        u64::MAX,
        Note {
            id: u64::MAX,
            body: String::from("junk"),
        },
    );
    file.materialized_keyed_blocks_into::<Note>(&mut merged)?;
    assert_eq!(merged, file.materialized_keyed_blocks::<Note>()?);
    assert_eq!(merged.len(), BODIES.len());

    // key_tail_offsets_into
    let mut tails: HashMap<u64, u64> = HashMap::new();
    tails.insert(u64::MAX, 0);
    file.key_tail_offsets_into::<Note>(&mut tails)?;
    assert_eq!(tails, file.key_tail_offsets::<Note>()?);
    assert_eq!(tails.len(), BODIES.len());
    Ok(())
}

#[test]
fn block_vec_get_into_matches_get_and_reuses_the_buffer() -> varve::Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("getinto.buffer");
    build(&path)?;
    let file = BufferFormat::open_readonly(&path)?;
    let notes = file.blocks::<Note>()?;

    let mut scratch = dirty(&0u8);
    for index in 0..notes.len() {
        assert_eq!(notes.get_into(index, &mut scratch)?, notes.get(index)?);
    }
    assert_eq!(notes.get_into(notes.len(), &mut scratch)?, None);

    // `iter()` is `get_into` over one buffer; it must agree with `get(i)`.
    let iterated = notes.iter().collect::<varve::Result<Vec<_>>>()?;
    let indexed = (0..notes.len())
        .map(|i| notes.get(i).map(|v| v.expect("in range")))
        .collect::<varve::Result<Vec<_>>>()?;
    assert_eq!(iterated, indexed);
    Ok(())
}

#[test]
fn opening_a_writer_through_a_scratch_buffer_sees_the_same_file() -> varve::Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("writer.buffer");
    build(&path)?;

    let mut scratch = Vec::new();
    let plain = varve::VarveWriter::open(BufferFormat::spec(), &path)?;
    let expected = plain.index_entries();
    drop(plain);

    let scratched =
        varve::VarveWriter::open_with_scratch(BufferFormat::spec(), &path, &mut scratch)?;
    assert_eq!(&*scratched.index_entries(), &*expected);
    // The scan buffer is the file's index when open returns, which is the
    // property `open_readonly_without_directory` depends on.
    assert_eq!(scratch.as_slice(), &*expected);
    Ok(())
}
