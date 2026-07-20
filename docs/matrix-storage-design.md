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
| Creation nonce region     |
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
| Static aux regions        |
+---------------------------+
| Page index                |
+---------------------------+
| Region CRC table          |
+---------------------------+
| Append-log record region  |
+---------------------------+
```

The creation nonce region is a fixed 24 bytes (`VMNC` magic, version, reserved,
and a 16-byte random nonce) written once when a matrix file is created. It binds
a matrix sidecar to one logical creation rather than to one OS file object, so a
recreated file at the same path with the same layout is not accepted by the old
sidecar.

The matrix layout header stores:

- magic `VMAT`
- layout version
- dimension count
- matrix block count
- commit category count
- offsets and lengths for the dimension table, block table, commit maps,
  optional offset tables, slot regions, page index, region CRC table, and
  append-log start.

All integers in `VMAT` metadata are little-endian in layout version 4. Matrix
slot payloads still use the block/format endian policy for canonical field
encoding. The append-log scanner must start from `append_log_start`, never from
the normal header length, when the static spec contains matrix blocks.

## VMAT Version 4 Header

The header is deliberately simple and dense-layout oriented:

```text
magic                 [u8; 4] = b"VMAT"
layout_version        u16 = 4
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
page_index_off        u64   // field 13, appended in v3
page_index_len        u64   // field 14, appended in v3
reserved              [u8; 16]
```

The two page-index fields are appended *after* `append_log_start` so every field
defined by version 2 keeps its index; the reserved tail absorbs the 16 bytes they
occupy. The region order is therefore

```text
... | slot region | static aux | page index | MCRC | append log
```

Tables use length-prefixed UTF-8 names and fixed-width integers. Offsets are
absolute file offsets from the beginning of the file. P0 does not require
alignment padding beyond what the tables explicitly encode. Readers must reject
table ranges that overlap incorrectly, point outside the file, or place
`append_log_start` before the end of the final matrix region.

P0 or `integrity: none` files set `region_crc_off = 0` and
`region_crc_len = 0`. Static auxiliary regions, when declared by the format
spec, are derived in declaration order immediately after the slot region. VMAT
v4 does not store an auxiliary table; readers reconstruct aux offsets from the
static `FormatSpec`, so changing aux names, byte lengths, or declaration order
is a schema change.

Layout version 2 replaced the version 1 integrity representation (see the next
section), layout version 3 added the page-index region, and layout version 4
replaced its encoding (see *Page Index* below). A version 1, 2, or 3 artifact is
refused at open with the typed
`Error::FormatVersionMismatch { expected: 4, actual: <1, 2, or 3> }`, matching
the container-version convention: such a file is stale and regenerable, never
migrated in place. The `MCRC` integrity table itself remains version 2.

### Page Index

The page index is the bounded, persisted answer to "which bitmap pages does this
file have data in **right now**". The region is `(page_count + 1) * 8` bytes:
slot `0` is an occupancy header, slots `1..=count` are entries, and each entry
encodes `page + 1`.

**Occupancy header, not a terminator.** Version 3 stored entries only and read
them until it hit a zero or out-of-range value. That made damage and end-of-array
indistinguishable: a single zeroed entry silently truncated enumeration and hid
every page after it, with no finding produced, and cells in those pages read as
`NotCommitted`. Version 4 derives the length from the header instead. The header
packs the count into its low 48 bits and a value derived from that count into its
high 16, so a torn or bit-flipped header is itself detected and reported. Because
`check(0) == 0`, an all-zero region still decodes as a legitimately empty index,
so creation writes no header and an empty index still costs one small read (the
scan grows geometrically from a 64-byte request). A zero or out-of-range entry
*inside* the counted prefix is now provable damage: it produces a `Fatal`
`MatrixCorruptionKind::CommitMap` finding and the scan continues past it. Silence
is not a possible outcome.

This header is a redundancy code, not authentication. It detects accidental
damage; it makes no claim against an actor who can rewrite the file, which is the
same trust boundary the page-digest array records.

**Live set, not history.** Version 3 never removed an entry, so both the array
and its in-memory tracking grew with every page ever published and reopen visited
all of them. Version 4 releases a page's entry when its final set bit clears, in
`O(1)`: the vacated slot is overwritten with the array's last entry and the
header count is decremented — two 8-byte writes, no scan. Resident and persisted
cost therefore track live pages. The resident side is charged to
`ReadLimitKey::MatrixBitmapBytes` alongside the payload pages, before the
allocation is taken, under a documented conservative residency model.

**Ordering.** An entry is written before the header that admits it, and both
before the bitmap byte they describe. A torn write can therefore leave an entry
naming a page that still reads as uninitialised zeros — a state open already
accepts, costing one extra page read — but never a written page with no entry. A
failed header write unwinds the in-memory mirror so the next attempt republishes
the entry. Removal runs strictly *after* the bitmap byte that emptied the page is
written, for the same reason: a superset is safe, a subset would hide data.

**Cap.** The header can represent at most `2^48 - 1` entries, so a matrix whose
page count would exceed that (about 1 EiB of bitmap) is refused at layout time
with `InvalidMatrixLayout`.

Open builds its visit set as the union of the index and, where the platform can
answer, the pages the filesystem allocation map reports as written, enumerated by
walking allocated *ranges* rather than pages. Neither term derives from the
logical page count, and an unavailable or over-cap allocation map falls back to
the index instead of treating every logical page as readable data. The union is
computed in one linear pass with a hash probe — `O(Q)` time and `Theta(Q)`
temporary memory for `Q` candidates, with no sort — and the resulting list is
deliberately unsorted, because page visits are independent and page loading is
idempotent. The page-digest
array is deliberately not mapped back into the visit set: one allocation granule
spans thousands of 8-byte digest slots, which would reinstate a width-proportional
count. The index covers torn commits instead.

Trade-off, recorded explicitly: when no allocation map is available, open visits
only indexed pages, so a stray byte written out of band into a page the matrix
never published is not detected at open. This is the same class as the documented
hole-skip trade-off — a CRC cannot authenticate metadata against anyone who can
write the file. With an allocation map present, which is the normal case,
detection is unchanged.

When `integrity: crc32` is enabled, the region CRC table is placed immediately
after the page index and before `append_log_start`.

Region CRC table (`MCRC` v2):

```text
magic          [u8; 4] = "MCRC"
version        u16 = 2
reserved       u16
metadata_crc32 u32  // dimension table + block table + commit category table
reserved       u32
commit_pages   [{ crc32: u32, state: u32 };
                sum over commit categories of
                ceil(bit_bytes(category_bits) / 4096)]
