# Matrix Durability Model

This document defines ordered durability for matrix writes and the boundary for
post-commit hooks.

Append writers also expose [Immediate policies](immediate-policy.md): block
attributes, automatic conditions, and an explicit `immediate()` boundary. These
do not change the matrix ordered-barrier or transaction-marker visibility rules.

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
retrying the operation. When the parent-directory sync for that same
publication also failed, the variant carries both facts — see "Two Durability
Facts, One Result" below.

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
authority for exclusion. `clear_stale_writer_lock` locks the marker object
first, then inspects it, and probes the target's native-object lock after that;
under the default `WriterLockBreakPolicy::Refuse` a marker that is present
refuses before the probe is reached at all. An active object lock still cannot
be displaced by recovery, because the marker is cleared only after the probe
succeeds and a live writer's hold makes the probe fail. A
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

### Two Durability Facts, One Result

Those two outcomes are not exclusive. A replacement learns two independent
facts in a fixed order — whether the parent-directory sync succeeded, and
whether the writer could rebind to the published generation — and only the
second one ends the call. When the sync fails, publication has already
happened, so the writer rebinds anyway and the caller normally learns about the
pending sync through `PublishedButParentSyncPending`. If the rebind then fails
too, that variant is never constructed.

Until F-07 the second failure simply discarded the first. The rebind variant is
now `PublishedButRebindFailed { sequence, source, parent_sync }`, where
`parent_sync: Option<Box<Error>>` carries the earlier sync failure, and its
`Display` renders both clauses:

- `parent_sync: None` — the new generation is visible at the pathname and the
  writer is unusable. The rename's own durability was established.
- `parent_sync: Some(_)` — the same two facts, **plus**: the rename is not yet
  proved durable against power loss, so a crash before the parent directory is
  flushed can leave the pathname naming the old generation. Reopen, reconcile,
  and sync.

The field is attached only to this variant, and only by the replacement paths
that actually observed the sync failure. Every other error from a replacement
path means publication did not happen, so a parent-sync fact would be
meaningless there. This is a source-breaking change for downstream code that
destructured the variant with `{ sequence, source }`; add `parent_sync` or
`..`.

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
honesty applies to the native sidecar publication sites (stream/indexed sidecar
create, stream bootstrap, disk-index rebuild and compaction), which surface the pending state
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

It carries a second requirement that F-02 made explicit: on a
`keyed_offset_chain` format, the caller must **not change the record's key**.
The entry point takes `T: VarveBlock`, which exposes no key, so this cannot be
checked at that signature, and unlike the copy-on-write paths there is no new
generation in which the chain could be rebuilt — records appended after the
target already carry `prev_same_key_offset` pointers into it, and rewriting its
key in place would leave them linking a record that now claims a different key.
Ordinary keyed replacement enforces the same-key contract through
`VarveReplaceBlock::validate_replacement`; here it is part of the unsafe
contract.

What the method guarantees regardless of whether that contract is honoured is
that no resident keyed-tail cache survives the mutation. The affected block's
tail map is dropped **before** the first byte is written, so a stale
predecessor can never be read afterwards — not after a write that fails
part-way, not through a poisoned writer that is later inspected, and not after
a contract violation. The invalidation is infallible and allocation-free, so it
adds no fallible step in either direction of the commit.

Even under the default policy, readers trust only commit maps. A clear commit
bit means `NotCommitted` regardless of slot bytes.

Matrix readers capture a confirmed generation at open. Slot payloads, commit
maps, per-cell CRCs, CRC-valid bits, aux ranges and the append-log EOF belong to
that same generation. A cache miss reads that generation's page, including while
the single writer overwrites the same cell. Each reader is `Send + !Sync`, owns
its caches, and advances only through `follow(&mut self)`.

The writer copies each changed 4096-byte logical page to the generation file on
its first change in a batch. Further changes to that unpublished page reuse the
same physical page. Published pages are never overwritten or recycled. Clearing
or rebuilding maps changes unpublished pages too; failure or process death
before publication preserves the previous confirmed generation.

