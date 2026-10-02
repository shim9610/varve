# Immediate policy for append writers

An Immediate boundary makes all preceding writes, including its triggering
record, durable before returning success. On scalable writers it also publishes
the corresponding clean sidecar root. There are three ways to request one.

## Block declarations

```rust,ignore
varve_format! {
    pub format Journal {
        magic: b"JRNL";
        version: 1;
        schema_hash: computed;
        index: keyed_offset_chain;
        blocks {
            variable Sample(id = 1, key = [id], key_index = disk) {
                id: u64,
                bytes: Vec<u8>,
            }
            fixed Checkpoint(id = 2, durability = immediate) {
                position: u64,
            }
            fixed Status(id = 3, immediate_if = final_status) {
                complete: bool,
            }
        }
    }
}

fn final_status(status: &Status) -> bool { status.complete }
```

Each `Checkpoint` append immediately persists itself and preceding samples.
The same rule applies to deletions of an Immediate keyed block. `immediate_if`
receives an appended value; it does not run on a deletion, which has only a key.
Both declarations are OR-combined with any runtime policy; runtime settings
cannot disable a mandatory block boundary. Unannotated blocks are deferred.

For independently derived blocks, use `#[varve(durability = "immediate")]`
and `#[varve(immediate_if = "final_status")]`. Manual `VarveBlock` implementations
can override `const IMMEDIATE: bool` and `fn immediate_if(&self) -> bool`.
These are writer policies, not serialized schema or file-format changes.
Use pure predicates: a batch can evaluate a condition before its native write.

## Runtime conditions and explicit calls

```rust,ignore
writer.set_immediate_policy(
    ImmediatePolicy::new()
        .after_records(16_384)
        .after_bytes(64 * 1024 * 1024)
        .when(|event| event.block_id == 2)
)?;

writer.push_sample(&sample)?;
writer.immediate()?;
```

The conditions are OR-combined. `when` accepts a function pointer or a
non-capturing closure over `ImmediateEvent` (block id, operation, sequence,
record size, pending record count and pending native bytes). A delete event
names its user block, not the internal tombstone block. Byte accounting includes
record headers and footers. Thresholds include the triggering record; a large
record can exceed a byte threshold. Zero thresholds fail installation without
replacing the existing policy.

Counters cover appends/deletions on this handle and reset only after successful
`sync()` or `immediate()` (`commit_durable()` also resets native-writer counters).
Changing policy neither resets counters nor performs I/O. The next mutation
evaluates the new policy. `ImmediatePolicy::default()` removes runtime conditions.
There is no background timer and no I/O from a condition declaration itself.

Within `push_*s` / `push_iter`, an Immediate condition drains the current native
prefix and completes the durability boundary **before consuming the next input
value**. A declared Immediate block therefore requests a boundary per record,
even when passed through a plural/batch API. Use a separate checkpoint block or
a record/byte condition to select a larger durability interval.

The APIs are available on `VarveFile`, `VarveWriter`, `VarveStreamWriter`,
`VarveIndexedWriter`, and their generated native append writer wrappers.
Stream/indexed writers are included by default. Conditions govern typed
user appends and deletions; matrix cells, in-place replacement, metadata and
merge-op APIs retain their existing contracts. Matrix block declarations reject
these attributes: use the matrix ordered-barrier API.

On a native format with transaction markers, `immediate()` syncs bytes but does
not implicitly create a logical commit marker. Use `commit_durable()` when the
transaction itself must become committed and durable.

## Scalable commit and recovery order

1. Before the first mutation after a clean boundary, durably save the clean
   rollback checkpoint and mark the generation Dirty. This recovery marker is
   separate from confirmed heads; ordinary appends are not yet
   acknowledged as durable by it.
2. Write native chunks and append bounded sidecar working checkpoints without
   a durability barrier. A native chunk does not force an index commit; index
   record/byte bounds, API-end publication and explicit boundaries do.
3. At `immediate()` / `sync()`, finish the pending index batch, sync the native
   file, append and sync the clean checkpoint, then publish and individually sync
   both confirmed heads. The previous rollback checkpoint is retired logically.

`flush()` does not request a new durable clean generation. Creating/rebuilding
a sidecar and establishing a rollback checkpoint still perform required safety
I/O. This is not a promise of zero fsync calls before the first application
boundary. Neither Drop nor a working checkpoint is an application durability
acknowledgment.

Recovery restores the last acknowledged generation and discards its unsynced
suffix. Fresh readers open Dirty state using its saved confirmed root and EOF.
Existing readers adopt new confirmed generations with `follow()`; use
`follow_events` or `follow_blocks` to resume a parser without replaying its prefix.
Independent readers can run in separate processes. These paths add no mutex or
reader write-admission gate.

If an automatic boundary fails after append, the call returns
`Error::AppendedButImmediateFailed { sequence, source }`. Do not append the same
record again. The batch error's `written` prefix includes records already in the
native file, including the triggering record. If the writer remains usable,
retry `immediate`; a poisoned writer requires recovery. A direct `immediate()`
returns its original sync/index error because it appends no new user record.

The batch and private-cache limits bound index residency, not on-disk history.
Append-only pages grow until explicit `compact_index()` after a clean boundary.
Old open readers can retain the replaced file. Select record/byte conditions for
a finite durability interval and manage disk space explicitly; the million-key
heap gate remains a measured budget, not a proof for every workload.

## Validation

`immediate_policy` covers exact in-batch boundaries, scalar appends and deletes,
unindexed records, threshold/predicate behavior, failed-boundary prefix reporting,
pinned readers, and subprocess exits without running writer destructors.
`immediate_native` exercises the public native wrappers without scalable features.
The existing crash matrix, reader-open churn, seeded histories and memory gate
remain required by `scripts/qualify_scalable.py`.
