# Varve API Reference

This is a practical public API map for application authors. It is not a
rustdoc replacement; it explains which API to reach for and what it means.

## Generated Format API

For a format declared as:

```rust
varve_format! {
    pub format AppFormat {
        magic: b"APP";
        version: 1;
        blocks {
            fixed Point(id = 1) { x: u32, y: u32 }
            variable User(id = 2, key = [id]) { id: u64, name: String }
        }
    }
}
```

the macro generates:

| Item | Purpose |
| --- | --- |
| `AppFormat` | zero-sized namespace for the format |
| `AppFormat::spec()` | returns the static `FormatSpec` |
| `AppFormat::create(path)` | create/truncate and return raw `VarveFile` |
| `AppFormat::create_writer(path)` | create/truncate and return typed writer |
| `AppFormat::open(path)` | open read-write raw `VarveFile` |
| `AppFormat::open_writer(path)` | open read-write typed writer |
| `AppFormat::open_readonly(path)` | open raw read-only `VarveFile` |
| `AppFormat::open_reader(path)` | open typed snapshot reader |
| `AppFormat::open_reader_with_limits(path, limits)` | open with limits tightened below the declaration |
| `AppFormat::open_reader_with_resource_limits(path, limits)` | open with runtime policy that may raise or lower optional defaults |
| `AppFormat::open_reader_trusted_unbounded(path)` | explicit trusted-input open; never called by ordinary open |
| `AppFormat::open_writer_with_limits(path, limits)` | writer open with field-wise tighter limits |
| `AppFormat::open_writer_with_resource_limits(path, limits)` | writer open with runtime resource-policy override |
| `AppFormat::open_writer_trusted_unbounded(path)` | explicit trusted-input writer open |
| `AppFormat::open_recover(path)` | explicit recovery open |
| `AppFormat::open_recover_with_report(path)` | recovery open plus report |
| `AppFormat::diagnostics()` | static format diagnostics |
| `AppFormat::diagnose_file(path)` | read-only file diagnostics |
| `AppFormat::self_test(path)` | build an end-to-end self-test |

`AppFormat::create` and `create_writer` create-or-truncate the path. For callers
that must prove they created and own the file (diagnostic scaffolding, self-test
scaffolding) and must never destroy caller data, `VarveFile::create_new(spec, path)`
and `VarveWriter::create_new(spec, path)` open with `create_new`: they never
truncate or reuse an existing path and fail with an `AlreadyExists` I/O error if
one exists. Matrix formats have the same pair —
`VarveFile::create_new_with_dims(spec, path, dims)` and
`VarveWriter::create_new_with_dims` — which keep the one exclusively created,
lock-bound handle from claim through matrix initialization, with no pathname
re-open window in between.

Generated typed methods depend on block names:

| Declaration | Writer method | Reader method |
| --- | --- | --- |
| `fixed Point` | `push_point(&Point)`, `replace_point(index, &Point)` | `points() -> BlockVec<Point>` |
| `variable User(key=[id])` | `push_user(&User)`, `replace_user(index, &User)`, `delete_user(&key)` | `users() -> KeyedBlockVec<K, User>` |
| `matrix Cell` | `write_cell(key, &Cell)`, `commit_cell(key)` | `cell(key)`, `cell_status(key)` |
| `aux { thumbnail: 16 }` | `write_thumbnail_aux(offset, bytes)` | `read_thumbnail_aux(offset, len)` |

## FormatSpec

`FormatSpec` is the runtime registry and file contract.

| API | Use |
| --- | --- |
| `FormatSpec::new(...)` | manual/derive-first spec construction |
| `FormatSpec::builder()` | builder-style construction |
| `with_extension(ext)` | recommended file extension metadata |
| `with_commit_policy(policy)` | append-log commit policy |
| `with_compression_policy(policy)` | global variable-block compression |
| `with_block_compression(descriptors)` | per-variable-block record-explicit compression |
| `with_computed_schema_hash()` | pin computed schema hash into the header contract |
| `with_read_limits(limits)` | set declaration-level resource policy without changing schema identity |
| `with_resource_limits(limits)` | resolve standard/default policy and overlay runtime values |
| `tighten_read_limits(limits)` | component-wise meet; a finite ceiling can never be widened |
| `with_matrix_spec(dims, commits, blocks)` | manual matrix registry |
| `with_matrix_aux(aux)` | manual matrix aux registry |
| `with_block_identities(identities)` | attach the per-block identity table (endian override, keyedness, generated codec fingerprint) hashed by `computed_schema_hash()`; generated `spec()` does this automatically |
| `validate()` | check static spec consistency, including duplicate/unregistered block identities |
| `computed_schema_hash()` | deterministic schema fingerprint (algorithm version `FormatSpec::SCHEMA_HASH_ALGORITHM_VERSION`, currently 3) |
| `effective_layout()` | physical layout plan using header/segment/lead-in/raw/footer vocabulary |
| `inspect_layout_file(path)` | validate a native or custom file and return physical layout ranges |
| `schema_debug_dump()` | human-readable schema dump |
| `diagnostics()` | structured static diagnostics |
| `diagnose_file(path)` | structured file diagnostics |
| `self_test(path)` | self-test builder |

Use the generated `Format::spec()` path unless you need derive-first or manual
registry construction.

The computed schema hash covers wire layout, not just field membership: fields
are hashed in declaration order with their encoding ordinal and a field-count
frame, and each block's endian override, keyedness, and generated codec
fingerprint are folded in through `block_identities`. Reordering same-typed
field declarations, changing a per-block endian, or changing a codec identity
therefore changes the hash. Omitting `schema_hash` in a declaration stores 0
and disables the open-time comparison; use `schema_hash: computed` (or pin a
literal derived from `computed_schema_hash()`) whenever schema locking is
wanted.

### ReadLimit And ReadLimits

`ReadLimit` is `Missing`, `Finite(u64)`, or `TrustedUnbounded`. `ReadLimits`
contains operational policy fields documented in the format declaration guide.
Missing fields are resolved when a handle opens. Ordinary
APIs return `Error::MissingResourceLimit` only for an explicitly unresolved policy and
`Error::TrustedUnboundedRequiresExplicitApi` when a trusted policy is presented
to an ordinary entrypoint. Values above a finite ceiling return
`Error::LimitExceeded` before a claim-sized allocation or read begins.

`ReadLimits::missing()` is a partial-overlay builder base,
`ReadLimits::finite_all(n)` sets every ceiling to `n`, and the const
`with_max_*` builders set individual finite values. `ReadLimits::STANDARD`
places no total ceiling on append-log file length, scan length, record count,
segment count, or cumulative index bytes; it limits one-shot payload decoding
and materialization. Compatibility `*_with_limits` methods perform a meet, so
`Finite(64 MiB)` tightened by `Finite(16 MiB)` is `Finite(16 MiB)`.
`*_with_resource_limits` overlays fields and may raise or lower defaults.

`ReadLimits::UNTRUSTED` (`ReadLimits::untrusted()`) is the finite companion to
`STANDARD` for input from untrusted sources. Every aggregate dimension that
`STANDARD` leaves effectively unbounded is finite: `max_records` 16,000,000,
`max_scan_bytes` 16 GiB, `max_index_bytes` 1 GiB,
`max_segments` 65,536, and `max_keyed_tail_bytes` 256 MiB, inheriting the
`STANDARD` per-item caps for everything else. Use it when a resident open must not let a hostile file choose the reader's
CPU, I/O, or memory; large trusted files should use the scalable APIs or explicit
wider limits instead.

### MatrixMetadataResidency

`ReadLimits::matrix_metadata_residency` declares how much of a matrix's
persisted commit metadata a reader keeps resident. It is set with
`ReadLimits::with_matrix_metadata_residency`, and it is a reader-side policy
only: no on-disk byte depends on it, so two processes may open the same file with
different bounds.

Residency is **always demand-filled and bounded**; the only thing left to declare
is the bound. `MatrixMetadataResidency::Lazy { cache_bytes }` reads the persisted
page index at open and nothing else — no page payload and no page digest. A
commit-map page is read, authenticated against its stored digest, and cached the
first time a bit inside it is addressed, and the least recently used cached page
is dropped when admitting another would exceed `cache_bytes` (rounded up to a
whole 4096-byte page, with a one-page floor).

The field's **unset** state is `MatrixMetadataResidency::Missing`, which is what
every `ReadLimits` constructor carries until someone declares a policy. It resolves
to `MatrixMetadataResidency::DEFAULT` via
`ReadLimits::effective_matrix_metadata_residency()` — read that rather than the
field, because it never yields `Missing`. `DEFAULT` is `Lazy` with
`DEFAULT_CACHE_BYTES` (2 MiB = 512 commit-map pages, derived as
`ReadLimits::STANDARD.max_matrix_bitmap_bytes / 32`), clamped down to whatever
`max_matrix_bitmap_bytes` is in force.

A **declared** `cache_bytes` is admitted against `max_matrix_bitmap_bytes` at
open, so the option cannot be used to raise a declared ceiling; the **derived**
default is clamped to that ceiling instead, so a default can never be the reason
an open fails. A live set larger than the ceiling is therefore *openable* rather
than refused — the ceiling bounds a cache, not an admission.

`cache_bytes` bounds **each bitmap, not each reader**: every commit category and
every block validity map owns a cache of its own, so a format with `n`
demand-loaded maps bounds itself at `n * cache_bytes`. The bound is still
independent of the file's size.

