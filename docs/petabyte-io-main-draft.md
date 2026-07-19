# Petabyte I/O Main Draft

Status: main-agent draft for clean-context review. This document is not the
frozen implementation specification.

## Goal

Make Varve's scalable native stream and disk-indexed entrypoints usable when
the native file may contain petabytes of data and billions of records. Normal
open, append, and point lookup work must not be proportional to total native
file size or total record count.

## Hard Invariants

1. Opening a clean scalable reader or writer does not iterate native records.
   Its native reads are limited to fixed-size header/checkpoint data and the
   bounded B-tree pages needed to open a sidecar snapshot.
2. A normal append never rereads or reparses bytes written by that append.
   Native bytes and the derived index update come from one checked prepared
   record value.
3. Resident memory is `O(schema blocks + configured cache + configured batch)`.
   It is independent of native record count and key cardinality.
4. Disk B-tree work occurs only for blocks declared `key_index = disk` and only
   through indexed entrypoints. A previous-key lookup occurs only when the
   format actually enables `keyed_offset_chain`.
5. Batch limits bound transient memory and syscall aggregation only. They never
   impose a total file, segment, record-count, or append ceiling.
6. Every offset and length obtained from disk crosses a checked typed boundary
   before it can address the snapshot. Arithmetic uses checked operations and
   a record range cannot exist unless it is contained by its captured snapshot.
7. Full native iteration is permitted only through an explicitly named scan,
   verification, recovery, migration, merge, compact, or rebuild operation.
   No `open` operation silently falls back to a full scan.

Carve-out for the keyed merge/compact family. Invariant 7 permits those
operations to iterate; it does not claim they are PB-scale. `merge_keyed_files`,
`compact_keyed_files`, and `compact_keyed_file` are **resident-only** and are
explicitly outside the petabyte surface. Each opens its inputs as whole
`VarveFile` values and holds one map entry per distinct key ever seen
(tombstoned keys included), so memory is
`O(K-ever + largest resident input index + retained live values)` and nothing
spills to disk. Varve exports no bounded-memory external merge or compact; the
scalable stream and indexed writers cover bounded *ingest*, not bounded
merge/compact. Callers must size the operation with `estimate_keyed_merge`
first, or bound it with `merge_keyed_files_with_key_limit`,
`compact_keyed_files_with_key_limit`, or `compact_keyed_file_with_key_limit`,
which fail with `Error::LimitExceeded { resource: "merge distinct keys", .. }`
at the key boundary and publish no output.

## API Surface

The unstable `high-cardinality-dev` feature receives these runtime policies:

```rust
pub struct BatchOptions {
    pub max_records: usize,
    pub max_bytes: usize,
}

pub struct BatchAppendInfo {
    pub records: u64,
    pub first_sequence: Option<u64>,
    pub last_sequence: Option<u64>,
    pub start_offset: u64,
    pub end_offset: u64,
    pub write_calls: u64,
}

pub enum IndexRecoveryPolicy {
    RequireClean,
    RebuildIfNeeded,
}
```

`BatchOptions` must be nonzero and has a conservative bounded default. A single
record larger than `max_bytes` is checked against the format's per-record read
policy and written alone; it does not redefine the transient target into a
file-wide limit.

Low-level writers expose consuming iterator batch methods that return a compact
summary rather than a per-record `Vec`:

```rust
writer.push_iter::<T, _>(values, BatchOptions::default())?;
indexed_writer.push_iter::<T, _>(values, BatchOptions::default())?;
```

Generated writers expose block-specific equivalents such as
`push_frames(values, options)`. Single-record `push_*` remains immediate and
keeps its existing result semantics.

Readers expose `verify_all()` as the explicit eager integrity operation.
`events()` and typed streaming iterators validate records lazily as they cross
the iterator boundary. Indexed `get_*` validates only the selected native
record's typed range, framing, checksum, block/version, and decoded key.

`IndexRecoveryPolicy::RequireClean` is the default. A dirty, stale, absent, or
invalid sidecar returns a typed error without scanning. Explicit rebuild mode
may perform a bounded-memory full native scan and must be named in the call or
options.

## Typed Safety Boundary

The scalable implementation introduces internal newtypes rather than passing
untrusted `u64` values directly into I/O:

- `FileOffset`: a checked absolute native offset.
- `ByteLength`: a checked byte count.
- `SnapshotBounds`: the immutable `[0, captured_len)` boundary.
- `RecordSpan`: header/payload/footer ranges proven ordered, non-overflowing,
  and contained by `SnapshotBounds`.
- `ValidatedRecordPointer`: a sidecar candidate converted into a `RecordSpan`
  and matched to expected block id, sequence, and key before decoding.

Raw integer encoding remains the wire representation. Construction from wire
values is fallible; only validated types reach positional reads. Resource
limits remain runtime read/write policies and are not format-wide append caps.

## Clean Open

### Reader

