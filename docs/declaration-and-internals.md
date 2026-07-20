# Declaration And Internals

This document explains what a Varve format declaration means, what code the
macro generates, and how the native file bytes are organized. Use
`docs/quickstart.md` for the smallest example. Use this file when deciding
whether a format contract is stable enough to build on.

Varve has two storage paths:

- Native append-log formats: the normal path for application data.
- Custom physical layouts: a path for external byte formats with their own
  segment lead-ins, offsets, metadata regions, and raw regions.

The declaration chooses which path you are building.

## Format Declaration Shape

The common format-first declaration looks like this:

```rust
use varve::varve_format;

varve_format! {
    pub format AppFormat {
        magic: b"APPDATA";
        version: 1;
        limits {
            file_len: 8_589_934_592;
            records: 4_000_000;
            index_bytes: 536_870_912;
            scan_bytes: 8_589_934_592;
            record_payload: 67_108_864;
            logical_payload: 268_435_456;
            materialized_bytes: 1_073_741_824;
        }
        endian: little;
        schema_hash: computed;
        extension: "vrv";
        index: [scan_on_open, checkpoint_on_flush, block_offset_chain, keyed_offset_chain];
        commit: transaction_marker(on_flush);
        manifest: embedded;

        blocks {
            fixed Point(id = 1) {
                x: u32,
                y: u32,
            }

            variable User(id = 2, key = [id]) {
                id: u64,
                name: String,
                tags: Vec<String> = default,
            }
        }
    }
}
```

The declaration is both a compile-time schema and a runtime file contract. The
static `AppFormat::spec()` is the authority used to create, open, validate,
scan, and decode files.

## Top-Level Keys

| Key | Meaning | Notes |
| --- | --- | --- |
| `magic` | User file signature bytes. | Required. Native files start with these bytes before the Varve container marker. Custom `preset: none` layouts use their declared physical bytes instead. |
| `version` | Format version. | Required. This is the whole-format version, not the per-block version. |
| `limits` | Optional operational defaults or explicit `trusted_unbounded`. | Optional and partial. Runtime policy does not change schema hashes or wire bytes. |
| `endian` | `little` or `big`. | Optional in simple cases, but recommended. Block override wins over format endian; otherwise little-endian is the default. |
| `schema_hash` | `computed` or a literal integer. | `computed` hashes the declared policies, blocks, fields in declaration order with encoding ordinals, per-block endian/keyedness/codec identities, and custom layout descriptors. Omission stores 0 and disables the open-time comparison. Pin a literal after release if you want exact schema locking. |
| `extension` | Recommended extension metadata. | Informational and embedded in manifests when enabled. It does not rename files. |
| `preset` | `varve_native` or `none`. | Omitted means native unless a custom layout is declared. `none` lets the declared layout own byte zero. |
| `index` | `scan_on_open`, `checkpoint_on_flush`, or a list. | Offset-chain indexes are written automatically in `VARVE3` footers. |
| `commit` | `none`, `record_footer`, `transaction_marker(on_flush)`, `transaction_marker(explicit)`, or matrix `cell_bitmap`. | Controls reader visibility and writer tail behavior. |
| `integrity` | `none`, `crc32`, or `crc32_with_header`. | CRC modes require the `integrity` Cargo feature for actual validation. They detect corruption, not malicious tampering. |
| `recovery` | `strict` or `truncate_tail`. | Recovery truncation is explicit through recovery open APIs. Read-only open never truncates. |
| `manifest` | `none` or `embedded`. | Embedded manifests are diagnostics/debug metadata. The static spec remains authoritative. |
| `compression` | `none` or `variable_blocks(...)`. | Applies after variable-block canonical encoding. Requires `compression-zstd` when using zstd. |
| `dims`, `aux`, `matrix` blocks | Matrix storage declarations. | Used for bounded direct-addressed grids, not append-log records. |
| `layout` | Custom physical file layout. | Used for external formats with custom segment framing. |

## Read Limits

Resource policy is selected when a reader or writer is opened. A declaration
may provide optional, partial defaults in `limits { ... }`; unknown and
duplicate keys remain compile errors. Missing fields fall back to
`ReadLimits::STANDARD`. Standard policy does not cap total append-log file
length, scan length, record count, segment count, or cumulative index bytes.
It limits one-shot payload allocation and materialization.

