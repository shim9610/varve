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
| `validate()` | check static spec consistency |
| `computed_schema_hash()` | deterministic schema fingerprint |
| `effective_layout()` | physical layout plan using header/segment/lead-in/raw/footer vocabulary |
| `inspect_layout_file(path)` | validate a native or custom file and return physical layout ranges |
| `schema_debug_dump()` | human-readable schema dump |
| `diagnostics()` | structured static diagnostics |
| `diagnose_file(path)` | structured file diagnostics |
| `self_test(path)` | self-test builder |

Use the generated `Format::spec()` path unless you need derive-first or manual
registry construction.

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
| `push(&block)` | append fixed/variable user block |
| `delete::<T>(&key)` | append keyed tombstone |
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

Common `VarveReader` APIs:

| API | Meaning |
| --- | --- |
| `blocks::<T>()` | lazy typed records by block type |
| `keyed_blocks::<T>()` | latest put by key after tombstones are applied |
| `materialized_keyed_blocks::<T>()` | applies puts, ops, tombstones |
| `metadata(key)` | latest metadata value |
| `schema_manifest()` | latest embedded manifest if present |
| `scan()` | physical record event iterator |

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

Copy-on-write replacement has a distinct post-publication failure state.
`PublishedButRebindFailed { sequence, source }` means the new generation was
already atomically published, but the current writer could not reopen and bind
to it. Discard the poisoned writer and reopen the path to inspect the published
state. Do not blindly retry the same logical update: publication may already
have applied it.

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
read-lease design and are not promised by VMAT v1.

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

Conflict order is shard order, then local sequence, then record ordinal. Later
delta shards win.

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
| `cleanup(true)` | remove self-test file and lock after run |

Report domains:

| Domain | Look first at |
| --- | --- |
| `FormatDefinition` | schema declaration, ids, policies |
| `CallerUsage` | API call, key, dims, commit state, block type |
| `FeatureGate` | Cargo features |
| `FileData` | actual bytes, corruption, wrong spec for file |
| `Environment` | filesystem, locks, concurrent processes |
| `LibraryInvariant` | possible Varve bug after simple codecs are ruled out |

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
| `SchemaHashMismatch` | reader spec does not match writer spec |
| `BlockVersionMismatch` | block version changed without migration |
| `CompressionFeatureDisabled` | enable `compression-zstd` or disable compression |
| `IntegrityFeatureDisabled` | enable `integrity` or disable CRC policy |
| `MatrixDimensionsRequired` | matrix file creation needs runtime dimensions |
| `MatrixNotCommitted` | slot bytes exist but commit bit is clear |
| `MatrixSizeMismatch` | encoded matrix payload does not match slot stride |
| `WriterLockHeld` | another writer or stale lock exists |