`commit_matrix_cell()` marks the cell in the writer's working generation.
`flush()` writes buffered chunks but does not publish a matrix generation.
`sync()`, `immediate()` and the durable cell API publish. Dropping a writer is
not a substitute for syncing; reopening discards unconfirmed native-log bytes.
With transaction-marker formats, `sync()` still does not manufacture a marker:
use `commit_durable()` or `flush()` followed by `sync()` for `OnFlush` markers.
A matrix-only sync can advance matrix pages while leaving uncommitted native
records invisible; the published EOF excludes that tail so recovery cannot
truncate bytes promised to a reader.

Publication orders native-log persistence, new matrix pages and directory pages,
a generation-store sync, then a checksummed head in alternating slots and a final
sync. The head names the root and native-log EOF. Readers never take writer
admission or wait for cache ownership. An unstable head read may return
`Error::Io` with `WouldBlock`; retry open/follow. A failed follow leaves the
existing reader's generation usable. Stable invalid heads or pages fail closed.
The checksums detect accidental corruption, not malicious rewriting.

Matrix storage now has **two required files**: the primary file and
`<primary>.<creation-nonce>.vmg`, discoverable through
`matrix_generation_path()`. The companion is durable data, not a disposable
cache. Stop the writer while copying/renaming the pair; copying the primary
alone is not a backup. Old files without the companion need recreation.

`compact_matrix()` requires a synced writer and copies the current page tree
into a temporary companion, syncs it, then replaces the companion pathname.
Existing readers retain the old file object without blocking the writer. Their
`follow()` can adopt the replacement. Old physical storage is released once all
old file handles/maps close. Compaction covers the matrix generation store;
obsolete growing-chunk records in the primary append log are not reclaimed.
Primary-file replacement APIs remain refused for matrix files.

Each head persists a `compaction_epoch` (u64, initially zero). Only explicit
`compact_matrix()` increments it; every ordinary publication carries it forward.
Thus a reader detects replacement even after skipping the compaction generation
and several subsequent commits. Equal epochs permit comparing COW roots: equal
subtree pointers are skipped, changed bitmap index slots are patched, and only
changed metadata pages are reverified under the default verification policy.
Unchanged bitmap and physical-page caches survive. Different epochs trigger the
full layout load against the replacement file. Follow never initiates compaction.
All fallible work precedes adoption; a refused delta leaves the old root, bitmap
state and log EOF visible. A damaged or quarantined old mirror requires reopening
for recovery rather than treating an incomplete mirror as a valid diff base.


The generation page cache defaults to **2 MiB per handle**, independent of the
existing per-bitmap cache. Configure it at open with
`ReadLimits::with_matrix_generation_cache_bytes(bytes)`; zero disables it.
The bound counts retained 4096-byte data/directory buffers, excluding map
bookkeeping and temporary page buffers. Internal bitmap backings retain no
second generation-page cache. `matrix_generation()` reports the captured
confirmed generation; `follow()` still returns newly adopted record bytes, so
matrix-only advancement can return zero.

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

Matrix `commit_*` updates working visibility without a hidden fsync. A reader
sees those changes only after explicit generation publication.

## Ordered Barrier Policy

The ordered barrier is opt-in per call rather than a format declaration, and is
intended for workflows that emit live progress only after durable commit.
`VarveFile::write_matrix_cell_durable(key, value, hook)` runs the sequence with
file-handle `sync_data`/`sync_all`;
`write_matrix_cell_durable_with_barrier(key, value, &mut barrier, hook)`
substitutes a caller-supplied `MatrixDurabilityBarrier` for the two sync phases.
There is no `durability:` key in `varve_format!` — the macro refuses one with
`unsupported varve_format key` — and the hook is a closure argument rather than a
declared option.

The durable cell API runs:

1. withdraw old evidence in the working generation and write new slot bytes;
2. call the data barrier;
3. write CRC metadata and working commit bits;
4. call the commit barrier;
5. sync the native log and new matrix pages, publish and sync the generation head;
6. invoke the post-commit hook.

