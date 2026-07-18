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

Safe append-log fixed replacement is copy-on-write. The replacement generation
is written in the target directory, flushed and synced, fully reopened and
validated, then atomically published. Publication failure leaves the original
path generation unchanged. An already-open reader remains bound to its retained
file object and captured logical EOF rather than reopening the pathname.
If publication succeeds but the writer cannot reopen the published pathname,
Varve returns `PublishedButRebindFailed` and poisons that writer. This is not a
publication rollback: callers must reopen and reconcile instead of blindly
retrying the operation.

## Single-Writer Lock On The Native File Object

The authoritative single-writer guard is an OS lock held on the open native
file *object*, not on a path-derived marker. On Windows Varve takes an exclusive
`LockFileEx` (`LOCKFILE_EXCLUSIVE_LOCK | LOCKFILE_FAIL_IMMEDIATELY`) over a
one-byte range at a reserved offset that never overlaps real data, so ordinary
readers are unaffected. On Unix it takes an advisory exclusive lock on the same
handle. The guard is bound to freshly created files at create time and probed
for existing files at open time, and it is re-bound to each newly published
copy-on-write generation. Because the lock lives on the file object that every
name for the file shares, a second writer opening the file through a hard link
or reparse alias cannot acquire it — closing the path-alias bypass that the
former path-derived `.lock` scheme allowed.

The `<file>.lock` marker is retained only as diagnostic and break-policy
metadata (content is written below the reserved lock range). It is never the
authority for exclusion. `clear_stale_writer_lock` acquires the native-object
lock before it inspects or clears marker metadata regardless of the break
policy, so an active object lock can never be displaced by recovery. A
zero-length `.lock` file may persist as a stable filesystem identity for future
guard acquisition; `inspect_writer_lock()` reports it as neither active nor
stale.

## Typed Post-Publication Replace Outcome

Atomic replacement renames or `ReplaceFileW`s the validated generation into
place and then syncs the parent directory. The rename is the publication point:
once it returns, the pathname already names the new generation. A parent-sync
error afterward therefore cannot mean "original unchanged". Varve models the two
outcomes explicitly rather than letting a late error escape before rebinding:

- On a parent-sync failure after a successful rename, the writer is first
  re-bound to the published generation and then `PublishedButParentSyncPending`
  is returned. Publication happened; only power-loss durability of the new
  directory entry is unconfirmed. Callers may proceed or re-sync, but must not
  treat this as a rollback.
- If the post-publication re-bind itself fails, the writer is poisoned and
  `PublishedButRebindFailed` is returned.

Any error from replacement other than these two typed post-publication states
means publication did not happen and the original generation still stands.
Exclusive-create constructors (`VarveFile::create_new`, `VarveWriter::create_new`)
open with `create_new`, never truncating or reusing an existing path; they fail
with an `AlreadyExists` I/O error instead.

The unsafe exclusive in-place replacement method does not provide that snapshot
guarantee. Its safety contract requires process-wide and cross-process
exclusion, including mmap and raw references.

Even under the default policy, readers trust only commit maps. A clear commit
bit means `NotCommitted` regardless of slot bytes.

For overwrite, Varve clears the old commit and CRC-valid evidence before the
first slot byte is changed. A partial slot write therefore remains hidden and
poisons the writer. Matrix readers must still be coordinated with in-place
writes: an already-open reader owns a commit-map snapshot but does not own a copy
of the slot region.

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

1. withdraw any old commit and CRC-valid evidence
2. write data slot bytes
3. `sync_data` the data range or portable file handle
4. write affected CRC metadata and CRC-valid evidence if enabled
5. write commit map CRC metadata and publish the commit bit last
6. `sync_all` the commit map and required metadata
7. invoke the post-commit hook

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

## Matrix Sidecar Identity And Atomic Publication

The matrix sidecar manifest is version 2 (88-byte fixed header). Beyond the
existing format/schema/category/caller-generation/length/CRC fields it binds the
sidecar to a specific native file with two additions:

- a native object fingerprint that folds the OS file identity (Windows
  volume + file id, Unix device + inode) with the schema hash, and
- a matrix layout generation.

A reader recomputes the fingerprint from its own native file and rejects a
mismatch as `MatrixSidecarMismatch("native identity")` or
`MatrixSidecarMismatch("matrix layout generation")`. A version-1 or otherwise
unrecognized envelope is refused as `MatrixSidecarMismatch("sidecar version")`.
Because sidecars are regenerable resume state, a refused sidecar is a
regenerate-and-retry signal, not data loss — so a same-spec sibling file can no
longer silently adopt another file's sidecar.

Publication is atomic and ordered. `write_matrix_sidecar` writes the new
manifest to a same-directory RAII temp file, `sync`s it, atomically replaces the
existing sidecar, and syncs the parent directory. The native file is the
authority and is written and synced before the sidecar is published
(native then sidecar ordering). A parent-sync failure after the replace returns
`PublishedButParentSyncPending` without deleting the now-published sidecar; the
temp is removed only when the replace itself failed. This replaces the former
in-place `File::create` truncate-and-`sync_all` publication, which exposed a
partially written sidecar during a crash.

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
