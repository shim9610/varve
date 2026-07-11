# Matrix Storage Design

This document defines the first Varve preallocated matrix storage design. It is
the implementation contract for P0 and the integration point for P1/P2
extensions.

## Goals

- Support runtime-sized matrix blocks whose cells are addressed directly by a
  composite key such as `(scan, ch)`.
- Preserve a bounded file size after creation.
- Allow random-order writes and same-size in-place overwrites.
- Treat commit bitmaps as the only source of matrix cell validity.
- Keep append-log fixed/variable blocks working without semantic changes.

## Non-Goals

- Live reader tailing.
- Multi-writer concurrent mutation of the same file.
- Inferring domain recovery choices.
- Making canonical fixed blocks raw zero-copy by default.
- Replacing append-log merge/compact with matrix semantics.

## Format Model

A format can contain both append-log blocks and matrix blocks:

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
        endian: little;
        schema_hash: computed;
        extension: "vrv";

        dims {
            n_scans: u32,
            n_channels: u32,
            n_wells_max: u32,
        }

        commit: cell_bitmap {
            keyspace = [scan, ch];
            categories = [analysis, markers];
            singles = [master_grid];
            per_channel = [threshold];
        }

        blocks {
            matrix AnalysisCell(id = 10, dims = [scan, ch], category = analysis) {
                tuned_centroids: [[f32; 2]; n_wells_max],
                rfu: [f32; n_wells_max],
                finetune_mask: bitmap(n_wells_max),
                inverse_meta: InverseMeta,
            }

            variable Event(id = 20, key = [id]) {
                id: u64,
                message: String,
            }
        }
    }
}
```

The exact macro grammar may evolve, but the generated runtime must preserve the
semantics in this document.

## Runtime API Shape

Matrix-capable formats generate dimension and key types:

```rust
let dims = AnalysisFormatDims {
    n_scans: 46,
    n_channels: 5,
    n_wells_max: 22000,
};

let mut writer = AnalysisFormat::create_writer_with_dims("run.vrv", dims)?;
writer.write_analysis_cell(
    AnalysisCellKey { scan: 2, ch: 1 },
    &cell,
)?;
writer.commit_analysis_cell(AnalysisCellKey { scan: 2, ch: 1 })?;
writer.flush()?;
writer.sync()?;

