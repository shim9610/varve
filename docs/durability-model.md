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
- `sync` (and `commit_durable`) additionally makes a pathname *created by that
  handle* durable — see "Created Pathname Durability" below
- a returned write may be visible through the current writer handle before it is
  crash-durable

Safe append-log fixed replacement is copy-on-write. The replacement generation
is written in the target directory, flushed and synced, fully reopened and
validated, then atomically published. Publication failure leaves the original
path generation unchanged, with the single Windows exception of the typed
indeterminate state described below. An already-open reader remains bound to its retained
file object and captured logical EOF rather than reopening the pathname.
If publication succeeds but the writer cannot reopen the published pathname,
Varve returns `PublishedButRebindFailed` and poisons that writer. This is not a
publication rollback: callers must reopen and reconcile instead of blindly
retrying the operation.

## Commit Marker Versus Commit Durability

`commit_durable` does two things in order: it appends the transaction's commit
marker, and it then asks the operating system to make the file durable. The
first is the authoritative commit — once the marker bytes are in the file, a
reader that opens the file after a clean process exit sees the transaction as
committed, whether or not the durability request that follows succeeded.

Those two outcomes are therefore reported separately rather than collapsed into
one `Err`:

- a failure **before** the marker is appended returns an ordinary error and the
  transaction is not committed. Re-running it is correct;
- a failure of the `flush`/`sync_all` **after** the marker is appended returns
  `Error::CommittedButDurabilityUnproven { sequence, source }`. The transaction
  *is* committed and `sequence` names the marker record. What is unproven is
  only that it survives a power loss. The correct response is to retry `sync`;
  re-running the transaction appends a second marker for work that is already
  recorded;
- a failure of the created pathname's directory sync after the marker returns
  `Error::PublishedButParentSyncPending`, described in the next section, and
  leaves the request pending so a later `sync` retries it.

This is the same published-outcome family as `PublishedButRebindFailed`,
`MatrixCommittedButHookFailed` and `MatrixCommittedButDurabilityUnproven`: a
caller that receives one of them must not treat the operation as
not-performed.

## Created Pathname Durability

Decision (DUR-01): `sync` and `commit_durable` establish the durability of a
pathname the calling handle created, in addition to the file's contents.

`sync_all` makes the file's bytes durable, but on platforms that require an
explicit directory sync it does not make the newly created *directory entry*
durable. A fully synced object with no surviving name does not deliver the
"durable persistence" the public contract promises, and the atomic replacement
path already syncs the parent directory and reports a pending parent sync — so a
create path that did not was internally inconsistent with the rest of the model.

The rule, precisely:

- the parent directory is fsynced on the **first** successful `sync` or
  `commit_durable` of a handle that created its pathname, and never again;
- it costs one directory fsync per created file. It is not per sync, not per
  commit, and never on the append path;
- a handle that merely **opened** an existing pathname does not re-establish it,
  because that name's durability is not this handle's to claim;
- if the platform refuses the directory sync, the result is the typed
  `Error::PublishedButParentSyncPending` rather than a silent success. The file
  contents are durable and the pathname is visible; only the directory entry's
  durability is unconfirmed, and the request stays pending so a later `sync`
  retries it.

This is a behaviour change: a first `sync` on a created file can now return
`PublishedButParentSyncPending` where it previously returned `Ok(())`.

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

On Windows, `ReplaceFileW` failures are classified per the documented OS
contract (`classify_replace_publication_error`, pure and unit-testable on every
platform):

- `ERROR_UNABLE_TO_REMOVE_REPLACED` (1175) is the documented no-mutation
  failure: the replaced file is intact under its original name.
- Every other error except the two below occurs before any mutation; both
  files retain their original names and the temp may be deleted.
- `ERROR_UNABLE_TO_MOVE_REPLACEMENT` (1176) and
  `ERROR_UNABLE_TO_MOVE_REPLACEMENT_2` (1177) can leave the two files' names,
  streams, and attributes partially moved. Varve captures the replacement's OS
  object identity before the call and reconciles on 1176/1177: if the target
  pathname already resolves to the replacement object, the publication is
  treated as complete and proceeds through the normal rebind path. Otherwise
  Varve returns the typed `Error::ReplacePublicationIndeterminate`: the
  replacement temp file is preserved for reconciliation, and a writer bound to
  the target is poisoned so a blind retry is impossible. Callers must inspect
  the pathname and the preserved temp separately before acting.

Any error from replacement other than these typed post-publication and
indeterminate states means publication did not happen and the original
generation still stands.