| DSL key | Runtime ceiling |
| --- | --- |
| `file_len`, `scan_bytes` | snapshot file length and bytes advanced by one scan |
| `records`, `index_bytes` | native record count and resident index storage |
| `record_payload`, `logical_payload` | stored and decoded size of one payload |
| `materialized_bytes` | cumulative bytes decoded by one lookup, merge, compact, migration, or diagnostic operation |
| `segments` | custom-layout segment count |
| `matrix_dimension`, `matrix_cells` | one dimension and checked derived cell count |
| `matrix_bitmap`, `matrix_crc`, `matrix_metadata`, `matrix_slot_region` | matrix allocation and physical-region ceilings |
| `sidecar` | complete matrix sidecar size |
| `mmap` | bytes owned by one mmap handle |
| `keyed_tail` | resident keyed-tail cache of one keyed block id: the map that resolves a keyed append's predecessor in O(1). Charged before the memory is taken, per block id, covering both the map's initial build from file content and its later growth, as inline storage plus (on the resident paths) the key payload bytes the map owns. The build charge is the structural peak and is larger than the map it produces |

Compatibility APIs tighten the resolved policy:

```rust
use varve::ReadLimits;

let tighter = ReadLimits::missing()
    .with_max_file_len(512 * 1024 * 1024)
    .with_max_records(100_000);
let reader = AppFormat::open_reader_with_limits("data.vrv", tighter)?;
# Ok::<(), varve::Error>(())
```

Use the resource-policy APIs when a runtime decision must raise or lower an
optional declaration default:

```rust
let runtime = ReadLimits::missing()
    .with_max_record_payload_len(512 * 1024 * 1024)
    .with_max_logical_payload_len(1024 * 1024 * 1024);
let reader = AppFormat::open_reader_with_resource_limits("data.vrv", runtime)?;
# Ok::<(), varve::Error>(())
```

`Missing`, `Finite(u64)`, and `TrustedUnbounded` are distinct states, and zero
is a valid finite ceiling. `limits: trusted_unbounded;` is reserved for inputs
whose provenance is already trusted. Such a declaration still requires a
visibly named `*_trusted_unbounded` open method; an ordinary method never
silently selects that path. Finite fields supplied alongside a trusted policy
remain enforced.

## What The Macro Generates

For the `AppFormat` declaration above, the macro generates:

| Generated item | Purpose |
| --- | --- |
| `Point` and `User` | Rust structs for the declared blocks. |
| `impl VarveBlock for Point/User` | Block id, version, kind, endian, and field descriptor metadata. |
| `impl VarveEncode` and `impl VarveDecode` | Canonical field encoding/decoding. |
| `impl VarveKeyedBlock for User` | Key extraction because `key = [id]` was declared. |
| `AppFormat` | Zero-sized namespace for the format. |
| `AppFormat::spec()` | Static runtime `FormatSpec`. |
| `open_*_with_limits` | Typed open with field-wise runtime tightening. |
| `open_*_with_resource_limits` | Typed open with runtime override that may raise or lower defaults. |
| `open_*_trusted_unbounded` | Explicit trusted-input boundary; finite fields remain active. |
| `AppFormatWriter` and `AppFormatReader` | Typed wrappers over `VarveWriter` and `VarveReader`. |
| `AppFormatWrite` and `AppFormatRead` | Generated typed traits for the format methods. |
| `push_point`, `push_user` | Typed append methods. |
| `points`, `users` | Typed read collections. |
| `delete_user` | Tombstone helper for keyed variable blocks. |
| Layout wrappers | Generated only when `layout { ... }` is declared. |
| Matrix wrappers | Generated only when `matrix` blocks are declared. |

Application code should not write native record headers, block ids, payload
lengths, commit markers, or offset-chain footers manually. Those are generated
and maintained by the runtime.

## Block Declarations

### Fixed Blocks

```rust
fixed Point(id = 1, version = 1) {
    x: u32,
    y: u32,
}
```

Fixed blocks are positional and all fields are required. They are called
`fixed` because their canonical encoded shape is expected to stay stable, not
because Varve copies Rust memory layout. Varve encodes each field with its
canonical codec and configured endian.

