# Petabyte I/O Clean-Context Draft

Status: independent clean-context specification result, preserved for
consolidation.

## Required Contract

- Scalable clean reader/writer open performs zero native record scans.
- Open cost is independent of native record and key count.
- Memory is bounded by configured cache, batch, record, key, and declared
  block state.
- Append/index publication never rereads newly appended native bytes.
- Disk key indexing remains optional.
- Rebuild, verification, and legacy bootstrap are explicitly named operations.
- Raw offsets and lengths become fallible checked extents before positional I/O.

## Required State

A trusted checkpoint is necessary to resume a writer without scanning. It must
retain file identity, schema, generation, committed EOF, record count, sequence
state, commit state, internal tails, and one tail per declared block. The tail
table is bounded by the format declaration.

Without such a checkpoint, constant-time code cannot determine logical EOF
after a partial write, next sequence, block tails, transaction frontier, or
whether trailing bytes are committed. Without a disk index, latest lookup for
an arbitrary key cannot be bounded independently of key count.

The independent draft proposed dual fixed native-header checkpoint slots and
append-only checkpoint records. It also required exact native/index generation
matching, typed sidecar entries, and old/new generation selection under every
fault-injection point.

## Typed Extents

The draft proposed `FileOffset`, `ByteLength`, `RecordOffset`, `RecordExtent`,
`SnapshotId`, and `CheckedRecordRef`. Construction rejects overflow, extents
past captured EOF, undersized records, and unsupported platform conversions.

Every indexed access validates snapshot identity, extent, block id/version,
sequence, encoded length, expected key, and declared record checksum before
decode. A hostile sidecar entry therefore fails locally without initiating a
scan.

## Append And Crash Ordering

Prepared records contain both native bytes and the complete checked descriptor
used to derive index updates. After `write_all`, only bounded sequence, tail,
EOF, and index state is updated. Fresh native records are never scanned.

The proposed durable order was native record/checkpoint write, native sync,
prepared sidecar-root sync, native checkpoint publication, then sidecar-root
commit. A crash must select exactly an old or new generation without replaying
native records.

## API Requirements

- Clean existing open never rebuilds implicitly.
- Missing, stale, dirty, and identity-mismatched state has typed errors.
- `flush` does not publish a durable snapshot.
- Durable publication is an explicit checkpoint/sync operation.
- `rebuild_disk_index`, `verify_native_file`, and legacy checkpoint bootstrap
  are explicit operations with progress/cancellation/reporting surfaces.

## Acceptance Tests

1. Counting I/O proves identical clean-open reads for sparse GiB, TiB, and PiB
   logical snapshots and no record-region reads.
2. No read overlaps newly written record extents during append/checkpoint.
3. Unique-key allocator growth is bounded by configured transient state.
4. Failure injection after every durability step chooses exactly old or new.
5. Hostile offsets, lengths, identities, sequences, blocks, keys, and checksums
   return typed errors without panic.
6. Stream-only operation never touches a disk key-index path.
7. Ordinary open never triggers rebuild.
8. Corruption is reported when the affected record is accessed; unrelated
   records remain accessible.
9. Existing snapshots stay pinned after a newer generation is published.
10. Checkpointless legacy input requires explicit bootstrap and reports scan
    bytes and records.
