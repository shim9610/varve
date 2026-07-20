// F-03 (round 8): the generated keyed writer's `delete_*` used to evaluate
// `key.clone()` *after* `delete_with_prev_key_info` had already appended the
// tombstone and updated writer state.
//
// `VarveWriter::reserve_keyed_tail_slot` reserves the `HashMap` slot, but by
// its own documented contract it charges - and can only reserve - the inline
// `(K, u64)` storage. It cannot reserve heap owned by `K`. So for a heap-owning
// key type the clone could still allocate after the mutation was authoritative,
// and a user-defined `Clone` could panic there outright, unwinding out of a
// writer whose file had already grown.
//
// The clone now runs after the reservation but before the authoritative delete,
// and the owned key is moved into the reserved slot afterwards. This test drives
// the failure with a key type whose `Clone` panics on command and proves that
// file length, sequence, index, and visible state are all unchanged.

use std::cell::Cell;
use std::panic::AssertUnwindSafe;
use std::path::Path;

use varve::{Decoder, Encoder, VarveDecode, VarveEncode, WireType, varve_format};

thread_local! {
    /// When set, the next [`PanicKey::clone`] panics instead of copying.
    static CLONE_PANICS: Cell<bool> = const { Cell::new(false) };
}

/// A key whose `Clone` is under the test's control.
///
/// Everything else about it is an ordinary `u64` codec: it is the *timing* of
/// the clone that is under test, not the key's contents.
#[derive(Debug, PartialEq, Eq, Hash)]
pub struct PanicKey(pub u64);

impl Clone for PanicKey {
    fn clone(&self) -> Self {
        if CLONE_PANICS.with(Cell::get) {
            panic!("PanicKey::clone was armed to panic");
        }
        Self(self.0)
    }
}

impl VarveEncode for PanicKey {
    const WIRE_TYPE: WireType = WireType::U64;
    const SCHEMA_ID: u64 = 0x50_41_4e_4b_00_00_00_01; // "PANK" v1

    fn encode_varve(&self, encoder: &mut Encoder) -> varve::Result<()> {
        encoder.write_u64(self.0);
        Ok(())
    }
}

impl VarveDecode for PanicKey {
    const WIRE_TYPE: WireType = WireType::U64;
    const SCHEMA_ID: u64 = <PanicKey as VarveEncode>::SCHEMA_ID;

    fn decode_varve(decoder: &mut Decoder<'_>) -> varve::Result<Self> {
        Ok(Self(decoder.read_u64()?))
    }
}

varve_format! {
    pub format PanicKeyFormat {
        magic: b"PANKEY";
        version: 1;
        index: keyed_offset_chain;
        blocks {
            variable Tracked(id = 51, key = [id]) {
                id: PanicKey,
                value: u32,
            }
        }
    }
}

/// `(sequence, record_offset, payload_len, prev_same_key_offset, committed)`
/// for every record, read back from a freshly opened file.
type IndexSnapshot = Vec<(u64, u64, u64, Option<u64>, bool)>;

/// The whole observable state of the file: byte length, index, visible records.
struct Snapshot {
    len: u64,
    index: IndexSnapshot,
    visible: Vec<(u64, u32)>,
}

fn snapshot(path: &Path) -> varve::Result<Snapshot> {
    let len = std::fs::metadata(path)?.len();
    let file = PanicKeyFormat::open(path)?;
    let index = file
        .index_entries()
        .iter()
        .map(|entry| {
            (
                entry.sequence,
                entry.record_offset,
                entry.payload_len,
                entry.prev_same_key_offset,
                entry.committed,
            )
        })
        .collect();
    let keyed = file.keyed_blocks::<Tracked>()?;
    let mut visible: Vec<(u64, u32)> = Vec::new();
    for key in keyed.keys() {
        let block = keyed
            .get(key)?
            .expect("a key enumerated from the map must resolve");
        visible.push((block.id.0, block.value));
    }
    visible.sort_unstable();
    Ok(Snapshot {
        len,
        index,
        visible,
    })
}

fn assert_unchanged(before: &Snapshot, after: &Snapshot) {
    assert_eq!(
        before.len, after.len,
        "a panicking key clone must not leave the file longer than it was",
    );
    assert_eq!(
        before.index, after.index,
        "a panicking key clone must not append a record or move the sequence",
    );
    assert_eq!(
        before.visible, after.visible,
        "a panicking key clone must not make a tombstone visible",
    );
}

