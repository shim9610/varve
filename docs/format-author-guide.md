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

The `limits` block is optional operational policy, not wire schema. Prefer
choosing limits where a reader or writer is opened. Standard policy leaves the
append log itself uncapped and bounds one-shot payload/materialization work.

**`file_len` is gone from the examples in this guide, deliberately.** It is
still accepted so that existing definitions keep parsing, but nothing enforces
it: a ceiling on how large a *file* may be bounded nothing a reader allocates.
What a reader allocates is bounded by `records`, `index_bytes`, `scan_bytes`,
`record_payload` and `logical_payload`, each of which has a check that consults
it. A format that relied on `file_len` to refuse a large file must state one of
those instead. See [Known Limitations
§2.1](known-limitations.md#21-varvefile-scans-the-whole-file-at-open-and-holds-a-record-index).
Generated `*_with_resource_limits` methods may raise or lower optional format
defaults; compatibility `*_with_limits` methods only tighten. Do not use
trusted-unbounded methods for files supplied by users, networks, or other
processes.

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
        }

        commit: cell_bitmap {
            keyspace = [scan, ch];
            categories = [analysis];
        };

        blocks {
            matrix AnalysisCell(id = 10, dims = [scan, ch], category = analysis) {
                rfu: [f32; WELLS_PER_CELL],
            }
        }
    }
}
```

**A cell's own shape is compile-time; the grid's shape is runtime.** `dims`
declares the matrix's runtime dimensions, chosen when the file is created, and
those names address cells — they are what `dims = [scan, ch]` selects from. A
cell's *payload* is an ordinary canonical-codec field, so an array in one needs
a length Rust can see:

```rust
const WELLS_PER_CELL: usize = 96;
```

An earlier version of this example wrote `rfu: [f32; n_wells_max]` with
`n_wells_max` declared under `dims`. That does not compile — a runtime dimension
is not a constant — and it is the exact confusion this paragraph exists to
prevent. If the per-cell length is genuinely not known until create time, it is
not a fixed array in a cell; make it a third matrix dimension, or an append-log
variable block.

The first implementation focuses on dense `(scan, ch)` addressing, bounded slot
payloads, `NotCommitted` reads, and same-size in-place overwrites. Append-log
blocks may coexist after the preallocated matrix regions.

Same-size overwrite is fail-safe within one writer: Varve clears the old commit
and CRC-valid evidence before writing slot bytes, and a later explicit commit is
the final visibility step. Partial matrix I/O poisons the writer and leaves the
cell uncommitted. Matrix readers snapshot the layout, not commit maps and not
immutable copies of every slot: since 0.5.0 each commit-map page is as of the
first read that faulted it in. Applications must not overlap a reader with
in-place writes to slots it may read; use external read leases or a higher-level
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
optional resource defaults, optional schema hash, optional extension, optional integrity, optional commit
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
literal value to pin. The hash covers wire layout, not just field membership:
fields are hashed in declaration order with their encoding ordinal, and each
block's endian override, keyedness, and generated codec fingerprint are folded
in (`FormatSpec::SCHEMA_HASH_ALGORITHM_VERSION` names the algorithm revision,
currently 3). Each field also contributes its codec's `SCHEMA_ID`, so a
hand-written codec must declare one (the derive refuses a field whose codec does
not) and must revise it whenever its bytes change; see
`docs/custom-codec-guide.md`. Reordering field declarations changes the hash
because it changes the canonical bytes. Omitting `schema_hash` stores 0 and disables the open-time
comparison entirely — only do that when schema locking is deliberately
unwanted.

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

## An Editable Region In The Header

Append is the wrong shape for a fact that changes — a watermark, a processing
state, a pointer into an external system. `header_slots` reserves a **fixed**
run of bytes in the file header that a caller rewrites in place, for the life of
the file, without republishing it.

```rust
varve_format! {
    pub struct AppFormat {
        magic: b"MYFORMAT";
        version: 1;
        endian: little;
        schema_hash: computed;
        header_slots {
            capacity: 256;        // bytes; fixed for the life of the file
            integrity: rolling;   // none | rolling | sealed
            blocks: [Watermark];  // only these may be written there
        }
        blocks: [Sample, Watermark];
    }
}
```

All three keys have to be there except `integrity`, which defaults to `none`.
Neither of the other two has a defensible default: a region with no declared
blocks can hold nothing, and a capacity picked by the macro would be a size you
discover from a failure rather than choose.

`capacity` is the whole region's entry area in bytes. Each block costs its
canonical encoded length plus twelve bytes of entry framing (`block_id u32 |
block_version u16 | reserved u16 | len u32`), and a rewrite of a block already
there reclaims its bytes first — so the budget is against the set of blocks held
at once, not against how many times you write them.

Then:

```rust
let mut file = AppFormat::create("f.varve")?;
file.write_header_block(&Watermark { label: "ingested".into(), at: 42 })?;

