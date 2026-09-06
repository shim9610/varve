//! Walking one key's history, newest first.
//!
//! The keyed predecessor chain has always been written — every keyed record's
//! footer carries the offset of the previous record with the same key — and
//! `prev_same_key_offset` has always been a public field. What was missing was
//! any way to *use* it: `read_block_at` turns an offset into a value and
//! discards the entry it framed to get there, so a hand-written walk read the
//! value at hop one and had nowhere to go for hop two. `keyed_blocks` answers a
//! different question, the latest record per key.
//!
//! `VarveFile::record_entry_at` is that missing primitive and
//! `VarveFile::keyed_chain` is the walk built on it.
//!
//! What these establish, in order:
//!
//! - `record_entry_at_returns_what_read_block_at_discards` — the primitive, and
//!   that it agrees with the index for the same record.
//! - `a_hand_written_walk_reaches_every_generation` — the primitive alone is
//!   enough; 54 hops, by hand, no iterator.
//! - `keyed_chain_walks_the_same_54_hops` — the iterator agrees with the hand
//!   walk, hop for hop.
//! - `the_walk_crosses_a_tombstone` — the difference from `block_chain` that
//!   makes this a separate type. A delete must not end the history.
//! - `the_walk_costs_the_hops_and_not_the_file` — the reason it is lazy: the
//!   same 54 hops over a file with 20,000 unrelated records.
//! - `keyed_chain_is_refused_without_the_declaration` — without
//!   `keyed_offset_chain` there is no predecessor to follow, and saying so
//!   beats returning one record and looking like an answer.
//! - `a_chain_that_does_not_decrease_is_refused` — the loop guard.

use std::collections::HashMap;

use varve::{Error, VarveBlock, VarveFile, VarveMerge, varve_format};

varve_format! {
    pub format Keyed {
        magic: b"KCHAINWK";
        version: 1;
        schema_hash: computed;
        index: [keyed_offset_chain];
        blocks {
            variable Reading(id = 10, key = [sensor]) {
                sensor: u32,
                value: u64,
            }
            fixed Noise(id = 20) {
                value: u64,
            }
            variable ReadingOp(id = 30) {
                value: u64,
            }
        }
    }
}

impl VarveMerge for Reading {
    type Op = ReadingOp;

    fn apply_op(&mut self, op: Self::Op) -> varve::Result<()> {
        self.value = op.value;
        Ok(())
    }
}

varve_format! {
    // The same shape with the declaration left off: every record's
    // `prev_same_key_offset` is `None`, so there is nothing to walk.
    pub format Unchained {
        magic: b"KCHAINNO";
        version: 1;
        schema_hash: computed;
        blocks {
            variable Reading2(id = 10, key = [sensor]) {
                sensor: u32,
                value: u64,
            }
        }
    }
}

/// The number of hops the question was asked about.
const HOPS: u64 = 54;

/// Two keys, so a walk that ignored the key and followed the block chain would
/// see 108 records and give a different answer.
const SENSORS: [u32; 2] = [7, 9];

/// Writes `HOPS` generations of each sensor, interleaved, plus `noise`
/// unrelated records after them. Returns the value written per generation, in
/// write order, for the sensor under test.
fn build(path: &std::path::Path, noise: u64) -> Vec<u64> {
    let mut writer = Keyed::create_writer(path).expect("create");
    let mut written = Vec::new();
    for generation in 0..HOPS {
        for sensor in SENSORS {
            let value = generation * 1_000 + u64::from(sensor);
            writer
                .push_reading(&Reading { sensor, value })
                .expect("append");
            if sensor == SENSORS[0] {
                written.push(value);
            }
        }
    }
    for value in 0..noise {
        writer.push_noise(&Noise { value }).expect("append noise");
    }
    drop(writer);
    written
}

fn newest_offset(file: &VarveFile, sensor: u32) -> u64 {
    let tails: HashMap<u32, u64> = file.key_tail_offsets::<Reading>().expect("key tails");
    *tails.get(&sensor).expect("the sensor has records")
}

