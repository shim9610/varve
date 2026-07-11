# Format Author Guide

Use Varve when you want a typed, append-friendly binary format with a stable
format contract. The primary API is format-first: declare the format, blocks,
fields, and policies, and `varve_format!` generates the block structs plus typed
reader/writer wrappers.

If you are new to the crate, read this together with:

- `docs/quickstart.md` for the shortest working example.
- `docs/how-it-works.md` for the runtime model behind the generated API.
- `docs/api-reference.md` for method names and public extension points.

## Define A Format

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
            fixed Point(id = 100) {
                x: u32,
                y: u32,
            }

            variable User(id = 101, key = [id]) {
                id: u64,
                name: String,
                flags: u32 = default,
            }
        }
    }
}
```

This generates `Point`, `User`, `AppFormatReader`, `AppFormatWriter`,
`AppFormatRead`, and `AppFormatWrite`. Required record metadata such as block
ids, field ids, wire types, payload lengths, commit marker handling, CRC checks
when enabled, and offset-chain footers are handled by the generated code and
runtime writer. Application code should not hand-build Varve record headers or
offset chains in normal use.

The `limits` block is part of the runtime safety contract, not the wire schema.
Choose ceilings from the largest legitimate dataset the application accepts,
including cumulative materialization rather than only one record. Runtime code
may pass a tighter `ReadLimits` value to generated `*_with_limits` methods. Do
not use `limits: trusted_unbounded;` for files supplied by users, networks, or
other processes; that policy is reached only through visibly named trusted
open methods.

Choose `fixed` for records whose canonical encoded payload size should stay
stable. Fixed blocks still use Varve's canonical field codec, not Rust memory
layout. Choose `variable` for evolvable records. Variable fields are encoded as
field id, wire type, length, and payload, so unknown field ids with known wire
types can be skipped.

## Matrix Blocks

Matrix blocks are for bounded runtime-sized grids that need direct cell access
and in-place same-size overwrite. They are not append-log records. A matrix file
stores runtime dimensions at create time, preallocates deterministic slot
regions, and uses commit bitmaps as the only normal read-time validity source.

The intended authoring shape is:

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
            n_scans: u32,
            n_channels: u32,
            n_wells_max: u32,
        }

        commit: cell_bitmap {
            keyspace = [scan, ch];
            categories = [analysis];
        }

        blocks {
            matrix AnalysisCell(id = 10, dims = [scan, ch], category = analysis) {
                rfu: [f32; n_wells_max],
            }
        }
    }
}
```

The first implementation focuses on dense `(scan, ch)` addressing, bounded slot
payloads, `NotCommitted` reads, and same-size in-place overwrites. Append-log
blocks may coexist after the preallocated matrix regions.

Same-size overwrite is fail-safe within one writer: Varve clears the old commit
and CRC-valid evidence before writing slot bytes, and a later explicit commit is
the final visibility step. Partial matrix I/O poisons the writer and leaves the
cell uncommitted. Matrix readers snapshot layout and commit maps, not immutable
copies of every slot. Applications must not overlap a reader with in-place
writes to slots it may read; use external read leases or a higher-level
generation/version scheme when concurrent immutable snapshots are required.

Rules to keep stable:

- Block ids must be explicit `u32` values below `0xFFFF_FF00`.
- Bump a block `version` for incompatible payload changes.
- Reuse generated field positions only for the same meaning and compatible codec.
- Use `= default` only on variable blocks, and only when the Rust default is a valid fallback.
- Use `key = [field]` or `key = [a, b]` only on fields that identify the value.

## Derive-First Form

The older derive-first form remains available when block structs need to live
outside the format declaration:

```rust
use varve::{VarveBlock, varve_format};

#[derive(Clone, Debug, PartialEq, VarveBlock)]
#[varve(id = 100, version = 1, kind = "fixed")]
struct Point {
    x: u32,
    y: u32,
}

#[derive(Clone, Debug, PartialEq, VarveBlock)]
#[varve(id = 101, version = 1, kind = "variable", key = "id")]
struct User {
    #[varve(field_id = 1)]
    id: u64,
    #[varve(field_id = 2)]
    name: String,
    #[varve(field_id = 3, default)]
    flags: u32,
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
        index: checkpoint_on_flush;
        manifest: embedded;
        blocks: [Point, User];
    }
}
```

