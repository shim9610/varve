# Petabyte I/O Final Specification

## Scope

The feature-gated stream/indexed API is Varve's petabyte-capable path. Clean
open, append, and point lookup are independent of total native bytes, record
count, and key cardinality. Existing resident APIs retain explicit
scan/materialization semantics and are not petabyte-safe.

Native record framing does not change in this slice. `redb` is pinned to the
validated 4.1 release/database behavior while the feature is experimental.

## Hard Invariants

1. Clean scalable open constructs no native record scanner and reads no record
   region.
2. Append and sync never read or decode newly appended native bytes.
3. Ordinary open never scans, verifies, bootstraps, rebuilds, or truncates.
4. Memory is bounded by declared block count plus runtime cache, batch, record,
   and key sizes.
5. Cache/batch/read policies never impose total file, total record, or append
   ceilings. Only `u64` representation/sequence exhaustion ends append.
6. Disk key-table access is limited to a validated generated disk-index plan.
7. Untrusted offsets and lengths cross a checked snapshot boundary before I/O
   or allocation.

## Persistent State

Every scalable writer uses a redb state sidecar. Stream-only state uses `.vks`;
disk-key state uses `.vki`. The state sidecar is required for O(1) writer
resume, while disk key indexing remains optional.

Metadata contains version/CRC, native identity, schema digest, disk-plan digest,
clean/dirty state, generation, committed and working EOF/count/sequence,
savepoint id/base EOF, and bounded per-declared/internal-block tails.

Exactly one persistent savepoint may exist for a dirty generation. Unexpected
savepoints or inconsistent metadata are typed corruption errors.

## Redb Transaction Rules

Every mutating transaction calls `set_quick_repair(true)` before commit. This
enables redb's two-phase commit and avoids full database repair on ordinary
reopen. Every bounded append batch commits with `Durability::Immediate`; no
unbounded chain of non-durable changes is permitted.

Begin-generation transaction order is fixed:

1. begin write transaction (default `Immediate`);
2. call `set_quick_repair(true)`;
3. before opening any user table, assert no savepoint exists and create one
   persistent savepoint of the clean root;
4. open metadata, record savepoint/base state, mark dirty;
5. commit.

Batch transaction order is quick repair, open key/state tables, apply a bounded
number/byte size of already-derived updates, write metadata/tails once, commit
`Immediate`.

Clean-publication transaction order is quick repair, validate dirty working
state, open/update metadata and tails, delete the one persistent savepoint,
mark clean/increment generation, then commit `Immediate`.

## Crash Ordering

Append starts only after the durable dirty/savepoint transaction. Each native
chunk is written once, then its derived bounded sidecar batch is committed.

`sync` performs:

1. finish the current sidecar batch;
2. flush native output and call `sync_all`;
3. publish clean state and delete the savepoint in one `Immediate` quick-repair
   transaction.

A crash before step 3 leaves dirty state and a clean savepoint. A crash after
step 3 exposes the new clean generation whose native bytes were already synced.

Explicit restore performs:

1. acquire the native writer lock and read dirty metadata;
2. begin an `Immediate` quick-repair redb transaction;
3. fetch and stage `restore_savepoint` before opening user tables;
4. read the staged old root and validate identity, schema/plan digest, clean
   state, generation, base EOF, sequence, and tails;
5. abort without native mutation on any failure;
6. while retaining the validated uncommitted restore transaction, reject native
   length below base EOF, truncate to base EOF, and `sync_all`;
7. delete the savepoint and commit the staged restore transaction.

An interrupted restore remains dirty or leaves an uncommitted physical tail;
repeating it is safe. Ordinary readers pin committed EOF and may ignore a
longer physical tail. Clean writer open requires exact physical/committed EOF.

## Typed I/O Boundary

The scalable internals define private-field `FileOffset`, `ByteLength`,
`SnapshotBounds`, `RecordSpan`, `UntrustedRecordPointer`, and
`ValidatedRecordPointer` types.

The only sidecar point-read route is:

`decoded sidecar value -> UntrustedRecordPointer -> snapshot.validate(...) ->
ValidatedRecordPointer -> positional header/payload read`.

Construction rejects arithmetic overflow, past-EOF ranges, impossible framing,
undersized records, and unsupported `usize` conversions. Snapshot length growth
is fallible. Before payload allocation, point reads match physical extent,
block id/version, sequence, flags/kind, footer extent, and declared checksum.
After bounded decode they match the expected key.

These checks provide memory and I/O safety. They do not authenticate bytes or
prove that a missing key never existed. CRC remains declaration-driven; keyed
authenticity is a separate future policy.

## Disk Index Plan

The macro generates a canonical `DiskIndexPlan` containing block id/version,
key codec identity, descriptor function, and deterministic digest for every
`key_index = disk` block. Core create/open/rebuild validates the plan against
`FormatSpec` and sidecar metadata.

