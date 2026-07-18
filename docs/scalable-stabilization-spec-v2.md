# Scalable I/O Stabilization Spec v2

## Goal

Close and execute all remaining `high-cardinality-dev` gates without changing
the native wire format or the bounded, scan-free cost of ordinary open, append,
sync, restore, and point lookup:

1. progress and cooperative cancellation for explicit scans;
2. process-abort durability testing at persistent boundaries;
3. real PiB-offset operating-system I/O;
4. bounded redb sidecar fuzz campaigns.

## Scan Control

Expose `ScanProgress`, a cloneable `ScanCancellationToken`,
`ScanProgressOptions`, and `ScanOptions`. Existing `verify_all`, bootstrap, and
rebuild signatures remain. Add corresponding `*_with_progress` forms accepting
`ScanOptions` and `FnMut(ScanProgress)`.

The token is the cancellation primitive; callbacks are observational and may
cancel through a cloned token. Check it before scanner construction, after each
completed record, after callbacks, and after the final notification immediately
before publication. Notify initially, at either configured cadence, and on scan
completion. Defaults are 16,384 records or 16 MiB. Progress is exact,
monotonic, synchronous, and allocation-free per record.

`Error::ScanCancelled` carries exact progress. Verification never mutates.
Cancelled bootstrap publishes no `.vks`; cancelled rebuild preserves the prior
`.vki` and removes its temporary sidecar by RAII.

## Crash Matrix

Use the non-default `scalable-fault-injection` feature. Fault hooks do nothing
until a subprocess worker explicitly arms them; ordinary builds contain no
environment-triggered abort behavior.

Instrument before and after:

- dirty generation/savepoint commit;
- every native chunk write;
- every sidecar batch commit;
- native `sync_all`;
- clean publication/savepoint-deletion commit;
- restore staging, native truncate, native sync, and restore commit;
- initial native sync, temporary sidecar completion, atomic replacement, and
  parent-directory sync.

Trace `(point, occurrence)` and rerun each reached occurrence in a child process
using deterministic multi-chunk and multi-batch fixtures. Applicable matrices
cover stream and indexed create/append/sync/restore, bootstrap, and rebuild.
Every run must really abort. Recovery permits only an explicitly, idempotently
restorable old generation or a fully native-synced clean new generation.
Replacement leaves a complete old or complete new sidecar.

## Real PiB Gate

Add ignored `real_file_positional_io_at_one_pib`. `VARVE_PIB_TEST_DIR` chooses
the filesystem; `VARVE_REQUIRE_PIB_SPARSE=1` converts unsupported behavior into
failure.

Create an explicitly sparse real file beyond `1 << 50`, write/sync/close/reopen
a valid bounded frame at or above one PiB, read it through the production typed
positional-I/O boundary, and prove a one-byte-past-EOF pointer fails before I/O
or allocation. Report platform, filesystem, logical length, and allocated bytes;
physical allocation must stay below 64 MiB.

Virtual PiB tests remain mandatory but do not close this gate. Required release
lanes must successfully execute the real probe. It proves real offset handling,
not physical PiB storage or a valid sequential log over a sparse hole.

## Sidecar Fuzzing

Enable `high-cardinality-dev` in the fuzz workspace and add:

- `sidecar_arbitrary` for capped raw `.vks` and `.vki` with valid companions;
- `sidecar_mutation` for valid clean/dirty corpus mutation;
- `sidecar_state_machine` for bounded stream/indexed operation sequences checked
  against a simple model.

Cap raw inputs at 1 MiB and state machines at 64 operations/256 records. Seeds
cover empty/populated, multiple batches, dirty checkpoints, repeated/unique
keys, tombstones, and mixed indexed/unindexed records. Each invocation uses an
isolated temporary directory.

The runner recognizes all seven targets. Gate closure requires replaying seeds
and actually running each new target under ASan for at least 120 seconds with a
10-second per-input timeout, 1 MiB cap, and finite RSS limit. Panics, aborts,
sanitizer findings, hangs, unsafe allocation, or corrupt state accepted as clean
are failures; typed rejection is expected.

## Acceptance

All four gates have recorded successful executions. Then default/all-feature
workspace tests, trybuild, format, clippy `-D warnings`, audit, deny, zero-scan
counters, and the million-key release envelope pass.

Rejected choices: callback-only cancellation, production environment-triggered
aborts, virtual-only PiB proof, compile-only fuzzing, and faulting every
non-persistent instruction. Persistent boundaries and every traced occurrence
remain mandatory.