## Register A Format

`varve_format!` pins the file contract: magic bytes, format version, endian,
required resource limits, optional schema hash, optional extension, optional integrity, optional commit
policy, optional checkpoint/offset-chain index, optional recovery policy,
optional embedded manifest, optional variable-block compression, and registered
blocks.

The static `FormatSpec` is authoritative for typed access. Embedded manifests
are diagnostic; they help inspection and support, but they do not dynamically
load unknown schemas.

Useful inspection helpers:

```rust
let spec = AppFormat::spec();
let computed = spec.computed_schema_hash();
let dump = spec.schema_debug_dump();
let diagnostics = AppFormat::diagnostics();
```

`schema_hash: computed;` is the convenient default in format-first declarations.
For release-pinned schemas, `computed_schema_hash()` can be used to decide what
literal value to pin.

For generated API and file sanity checks, use `AppFormat::self_test(path)` with
representative sample values and `AppFormat::diagnose_file(path)` for existing
files. See `docs/self-check-guide.md`.

Use `AppFormat::inspect_layout_file(path)` when you need to inspect physical
framing. It validates either the Varve-native preset or a custom physical layout
and returns file-header length plus segment/raw/footer ranges.

## Variable Compression

Compression is disabled by default. Enable it only for variable user blocks
whose canonical payloads are large enough to benefit. The macro-level syntax
sets one global variable-block policy:

```rust
varve_format! {
    pub struct CompressedFormat {
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
        extension: "vrv";
        compression: variable_blocks(
            zstd,
            level = default,
            header = record_explicit,
            min_len = 1024,
            only_if_smaller = true,
            max_len = 67108864,
        );
        blocks: [User];
    }
}
```

The zstd backend is optional; build or test with `--features compression-zstd`
when a format actually writes or reads compressed records.

For finer control in derive-first or manually assembled specs, attach
block-specific policies with `FormatSpec::with_block_compression`. These
overrides are intentionally limited to `record_explicit` compression, so each
compressed block carries its own `VCMP` envelope and no extra file-header
contract is required:

```rust
use varve::{
    BlockCompressionDescriptor, CompressionAlgorithm, CompressionHeaderMode,
    CompressionLevel, VariableCompression,
};

static BLOCK_COMPRESSION: &[BlockCompressionDescriptor] = &[BlockCompressionDescriptor {
    block_id: LargeBlob::ID,
    compression: VariableCompression {
        algorithm: CompressionAlgorithm::Zstd,
        level: CompressionLevel::Fast,
        header_mode: CompressionHeaderMode::RecordExplicit,
        min_uncompressed_len: 1024,
        only_if_smaller: true,
        max_uncompressed_len: 64 * 1024 * 1024,
    },
}];

let spec = AppFormat::spec().with_block_compression(BLOCK_COMPRESSION);
```

Block-specific compression only accepts registered `variable` blocks. Fixed
blocks and internal records remain uncompressed.

Compression modes:

- `record_explicit`: stores a `VCMP` envelope in each compressed record. This is
  the safest default because the record carries algorithm, level, and logical
  length.
- `file_explicit`: stores algorithm and level once in a `VARVE2` file header
  extension. This is smaller per record, but the format must keep
  `max_len <= u32::MAX`.
- `format_contract`: stores no algorithm/level bytes. Use
  `spec.with_computed_schema_hash()` and pin that hash, because the static
  format spec is the compression contract.

Physical APIs such as `scan()`, `RecordIndexEntry::read_payload`, and mmap
payload windows expose stored compressed bytes. Typed reads through
`blocks::<T>()`, migration reads, and merge/materialization return decompressed
logical values.

## Length Limits And Caller Policy

Varve defines the wire contract and generated reader/writer shape; it does not
know every domain's safe maximum string, vector, map, chunk, or decompressed
payload size. Built-in codecs avoid avoidable allocation-before-validation, but
format authors should still enforce domain limits in custom codecs, adapters,
compression `max_len`, or caller validation.

For caller-managed compressed blobs, prefer
`ChunkedBytes::decode_to_vec_limited(limit)` over unbounded decoding when the
limit is part of the format contract.

For physical records, use
`RecordIndexEntry::read_payload_limited(path, physical_limit)` to cap stored
bytes, or
`read_logical_payload_limited(spec, path, physical_limit, logical_limit)` to cap
both the stored compressed allocation and post-decompression logical allocation
before either allocation. These are caller policy controls; Varve does not guess
a global domain ceiling.