When global `keyed_offset_chain` is enabled, scalable keyed mutation is exposed
only for disk-indexed keyed blocks. Non-disk keyed mutation is rejected before
native write. Previous-key B-tree lookup occurs only in this chain-enabled
case. Latest-key insertion occurs only for disk-plan blocks. Unindexed blocks
touch state/tails but never key tables.

The scalable feature must not infer manual keyedness from an overridable
default. Every manual `VarveBlock` declares `IS_KEYED`; omission is a compile
error, and generated implementations emit the exact declaration fact.

## Limits

Normal scalable operations enforce per-record stored/logical lengths, decode
materialization, key size, cache size, and transient batch bounds. They do not
check total file length, total record count, or scan bytes. File offsets and
record sequences still use checked `u64` arithmetic.

The total `.vks`/`.vki` file length is not a materialization limit and cannot
be used as a lifetime ceiling. redb accesses that file by bounded pages/cache;
`max_sidecar_len` remains applicable to sidecar formats materialized as owned
bytes elsewhere in Varve. Bounded encoders stop accepting output as soon as a
logical payload or canonical disk key crosses its runtime limit.

Explicit bootstrap/rebuild/verify operations use runtime `ResourceLimits`
carried by their stream/index options for scan-byte and record budgets.
Progress callbacks and cooperative cancellation for explicit scans are defined
by `scalable-stabilization-final-spec.md`; ordinary open and append remain
scan-free.

## Batch API

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

pub struct BatchAppendError {
    pub written: BatchAppendInfo,
    pub source: Error,
}
```

Batch bounds must be nonzero. `write_calls` counts native `write_all`
invocations, not guaranteed kernel syscalls. A record larger than `max_bytes`
is written alone after per-record checks.

Low-level stream/indexed writers accept `IntoIterator` whose items implement
`Borrow<T>`. Generated writers expose typed block-specific methods. Chunk
formation is deterministic and results never contain a per-record vector.

If a later chunk fails, the error reports already written chunks and the writer
is poisoned. No failed dirty generation can become clean; explicit restore
rolls the whole generation back to its durable checkpoint.

## Open And Explicit Costly APIs

Reader open validates the fixed header and a single pinned redb read transaction
containing metadata and key rows, checks the already-open native object's
identity/physical length, then captures committed EOF. Iterators construct a
scanner only on first use. Indexed get validates only its candidate record.

Writer open locks native first, rereads clean state under that lock, requires
exact EOF, and restores count/sequence/tails directly.

Ordinary open returns typed missing/dirty/stale/identity/schema/plan/busy errors.
Costly operations are explicit:

- `restore_checkpoint_and_open`: sidecar restore and native truncate, no scan;
- `bootstrap_checkpoint`: scan a checkpointless native file;
- `rebuild_disk_index`: scan native records and rebuild key tables;
- `verify_all`: eager native validation.

Bootstrap must reject an existing state sidecar. Rebuild may replace a missing
or clean stale disk index, but must reject dirty state and require restore.
Neither scan operation may convert an uncommitted physical tail into a clean
generation.

## Creation And Concurrency

Create acquires the native writer lock, publishes header and embedded manifest,
syncs native storage, builds a clean sidecar in a sibling temporary file with
quick repair, then atomically replaces the target sidecar and syncs the parent
directory where the platform supports it. A stale sidecar can never match the
new opened-file identity.

One writer is enforced across processes. Existing readers retain pinned native
and redb snapshots. Independently opening cross-process readers while another
process owns redb for writing is not promised and returns `IndexBusy` when the
backend refuses it. No blocking retry loop is hidden in open.

## Acceptance Gates

1. Counters prove zero scanner construction/entries and zero record-region reads
   in clean open, append/sync, and savepoint restore.
2. Virtual typed snapshots at GiB/TiB/PiB lengths use identical bounded open
   operations; platform sparse tests are supplemental.
3. Process-isolated repeated/unique million-key probes enforce a bounded
   allocation/RSS slope during long unsynced operation and after sync.
4. Batch write-call count follows chunks and single/batch logical streams match
   for fixed, variable, compression, checksums, tombstones, chains, and mixed
   indexed/unindexed data.
5. Fault subprocesses cover dirty mark, every native chunk, sidecar batch commit,
   native sync, clean publication, truncate, staged restore, savepoint deletion,
   and uncertain commit.
6. Hostile sidecar/native extents, lengths, identities, sequences, blocks, keys,
   flags, footers, and checksums fail before unsafe allocation/I/O and never
   panic.
7. `.vks` operations never stat/create `.vki`; unindexed records produce zero
   key-table operations; keyed predecessor lookup is chain-only.
8. Explicit bootstrap/rebuild scans report bytes/records, obey runtime scan
   budgets, and implement the progress/cancellation contract in
   `scalable-stabilization-final-spec.md`.
9. Native golden fixtures, Windows/Unix behavior, and representational offset
   tests pass.
10. Format, workspace/all-feature tests, trybuild, property tests, clippy
    `-D warnings`, audit, deny, and release performance probes pass.

If quick repair, savepoint restore, bounded memory, or fault ordering cannot be
demonstrated, the feature remains unstable and is not published as complete.