Use fixed blocks for:

- small stable records;
- records where all fields are always present;
- copy-on-write replacement, when the replacement encodes to the same payload
  size and already-open readers must retain their snapshots.

Important detail: if you put variable-length field types in a fixed block,
Varve will still use canonical encoding. Same-size replacement may fail because
the encoded payload size can change. The separately named unsafe exclusive API
is the only append-log path that overwrites the existing generation in place.

### Variable Blocks

```rust
variable User(id = 2, version = 1, key = [id]) {
    id: u64,
    name: String,
    tags: Vec<String> = default,
}
```

Variable blocks encode fields independently:

```text
field_id + wire_type + length + payload
```

In format-first declarations, field ids are assigned from declaration order
starting at `1`. The generated field descriptors keep the id, name, wire type,
and required/defaulted presence. Unknown field ids with known wire types can be
skipped by newer or older readers. Unknown wire type values are malformed data.

Use variable blocks for:

- records expected to evolve;
- optional/defaulted fields;
- keyed latest-value collections;
- larger payloads that may benefit from compression.

`= default` is allowed only on variable blocks. When an older file lacks that
field, the Rust default for the field type is used.

### Keyed Blocks

`key = [id]` creates a single-field key. `key = [user_id, region]` creates a
composite tuple key.

```rust
let reader = AppFormat::open_reader("data.vrv")?;
let users = reader.users()?;
let maybe_user = users.get(&7)?;
```

For repeated keys, keyed lookup returns the newest put record by sequence.
Tombstones and user-defined ops are visible through the materialized keyed
state APIs.

### Derive-First Blocks

When structs must live outside the format declaration, use `#[derive(VarveBlock)]`:

```rust
use varve::{VarveBlock, varve_format};

#[derive(Clone, Debug, PartialEq, VarveBlock)]
#[varve(id = 2, version = 1, kind = "variable", key = "id")]
struct User {
    #[varve(field_id = 1)]
    id: u64,
    #[varve(field_id = 2)]
    name: String,
}

varve_format! {
    pub struct AppFormat {
        magic: b"APPDATA";
        version: 1;
        limits {
            file_len: 8_589_934_592;
            records: 4_000_000;
            index_bytes: 536_870_912;
            scan_bytes: 8_589_934_592;
            record_payload: 67_108_864;
            logical_payload: 268_435_456;
            materialized_bytes: 1_073_741_824;
        }
        endian: little;
        blocks: [User];
    }
}
```

Derive-first is useful for shared domain structs. Format-first is preferred
when you want one declaration to generate the structs, registry, and typed API.

## Native File Bytes

Native Varve files are append-log files. The high-level shape is:

```text
user magic
Varve container header
optional file-header extensions
append record
append record
append record
...
```

The native file header contains:

| Field | Meaning |
| --- | --- |
| user magic | The declared `magic` bytes. |
| container marker | `VARVE1`, `VARVE2`, or `VARVE3`. |
| format version | Declared top-level `version`. |
| endian byte | Declared endian. |
| flags | Header-level runtime flags. |
| schema hash | Declared or computed schema hash. |
| optional extension length and bytes | Used by file-explicit compression and footer-capable native variants. |

Container marker selection:

| Marker | When used |
| --- | --- |
| `VARVE1` | Plain native append-log records. |
| `VARVE2` | File-header extensions are present, such as file-explicit compression. |
| `VARVE3` | Record footers, transaction markers, or offset-chain indexes are needed. |

Each native record has a 32-byte header:

| Field | Meaning |
| --- | --- |
| `block_id` | User block id or reserved internal block id. |
| `block_version` | Per-block version. |
| `flags` | Stored-payload flags such as compressed/internal. |
| `sequence` | Monotonic record sequence in the file. |
| `payload_len` | Physical stored payload byte length. |
| `checksum` | CRC field when integrity is enabled, otherwise structural value. |
| `uncompressed_len_hint` | Logical length hint for compressed records, otherwise zero. |

The stored payload follows the header. For typed reads, Varve then:

1. checks block id and block version against the static spec;
2. validates checksum when enabled;
3. decompresses when the record flag and compression policy require it;
4. decodes the logical block payload with canonical codecs.