## Integrity Policy

`integrity: none` performs structural parsing only. It does not detect payload
bit flips unless decoding happens to fail.

`integrity: crc32` validates each record payload when the `integrity` feature is
enabled. In `VARVE3` files, the record CRC covers the stored payload plus the
record footer, so commit/offset metadata is validated with the payload.

`integrity: crc32_with_header` additionally covers the native 32-byte record
header with the checksum field normalized to zero. Use it when block id,
version, flags, sequence, payload length, or length hints must be protected by
the record checksum too.

Checkpoint payload CRC mismatches are fatal; recovery truncates incomplete
tails but does not hide complete CRC mismatches in visible records.
Transaction-marker readers may ignore corrupt tail after the latest valid
marker because that tail is not part of the reader snapshot.

## Commit And Offset Chains

`commit: none;` keeps the legacy append behavior. `commit: record_footer;`
stores a footer after each record and treats a valid footer as that record's
commit flag. `commit: transaction_marker(on_flush);` writes an internal marker
on `flush`, while `transaction_marker(explicit)` writes one only when
`commit()` is called.

`commit()` is a logical visibility marker, not a hidden fsync. Use
`commit_durable()` when records covered by an explicit marker must be flushed
and synced before the marker is published and synced.

Offset chains are enabled through `index`:

```rust
index: [scan_on_open, block_offset_chain, keyed_offset_chain];
```

The writer automatically fills `prev_same_block_offset` for each block id and,
for generated keyed writers, `prev_same_key_offset` for the key. User code never
has to calculate or patch offsets.

## Custom Physical Layout

Most formats should use the Varve-native append log. Use custom physical layout
when you need the file bytes themselves to follow another segmented format, for
example a TDMS-style lead-in with ToC mask, next segment offset, raw data
offset, metadata bytes, and contiguous raw channel data.

`preset: none;` gives your layout ownership of byte zero. You can declare a
literal or caller-filled file header, segment lead-in fields, opaque metadata
bytes, raw-region bytes, and optional footer fields:

These declarations are physical field groups. They intentionally do not reuse
`VarveBlock`: logical blocks describe native append-log payloads, while
`file_header`, `lead_in`, and `footer` describe bytes used to frame a custom
external format.