let mark = file.read_header_block::<Watermark>()?;   // takes &self, no I/O
let left = file.header_slots_free_bytes()?;
```

Sizing it: `header_slots_used()` after one write of each block you intend to
hold tells you the real cost, and `header_slots_free_bytes()` is what remains.

The ceiling on `capacity` is 64 KiB unless the format raises it. The region
shares the file-header extension budget, and that budget is the `header_extension`
key in `limits { ... }`:

```rust
limits {
    // ... the rest of the block ...
    header_extension: 524_288;
}
header_slots {
    capacity: 262_144;
    integrity: rolling;
    blocks: [Watermark];
}
```

A capacity above the budget is refused at create, not clamped. The budget also
bounds what a reader will accept from a header it has not parsed yet, so declare
it in **every** copy of the format declaration: a reader left at the 64 KiB
default refuses, at open, a file a declaring writer produced.
A write that would not fit is refused with `Error::LimitExceeded` before any
byte is written, so a region that is too small fails loudly at the write rather
than quietly at the read.

### Choosing `integrity`

| you want | declare | it costs |
| --- | --- | --- |
| editable forever, never verified | `integrity: none` | nothing |
| every read verified | `integrity: rolling` | one pass over the region per write and per read |
| the value frozen at a point you pick | `integrity: sealed` | the same, plus one `seal_header_slots()` call |

`sealed` is the freeze point: before the call the region is editable and the
checksum is advisory, after it reads verify and writes fail with
`Error::InvalidFormatSpec`. It is one-way. `seal_header_slots()` on `none` or
`rolling` is refused rather than ignored, because there is nothing there for it
to mean.

The checksum is FNV-1a 32 rather than a crc32, so it works without the
`integrity` feature — a `rolling` region verifies in a default build.

### What it costs when you do not declare it

Nothing, measurably: the header bytes and `computed_schema_hash()` are the ones
your format produced before the option existed. Two refusals to know about — the
capacity is part of the schema hash, so it cannot be changed for an existing
file, and a format declaring matrix blocks is refused outright because the
matrix creation nonce and layout header sit at fixed offsets after the file
header that a reserved region would move.

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

### Making Open Stop Scanning

Open builds a resident record index, and by default it builds it by reading
every record in the file. `segment_on_flush` replaces that walk: each commit
point appends an internal **segment** record covering the records it added,
chained to the previous one through `prev_same_block_offset`, and open follows
that chain backwards from the end of the file without reading a data record.

It is not a DSL clause, on purpose. A block is the unit your declaration names;
a segment is varve's internal lookup unit, and choosing its granularity is not
something a format author should have to do. Turn it on where the rest of the
runtime policy lives:

```rust
const SPEC: FormatSpec = MyFormat::SPEC
    .with_index_policy(MyFormat::SPEC.index_policy.with_segment_on_flush(true));