slot_crc32     [u32; sum(matrix_block.cell_count)]
slot_valid     [packed bits; sum(bit_bytes(matrix_block.cell_count))]
```

Commit integrity is per page, not per category. Each commit-category bitmap is
divided into 4 KiB pages and each page carries one 8-byte digest: a CRC32 of the
page bytes plus a state word. `state = 0` means uninitialized — the page was
never published and must still read as all zeros — and `state = 1` means
initialized, in which case the stored CRC is authoritative. Any other state word
is corruption. That distinction is what makes a page written with zeros
unambiguously different from a page that was never written, so a never-written
cell can no longer accidentally match a checksum computed over zeros.

The page representation exists to keep integrity maintenance independent of
matrix size. Mutating one commit bit rehashes only the 4 KiB page containing the
mutated byte (less for a map smaller than one page), so writing and committing
`M` cells hashes `Theta(M)` bitmap bytes in total instead of the `Theta(M^2)` a
whole-bitmap-per-bit digest costs. There is deliberately no composition checksum
over the page-digest array: maintaining one would reintroduce a size-dependent
cost on every mutation. Whole-map verification is "verify every page against its
digest", which is exactly what open performs, and the digest array is exactly as
exposed as the single unprotected per-category CRC it replaces.

Commit page-digest arrays follow the declared commit-category order. Slot CRC
entries are dense in matrix-block declaration order, then ordinal order within
that block. Slot valid bitmaps use the same block and ordinal order; a per-cell
CRC is trusted only when its persistent validity bit is set, which is what
distinguishes a committed all-zero payload from an untouched all-zero slot
during commit-map rebuild. The table has no per-entry offset fields because
offsets are derived from the VMAT tables and runtime dimensions.

### Creation And Residency Cost

Creation writes the descriptor tables and the 16-byte `MCRC` header and nothing
else. The per-cell CRC array, the per-cell validity bitmaps, and the commit
bitmaps are established as a sparse zero extent by the same `set_len` that
establishes the slot region, so explicit create-time metadata I/O does not scale
with cell count at all. The uninitialized encoding above is what makes that
sound: a zero extent is a region of `state = 0` pages, which assert "never
published" rather than "checksum of zeros".

Commit, CRC-valid, and current-write bitmaps are held sparsely after open. A
page is materialized in memory only when it carries a set bit; an absent page is
provably all zero and answers every query without I/O or allocation. Resident
bitmap bytes are therefore proportional to the pages that carry state, not to
cell count — a freshly created matrix holds none at any size — and the running
set-bit total each sparse map maintains makes committed-cell counting (and so
resume signals and recovery reports) `O(1)` instead of a full scan.

Residency is also released, not just acquired. Each page carries its own set-bit
count, so "is this page now all zero" is an `O(1)` question on the page that was
just mutated; a page that loses its final set bit is evicted immediately and its
bytes refunded to the resident bitmap budget. Nothing scans the page or the map
to decide that, so it costs nothing on the mutation path, and a long-running
sparse set/clear workload no longer holds residency proportional to every page it
has ever touched. Loading a page is idempotent, so a duplicate page-index entry
cannot double-count.

Open cost is bounded by the *candidate* pages, not by the bitmap width: the visit
set comes from the page index and the allocation map (see *Page Index* above),
and the index scan itself starts at 64 bytes and grows geometrically. Only the
index term follows the pages that carry data; the allocation term follows how
densely the file is allocated, so "bounded by the pages that carry data" is the
sparse-allocation operating case rather than a worst-case bound.
Not everything in the matrix is width-independent: `rebuild_commit_map_from_crc`
is linear in cells by construction — it re-derives every cell's validity from its
stored CRC — and `clear_category`'s digest reset falls back to a per-page write
loop where hole punching is unavailable.

Open is bounded the same way. Authenticating an uninitialized page means
proving it still reads as zero, which would otherwise mean streaming every page
of every map. Instead open asks the filesystem which byte ranges of the file are
allocated (`FSCTL_QUERY_ALLOCATED_RANGES` on Windows, `SEEK_DATA`/`SEEK_HOLE`
elsewhere) and skips the ranges it reports as holes: a hole has never been
written since creation, so it reads as zero and cannot hold stray bytes, and
writing a stray byte into an untouched page necessarily allocates that page and
brings it back into the read set. Where the allocation map is available,
detection strength is therefore unchanged. The page digest of a skipped page is
still read when the digest slot itself is allocated, so a digest recorded for a
page whose bytes never reached disk stays detectable.

The allocation map is a *secondary* source. Open enumerates the union of the
persisted page index and the allocation map in `O(Q)` time for `Q` candidate
pages — the `L` pages currently holding state plus the `A` pages the map reports
as written — with `Theta(L + A)` temporary memory and up to `O(4096U)` page-byte
reads for `U` distinct candidates, independently of the logical matrix width and
of how many pages the matrix has published historically. `A` is not a live-state
figure: a densely allocated bitmap region contributes candidates in proportion to
that region, so open tracking live state is the sparse-allocation operating case
and not a worst-case guarantee.

Where the platform or filesystem cannot answer the query, or the file is
fragmented past the tracked extent ceiling, the allocation map is simply absent:
the page index alone drives enumeration, which still costs `O(live pages)`. Two
things are then lost. The ability to skip reading an indexed page the filesystem
would have proved zero, and the coverage of pages the matrix never indexed —
every persisted-index page is verified on every platform, but a never-indexed
page is additionally checked only where a usable allocation map exists, so a
stray byte written out of band into a page the matrix never published is not
detected at open. It is not accepted either: that page is never loaded. Since
layout version 3 there is no full-logical-scan fallback; any text describing one
is stale.

Clearing a whole commit category asks the filesystem to remove the byte ranges
of the map, its page index, and its page digests rather than writing zeros, so
where removal is available it restores the uninitialized encoding in `O(1)`
writes. Removal is attempted on Windows (`FSCTL_SET_ZERO_DATA`) and Linux
(`FALLOC_FL_PUNCH_HOLE`) only; on every other target, and whenever the call
fails (for example on a filesystem without sparse-file support), the ranges are
streamed as zero bytes and the clear costs `Theta(cells / 8)`. Every range
request records its outcome, so a caller can prove which path it got by taking
`MatrixRecoveryReport::matrix_total_zero_range_streamed_bytes()` before and
after the clear, on the thread performing it: a nonzero delta means some range
had to be streamed.

One cost is deliberately unchanged: `rebuild_matrix_commit_from_crc` remains
`O(cells)` with two reads per cell, which is inherent to rebuilding from
per-cell checksums.

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
increasing `layout_version`; v2 keeps CRC offsets derived from table order.

## P0/P1/P2 Split

P0 includes dense matrix declarations, runtime dimensions, `VMAT` layout
persistence, fixed-stride slots, direct addressing, commit maps, same-size
overwrite, `NotCommitted`, and mixed matrix plus append-log scan behavior.
In VMAT v4, a cell commit category belongs to exactly one matrix block.

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
outside VMAT v4.
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
