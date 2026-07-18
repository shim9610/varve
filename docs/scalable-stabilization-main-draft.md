# Scalable I/O Stabilization Main Draft

## Goal

Close the four remaining `high-cardinality-dev` stabilization gates without
changing Varve's native wire format or making ordinary open/append scan the
native record region:

1. real operating-system sparse-file I/O at PiB-class offsets;
2. process-crash fault injection at every scalable durability boundary;
3. cooperative cancellation and bounded progress reporting for explicit long
   scans;
4. arbitrary and structured fuzzing of redb-backed `.vks`/`.vki` sidecars.

## Non-goals

- No claim that the development host stores one physical PiB of data.
- No cancellation thread, hidden retry loop, background worker, or async API.
- No change to normal reader/writer cost, native record framing, or stable APIs.
- No automatic recovery, rebuild, bootstrap, or verification during open.
- No production build path that can terminate a process for fault injection.

## Explicit Scan Control

Add a reusable public scan-control API under `high-cardinality-dev`:

- `ScanProgress` reports completed records, scanned record-region bytes, the
  current logical offset, and the pinned snapshot length.
- `ScanControl` is `Continue` or `Cancel`.
- `ScanProgressOptions` configures nonzero record and/or byte notification
  cadence. Cancellation is checked at every record boundary; notifications are
  coalesced and never require per-record allocation.
- `Error::ScanCancelled` retains the last exact progress snapshot.

Existing `verify_all`, `bootstrap_stream_checkpoint`, and
`rebuild_disk_index` remain source-compatible and call the new implementation
without an observer. Add corresponding `*_with_progress` forms accepting a
caller-owned `FnMut(ScanProgress) -> ScanControl`.

The callback runs only on the calling thread. Cancellation must leave the
native file unchanged. Rebuild cancellation must remove or abandon only its
temporary sidecar and leave the prior sidecar untouched. Bootstrap cancellation
must publish no `.vks`. A final progress notification is emitted on successful
completion even when cadence was not reached.

## Process Crash Matrix

Add a non-default `scalable-fault-injection` test feature. Only builds with
that feature observe `VARVE_SCALABLE_FAULT`; ordinary/published configurations
cannot be terminated by environment variables.

Named fault points cover:

1. dirty generation/savepoint publication;
2. before and after each native chunk write;
3. before and after each sidecar batch commit;
4. before and after native `sync_all`;
5. before and after clean publication commit;
6. before and after restore staging;
7. before and after native truncate;
8. before and after restore native `sync_all`;
9. immediately before and after savepoint deletion/restore commit;
10. before and after atomic replacement plus parent-directory sync.

Fault workers run as child processes and terminate with `abort`, not a caught
Rust error. Parent tests reopen the artifacts and assert one of two valid
states only: the previous clean generation is explicitly restorable, or the
new fully synced generation is clean and readable. Repeating restore is
idempotent. No outcome may silently promote a partial native tail.

## PiB Sparse-File Gate

Add an ignored, explicit platform probe that creates a real sparse file, writes
and reads a valid bounded region through Varve's positional typed-I/O boundary
at an offset at or above one PiB, and verifies checked rejection immediately
past the captured EOF. It must inspect physical allocation where the platform
exposes it and report logical versus allocated bytes.

`VARVE_REQUIRE_PIB_SPARSE=1` turns unsupported filesystem/platform behavior
into test failure. Without it, the probe reports an explicit unsupported
reason and returns so ordinary CI remains portable. Representational PiB tests
remain mandatory and non-ignored.

## Sidecar Fuzzing

Add cargo-fuzz targets for:

- arbitrary `.vks` bytes opened through the public stream API;
- arbitrary `.vki` bytes opened through the public indexed API;
- byte mutations of a generated valid `.vks` corpus;
- byte mutations of a generated valid `.vki` corpus followed by public open,
  point lookup, protocol validation, restore/rebuild refusal paths as applicable.

Inputs are capped before filesystem writes. Harness cleanup is deterministic.
Panics, aborts, sanitizer findings, unbounded hangs, and unsafe claim-sized
allocation are failures; typed errors are expected. Seed generation includes
clean, dirty, tombstone, repeated-key, unique-key, and mixed indexed/unindexed
sidecars.

## Acceptance Criteria

1. Scan callbacks have deterministic monotonic progress, cancellation latency
   of at most one decoded record, and no native/prior-sidecar mutation.
2. Every named crash point is exercised in a real child process and recovery
   invariants pass for stream and indexed modes where applicable.
3. The explicit PiB probe performs actual OS file I/O or fails in required mode
   with a precise platform/filesystem reason.
4. New sidecar fuzz targets compile and complete a bounded ASan campaign with
   no crash; seed corpus generation remains reproducible.
5. Default and all-feature workspace tests, trybuild, clippy `-D warnings`,
   formatting, `cargo audit`, and `cargo deny check` pass.
6. The million-key release probe remains within the documented regression
   envelope; ordinary clean open, append, and lookup retain zero full scans.