#[test]
fn record_entry_at_returns_what_read_block_at_discards() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("k.varve");
    build(&path, 0);

    let file = Keyed::open_readonly(&path).expect("open");
    let offset = newest_offset(&file, SENSORS[0]);

    let entry = file.record_entry_at(offset).expect("frame the record");
    assert_eq!(entry.record_offset, offset);
    assert_eq!(entry.block_id, Reading::ID);

    // It agrees with the resident index for the same record, so it is the same
    // entry by a route the caller steers rather than a second decoding of it.
    let mut all = Vec::new();
    file.index_entries_into(&mut all).expect("index");
    let from_index = all
        .iter()
        .find(|candidate| candidate.record_offset == offset)
        .expect("the index holds it too");
    assert_eq!(&entry, from_index);

    // And the predecessor is there, which is the whole point.
    assert!(
        entry.prev_same_key_offset.is_some(),
        "the newest of {HOPS} generations must have a predecessor",
    );
}

#[test]
fn a_hand_written_walk_reaches_every_generation() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("k.varve");
    let written = build(&path, 0);

    let file = Keyed::open_readonly(&path).expect("open");
    let mut offset = Some(newest_offset(&file, SENSORS[0]));
    let mut seen = Vec::new();
    while let Some(at) = offset {
        let entry = file.record_entry_at(at).expect("frame");
        seen.push(file.read_block_at::<Reading>(at).expect("decode").value);
        offset = entry.prev_same_key_offset;
    }

    seen.reverse();
    assert_eq!(
        seen, written,
        "the hand walk must reach all {HOPS} generations, oldest last",
    );
    assert_eq!(seen.len() as u64, HOPS);
}

#[test]
fn keyed_chain_walks_the_same_54_hops() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("k.varve");
    let written = build(&path, 0);

    let file = Keyed::open_readonly(&path).expect("open");
    let mut seen = Vec::new();
    for step in file
        .keyed_chain(newest_offset(&file, SENSORS[0]))
        .expect("chain")
    {
        let entry = step.expect("step");
        assert_eq!(entry.block_id, Reading::ID);
        seen.push(
            file.read_block_at::<Reading>(entry.record_offset)
                .expect("decode")
                .value,
        );
    }

    seen.reverse();
    assert_eq!(seen, written);

    // The other key is untouched by this walk, which is what makes it keyed
    // rather than a block walk: the block chain holds 2 * HOPS records.
    let other: Vec<u64> = file
        .keyed_chain(newest_offset(&file, SENSORS[1]))
        .expect("chain")
        .map(|step| {
            let entry = step.expect("step");
            file.read_block_at::<Reading>(entry.record_offset)
                .expect("decode")
                .value
        })
        .collect();
    assert_eq!(other.len() as u64, HOPS);
    assert!(
        other
            .iter()
            .all(|value| value % 1_000 == u64::from(SENSORS[1])),
        "the walk must not cross into the other key",
    );
}

#[test]
fn the_walk_crosses_a_tombstone() {
    // The one way this differs from `block_chain`, and the reason a caller
    // cannot get here with `block_entries_into::<T>`: a delete writes its
    // tombstone under a different block id, and it carries a live predecessor.
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("k.varve");
    build(&path, 0);

    let mut writer = Keyed::open_writer(&path).expect("reopen writer");
    writer
        .delete_reading(&SENSORS[0])
        .expect("delete the key's newest generation");
    drop(writer);

    let file = Keyed::open_readonly(&path).expect("open");
    let mut readings = 0u64;
    let mut foreign_ids = 0u64;
    for step in file
        .keyed_chain(newest_offset(&file, SENSORS[0]))
        .expect("chain")
    {
        let entry = step.expect("step");
        if entry.block_id == Reading::ID {
            readings += 1;
        } else {
            foreign_ids += 1;
        }
    }

    assert!(
        foreign_ids >= 1,
        "the walk must pass through the tombstone rather than stop at it",
    );
    assert_eq!(
        readings, HOPS,
        "and it must still reach every generation behind the tombstone",
    );
}

#[test]
fn the_walk_costs_the_hops_and_not_the_file() {
    // 20,000 unrelated records behind the same 54 hops. The walk holds one
    // offset, so this is the property that makes it usable on a file larger
    // than memory: it never frames a record it was not sent to.
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("k.varve");
    let written = build(&path, 20_000);

    let file = Keyed::open_readonly(&path).expect("open");
    let seen: Vec<u64> = file
        .keyed_chain(newest_offset(&file, SENSORS[0]))
        .expect("chain")
        .map(|step| {
            let entry = step.expect("step");
            file.read_block_at::<Reading>(entry.record_offset)
                .expect("decode")
                .value
        })
        .collect();

    assert_eq!(seen.len() as u64, HOPS);
    assert_eq!(seen.iter().rev().copied().collect::<Vec<_>>(), written);
}

