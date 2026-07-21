// F-01 (round 9): no user-defined trait may run after a generated keyed
// mutation is authoritative.
//
// Round 8 fixed one instance of this: the generated `delete_*` cloned the
// borrowed key *after* the tombstone had been appended. The clone moved, but
// the shape stayed - the generated writers kept a `HashMap<T::Key, u64>` tail
// map and filled it with `HashMap::insert` after the append, and that insert
// runs the caller's `Hash` and (on a bucket collision) `Eq`. A stateful or
// panicking implementation could therefore succeed before the append and fail
// after it. If the unwind was caught, the writer stayed reachable holding a
// stale predecessor, and the next same-key mutation linked *around* the record
// that had in fact been committed.
//
// The structural fix removes the typed map. Generated keyed writers now route
// through `VarveWriter::push_keyed_info` / `VarveWriter::delete_info`, which
// maintain the byte-keyed resident cache the generic keyed API already used:
// the cache key is the canonical internal key payload, so the only step after
// the authoritative append is an insert of an owned `Vec<u8>` into a slot
// reserved and charged beforehand. No `Clone`, `Hash` or `Eq` of the caller's
// key type executes there - and this file asserts that none executes at all.
//
// The tests below drive every user trait the tail machinery could ever touch
// (`Clone`, `Hash`, `Eq`), on both mutations (push, delete), through both
// generated routes (inherent method, writer trait), with the panic armed on
// each of the first six invocations in turn. Two properties are asserted for
// every combination:
//
//   1. if the call unwinds, file length, sequence, index and visible state are
//      exactly what they were before it;
//   2. whether it unwinds or not, the *next* same-key mutation on the same
//      writer links to the newest committed record for that key - never around
//      it.
//
// Against the round-8 code, arming `Hash` on its second invocation lands a
// record and then unwinds out of the post-append insert, which fails (1), and
// the follow-up mutation links around it, which fails (2).

use std::cell::Cell;
use std::hash::{Hash, Hasher};
use std::panic::AssertUnwindSafe;
use std::path::Path;

use varve::{AppendInfo, Decoder, Encoder, VarveDecode, VarveEncode, WireType, varve_format};

/// The user-defined traits the keyed tail machinery could invoke.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum UserTrait {
    Clone = 0,
    Hash = 1,
    Eq = 2,
}

thread_local! {
    /// `(trait, nth)`: the `nth` invocation of `trait` panics.
    static ARMED: Cell<Option<(UserTrait, u32)>> = const { Cell::new(None) };
    /// Invocation counts, indexed by `UserTrait as usize`.
    static CALLS: Cell<[u32; 3]> = const { Cell::new([0; 3]) };
}

fn reset_calls() {
    CALLS.with(|calls| calls.set([0; 3]));
}

fn calls() -> [u32; 3] {
    CALLS.with(Cell::get)
}

/// Records one invocation of `which`, panicking if this is the armed one.
fn note(which: UserTrait) {
    let count = CALLS.with(|calls| {
        let mut counts = calls.get();
        counts[which as usize] += 1;
        calls.set(counts);
        counts[which as usize]
    });
    if let Some((armed, nth)) = ARMED.with(Cell::get)
        && armed == which
        && count == nth
    {
        panic!("PanicKey::{which:?} was armed to panic on call {nth}");
    }
}

/// A key whose `Clone`, `Hash` and `Eq` are all under the test's control.
///
/// `Hash` is deliberately constant so that every map lookup also exercises
/// `Eq`: an `Eq` that is never called cannot prove anything about when it is
/// called. The key sets in this file are tiny, so the degenerate hash costs
/// nothing.
#[derive(Debug)]
pub struct PanicKey(pub u64);

impl Clone for PanicKey {
    fn clone(&self) -> Self {
        note(UserTrait::Clone);
        Self(self.0)
    }
}

impl PartialEq for PanicKey {
    fn eq(&self, other: &Self) -> bool {
        note(UserTrait::Eq);
        self.0 == other.0
    }
}

impl Eq for PanicKey {}

