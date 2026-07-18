# Scalable I/O Stabilization Clean Draft

## Finding

The four selected gates are open in the inspected implementation: progress and
cancellation are deferred, PiB coverage is representational, no process-abort
durability matrix exists, and cargo-fuzz does not exercise redb `.vks`/`.vki`
sidecars.

## Scan API

Expose, under `high-cardinality-dev`:

- `ScanProgressPhase::{Started, Running, Complete}`;
- `ScanProgress { phase, records, scanned_bytes, current_offset, snapshot_len }`;
- `ScanControl::{Continue, Cancel}`;
- cloneable `ScanCancellationToken` backed by `Arc<AtomicBool>`;
- `ScanProgressOptions` with optional nonzero record/byte cadences;
- `ScanOptions` containing cadence and an optional cancellation token.

Default notification cadence is 16,384 records or 16 MiB. Cancellation tokens
are checked before scanning and at every record boundary. Callbacks run on the
calling thread. Existing operations retain their signatures; add
`verify_all_with_progress`, `bootstrap_stream_checkpoint_with_progress`, and
`rebuild_disk_index_with_progress`, including generated wrappers.

Cancellation is a typed error carrying exact completed progress. Verification
never mutates. Cancelled bootstrap publishes no `.vks`; cancelled rebuild
preserves the prior `.vki` and removes its temporary file by RAII. A final
completion callback runs before bootstrap/rebuild publication and can cancel
publication.

## Crash Matrix

Use a non-default `scalable-fault-injection` feature and process aborts. Cover
before/after persistence boundaries for native create/write/sync, sidecar
creation and replacement, generation savepoint/dirty commit, every append
native chunk and sidecar batch, clean publication/savepoint deletion, restore
fetch/stage/validation/truncate/sync/delete/commit, bootstrap, and rebuild.

A trace-only run records every reached point and occurrence. Parent tests rerun
each point in a child process and require a real abort. Recovery permits only a
restorable old generation or a fully synced clean new generation. Restore must
be repeatable when the native file was already truncated. Atomic replacement
must leave either the complete old or complete new target.

## Real PiB File Gate

Add ignored `real_file_positional_io_at_one_pib`, with
`VARVE_PIB_TEST_DIR` selecting the filesystem and
`VARVE_REQUIRE_PIB_SPARSE=1` converting unsupported behavior to failure.

Explicitly mark a sparse file on Windows and inspect allocation metadata where
available. Create a real logical file at least `1 << 50` bytes, write/sync/close
a valid bounded native frame at or above that offset, reopen and validate it
through the production typed positional-read path, then prove a one-byte
past-EOF pointer fails before I/O/allocation. Report logical/allocated bytes
and require physical allocation below 64 MiB.

The test demonstrates real OS offset handling, not PiB physical storage or a
valid sequential log across a sparse hole. At least one stabilization lane
must run it in required mode; virtual tests alone do not close the gate.

## Sidecar Fuzzing

Enable `high-cardinality-dev` in the independent fuzz workspace. Add arbitrary,
mutation, and state-machine coverage for both `.vks` and `.vki`. Raw inputs are
capped at 1 MiB and are paired with a valid native companion. Mutation seeds
cover clean empty/populated, dirty, multiple batches, unique/repeated keys,
tombstones, and mixed indexed/unindexed data. State-machine inputs are bounded
to 64 operations and 256 records and compare clean indexed reads and restore
results to a simple model.

Every invocation uses an isolated temporary directory. PR validation compiles
all targets and replays seeds. The stabilization campaign runs every new target
under ASan with finite timeout, input, and RSS limits; crashes, sanitizer
findings, timeouts, or unsafe successful corruption acceptance fail.

## Acceptance

1. Progress is monotonic and exact for empty, cadence, oversized-record,
   pre-start, callback, external-token, and final-publication cases.
2. Cancelled bootstrap/rebuild leave native and prior sidecar bytes unchanged.
3. Child-process aborts exercise every traced stream/indexed durability point,
   including multiple chunk/batch occurrences.
4. A required PiB lane performs actual OS I/O and reports filesystem and
   allocation evidence.
5. Sidecar fuzz targets compile, replay deterministic corpora, and run bounded
   ASan campaigns without findings.
6. Clean open/append/sync/restore/lookup retain zero full scans, and million-key
   memory/performance stays inside the documented envelope.
7. Workspace tests, trybuild, format, clippy, audit, and deny pass.

