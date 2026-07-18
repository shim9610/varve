# Scalable Stream And Disk-Index I/O

This guide covers the experimental `high-cardinality-dev` API. It is the
Varve path intended for very large append logs. Its normal open and append
costs do not grow with native file size or record count.

The resident `create_writer`, `open_reader`, `blocks`, and `keyed_blocks`
APIs remain useful for bounded files. They may scan or retain indexes and are
not the petabyte-scale path.

## Declare The Policy

```rust
use varve::varve_format;

varve_format! {
    pub format Capture {
        magic: b"CAPT";
        version: 1;
        index: keyed_offset_chain;

        blocks {
            fixed Run(id = 1) {
                id: u64,
            }

            variable Frame(id = 2, key = [scan, frame], key_index = disk) {
                scan: u32,
                frame: u32,
                payload: Vec<u8>,
            }
        }
    }
}
```

`key_index = disk` is declaration-time policy. The macro generates a canonical
disk plan for `Frame`, typed indexed reader/writer methods, and stable key codec
identities. It does not generate chain-unsafe mutation methods for a keyed
block that lacks the required disk plan.

Generated non-keyed block methods continue to work in a
`keyed_offset_chain` format. Generated `VarveBlock` implementations carry an
`IS_KEYED` type fact so the runtime rejects chain-unsafe low-level batch calls
without rescanning the file.

When `high-cardinality-dev` is enabled, manual `VarveBlock` implementations
must declare `const IS_KEYED: bool`; there is no chain-unsafe default. Set it
to `true` for every manual `VarveKeyedBlock` implementation and `false`
otherwise. Omitting it is a compile error.

## Choose One File Mode

| Mode | Sidecar | Use when |
| --- | --- | --- |
| stream | `<file>.vks` | sequential append and explicit lazy scans are enough |
| indexed | `<file>.vki` | one or more blocks need disk-backed latest-key lookup |

Use one mode consistently for a file. A `.vks` stores only bounded resume
state. A `.vki` stores the same resume state plus the declared disk-key B-tree.
Unindexed blocks in an indexed file update only coverage and optional block
tails; they do not create key rows.

## Indexed Write And Read

```rust
use varve::{BatchOptions, DiskIndexOptions};

let mut writer = Capture::create_indexed_writer(
    "capture.varve",
    DiskIndexOptions::default(),
)?;

writer.push_run(&Run { id: 7 })?;

let report = writer.push_frames(
    (0..1_000_000u32).map(|frame| Frame {
        scan: frame / 1_000,
        frame,
        payload: Vec::new(),
    }),
    BatchOptions::default(),
).map_err(|error| error.source)?;

assert_eq!(report.records, 1_000_000);
writer.sync()?;

let reader = Capture::open_indexed_reader(
    "capture.varve",
    DiskIndexOptions::default(),
)?;

let frame = reader.get_frame(&(12, 12_345))?;
let all_runs = reader.runs()?;       // explicit lazy native scan
let all_events = reader.events()?;  // explicit lazy native scan
let verified = reader.verify_all()?; // explicit eager full validation
# Ok::<(), varve::Error>(())
```

`get_frame` performs one B-tree lookup and one bounded positional native
record read. Opening the reader does not read the native record region.
Calling `runs`, `frames`, `events`, or `verify_all` explicitly creates a native
scanner; their cost is linear in the selected snapshot.

## Stream Write And Read

For a format/file that does not need disk-key lookup:

```rust
use varve::{BatchOptions, StreamOptions};

let mut writer = Capture::create_stream_writer(
    "capture-stream.varve",
    StreamOptions::default(),
)?;

let report = writer.push_runs(
    (0..100_000u64).map(|id| Run { id }),
    BatchOptions::default(),
).map_err(|error| error.source)?;
writer.sync()?;

let reader = Capture::open_stream_reader(
    "capture-stream.varve",
    StreamOptions::default(),
)?;
let runs = reader.runs()?;
# Ok::<(), varve::Error>(())
```

The stream writer does not expose `push_frame` for this declaration because
`Frame` requires a previous-key chain backed by the disk index.

## Batch Cost And Memory

`BatchOptions` bounds transient native buffering:

```rust
let batch = BatchOptions {
    max_records: 16_384,
    max_bytes: 4 * 1024 * 1024,
};
```

A chunk is encoded in memory and written once with `write_all`. `write_calls`
in `BatchAppendInfo` counts those calls, not guaranteed kernel syscalls. A
single valid record larger than `max_bytes` is written alone; per-record stored
and logical payload limits still apply before allocation or I/O.

`DiskIndexOptions::batch` independently bounds one redb update transaction.
`cache_bytes` bounds the redb cache and `max_key_bytes` bounds an encoded key.
None of these values is a total file-size, total record-count, or lifetime
append limit.

