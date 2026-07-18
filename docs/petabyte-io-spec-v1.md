# Petabyte I/O Spec v1

Status: synthesis of the main-agent and clean-context drafts. Pending one
clean-context consolidation pass.

## Contract

This slice makes the feature-gated stream/indexed API petabyte-capable. Legacy
resident APIs keep their explicit scan/materialization behavior and are not
advertised as scalable.

Normal scalable operations obey these invariants:

1. Clean open performs zero native record scans and no hidden rebuild.
2. Append never reads back bytes it just wrote.
3. Memory does not grow with record count or key cardinality.
4. Total file size is never constrained by a batch/cache setting.
5. Disk B-tree work occurs only for `key_index = disk` indexed handles.
6. Untrusted integer offsets/lengths cannot reach I/O before checked typed
   snapshot containment.
7. Native iteration occurs only in iteration itself or explicitly named
   verify/rebuild/bootstrap/recovery operations.

## Checkpoint Choice

The clean draft's dual native-header slots would provide a self-contained
checkpoint but require a native wire-format change. This slice instead uses a
required scalable-state sidecar and redb persistent savepoints. This preserves
the stable native record framing while retaining the previously committed
B-tree root across bounded append transactions.

The state sidecar is not the optional disk key index. Stream-only scalable
writer reopen may use a small state-only sidecar; `key_index = disk` adds key
tables to the same checkpoint mechanism. Losing the state sidecar requires an
explicit native bootstrap scan.

The committed state contains:

- schema and opened-file identity;
- generation and committed EOF;
- record count and next-sequence/exhaustion state;
- commit-policy state;
- last manifest/tombstone offsets;
- one block-tail offset per declared block when block chaining is enabled;
- key-index schema digest when disk key indexing is enabled;
- persistent savepoint id while an append generation is dirty.

The table is bounded by declared block count.

## Crash Model

Before the first append after a clean checkpoint, an immediate redb transaction
creates a persistent savepoint of the clean root and marks metadata dirty while
preserving the committed EOF/generation.

Append batches may commit derived latest-key pages with non-durable redb
transactions. The persistent savepoint pins the previous clean root. Metadata
and tail rows are written once per sidecar batch, never per record.

`sync` performs:

1. flush and `sync_all` native bytes;
2. commit any pending bounded sidecar batch;
3. in one immediate sidecar transaction, publish the new EOF/sequence/tails,
   increment generation, mark clean, and delete the old persistent savepoint.

A crash before step 3 leaves a dirty root with a durable clean savepoint.
Writer recovery restores that savepoint and truncates native bytes directly to
the committed EOF without parsing them. A crash after step 3 exposes the new
clean root. Native length shorter than committed EOF is corruption. Extra
native bytes beyond committed EOF are uncommitted and ignored by readers.

The implementation must verify redb's savepoint and durability behavior with
fault tests. If a redb operation cannot provide the required atomic boundary,
the feature remains development-only rather than weakening this contract.

## API

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

pub enum RecoveryPolicy {
    RequireCheckpoint,
    RestoreCheckpoint,
    RebuildExplicitly,
}
```

Batch options are runtime transient bounds. Values must be nonzero. A record
larger than the byte target is checked using the format's per-record policy and
written alone.

Low-level stream/indexed writers expose consuming-iterator batch append and
return a compact summary. Generated writers expose block-specific typed batch
methods. No batch method returns an unbounded per-record vector.

Readers expose `verify_all` for eager validation. Stream iterators validate as
they advance. Indexed gets validate only their selected record.

Ordinary scalable open requires a valid checkpoint and never rebuilds. Explicit
bootstrap/rebuild APIs may scan with bounded memory and report progress.

## Typed I/O Boundary

Internal `FileOffset`, `ByteLength`, `SnapshotBounds`, `RecordSpan`, and
`ValidatedRecordPointer` types make construction from wire values fallible.
They prove checked arithmetic, ordered header/payload/footer ranges, and
containment in the captured logical EOF before positional reads or allocation.

Disk entries include enough data to validate the selected native candidate:
record offset, physical length, sequence, kind, expected block, and entry CRC.
The native header is then matched against those values and the decoded key is
matched before return. Native record CRC is verified on access when the format
declares an integrity policy. CRC is not forced merely for memory safety;
authentication remains a separate future MAC/signature policy.

## Append Path

`PreparedRecord` is the single source for bytes, checked span, sequence, tails,
checksum, canonical key, and sidecar update.

Single append writes one contiguous record. Batch append reuses a bounded byte
buffer, prepares until a byte/record target, writes one contiguous chunk, then
applies already-derived sidecar updates. There is no native scanner or payload
decode in append publication.

Previous-key B-tree lookup is skipped unless `keyed_offset_chain` is enabled.
Latest-key insertion remains required only for disk-indexed blocks. Unindexed
blocks advance only bounded checkpoint/tail state.

## Acceptance

- Clean stream/indexed reader and indexed writer open invoke zero scanner
  entries for small and million-record files.
- Dirty recovery restores a persistent savepoint and truncates to committed EOF
  without native reads from the record region.
- Missing/stale state fails without scanning; explicit rebuild is the only full
  native index reconstruction.
- One million unique keys retain bounded allocator growth and zero resident key
  entries.
- Batch append uses approximately one native write per configured chunk and
  produces the same event stream as single append.
- Append and sync instrumentation observes no reads overlapping newly appended
  native extents.
- Fault injection around dirty mark, native write/sync, sidecar batch commit,
  savepoint restore, and clean publication selects a valid old or new state.
- Hostile sidecar ranges and native lengths return typed errors before
  allocation and never panic.
- Fixed, variable, compressed, checksummed, tombstone, keyed-chain, block-chain,
  and mixed indexed/unindexed records pass single/batch/reopen round trips.
- Formatting, workspace/all-feature tests, trybuild, property tests, clippy,
  audit, deny, and release performance probes pass.