Physical APIs such as `scan()`, `RecordIndexEntry::read_payload`, and mmap
payload windows expose stored physical bytes. Typed collections expose logical
decoded values.

The path-taking `RecordIndexEntry` convenience methods are explicitly
non-snapshot and reopen the current pathname. Use their limited variants only
for low-level tools that establish entry provenance, and use generated readers
or typed collections when snapshot identity is required.

When the file uses `VARVE3`, each visible user record can have a 32-byte footer:

| Footer field | Meaning |
| --- | --- |
| magic | `VRF1`. |
| footer version | Footer layout version. |
| footer flags | Which offset-chain fields are present. |
| `prev_same_block_offset` | Previous record offset for this block id, or zero. |
| `prev_same_key_offset` | Previous record offset for this key, or zero. |
| footer CRC/reserved | Reserved footer fields. |

If `index` includes `block_offset_chain` or `keyed_offset_chain`, the generated
writer fills these offsets. Caller code does not calculate them.

## Write And Read Semantics

Creating a new native file:

```rust
let mut writer = AppFormat::create_writer("data.vrv")?;
writer.push_point(&Point { x: 1, y: 2 })?;
writer.push_user(&User {
    id: 7,
    name: "Ada".to_string(),
    tags: Vec::new(),
})?;
writer.flush()?;
```

Appending to an existing native file:

```rust
let mut writer = AppFormat::open_writer("data.vrv")?;
writer.push_user(&User {
    id: 7,
    name: "Grace".to_string(),
    tags: vec!["math".to_string()],
})?;
writer.flush()?;
```

Reading is snapshot-on-open:

```rust
let reader = AppFormat::open_reader("data.vrv")?;
let points = reader.points()?;
let first_point = points.get(0)?;

let users = reader.users()?;
let latest_for_id_7 = users.get(&7)?;
```

Readers do not live-tail a writer. Open a new reader when you need a later
committed snapshot.

For append logs, snapshot means the originally opened file object plus the
validated logical EOF. Renaming or replacing the pathname does not rebind lazy
reads. It is not a byte copy or an OS write lease: a process that can mutate the
same underlying object can still change bytes in place. Coordinate all writers
and enable `crc32` or `crc32_with_header` when covered-byte corruption must be
detected.

Writers are single-writer per file. Varve uses a sidecar lock file by default.
Stale lock breaking is explicit; normal create/open/recover paths refuse an
existing lock.

## Commit And Durability

`push_*` appends through the current file handle. It is not a per-record fsync.

| Call | Effect |
| --- | --- |
| `flush()` | Flush buffered records and write configured manifest/checkpoint/transaction marker records. |
| `commit()` | For `transaction_marker(explicit)`, append a marker that defines reader visibility. |
| `sync()` | Ask the OS to durably persist the file. |

Commit policies:

| Policy | Reader visibility |
| --- | --- |
| `commit: none` | Complete readable records are visible. |
| `commit: record_footer` | A record is visible only with a valid footer. |
| `commit: transaction_marker(on_flush)` | `flush()` writes an internal marker; readers see marker-covered records. |
| `commit: transaction_marker(explicit)` | Only records before the latest explicit `commit()` marker are visible. |

For transaction-marker formats, opening a writer truncates uncommitted tail
after the latest valid marker so stale tail bytes cannot accidentally become
committed by a later append.

## Index Policies

`index` controls open-time scanning and extra metadata:

| Policy bit | Meaning |
| --- | --- |
| `scan_on_open` | Build the in-memory index by scanning records. |
| `checkpoint_on_flush` | Write an internal checkpoint record on flush so later opens can reuse known index entries. |
| `block_offset_chain` | Store previous same-block offset in record footers. |
| `keyed_offset_chain` | Store previous same-key offset for generated keyed writer paths. |

Offset chains are a physical acceleration/debug structure. The typed API still
returns normal block collections and keyed collections.

With `keyed_offset_chain`, the generic `VarveFile::push` / `push_info` and
`VarveWriter::push` / `push_info` refuse a keyed block with
`Error::KeyedChainRequiresKeyedApi { block_id }` rather than writing a truncated
chain, because a generic `T` does not expose its key to the file. Use the
maintaining `push_keyed` / `push_keyed_info`, or the generated typed keyed
writer, which already maintains the chain. `delete` maintains it directly.