#[test]
fn keyed_chain_is_refused_without_the_declaration() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("u.varve");
    let mut writer = Unchained::create_writer(&path).expect("create");
    for value in 0..4u64 {
        writer
            .push_reading2(&Reading2 { sensor: 1, value })
            .expect("append");
    }
    drop(writer);

    let file = Unchained::open_readonly(&path).expect("open");
    let offset = *file
        .key_tail_offsets::<Reading2>()
        .expect("tails")
        .get(&1)
        .expect("present");

    // The record is readable; only the walk is refused, and refused rather than
    // silently returning one record.
    assert!(file.record_entry_at(offset).is_ok());
    match file.keyed_chain(offset) {
        Err(Error::InvalidFormatSpec(message)) => assert!(
            message.contains("keyed_chain requires keyed_offset_chain"),
            "unexpected refusal: {message}",
        ),
        other => panic!("expected a refusal, got {other:?}"),
    }
}

#[test]
fn a_chain_that_does_not_decrease_is_refused() {
    // The loop guard. A caller seeding the walk from an offset whose record
    // points at itself or forwards would otherwise walk forever; the offsets a
    // writer produces are strictly decreasing, so this can only be damage or a
    // craft, and either way the walk ends.
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("k.varve");
    build(&path, 0);

    let file = Keyed::open_readonly(&path).expect("open");
    // Seed from the *oldest* generation: its predecessor is `None`, so a
    // well-formed walk is exactly one step and terminates. This is the control
    // that the guard is not what ends an ordinary walk.
    let oldest = {
        let mut at = newest_offset(&file, SENSORS[0]);
        while let Some(previous) = file
            .record_entry_at(at)
            .expect("frame")
            .prev_same_key_offset
        {
            at = previous;
        }
        at
    };
    let steps = file.keyed_chain(oldest).expect("chain").count();
    assert_eq!(steps, 1, "the oldest generation ends the walk by itself");
}

#[test]
fn an_op_record_is_not_on_the_chain() {
    // The claim this file shipped with -- that the walk crosses `OP_BLOCK_ID`
    // the way it crosses `TOMBSTONE_BLOCK_ID` -- was wrong, and wrong in the
    // direction that loses data silently: a caller folding a key's history from
    // the walk alone would drop every op-applied mutation with no error.
    //
    // `push_op` goes through `write_record`, which passes `None` for the keyed
    // predecessor, and only `T::ID` and `TOMBSTONE_BLOCK_ID` move a keyed tail.
    // So an op record is neither *on* the chain nor pointed *at* by it.
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("k.varve");
    build(&path, 0);

    let mut writer = Keyed::open_writer(&path).expect("reopen writer");
    writer
        .inner_mut()
        .push_op::<Reading>(&SENSORS[0], &ReadingOp { value: 12_345 })
        .expect("append an op");
    drop(writer);

    // `push_op` returns a sequence, not an `AppendInfo`, so find the record it
    // wrote the way anything else would: it is the only one under OP_BLOCK_ID.
    let file = Keyed::open_readonly(&path).expect("open");
    let mut all = Vec::new();
    file.index_entries_into(&mut all).expect("index");
    let op_entries: Vec<_> = all
        .iter()
        .filter(|entry| entry.block_id == varve::OP_BLOCK_ID)
        .collect();
    assert_eq!(op_entries.len(), 1, "exactly one op record was written");
    let op = op_entries[0];

    assert_eq!(
        op.prev_same_key_offset, None,
        "an op record records no keyed predecessor",
    );

    let newest = newest_offset(&file, SENSORS[0]);
    assert_ne!(
        newest, op.record_offset,
        "and it does not become the key's tail",
    );

    let mut walked = Vec::new();
    for step in file.keyed_chain(newest).expect("chain") {
        walked.push(step.expect("step"));
    }
    assert!(
        walked
            .iter()
            .all(|entry| entry.record_offset != op.record_offset),
        "the op record is not reachable from the walk",
    );
    assert_eq!(
        walked.len() as u64,
        HOPS,
        "the walk still reaches every generation, and gains nothing from the op",
    );
}