```

It requires `block_offset_chain` (which it turns on for you — the chain is that
footer field) and a `crc32` integrity policy, it gives up in-place fixed
replacement, and it replaces `checkpoint_on_flush` — declaring both is refused,
because with the chain on a checkpoint is a periodic full copy of the index that
nothing reads. Each segment record carries 73 bytes per record it covers, so the
bytes it costs are set by how often you flush; it pays for itself at a commit
point every few hundred records and not at a commit point per record.

Measured with a commit point every 256 records and 4 KiB payloads: an 852 MB,
200,000-record file opens in 370 ms instead of 5,867 ms, framing 782 records
instead of 200,782.

**Finish writing with `flush()`.** Open finds the chain by looking at the record
at the end of the file, so a writer that stops on a data record leaves nothing to
start from and the open scans the whole file — correct, but with none of the
benefit and no warning. The shape that bites is a loop with no final flush:

```rust
for i in 0..n {
    writer.push(&block)?;
    if i % 256 == 255 { writer.flush()?; }
}
writer.flush()?;   // <-- without this, n = 50_000 opens in 1,272 ms, not 60 ms
```

A file it cannot account for — one written before you enabled it, one whose
writer appended past its last commit point, a truncated tail — opens by the old
scan and produces the identical index. See `docs/known-limitations.md` §2.1.

### Making Open Stop Building An Index

`segment_on_flush` makes the open-time walk cheap. `open_digest_on_flush` asks a
different question: what if the open does not build an index at all?

Three facts are the only reason an open reads a file it is not indexing — where
the committed file ends, what sequence the next append takes, and where each
block's newest record sits. None of them is in any single record. A **digest** is
those three, written at each commit point:

```rust
const SPEC: FormatSpec = MyFormat::SPEC
    .with_index_policy(MyFormat::SPEC.index_policy.with_open_digest_on_flush(true));