The parent-directory sync itself is honest about refusal. `FlushFileBuffers`
requires `GENERIC_WRITE` on the handle, so the Windows implementation opens the
parent directory with write access, and no open or flush failure is ever
promoted to a `Durable` result. On filesystems that refuse a directory
write-open or flush, every publication surfaces
`PublishedButParentSyncPending` rather than a false durability claim; the same
honesty applies to the redb sidecar publication sites (stream/indexed sidecar
create, stream bootstrap, disk-index rebuild), which surface the pending state
while preserving the already-published sidecar instead of silently discarding
it.

Exclusive-create constructors (`VarveFile::create_new`,
`VarveWriter::create_new`, and the matrix `create_new_with_dims` pair) open
with `create_new`, never truncating or reusing an existing path; they fail
with an `AlreadyExists` I/O error instead. All create paths bind the
single-writer object lock before destructive initialization: open without
truncate, bind the lock on the file object, then `set_len(0)` and write the
header, so a losing concurrent creator can never truncate the winner's freshly
initialized file inside the pre-bind window.

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

That distinction is now in the type, not only in this document (F-04). A hook
failure returns
`Error::MatrixCommittedButHookFailed { event: Box<MatrixCommitEvent>, source }`,
the same shape as `PublishedButParentSyncPending` and
`ReplacePublicationIndeterminate`. Retrying the whole call after this variant
repeats the durable write and re-runs the hook, which is how a result-driven
retry used to duplicate external work. The carried event is the one the hook
was given; it is derived from layout geometry before the commit, so producing
it can no longer fail for a cell that is already durable.

The hook is not the only step after the commit. Walking forward from the
authoritative commit — `commit_matrix_cell`, which puts the commit bit in the
file — there are exactly two remaining steps, and *both* are published outcomes
(round 11):

1. the commit sync (`MatrixDurabilityBarrier::sync_matrix_commit`). Its failure
   returns
   `Error::MatrixCommittedButDurabilityUnproven { event: Box<MatrixCommitEvent>, source }`.
   The cell **is** committed and a reader that opens the file after a clean
   process exit sees it; what is unproven is only that the commit survives a
   power loss. The hook does not run, so no notification was emitted. The
   writer is poisoned, because a durability request was refused
   mid-publication: recover by reopening the file and calling `sync`, then
   issue the notification with the carried event. The cell does not need to be
   rewritten. Until round 11 this returned a bare `Err`, which a caller could
   not distinguish from "nothing happened" — the same defect as the hook one
   step later, and the same shape as the `commit_durable` defect fixed in round
   10;
2. the hook, described above.

The step *before* the commit — the data sync
(`MatrixDurabilityBarrier::sync_matrix_data`) — is genuinely pre-publication:
the slot bytes may be on disk but the commit bit is not, an uncommitted slot is
invisible to readers, and its failure therefore stays a plain error. Both
classifications are asserted by
`crates/varve/tests/matrix.rs::a_commit_sync_failure_is_reported_as_published_and_a_data_sync_failure_is_not`,
which also reads the cell back through a fresh reader to prove the published
claim rather than only its shape.

Those two variants are the only published outcomes of
`write_matrix_cell_durable`. Every *other* error from it means the cell was not
committed by that call, so a caller can decide between "nothing happened, retry
the write", "committed, retry only the notification" and "committed,
re-establish durability" from the result alone.

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

The matrix sidecar manifest is version 3 (104-byte fixed header). Beyond the
existing format/schema/category/caller-generation/length/CRC fields it binds
the sidecar to a specific native file and a specific logical creation with
three additions:

- a native object fingerprint that folds the OS file identity (Windows
  volume + file id, Unix device + inode) with the schema hash,
- a matrix layout generation, and
- the 16-byte matrix creation nonce.

The creation nonce closes the same-object recreation gap: OS file identity and
layout offsets are stable when a matrix is recreated into the same
pathname/file object with the same dimensions, so identity and generation
alone could accept the previous creation's sidecar. Every matrix create stamps
a fresh nonce region (`VMNC` magic, version, 128-bit nonce) between the native
file header and the matrix layout header; the nonce is read once at open and
cached in the handle, adding no per-operation cost. A caller-supplied
application generation cannot substitute for the native creation identity.

A reader recomputes the identity from its own native file and rejects a
mismatch as `MatrixSidecarMismatch("native identity")`,
`MatrixSidecarMismatch("matrix layout generation")`, or
`MatrixSidecarMismatch("creation nonce")`. A version-1, version-2, or
otherwise unrecognized envelope is refused as
`MatrixSidecarMismatch("sidecar version")`. Because sidecars are regenerable
resume state, a refused sidecar is a regenerate-and-retry signal, not data
loss — so neither a same-spec sibling file nor a recreated matrix in the same
file object can silently adopt another creation's sidecar. All fixed-header
identity fields and the small payload magic/category prefix are validated
before any payload allocation, read, or hash, so an obviously foreign sidecar
is rejected without configured-limit-sized work.

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