The barrier callbacks receive the physical companion file for synchronization,
not a logical matrix image. They must not inspect logical offsets or modify the
file. Both callbacks precede publication; a callback failure poisons the writer
and leaves readers on the previous generation.

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

A failed generation publication returns `MatrixPublicationUncertain` if head
completion was not confirmed. Reopen and inspect the confirmed generation before
retrying the write. If the complete head was written but its final sync failed,
the result is `MatrixCommittedButDurabilityUnproven`. Both poison the writer and
skip the hook. A successful publication followed by a hook error returns
`MatrixCommittedButHookFailed`; retry the notification rather than the write.

`matrix_generations.rs` aborts child writers at six publication boundaries and
checks that reopened cells and aux bytes all belong to one generation. These are
process-crash tests, not a substitute for power-loss testing of a storage device.

## Allocator Failure And Published Outcomes

The sentence above is a statement about the value a call **returns**, and F-06
is the reason it now says so. Both post-commit variants box their event and
their source to keep `Error` small, so constructing either one allocates twice
*after* the cell is authoritative. A review harness failed the very next
allocation at each branch: both children terminated on a 56-byte allocation
while the reopened file contained the committed values.

Varve has one policy on allocator failure, and it applies to every contract in
this document, not only to the matrix ones:

- **Content-sized allocations** — anything whose size is chosen by file bytes,
  a caller-supplied count, or any other quantity a producer of the input can
  influence — are charged against a `ReadLimits` ceiling *before* the memory is
  taken and are then reserved fallibly. A refusal is the typed
  `Error::AllocationFailed` or `Error::LimitExceeded { resource, .. }`, and the
  operation reports it without mutating anything. This is the class that
  hostile input can reach, and it is the class the limits exist for.
- **Shape-sized allocations** — fixed-size allocations bounded by the program's
  own compile-time shape, such as the two boxes a published-outcome variant
  holds — use ordinary infallible allocation. `Box::new` has no fallible form,
  and the Rust default is to abort the process when the allocator refuses.
  Varve does not pretend otherwise.

The division matters because only a content-sized allocation is one a producer
of the input can steer, which is why that class carries the ceilings and the
typed refusals and the other does not.

The consequence for every published-outcome variant in this document is
precise: a shape-sized allocation failure after publication
**terminates the process** rather than substituting a different error. No caller ever observes a
*wrong* outcome; a caller can observe *no* outcome, which is the same thing a
`SIGKILL` or a power loss delivers at that instant. The on-disk state is the
published state, so the correct recovery is the one this document already
prescribes for a crash: reopen the file and read what is there. In the F-06
harness the reopened file held the committed cell, which is exactly the claim
the variant would have made.

Three alternatives were considered and rejected for the post-commit path:

- pre-staging the outcome boxes before the commit removes the event
  allocation but not `Box::new(source)` — the source only exists once the step
  has failed, and `Error` is recursive, so it cannot be carried inline. It also
  puts two allocations on the success path of every durable cell write to serve
  a path that ends in `abort`;
- a non-recursive post-commit outcome type cannot carry the caller's own hook
  error, which is an arbitrary `Error` and may itself own heap;
- a fallible-allocation form of the whole error family would make every
  `Result` in the crate depend on an allocator that has already failed.

The same policy is why round 12's `SparseBitmap::clone` in the CRC rebuild was
*removed* rather than made fallible: deleting a shape-sized allocation is worth
doing where it is free, and dressing one up in a `Result` it cannot honour is
not.

## Sync Granularity

The first implementation may use file-handle `sync_data`/`sync_all` primitives
for portability. Later implementations can add platform-specific range syncs
without changing the logical barrier.

The current `write_matrix_cell_durable` helper uses that portable file-handle
ordering: write slot bytes, `sync_data`, then commit the cell — which writes any
enabled CRC and CRC-valid metadata and publishes the commit bit last — then
`sync_all`, then invoke the hook.
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

The injectable recorder verifies that a fresh reader still sees the previous
generation during both barrier callbacks, and sees the new generation after success.
- verified sidecar envelopes can be rejected before resume signal publication
