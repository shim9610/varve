# Matrix Durability Model

This document defines ordered durability for matrix writes and the boundary for
post-commit hooks.

## Default Policy

Without an ordered barrier policy, matrix writers follow the existing Varve
durability model:

- writes may be buffered
- `flush` pushes buffered bytes to the operating system
- `sync` requests durable persistence
- no per-cell fsync is implied
- a returned write may be visible through the current writer handle before it is
  crash-durable

Even under the default policy, readers trust only commit maps. A clear commit
bit means `NotCommitted` regardless of slot bytes.

Append-log transaction markers follow the same explicit durability principle.
`commit()` writes a logical visibility marker for
`transaction_marker(explicit)` formats, but it is not a hidden fsync barrier.
Use `commit_durable()` when the marker itself must only be published after
commit-covered records, embedded manifest data, and index checkpoints have been
flushed and synced.

The durable append-log order is:

1. write commit-covered records, manifest, and index checkpoint as needed
2. flush and `sync_data` those bytes
3. write the commit marker
4. flush and `sync_all` the marker

For P0, `commit_*` is logical visibility, not a hidden fsync. It writes the
commit bit after slot bytes have been written through the file handle. Callers
that need crash-durable completion must call `flush`/`sync` explicitly or use
the P1 ordered barrier.

## Ordered Barrier Policy

The ordered barrier policy is opt-in and intended for workflows that emit live
progress only after durable commit.

```rust
durability: ordered_barrier {
    phases = [data: sync_data, index: sync_data, commit: sync_all];
    post_commit_hook = true;
}
```

The required order for a committed cell write is:

1. write data slot bytes
2. `sync_data` the data range or portable file handle
3. write affected CRC metadata if enabled and set the commit bit
4. write commit map CRC metadata if enabled
5. `sync_all` the commit map and required metadata
6. invoke the post-commit hook

The hook must never run before the commit map sync completes successfully.

## Hook Contract

The post-commit hook receives storage facts only:

```rust
pub struct MatrixCommitEvent {
    pub block_id: u32,
    pub category: &'static str,
    pub key: MatrixKey,
    pub slot_offset: u64,
    pub slot_len: u64,
}
```

Hook behavior is caller-owned. Varve should not interpret progress messages,
network delivery, UI updates, or domain side effects.

If the hook fails after durable commit, the data remains committed. The error is
reported to the caller as a post-commit failure, not as a storage rollback.

## Sync Granularity

The first implementation may use file-handle `sync_data`/`sync_all` primitives
for portability. Later implementations can add platform-specific range syncs
without changing the logical barrier.

The current `write_matrix_cell_durable` helper uses that portable file-handle
ordering: write slot bytes, `sync_data`, commit the cell, update enabled CRC
metadata, `sync_all`, then invoke the hook.
`write_matrix_cell_durable_with_barrier` exposes the same sequence through an
injectable `MatrixDurabilityBarrier`, which lets tests and policy adapters
record or replace the sync phases. Sidecar-aware barriers remain future
hardening work.

Per-record or per-cell fsync is not the default. Ordered barrier is chosen only
when callers need the stronger completion semantics.

## Sidecar Ordering

When a sidecar participates in the same logical commit:

1. write and sync sidecar data needed for resume
2. write and sync main file data/index
3. set and sync main commit bit
4. update sidecar progress metadata if configured
5. invoke post-commit hook

The current implementation provides an optional sidecar envelope and verified
resume signal. A separate durable parent sidecar-active flag remains future
policy work; callers that need that stronger contract should encode the active
state in their own payload or metadata until the policy is promoted.

## Acceptance Tests

Durability tests should use an injectable sync/hook recorder so ordering can be
verified without depending on real crash behavior:

- data sync occurs before commit sync
- commit sync occurs before post-commit hook
- hook failure does not clear a committed bit
- default policy does not perform per-cell sync

The current matrix test suite includes an injectable recorder that verifies the
commit bit is still clear during data sync and set before commit sync.
- verified sidecar envelopes can be rejected before resume signal publication