## Compression

Compression is disabled by default. This declaration enables global compression
for variable user blocks:

```rust
compression: variable_blocks(
    zstd,
    level = fast,
    header = record_explicit,
    min_len = 1024,
    only_if_smaller = true,
    max_len = 67108864,
);
```

Compression happens after variable-block canonical encoding and before native
record write. Fixed blocks, internal records, native record headers, native
record footers, and matrix slots are not compressed by this policy.

Header modes:

| Mode | Stored metadata |
| --- | --- |
| `record_explicit` | Each compressed payload carries a `VCMP` envelope. Most self-describing. |
| `file_explicit` | Compression metadata is stored once in a `VARVE2` file-header extension. |
| `format_contract` | No algorithm metadata in the file; the static schema is the contract. |

Build with `--features compression-zstd` when a file actually writes or reads
zstd-compressed records.

## Integrity And Recovery

`integrity: crc32` enables payload corruption detection when the `integrity`
feature is enabled. In `VARVE3`, the CRC covers stored payload plus footer.
`integrity: crc32_with_header` also covers the native record header with the
checksum field normalized to zero.

CRC32 is not authentication. It detects accidental corruption; it does not stop
a malicious writer from recomputing checksums.

Recovery policy decides what can happen to damaged tails:

| Recovery | Meaning |
| --- | --- |
| `strict` | Reject incomplete or corrupt tails. |
| `truncate_tail` | Recovery open may truncate incomplete tails and return a report. |

Read-only open never truncates. Use `open_recover` or
`open_recover_with_report` when recovery is intended.

## Schema Evolution

Keep these rules stable:

- Do not reuse a block id for a different meaning.
- Bump the block `version` for incompatible payload changes.
- For variable blocks, append new fields at the end and prefer `= default` when
  old files should still decode.
- Do not change the meaning of an existing field id.
- Normal typed reads reject block-version mismatches.
- Semantic migrations are explicit user Rust code through migration traits and
  migration read APIs.

`schema_hash: computed` catches accidental changes to the declared contract,
including field declaration reorder, per-block endian changes, keyedness
changes, and generated codec identity changes — the hash covers wire layout,
not just field membership. For released formats, compute the hash, pin it as a
literal, and require a migration path for incompatible changes. Note that the
hash algorithm itself is versioned pre-1.0
(`FormatSpec::SCHEMA_HASH_ALGORITHM_VERSION`); an algorithm revision changes
every computed value, so pinned literals must be re-derived when it moves.

## Matrix Storage

Matrix blocks are not append-log records. They are for bounded runtime-sized
grids with direct cell addressing.

```rust
varve_format! {
    pub format AnalysisFormat {
        magic: b"ANALYSIS";
        version: 1;
        limits {
            file_len: 8_589_934_592;
            record_payload: 67_108_864;
            materialized_bytes: 268_435_456;
            matrix_dimension: 16_000_000;
            matrix_cells: 16_000_000;
            matrix_bitmap: 64_000_000;
            matrix_crc: 128_000_000;
            matrix_metadata: 268_435_456;
            matrix_slot_region: 8_589_934_592;
            sidecar: 268_435_456;
        }
        schema_hash: computed;

        dims {
            scan: u32,
            ch: u32,
        }

        commit: cell_bitmap {
            keyspace = [scan, ch];
            categories = [analysis];
        }

        aux {
            thumbnail: 1024,
        }

        blocks {
            matrix Cell(id = 10, dims = [scan, ch], category = analysis) {
                value: u32,
            }
        }
    }
}
```

At create time, matrix files store runtime dimensions, preallocate commit
bitmaps, preallocate fixed-stride slot regions, and optionally preallocate aux
regions. Reads trust commit bits. If a cell is not committed, Varve returns
`MatrixNotCommitted` even if bytes exist in the slot.

Use matrix storage for direct-addressed grids. Use append-log blocks for event
streams, versioned records, and keyed latest-value collections.

## Custom Physical Layouts

Use custom physical layouts when the file bytes must follow an external
segmented format. This is separate from native append-log blocks.

