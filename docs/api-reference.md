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
`STANDARD` leaves effectively unbounded is finite: `max_file_len` 16 GiB,
`max_records` 16,000,000, `max_scan_bytes` 16 GiB, `max_index_bytes` 1 GiB, and
`max_segments` 65,536, inheriting the `STANDARD` per-item caps for everything
else. Use it when a resident open must not let a hostile file choose the reader's
CPU, I/O, or memory; large trusted files should use the scalable APIs or explicit
wider limits instead.

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
| `unsafe replace_fixed_in_place_exclusive(index, &block)` | expert-only in-place replacement; caller must exclude readers and writers |
| `replace_rewrite(index, &block)` | rewrite whole file through temp file |
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
`PublishedButRebindFailed { sequence, source }` means the new generation was
already atomically published, but the current writer could not reopen and bind
to it. Discard the poisoned writer and reopen the path to inspect the published
state. Do not blindly retry the same logical update: publication may already
have applied it. On Windows, `ReplacePublicationIndeterminate` means
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
every `varve_format!`-generated spec does — that immutable `(keyedness,
fingerprint)` pair is the authority, and a `T` that disagrees is rejected with
`Error::BlockSchemaFingerprintMismatch` or `Error::BlockKeyednessMismatch`
before anything is cached or written. Call order therefore cannot decide which
wire type a process accepts. The process registry is only a cache of an
already-validated result, keyed by the spec's block table *and* identity table,
so two specs that share a descriptor table but declare different identities
cannot alias one cached contract. A hand-built spec with an empty identity
table keeps the older first-use behaviour for the block ids it does not cover:
the first `T` seen defines the contract and later disagreeing types are
rejected. A manual block mirroring a generated one must reuse that block's
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
| `matrix_resume_signal(category)` | classify partial matrix progress |
| `matrix_recovery_report()` | report matrix findings/actions |
| `rebuild_matrix_commit_from_crc::<T>()` | rebuild commit map from slot CRC evidence |
| `write_matrix_cell_durable` | write/commit with ordered durability barrier |

Lower-level matrix calls use `MatrixKey { scan, ch }`. Generated format-first
wrappers expose block-specific key structs such as `CellKey { scan, ch }` and
convert them into the runtime key internally.

Matrix cell read, write, and mmap access enforce the same typed registration
gate as the append-log APIs before any structural checks: a manual matrix
block that matches a registered block's id, shape, and stride but declares a
different `SCHEMA_FINGERPRINT` or keyedness is rejected with
`BlockSchemaFingerprintMismatch` / `BlockKeyednessMismatch` instead of
decoding foreign cells.

When integrity verification finds a damaged commit map, open preserves the raw
bytes as recovery evidence but quarantines them from visibility. Cell categories
can be rebuilt from per-slot CRC evidence; single/per-channel categories must be
explicitly cleared. Status and value reads return
`MatrixCommitQuarantined(category)` instead of conflating unavailable evidence
with `NotCommitted`; writes reject the category until recovery.

An overwrite withdraws the old commit and CRC-valid evidence before touching
slot bytes. A partial I/O failure therefore leaves the slot uncommitted and
poisons the writer; successful replacement becomes readable only after a new
commit. Matrix layout and commit maps are snapshotted on open, but slot bytes are
in-place storage. Do not overlap a reader with writes to slots it may read.
Immutable concurrent matrix snapshots require a future generation/version or
read-lease design and are not promised by VMAT v2.

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
| `LayoutReader::segments()` | inspect validated physical segment ranges |
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
            file_len: 1_073_741_824;
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
| `SidecarIdentity::from_main_file(...)` | fingerprint the captured extent up to the finite 256 MiB default |
| `SidecarIdentity::from_main_file_with_scan_limit(...)` | fingerprint with a caller-selected scan ceiling |
| `AdapterCheckReport` | compose physical tail status and adapter diagnostics |
| `AdapterTailStatus` | summarize expected end, available tail length, and evidence for damaged tails |
| `AdapterInputFile` | bridge path-backed and temporary byte-backed adapter inputs |
| `AdapterInputFile::from_bytes(ext, bytes)` | create an exclusive temporary input; clones keep it alive until the last drop |

