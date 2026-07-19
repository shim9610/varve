# Update And Compact Guide

Varve is optimized for append-friendly updates. Prefer put, op, tombstone, and
compact workflows for keyed data. Use direct replacement only when the caller
intentionally wants to rewrite existing records.

## Keyed Updates

A keyed block declares `key = "..."` and implements `VarveMerge` when it accepts
ops.

```rust
use varve::{VarveBlock, VarveMerge};

#[derive(Clone, Debug, PartialEq, VarveBlock)]
#[varve(id = 101, version = 1, kind = "variable", key = "id")]
struct User {
    #[varve(field_id = 1)]
    id: u64,
    #[varve(field_id = 2)]
    name: String,
}

#[derive(Clone, Debug, PartialEq, VarveBlock)]
#[varve(id = 102, version = 1, kind = "variable")]
struct RenameUser {
    #[varve(field_id = 1)]
    name: String,
}

impl VarveMerge for User {
    type Op = RenameUser;

    fn apply_op(&mut self, op: Self::Op) -> varve::Result<()> {
        self.name = op.name;
        Ok(())
    }
}
```

Append updates:

```rust
let mut file = AppFormat::open("users.varve")?;
file.push(&User { id: 1, name: "old".to_string() })?; // put
file.push_op::<User>(&1, &RenameUser { name: "new".to_string() })?;
file.delete::<User>(&2)?; // tombstone
file.flush()?;
```

Read modes:

- `keyed_blocks::<T>()` gives latest-put keyed lookup after tombstones.
- `materialized_keyed_blocks::<T>()` applies same-file puts, ops, and
  tombstones into final keyed state.

## Merge Ordered Shards

Use base plus ordered delta shards when a workflow naturally writes append-only
deltas.

```rust
use varve::merge_keyed_files;

merge_keyed_files::<User, _>(
    AppFormat::spec(),
    "base.varve",
    &["delta-1.varve", "delta-2.varve"],
    "merged.varve",
)?;
```

Conflict order is shard ordinal, then local sequence, then record ordinal. Later
delta shards win over earlier deltas and base records.

## Compact Final State

Compact drops tombstones and ops and writes only final keyed values for one
keyed block type.

```rust
use varve::{compact_keyed_file, compact_keyed_files};

compact_keyed_file::<User, _>(
    AppFormat::spec(),
    "users.varve",
    "users.compact.varve",
)?;

compact_keyed_files::<User, _>(
    AppFormat::spec(),
    "base.varve",
    &["delta-1.varve", "delta-2.varve"],
    "users.final.varve",
)?;
```

The direct base+delta compact path avoids writing a merged intermediate file.
The first compact API is scoped to one keyed block type per call; non-keyed
records and unrelated block ids are not copied.

Merge and compact outputs are published through same-directory temp files,
flush/sync, and atomic replacement.

## Merge And Compact Are Resident, Not PB-Scale

`merge_keyed_files`, `compact_keyed_file`, and `compact_keyed_files` open their
inputs as whole `VarveFile` values and hold one map entry per distinct key ever
seen - including keys whose latest record is a tombstone - plus the live values
that reach the output:

- time: `Theta(records + decoded bytes) + O(K-live log K-live)`
- memory: `O(K-ever + largest resident input index + retained live values)`

Nothing spills to disk, so `K-ever` must fit in memory. Varve exports no
bounded-memory external merge or compact; the scalable stream and indexed
writers bound *ingest*, not merge/compact. Do not call this family on a file
whose key cardinality you cannot afford to hold resident.

Size the run first:

```rust
use varve::estimate_keyed_merge;

let estimate = estimate_keyed_merge::<User, _>(
    AppFormat::spec(),
    "base.varve",
    &["delta-1.varve"],
)?;
// estimate.max_distinct_keys is an upper bound on K-ever;
// estimate.max_state_bytes is the resident state that bound implies.
```

`estimate_keyed_merge` decodes no values, but it does open each input as a
resident file, so it costs `O(largest input index)` itself. It reports that
number as `largest_input_index_bytes`; it is not a way to size an input you
cannot already afford to open. Pass an empty delta slice to size a single-input
`compact_keyed_file`.

Or bound the run and fail typed instead of exhausting memory:

```rust
use varve::{
    compact_keyed_file_with_key_limit, compact_keyed_files_with_key_limit,
    merge_keyed_files_with_key_limit,
};

merge_keyed_files_with_key_limit::<User, _>(
    AppFormat::spec(),
    "base.varve",
    &["delta-1.varve"],
    "merged.varve",
    1_000_000,
)?;

compact_keyed_file_with_key_limit::<User, _>(
    AppFormat::spec(),
    "users.varve",
    "users.compact.varve",
    1_000_000,
)?;
```

The ceiling is checked before each new key is admitted, so exceeding it yields
`Error::LimitExceeded { resource: "merge distinct keys", .. }` at the key
boundary. A refused run publishes no output file. The ceiling counts tombstoned
keys, matching what the state actually retains. The unbounded entry points
delegate with `u64::MAX`, so their behaviour is unchanged.

## Direct Replacement

`replace_fixed` and
`replace(index, block, ReplaceStrategy::FixedCopyOnWrite)` are the safe fixed
replacement paths. The canonical payload size must be unchanged. Varve copies
the current opened generation to a same-directory temporary file, patches the
record header/payload and checksum, validates the complete new generation,
syncs it, and atomically publishes it. Readers opened before publication keep
their original file object and value; new readers observe the replacement.

`unsafe replace_fixed_in_place_exclusive` retains the lower-copy expert path.
The caller must exclude every reader, writer, mmap, raw reference, handle,
thread, and process for the operation and for every affected view's lifetime.
It is deliberately not represented as a safe `ReplaceStrategy` variant.

`replace_rewrite` and `ReplaceStrategy::RewriteFile` rewrite the file through a
temporary file and atomically publish it. Prefer append plus compact for routine
updates because replacement gives up the append-friendly history model.

An error before atomic publication leaves the old pathname generation in
place. `PublishedButRebindFailed`, however, explicitly means publication
succeeded and only the writer's post-publication reopen/rebind failed. That
writer is poisoned. Drop it, reopen the pathname, and reconcile the published
sequence before issuing another logical update; a blind retry can apply the
operation twice.

## Performance Check

Run the ignored smoke tests when update, merge, compact, replacement, recovery,
or keyed materialization behavior changes:

```powershell
cargo test -p varve --test perf_smoke -- --ignored --nocapture
cargo test -p varve --features mmap --test perf_smoke -- --ignored --nocapture
cargo test -p varve --features mmap,zero-copy --test perf_smoke -- --ignored --nocapture
```

Watch large cases for accidental O(n^2) scans, repeated allocation, slow open
paths, and direct compact becoming slower than merge-then-compact without a
clear reason.