let reader = AnalysisFormat::open_reader("run.vrv")?;
let cell = reader.analysis_cell(AnalysisCellKey { scan: 2, ch: 1 })?;
```

Core runtime primitives should be usable without macros:

- `MatrixDims`: persisted runtime dimensions.
- `MatrixKey`: validated logical cell coordinates.
- `MatrixBlockSpec`: block id, version, dimension names, stride, category, and
  codec.
- `MatrixLayout`: computed file offsets and region lengths.
- `MatrixWriter<T>` and `MatrixReader<T>`: typed cell access.
- `MatrixCellStatus::{Committed, NotCommitted}`.
- `MatrixCellError::NotCommitted`.

## Physical Layout

The first matrix container layout is a versioned `VMAT` region immediately after
the normal Varve header. Append-log records, if any, begin at the
`append_log_start` offset stored in the `VMAT` header. Existing append-log files
without matrix blocks continue to scan from the normal Varve header length.

```text
+---------------------------+
| Varve file header         |
+---------------------------+
| Matrix layout header      |
+---------------------------+
| Dimension table           |
+---------------------------+
| Matrix block table        |
+---------------------------+
| Commit bitmap regions     |
+---------------------------+
| Optional offset tables    |
+---------------------------+
| Matrix slot regions       |
+---------------------------+
| Region CRC table          |
+---------------------------+
| Append-log record region  |
+---------------------------+
```

The matrix layout header stores:

- magic `VMAT`
- layout version
- dimension count
- matrix block count
- commit category count
- offsets and lengths for the dimension table, block table, commit maps,
  optional offset tables, slot regions, region CRC table, and append-log start.

All integers in `VMAT` metadata are little-endian in layout version 1. Matrix
slot payloads still use the block/format endian policy for canonical field
encoding. The append-log scanner must start from `append_log_start`, never from
the normal header length, when the static spec contains matrix blocks.

## VMAT Version 1 Header

The P0 header is deliberately simple and dense-layout oriented:

```text
magic                 [u8; 4] = b"VMAT"
layout_version        u16 = 1
flags                 u16
header_len            u32
dimension_count       u32
matrix_block_count    u32
commit_category_count u32
dimension_table_off   u64
dimension_table_len   u64
block_table_off       u64
block_table_len       u64
commit_category_off   u64
commit_category_len   u64
commit_map_off        u64
commit_map_len        u64
slot_region_off       u64
slot_region_len       u64
region_crc_off        u64
region_crc_len        u64
append_log_start      u64
reserved              [u8; 32]
```

Tables use length-prefixed UTF-8 names and fixed-width integers. Offsets are
absolute file offsets from the beginning of the file. P0 does not require
alignment padding beyond what the tables explicitly encode. Readers must reject
table ranges that overlap incorrectly, point outside the file, or place
`append_log_start` before the end of the final matrix region.

P0 or `integrity: none` files set `region_crc_off = 0` and
`region_crc_len = 0`. Static auxiliary regions, when declared by the format
spec, are derived in declaration order immediately after the slot region. VMAT
v1 does not store an auxiliary table; readers reconstruct aux offsets from the
static `FormatSpec`, so changing aux names, byte lengths, or declaration order
is a schema change.
When `integrity: crc32` is enabled, the region CRC table is placed immediately
after the derived aux region and before `append_log_start`.

Region CRC table (`MCRC` v1):

```text
magic          [u8; 4] = "MCRC"
version        u16 = 1
reserved       u16
metadata_crc32 u32  // dimension table + block table + commit category table
reserved       u32
commit_crc32   [u32; commit_category_count]
slot_crc32     [u32; sum(matrix_block.cell_count)]
slot_valid     [packed bits; sum(bit_bytes(matrix_block.cell_count))]
```

Commit CRC entries follow the declared commit-category order. Slot CRC entries
are dense in matrix-block declaration order, then ordinal order within that
block. Slot valid bitmaps use the same block and ordinal order and distinguish
a committed all-zero payload from an untouched all-zero slot during commit-map
rebuild. The table has no per-entry offset fields because offsets are derived
from the VMAT tables and runtime dimensions.

Dimension table entry:

```text
name_len u16
name     [u8; name_len]
value    u64
```

Matrix block table entry:

```text
block_id       u32
block_version  u16
dimension_0    u16  // index into dimension table
dimension_1    u16  // index into dimension table
category       u16  // index into commit category table
slot_stride    u64
cell_count     u64
slot_region_off u64
slot_region_len u64
```

Commit category table entry:

```text
name_len u16
name     [u8; name_len]
kind     u8   // 1 = cell, 2 = single, 3 = per_channel
reserved [u8; 3]
bit_count u64
map_off   u64
map_len   u64
```

Future versions may extend these tables with additional recovery metadata by
increasing `layout_version`; v1 keeps CRC offsets derived from table order.

## P0/P1/P2 Split

P0 includes dense matrix declarations, runtime dimensions, `VMAT` layout
persistence, fixed-stride slots, direct addressing, commit maps, same-size
overwrite, `NotCommitted`, and mixed matrix plus append-log scan behavior.
In VMAT v1, a cell commit category belongs to exactly one matrix block.

P0 excludes recovery decisions, sidecars, compression, zero-copy, sparse or
offset-table-backed matrices, declarative migration, and per-cell crash
durability. Those are added as P1/P2 extensions.

P1 adds region/per-entry CRCs, recovery classification, primitive recovery
actions, ordered durability barriers, post-commit hooks, and sidecar/resume
signals.

P2 adds checked borrowed views, packed bitmap/numeric positional access,
per-block chunk compression, auxiliary noncommit regions, and migration
scaffolds.

The current implementation exposes durable write hooks, resume advisory
signals, `PackedBitmap`, checked slot payload reads, read-only matrix mmap
payload windows, safe endian-aware numeric scalar views, opt-in raw matrix
zero-copy views, static auxiliary noncommit regions, and the `integrity: crc32`
VMAT CRC table for metadata, commit maps, and per-cell slot payloads. Commit
maps can be rebuilt from valid per-cell CRC evidence. Compatible committed
matrix slots can be byte-copied into a target writer through the migration
scaffold API. `ChunkedBytes` supplies chunked zstd + per-chunk CRC32 for
caller-managed blobs such as variable fields or aux payloads. VMAT-native
direct-slot chunk compression, numeric batch slice helpers, and bulk migration
publication remain future work.

Matrix mmap construction is unsafe because external file mutation cannot be
enforced across processes. Under the caller's immutability guarantee, Varve
maps the existing handle read-only, validates the complete VMAT extent before
construction, checks every slot range, verifies configured CRC evidence, and
ties returned slices to the mapping owner.

## Slot Addressing

For dense rectangular keyspaces, deterministic addressing is preferred:

```text
ordinal = scan * n_channels + ch
offset = block.slot_region_start + ordinal * block.slot_stride
```

Bounds are checked before offset calculation:

- `scan < n_scans`
- `ch < n_channels`
- multiplication and addition must not overflow `u64`
- final `offset + slot_stride` must stay within the preallocated region

Offset tables are optional and reserved for future sparse or variable-stride
matrix modes. P0 dense matrix blocks should not need to read an offset table for
normal `(scan, ch)` addressing, but the layout records the deterministic formula
so tooling can distinguish it from future table-backed modes.

## Slot Payloads

Each matrix block has a fixed `slot_stride` computed from declared bounded field
shapes and runtime dimensions. The writer encodes the cell into a scratch buffer
and requires `encoded_len == slot_stride` for in-place writes.

Variable logical content must be bounded in the declaration:

- fixed arrays use literal lengths or runtime dimensions.
- numeric streams use a declared maximum length and are zero-padded.
- bitmap fields use packed bits with deterministic byte length.
- unsupported unbounded values such as plain `String` are rejected for P0 matrix
  slots unless a bounded representation is declared.

Same-size overwrite writes exactly one slot range and never changes file size.
Wrong-size writes return `MatrixSizeMismatch`.

## Commit Bitmap

Commit maps are independent regions. A matrix cell is readable only when the
matching category bit is set. Physical nonzero bytes in the slot are ignored
when the bit is clear.

`commit_map_off` points to packed bitmap bytes only. Category names, kinds, bit
counts, and bitmap offsets live in the separate category table at
`commit_category_off`.

Commit dimensions:

- cell category: one bit per `(scan, ch)` cell.
- single category: one global bit.
- per-channel category: one bit per channel.

Commit map APIs:

- `is_cell_committed(category, key) -> bool`
- `set_cell_committed(category, key, value)`
- `clear_cell(category, key)`
- `clear_category(category)`
- generated typed helpers such as `commit_analysis_cell(key)`

P0 generated APIs also expose single and per-channel flag helpers when declared:

- `is_<single>_committed()`
- `set_<single>_committed(value)`
- `is_<per_channel>_committed(channel)`
- `set_<per_channel>_committed(channel, value)`

Cell commits require a prior successful slot write in the current file history.
This prevents accidentally committing zero-preallocated bytes as valid data.
Clearing a cell does not erase its written marker; rewriting the slot and
recommitting remains allowed.

Commit bits are packed LSB-first within each byte. Bits beyond the logical count
in the final byte must be zero on write and ignored on read unless strict
validation is requested.

## Write Sequence

The default durable matrix write sequence is:

1. encode and validate slot payload
2. write slot bytes at the deterministic offset
3. update affected per-entry CRC metadata when enabled
4. sync data region according to durability policy
5. set commit bit
6. sync commit map according to durability policy
7. call post-commit hook when configured

Without a durable barrier policy, `flush` and `sync` remain explicit like the
append-log writer and there is no crash-durability guarantee for each individual
cell write. The commit map still remains the reader-visible validity source.

P0 `commit_*` therefore means logical visibility after a slot write reached the
file handle. P1 ordered barriers add the stronger requirement that data/index are
durably synced before the commit bit is synced and hooks are emitted.

## Reader Semantics

Opening a matrix file creates a snapshot of layout metadata and commit maps.
It does not copy the preallocated slot region. Applications must not overlap a
reader with an in-place write to a slot that reader may access. True immutable
concurrent snapshots require versioned slots/generations or a read-lease design
outside VMAT v1.
Reading a cell:

1. validates the key
2. checks the relevant commit bit
3. returns `NotCommitted` if the bit is clear
4. reads exactly the slot range if committed
5. validates per-entry or region CRC when enabled
6. decodes or returns a checked view depending on the access mode

Readers do not infer validity from slot bytes, record footers, or append-log
transaction markers.

## Generated API Contract

For a matrix block `AnalysisCell`, the macro should generate:

- `AnalysisCellKey`
- `AnalysisFormatDims`
- `write_analysis_cell(key, &value)`
- `commit_analysis_cell(key)`
- `clear_analysis_cell(key)`
- `analysis_cell(key) -> Result<AnalysisCell>`
- `analysis_cell_status(key) -> Result<MatrixCellStatus>`
- optional view accessors when fields are view-compatible

The generated API may expose lower-level `matrix_writer::<AnalysisCell>()` and
`matrix_reader::<AnalysisCell>()` handles for generic code.

## Performance Contract

P0 direct addressing must be O(1) with respect to total cell count for a single
cell read or write. It may depend on the number of fields in one slot.

Required performance smoke coverage:

- random-order write of a small and medium matrix
- direct read of committed cells
- repeated same-size overwrite while file size remains unchanged
- commit bitmap set/check throughput

These checks should live beside the existing ignored smoke suite and be runnable
without external services.
