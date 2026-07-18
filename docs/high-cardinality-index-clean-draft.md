# High-Cardinality Streaming Index: Clean Draft

Status: independent clean-context draft for consolidation.

## Profiles

Keep existing resident handles unchanged and add opt-in profiles:

| Profile | Record index | Key index | Resident memory |
| --- | --- | --- | --- |
| `resident` | existing `Vec<RecordIndexEntry>` | existing maps | O(records + keys) |
| `streaming` | none | none | O(schema + maximum current record) |
| `keyed_sidecar` | none | paged on-disk B+ trees | O(schema + current record + configured cache) |

The access choice is operational and does not alter native wire bytes, schema
hashes, or existing generated handles. Generated APIs include `open_stream`,
`create_stream`, `open_indexed`, and `create_indexed`, plus typed streaming and
per-key methods. Existing APIs retain their current semantics.

## Streaming state

A `VarveStreamReader` retains a snapshot, logical EOF, current offset, and
fixed scanner state. A `VarveStreamWriter` retains sequence/append state, commit
state, and at most one tail per declared block. Neither owns the complete
record index. Reopen uses a bounded-memory validation scan.

Transaction visibility requires a first pass to locate the last committed
marker. Sequence validation uses a monotonic fast path; supporting arbitrary
non-monotonic sequences with duplicate detection requires caller-selected
scratch/external sort. Pure streaming keyed append is incompatible with keyed
offset chains unless an on-disk keyed index supplies the tail.

## Sidecar

Use a derived `<native-path>.vxi` companion with dual fixed superblocks and
copy-on-write, CRC-protected, slotted B+ tree pages. It stores:

- a state tree from `(block id, canonical key bytes)` to the latest put or
  tombstone and chain tail;
- an event tree from `(block id, canonical key bytes, sequence, offset)` to the
  event record, enabling per-key op materialization.

Pages are read positionally through a cache bounded by `cache_bytes`. Key bytes
are compared in full. Lookup is worst-case O(log_B K) page reads, not strict
O(1), but resident memory is independent of cardinality.

The sidecar is non-authoritative. Every offset and decoded key is revalidated
against the retained native snapshot. Missing, stale, corrupt, or dirty
sidecars are caught up or rebuilt through a bounded-memory scan and atomic
temporary-file publication. Native append is published before a new sidecar
root. A crash in between leaves a recoverable native suffix.

## Initial exclusions

- checkpoint-on-flush bounded writers, replacement, ordinal block lookup,
  record-index exposure, append-log mmap, whole-map keyed materialization;
- existing merge/compact helpers without external sorting;
- custom physical layouts and mixed matrix/append-log files;
- shared sidecar rebuild without an exclusive native writer lock.

Compression, integrity, manifests, record footers, transaction markers,
tombstones, ops, and composite keys remain supported where their bounded-state
rules are implemented.

## Acceptance

- Native output remains byte-compatible with resident writers.
- 100K, 1M, and 10M unique-key runs with an 8 MiB cache show no
  cardinality-correlated resident bookkeeping slope.
- Streaming append remains within 15 percent of resident append.
- Lookup page reads scale logarithmically; rebuild and indexed append show no
  superlinear throughput collapse.
- Injected failures at native/page/superblock/sync boundaries never hide native
  records or publish an invalid authoritative state.
- Existing default/all-feature tests continue to pass.