The paged `.vks`/`.vki` backing file is deliberately not rejected by
`ResourceLimits::max_sidecar_len`: opening it does not materialize its full
length. That limit still applies to Varve sidecar formats that are read into
memory, such as matrix/adapter envelopes. Scalable sidecar safety instead
comes from bounded redb pages/cache, fixed metadata rows, bounded key rows,
and checked native extents.

The runtime retains declared block tails and bounded buffers, not one RAM entry
per record or key. Key cardinality grows `.vki`, not a Varve `HashMap`.

## Scalar Append And The Bounded Sidecar Transaction

Single-record scalar `push_*` calls do not commit a sidecar transaction per
record. A stream writer keeps one open sidecar transaction and commits it only
at a chunk boundary — the configured transaction bound (`DiskIndexOptions::batch`
/ `StreamOptions`, derived from `max_records` and `max_bytes`) — or on an
explicit `flush()`/`sync()`. Several scalar appends therefore share one sidecar
commit rather than forcing one durable redb transaction each.

This means the sidecar intentionally lags the native file between commits: the
native records are the authority, and the sidecar carries only bounded resume
state that is republished cleanly at `sync()`. `sync()` finishes the open sidecar
chunk, `sync_all`s the native file, then durably publishes the clean sidecar root
(redb `Durability::Immediate`) and drops the rollback savepoint. A crash between
commits loses only unsynced resume state, which `restore_*` or a scan-based
`bootstrap`/`rebuild` reconstructs; committed native records are never lost by
the deferred sidecar commit. Block-tail maintenance for `keyed_offset_chain`
formats uses binary search over the sorted tail vector and collapses each chunk
to one final tail per block, so tail upkeep is `O(log blocks)` per record rather
than linear.

## Historical Distinct Keys And Reclaim

`historical_distinct_keys()` is exposed on `DiskIndexStore`, `DiskIndexSnapshot`,
`VarveIndexedReader`, and `VarveIndexedWriter`. It reports the sidecar capacity
metric: the number of distinct keys the `.vki` has *ever* held (`K-ever`), not
the current live-key count.

A tombstone does not delete its redb row; it replaces the latest-table value.
Rebuild re-creates rows from the native log. So `K-ever` is monotonic across
insert/delete cycles and rebuilds, and the sidecar's logical cardinality grows
with distinct keys ever seen even when live keys return to zero. Deleting
tombstone rows alone is unsound: a later rebuild would resurrect them, and
without the tombstone row an older put earlier in the log could incorrectly win
a latest-key lookup. Reclaiming space must therefore be a paired operation under
the writer lock — compact the native log (drop dead put/tombstone records), then
rebuild the sidecar from the compacted log, publishing native-first then sidecar
and relying on the primary-identity change to fence stale readers. Use
`historical_distinct_keys()` to decide when that reclaim is worth running.

## Durability And Recovery

`flush()` flushes the native file handle but does not publish a clean scalable
generation. `sync()` is the clean publication boundary:

1. finish the bounded sidecar update;
2. flush and `sync_all` the authoritative native file;
3. publish the clean sidecar root and delete its rollback savepoint.

After any append, dropping the writer without a successful `sync()` leaves a
dirty generation intentionally. Ordinary open reports that state and never
scans or repairs silently.

Use the explicit generated operations:

| Situation | Operation | Native scan |
| --- | --- | --- |
| dirty `.vks` | `restore_stream_writer` | no; restore savepoint and truncate |
| dirty `.vki` | `restore_indexed_writer` | no; restore savepoint and truncate |
| native file has no `.vks` | `bootstrap_stream_checkpoint` | yes |
| native file has no/stale `.vki` | `rebuild_disk_index` | yes |
| eager integrity check | reader `verify_all` | yes |

Bootstrap and rebuild use the runtime `ResourceLimits` carried by their
options. They return scanned record/byte counts. Their `*_with_progress`
variants expose synchronous progress and cooperative cancellation.
`bootstrap_stream_checkpoint` refuses to overwrite any existing `.vks`.
`rebuild_disk_index` may replace a missing or clean stale `.vki`, but refuses a
dirty `.vki`; restore that generation first. Thus an unsynced physical tail
cannot be promoted to clean merely by invoking a scan operation.

Sidecar protocol failures retain their public `DiskIndexError` source inside
`Error::DiskIndex`; backend lock contention is `Error::IndexBusy`. Callers can
distinguish dirty, stale, identity, plan, metadata, and I/O failures without
parsing error text.

Atomic sidecar publication syncs the parent directory on Unix. On Windows it
requests a directory flush after `ReplaceFileW`/rename; filesystems that reject
directory `FlushFileBuffers` keep the completed atomic replacement instead of
turning it into a false write failure.

### Long Scan Progress And Cancellation

`verify_all_with_progress`, `bootstrap_stream_checkpoint_with_progress`, and
`rebuild_disk_index_with_progress` use the same scan-control types:

```rust
use std::num::NonZeroU64;
use varve::{
    ScanCancellationToken, ScanOptions, ScanProgressOptions, ScanProgressPhase,
};

let cancellation = ScanCancellationToken::new();
let scan = ScanOptions {
    progress: ScanProgressOptions {
        every_records: NonZeroU64::new(100_000),
        every_bytes: NonZeroU64::new(64 * 1024 * 1024),
    },
    cancellation: Some(&cancellation),
};

let records = reader.verify_all_with_progress(scan, |progress| {
    if progress.phase == ScanProgressPhase::Running {
        eprintln!(
            "{} records, {} / {} bytes",
            progress.records,
            progress.current_offset,
            progress.snapshot_len,
        );
    }
})?;
# Ok::<(), varve::Error>(())
```

The default callback cadence is every 16,384 records or 16 MiB, whichever is
reached first. Setting both cadence fields to `None` emits only `Started` and
`Complete`. `current_offset` and `snapshot_len` are absolute native offsets;
`scanned_bytes` excludes the file header. Values are monotonic and callbacks
run on the scanning thread. Display absolute completion with
`current_offset / snapshot_len`; use `scanned_bytes` for scan-throughput
accounting rather than mixing the two coordinate systems.

Cancellation is checked before scanning, after every completed record, after
every callback, and after the final `Complete` callback. It returns
`Error::ScanCancelled { progress }`. A cancellation observed before that final
check publishes no `.vks`/`.vki`; a request arriving afterward may race with a
successful publication. A cancelled rebuild leaves the previous `.vki`
byte-for-byte unchanged.

### Clearing A Stale Writer Lock

Default create/open/recovery remains single-writer and refuses any existing
lock. After independently establishing that the recorded process has exited,
the generated format exposes an O(1) lock-only recovery operation:

```rust
use varve::WriterLockBreakPolicy;

Capture::clear_stale_writer_lock(
    "capture.varve",
    WriterLockBreakPolicy::BreakIfProcessAbsent,
)?;
# Ok::<(), varve::Error>(())
```

This operation does not open or scan the native data file. Every cooperating
writer first acquires an OS-level exclusive byte-range lock outside the bounded
metadata region. Recovery must acquire the same guard before it inspects or
clears stale metadata, so an active or concurrent writer cannot be displaced.
`Refuse` remains the default. Age-only policies are explicit operator choices,
but they are evaluated only after the exclusive guard is held; prefer
`BreakIfProcessAbsent` when process state is available.

Clean close and successful recovery clear the metadata while holding the
guard. A zero-length `.lock` file may remain as the stable filesystem identity
used by future guard acquisitions; `inspect_writer_lock()` reports it as no
active or stale marker.

## Shared In-Process Indexed Handles

Indexed handles for the same file share one backing redb database through a
process-local registry keyed by native file identity (Windows volume + file
index, Unix device + inode). Independent readers of the same `.vki` now coexist
in one process instead of the second open failing, and a reader can be opened
beside a writer. A writer's uncommitted batch holds a write gate; new handles
opened while that gate is held observe a typed `Error::IndexBusy` rather than
blocking inside redb. After the batch commits, fresh handles proceed. When a
rebuild republishes the sidecar it invalidates the registry entry so later
handles bind to the new database.

This coordination is process-local. Cross-process exclusivity is unchanged: a
`.vki` remains single-process for writing, and the native-object writer lock and
sidecar identity checks fence other processes. An identity re-probe after open
closes the replace race so a handle never keeps serving a superseded database.

## Checked I/O Boundary

Sidecar offsets and lengths are untrusted. They become private-field
`FileOffset`, `ByteLength`, `SnapshotBounds`, and validated record-pointer
types before positional I/O or allocation. Validation uses checked `u64`
arithmetic and rejects overflow, ranges past the pinned EOF, impossible
framing, block/version/sequence mismatch, noncanonical flags, and checksum
mismatch where the format declares integrity.

This prevents a malicious file from turning a claimed extent into an unchecked
slice or claim-sized allocation. It does not authenticate a file. CRC is an
integrity/error-detection policy, not a cryptographic authenticity mechanism.

## Current Regression Baseline

On the Windows development host on 2026-07-17, the release-mode million unique
composite-key probe reported:

| Measurement | Result |
| --- | ---: |
| append | 4.072 s, about 245,600 records/s |
| native `write_all` calls | 62 |
| `sync()` | 116.2 ms |
| clean indexed reopen | 9.35 ms |
| 10,000 point lookups | 141.5 ms |
| allocator peak delta | 12,346,781 bytes |

These are regression values for one machine, not cross-platform guarantees.
Run the probe with:

```powershell
cargo test --release -p varve --features high-cardinality-dev `
  --test high_cardinality million_unique_keys_keep_varve_resident_maps_empty `
  -- --ignored --nocapture
```