The toolkit deliberately does not define TDMS object paths, scaling, DAQmx,
DataFrame/HDF export, or other domain semantics. See
`docs/adapter-toolkit-design.md`.

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

- time: `Theta(records + decoded bytes) + O(K-live log K-live)`
- memory: `O(K-ever + largest resident input index + retained live values)`

Nothing spills to disk, so `K-ever` must fit in memory. Varve exports no
bounded-memory external merge or compact; the scalable stream and indexed
writers cover bounded *ingest*, not bounded merge/compact.

Callers whose key cardinality is not known to be resident-sized should either
size the run first with `estimate_keyed_merge` - which reports
`input_records`, `key_bearing_records`, `max_distinct_keys` (an upper bound on
`K-ever`), `largest_input_index_bytes`, and `max_state_bytes` without decoding
any value - or bound it with a `*_with_key_limit` entry point, which fails with
`Error::LimitExceeded { resource: "merge distinct keys", .. }` at the key
boundary and publishes no output. `estimate_keyed_merge` itself opens each
input as a resident file, so it costs `O(largest input index)`; it reports that
number but is not bounded below it.

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

| Feature | Enables |
| --- | --- |
| `integrity` | CRC32 integrity and matrix/sidecar CRC checks |
| `compression-zstd` | zstd record compression |
| `mmap` | mmap payload/matrix views |
| `zero-copy` | raw mmap views; implies `mmap` |

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
| `MatrixFatalCorruption` | recovery found a fatal matrix finding; default access is fail-closed. Use `FormatSpec::with_matrix_fatal_forensics()` for forensic read-through |
| `MatrixSidecarMismatch(reason)` | matrix sidecar rejected: wrong native identity, layout generation, creation nonce, or sidecar version; regenerate the sidecar |
| `InvalidMatrixLayout` | matrix layout bytes are not valid for this build — including a matrix file created before the creation-nonce region existed; recreate the matrix file |
| `MatrixSizeMismatch` | encoded matrix payload does not match slot stride |
| `FormatVersionMismatch { expected, actual }` | container/layout version is not the one this build writes — including a `VMAT`/`MCRC` layout version 1 matrix file; the artifact is stale and regenerable |
| `KeyedChainRequiresKeyedApi { block_id }` | generic `push`/`push_info` cannot maintain the keyed offset chain; use `push_keyed`/`push_keyed_info` or the generated keyed writer |
| `LimitExceeded { resource: "variable field ids" }` | decoding charged the materialization budget for distinct variable field ids above 63 and the budget ran out |
| `LimitExceeded { resource: "merge distinct keys" }` | a `*_with_key_limit` merge/compact hit the caller's `K-ever` ceiling; nothing was published |
| `BlockSchemaFingerprintMismatch` | a block impl disagrees with the format's declared identity for that block id, or two impls share an id with different `SCHEMA_FINGERPRINT` |
| `BlockKeyednessMismatch` | two block impls share an id but disagree on keyedness |
| `PublishedButParentSyncPending` | atomic replacement published, but parent-directory durability is unconfirmed; not a rollback. Also surfaced by redb sidecar create/bootstrap/rebuild |
| `PublishedButRebindFailed` | replacement published but the writer could not rebind and was poisoned |
| `ReplacePublicationIndeterminate` | Windows `ReplaceFileW` 1176/1177 with unresolvable pathname state; temp preserved, writer poisoned, do not blindly retry |
| `WriterLockHeld` | another writer or stale lock exists |
| `WriterLockBreakRefused` | the explicit lock policy did not prove removal was allowed |
| `ScanCancelled { progress }` | an explicit scalable scan stopped cooperatively at the reported boundary |
