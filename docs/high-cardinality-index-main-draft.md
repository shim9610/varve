# High-Cardinality Streaming Index: Main Draft

Status: main-agent draft for consolidation. This is not a frozen public
contract.

## Goal

Provide an opt-in native append/read path whose resident memory does not grow
with record count or unique-key count, and provide latest-value keyed lookup
without a process-resident `HashMap<Key, Offset>`.

Existing v0.3 formats and generated APIs remain source- and wire-compatible.

## Non-goals

- Changing existing `keyed_blocks()` semantics or silently making it slower.
- Claiming worst-case constant-time lookup for an adversarial hash table.
- Making the rebuildable index sidecar authoritative for user data.
- Supporting replacement, merge, compaction, mmap, checkpoints, and transaction
  markers through the first streaming handle unless their bounded-state
  invariants are explicit.

## API and declaration

Inline keyed blocks gain an operational key-index declaration:

```rust
block Frame variable key(scan, frame) key_index disk_hash {
    // fields
}
```

Choices:

- `memory` (default): current generated `HashMap` and `KeyedBlockVec` behavior.
- `scan`: no generated key-tail map; lookup is an explicit bounded-memory scan.
- `disk_hash`: generated writer and reader use a rebuildable per-block sidecar
  with a bounded cache.

Formats gain explicit generated streaming constructors:

```rust
let writer = CaptureFormat::create_streaming(path)?;
let reader = CaptureFormat::open_streaming(path)?;
```

The ordinary `create/open` APIs continue retaining the complete native record
index. Streaming handles expose append, sequential typed iteration, physical
event iteration, flush/sync, and supported keyed lookup. APIs requiring random
record ordinals return a typed `IndexUnavailable` error or are absent from the
generated streaming type.

## Streaming native state

The scanner is refactored into a cursor over `SnapshotFile`. It validates one
fixed record header, checked payload/footer extent, integrity, sequence, and
commit state at a time. It yields one `RecordIndexEntry` and then discards it.

Streaming writer resident state is limited to:

- current sequence and append offset,
- one latest offset per declared block when block chains are enabled,
- bounded pending transaction state where supported,
- fixed-size I/O buffers,
- bounded on-disk-index cache.

It never owns `Vec<RecordIndexEntry>` or a per-key map.

## Disk hash sidecar

Each `disk_hash` block uses `<data-path>.<block-id>.vki`. The primary Varve file
is authoritative; deleting or corrupting a sidecar only causes a bounded-memory
rebuild.

The sidecar contains:

- magic/version, schema hash, block id/version,
- random hash seed, table capacity, occupied count,
- captured primary generation fingerprint and validated primary length,
- dirty/clean generation marker,
- fixed-size open-addressed slots containing keyed hash, latest record offset,
  sequence, and state.

Key bytes are not duplicated. Hash collisions are resolved by reading the
candidate primary record at the stored offset and comparing its decoded key.
Probe count is bounded by table capacity and policy. A keyed hash and load
factor below 0.70 provide expected constant-time lookup; the API documentation
must not claim adversarial worst-case O(1).

The table is file-backed. Lookup reads slots positionally and keeps only a
bounded page cache. Growth writes a doubled temporary table by streaming old
slots, syncs it, and atomically replaces the sidecar. No operation allocates a
buffer proportional to capacity.

## Publication and recovery

Before a sidecar mutation its header is marked dirty. Primary append is
published first, then the sidecar slot and captured primary length are updated,
then the sidecar is marked clean. A failure after primary publication poisons
the streaming writer and returns a distinct published-but-index-stale error.

Open validates sidecar identity, clean state, exact size arithmetic, load
factor, slot offsets, and every returned primary offset. Any mismatch rebuilds
from a bounded-memory primary scan into a temporary sidecar, then atomically
publishes it.

Offset-changing primary rewrites invalidate and rebuild all affected sidecars.

## Compatibility and unsupported combinations

- `memory` remains the default and existing wire bytes are unchanged.
- `key_index` is an operational generated-API policy and is not added to the
  primary schema hash. Sidecar headers bind to the actual schema hash.
- Streaming mode initially rejects checkpoint-on-flush and APIs that require a
  stable ordinal index.
- Transaction-marker streaming support is accepted only if pending state is
  bounded by an explicit runtime byte/record policy.
- `disk_hash` supports keyed puts and tombstones. User-defined merge ops remain
  scan/materialize-only until an incremental-op contract is defined.

## Acceptance criteria

1. Appending and reopening at least one million unique keys shows resident
   bookkeeping bounded independently of key count; tests use exposed resident
   state metrics rather than flaky process RSS alone.
2. `disk_hash.get(key)` returns the latest put, observes tombstones, survives
   reopen, and handles deliberate hash collisions.
3. Sidecar loss, truncation, dirty marker, stale primary length, and hostile
   slot offsets rebuild or return typed errors without panic or claim-sized
   allocation.
4. Interrupted primary/sidecar publication never hides primary records after
   reopen.
5. Streaming iteration yields the same committed records and corruption errors
   as ordinary open for supported policies.
6. Existing default and all-feature tests remain unchanged and pass.
7. Performance tests report append throughput, streaming scan throughput,
   lookup latency, rebuild throughput, and stable resident bookkeeping.