/// The delete path with a key that is already in the writer's tail map, so the
/// reservation short-circuits and the clone is the only thing that can fail.
#[test]
fn a_panicking_key_clone_cannot_land_a_tombstone_for_a_known_key() -> varve::Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("panicking-clone-known-key.varve");

    {
        let mut writer = PanicKeyFormat::create_writer(&path)?;
        for id in 1..=3u64 {
            writer.push_tracked(&Tracked {
                id: PanicKey(id),
                value: id as u32 * 10,
            })?;
        }
        writer.flush()?;
    }
    let before = snapshot(&path)?;
    assert_eq!(before.visible.len(), 3, "the fixture must have three keys");

    {
        let mut writer = PanicKeyFormat::open_writer(&path)?;
        CLONE_PANICS.with(|armed| armed.set(true));
        let outcome =
            std::panic::catch_unwind(AssertUnwindSafe(|| writer.delete_tracked(&PanicKey(2))));
        CLONE_PANICS.with(|armed| armed.set(false));
        assert!(
            outcome.is_err(),
            "the armed clone must have unwound out of the delete",
        );
        // Dropping the writer flushes whatever it buffered. If the tombstone
        // had already been appended before the clone ran, this is where it
        // would become visible.
        writer.flush()?;
    }

    assert_unchanged(&before, &snapshot(&path)?);
    Ok(())
}

/// The delete path with a key the writer has never seen, so the tail-slot
/// reservation runs for real before the clone. The reservation is charged and
/// may fail, which is fine - it happens before the mutation. The clone must be
/// on the same side of that line.
#[test]
fn a_panicking_key_clone_cannot_land_a_tombstone_for_an_unknown_key() -> varve::Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("panicking-clone-unknown-key.varve");

    {
        let mut writer = PanicKeyFormat::create_writer(&path)?;
        writer.push_tracked(&Tracked {
            id: PanicKey(1),
            value: 10,
        })?;
        writer.flush()?;
    }
    let before = snapshot(&path)?;

    {
        let mut writer = PanicKeyFormat::open_writer(&path)?;
        CLONE_PANICS.with(|armed| armed.set(true));
        let outcome =
            std::panic::catch_unwind(AssertUnwindSafe(|| writer.delete_tracked(&PanicKey(99))));
        CLONE_PANICS.with(|armed| armed.set(false));
        assert!(
            outcome.is_err(),
            "the armed clone must have unwound out of the delete",
        );
        writer.flush()?;
    }

    assert_unchanged(&before, &snapshot(&path)?);
    Ok(())
}

/// The generated writer *trait* implementation carries its own copy of the
/// delete body, so it needs its own coverage: fixing only the inherent method
/// would leave the trait route with the original post-mutation clone.
#[test]
fn a_panicking_key_clone_cannot_land_a_tombstone_through_the_writer_trait() -> varve::Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("panicking-clone-trait.varve");

    {
        let mut writer = PanicKeyFormat::create_writer(&path)?;
        for id in 1..=2u64 {
            writer.push_tracked(&Tracked {
                id: PanicKey(id),
                value: id as u32,
            })?;
        }
        writer.flush()?;
    }
    let before = snapshot(&path)?;

    {
        let mut writer = PanicKeyFormat::open_writer(&path)?;
        CLONE_PANICS.with(|armed| armed.set(true));
        let outcome = std::panic::catch_unwind(AssertUnwindSafe(|| {
            PanicKeyFormatWrite::delete_tracked(&mut writer, &PanicKey(1))
        }));
        CLONE_PANICS.with(|armed| armed.set(false));
        assert!(
            outcome.is_err(),
            "the armed clone must have unwound out of the trait delete",
        );
        writer.flush()?;
    }

    assert_unchanged(&before, &snapshot(&path)?);
    Ok(())
}

/// The same writer must still work normally once the clone stops panicking:
/// the failed delete leaves no half-applied reservation or poisoned state.
#[test]
fn the_writer_still_deletes_after_a_clone_panic() -> varve::Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("panicking-clone-recovery.varve");

    let mut writer = PanicKeyFormat::create_writer(&path)?;
    for id in 1..=2u64 {
        writer.push_tracked(&Tracked {
            id: PanicKey(id),
            value: id as u32,
        })?;
    }
    CLONE_PANICS.with(|armed| armed.set(true));
    let outcome =
        std::panic::catch_unwind(AssertUnwindSafe(|| writer.delete_tracked(&PanicKey(1))));
    CLONE_PANICS.with(|armed| armed.set(false));
    assert!(outcome.is_err(), "the armed clone must have unwound");

    writer.delete_tracked(&PanicKey(1))?;
    writer.flush()?;
    drop(writer);

    let after = snapshot(&path)?;
    assert_eq!(
        after.visible,
        vec![(2, 2)],
        "the retried delete must be the only tombstone that landed",
    );
    Ok(())
}