```

```rust
let file = VarveFile::open_readonly_lazy(SPEC, path)?;   // frames one record
let mut buffer = Vec::new();
let mut map = file.record_map(&mut buffer)?;             // reads nothing yet
let hit = map.find(|entry| entry.block_id == Note::ID)?; // walks until it finds
```

**Which of the two you want is a disk-size question**, and it is the only
question. Measured on a 31 MB, 50,000-record file with a commit point every 500
records:

| | read syscalls at open | on-disk cost | builds an index? |
| --- | --- | --- | --- |
| neither (scan) | 100,316 | — | yes |
| `segment_on_flush` | 516 | **+3.67 MB** | yes |
| `open_digest_on_flush` | **20** | **+12.8 KB** | no |

A segment carries an index entry per record it covers, so what it costs on disk
tracks your record count. A digest carries 12 bytes per *distinct block id*, so
it does not: the same format at 200 and at 2,000 records writes exactly the same
128-byte digest. That is what makes the digest the one you can leave on for a
file that will hold a billion records.

The two are not alternatives — declare both if some of your readers want the
index cheaply and others want no index at all. A digest also composes with
`checkpoint_on_flush`, which a segment does not.

The trade is that a digest open has **no record directory**, so `blocks`,
`scan` and `keyed_blocks` return `Error::NoResidentDirectory` rather than
answering. They are not withdrawn: `with_directory(&map)` answers all of them
against whatever prefix the map has walked, and `block_chain`,
`block_tail_offset`, `read_block_at` and every entry-taking `_into` read need no
directory at all — which is why the digest carries the block tails.

It requires `block_offset_chain` (which it turns on for you). A file it cannot
account for falls back to the scan, and unlike the segment chain that fallback
is reportable: `open_readonly_lazy_with_report` returns
`LazyOpenSource::{Digest, FullScan}`. **Finish writing with `flush()`** here
too, and for the same reason.

### When A Later Append Must Not Be Able To Hide The Answer

The digest has one property that is a real limit rather than a cost: it is a
*record*, and it is usable only while it is the file's last one. Anything
appended after it — a partial write, records from a run that then crashed —
hides it, and the open falls back to reading everything. The moment you most
need a cheap resume is the moment that is most likely.

`index: header_tails` writes the same table into a fixed region of the **file
header**, where nothing appended can move it or bury it:

```rust
varve_format! {
    pub struct MyFormat {
        // ...
        integrity: crc32;
        index: header_tails;
        commit: transaction_marker(on_flush);
        blocks: [Note, Reading];
    }
}
```

```rust
let (file, source) = VarveFile::open_readonly_lazy_with_report(SPEC, path)?;
assert_eq!(source, LazyOpenSource::HeaderTails);
```

**What it costs.** A fixed region sized by your declaration and nothing else:
`8 + 2 x (28 + 12 x (blocks + 10) + 4)` bytes, so **360 bytes for a two-block
format**, the same at two hundred records and at two billion. The `+ 10` is one
slot per block id varve reserves for its own records. The update rides
the durability request that already ends a commit, so there is **no extra
`fsync`**. Measured on the two-block fixture: a scanning open frames every
record; this one frames **4** — the commit marker plus one per distinct block id
— and frames the same 4 on a file ten times larger.

**What it requires.** `block_offset_chain`, which it turns on for you;
`integrity: crc32` (or `crc32_with_header`); and a `transaction_marker` commit
policy. The checksum is not optional here for a reason the other options do not
have: the region is overwritten *in place* at a constant length, so a torn write
leaves a region that frames perfectly and names records that are not there. Each
of the two slots carries its own checksum, and that is the only thing that
separates the two. The commit policy is required because a slot names a commit
marker — with no marker there is nothing to name, and the region would stay cold
for the life of the file while still costing its bytes.

**What the handle can do.** It keeps no resident directory, which is the point —
a resume must not cost memory proportional to the file. So `blocks::<T>()` and
`mmap_payloads` refuse with `NoResidentDirectory`, and `record_map` is how you
walk records. Keyed lookups work: `key_tail_offsets` rebuilds from the chains at
a cost bounded by the keyed records rather than by the file, which is what makes
a keyed resume on a 600,000-record file affordable. Bounded by the keyed records
*and by every tombstone in the file*: deletions are their own block with their
own chain, one chain shared by all keyed blocks, and the rebuild walks it whole
because it cannot know which entries belong to the block being asked about
without decoding them.

**What it refuses.** A format declaring matrix blocks (the matrix layout sits at
a fixed offset after the header, which the region moves), and
`open_digest_on_flush` — the two are answers to the same question with different
safety properties, so declare one.

**What it gives up on an in-place replacement.** `replace_fixed` and
`unsafe replace_fixed_in_place_exclusive` reset the region to cold, so the next
open reports `LazyOpenSource::FullScan` and the commit after it warms the region
again. Nothing moves in such a replacement, so the offsets in the table would
still be true — but the table does not only answer offsets. The high-water mark
an open resumes from is *derived* from the commit marker the slot names, and
that derivation assumes a record's sequence rises with its offset. An in-place
replacement is the one operation that breaks it: it takes a fresh sequence and
writes it before the marker. So the region goes out of use whole rather than
answering two facts correctly and one wrongly. A digest cannot take this route —
it is a record, with no cold state — which is why it refuses the operation
outright instead.

**Turning it on changes the schema hash**, because the region lives in the
header and so moves every record offset in the file. An existing file does not
open with it and cannot be given the region in place; write a new one.

**How a reader knows the table is still true.** A table at a fixed offset is
always present and always "last", so unlike the digest its position proves
nothing. It records the offset of the commit marker it was written for, and an
open frames that record, requires it to be a commit marker, then walks forward —
at most a segment and a digest may follow one — and requires the walk to land
exactly on the end of the file. Anything else falls back to the scan and answers
identically, only slower: a file appended to since that commit, a table left
over from an earlier one, a torn slot. You do not have to do anything about any
of those.

## Dead Records And Crash Recovery

Two options, and they are a pair. Declare the first to be able to say a record
is dead; declare both to stop a crash from deleting a writer's last work.

### `liveness: footer_flags`

Without it, a varve record can stop being the answer in exactly two ways, and
neither reaches a block with no key. A tombstone names a block id and a **key**,
never an offset — every delete entry point requires `T: VarveKeyedBlock`. And a
`replace_*` publishes a whole new file in which the superseded record simply is
not there, which is a rewrite, not a mark.

`liveness: footer_flags` turns the record footer's trailing `reserved` word into
a mutable flag word and **takes it out of the record checksum**:

```rust
varve_format! {
    pub struct Log {
        magic: b"MYLOG001";
        version: 1;
        endian: little;
        integrity: crc32;
        index: block_offset_chain;
        commit: transaction_marker(on_flush);
        liveness: footer_flags;
        blocks: [Reading];
    }
}
```

```rust
writer.mark_record_dead(offset)?;        // four bytes, in place
file.record_is_dead(offset)?;            // or entry.is_dead()
```

**It costs no bytes.** The word is already in every footer such a format writes;
only its meaning and its checksum coverage change. What it does cost is a
different schema hash — excluding four bytes changes the checksum of every
record, so an existing file does not open under it.

**Why the footer and not the header.** The word has to be writable *after* the
record was written, and putting it in the footer outside the checksum means the
record's checksum stays valid (nothing re-reads the payload to recompute a CRC),
`crc32_with_header` is unaffected (it covers the header), and a reader that
already framed the record does not disagree with the disk — the seven fields
that establish a record's identity are all header fields.

**A dead record is advisory and is still there.** It still frames, `blocks`,
`scan` and `record_map` still return it, and every chain that pointed at it
still does. Hiding it would silently change what a chain walk means. What
consumes the mark is a defragmenting rewrite — the operation that can actually
drop a record and rebuild the chains that named it.

**Be clear about the trade:** those four bytes are the only part of a record no
checksum covers. A flipped bit there is detected by nothing. A set flag says
"the writer marked this", never "these bytes are sound". That is the price of a
field that can change without rewriting the record.

Requires a record footer, so declare a commit policy or an offset chain.
Internal records — commit markers, segments, digests — cannot be marked:
dropping those would drop the records that say where the commit boundary is.

### `recovery: mark_tail`

varve's default answer to a crash is `recovery: strict`: at the next read-write
open, every record past the last commit marker is deleted. (`recovery:
truncate_tail` deletes the same tail; they differ in what `open_recover` does
with a half-written record — it truncates one, where `strict` returns
`Error::CorruptTail`.) It is correct and cheap, and it is also the only
operation in varve that destroys data you might have wanted — those records
framed, they are structurally complete, and the writer simply never got to say
so.

```rust
        liveness: footer_flags;
        recovery: mark_tail;