```rust
varve_format! {
    pub format FramedFormat {
        magic: b"FRAM";
        version: 1;
        limits {
            file_len: 8_589_934_592;
            scan_bytes: 8_589_934_592;
            segments: 4_000_000;
            index_bytes: 536_870_912;
            record_payload: 268_435_456;
        }
        schema_hash: computed;
        extension: "frame";
        preset: none;

        layout {
            file_header Header {
                bytes signature = b"VRV!";
                u16 header_version = 1;
            }

            segment DataSegment repeat until_eof {
                lead_in DataLeadIn {
                    bytes tag = b"SEGM";
                    u32 toc_mask;
                    u64 next_segment_offset =
                        finalize(target = segment_end, relative_to = after_lead_in);
                    u64 raw_data_offset =
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

With `preset: none`, Varve does not write the native `VARVE1/2/3` header.
Instead, the declared `file_header`, segment `lead_in`, `metadata`,
`raw_region`, and optional `footer` own the physical bytes.

Field sources:

| Field form | Writer behavior | Reader behavior |
| --- | --- | --- |
| `bytes tag = b"SEGM"` | Writes literal bytes. | Validates exact bytes. |
| `u32 toc_mask` | Caller supplies the value. | Exposes validated field value. |
| `u64 next_segment_offset = finalize(...)` | Writes placeholder, then backpatches computed offset. | Validates offset against segment bounds. |
| `metadata Metadata` | Caller supplies opaque metadata bytes. | Reader exposes whole or ranged metadata bytes. |
| `raw_region Raw` | Caller supplies opaque raw bytes. | Reader exposes whole or ranged raw bytes. |
| `footer Footer { ... }` | Writes footer after raw bytes. | Validates footer and excludes it from raw bytes. |

Generated layout APIs are named from the segment:

```rust
let mut writer = FramedFormat::create_layout_writer("data.frame")?;
writer.write_data_segment(FramedFormatDataSegmentLayoutWrite {
    fields: FramedFormatDataSegmentLayoutFields { toc_mask: 0x1108 },
    footer_fields: FramedFormatDataSegmentLayoutFooterFields,
    metadata: b"metadata bytes",
    raw: b"raw bytes",
})?;
writer.flush()?;

let reader = FramedFormat::open_layout_reader("data.frame")?;
let first = reader.data_segment(0)?.unwrap();
let raw = reader.read_data_segment_raw(0)?;
```

Custom layout metadata and raw bytes intentionally remain opaque. Varve owns
physical framing, backpatching, bounds checks, and range reads. The adapter
author owns domain metadata parsing, tagged value meaning, channel models,
object paths, scaling, or sidecar semantics.

## Varve-Owned Versus Caller-Owned

| Area | Varve owns | Caller owns |
| --- | --- | --- |
| Native headers/records | Header, record framing, payload length, sequence, checksum field, footer, offset chains. | Choosing ids, versions, policies, and when to flush/sync. |
| Block encoding | Canonical field encoding and decoding. | Domain validation and custom codecs for custom field types. |
| Variable evolution | Field ids, unknown-field skipping, defaulted fields. | Deciding semantic compatibility and migrations. |
| Keyed state | Latest put, tombstone, op application hooks, merge/compact helpers. | Key design and merge operation meaning. |
| Physical layouts | Literal validation, finalized offsets, region bounds, metadata/raw range IO. | External format metadata grammar and public adapter API. |
| Diagnostics | Static spec checks, file diagnostics, self-test classification. | Interpreting domain-specific bad data and support policy. |

## Trust But Verify

Before using a new format with real data:

```rust
let report = AppFormat::self_test("app.selfcheck.vrv")
    .with_block(Point { x: 1, y: 2 })
    .with_keyed_block(User {
        id: 7,
        name: "Ada".to_string(),
        tags: Vec::new(),
    })
    .cleanup(true)
    .run();

assert!(report.passed(), "{report:#?}");
```

Then add an application-level roundtrip test that writes representative domain
values, reopens a reader, and checks exact values. If a failure happens,
`diagnose_file(path)` classifies whether to inspect the format declaration,
caller usage, feature gates, file data, environment, or a possible library
invariant.

## Current Stability

Varve 0.3 remains alpha software at the Rust API layer. Valid native 0.1 wire
bytes remain readable, and incompatible future wire changes require an explicit
migration path. Keep representative byte fixtures and migration tests around
data that matters even when a release promises wire compatibility.
