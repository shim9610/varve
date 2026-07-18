# Petabyte I/O Spec v2

Status: consolidated implementation candidate. Pending clean-context validation.

## Scope

The `high-cardinality-dev` stream and indexed entrypoints become Varve's
petabyte-capable path. Their cost model is independent of total native bytes,
record count, and key cardinality during normal clean open, append, and point
lookup. Resident collection APIs remain compatible but explicitly retain
scan/materialization semantics.

No native record framing changes are permitted in this slice.

## Non-Negotiable Invariants

1. Clean scalable open invokes no native record scanner and reads no record
   region.
2. Append publication never reads or decodes newly appended native bytes.
3. Normal open never bootstraps, rebuilds, verifies, or recovers implicitly.
4. Memory is bounded by declared block count and runtime cache/batch/record/key
   sizes, never total records or keys.
5. Runtime cache and batch bounds never become total append/file limits.
6. Disk key-table access occurs only for `key_index = disk`; previous-key lookup
   occurs only when `keyed_offset_chain` is enabled.
7. Raw disk offsets and lengths become checked snapshot-contained types before
   positional I/O or allocation.

## Scalable Checkpoint

Every scalable handle uses a redb state sidecar. Stream-only files use `.vks`;
disk-indexed files use `.vki` with the same state tables plus key tables. A
state sidecar is required for O(1) writer resume and is distinct from optional
disk key indexing.

Committed metadata stores:

- sidecar version and CRC;
- opened native-file identity and schema/indexed-block digests;
- clean/dirty state and generation;
- committed and working EOF;
- committed and working record count;
- committed and working last-sequence/next-sequence state;
- persistent savepoint id and immutable base EOF while dirty;
- one tail per declared/internal block when block chaining is enabled.

The tail table is `O(declared blocks)`. Metadata and tails are updated once per
bounded sidecar transaction, not once per record.

## Crash Protocol

### Begin generation

After acquiring the native writer lock and revalidating identity, an
`Immediate` redb transaction creates a persistent savepoint of the clean root,
records the savepoint id/base EOF, and marks the working root dirty. It commits
before any native record write.

### Append

One checked prepared record or bounded prepared chunk is written once. Its
already-derived key/tail/coverage changes enter a bounded redb transaction.
Transactions may commit with `Durability::None`; redb specifies that a later
`Immediate` commit persists them. The persistent savepoint keeps the old clean
root recoverable. No native readback occurs.

### Sync

1. Commit the pending bounded sidecar transaction.
2. Flush native output and call native `sync_all`.
3. In one `Immediate` sidecar transaction, publish working EOF/count/sequence
   and tails, increment generation, mark clean, and delete the savepoint.

The final sidecar transaction publishes only after native durability. A crash
therefore exposes either dirty state plus a recoverable clean root or the new
clean root.

### Explicit restore

`restore_checkpoint_and_open` validates dirty metadata and rejects native
length below base EOF. It truncates native storage to base EOF and syncs that
truncation before restoring/deleting the redb savepoint in an `Immediate`
transaction. It reads no native record region. Interrupted restore is
idempotent.

Reader open may ignore a physical tail beyond committed EOF and pins that
logical EOF. Clean writer open requires physical EOF equal to committed EOF;
tail truncation is explicit restore behavior.

## Typed Boundary

The scalable core adds:

```rust
pub struct FileOffset(u64);
pub struct ByteLength(u64);
pub struct SnapshotBounds { logical_len: ByteLength }
pub struct RecordSpan { /* ordered header/payload/footer extents */ }
pub struct ValidatedRecordPointer { /* sidecar expectation + RecordSpan */ }
```

Fallible constructors reject addition overflow, impossible ordering, past-EOF
ranges, undersized framing, and unsupported `usize` conversions. Sidecar
entries include offset, physical length, sequence, kind, expected block, and
entry CRC. Point lookup checks the native header, sequence, block/version,
extent, declared checksum policy, and decoded key before returning a value.

CRC remains declaration-driven and is not confused with memory safety or
authentication. A future keyed MAC/signature policy is outside this slice.

## Public API

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
```

Both fields are nonzero runtime transient bounds. A record larger than the byte
target is written alone after ordinary per-record policy checks.

Low-level stream and indexed writers expose consuming-iterator batch append
accepting owned or borrowed values through `Borrow<T>`. Generated writers
expose typed block-specific batch methods. Results are compact summaries, not
per-record vectors.

Explicit costly operations are named:

- `restore_checkpoint_and_open`: sidecar restore plus native truncate, no scan;
- `bootstrap_checkpoint`: bounded scan for checkpointless native files;
- `rebuild_disk_index`: bounded native scan rebuilding key tables;
- `verify_all`: eager native integrity scan.

Missing, dirty, stale, swapped, or schema-mismatched state returns typed errors
from ordinary open. No `RebuildIfNeeded` open policy exists.

## Append Implementation

`PreparedStreamRecord` becomes the sole source for contiguous bytes, checked
span, sequence, offsets, tails, checksum, canonical key, and index update
inputs. Single append writes one contiguous record. Batch append reuses one
bounded byte buffer and performs one native `write_all` per chunk.

The current `validate_batch` native rescan is removed. Sidecar metadata writes
move from per-record update to transaction commit. Disk lookup before append is
skipped unless a keyed offset chain needs the predecessor. Stream/unindexed
records never open the latest-key table.

## Open Implementation

Scalable reader open validates the fixed native header, opens the matching
clean sidecar checkpoint, verifies physical length is at least committed EOF,
and captures `SnapshotFile` at committed EOF. Iterators instantiate scanners
only when first advanced.

Scalable writer open acquires the native lock, rereads the clean checkpoint,
requires exact physical/committed EOF, and initializes sequence/count/tails
directly. It does not instantiate `NativeStreamScanner`.

## Deterministic Gates

1. Test counters prove zero scanner entries and zero record-region reads for
   clean reader/writer open, append publication, and savepoint restore.
2. Sparse GiB/TiB/PiB logical files have identical bounded clean-open reads.
3. One million unique keys retain bounded allocation and zero resident key map.
4. Batch append write-call count follows configured chunks, not record count.
5. Single/batch event streams match across fixed, variable, compressed,
   checksummed, tombstone, block/key chains, and mixed indexed/unindexed data.
6. Fault injection covers dirty mark, native write/sync, bounded sidecar commit,
   clean publication, truncate, savepoint restore, and savepoint deletion.
7. Hostile offsets, lengths, identities, sequences, blocks, keys, and checksums
   fail before allocation or out-of-snapshot I/O and never panic.
8. Stream-only operations never create or stat a `.vki` path; unindexed records
   never touch the key table.
9. Explicit bootstrap/rebuild/verify reports bytes and records scanned and uses
   bounded memory.
10. Workspace/all-feature tests, trybuild, property tests, formatting, clippy
    with warnings denied, audit, deny, and release performance probes pass.

## Rejected Choices

- Native dual checkpoint slots: unnecessary wire-format change for this slice.
- Optional state sidecar for scalable writer reopen: O(1) resume is impossible
  without persistent state.
- Hidden rebuild/verify/recovery on ordinary open.
- Restoring the sidecar before truncating native tail.
- Per-record metadata commits, resident key maps, native readback validation,
  and unbounded batch result vectors.
- Mandatory CRC or cryptographic authentication solely for scalability.
- Claiming resident collection APIs are petabyte-safe.
