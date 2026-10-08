# Scalable Stream And Disk-Index I/O

This guide covers the default stream/indexed public API. It is the
Varve path intended for very large append logs. Its normal open and append
costs do not grow with native file size or record count.

The resident `create_writer`, `open_reader`, `blocks`, and `keyed_blocks`
APIs remain useful for bounded files. By default they scan the file and retain a
16-byte directory slot per record at open, and are not the petabyte-scale path.

Opt-in policies narrow that gap without closing it. `segment_on_flush` makes
open frame one record per commit point instead of one per record;
`open_digest_on_flush` plus `VarveFile::open_readonly_lazy` makes open frame one
record and retain **nothing**, leaving the caller to build only the part of the
index its question needs with `record_map`. Measured on a 50,000-record file:
100,316 read syscalls at open by default, 516 chained, 20 from a digest.

What that does *not* give you is this family's other two properties. A resident
open still has no bounded-memory keyed merge or compact, and a `record_map` walk
is still a forward walk of the record chain — `O(records before the answer)`,
not a keyed lookup. Choose this family when key cardinality is the problem;
choose the digest when open cost is.

> **Status (0.10.2).** Stream/indexed handles, disk-index
> plans, finite keys and scan control are part of the default public API, also
> with `--no-default-features`. Remove `high-cardinality-dev` from Cargo manifests.
> Repository release and long-duration load qualification are tracked separately.
> A release tag does not establish a completed endurance test.
> Explicit `compact_index()` reclaims obsolete companion pages; whole-native-log
> keyed merge/compact still uses resident key maps. The historical load and
> sparse-offset measurements apply to their recorded revisions and filesystems.
> See [Known Limitations §3](known-limitations.md#3-streamindexed-api-status)
> and [§6](known-limitations.md#6-not-verified) for the remaining scope.

The [self-check guide](self-check-guide.md#scalable-io-validation) gives commands
for model histories, concurrent snapshots, external-kill recovery, memory budgets
and actual-file write/read/delete validation. Passing a bounded campaign does
not establish power-loss safety or long-duration endurance.

## Declare The Policy

```rust
use varve::varve_format;

varve_format! {
    pub format Capture {
        magic: b"CAPT";
        version: 1;
        schema_hash: computed;
        index: keyed_offset_chain;

        blocks {
            fixed Run(id = 1) {
                id: u64,
            }

            variable Frame(id = 2, key = [scan, frame], key_index = disk,
                key_domain = [scan = 0..4, frame = 0..16]) {
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

### Finite schema keys

A key identifies a **declared value combination**, not a record and not merely
its block type. Declare either named combinations or a Cartesian domain:

```rust
fixed Frame(id = 2, key = [scan, frame], key_index = disk,
    key_values = [Scan1Frame2 = (1, 2), Scan2Frame4 = (2, 4)]) {
    scan: u32, frame: u32, payload: u64,
}
// Alternative: 4 * 16 = 64 permitted combinations.
fixed Sample(id = 3, key = [scan, frame], key_index = disk,
    key_domain = [scan = 0..4, frame = 0..16]) {
    scan: u32, frame: u32, payload: u64,
}
```

The enclosing format must use `schema_hash: computed;`. The macro generates
`FrameKey::{Scan1Frame2, Scan2Frame4}` and a block with **`key: FrameKey`** and
`payload: u64`. The declared `scan` and `frame` fields become that one key field;
their raw values are not repeated in the record or the point index. The generated
key occupies the first removed key field's position and field ID; other field
IDs are preserved. A non-key field named `key` is therefore rejected.

```rust
let key = FrameKey::from_values((1, 2))?;
let record = Frame { key, payload: 17 };
writer.push_frame(&record)?;
writer.sync()?;
let latest = reader.get_frame(&FrameKey::Scan1Frame2)?;
assert_eq!(key.values(), (1, 2));
assert!(FrameKey::from_values((1, 4)).is_err());
```

Both forms generate `COUNT`, `ALL`, `code()`, `from_code()`, `from_values()` and
`values()`. Direct lists produce the supplied variant names. Cartesian domains
produce `K0`, `K1`, etc.; use `from_values()` to avoid depending on those names.
The last field varies fastest, and all fields must be listed in key declaration
order. Lists retain their declared order; ranges support `a..b` and `a..=b`.

The hard limit is **4,096 combinations per block**, checked before multiplying
out a Cartesian product. Every code has a fixed **u16 / 2-byte** representation,
even for a small enum. This bounds generated-code/compiler cost without
introducing width transitions. It is a policy limit, not the u16 numeric limit.
A 4,096-variant integration test compiles the actual enum and checks every
combination. The initial incremental dev test build took 3.26 s with a maximum
child-process RSS of about 640 MiB; dependencies were already built. This is
one environment measurement, not a universal compile-time guarantee.

Supported domain fields are `u8/u16/u32/u64`, `i8/i16/i32/i64`, `bool`, and
`String` (literal labels; conversion takes `&str`). Type aliases, expressions,
128-bit integers, and runtime-sized domains are not accepted by this syntax.
Empty domains, duplicates, wrong tuple arity, out-of-range literals, defaulted
key fields, and oversized products are compile errors. Invalid input combinations
and unknown on-disk codes return errors; they never extend a dictionary or create
an enum variant. Public block construction takes a valid enum, so there is no
unchecked raw tuple hiding in a constructed block.

Codes follow declaration order. Reordering or changing the value/code mapping
changes the key codec identity, block fingerprint, and computed format hash.
Opening a file with a reassigned mapping is rejected. Schema changes require an
explicit migration/new file; mappings are not silently reconciled at runtime.
Renaming a variant alone does not change its on-disk meaning.

The API's encoded lookup key is `[present=1: u8][block ID: u32][enum code: u16]`
(7 bytes), but finite keys are **not stored in B-tree leaves**. The schema block
selects a fixed table; `code / 64` selects an immutable slot chunk and `code % 64`
selects its row. Neither the enum code nor the block ID is repeated in each row.
Each row occupies 40 bytes: the native record offset plus sequence, extent,
deletion state and integrity information needed to validate the target record.
An unused row is all zero. A full chunk contains 64 rows plus a 4-byte CRC.
`DiskIndexKeyTag` remains the API presence/block discriminator, not the value enum.

These tables are persistent extents in the `.vki` sidecar, not temporary files
or OS memory pages. Each reader caches chunks privately within its configured
`DiskIndexOptions.cache_bytes` budget. CRC is checked when a chunk enters that
cache; only the requested row is decoded for a lookup. An already known native
record offset does not need key lookup at all.

There is no runtime value dictionary, registration API, or shared cache added
for finite keys. A block's distinct key rows cannot exceed its declared domain.
Repeated writes update the latest offset for the same code; the previous-record
chain and sequential scan retain the distinct records. Use unkeyed blocks and
record offsets for ever-growing record identities. The sole writer updates its
own fixed arrays in memory. Small internal batches and `flush()` do not publish
new slot extents. At the caller's `sync()`/Immediate boundary, only changed
64-slot chunks are appended, followed by a flat block/chunk directory and the
confirmed checkpoint. Old readers retain the old directory and extents; `follow()`
adopts a new confirmed generation. No reader mutex or writer wait was added.
Append-only chunk versions and checkpoints still accumulate until explicit
`compact_index()`; the finite key bound is not a bound on total file size.

The raw `key = [fields]` form without `key_values` or `key_domain` remains an
**unrestricted key API** for existing generic storage tests and applications
that explicitly need that model. It uses the generic COW B-tree and does not
provide the finite-key guarantee. Both kinds can coexist in one format.
The finite enum facility and disk-index APIs are both included by default. The mistaken `<Format>KeyKind` enum has been removed.

The sidecar container is now `VARVEIX5`, with `VIXROOT4` checkpoints. Old sidecars
require explicit rebuild; native record encoding has not changed in this storage
revision. Finite keys address fixed slots directly; small internal batches do
not publish new slot versions. Only changed slot chunks and their directory
are appended at a confirmed sync boundary. File growth therefore depends on
changed chunks and sync frequency, not just the number of enum variants.

Generated non-keyed block methods continue to work in a
`keyed_offset_chain` format. Generated `VarveBlock` implementations carry an
`IS_KEYED` type fact so the runtime rejects chain-unsafe low-level batch calls
without rescanning the file.

Manual `VarveBlock` implementations
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
    (0..1_000_000u32).map(|sequence| Frame {
        key: FrameKey::from_values((sequence % 4, (sequence / 4) % 16)).unwrap(),
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

let frame = reader.get_frame(&FrameKey::from_values((2, 3))?)?;
let all_runs = reader.runs()?;       // explicit lazy native scan
let all_events = reader.events()?;  // explicit lazy native scan
let verified = reader.verify_all()?; // explicit eager full validation
# Ok::<(), varve::Error>(())
```

`get_frame` performs one B-tree lookup and one bounded positional native
record read. Opening the reader does not *scan* the native record region, but it
does read a bounded prefix of it: the primary-generation witness digests the
leading window of the file — at most 4 KiB from offset 0 — and takes one point
read of the leading record for the creation nonce. No scanner is created at
open. Calling `runs`, `frames`, `events`, or `verify_all` explicitly creates a
native scanner; their cost is linear in the selected snapshot.

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

`DiskIndexOptions::batch` independently bounds one native COW page-update batch.
Native chunk boundaries no longer force an index transaction commit. Ordinary
sidecar batches append working pages without a durability barrier; successful `sync()` / `immediate()`
publishes the durable clean generation after syncing the native file. Blocks
can require Immediate, and callers can declare automatic conditions; see
[Immediate policy](immediate-policy.md).
`cache_bytes` bounds each reader's private index cache and `max_key_bytes` bounds an encoded key.
None of these values is a total file-size, total record-count, or lifetime
append limit.

The paged `.vks`/`.vki` backing file is deliberately not rejected by
`ResourceLimits::max_sidecar_len`: opening it does not materialize its full
length. That limit still applies to Varve sidecar formats that are read into
memory, such as matrix/adapter envelopes. Scalable sidecar safety instead
comes from bounded native pages and private caches, fixed metadata rows, bounded key rows,
and checked native extents.

The runtime retains declared block tails and bounded buffers, not one RAM entry
per record or key. Key cardinality grows `.vki`, not a Varve `HashMap`.

## Scalar Append And The Bounded Sidecar Transaction

Single-record scalar `push_*` calls do not commit a sidecar transaction per
record. A stream writer keeps one open sidecar transaction and commits it only
at a chunk boundary — the configured transaction bound (`DiskIndexOptions::batch`
/ `StreamOptions`, derived from `max_records` and `max_bytes`) — or on an
explicit `flush()`/`sync()`. Several scalar appends therefore share one sidecar
commit rather than forcing one durable index publication each.

This means the sidecar intentionally lags the native file between commits: the
native records are the authority, and the sidecar carries only bounded resume
state that is republished cleanly at `sync()`. `sync()` finishes the open sidecar
chunk, `sync_all`s the native file, then durably publishes the clean sidecar root
and replaces the rollback checkpoint with a confirmed checkpoint. A crash between
commits does not advance the acknowledged generation. `restore_*` truncates
the unsynced suffix back to the last successful durability boundary; a
non-durable index commit is not an application acknowledgment. The first
mutation of a generation still durably establishes its rollback checkpoint.
Block-tail maintenance for `keyed_offset_chain`
formats uses binary search over the sorted tail vector and collapses each chunk
to one final tail per block, so tail upkeep is `O(log blocks)` per record rather
than linear.

## Historical Distinct Keys And Reclaim

`historical_distinct_keys()` is exposed on `DiskIndexStore`, `DiskIndexSnapshot`,
`VarveIndexedReader`, and `VarveIndexedWriter`. It reports the sidecar capacity
metric: the number of distinct keys the `.vki` has *ever* held (`K-ever`), not
the current live-key count.

A tombstone does not delete its native index entry; it replaces the latest-table value.
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

### Explicit sidecar compaction

`writer.compact_index()` requires an already clean writer. Call `sync()` or
`immediate()` explicitly beforehand; compaction refuses Dirty state and never
inserts an automatic Immediate boundary. It copies current keys and tombstones
into a bounded-memory temporary file, syncs it, atomically replaces the sidecar,
and syncs the parent directory. `IndexCompaction` reports before/after logical
bytes and historical distinct keys. Old reader file descriptors remain valid;
`follow()` can adopt the replacement and subsequent writer generations.

This reclaims obsolete *index pages*, not historical keys or native log records.
Without explicit compaction, append-only index space grows with changed pages
and checkpoints. Small scattered batches can amplify that growth. A filesystem
may retain the old file's allocated space until the final old handle follows or closes;
compaction requires enough free space for both files while it runs. A
`PublishedButParentSyncPending` error means the replacement is visible but its
name is not yet durably acknowledged. Compaction errors conservatively poison
the writer; close and reopen/reconcile before more writes.

## Durability And Recovery

`flush()` flushes the native file handle but does not publish a clean scalable
generation. `sync()` is the clean publication boundary:

1. finish the bounded sidecar update;
2. flush and `sync_all` the authoritative native file;
3. publish the clean sidecar root and delete its rollback checkpoint.

After any append, dropping the writer without a successful `sync()` leaves a
dirty generation intentionally. Writer open requires explicit recovery. Reader
open serves the saved confirmed root and EOF without recovering or scanning the
unfinished suffix.

Use the explicit generated operations:

| Situation | Operation | Native scan |
| --- | --- | --- |
| dirty `.vks` | `restore_stream_writer` | no; restore checkpoint and truncate |
| dirty `.vki` | `restore_indexed_writer` | no; restore checkpoint and truncate |
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

### Primary Generation Binding

A sidecar is bound to one logical *generation* of its primary, not merely to one
pathname or one OS file object. Every `VarveStreamWriter::create` /
`VarveIndexedWriter::create` stamps a 128-bit random nonce as the primary's
first record, under the reserved internal block id `CREATION_NONCE_BLOCK_ID`.
The sidecar records a primary-generation witness that folds that nonce together
with the primary's length and a checksum over its bounded leading window (4 KiB;
frozen once the primary has grown past it, so appends do no witness work).

Every stream/indexed writer open, reader snapshot open, and checkpoint restore
recomputes the witness and refuses a mismatch with
`DiskIndexError::PrimaryGenerationMismatch`. This is what makes an in-place
rewrite of a primary by another equal-length primary of the same format — same
path, same OS object, same header bytes, same schema hash — a refusal rather
than an accepted stale generation. The recovery is `rebuild_disk_index`, never
"trust the sidecar".

One byte range of the primary is excluded from that window, and it is the only
part of a file that may change without being a new generation: a format that
declares `index: header_tails` reserves a region inside the file header which a
commit rewrites in place, and the witness blanks that region's *payload* before
checksumming. Its magic and its declared length stay covered, so a region that
changed size — the change that moves the end of the header and every record
offset after it — is still a refusal. For a format that does not declare the
region, the window is exactly what it was.

The nonce costs one record at create and nothing per append. Reading it is one
bounded point read of the leading record, performed only at create and open. A
primary that carries no nonce — a legacy file, or one bootstrapped from a
resident `VarveFile` — is reported as "no nonce" rather than as an error, which
is fail-closed: a primary that *was* created with one recorded a witness that
folds it in, so answering "none" can only produce a different witness and refuse
the sidecar.

Sidecar metadata is at record version 3. A version 2 sidecar is 260 bytes where
the v3 record is 300, so it is refused with the typed
`DiskIndexError::MetadataLength` before the version field is read, and the
disk-index plan digest domain was bumped alongside it, so plan digests published
before this change are refused as stale. Both are rebuild-and-regenerate
conditions under the pre-1.0 wire policy; neither is migrated in place.

### Disk-Index Descriptor Identity

A `DiskIndexDescriptor` records the block schema fingerprint of the concrete
type whose decode and key-extraction function pointers it captured, and that
fingerprint is folded into the plan digest. Plan construction and plan
validation run the format's block-registration gate once per descriptor —
before any primary bytes can reach a captured codec — so a descriptor that
matches a declared block's id and version while being a different type is
refused with `Error::BlockSchemaFingerprintMismatch` without a single decoder
call. Descriptor validation stays off every per-record and per-lookup path: it
runs at plan construction/validation only, and repeat validation short-circuits
on the spec's block and identity table identity.

Atomic sidecar publication syncs the parent directory after the
`ReplaceFileW`/rename (the Windows directory handle is opened with the write
access `FlushFileBuffers` requires). A parent-sync failure is never silently
promoted to full durability: sidecar create, stream bootstrap, and disk-index
rebuild surface `Error::PublishedButParentSyncPending` while keeping the
completed atomic replacement in place, so the published sidecar remains usable
and the pending state is a durability warning, not a rollback.

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

## Per-thread readers and private caches

Each stream/indexed, resident and matrix reader belongs to one reading thread:
these handles and generated wrappers are `Send` but not `Sync`. Moving ownership to a worker is
supported; sharing one reader through `Arc` or `&Reader` across threads fails to
compile. Open a fresh reader on the same path for each reading thread. The
underlying files are common; each reader's caches and cursor state remain
independent. There is no implicit reader clone or shared-reader mutex.

Matrix bitmap caches also belong to each reader and use local `RefCell` access,
without a cache mutex. Configure them at open using
`ReadLimits::with_matrix_metadata_residency(MatrixMetadataResidency::Lazy {
cache_bytes })`. The default is 2 MiB per bitmap (clamped by the bitmap limit),
allocated on demand. Total memory adds across bitmaps and readers. This changes
ownership, not matrix generation visibility; see
[concurrency limitations](known-limitations.md#42-where-a-reader-gets-a-snapshot-rather-than-live-state).

Ordinary queries (`get`, `lookup`, `blocks`, `events`, `verify_all`) take `&self`.
`follow(&mut self)` updates only that handle's confirmed generation, and iterator
`next(&mut self)` advances only its cursor. Neither requires a writable primary
file or a writer transaction.

Matrix follow compares immutable COW roots and skips equal subtrees. It patches
changed bitmap index entries and invalidates only affected bitmap cache pages;
under default verification, only changed metadata pages are reverified. The
persisted `compaction_epoch` changes only on explicit `compact_matrix()`. A new
epoch selects a full metadata rebuild; ordinary syncs preserve it so readers
that skip generations still detect replacement. Follow never compacts by itself.


Each handle opens the sidecar read-only and each snapshot owns its bounded page
cache. A sole writer appends immutable COW tree pages and checkpoint records.
Two alternating confirmed heads are separate from the working heads and durable
Dirty marker. Readers validate CRC-protected confirmed heads, then read only the
root and native EOF they name. Unconfirmed pages never replace a reader's root.
Reader open performs no write, transaction creation, OS lock or writer admission.
The same contract works across threads and processes on a coherent local filesystem.

`follow(&mut self)` adopts the latest confirmed generation and returns the
number of additional native records (including internal records and tombstones).
It captures the index first, then validates EOF and the generation witness on
the retained native file object before replacing the handle's state. When the
writer is still Dirty, follow serves its last confirmed generation. A failed
follow leaves the reader unchanged. A sidecar-only `compact_index()` replacement is followed after identity and
frontier validation. Replacement of the native file itself is not followed.

Existing iterators keep their captured view. `follow_events(&mut events)` and
`follow_blocks(&mut blocks)` extend a cursor created by the same reader and
resume from its existing offset, including after EOF. They preserve sequence
validation and cumulative read limits, without reopening or scanning old records.
A cursor that returned an error cannot be resumed. A separately opened reader's
cursor is rejected even if it names the same path.

```rust,ignore
let mut reader = MyFormat::open_indexed_reader(path, options)?;
let mut cursor = reader.items()?;
loop {
    for item in cursor.by_ref() {
        consume(item?);
    }
    wait_for_application_notification();
    reader.follow_blocks(&mut cursor)?;
}
```

The single writer holds nonblocking native/sidecar OS writer guards. These
exclude a second writer; readers never acquire them. The store owns its pending
write batch and caches. Publication and restore require exclusive mutable
access; no atomic admission gate or cache-transfer queue is involved. No reader is required to exit, release or acknowledge a
checkpoint before the writer can append or publish.

Confirmed roots carry the native EOF and immutable index offsets. Readers
validate a published checkpoint and never read the writer's unconfirmed tail.
Native data is synced before a durable confirmed root is published.
Old redb sidecars require rebuild/bootstrap;
there is deliberately no compatibility decoder.

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
cargo test --release -p varve `
  --test high_cardinality million_unique_keys_keep_varve_resident_maps_empty `
  -- --ignored --nocapture
```

## Snapshot retention management

Stream/indexed readers expose `snapshot_status()` with the adopted and latest
confirmed generations, record/byte lag, and whether their backend snapshot is
pinned. Each reader owns its file snapshot; there is no process-global reader
registry or `snapshot_retention()` API. Applications that need a reader count or
oldest active generation track their own reader lifetimes and reported status.
Old open files remain readable after compaction until their handles are dropped.
`IndexCompaction` reports the old and new logical sidecar sizes.

Call `release_snapshot(&mut self)` on an idle reader to drop its backend page
reference immediately. It returns true once and false if already released.
Indexed key reads then return `Error::DiskIndex(DiskIndexError::SnapshotReleased)`.
Native scans and existing cursors retain their confirmed EOF and remain usable;
they do not pin sidecar pages. `follow()` reacquires the latest confirmed snapshot,
even if its generation is unchanged or the writer is Dirty. On failure, the
reader remains released and its previous native frontier is unchanged.

A typical policy is to report lag, ask the owning reader thread to `follow()`
when it may advance, or release its snapshot while idle. The writer can observe
all pins but cannot forcibly advance or invalidate another thread's snapshot.
Dropping or following a reader releases its old snapshot pin. Open reader
handles can retain an old file descriptor even after releasing a snapshot. Follow or close
the old handle to release its file descriptor after file-generation compaction.
None of these APIs commits, fsyncs, changes Immediate conditions, or waits for
readers to catch up.

### Independent reader startup

Native reader opens do not compete for writer admission or return `IndexBusy`
for a live writer. If an external replacement or unsupported filesystem reports
a transient sharing/open error, retry with a bounded backoff/deadline on the
reader thread. Never gate writer progress on reader startup or retries.

Event and typed cursors are local to their creating thread (`!Send + !Sync`).
Move a `Send + !Sync` reader first, then create its cursors on the reading thread.