**0.5.0 removed `MatrixMetadataResidency::EagerVerified`.** It materialised every
published page for the whole session and made `max_matrix_bitmap_bytes` an
availability limit. What callers actually wanted from it — authenticating the
whole commit map, and the complete `MatrixRecoveryReport` that follows — was a
*side effect* of that load and is now its own policy,
[`MatrixMetadataVerification`](#matrixmetadataverification), which retains one
4096-byte buffer and is on by default. Nothing was left behind as an alias,
because a name promising eager residency for a demand-filled cache would be worse
than a compile error.

Three consequences are part of the residency declaration:

* A page is authenticated when it is faulted in: a page whose bytes disagree with
  its digest is reported as `Error::MatrixFatalCorruption` by the read that
  touches it, so no read ever answers from unverified bytes. Whether the *map as a
  whole* is checked, and when, is `MatrixMetadataVerification`.
* A page the persisted index does not name still reads as clear, and that is a
  fact the file supplied rather than a guess: the index is loaded in full at
  open and is authoritative for which pages hold state, so "not cached" and "not
  published" stay distinct. An index that cannot be enumerated in full is a fatal
  finding at open.
* A page's contents are as of the first touch that faulted it in, not as of
  open, so pages not yet faulted in have no snapshot pinned. A reader that needs
  one consistent instant across a whole map must coordinate that itself; no
  residency bound provides it, because one that pinned the whole live set would be
  the eager load this design removed.

### MatrixMetadataVerification

`ReadLimits::matrix_metadata_verification` declares **when** a matrix's commit
metadata is authenticated as a whole. It is set with
`ReadLimits::with_matrix_metadata_verification`, composes through
`overlay`/`tighten` exactly as the residency policy does, and resolves through
`ReadLimits::effective_matrix_metadata_verification()`.

The pass reads every page of every commit map named by the persisted page index
**unioned with** the platform's allocation map, authenticates each against its
stored digest, and folds the outcome into `MatrixRecoveryReport`. The union's
second term is what detects stray bytes in a page the matrix never published; the
first keeps the pass bounded where no allocation map is available. Cost:
`O(live pages + allocated pages)` bytes read, and one reusable 4096-byte page
buffer retained — nothing it reads becomes resident, at any matrix size.

What only verification produces:

* the `Recoverable` `MatrixCorruptionKind::CommitMap` finding for a category whose
  map disagrees with its digests;
* the whole-category quarantine behind `Error::MatrixCommitQuarantined`, which
  fails every access to that category closed — including a
  `RecoveryPolicy::Strict` writer — instead of refusing only the pages a reader
  happens to touch;
* the `RebuildCommitMap` / `ClearCategory` recommendations that make the
  documented recovery path reachable;
* **any examination at all of a page the persisted index does not name.** Nothing
  else ever reads such a page: a demand fault-in only reads pages the index names.
  With verification off, stray bytes written out of band into a page the matrix
  never published are not detected later — they are not looked at.

| Variant | Meaning |
| --- | --- |
| `Missing` | Unset. Resolves to `DEFAULT`. |
| `AtOpen` | Verify while opening. **`DEFAULT`.** A damaged matrix announces itself at open and a writer is refused before it can mutate a damaged category. |
| `OnDemand` | Do not verify while opening; verify when asked, with `verify_matrix_metadata()`. Open reads the persisted page index only and does not query the allocation map. |

`verify_matrix_metadata()` on a reader, writer, or `VarveFile` runs the same pass
on demand and returns the same report. It **reports; it does not quarantine**: the
fail-closed gate is derived once, at open, from the findings the layout is
assembled with, so a caller who needs a damaged category failed closed reopens
with `AtOpen`. Under `OnDemand` and without such a call, per-page authentication
at fault-in is the only verification that happens — which is why there is no
`Never` variant: turning verification off removes the verdict, never the check on
bytes a read actually answers from.

`max_keyed_tail_bytes` (declared as `keyed_tail` in a `limits { ... }` block)
bounds the resident keyed-tail cache: the per-block-id map that lets a keyed
append resolve its predecessor in O(1) instead of rescanning the resident
index. It is charged before the memory is taken, so the map's initial build and
its later growth are both refused with
`Error::LimitExceeded { resource: "keyed tail bytes", .. }` rather than merely
attempted.

Both halves matter, and the build is the larger one. The map is built from file
content by `VarveFile::key_tail_offsets`, at generated-writer construction and
at first resident keyed use, so a file with `N` distinct keys would otherwise
force an `N`-entry resident map whatever the ceiling said, with the ceiling only
refusing further growth within the session. The build is therefore charged as it
proceeds. Its charge is the structural **peak**, which is larger than the map it
produces: the build keeps a transient per-key ordering entry alive while the
returned map is reserved, and the resident cache additionally transcodes into a
canonical-payload map while the returned map is still alive. Opening a file
consequently charges more than the steady-state map costs. This is deliberate
and conservative - the charge models what is allocated, not what survives - but
it means a ceiling sized from the steady-state map alone can refuse an open.

The charged value is inline storage plus the key payload bytes the map owns.
Both routes charge the same way: the generated keyed writers no longer keep a
`HashMap<T::Key, u64>` of their own and instead maintain the same byte-keyed
resident cache the generic keyed API uses, so heap owned by a `String` key is
charged on every path. It is checked per keyed block id, not
summed across block ids, and it excludes `HashMap` control bytes and
load-factor slack. `STANDARD` leaves it at `u64::MAX`.

Two consequences of "per block id" are worth stating outright, because they
decide which API a format should use (P-01). A generated writer primes one such
map per keyed block type at construction, and each priming pass walks the
resident record index, so construction costs `Theta(M*N)` for `M` keyed block
types and `N` resident index entries. And because the ceiling binds each map
separately, the aggregate a single writer can retain is about `M` times
`max_keyed_tail_bytes` rather than that value once. Both are the documented
behaviour of the resident path, not limit violations — but they are the wrong
shape for a high-cardinality format. Declare `key_index = disk` on the keyed
blocks and use the generated disk-indexed writer/reader instead: that path
keeps key state in the redb sidecar, does no per-construction index walk, and
is the one the petabyte-scale contract covers. The generated rustdoc for each
typed writer now says the same thing on the type itself.

The allocation budget these limits drive is nominal accounting, not a hard peak
RSS guarantee. It counts logical/nominal bytes and does not fully account for
container bucket/node overhead, the extra `8 × N` for sequence-uniqueness
tracking, mmap index/map duplication, or auxiliary maps built during keyed
materialization. Checked arithmetic and fallible reservation still bound each
individual claim; a true peak-RSS ceiling belongs to an external
process/cgroup/job policy.

For omitted or explicit `preset: varve_native`, `effective_layout()` returns a
synthetic plan containing the native `VarveFileHeader` and repeated
`VarveRecord` segment. `VARVE3` formats expose the native `VarveRecordFooter`
there too. This keeps the native preset visible through the same physical
layout vocabulary used by custom layouts while preserving existing native file
bytes.

## Reader And Writer Handles

| Type | Role |
| --- | --- |
| `VarveReader` | read-only snapshot wrapper |
| `VarveWriter` | write-capable wrapper |
| `VarveFile` | lower-level read/write type used by both wrappers |

**This is the resident family, and it is not the petabyte-scale path.** Opening
one of these handles scans the file and builds a **resident record directory**
that lives for the life of the handle: `Theta(records + decoded bytes)` plus an
`O(N log N)` per-open sequence-uniqueness sort over `N` records (which degrades
to `Theta(N)` for a file a Varve writer produced; the `N log N` bound is the
guarantee for reordered or hostile input). Budget **16 bytes of resident
directory per record** — the record's offset and its committed bit, the two
facts that are not in the record itself. Everything else in a
`RecordIndexEntry` is rebuilt from the record's own header and footer when a
read asks for it. Measured across 2,000 → 20,000 records: 16.00 bytes per
record retained.

Two other ways to pay for that directory, both of which cost the handle
nothing: hold it yourself (`open_readonly_without_directory`, below), or build
only the part your question needs (`record_map`, below).

**Three policies avoid the open-time scan; two that look as though they might
do not.**

- `IndexPolicy::segment_on_flush` **does** avoid it. Each commit point appends an
  internal *segment* record covering the records that commit point added, chained
  to the previous one through the record footer's `prev_same_block_offset`. Open
  confirms a record footer at the end of the file and walks that chain backwards,
  reading one record per commit point and **no data record at all**. Measured on
  one Linux host with a commit point every 256 records: an 852 MB, 200,000-record
  file opens in 370 ms chained against 5,867 ms scanning, framing 782 records
  instead of 200,782. What it removes is the open-time walk, not the directory:
  the index it produces is the same one a scan produces, so the
  16-bytes-per-record figure above is unchanged. Its cost is on disk — a segment
  carries an index entry per record it covers, so the file grows with the
  record count (measured: +3.67 MB on a 31 MB, 50,000-record file). **A writing
  session must end
  with `flush` or `commit`** — a data record at the end of the file leaves open
  no chain to start from, and it silently scans instead. See [Known Limitations
  §2.1](known-limitations.md#21-varvefile-scans-the-whole-file-at-open-and-holds-a-record-index)
  for the full table, the other costs, and every fallback.
- `IndexPolicy::open_digest_on_flush` avoids it **completely**, by not building
  an index at all. See [An open that reads no
  record](#an-open-that-reads-no-record) below: `open_readonly_lazy` frames one
  record, the digest, and the caller builds whatever index it needs with
  `record_map`. This is the only one of the three whose on-disk cost does not
  grow with the record count.
- `IndexPolicy::CheckpointOnFlush` does **not** seed an open from a checkpoint.
  Every open without the segment chain calls `load_index` → `scan_records_from`,
  which walks from the header to the file length; a checkpoint met on the way is
  validated and its decoded entries are discarded. What the policy bounds is
  writer-side checkpoint bytes (it spaces full checkpoints geometrically), not
  open cost. `docs/spec.md` describes a checkpoint-seeded open as a design
  target; it is not implemented.
- The `scan_on_open` flag is **not** a behaviour switch — it is folded into the
  schema manifest and hash bytes and is consulted nowhere else in `varve-core`, so
  clearing it does not produce a non-scanning open.

`ReadLimits::STANDARD` leaves `max_records`, `max_index_bytes` and
`max_scan_bytes` at `u64::MAX`, so the default profile places no ceiling on
resident index size; use `ReadLimits::UNTRUSTED` for input you did not produce.
`max_file_len` is accepted and inert — nothing enforces a file-length ceiling;
see [Known Limitations](known-limitations.md).

The petabyte-scale path is [Scalable Stream And Indexed
Handles](#scalable-stream-and-indexed-handles) below, behind
`high-cardinality-dev`. `VarveReader` is a snapshot as of its own open: records
another handle appends afterwards are not visible without reopening. See
[Known Limitations §2.1](known-limitations.md#21-varvefile-scans-the-whole-file-at-open-and-holds-a-record-index).

### Reads that fill a buffer you own

Every read that returns a collection has an `_into` twin taking the buffer
instead. `out` is cleared and then filled, so a caller that reuses one buffer
across calls or across files allocates once and never again. The allocating form
stays as a thin wrapper — nothing breaks.

| allocating | caller-supplied |
| --- | --- |
| `index_entries()` | `index_entries_into(&mut Vec<RecordIndexEntry>)` |
| `all_metadata()` | `all_metadata_into(&mut Vec<(String, Vec<u8>)>)` |
| `blocks::<T>()` | `block_entries_into::<T>(&mut Vec<RecordIndexEntry>)` |
| — | `decode_blocks_into::<T>(&mut Vec<T>)` — decodes **every** matching record; bounded by your buffer, not by the file |
| `blocks_migrated::<F, T, M>()` | `blocks_migrated_into::<F, T, M>(&mut Vec<T>)` |
| `keyed_blocks::<T>()` | `keyed_blocks_into::<T>(&mut Vec<RecordIndexEntry>, &mut HashMap<..>)` |
| `materialized_keyed_blocks::<T>()` | `materialized_keyed_blocks_into::<T>(&mut HashMap<..>)` |
| `key_tail_offsets::<T>()` | `key_tail_offsets_into::<T>(&mut HashMap<..>)` |
| `open_readonly(spec, path)` | `open_readonly_with_scratch(spec, path, &mut Vec<RecordIndexEntry>)` |
| `open(spec, path)` | `open_with_scratch(spec, path, &mut Vec<RecordIndexEntry>)` |

`decode_blocks_into` is deliberately **not** named as the twin of `blocks()`:
`blocks()` returns a lazy collection whose peak is one payload however many
records it covers, and this one materialises all of them.

The generated per-block accessors carry the same pair — `<plural>_entries_into`,
`<plural>_decoded_into`, and `<plural>_into` for a keyed block — on both the
inherent and the trait route.

Reading one record you already hold an entry for, into a buffer you own:

| API | Meaning |
| --- | --- |
| `read_payload_into(&entry, &mut Vec<u8>)` | stored bytes, checksum-verified |
| `read_logical_payload_into(&entry, &mut Vec<u8>)` | decompressed if the record is compressed |
| `decode_block_into::<T>(&entry, &mut scratch)` | decode, reading the bytes through `scratch` |

All three are bound to this open handle's snapshot, not to a path. The entry is
yours and is therefore not trusted: an extent past the snapshot is refused, and
bytes that do not match the entry's checksum are refused.

### Reads resolved through a directory you supply

`with_directory(&directory)` returns the same reads answered against a directory
of your own instead of the handle's. A directory is anything implementing
`RecordDirectory` — in practice the `Vec<RecordIndexEntry>` that
`index_entries_into` or `open_readonly_with_scratch` filled, or a subslice of
it.

```rust
let mut index = Vec::new();
let file = VarveFile::open_readonly_with_scratch(spec, path, &mut index)?;
let points = file.with_directory(&index).blocks::<Point>()?;
```

`open_readonly_without_directory(spec, path, &mut index)` opens a handle that
keeps **no** directory — nothing that scales with the file — and hands you the
one the scan produced. The `index` parameter is output, not scratch, and is not
optional: a handle that keeps no directory cannot produce one afterwards.

The reads are not withdrawn in that mode. They are answered through
`with_directory`; calling one on the handle itself returns
`Error::NoResidentDirectory { operation }`, which names the read and the way to
answer it, rather than answering as an empty file would.

### An index you build only as far as the question

`record_map(&mut buffer)` returns a `RecordMap<'_>` over a
`Vec<RecordIndexEntry>` you own. It **reads nothing** when it is created and
walks the record chain forward one record at a time, only when asked.

| Method | Meaning |
| --- | --- |
| `find(predicate)` | walk until a record matches, and stop there |
| `fill_to(k)` | walk until the map holds `k` entries, or the file ends |
| `fill()` | walk to the end — this is `index_entries_into`, spelled as the case it is |
| `advance()` | walk exactly one record |
| `entries()` / `len()` / `is_complete()` / `resume_offset()` | what has been walked |
| `clear()` | empty the buffer and rewind to the start of the file |
| `release()` | empty the buffer and keep the position |

```rust
let mut buffer = Vec::new();
let mut map = file.record_map(&mut buffer)?;          // no read yet
let hit = map.find(|entry| entry.block_id == Note::ID)?;
if let Some(entry) = hit {
    let note: Note = file.decode_block_into(&entry, &mut payload)?;
}
let points = file.with_directory(&map).blocks::<Point>()?;   // the prefix walked
map.clear();
```

It implements `RecordDirectory`, so a map **is** a directory and every read
`with_directory` serves is answered against the prefix walked so far. It borrows
the *buffer*, not the handle, so reads through the handle stay available while
the map is alive.

A second question resumes where the first stopped, and `find` searches what is
already framed before it reads anything, so asking twice costs the records
between the two answers. `release()` is the bounded-memory pass: `fill_to(k)`,
use them, `release()`, repeat — memory is your `k` entries however long the
file is, and no record is read twice.

Measured on a 50,000-record, 31 MB file, wanting ten records at position 25,000:

| route | read syscalls |
| --- | --- |
| `index_entries_into` (all `N`) then read | 100,238 |
| `blocks::<T>()` then `get(i)` ×10 | 100,238 |
| `scan()`, break at 25,010 | 50,028 |
| `record_map` `fill_to(25_010)` then read | 50,058 |
| `record_map` `find` the first record | **13** |

**What a partial index does not check.** A full open validates sequence
uniqueness across every record in the file; a map validates nothing beyond the
prefix it has read. A duplicate sequence past the stopping point is not detected
until a walk reaches it. A map walked to completion (`fill()`) is the open
scan's index, entry for entry.

### An open that reads no record

`open_readonly_lazy(spec, path)` opens from the **open digest** at the end of
the file: the three facts an open needs that live in no single record — where
the committed file ends, what sequence the next append takes, and where each
block's newest record sits. `IndexPolicy::with_open_digest_on_flush(true)`
writes them at each commit point.

Measured, 50,000 records / 31 MB:

| open | read syscalls | bytes read | file overhead |
| --- | --- | --- | --- |
| `open_readonly` (scan) | 100,316 | 3,207,102 | — |
| `open_readonly` (segment chain) | 516 | 7,332,522 | +3.67 MB |
| `open_readonly_lazy` (digest) | **20** | **516** | **+12.8 KB** |

The last column is how a format chooses between the two tail records. A segment
writes an entry per record it covers, so its overhead grows with the file; a
digest writes twelve bytes per distinct block id, so the 12.8 KB above is a
hundred flushes' worth and would be the same at a billion records. What a
segment buys for its size is an open that *builds the index*; a digest
deliberately does not, and pairs with `record_map` instead.

The handle keeps no directory. `blocks`, `scan` and `keyed_blocks` return
`Error::NoResidentDirectory` and are answered through `with_directory(&map)`;
`block_chain`, `block_tail_offset`, `read_block_at` and every entry-taking
`_into` read need no directory and work directly — which is what the block tails
are in the digest for.

**It falls back to the full scan** for a file with no usable digest: one written
before the option was enabled, one whose writer appended past its last commit,
one truncated or rotted. That is not an error and the answer is identical, but
it is four orders of magnitude, so `open_readonly_lazy_with_report` returns
`LazyOpenSource::{Digest, FullScan}` rather than leaving it to a stopwatch.

The option requires `block_offset_chain` and composes with both
`segment_on_flush` and `checkpoint_on_flush` — the three answer different
questions. It is off by default and a file written without it is byte-identical
to one written before it existed.

Common `VarveWriter` APIs:

| API | Meaning |
| --- | --- |
| `push(&block)` | append fixed/variable user block; refuses a keyed block on a `keyed_offset_chain` format |
| `push_keyed(&block)` | append a keyed user block and maintain the keyed offset chain |
| `delete::<T>(&key)` | append keyed tombstone; maintains the keyed offset chain |
| `push_op::<T>(&key, &op)` | append user-defined merge op |
| `write_metadata(key, bytes)` | append internal metadata record |
| `replace_block(index, &block)` | sequence-preserving copy-on-write replacement; encoded size may grow or shrink |
| `replace_fixed(index, &block)` | same-size copy-on-write replacement; already-open readers keep their snapshot |
| `unsafe replace_fixed_in_place_exclusive(index, &block)` | expert-only in-place replacement; caller must exclude readers and writers **and must not change a keyed record's key** |
| `replace_rewrite(index, &block)` | rewrite whole file through temp file |

All four replacement entry points select their target by block id and refuse a
stored `block_version` that differs from `T::VERSION` with
`Error::BlockVersionMismatch` (F-01). `replace_rewrite` and
`unsafe replace_fixed_in_place_exclusive` previously did not: they copied the
stored version while substituting the new payload, which is a persistent
type/version disagreement on disk whenever `schema_hash = 0` opts out of
header-level schema locking. The check is no longer a step a path performs — it
is a precondition of resolving the target at all, so a replacement path added
later inherits it. The refusal now also precedes the size, limit, and
generation checks, so a call that is wrong in two ways reports
`BlockVersionMismatch` where `replace_fixed` previously reported
`ReplaceSizeMismatch`.

`unsafe replace_fixed_in_place_exclusive` additionally requires, for a keyed
block on a `keyed_offset_chain` format, that the replacement keep the record's
key. Nothing can check that at a `T: VarveBlock` signature and there is no new
generation in which the physical chain could be rebuilt, so it is part of the
unsafe contract. What the method does guarantee unconditionally is that the
affected block's resident keyed-tail map is dropped *before* the first byte is
written (F-02), so no stale predecessor can be read afterwards even if the
write fails half-way or the contract is violated.
| `commit()` | write explicit transaction marker without an implied fsync |
| `commit_durable()` | write an explicit transaction marker with ordered flush/sync barriers |
| `flush()` | write buffered records/checkpoints/manifests/marker |
| `sync()` | request durable persistence |

Keyed appends on a `keyed_offset_chain` format must go through a maintaining
path. The generic `push` / `push_info` (on both `VarveWriter` and `VarveFile`)
cannot see `T::Key`, so it would write `prev_same_key_offset = None` and
truncate the chain; it therefore refuses a keyed block on such a format with
`Error::KeyedChainRequiresKeyedApi { block_id }`. The refusal is unconditional —
it fires on the first push, not only where a predecessor exists — so the
contract never depends on call order, and nothing is written. Use `push_keyed` /
`push_keyed_info` instead, which resolve the predecessor from the persisted
keyed tail offsets and rebuild that cache after reopen. `delete` maintains the
chain on its own. Unkeyed blocks and formats without `keyed_offset_chain` are
unaffected, and the generated typed keyed writers already take the maintaining
path.

Writer encoding is limit-bounded end to end: push, metadata, replacement, and
keyed-op entry points encode through a budgeted encoder, so an oversized value
fails with `Error::LimitExceeded { resource: "logical payload length" }` and
buffering stops at the limit instead of materializing the full encoding first.
Generated nested variable fields inherit the parent encoder's remaining budget
(`Encoder::encode_nested_to_vec`), and matrix cell writes bound the encode by
the slot stride while preserving the exact-size `MatrixSizeMismatch` error.
With `checkpoint_on_flush`, the flush-time checkpoint decision is O(1) writer
state, not an index scan, so flush-per-record workloads stay linear.

Common `VarveReader` APIs:

| API | Meaning |
| --- | --- |
| `blocks::<T>()` | lazy typed records by block type |
| `keyed_blocks::<T>()` | latest put by key after tombstones are applied |
| `materialized_keyed_blocks::<T>()` | applies puts, ops, tombstones |
| `metadata(key)` | latest metadata value |
| `schema_manifest()` | latest embedded manifest if present |
| `scan()` | physical record event iterator |

## Scalable Stream And Indexed Handles

The experimental `high-cardinality-dev` feature generates a second handle
family for files whose open/append memory and I/O cost must be independent of
total file bytes, record count, and key cardinality.

| Generated API | Meaning |
| --- | --- |
| `create_stream_writer(path, options)` | create native file plus state-only `.vks` |
| `open_stream_writer(path, options)` | O(1) clean resume from `.vks`; no fallback scan |
| `open_stream_reader(path, options)` | O(1) pinned open from `.vks` |
| `restore_stream_writer(path, options)` | explicit savepoint rollback; no scan |
| `bootstrap_stream_checkpoint(path, options)` | explicit native scan to create `.vks` |
| `bootstrap_stream_checkpoint_with_progress(path, options, scan, observer)` | controlled bootstrap scan with progress/cancellation |
| `create_indexed_writer(path, options)` | create native file plus disk-key `.vki` |
| `open_indexed_writer(path, options)` | O(1) clean resume from `.vki`; no fallback scan |
| `open_indexed_reader(path, options)` | O(1) pinned open with lazy B-tree pages |
| `restore_indexed_writer(path, options)` | explicit savepoint rollback; no scan |
| `rebuild_disk_index(path, options)` | explicit native scan to rebuild `.vki` |
| `rebuild_disk_index_with_progress(path, options, scan, observer)` | controlled rebuild scan; cancelled publication preserves the old `.vki` |
| `clear_stale_writer_lock(path, policy)` | O(1) explicit lock recovery without opening/scanning native data |

Generated `push_<blocks>(iterator, BatchOptions)` methods coalesce native
records into bounded writes and return one compact `BatchAppendInfo` rather
than a per-record vector. Indexed readers expose `get_<block>(&key)` for
`key_index = disk`, plus `events()`, `verify_all()`, and lazy typed plural
methods for explicit sequential scans.

Stream and indexed readers also expose
`verify_all_with_progress(ScanOptions, observer)`. Scan control uses:

| Type | Meaning |
| --- | --- |
| `ScanCancellationToken` | cloneable cooperative cancellation flag |
| `ScanProgressOptions` | callback cadence by records and/or bytes |
| `ScanOptions` | cadence plus an optional borrowed cancellation token |
| `ScanProgress` | phase, completed records, scanned bytes, absolute current offset, snapshot length |
| `ScanProgressPhase` | `Started`, `Running`, or final pre-publication `Complete` |

Defaults notify every 16,384 records or 16 MiB. Cancellation returns
`Error::ScanCancelled { progress }`; cancellation observed through the final
callback/check publishes no new sidecar target. Progress bookkeeping does not
allocate per record.

Normal scalable open never bootstraps, repairs, rebuilds, verifies, truncates,
or scans. Missing, dirty, stale, and identity-mismatched sidecars are errors
with explicit recovery operations. `sync()`, not `flush()`, publishes a clean
generation that can be reopened normally.

### One Poison Flag Per Writer

A stream writer poisons itself when an append fails and the truncate-or-seek
rollback that should undo it fails too: the file's tail is then in an unknown
state and no further mutation is safe. The public contract has always been that
every later mutation is refused with `Error::WriterPoisoned`.

An indexed writer used to keep a *second* poison flag of its own, and the two
could disagree (F-05). After the rollback failure above, the stream was
poisoned and the indexed flag was clear, so a later single-record put or delete
passed the indexed guard and then called the stream's internal prepared-append
directly — a method that did not consult the stream's flag either. The
contract said the mutation was refused; it was performed.

The two flags are now one. The indexed writer reads the stream's flag and
poisons the stream's flag, so it cannot present itself as healthy after any
stream error, and the mutating entry points require a witness value that only
the poison check produces — a caller that skips the check does not compile.
Visible consequences:

- an indexed writer refuses after *any* stream poison, including one raised by
  a batch this writer did not itself perform;
- the refusal reached through the indexed API still reports
  `WriterPoisoned("indexed")`, and the one reached through the stream API still
  reports the stream's own context, even though there is a single flag;
- each published chunk of a batch takes its own fresh check, so a chunk that
  poisons the writer stops the chunks after it in the same call.

`Error::DiskIndex` contains the original boxed `DiskIndexError`; callers should
match that source rather than parse display text. redb database lock contention
maps to `Error::IndexBusy`. Bootstrap refuses an existing `.vks`, and rebuild
refuses a dirty `.vki`, so neither operation can bless an unsynced tail.

Sidecar publication reports durability honestly: stream/indexed sidecar
create, stream bootstrap, and disk-index rebuild can return
`Error::PublishedButParentSyncPending` when the sidecar was atomically
published but the parent-directory sync failed. The published sidecar is
preserved and usable; treat the error as a durability warning, not as "nothing
was published". Under CRC policies, `rebuild_disk_index` reads and verifies
only the payloads the index plan needs; `verify_all()` is the whole-file
integrity scan.

Disk-index descriptors carry the block schema fingerprint of the exact concrete
type whose decode and key-extraction function pointers they hold.
`DiskIndexDescriptor::of::<T>()` records `T::SCHEMA_FINGERPRINT`, plan
construction and plan validation run the format's registration gate for every
descriptor before any primary bytes reach a captured codec, and the fingerprint
is folded into the plan digest. A descriptor that claims a declared block's
id and version but is a different type is therefore refused with
`Error::BlockSchemaFingerprintMismatch` with zero decoder calls, and a sidecar
published for one block schema is refused as stale for a plan that decodes
another. The strength of that gate follows the spec: for a block id whose
`FormatSpec` declares no identity, registration falls back to first use and the
check degrades to id/version/keyedness.

Stream and indexed primaries carry a per-create 128-bit nonce as their first
record (reserved block id `CREATION_NONCE_BLOCK_ID`, one record at create and
nothing per append), and the sidecar binds to it through its recorded
primary-generation witness. Replacing a primary in place with another
equal-length primary of the same format is refused at open with
`DiskIndexError::PrimaryGenerationMismatch` on both the reader and the writer;
`rebuild_disk_index` is the recovery. A primary bootstrapped from a resident
`VarveFile` carries no nonce and is protected only by the weaker
content-window witness over its leading bytes.

With `high-cardinality-dev`, manual `VarveBlock` implementations must state
`IS_KEYED` explicitly. This is a compile-time chain-safety requirement; macro
generated blocks already provide the exact value.

See [Scalable Stream And Disk-Index I/O](scalable-io.md) for declarations,
mode selection, durability ordering, checked extent types, and measured costs.

`scan()` yields entries whose physical payload can be read through
`RecordIndexEntry`. Use `read_payload_limited(path, physical_limit)` for a
stored-byte ceiling and
`read_logical_payload_limited(spec, path, physical_limit, logical_limit)` to
bound both the stored allocation and decoded logical allocation.
All path-taking `RecordIndexEntry` helpers are deliberately low-level and
non-snapshot: they reopen whatever object the pathname currently names. Their
default forms resolve finite standard/spec limits; `*_limited` lets tooling
supply stricter or deliberately raised ceilings after validating provenance. Generated
readers and typed collections do not use them; they retain the originally
opened object and captured logical EOF.
`checked_physical_end()` is the overflow-reporting extent API; `physical_end()`
remains a saturating compatibility helper.

Append sequence numbers start at `0`, including after reopening an empty file.
After `u64::MAX` is published once, further append operations return
`SequenceExhausted` before mutating the file. A returned append or streamed
layout write error is rolled back to the prior EOF when possible. If rollback
fails, the handle returns `WriteRollbackFailed`, becomes poisoned, and rejects
later mutation, `flush`, and `sync` with `WriterPoisoned`.

Copy-on-write replacement has distinct post-publication failure states.
`PublishedButRebindFailed { sequence, source, parent_sync }` means the new
generation was already atomically published, but the current writer could not
reopen and bind to it. Discard the poisoned writer and reopen the path to
inspect the published state. Do not blindly retry the same logical update:
publication may already have applied it. `parent_sync: Some(_)` adds the second
durability fact for a double fault (F-07): the parent-directory sync for the
published pathname failed too, so the rename is not yet proved durable against
power loss. `None` means only the first two facts. Downstream code that
destructured this variant with `{ sequence, source }` must add `parent_sync` or
`..`. On Windows, `ReplacePublicationIndeterminate` means
`ReplaceFileW` failed with error 1176/1177 and reconciliation could not prove
the outcome: the pathname state is unknown, the replacement temp file is
preserved, and the writer is poisoned. Inspect the pathname and the preserved
temp separately; `classify_replace_publication_error(raw_os_error)` exposes
the underlying classification (1175 = replaced file intact, 1176/1177 =
indeterminate, everything else = pre-publication).

`varve::Error` is `#[non_exhaustive]`. Downstream exhaustive matches must keep a
wildcard arm so new diagnostics can be added without another enum-shape break.

## Collections

| Type | Meaning |
| --- | --- |
| `BlockVec<T>` | lazy typed collection for one block type |
| `BlockVec::len()` | number of visible records |
| `BlockVec::get(index)` | decode one item on demand |
| `BlockVec::iter()` | lazy iterator of decoded items |
| `KeyedBlockVec<K, T>` | latest visible put per key after tombstones |
| `KeyedBlockVec::get(&key)` | decode latest value for key |
| `KeyedBlockVec::keys()` | iterate known keys |

`BlockVec` and `KeyedBlockVec` read payload bytes lazily from the captured file
snapshot, even if the pathname is later replaced.

## Blocks And Codecs

| Trait | Implemented by |
| --- | --- |
| `VarveBlock` | all stored block types |
| `VarveKeyedBlock` | blocks with a stable key |
| `VarveMerge` | keyed blocks with user-defined ops |
| `VarveMatrixBlock` | matrix cell blocks |
| `VarveEncode` / `VarveDecode` | field and block payload codecs |
| `VarveMigration<From, To>` | explicit semantic migration |

Most users get these implementations from `varve_format!` or
`#[derive(VarveBlock)]`. Implement codecs manually only for domain-specific
value types.

Canonical booleans are encoded only as `0` or `1`; other bytes are rejected.
Decoded `BTreeMap` and `HashMap` values reject duplicate keys instead of silently
keeping one value. `HashMap` encoding requires `Ord` for deterministic key order
but no longer requires `Clone`.

`VarveBlock` requires a `const SCHEMA_FINGERPRINT: u64` with no default.
`#[derive(VarveBlock)]` computes it deterministically (FNV-1a 64 over the
canonical schema: id, version, kind, endian, keyedness, ordered field
name/type identities, and each field's resolved codec `SCHEMA_ID`).

Typed registration validates against the format, not against whichever type
arrived first. When the `FormatSpec` declares an identity for the block id — as
every `varve_format!`-generated spec does — that immutable
`(endian, keyedness, fingerprint)` triple is the authority, and a `T` that
disagrees is rejected with `Error::BlockSchemaFingerprintMismatch`,
`Error::BlockKeyednessMismatch`, or `Error::EndianMismatch` before anything is
cached or written. Call order therefore cannot decide which wire type a process
accepts.

Endian is compared *after resolution*, not as an opaque tag. Every typed path
resolves a block's byte order as `T::ENDIAN.unwrap_or(spec.endian)`, so the check
compares `registered.unwrap_or(spec.endian)` with `T::ENDIAN.unwrap_or(spec.endian)`.
A declared override that contradicts the format's own declaration is rejected —
that is the silent byte swap this exists to catch — while two declarations that
resolve to the same byte order are accepted, because they genuinely produce
identical bytes.

For an identity-bearing block id the contract is a pure function of the spec's
immutable `&'static` data, so it is validated inline and the process-global
first-use cache is neither read nor written; registration order is provably
irrelevant, and the append path takes no global lock for it. A hand-built spec
with an empty identity table keeps the older first-use behaviour for the block
ids it does not cover: the first `T` seen defines the contract and later
disagreeing types are rejected. That residual cache is keyed by the spec's block
table *and* identity table, each as pointer **and length**, so an empty or prefix
view of a static array can no longer share an entry with the full view, and two
specs that share a descriptor table but declare different identities cannot alias
one cached contract. A failed validation is never cached as success. A manual block mirroring a generated one must reuse that block's
fingerprint constant. The fingerprint is process-local and is deliberately not
part of the wire format or on-disk descriptors, except where a disk-index
descriptor records it (see below).

Keyedness is an invariant, not a free-form flag. A type that implements
`VarveKeyedBlock` must declare `VarveBlock::IS_KEYED = true`; every public
keyed generic entry point — keyed collections, stream and indexed
lookup/push/delete, merge and compact, low-level `VarveFile`/reader/writer
deletes, `keyed_blocks`, `key_tail_offsets`, disk-index descriptors, and the
self-test keyed case — evaluates `KeyedBlockContract::<T>::OK`, turning an
`impl VarveKeyedBlock` with `IS_KEYED = false` into a post-monomorphization
compile error. First-seen registration remains the runtime backstop and also
rejects a keyedness disagreement for the same block id
(`Error::BlockKeyednessMismatch`). Under `high-cardinality-dev` manual
implementations must state `IS_KEYED` explicitly (no chain-unsafe default).

## Matrix API

Matrix API exists on `VarveFile`, `VarveReader`, `VarveWriter`, and generated
typed wrappers.

**Matrix reads take `&self`.** `read_matrix_cell`, `matrix_cell_payload`,
`read_matrix_aux`, `matrix_aux_len` and `matrix_cell_status` all borrow the
handle shared, on all three handle types. They read through positional I/O
(`pread` / `seek_read`) over immutable state, so no cursor moves: several
threads may read through **one** handle concurrently, and
`VarveReader`, `VarveFile` and `VarveWriter` are `Sync` and `Send` by derivation
— there is no `unsafe impl` for any of them. Existing callers holding a
`let mut reader` keep compiling. Writes are unaffected and still take
`&mut self`. `copy_matrix_cell_bytes_from` also takes its *source* handle by
shared reference.

Sharing a handle is also **faster** than not sharing it was, which is a separate
claim from the type signature and is measured rather than argued. On Windows,
`ReadFile` against a synchronous handle serialises on the kernel file object, so
`&self` alone would give the *right* to share a handle and none of the
throughput: four threads through one handle measured 0.27x of a single thread
(91,137 vs 343,285 cell reads/s, `IntegrityPolicy::Crc32`). Varve therefore
hands each reading thread its own file object, derived from the open handle with
`ReOpenFile` — no path, so no re-resolution — the first time that thread reads
that file. After it, the same measurement is 568,397 reads/s on four threads
against 256,559 on one (2.2x), matching what four separately-opened readers
achieve. The handles are read-only (`FILE_GENERIC_READ`), never escape the
module that owns them, and are closed when the file handle is dropped. On Unix
`pread` does not serialise, so no extra descriptor is opened at all.
`crates/varve/tests/matrix_concurrent_reads.rs` asserts the scaling in wall
clock, not by counting readers.

**Every number above is a Windows number, and the private-handle pool is
`#[cfg(windows)]`.** The Unix path — a shared handle plus `pread` — is executed
but not measured: the suite runs on Linux in CI, so the path is exercised, while
the two wall-clock scaling contracts have no Unix numbers behind them. See
[Known Limitations §6.1](known-limitations.md#61-the-unix-code-paths-and-what-the-first-linux-run-found).

There is one lock in the read path, stated because its absence used to be the
claim: each commit bitmap holds a `Mutex` over its page map, so that a demand
fault-in (see `MatrixMetadataResidency` above) can happen under `&self`. It is
taken only for `O(1)` map operations and **never held across I/O** — a fault-in
releases it for the read, so two threads faulting the same page duplicate a
4 KiB read rather than queueing. Session-only maps (a writer's current-write map,
a rebuilt map) have no backing to fault from, so there the lock only ever guards a
hash lookup.

Visibility follows the demand path: a page's contents are as of the first touch
that faulted it in, not as of open. Before 0.5.0 the `EagerVerified` residency
policy snapshotted every published page at open and a reader saw the commit state
as of its own open; that policy is gone, and with it that guarantee — a reader
needing one consistent instant across a whole map must coordinate it, because the
only mechanism that provided it was whole-live-set residency.

| API | Meaning |
| --- | --- |
| `create_writer_with_dims(path, dims)` | create matrix file with runtime dimensions |
| `write_matrix_cell::<T>(key, &value)` | encode and write a slot, clearing commit first |
| `commit_matrix_cell::<T>(key)` | mark a written cell visible |
| `read_matrix_cell::<T>(key)` | read committed cell or return `MatrixNotCommitted` |
| `matrix_cell_status::<T>(key)` | committed/not committed status |
| `clear_matrix_cell::<T>(key)` | clear one cell commit bit |
| `clear_matrix_category(category)` | clear a whole commit category |
| `matrix_cell_payload::<T>(key)` | checked raw payload bytes for a committed cell |
| `write_matrix_aux(name, offset, bytes)` | write preallocated noncommit aux bytes |
| `read_matrix_aux(name, offset, len)` | read aux bytes |
| `matrix_resume_signal(category)` | classify partial matrix progress; refuses a quarantined category with `MatrixCommitQuarantined` |
| `matrix_recovery_report()` | report matrix findings/actions |
| `verify_matrix_metadata()` | run the commit-metadata verification pass now; returns the same report |
| `rebuild_matrix_commit_from_crc::<T>()` | rebuild commit map from slot CRC evidence; refuses when the CRC-validity evidence is incomplete |
| `write_matrix_cell_durable` | write/commit with ordered durability barrier, then a post-commit hook |

`rebuild_matrix_commit_from_crc::<T>()` now refuses, with
`Error::MatrixFatalCorruption`, when the block's CRC-validity page index could
not be enumerated in full (F-04). A rebuild sets a commit bit only where
slot-valid evidence says the CRC is meaningful; pages missing from a damaged
validity index read as absent, so rebuilding from them would publish those
cells as *uncommitted* and erase the previous commit view using evidence that
was never read. The refusal is unconditional: `with_matrix_fatal_forensics`
relaxes *reading* fatal state, not a destructive republication, which is the
same rule the interrupted-rebuild poison already followed. Repair or regenerate
the validity evidence first. Varve deliberately offers no implicit way to
discard unverifiable visibility.

The hook of `write_matrix_cell_durable` runs **after** the cell is committed
and synced, so it cannot un-commit anything. A hook failure is reported as the
typed published outcome `Error::MatrixCommittedButHookFailed { event, source }`
(the event is boxed so `Error` stays small) rather than a bare error, so a caller can tell "not committed, retry the write"
from "committed, retry only the notification"; the carried `MatrixCommitEvent`
is the one the hook was given. Retrying the whole call after that error repeats
the durable write and re-runs the hook.

The commit sync is the only other step after the commit, and it is typed the
same way: `Error::MatrixCommittedButDurabilityUnproven { event, source }` means
the commit bit is in the file, the cell is readable after a clean exit, the
hook did not run, and only power-loss durability is unproven. Those two are the
call's only published outcomes; every other error means the cell was not
committed.

That family statement describes the value the call *returns* (F-06). Both
variants box their event and their source, so building either one allocates
after the cell is authoritative. Those are shape-sized allocations — fixed by
the type, never by a file length or caller count — and Varve allocates
shape-sized memory infallibly, so an allocator refusal terminates the process
instead of returning a different outcome. No caller observes a wrong outcome;
the cell is committed on disk, and reopening the file shows it. The crate-wide
rule, and the alternatives that were rejected, are in
`docs/durability-model.md` under "Allocator Failure And Published Outcomes".

Matrix slot types must have a width fixed by the type itself, because a slot
needs a stride the compiler knows. Scalars and fixed arrays of them qualify.
`PackedBitmap` does **not**: it owns a `Vec<u8>` and encodes a `bit_len` plus a
variable byte string, so it is rejected as a matrix field — it remains a normal
variable-field codec with a stable non-zero `SCHEMA_ID`.

Classification is decided **entirely by the resolved type, never by the source
spelling**. The generated `SLOT_STRIDE` folds each element's
`VarveEncode::WIRE_TYPE` (not `size_of`), and a wire type with no fixed width
fails const evaluation with `matrix fields must have a width fixed by their
type; this codec does not`. Two consequences, both intended:

- a **type alias** of a supported scalar works. `type Word = u32;` and
  `window: [Word; 4]` are ordinary 4-byte and 16-byte slots, because the alias
  resolves before `WIRE_TYPE` is read;
- a user type merely *named* `u32` or `PackedBitmap` is **not** admitted. It has
  no `VarveEncode` impl (or one without a fixed width), so it fails at the same
  const check or at the trait bound.

The macro's own syntactic check is deliberately permissive: it rejects only
source shapes that can never denote a fixed-stride type at all — references, raw
pointers, slices, tuples, trait objects, `impl Trait`, function pointers — and
defers everything else to `SLOT_STRIDE`. It is a diagnostic aid, not the
authority.

Lower-level matrix calls use `MatrixKey { scan, ch }`. Generated format-first
wrappers expose block-specific key structs such as `CellKey { scan, ch }` and
convert them into the runtime key internally.

Matrix cell read, write, and mmap access enforce the same typed registration
gate as the append-log APIs before any structural checks: a manual matrix
block that matches a registered block's id, shape, and stride but declares a
different `SCHEMA_FINGERPRINT` or keyedness is rejected with
`BlockSchemaFingerprintMismatch` / `BlockKeyednessMismatch` instead of
decoding foreign cells.

When the verification pass finds a damaged commit map, the whole category is
failed closed: the `Recoverable` `CommitMap` finding is the quarantine flag, and
no map bytes are retained (0.5.0 removed the retained "recovery evidence" copy and
the empty replacement map that stood in for it). Cell categories can be rebuilt
from per-slot CRC evidence; single/per-channel categories must be explicitly
cleared. Status and value reads return `MatrixCommitQuarantined(category)` instead
of conflating unavailable evidence with `NotCommitted`; writes reject the category
until recovery; and `matrix_resume_signal` / `matrix_sidecar_resume_signal` refuse
it as well, where before 0.5.0 they answered `Clean` from the replacement map.

An overwrite withdraws the old commit and CRC-valid evidence before touching
slot bytes. A partial I/O failure therefore leaves the slot uncommitted and
poisons the writer; successful replacement becomes readable only after a new
commit. The matrix *layout* is snapshotted on open; commit maps are **not** — since
0.5.0 each commit-map page is as of the first read that faulted it in — and slot
bytes are in-place storage. Do not overlap a reader with writes to slots it may read.
Immutable concurrent matrix snapshots require a future generation/version or
read-lease design and are not promised by VMAT v4.

When recovery finds a `Fatal` matrix finding (for example a metadata CRC
mismatch), default matrix access is fail-closed: every default read, write, aux,
resume, and rebuild accessor returns `Error::MatrixFatalCorruption` through an
`O(1)` flag precomputed once at open. `matrix_recovery_report()` stays readable
so the fatal state can be inspected. Forensic read-through of a fatal-state file
is an explicit opt-in via `FormatSpec::with_matrix_fatal_forensics()`, which
sets `FormatSpec::matrix_fatal_forensics`; it does not repair or mutate the file.

## Physical Layout API

Use this when the file bytes are not the Varve-native container. The first
supported shape is an optional declared file header followed by an append stream
of declared segments with lead-in fields, caller metadata bytes, contiguous raw
bytes, optional footer fields, and finalized/backpatched offsets.

Physical layout declarations are field groups, not logical `VarveBlock`
payload structs. `VarveBlock` remains the native append-log payload unit;
`file_header`, segment `lead_in`, and `footer` describe bytes that sit around
metadata and raw regions.

| API | Meaning |
| --- | --- |
| `preset: varve_native` | explicit native container preset; also the default |
| `preset: none` | custom layout owns byte zero |
| `layout { file_header ... segment ... }` | declare file header, segment lead-in, metadata region, raw region, optional footer |
| `Format::create_layout_writer(path)` | create a generated `FormatLayoutWriter` for format-first layout declarations |
| `Format::create_layout_writer_with_header(path, fields)` | low-level header-field creation path |
| `Format::create_layout_writer_with_typed_header(path, header)` | generated typed header-field creation path when a `file_header` is declared |
| `Format::open_layout_writer(path)` | validate an existing custom-layout file and append more segments through `FormatLayoutWriter` |
| `Format::open_layout_reader(path)` | open a generated `FormatLayoutReader` |
| `Format::inspect_layout_file(path)` | inspect native or custom physical file framing through one result type |
| `Format::inspect_layout_file_report(path)` | inspect complete physical prefix plus terminal tail status without claiming strict open success |
| `FormatLayoutWriter::write_data_segment(...)` | generated segment writer from the declared segment name |
| `FormatLayoutWriter::write_data_segment_streamed(...)` | generated segment writer that streams metadata/raw bytes into the file |
| `FormatLayoutReader::data_segments()` | generated typed segment info collection |
| `FormatLayoutReader::file_header()` | generated typed file-header info when a `file_header` is declared |
| `FormatLayoutReader::read_data_segment_metadata(index)` | generated metadata reader for the declared segment |
| `FormatLayoutReader::read_data_segment_metadata_range(index, offset, len)` | generated bounded metadata subrange reader |
| `FormatLayoutReader::read_data_segment_raw(index)` | generated raw-region reader for the declared segment |
| `FormatLayoutReader::read_data_segment_raw_range(index, offset, len)` | generated bounded raw-region subrange reader |
| `LayoutWriter::write_segment(SegmentWrite)` | write lead-in, metadata, raw bytes, footer, then backpatch offsets |
| `LayoutWriter::write_segment_streamed(SegmentWriteStream)` | stream metadata and raw bytes while still counting/backpatching offsets |
| `SegmentWrite::fields` | caller values for lead-in fields |
| `SegmentWrite::footer_fields` | caller values for footer fields |
| `SegmentWriteStream::write_metadata` | closure that writes metadata bytes directly to the file |
| `SegmentWriteStream::write_raw` | closure that writes raw-region bytes directly to the file |
| `LayoutReader::file_header_len()` | validated header length before the first segment |
| `LayoutReader::file_header_fields()` | validated declared file-header values |
| `LayoutReader::file_header_field(name)` | read one validated file-header value |
| `LayoutReader::segment_count()` | how many segments the file holds |
| `LayoutReader::segment(index)` | one validated physical segment range, by value |
| `LayoutReader::segments()` | every segment in order, `Iterator<Item = Result<LayoutSegmentInfo>>` |
| `LayoutReader::segments_into(&mut Vec<..>)` | the same into a buffer you own |
| `LayoutSegmentInfo::field(name)` | read validated lead-in values such as ToC mask or version |
| `LayoutSegmentInfo::footer_field(name)` | read validated footer values |
| `LayoutReader::read_metadata(index)` | read opaque metadata bytes for a segment |
| `LayoutReader::read_metadata_range(index, offset, len)` | read a checked metadata byte range without loading the whole region |
| `LayoutReader::read_raw(index)` | read contiguous raw-region bytes for a segment |
| `LayoutReader::read_raw_range(index, offset, len)` | read a checked raw byte range without loading the whole region |
| `LayoutScanReport` | tolerant scan result with complete segments and optional `LayoutTailInfo` |
| `Format::open_layout_reader_with_limits(path, limits)` | open the typed physical reader with runtime tightening |
| `Format::open_layout_reader_trusted_unbounded(path)` | explicit trusted physical-reader boundary |
| `Format::open_layout_writer_with_limits(path, limits)` | open the typed physical writer with runtime tightening |
| `Format::inspect_layout_file_report_with_limits(path, limits)` | bounded tolerant physical scan |

Layout readers retain the opened object and captured length. Whole/range reads
use positional I/O on that object, so replacing the pathname after open does
not redirect the reader. `LayoutReader::spec()` returns a sanitized ordinary
spec; private trusted authorization is never exported from a handle.

Stream callbacks are prevalidated before the first file mutation. A returned
callback error rolls back the partial segment when possible. A callback panic is
not caught: unwinding leaves the writer poisoned so it cannot be reused.
| `LayoutTailInfo` | terminal custom-layout scan status for truncated, invalid, or unmatched tails |

This path is separate from `create_writer/open_reader`; native append-log APIs
still write the `VARVE1/2/3` container. TDMS-style files use `preset: none` so
the first bytes can be `TDSm`.

Example:

```rust
varve_format! {
    pub format FramedFormat {
        magic: b"FRAM";
        version: 1;
        limits {
            scan_bytes: 1_073_741_824;
            segments: 1_000_000;
            index_bytes: 134_217_728;
            record_payload: 67_108_864;
        }
        schema_hash: computed;
        preset: none;

        layout {
            file_header Header {
                bytes signature = b"VRV!";
                u16 header_version = 1;
            }

            segment DataSegment repeat until_eof {
                lead_in LeadIn {
                    bytes tag = b"SEGM";
                    u32 toc_mask;
                    i64 next_segment_offset =
                        finalize(target = segment_end, relative_to = after_lead_in);
                    i64 raw_data_offset =
                        finalize(target = raw_region_start, relative_to = after_lead_in);
                }
                metadata Metadata;
                raw_region Raw;
                footer Footer {
                    bytes seal = b"END!";
                    u64 segment_len =
                        finalize(target = segment_end, relative_to = segment_start);
                }
            }
        }
    }
}
```

The macro also generates a typed physical-layout facade. For the example above,
the generated method names are derived from `DataSegment`:

```rust
let mut writer = FramedFormat::create_layout_writer("data.frame")?;
writer.write_data_segment(FramedFormatDataSegmentLayoutWrite {
    fields: FramedFormatDataSegmentLayoutFields { toc_mask: 0x1108 },
    footer_fields: FramedFormatDataSegmentLayoutFooterFields,
    metadata: b"objects",
    raw: b"raw channel bytes",
})?;
writer.flush()?;

let reader = FramedFormat::open_layout_reader("data.frame")?;
let header = reader.file_header();
assert_eq!(header.header_version()?, 1);
let first = reader.data_segment(0)?.unwrap();
assert_eq!(first.toc_mask()?, 0x1108);
let raw_slice = reader.read_data_segment_raw_range(0, 8, 8)?;
```

The generated wrapper still exposes the low-level `write_segment`,
`write_segment_streamed`, `read_metadata`, `read_metadata_range`, `read_raw`,
`read_raw_range`, and `segments` methods for adapters that need to bridge
dynamic metadata. Multiple segment descriptors are dispatched by their leading
literal lead-in prefix. A byte tag such as `bytes tag = b"DATA"` is the common
discriminator; immediately following literal numeric fields extend the dispatch
key. Generated segment-specific `index` parameters are per segment kind, while
the low-level range methods continue to use the global physical stream index.

`crates/varve/examples/tdms_physical/common.rs` is a thin façade over the TDMS
example modules: `common/adapter.rs` contains the physical layout and
reader/writer adapter, `common/example_data.rs` contains fixture/scenario data,
and `common/verify.rs` contains self-verification. The combined
`tdms_physical_adapter` example exposes `write`, `append`, `read`, `read-bytes`,
and `inspect` subcommands, while the older `tdms_physical_writer` and
`tdms_physical_reader` examples are thin wrappers over the same shared
declaration. The adapter-authored file includes mixed TDMS raw channel objects
across three segments, a changed raw-data-index segment, TDMS-style
`same-as-previous` raw index reuse, bool/string/integer/float tagged
properties, and an adapter-owned `.vtidx` sidecar. The npTDMS harness verifies
exact channel values for signed/unsigned integer widths, single/double floats,
single/double floats with unit type ids, booleans, strings, timestamps, and
complex single/double floats. The reverse harness writes the scalar/channel
types that npTDMS can author correctly, parses them with the Varve-based
adapter, appends a segment with Varve, then verifies the result with both
npTDMS and the same adapter code. These examples are not a Varve-provided TDMS
reader/writer feature.

`crates/varve/examples/bmp_physical.rs` is the non-TDMS external-format smoke
example. `scripts/verify_bmp_with_pillow.py` writes a 24-bit BMP through
Varve's generated layout writer and verifies it with Pillow, then writes a BMP
with Pillow and verifies the physical header and pixel payload through Varve's
layout reader. The BMP example builds row-level chunk-index entries so padded
physical rows are represented as logical pixel chunks.

## Adapter Toolkit API

Use this layer when a custom physical layout has format-specific metadata or raw
region semantics. The toolkit is generic: TDMS, BMP, and future external
formats provide their own domain codecs and models.

| API | Meaning |
| --- | --- |
| `BinaryCursor::new(bytes, endian)` | checked endian-aware metadata/raw byte reader |
| `BinaryCursor::with_materialization_limit(bytes, endian, limit)` | cursor with an explicit cumulative ceiling for all owned arrays/strings |
| `Decoder::decode_from_slice_limited(bytes, endian, limit)` | canonical decode with a cumulative nested allocation budget |
| `BinaryWriter::new(endian)` | endian-aware byte builder for external metadata/raw payloads |
| `LengthPrefix` | implemented for `u8`, `u16`, `u32`, and `u64` length-prefixed values |
| `cursor.len_prefixed_bytes::<u32>()` | read a length-prefixed byte slice |
| `writer.len_prefixed_string::<u32>(value)` | write a length-prefixed UTF-8 string |
| `TaggedValueCodec` | user-owned mapping from external type ids to value enums |
| `ChunkIndexBuilder` | validate logical raw chunks against `LayoutSegmentInfo::raw_len` |
| `ChunkIndex::entries_for(key)` | inspect logical chunks for a stream/key |
| `SegmentReducer` | user-owned stateful reducer for segmented metadata |
| `reduce_segments_by_ref::<R, _>(...)` | run a reducer over segment metadata without copying segment info |
| `SidecarPolicy` | derive/check companion sidecar paths and main-file identity |
| `SidecarPolicy::inspect_with_scan_limit(...)` | inspect with an explicit fingerprint I/O ceiling |
| `SidecarPolicy::verify(main, expected)` | the verdict alone, reading only what the policy's flags consult: with `verify_main_fingerprint` off, one `metadata()` at any main-file size; `inspect` fingerprints whenever the sidecar exists, because its report publishes the identity |
| `SidecarPolicy::verify_with_scan_limit(...)` | the same, with an explicit fingerprint I/O ceiling — reached only when `verify_main_fingerprint` is set |
| `SidecarIdentity::from_main_file(...)` | fingerprint the captured extent up to the finite 256 MiB default |
| `SidecarIdentity::from_main_file_with_scan_limit(...)` | fingerprint with a caller-selected scan ceiling |
| `AdapterCheckReport` | compose physical tail status and adapter diagnostics |
| `AdapterTailStatus` | summarize expected end, available tail length, and evidence for damaged tails |
| `AdapterInputFile` | bridge path-backed and temporary byte-backed adapter inputs |
| `AdapterInputFile::from_bytes(ext, bytes)` | create an exclusive temporary input; clones keep it alive until the last drop |

The toolkit deliberately does not define TDMS object paths, scaling, DAQmx,
DataFrame/HDF export, or other domain semantics. It sits above the physical
layout layer and supplies reusable mechanics only — tagged-value codecs, chunk
index builders, segment reducers and adapter self-checks — while the adapter
author supplies the domain meaning as ordinary Rust types. TDMS is one
instantiation of those pieces, not a Varve feature.

## Mmap And Zero-Copy

Feature-gated APIs:

| Feature | API | Meaning |
| --- | --- | --- |
| `mmap` | `unsafe mmap_payloads()` | mmap append-log payload windows |
| `mmap` | `unsafe mmap_matrix()` | mmap matrix slot windows |
| `mmap` | `MmapMatrix::cell_numeric::<T, N>(key)` | endian-aware numeric scalar read |
| `zero-copy` | `unsafe MmapPayloads::raw_fixed::<T>()` | raw fixed block view |
| `zero-copy` | `unsafe MmapMatrix::raw_cell::<T>(key)` | raw matrix cell view |

Creating any file-backed mapping is unsafe. For the mapping's entire lifetime,
the caller must prevent mutation, truncation, replacement, and backing-object
invalidation through every handle, thread, and process. The implementation
clones the already-open file handle and validates snapshot extents, but those
checks cannot enforce external immutability. Zero-copy adds unsafe marker-trait
and raw-reference contracts. Normal fixed/matrix reads use owned canonical
decoding.

Once the caller establishes that external immutability boundary, the returned
safe window APIs stay memory-safe through these enforced invariants:

1. The mapping is read-only and is created from a clone of the already-open file
   handle, so a path rename or substitution cannot redirect the mapping.
2. Record, footer, matrix, and slot extents use checked arithmetic and are
   validated against the actual mapped length before exposure.
3. `MmapPayloads` owns a copied snapshot index and rejects any
   `RecordIndexEntry` that is not exactly in that snapshot.
4. Safe accessors perform checked slicing; configured matrix CRCs are verified
   before committed slot bytes are returned.
5. The mmap owner holds the mapping for its full RAII lifetime, and returned
   slices borrow that owner, so Rust prevents them from outliving the mapping.
6. Raw zero-copy access additionally validates registration, block kind,
   version, endian, exact size, and alignment before creating a typed reference.

This is a conditional safety proof, not a claim that Varve can police arbitrary
external processes. If a caller cannot control every writer, it must use owned
reads such as `blocks::<T>()`, `read_payload`, or matrix typed reads instead of
mmap.

## Compression API

| API | Meaning |
| --- | --- |
| `compression: variable_blocks(...)` | macro global variable-block compression |
| `CompressionPolicy::VariableBlocks` | runtime global compression |
| `BlockCompressionDescriptor` | runtime per-block compression override |
| `CompressionHeaderMode::RecordExplicit` | self-describing `VCMP` envelope per record |
| `CompressionHeaderMode::FileExplicit` | file-header compression metadata |
| `CompressionHeaderMode::FormatContract` | no metadata; static spec is contract |
| `ChunkedBytes::from_zstd_chunks` | caller-managed chunked blob helper |
| `ChunkedBytes::decode_to_vec()` | decode with the finite standard logical-payload ceiling |
| `ChunkedBytes::decode_to_vec_limited(limit)` | decode with an explicit caller ceiling |

Compression requires the `compression-zstd` feature when records are actually
compressed or decompressed. `ChunkedBytes` additionally requires `integrity` for
per-chunk CRC creation/verification.

## Update, Merge, Compact

| API | Meaning |
| --- | --- |
| `push_op::<T>(&key, &op)` | append user-defined operation |
| `delete::<T>(&key)` | append tombstone |
| `merge_keyed_files::<T>(spec, base, deltas, output)` | materialize ordered shards |
| `compact_keyed_file::<T, P>(spec, input, output)` | compact one file's final keyed state |
| `compact_keyed_files::<T, P>(spec, base, deltas, output)` | compact base plus deltas directly |
| `estimate_keyed_merge::<T, P>(spec, base, deltas)` | pre-flight `KeyedMergeEstimate`, decodes no values |
| `merge_keyed_files_with_key_limit::<T, P>(spec, base, deltas, output, max_distinct_keys)` | merge, refused typed at the key ceiling |
| `compact_keyed_files_with_key_limit::<T, P>(spec, base, deltas, output, max_distinct_keys)` | base+delta compact with the same ceiling |
| `compact_keyed_file_with_key_limit::<T, P>(spec, input, output, max_distinct_keys)` | single-input compact with the same ceiling |

Conflict order is shard order, then local sequence, then record ordinal. Later
delta shards win.

### Resident scale contract

`merge_keyed_files`, `compact_keyed_files`, and `compact_keyed_file` are
resident operations and are deliberately **not** PB-scale. Each opens its
inputs as whole `VarveFile` values and retains one map entry per distinct key
ever seen, tombstoned keys included:

- time: `Theta(records + decoded bytes) + O(N log N) + O(K-live log K-live)`,
  where the `O(N log N)` term is the per-input open's sequence-uniqueness sort
  over that input's `N` records. It degrades to `Theta(N)` for the ordinary case
  of a file whose sequences ascend with offset — which is what a Varve writer
  produces — but the sort is the guaranteed bound for a reordered or hostile
  input;
- memory: `O(K-ever + largest resident input index + 8N uniqueness temporary for
  that input + retained live values)`

Nothing spills to disk, so `K-ever` must fit in memory. Varve exports no
bounded-memory external merge or compact; the scalable stream and indexed
writers cover bounded *ingest*, not bounded merge/compact.

Callers whose key cardinality is not known to be resident-sized should either
size the run first with `estimate_keyed_merge` — or bound it with a
`*_with_key_limit` entry point, which fails with
`Error::LimitExceeded { resource: "merge distinct keys", .. }` at the key
boundary and publishes no output. `estimate_keyed_merge` itself opens each
input as a resident file, so it costs `O(largest input index)`; it reports that
number but is not bounded below it. It decodes no value.

`KeyedMergeEstimate` reports:

| Field | Kind |
| --- | --- |
| `input_records` | exact count |
| `key_bearing_records` | exact count |
| `max_distinct_keys` | exact **upper bound** on `K-ever` |
| `largest_input_index_bytes` | structural byte estimate |
| `largest_input_open_transient_bytes` | structural byte estimate (the `8N` uniqueness temporary) |
| `max_state_bytes` | structural byte estimate |
| `max_output_values_bytes` | structural byte estimate |

Only the three count fields are bounds. The byte fields are **structural
estimates**: each is `count * size_of::<...>()` over inline storage.

`KeyedMergeEstimate::peak_resident_structural_bytes()` is the sizing entry
point. It is a **structural estimate, not an upper bound**: it sums the merge
state, the output vector reserved while that state is still alive, the largest
input's resident index, and the `8N` uniqueness transient that input's open can
hold alongside it, counting `count * size_of::<...>()` inline storage only. It
excludes the heap owned by individual `Key` and `T` values (unbounded for
heap-owning types such as `String` or `Vec` fields), `HashMap` load-factor slack
and control bytes, per-record decode scratch, and allocator metadata. Callers
who need a hard ceiling must bound the run with a `*_with_key_limit` entry point
rather than size it.

> Renamed in 0.4.0. The method was `peak_resident_bytes()` and was documented as
> a true upper bound. That was false for any heap-owning `Key` or `T`, by an
> arbitrarily large margin. The rename is deliberate rather than a deprecating
> alias, so that no caller keeps reading the old value as a guarantee.

## Migration API

| API | Meaning |
| --- | --- |
| `VarveMigration<From, To>` | user-defined semantic conversion |
| `blocks_migrated::<From, To, M>()` | read source-version blocks and convert |
| `copy_matrix_cell_bytes_from::<From, To>()` | copy compatible matrix slot payload bytes |

Normal typed reads reject block-version mismatch. Migration is explicit.

## Diagnostics And Self-Test

| API | Meaning |
| --- | --- |
| `diagnostics()` | static spec report |
| `diagnose_file(path)` | actual file report |
| `self_test(path)` | disposable roundtrip builder |
| `FormatSelfTest::with_block(value)` | append/read sample block |
| `with_keyed_block(value)` | append/read keyed sample |
| `with_dims(dims)` | matrix runtime dimensions |
| `with_matrix_cell(key, value)` | write/commit/read matrix sample |
| `with_uncommitted_matrix_cell(key, value)` | verify uncommitted read rejection |
| `with_matrix_aux(name, offset, bytes)` | write/read aux sample |
| `cleanup(true)` | after the run, remove only files the run itself created (its native file and `.lock`), verified by object identity |

Report domains:

| Domain | Look first at |
| --- | --- |
| `FormatDefinition` | schema declaration, ids, policies |
| `CallerUsage` | API call, key, dims, commit state, block type |
| `FeatureGate` | Cargo features |
| `FileData` | actual bytes, corruption, wrong spec for file |
| `Environment` | filesystem, locks, concurrent processes |
| `LibraryInvariant` | possible Varve bug after simple codecs are ruled out |

The self-test is non-destructive: it claims the target with exclusive create
(one exclusively created, lock-bound handle held through matrix
initialization) and refuses a pre-existing path as a `CallerUsage` failed step
instead of truncating it. `cleanup(true)` is identity-checked: the native file
is deleted only while the pathname still resolves to the object this run
created, and the `.lock` marker only after re-acquiring it through the writer
lock protocol, so a swapped-in foreign file or a foreign-owned marker
survives.

## Feature Flags

| Feature | Enables | Stability |
| --- | --- | --- |
| `integrity` | CRC32 integrity and matrix/sidecar CRC checks | stable |
| `compression-zstd` | zstd record compression | stable |
| `mmap` | mmap payload/matrix views | stable, `unsafe` entry points |
| `zero-copy` | raw mmap views; implies `mmap` | stable, `unsafe` entry points |
| `high-cardinality-dev` | disk-backed index, streaming and indexed handles, scan-control | **experimental**: the surface and the sidecar layout may change without a major version |
| `scalable-fault-injection` | fault-injection counters and hooks for the scalable-path tests; implies `high-cardinality-dev` | **test infrastructure**: not a production feature; the counters it exposes are `#[doc(hidden)]` |

All features are off by default. `high-cardinality-dev` and
`scalable-fault-injection` are listed because they are exported and therefore
reachable, not because they are recommended: the first is explicitly
experimental and the second exists to let the test suite inject faults and read
counters. Neither is covered by the stability expectations the rest of this
document assumes.

## Common Error Interpretation

| Error | Usually means |
| --- | --- |
| `InvalidMagic` | wrong file type or corrupt header |
| `SchemaHashMismatch` | reader spec does not match writer spec; also raised for files pinned with a computed hash from a previous hash-algorithm version |
| `BlockVersionMismatch` | block version changed without migration |
| `CompressionFeatureDisabled` | enable `compression-zstd` or disable compression |
| `IntegrityFeatureDisabled` | enable `integrity` or disable CRC policy |
| `MatrixDimensionsRequired` | matrix file creation needs runtime dimensions |
| `MatrixNotCommitted` | slot bytes exist but commit bit is clear |
| `MatrixFatalCorruption` | recovery found a fatal matrix finding; default access is fail-closed. Use `FormatSpec::with_matrix_fatal_forensics()` for forensic read-through. Also returned by `rebuild_matrix_commit_from_crc` when the CRC-validity page index is incomplete — that refusal is **not** relaxed by forensic mode, because a rebuild publishes state rather than reading it |
| `MatrixSidecarMismatch(reason)` | matrix sidecar rejected: wrong native identity, layout generation, creation nonce, or sidecar version; regenerate the sidecar |
| `InvalidMatrixLayout` | matrix layout bytes are not valid for this build — including a matrix file created before the creation-nonce region existed; recreate the matrix file |
| `MatrixSizeMismatch` | encoded matrix payload does not match slot stride |
| `FormatVersionMismatch { expected, actual }` | container/layout version is not the one this build writes — including a `VMAT` layout version 1, 2, or 3 matrix file (v4 is current; the `MCRC` integrity table is at v2); the artifact is stale and regenerable |
| `KeyedChainRequiresKeyedApi { block_id }` | generic `push`/`push_info` cannot maintain the keyed offset chain; use `push_keyed`/`push_keyed_info` or the generated keyed writer |
| `LimitExceeded { resource: "variable field ids" }` | decoding charged the materialization budget for distinct variable field ids above 63 and the budget ran out |
| `LimitExceeded { resource: "merge distinct keys" }` | a `*_with_key_limit` merge/compact hit the caller's `K-ever` ceiling; nothing was published |
| `BlockSchemaFingerprintMismatch` | a block impl disagrees with the format's declared identity for that block id, or two impls share an id with different `SCHEMA_FINGERPRINT` |
| `BlockKeyednessMismatch` | two block impls share an id but disagree on keyedness |
| `PublishedButParentSyncPending` | atomic replacement published, but parent-directory durability is unconfirmed; not a rollback. Also surfaced by redb sidecar create/bootstrap/rebuild |
| `MatrixCommittedButHookFailed { event: Box<MatrixCommitEvent>, source }` | `write_matrix_cell_durable` committed and synced the cell, then the caller's post-commit hook failed. Not a rollback: retry the notification with the carried `MatrixCommitEvent`, not the whole call |
| `MatrixCommittedButDurabilityUnproven { event: Box<MatrixCommitEvent>, source }` | `write_matrix_cell_durable` put the commit bit in the file and the durability request after it failed. Not a rollback: the cell is committed and readable after a clean exit, and the hook did **not** run. The writer is poisoned; reopen and `sync`, then issue the notification with the carried event |
| `CommittedButDurabilityUnproven { sequence, source }` | `commit_durable` appended the transaction's commit marker and the `flush`/`sync_all` after it failed. Not a rollback: the transaction is committed and `sequence` names the marker. Retry `sync`, not the transaction — re-running it appends a second marker |
| `WriterLockMarkerNotDedicated { path, reason }` | the `<target>.lock` marker path is a symlink/reparse point, a multi-link object, or not a regular file, so Varve refused to truncate and rewrite it. Remove or un-alias the marker path |
| `PublishedButRebindFailed { sequence, source, parent_sync }` | replacement published but the writer could not rebind and was poisoned. `parent_sync: Some(_)` additionally means the parent-directory sync for the published pathname failed in the same publication, so the rename is not yet proved durable against power loss |
| `ReplacePublicationIndeterminate` | Windows `ReplaceFileW` 1176/1177 with unresolvable pathname state; temp preserved, writer poisoned, do not blindly retry |
| `WriterLockHeld` | another writer or stale lock exists |
| `WriterLockBreakRefused` | the explicit lock policy did not prove removal was allowed |
| `ScanCancelled { progress }` | an explicit scalable scan stopped cooperatively at the reported boundary |
