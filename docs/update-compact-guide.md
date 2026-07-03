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

## Direct Replacement

`replace_fixed` and `replace(index, block, ReplaceStrategy::FixedInPlace)` are
for fixed blocks whose canonical encoded payload size is unchanged.

`replace_rewrite` and `ReplaceStrategy::RewriteFile` rewrite the file through a
temporary file and atomically publish it. Prefer append plus compact for routine
updates because replacement gives up the append-friendly history model.

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
