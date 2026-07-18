# Scalable I/O Stabilization Final Specification

This document supersedes the deferred scan-control paragraph in
`petabyte-io-final-spec.md`. It closes four `high-cardinality-dev` gates without
changing native wire bytes or adding work to ordinary open and point lookup.

## Scan Control API

```rust
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScanProgressPhase { Started, Running, Complete }

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ScanProgress {
    pub phase: ScanProgressPhase,
    pub records: u64,
    pub scanned_bytes: u64,
    pub current_offset: u64,
    pub snapshot_len: u64,
}

#[derive(Clone, Debug, Default)]
pub struct ScanCancellationToken(/* Arc<AtomicBool> */);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ScanProgressOptions {
    pub every_records: Option<NonZeroU64>,
    pub every_bytes: Option<NonZeroU64>,
}

#[derive(Clone, Copy, Debug)]
pub struct ScanOptions<'a> {
    pub progress: ScanProgressOptions,
    pub cancellation: Option<&'a ScanCancellationToken>,
}
```

Default cadence is 16,384 records or 16 MiB, whichever is crossed first. Both
cadences may be absent, leaving only started/complete notifications. Progress
bookkeeping itself performs no per-record allocation.

Existing APIs retain their signatures. Add:

```rust
VarveStreamReader::verify_all_with_progress<F>(
    &self, scan: ScanOptions<'_>, observer: F,
) -> Result<u64>
where F: FnMut(ScanProgress);

bootstrap_stream_checkpoint_with_progress<F>(
    spec: FormatSpec, path: impl AsRef<Path>, options: StreamOptions,
    scan: ScanOptions<'_>, observer: F,
) -> Result<StreamBootstrapReport>
where F: FnMut(ScanProgress);

rebuild_disk_index_with_progress<F>(
    spec: FormatSpec, path: impl AsRef<Path>, options: DiskIndexOptions,
    plan: DiskIndexPlan, scan: ScanOptions<'_>, observer: F,
) -> Result<DiskIndexRebuildReport>
where F: FnMut(ScanProgress);
```

Indexed readers and generated formats expose delegating forms. `scanned_bytes`
excludes the native header; `current_offset` and `snapshot_len` are absolute.
Notifications are synchronous and monotonic.

The token is checked before scanner construction, after every completed record,
after every callback, and after the final callback. Cancellation returns
`Error::ScanCancelled { progress }`. The final check is the publication
linearization point: cancellation observed before it publishes nothing;
cancellation arriving after it may race with successful publication.
Verification is read-only. Cancelled bootstrap creates no target `.vks`.
Cancelled rebuild retains byte-for-byte the prior `.vki`; temporary state is
RAII-owned.

## Fault Injection

`scalable-fault-injection` is non-default and depends on
`high-cardinality-dev`. Integration-test builds contain inert hooks. The child
worker must explicitly arm a hidden test API; only that worker parses
`VARVE_SCALABLE_FAULT`. Merely setting an environment variable never activates
or aborts library code.

Trace and fault before/after every occurrence of:

- `generation.commit`;
- `append.native_chunk_write`;
- `append.sidecar_batch_commit`;
- `sync.native_sync`;
- `publish.clean_commit` (including savepoint deletion);
- `restore.stage`, `restore.native_truncate`, `restore.native_sync`, and
  `restore.commit`;
- `create.native_sync`, `create.sidecar_complete`;
- `replace.atomic` and `replace.parent_sync`.

Hooks are placed immediately around persistence primitives, including an
after-return hook before caller-visible in-memory state is advanced. A trace
run records `(point, occurrence)`, and the parent reruns every discovered pair
in an aborting subprocess. Fixtures force at least two native chunks and two
sidecar batches.

Allowed outcomes are boundary-specific:

| Boundary | Allowed persistent outcome |
| --- | --- |
| before dirty commit | unchanged clean old generation, no native append |
| after dirty commit through before clean commit | dirty generation explicitly restorable to exact old EOF |
| after clean commit | fully synced, complete clean new generation |
| restore before commit | dirty state; physical file is either tail-bearing or already truncated to base EOF; repeating restore converges |
| restore after commit | clean old generation at exact base EOF |
| interrupted creation | no valid target sidecar, or a complete clean created sidecar after native sync |
| replacement | complete old target or complete new target; temporary debris is allowed |

No clean sidecar may reference partial, unsynced, or shorter native content.
`DiskIndexStore` is private to the crate; its clean/restore commit methods are
also narrowed to crate visibility so callers cannot bypass writer ordering.

An interrupted writer leaves a lock marker by design. Generated formats expose
`clear_stale_writer_lock(path, policy)`, which only runs the lock acquisition
protocol and never opens or scans native data. Default behavior still refuses
stale metadata. Process-aware recovery must prove the recorded process is
absent. All writers and recovery calls serialize on an OS-level exclusive byte
range outside the bounded metadata region, so an active or concurrent writer
cannot be silently displaced. Clean close clears metadata; the empty lock file
may remain to preserve a stable lock identity.

## Real PiB Probe

Ignored test `real_file_positional_io_at_one_pib` uses
`VARVE_PIB_TEST_DIR` and `VARVE_REQUIRE_PIB_SPARSE=1`. It explicitly marks a
sparse file on Windows (`FSCTL_SET_SPARSE`) and uses filesystem allocation
metadata on each supported platform.

The test writes a valid bounded native frame at `1 << 50`, calls `sync_all`,
closes, reopens, and reads through
`UntrustedRecordPointer -> SnapshotBounds -> ValidatedRecordPointer ->
read_stream_entry_at`. It checks framing, sequence, payload, footer, and CRC.
Test-only point-read and allocation counters prove a one-byte-past-EOF pointer
fails before I/O/allocation. It reports architecture, filesystem, logical
length, and allocated bytes and requires allocation below 64 MiB.

Unsupported filesystem behavior is an explicit reported skip only when required
mode is absent. Required release lanes on supported 64-bit Windows and Unix I/O
families must pass. The probe proves real OS offset handling, not physical PiB
storage or a sequentially valid log across the sparse hole.

## Redb Sidecar Fuzzing

The independent fuzz workspace enables `high-cardinality-dev` and adds exactly:

- `sidecar_arbitrary`;
- `sidecar_mutation`;
- `sidecar_state_machine`.

All cover both `.vks` and `.vki`. Raw/mutated input is capped at 1 MiB before
filesystem writes. State machines permit at most 64 operations and 256 records.
Each input uses an isolated temporary directory. Seeds cover empty/populated,
clean/dirty, multiple batches, repeated/unique keys, tombstones, and mixed
indexed/unindexed records.

The Windows ASan runner recognizes all seven total targets and invokes each new
target for at least 120 seconds with `-timeout=10`, `-max_len=1048576`, and
`-rss_limit_mb=1024`. `fuzz/corpus/<target>` is the reproducible seed location.
Nonzero exit, artifact, panic, abort, timeout, sanitizer finding, uncontrolled
allocation, or corrupt sidecar accepted as clean is failure; typed rejection is
expected. The independent fuzz lockfile is audited and denied separately.

## Final Gates

Recorded execution, not compilation, is required for all four areas. Then run
default/all-feature workspace tests, trybuild, format, clippy `-D warnings`,
audit, deny, zero-scan counters, allocator-instrumented scan bookkeeping, and
the million-key release performance envelope.

Workspace tests and benchmark commands run through `varve-test-runner`. A
passing command must also remove and verify the absence of its unique temporary
session. Failed commands retain and report only their owned session for
diagnosis. Cargo build caches are managed separately to avoid repeated full
rebuild writes.