impl Hash for PanicKey {
    fn hash<H: Hasher>(&self, state: &mut H) {
        note(UserTrait::Hash);
        state.write_u8(0);
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

fn assert_unchanged(before: &Snapshot, after: &Snapshot, what: &str) {
    assert_eq!(
        before.len, after.len,
        "{what}: a panicking key trait must not leave the file longer than it was",
    );
    assert_eq!(
        before.index, after.index,
        "{what}: a panicking key trait must not append a record or move the sequence",
    );
    assert_eq!(
        before.visible, after.visible,
        "{what}: a panicking key trait must not change what is visible",
    );
}

/// The two generated routes to the same mutation. Each has its own copy of the
/// method body, so each needs its own coverage.
#[derive(Clone, Copy, Debug)]
enum Route {
    Inherent,
    Trait,
}

#[derive(Clone, Copy, Debug)]
enum Op {
    Push,
    Delete,
}

/// The three keys the fixture writes, in push order.
const FIXTURE_KEYS: [u64; 3] = [1, 2, 3];

fn write_fixture(path: &Path) -> varve::Result<()> {
    let mut writer = PanicKeyFormat::create_writer(path)?;
    for key in FIXTURE_KEYS {
        writer.push_tracked(&Tracked {
            id: PanicKey(key),
            value: key as u32 * 10,
        })?;
    }
    writer.flush()?;
    Ok(())
}

fn perform(
    writer: &mut PanicKeyFormatWriter,
    op: Op,
    route: Route,
    key: u64,
    value: u32,
) -> varve::Result<AppendInfo> {
    let block = Tracked {
        id: PanicKey(key),
        value,
    };
    match (op, route) {
        (Op::Push, Route::Inherent) => writer.push_tracked(&block),
        (Op::Push, Route::Trait) => PanicKeyFormatWrite::push_tracked(writer, &block),
        (Op::Delete, Route::Inherent) => writer.delete_tracked(&PanicKey(key)),
        (Op::Delete, Route::Trait) => PanicKeyFormatWrite::delete_tracked(writer, &PanicKey(key)),
    }
}

/// Runs `body` with `which`'s `nth` invocation armed to panic, and reports
/// whether it unwound. Arming is cleared before returning, so the assertions
/// that follow can read the file freely.
fn with_armed<R>(which: UserTrait, nth: u32, body: impl FnOnce() -> R) -> Option<R> {
    reset_calls();
    ARMED.with(|armed| armed.set(Some((which, nth))));
    let outcome = std::panic::catch_unwind(AssertUnwindSafe(body));
    ARMED.with(|armed| armed.set(None));
    outcome.ok()
}

/// Offset of the newest record the fixture wrote for `key`, or `None` if the
/// fixture never mentioned it.
fn fixture_tail(before: &Snapshot, key: u64) -> Option<u64> {
    assert_eq!(
        before.index.len(),
        FIXTURE_KEYS.len(),
        "the fixture must be exactly one record per key",
    );
    FIXTURE_KEYS
        .iter()
        .position(|candidate| *candidate == key)
        .map(|position| before.index[position].1)
}

/// Property 1: if an armed key trait unwinds out of a generated keyed
/// mutation, the file is exactly as it was.
///
/// Property 2 is asserted by [`the_next_same_key_mutation_never_links_around`],
/// which must keep the *same* writer alive - a reopened writer rebuilds its
/// tails from the file and would hide a stale cache.
fn a_panicking_key_trait_leaves_no_trace(which: UserTrait, op: Op, route: Route) {
    for nth in 1..=6u32 {
        let directory = tempfile::tempdir().expect("temp dir");
        let path = directory
            .path()
            .join(format!("trace-{which:?}-{op:?}-{route:?}-{nth}.varve"));
        write_fixture(&path).expect("fixture");
        let before = snapshot(&path).expect("snapshot");
        let what = format!("{which:?}/{op:?}/{route:?} armed on call {nth}");

        let panicked = {
            let mut writer = PanicKeyFormat::open_writer(&path).expect("open writer");
            let outcome = with_armed(which, nth, || perform(&mut writer, op, route, 2, 99));
            match outcome {
                None => true,
                Some(result) => {
                    result.expect("an unarmed generated keyed mutation must succeed");
                    false
                }
            }
            // The writer is dropped here, flushing whatever it buffered. A
            // record appended before the panic becomes visible at the latest
            // now.
        };

        let after = snapshot(&path).expect("snapshot");
        if panicked {
            assert_unchanged(&before, &after, &what);
        } else {
            assert_eq!(
                after.index.len(),
                before.index.len() + 1,
                "{what}: the mutation completed, so exactly one record must have landed",
            );
            assert_eq!(
                after.index[before.index.len()].3,
                fixture_tail(&before, 2),
                "{what}: the completed mutation must link to the key's previous record",
            );
        }
    }
}

/// Property 2: whatever the armed trait did, the *next* same-key mutation on
/// the same writer links to the newest committed record for that key.
///
/// This is the defect the post-publication cache update produced: the append
/// succeeded, the cache update failed, and the surviving stale predecessor made
/// the following mutation point past the committed record, silently truncating
/// the physical keyed chain.
fn the_next_same_key_mutation_never_links_around(which: UserTrait, op: Op, route: Route) {
    for nth in 1..=6u32 {
        let directory = tempfile::tempdir().expect("temp dir");
        let path = directory
            .path()
            .join(format!("chain-{which:?}-{op:?}-{route:?}-{nth}.varve"));
        write_fixture(&path).expect("fixture");
        let before = snapshot(&path).expect("snapshot");
        let what = format!("{which:?}/{op:?}/{route:?} armed on call {nth}");

        let mut writer = PanicKeyFormat::open_writer(&path).expect("open writer");
        let outcome = with_armed(which, nth, || perform(&mut writer, op, route, 2, 99));
        let armed_info =
            outcome.map(|result| result.expect("an unarmed generated keyed mutation must succeed"));

        // The same writer, no longer armed, mutates the same key again.
        let follow = perform(&mut writer, Op::Push, route, 2, 123).expect("follow-up push");
        writer.flush().expect("flush");
        drop(writer);

        let expected = armed_info
            .map(|info| info.record_offset)
            .or_else(|| fixture_tail(&before, 2));
        assert_eq!(
            follow.prev_same_key_offset, expected,
            "{what}: the next same-key mutation linked around the committed event",
        );

        let after = snapshot(&path).expect("snapshot");
        let expected_records = before.index.len() + usize::from(armed_info.is_some()) + 1;
        assert_eq!(
            after.index.len(),
            expected_records,
            "{what}: exactly the mutations that succeeded may exist",
        );
        assert_eq!(
            after.index.last().expect("a record").3,
            expected,
            "{what}: the persisted link must match the reported one",
        );
    }
}

macro_rules! atomicity_tests {
    ($($name:ident: $which:ident, $op:ident, $route:ident;)*) => {
        $(
            mod $name {
                use super::*;

                #[test]
                fn leaves_no_trace() {
                    a_panicking_key_trait_leaves_no_trace(
                        UserTrait::$which,
                        Op::$op,
                        Route::$route,
                    );
                }

                #[test]
                fn never_links_around() {
                    the_next_same_key_mutation_never_links_around(
                        UserTrait::$which,
                        Op::$op,
                        Route::$route,
                    );
                }
            }
        )*
    };
}

atomicity_tests! {
    clone_push_inherent: Clone, Push, Inherent;
    clone_push_trait: Clone, Push, Trait;
    clone_delete_inherent: Clone, Delete, Inherent;
    clone_delete_trait: Clone, Delete, Trait;
    hash_push_inherent: Hash, Push, Inherent;
    hash_push_trait: Hash, Push, Trait;
    hash_delete_inherent: Hash, Delete, Inherent;
    hash_delete_trait: Hash, Delete, Trait;
    eq_push_inherent: Eq, Push, Inherent;
    eq_push_trait: Eq, Push, Trait;
    eq_delete_inherent: Eq, Delete, Inherent;
    eq_delete_trait: Eq, Delete, Trait;
}

/// The structural assertion behind all of the above: a generated keyed
/// mutation invokes **no** `Hash` and **no** `Eq` on the caller's key type, and
/// clones it only where the format's own `VarveKeyedBlock::key` accessor does -
/// before anything is written.
///
/// Counting the calls is what distinguishes the structural fix from a reorder:
/// a design that merely moved the user-code call earlier would still report a
/// nonzero count here, and the next reviewer would have to re-derive by hand
/// whether the surviving call sites are all pre-publication.
#[test]
fn a_generated_keyed_mutation_runs_no_user_hash_or_eq() -> varve::Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("no-user-traits.varve");
    write_fixture(&path)?;

    let mut writer = PanicKeyFormat::open_writer(&path)?;
    for (label, op, route) in [
        ("push/inherent", Op::Push, Route::Inherent),
        ("push/trait", Op::Push, Route::Trait),
        ("delete/inherent", Op::Delete, Route::Inherent),
        ("delete/trait", Op::Delete, Route::Trait),
    ] {
        // A known key, then a key this writer has never seen: the second is
        // the one that has to grow the tail map.
        for key in [2u64, 40 + u64::from(matches!(op, Op::Push))] {
            reset_calls();
            perform(&mut writer, op, route, key, 7)?;
            let [clones, hashes, eqs] = calls();
            assert_eq!(
                hashes, 0,
                "{label} on key {key} hashed the caller's key type {hashes} times; the \
                 tail cache is keyed by canonical key bytes, so no user `Hash` may run",
            );
            assert_eq!(
                eqs, 0,
                "{label} on key {key} compared the caller's key type {eqs} times; the \
                 tail cache is keyed by canonical key bytes, so no user `Eq` may run",
            );
            let allowed_clones = u32::from(matches!(op, Op::Push));
            assert_eq!(
                clones, allowed_clones,
                "{label} on key {key} cloned the caller's key type {clones} times; only \
                 `VarveKeyedBlock::key` may clone, and only on the push path",
            );
        }
    }
    writer.flush()?;
    Ok(())
}

/// The same writer must still work normally once the armed trait stops
/// panicking: a failed mutation leaves no half-applied reservation and no
/// poisoned state.
#[test]
fn the_writer_still_deletes_after_a_key_trait_panic() -> varve::Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("panicking-clone-recovery.varve");

    let mut writer = PanicKeyFormat::create_writer(&path)?;
    for id in 1..=2u64 {
        writer.push_tracked(&Tracked {
            id: PanicKey(id),
            value: id as u32,
        })?;
    }
    // Armed on the first invocation of every trait in turn; whichever fires
    // first, the writer must survive it.
    for which in [UserTrait::Clone, UserTrait::Hash, UserTrait::Eq] {
        let _ = with_armed(which, 1, || writer.delete_tracked(&PanicKey(1)));
    }

    writer.delete_tracked(&PanicKey(1))?;
    writer.flush()?;
    drop(writer);

    let after = snapshot(&path)?;
    assert_eq!(
        after.visible,
        vec![(2, 2)],
        "the retried delete must have removed key 1 and left key 2 alone",
    );
    Ok(())
}