`VarveStreamReader::open` validates the fixed file header, captures the file
length, and returns. It does not call `NativeStreamScanner`. Iterators create a
scanner only when iteration begins.

`VarveIndexedReader::open` additionally opens a clean sidecar snapshot and
requires exact identity, schema, generation state, and `covered_eof` agreement
with the captured native length. It does not inspect native record bodies.

### Writer

A clean indexed sidecar stores enough bounded state to resume without scanning:

- covered native EOF;
- total record count and next sequence derivation;
- schema and primary-file identity;
- generation and clean/dirty state;
- one last-record offset per declared block/internal block when block chaining
  is enabled.

Indexed writer open acquires the native writer lock, validates the sidecar
checkpoint against the locked file, and initializes writer state directly.
The state is reread after lock acquisition so a racing completed writer cannot
publish a stale checkpoint into the new handle.

Dirty/missing/stale recovery is explicit. The current full rebuild remains a
bounded-memory recovery tool, not an implicit normal-open path. Incremental
recovery from the last durable generation is a later optimization unless this
slice can add it without weakening crash semantics.

An unindexed stream writer has no key sidecar. Creating and appending through
one handle is scalable. Reopening it without a persistent checkpoint cannot be
O(1); therefore the API must either require a generic writer-state checkpoint
or return a typed `CheckpointRequired` error. It must not hide a scan.

## Append Data Flow

Single append:

1. Encode and validate one `PreparedRecord` in memory.
2. Derive its typed `RecordSpan`, sequence, block tail, canonical key, and index
   update from that same value.
3. Write the prepared bytes once to native storage.
4. Publish bounded in-memory writer state and the derived sidecar update.
5. Do not read native storage.

Batch append:

1. Prepare records into a reusable buffer until `max_records` or `max_bytes`.
2. Track only bounded per-chunk tails/keys needed for chains.
3. Write the contiguous chunk with one `write_all` call.
4. Apply its already-derived sidecar updates in the bounded redb transaction.
5. Repeat until the iterator is exhausted.

For a native write error, truncate to the pre-chunk EOF and discard staged
state. If native publication succeeds but sidecar publication fails, poison the
writer and leave the durably dirty sidecar for explicit recovery.

The redb transaction may contain a bounded number of updates, but its metadata
row is written once at transaction commit, not once per record. Transaction
commit does not imply native `fsync`.

## Durability

Before the first post-clean native append, indexed metadata is durably marked
dirty. `flush` only forwards buffered native bytes. `sync` performs:

1. native `sync_all`;
2. commit of pending derived index updates;
3. durable clean checkpoint/generation publication.

A crash before step 3 cannot produce a trusted clean sidecar. Clean open never
tries to prove the entire native file by scanning it. Authenticity against an
attacker able to rewrite both native data and sidecar requires a separately
declared MAC/signature policy; CRC and type validation provide corruption and
memory-safety defenses, not authenticity.

## Compatibility

The existing resident `VarveFile`, `BlockVec`, and `KeyedBlockVec` APIs retain
their scan/materialization semantics for compatibility and small/medium files.
They are explicitly outside the petabyte scalability contract. Scalable
generated stream/indexed entrypoints are the required API for petabyte files.
No documentation may present resident open as petabyte-safe.

The sidecar format is development-only and may be version-bumped. Native wire
records are not changed merely to optimize sidecar/open behavior.

## Tests And Acceptance Criteria

1. Test instrumentation proves clean stream/indexed reader open invokes zero
   native record scans for both small and million-record files.
2. Clean indexed writer reopen invokes zero native record scans and preserves
   sequence, block-chain, keyed-chain, tombstone, and mixed indexed/unindexed
   behavior.
3. Dirty/stale/missing sidecars fail without scanning under `RequireClean` and
   rebuild only under an explicit recovery policy.
4. Appending one million unique keys invokes zero native readback scans and has
   bounded allocator growth controlled by cache and batch settings.
5. A batch of many small records performs approximately one native write per
   configured chunk, not one per record. Tests use deterministic write-call
   instrumentation rather than wall-clock assumptions.
6. Single and batch append produce byte-equivalent logical event streams across
   fixed, variable, compressed, checksummed, tombstone, and mixed blocks.
7. Malicious sidecar offsets, integer overflows, oversized lengths, wrong
   block/sequence/key candidates, truncated records, and checksum failures
   return typed errors without panic or allocation-before-validation.
8. Release benchmarks report records/s, bytes/s, clean open latency, warm/cold
   lookup latency, native/sidecar size, write calls, and peak allocations.
9. Workspace tests, all-features tests, trybuild, clippy `-D warnings`, format,
   audit, and deny all pass.

## Non-Goals For This Slice

- distributed or multi-writer coordination;
- cryptographic authenticity without a declared key/signature policy;
- making resident collection APIs constant-memory;
- hiding the cost of explicit verification or sidecar rebuild;
- changing native record framing solely for benchmark results.