```rust
varve_format! {
    pub format PhysicalFormat {
        magic: b"PHYS";
        version: 1;
        limits {
            file_len: 8_589_934_592;
            scan_bytes: 8_589_934_592;
            segments: 4_000_000;
            index_bytes: 536_870_912;
            record_payload: 268_435_456;
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

For format-first declarations, the macro generates a typed physical-layout
facade. `PhysicalFormat::create_layout_writer` returns
`PhysicalFormatLayoutWriter`, and `segment DataSegment` generates
`write_data_segment`, `write_data_segment_streamed`, `data_segments`,
`data_segment`, `read_data_segment_metadata`,
`read_data_segment_metadata_range`, `read_data_segment_raw`, and
`read_data_segment_raw_range`. A declared `file_header` also generates
`file_header()` on the layout reader. Caller fields become typed Rust struct
fields; literal and finalized fields are supplied, validated, or backpatched by
Varve, then exposed through typed info getters.
Finalized numeric fields may use the numeric width required by the target
format. For example, BMP-style headers can use `u32 file_size` relative to
`segment_start`, while TDMS-style headers can use `u64 next_segment_offset`
relative to `after_lead_in`.

Formats can declare more than one segment descriptor. The reader chooses the
descriptor at each file offset from the leading literal lead-in prefix, for
example `bytes tag = b"CTRL"` versus `bytes tag = b"DATA"`. Literal numeric
fields immediately after the tag can extend that prefix, which lets a format use
one byte tag plus a literal kind field. Generated typed reader indexes are per
segment kind: `read_data_segment_raw(0)` reads the first `DataSegment` even if a
different segment appears before it in the physical stream.

The low-level `LayoutWriter::write_segment(SegmentWrite)` and
`write_segment_streamed(SegmentWriteStream)` paths remain exposed through the
generated wrapper for dynamic adapters. Reopen the same file with
`open_layout_writer` when you need to validate the existing segment stream and
append more segments. `open_layout_reader` validates literal tags, offset
fields, header/footer bounds, exposes typed getters on generated
`...LayoutInfo`, and still returns opaque `read_metadata`, `read_metadata_range`,
`read_raw`, and `read_raw_range` methods. Use `inspect_layout_file_report` when
an adapter needs to separate a valid complete prefix from a truncated or invalid
tail before deciding whether the file should be rejected or reported as an
incomplete external-format file.

Returned callback errors trigger truncation back to the original EOF and leave
the writer reusable only when rollback succeeds. A failed rollback poisons the
writer. Panics are not caught; unwinding through a streaming callback also
leaves that handle poisoned. Do not retain or reuse it after catching such a
panic outside Varve.

For concrete compatibility checks, `crates/varve/examples/tdms_physical/common.rs`
is a façade over split TDMS example modules. The physical adapter implementation
lives in `tdms_physical/common/adapter.rs`, fixture/scenario data lives in
`tdms_physical/common/example_data.rs`, and self-verification lives in
`tdms_physical/common/verify.rs`. The example writes a three-segment file with
real `TDSm` lead-ins, TDMS object metadata,
mixed raw channel objects, a changed raw-data-index segment,
`same-as-previous` raw-index reuse, bool/string/integer/float tagged
properties, an adapter-owned `.vtidx` sidecar, and appended raw samples using
the generated physical-layout writer plus adapter toolkit helpers. The combined
`tdms_physical_adapter` example exposes `write`, `append`, `read`, `read-bytes`,
and `inspect` subcommands, and the older writer/reader examples are thin
wrappers over the same shared declaration. The optional Python harness verifies
adapter-authored create+append files with `npTDMS` across signed/unsigned
integer widths, single/double floats, single/double floats with unit type ids,
booleans, strings, timestamps, and complex single/double floats. The reverse
harness creates a two-segment scalar type matrix with `npTDMS`, parses it
through the same Varve-based adapter code, appends one segment with Varve, then
verifies the appended file with both npTDMS and Varve. These examples are not a
Varve-provided TDMS reader/writer feature:

```powershell
python -m venv .venv-tdms
.\.venv-tdms\Scripts\python.exe -m pip install -r scripts\requirements-tdms-harness.txt
.\.venv-tdms\Scripts\python.exe scripts\verify_tdms_with_nptdms.py
.\.venv-tdms\Scripts\python.exe scripts\verify_nptdms_multichannel_with_varve.py
.\.venv-tdms\Scripts\python.exe scripts\verify_bmp_with_pillow.py
```

`npTDMS` and Pillow are used only as external verification harness dependencies,
not as Rust crate dependencies or runtime dependencies of Varve.

## Read And Write

```rust
let mut writer = AppFormat::create_writer("data.varve")?;
writer.push_point(&Point { x: 1, y: 2 })?;
writer.push_user(&User { id: 7, name: "Ada".to_string(), flags: 0 })?;
writer.flush()?; // writes buffered bytes and checkpoint/manifest records
writer.sync()?;  // fsync when the caller needs durable storage

let reader = AppFormat::open_reader("data.varve")?;
let points = reader.points()?;
let users = reader.users()?;
```

Readers are snapshot-on-open. Writers are single-writer per file and use a
sidecar lock. Default create/open/recover paths refuse an existing lock; stale
lock handling is explicit through `inspect_writer_lock` and
`open_with_lock_policy`.

The older `create`, `open`, and `open_readonly` helpers remain available and
return `VarveFile` directly. Prefer `create_writer` and `open_reader` in new
examples when you want API intent to be obvious.

## Performance Check

Run the ignored smoke tests when a format change touches indexing, scanning,
manifest output, integrity, compression, codecs, merge, compact, mmap, or
zero-copy paths. The default ignored smoke test also exercises the external
adapter toolkit path so cursor, chunk-index, and reducer changes show up in the
same regression pass:

```powershell
cargo test -p varve --test perf_smoke -- --ignored --nocapture
cargo test -p varve --features mmap --test perf_smoke -- --ignored --nocapture
cargo test -p varve --features mmap,zero-copy --test perf_smoke -- --ignored --nocapture
```

The output is a regression guard, not a product benchmark. Large unexplained
slowdowns on open, scan, keyed materialization, merge, or compact should block
the change until understood.