```

Now that tail is kept, every record in it is marked dead, and a commit marker is
appended. Read the verdict with `writer.opened_after_crash()`.

**How a crash is told from a clean shutdown.** A writer sets a bit in the file
header when it takes the object and clears it when it releases. The writer lock
answers the other half — "is anybody holding it now" — and it is an *object*
lock, so hard links and other path aliases cannot dodge it:

| lock | header bit | verdict |
| --- | --- | --- |
| held by someone else | set | a writer is working normally |
| free | clear | the last writer released cleanly |
| free | **set** | the last writer died without releasing |

This is the only durable record varve keeps of a writer's lifecycle. The signal
it replaces — "does the file end exactly at a commit point" — can only describe
a suffix, never an interior record, and the next open truncates that suffix away
before anything can ask.

**Only a crashed writer's tail is kept.** An uncommitted tail left by an orderly
writer is that writer's business, and truncating it is the contract it was
written under.

**Why a commit marker is appended.** Without it the file ends past its last
marker, and the *next* marker written would make the whole tail count as
committed — records the original writer never committed, becoming visible
retroactively. Committing them here, flagged dead, is the honest statement: they
arrived, they were never committed by their writer, they are kept, and a
defragmenting rewrite may drop them.

**What is still cut.** A half-written record is the expected find in a crashed
tail. Everything that framed before it is kept; it and anything after it never
became a record and is truncated.

**What it costs.** The file keeps bytes `truncate_tail` would have reclaimed,
until something defragments it. Refused without `liveness: footer_flags` (no
word to write the verdict in) and without a transaction marker (nothing to name
a boundary with).

### Reclaiming the space: `defragment()`

Marking a record dead costs four bytes and frees nothing. The operation that
frees something is a defragmenting rewrite:

```rust
let report = writer.defragment()?;
// report.records_dropped, report.bytes_before, report.bytes_after
```

It rewrites the file without the records marked dead and publishes the result
the way `replace_*` does — a temp file in the same directory, synced, renamed
over the pathname. **A reader open across it keeps its own generation whole**:
the old object is unlinked but alive for as long as a handle holds it, so reads
in flight neither fail nor change their answers. That handle learns about the
new generation the same way it learns about any republish — `is_current()`, then
`reopen_readonly()`.

The cost of that route is **peak disk**: both generations exist at once. An
in-place compaction would avoid it and would break every reader open at the
time, which is the trade this chose against.

Every internal record is kept. The derived ones — index checkpoints, segments,
open digests — are rebuilt from the records actually written rather than copied,
because their payloads *are* record offsets and a rewrite moves them.

**Both offset chains are rebuilt, not shifted.** A dropped record's successors
are relinked to the nearest surviving predecessor in the same chain, and a link
whose whole chain was dropped is cleared rather than pointed at whatever now
occupies that offset.

Refused without `liveness: footer_flags` — nothing would ever be dead, so it
would be a whole-file copy that achieves nothing — and for custom layouts,
matrix storage, and a handle that keeps no resident directory. Memory is two
offset tables sized by the record count, charged to `max_index_bytes`.

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
                    bytes write = b"END!";
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
