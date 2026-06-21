# Varve API Reference

This is a practical public API map for application authors. It is not a
rustdoc replacement; it explains which API to reach for and what it means.

## Generated Format API

For a format declared as:

```rust
varve_format! {
    pub format AppFormat {
        // ...
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
| `AppFormat::open_recover(path)` | explicit recovery open |
| `AppFormat::open_recover_with_report(path)` | recovery open plus report |
| `AppFormat::diagnostics()` | static format diagnostics |
| `AppFormat::diagnose_file(path)` | read-only file diagnostics |
| `AppFormat::self_test(path)` | build an end-to-end self-test |

Generated typed methods depend on block names:

| Declaration | Writer method | Reader method |
| --- | --- | --- |
| `fixed Point` | `push_point(&Point)` | `points() -> BlockVec<Point>` |
| `variable User(key=[id])` | `push_user(&User)`, `delete_user(&key)` | `users() -> KeyedBlockVec<K, User>` |
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
| `with_matrix_spec(dims, commits, blocks)` | manual matrix registry |
| `with_matrix_aux(aux)` | manual matrix aux registry |
| `validate()` | check static spec consistency |
| `computed_schema_hash()` | deterministic schema fingerprint |
| `schema_debug_dump()` | human-readable schema dump |
| `diagnostics()` | structured static diagnostics |
| `diagnose_file(path)` | structured file diagnostics |
| `self_test(path)` | self-test builder |

Use the generated `Format::spec()` path unless you need derive-first or manual
registry construction.

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
| `replace_fixed(index, &block)` | same-size in-place fixed replacement |
| `replace_rewrite(index, &block)` | rewrite whole file through temp file |
| `commit()` | write explicit transaction marker |
| `flush()` | write buffered records/checkpoints/manifests/marker |
| `sync()` | request durable persistence |

Common `VarveReader` APIs:

| API | Meaning |
| --- | --- |
| `blocks::<T>()` | lazy typed records by block type |
| `keyed_blocks::<T>()` | latest put by key |
| `materialized_keyed_blocks::<T>()` | applies puts, ops, tombstones |
| `metadata(key)` | latest metadata value |
| `schema_manifest()` | latest embedded manifest if present |
| `scan()` | physical record event iterator |

## Collections

| Type | Meaning |
| --- | --- |
| `BlockVec<T>` | lazy typed collection for one block type |
| `BlockVec::len()` | number of visible records |
| `BlockVec::get(index)` | decode one item on demand |
| `BlockVec::iter()` | lazy iterator of decoded items |
| `KeyedBlockVec<K, T>` | latest put per key |
| `KeyedBlockVec::get(&key)` | decode latest value for key |
| `KeyedBlockVec::keys()` | iterate known keys |

`BlockVec` and `KeyedBlockVec` read payload bytes lazily from the file path.

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

## Physical Layout API

Use this when the file bytes are not the Varve-native container. The first
supported shape is an optional declared file header followed by an append stream
of declared segments with lead-in fields, caller metadata bytes, contiguous raw
bytes, optional footer fields, and finalized/backpatched offsets.

| API | Meaning |
| --- | --- |
| `preset: varve_native` | explicit native container preset; also the default |
| `preset: none` | custom layout owns byte zero |
| `layout { file_header ... segment ... }` | declare file header, segment lead-in, metadata region, raw region, optional footer |
| `Format::create_layout_writer(path)` | create a custom physical-layout writer |
| `Format::create_layout_writer_with_header(path, fields)` | create a writer when the file header has caller-supplied fields |
| `Format::open_layout_writer(path)` | validate an existing custom-layout file and append more segments |
| `Format::open_layout_reader(path)` | open a custom physical-layout reader |
| `LayoutWriter::write_segment(SegmentWrite)` | write lead-in, metadata, raw bytes, footer, then backpatch offsets |
| `SegmentWrite::fields` | caller values for lead-in fields |
| `SegmentWrite::footer_fields` | caller values for footer fields |
| `LayoutReader::file_header_len()` | validated header length before the first segment |
| `LayoutReader::segments()` | inspect validated physical segment ranges |
| `LayoutSegmentInfo::field(name)` | read validated lead-in values such as ToC mask or version |
| `LayoutSegmentInfo::footer_field(name)` | read validated footer values |
| `LayoutReader::read_metadata(index)` | read opaque metadata bytes for a segment |
| `LayoutReader::read_raw(index)` | read contiguous raw-region bytes for a segment |

This path is separate from `create_writer/open_reader`; native append-log APIs
still write the `VARVE1/2/3` container. TDMS-style files use `preset: none` so
the first bytes can be `TDSm`.

Example:

```rust
varve_format! {
    pub format FramedFormat {
        magic: b"FRAM";
        version: 1;
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

## Mmap And Zero-Copy

Feature-gated APIs:

| Feature | API | Meaning |
| --- | --- | --- |
| `mmap` | `mmap_payloads()` | mmap append-log payload windows |
| `mmap` | `mmap_matrix()` | mmap matrix slot windows |
| `mmap` | `MmapMatrix::cell_numeric::<T, N>(key)` | endian-aware numeric scalar read |
| `zero-copy` | `MmapPayloads::raw_fixed::<T>()` | raw fixed block view |
| `zero-copy` | `MmapMatrix::raw_cell::<T>(key)` | raw matrix cell view |

Zero-copy is opt-in and requires unsafe marker traits. Normal fixed/matrix reads
use owned canonical decoding.

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
