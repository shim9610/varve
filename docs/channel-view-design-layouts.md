# Datablock payload layout options and their I/O physics

Scope: the physical layout of a datablock payload for the owner's stream

```
datablock[CH1,CH2,CH3,CH4,CH1,...,CH4]  metablock[meta]  datablock[...]  datablock[...]  metablock[meta] ...
```

where **one datablock is one record** whose payload holds many samples of CH1..CH4, and
metablock records are interspersed. Stream is unbounded.

No repository file was modified.

---

## 0. Verified facts this design rests on

Re-verified by reading source, not taken on trust:

- `RECORD_HEADER_LEN = 32`, `RECORD_FOOTER_LEN = 32` — `crates/varve-core/src/file.rs:41-42`.
- `payload_offset == record_offset + RECORD_HEADER_LEN` is an enforced invariant —
  `validate_mmap_index_entry`, `file.rs:1472-1486` (errors `MmapPayloadOutOfBounds` otherwise).
- `SnapshotFile::read_exact_at(&self, offset, buffer)` — `snapshot.rs:74-96`. Confirmed
  `&self`, bounds-checked against `SnapshotBounds`, loops over `read_at` (pread / `seek_read`),
  writes into a **caller-supplied buffer**: zero allocation, one syscall in the common case.
  It is `pub(crate)`; `SnapshotFile` is `pub(crate)` at `snapshot.rs:15`.
- `MmapPayloads::payload_window(&self, entry) -> &[u8]` — `file.rs:1219-1231`, binary search on
  `record_offset` + full equality compare, then a zero-copy slice of the whole payload extent.
- Matrix addressing is **scan-major / channel-minor**: `ordinal_for_block` computes
  `key.scan * dim1 + key.ch` (`matrix.rs:2146-2164`) and `slot_offset` computes
  `slot_region_offset + ordinal * slot_stride` (`matrix.rs:2166-2172`). The dimension binding is
  positional (`block.dimensions[0]` ← `key.scan`, `[1]` ← `key.ch`), so the field *names* carry no
  meaning.
- `ChunkedBytes` enforces **uniform chunk length**: `validate_chunked_bytes` requires
  `chunk_count == ceil(uncompressed_len / chunk_len)` and entry *i*'s `uncompressed_len == chunk_len`
  for every entry but the last — `chunks.rs:200-241`. Its per-entry length field cannot vary.
- `IndexPolicy { scan_on_open, checkpoint_on_flush, block_offset_chain, keyed_offset_chain }` —
  `format.rs:482-487`. `block_offset_chain` is written and never traversed (survey, confirmed).
- `PreparedStreamRecord { bytes: Vec<u8>, .. }` — `file.rs:8106-8113`: the batch append path
  **still heap-allocates per record**. This is a pre-existing violation of the owner's constraint
  and is independent of anything below.

### Working example used for every cost figure

| symbol | value |
|---|---|
| channels `C` | 4 |
| sample width `W` | 8 B (f64) |
| samples per channel per block `S` | 4096 |
| datablock payload | `C*S*W` = 131 072 B (128 KiB) |
| record framing | 32 B header (+32 B footer when `block_offset_chain` on) |
| acquisition rate | 1 MS/s/channel → 32 MB/s, one datablock every 4.096 ms |
| query | **"CH1 samples 1 000 000 .. 1 000 100"** = 100 samples = **800 B of useful data** |

Sample 1 000 000 of CH1 lives in block `⌊1e6/4096⌋ = 244`, at in-block sample index
`1e6 − 244·4096 = 576`. All 100 samples fall inside block 244.

Page granularity floor: the OS transfers 4 KiB pages; NVMe transfers 512 B–4 KiB device blocks.
**No layout can deliver 800 B for less than one page.** Every "bytes read" figure below is given
both at byte granularity and at 4 KiB page granularity, because for small queries the page floor
dominates and for full-channel scans the byte figure dominates.

---

## 1. The physics, stated plainly

In an interleaved payload the bytes of CH1 recur with period `C*W = 32 B`. A 4 KiB page holds
128 such periods, and **every one of them contains CH1 bytes**. Therefore:

> Any read that obtains all of CH1's bytes from an interleaved payload necessarily transfers
> all of CH2..CH4's bytes, because the smallest unit the storage stack transfers (512 B–4 KiB)
> is larger than the 32 B period and every such unit is mixed.

An index is a map from a logical question to a file offset. It changes *which* offset you read
and *how many* offsets you read. It cannot change the transfer granularity of the device. So:

- An index **can** eliminate the `O(k)` record-header walk needed to *find* block 244
  (that is a real and large win — see §7).
- An index **can** eliminate the *decode* of CH2..CH4 (branch-free stride gather instead of four
  full decodes: ~4× CPU on the decode step).
- An index **cannot** reduce bytes-read below `C ×` the useful bytes for a full-channel scan.

**Any design that claims channel-selective I/O on strided interleaved storage is false.**
Channel-selective *I/O* requires the writer to place a channel's samples contiguously. That is the
whole decision, and it belongs to the write path, not the read path.

Second-order consequence, equally important: interleaved storage is not just 4× the bytes, it is
also *sequential*, so at 4 channels the device delivers it at full streaming bandwidth. Planar
storage reads 25 % of the bytes but in 32 KiB extents separated by 96 KiB gaps. On NVMe (≥ 100 K
IOPS, 32 KiB random read at near-sequential bandwidth) planar wins by ~3.5×. On spinning disk with
7 ms seeks, a 32 KiB read every 128 KiB is *slower* than streaming the whole file. State the
storage assumption: **this design assumes NVMe/SSD.**

---

## 2. Layout A — interleaved in the payload, as the owner drew it

Payload byte `p` holds sample `i` of channel `c` at `p = (i*C + c)*W`.

**Query cost.** CH1 samples 576..676 of block 244 occupy the payload byte span
`576*32 .. 676*32` = `18432 .. 21632` → **3 200 B contiguous**, containing 800 useful bytes.
- byte granularity: 3 200 B read for 800 B useful → **4.0× amplification**
- page granularity: the span crosses `⌊18432/4096⌋=4` .. `⌊21631/4096⌋=5` → **2 pages = 8 192 B**
  → 10.2× amplification (the page floor, not the layout, dominates here)
- one `pread` if a payload-range read exists; today: one whole-payload read of **131 072 B**
  (`read_payload_snapshot`, `file.rs:494-508`, always reads `payload_len`) → **164× amplification**
- plus locating block 244 — see §7. Without a directory this is 244 header `pread`s; at sample 10⁹
  it is 244 000 `pread`s and the query is unusable.

**Full-channel scan (all of CH1 across a 1 TB file):** reads **1 TB**, delivers 256 GB.
Sequential, so wall-clock ≈ file size / sequential bandwidth. At 3 GB/s: ~5.6 min. Irreducible.

**Append cost.** Per sample: one store into a reused block buffer at
`base + (i*C + c)*W`; one address computation (`imul`/`lea`). **0 allocations, 0 syscalls,
1 open write stream** — perfect sequential store locality.
Per block: one record append. On the resident path (`VarveFile::push`) that is
`seek + write_all(header) + write_all(payload)` (+ footer) = 3–4 syscalls per **block**, plus the
`encode_to_vec_limited` allocation. On the chunk path, 0 syscalls per record but 2 allocations per
record (`PreparedStreamRecord.bytes` at `file.rs:8107` plus the encode `Vec`, then a third copy at
`stream.rs:1165`).

**Memory to serve the query:** with a payload-range read, one 3 200 B caller buffer. Today:
a 131 072 B `Vec` per block touched, plus either the 104 B/record resident index or an mmap.

**TB scale:** the storage layout is fine (append-only, no growth limit). The *access* path is not —
see §7 and §8.

**Wire format change:** none.

**User-implementable today?** The layout, yes (it is just a `Vec<u8>`/`ChunkedBytes` field). The
selective read, no: there is no `&self` payload byte-range API (survey item 5), so a user gets the
whole 128 KiB payload per block, or uses `MmapPayloads::payload_window` — which requires
`scan_on_open` (Θ(N) open, 104 B/record resident) and an immutable file, both false for a
live-appended TB stream.

**When A is still right:** when the acquisition hardware hands you an already-interleaved DMA
buffer and the dominant read pattern is "all channels of a time window" (scope-style replay,
cross-channel math, resampling). Then interleaved is *optimal* — a time window is one contiguous
extent and planar would need `C` reads. Do not remove A; make it a declared choice.

---

## 3. Layout B — planar within the datablock (per-channel extents + in-payload directory)

Payload = `[extent directory][CH1 samples][CH2 samples][CH3 samples][CH4 samples]`.
Sample `i` of channel `c` sits at `p = dir_len + c*S*W + i*W` when geometry is uniform.

### Directory design

Two variants; the choice is the owner's (see openQuestions).

**B-uniform (recommended):** the block declares `C`, `S`, `W` in a fixed 16 B payload prologue —
`magic(4) | version(2) | channel_count(2) | sample_width(2) | reserved(2) | samples_per_channel(4)`.
Extent offsets are then a pure multiplication; nothing per-channel is stored.
**Directory cost: 16 B per block = 0.012 % of a 128 KiB payload.**

**B-variable:** 16 B prologue + `C × 16 B` entries `(offset: u32, len: u32, crc32: u32, sample_count: u32)`.
**Directory cost: 16 + 4·16 = 80 B per block = 0.061 %.** Needed if channels have different rates or
widths, or if per-extent CRC is wanted (it is — see §9).

This is exactly the `ChunkedBytes` wire shape (`chunks.rs:46-64`: 32 B header, `chunk_count × 16 B`
entries, then concatenated bodies) with one change: `validate_chunked_bytes` (`chunks.rs:226-238`)
currently *forbids* per-entry lengths from varying. Reusing the shape and relaxing that constraint
is far cheaper than inventing a new container. But `ChunkedBytes` is a *field* codec whose
`decode_varve` (`chunks.rs:186`) first materialises the whole blob — so it cannot be the read path,
only the byte-layout precedent.

### Query cost — "CH1 samples 1 000 000..1 000 100"

1. locate block 244 (§7): 1 `pread` of the block directory page.
2. read the 16 B prologue: **1 `pread` of 16 B** (or fold it into step 1's cached geometry and pay 0).
3. `pread` at `payload_offset + 16 + 0*4096*8 + 576*8`, len **800 B**.

- byte granularity: **800 B useful of 800 B read → 1.00× amplification.**
- page granularity: the span `4624..5424` inside the payload crosses at most 2 pages, and 1 if the
  extent is page-aligned → **4 096–8 192 B**. Same page floor as A; A is not worse *for this
  particular tiny query*. The difference appears the moment the query is larger than a page.
- 1 000 samples instead of 100: A reads 32 000 B, B reads 8 000 B — **4.0×**.
- decode: a plain `&[f64]` slice/`copy_from_slice`, no stride gather. ~4× less CPU than A.

**Full-channel scan (all of CH1 across a 1 TB file):** reads **256 GB** — one 32 KiB `pread` per
datablock, 8 M `pread`s for a 1 TB file at 128 KiB blocks. At NVMe 100 K IOPS × 32 KiB that is
~3.2 GB/s, i.e. ~80 s versus A's ~5.6 min: **~4× wall-clock win**, and 4× less page cache pressure,
which at TB scale is the figure that actually matters. Issue `pread`s for the next `k` blocks
concurrently (or `madvise(WILLNEED)`) to keep the queue depth up; a serial `pread` loop at QD1 gets
~1/8 of that.

### Append cost — the key finding: planar is essentially free for the writer

Per sample: one store at `base + dir_len + c*S*W + i*W`. **Exactly the same instruction count as A**
— one `lea`/`imul` and one store. **0 allocations, 0 syscalls.** The only difference is that the
writer keeps `C` open write streams instead of 1.

Per block: one record append, identical to A, + 16 B (B-uniform) or 80 B (B-variable) of directory.

> **The owner's stream shape already requires the whole block to be buffered** — a datablock is one
> record, and a record is written whole. So B adds **zero** buffering memory and **zero** latency
> over A. The "must buffer a whole block" cost the brief anticipated is already paid by A.
> Buffer: `C*S*W` = 128 KiB, double-buffered for overlap = 256 KiB. Latency: 4.096 ms (block
> duration), identical for A and B.

Two real writer costs, both small and both bounded:

1. **Write-stream count.** `C` concurrent sequential store streams. Each needs one open cache line
   plus a page-table entry. At `C = 4` this is invisible. Crossover: L1 is 32 KiB / 8-way, so
   ~8 streams are free; L2 512 KiB supports ~64–128. **Above ~64 channels**, switch to a tiled
   transpose: accumulate `T = 64` samples interleaved in a 64·C·W scratch buffer (16 KiB at C=32),
   then scatter tile-wise. That restores 1 read stream + `C` short bursts and costs one extra
   load/store pair per sample (~1 ns/sample), still 0 alloc / 0 syscall.
2. **Variable geometry.** If sample counts per channel differ *within* a block and are not known
   until the block closes, extent offsets are not known when the first sample arrives. Then the
   writer needs `C` reusable staging buffers plus one concatenating `memcpy` of the whole payload
   into the record buffer at block close: **+8 B/sample of copy, +`C*S_max*W` bytes of reusable
   staging memory, still 0 allocations and 0 syscalls.** At 32 MB/s this memcpy is ~1 % of one core.

**Memory to serve the query:** one caller buffer of the requested size (800 B). O(1). No resident
index, no eager load — *given* a `&self` payload-range read (§9).

**TB scale:** yes. Nothing about B grows with file size.

**Wire format change:** **none at the record framing level.** B is a payload *convention*: the
record header, footer, checksum and index entry are untouched. The extent directory lives inside
the payload bytes, exactly where a `Vec<u8>` field's bytes live today. This is B's decisive
advantage over D — it is additive, it does not bump the format version, and old readers can still
read the payload as opaque bytes.
(If the geometry is instead declared in the block *descriptor* / DSL so the reader can compute
extents without reading the prologue, that **is** a schema change and moves the schema hash. The
in-payload prologue avoids it. Recommend the prologue.)

**User-implementable today?** Write side: **yes, fully**, today, with no library change — the user
writes the prologue and the extents into a `Vec<u8>` field. Read side: **no** — the missing piece is
one function (§9). Until then the user must read the whole 128 KiB payload and slice it in memory,
which gives the decode win and the CPU win but none of the I/O win.

---

## 4. Layout C — one record per (channel, chunk), the TDMS-model approach

`TdmsChannelChunk(id = 103, key = [group, channel, chunk_index]) { values_f64: Vec<f64> }`
(`crates/varve/tests/tdms_model.rs:46-81`). One datablock becomes `C` records.

**Framing overhead:** 64 B/record (header+footer) × 4 = 256 B per 128 KiB of data instead of 64 B.
Delta **192 B per block = 0.15 %.** In *bytes* this layout is cheap. Its costs are elsewhere.

**Append cost.** Per sample: same as B (one store into one of `C` staging buffers).
Per block: **4× the record count.** That multiplies, per block:
- 4 record appends instead of 1 → on the resident path 12–16 syscalls instead of 3–4;
  on the chunk path 0 syscalls but 4× the (existing) 2-allocations-per-record.
- 4× resident index entries: 104 B × 4 = 416 B/block (`file.rs:385-401`, `index_bytes_for_count`
  at `file.rs:8966-8979`).
- 4 × `set_tail` (`stream.rs:1327`, O(log B) each) and 4 × the `stage_state_records` filter.
- **the disqualifier:** if the key is used, the sidecar `LATEST_TABLE` gets **one row per
  (group, channel, chunk_index)** (`historical_distinct_keys`, `indexed.rs:257`). That is one redb
  row per record, forever. At 8 M blocks/TB × 4 channels = 32 M rows/TB, ≈ 40+ B each ≥ 1.3 GB of
  sidecar per TB, growing without bound. The sidecar is designed as an O(B + K) recovery index, not
  an O(N) record index.

**Query cost.** Best available *random access* today, and worth naming honestly:
`VarveIndexedReader::get::<TdmsChannelChunk>(&(group, "CH1", 244))` — `indexed.rs:210-243`, `&self`,
open does not scan, memory bounded. One redb lookup ≈ 3–4 page reads (B-tree depth at 10⁷–10⁸ keys)
= ~16 KB, then the record payload.
But the payload read is **whole-record**: `read_logical_payload_snapshot` reads all `payload_len`
bytes. A chunk = one channel's 4096 samples = **32 768 B for 800 useful → 41× amplification**, and
a `Vec<u8>` allocation plus a `Vec<f64>` decode per query.
Full-channel scan: **25 % of bytes** (same as B) but 4× the record headers to walk and no way to
enumerate `(group, "CH1", *)` — `DiskIndexSnapshot` exposes only `lookup_pointer`
(`disk_index.rs:2164-2207`), **no range or prefix API**, and the disk key is encoded
**little-endian** (`encode_disk_key`, `disk_index.rs:1161-1170`) while redb orders lexicographically,
so a channel-prefix range scan is not merely unimplemented, it is *unsortable* under the current
encoding. So enumerating CH1 means knowing every `chunk_index` a priori, or a full scan.
Via the non-indexed path it is worse: `keyed_blocks::<T>()` (`file.rs:3888-3963`) **decodes every
chunk of every channel in the file** to build its map.

**Memory:** bounded on the indexed path (redb read transaction), catastrophic on the resident path.

**Wire format change:** none. **User-implementable today?** Yes — it is entirely a modelling
convention, and it is the *only* option that gives working random access on the API as it exists.
That is precisely why it is in the repo. It is the right stopgap and the wrong endgame.

---

## 5. Layout D — matrix cells at (scan, ch)

Derived from `ordinal_for_block` + `slot_offset` (`matrix.rs:2146-2172`), verified above:

```
ordinal = scan * n_ch + ch
offset  = slot_region_offset + ordinal * slot_stride
```

`ch` is the **fastest-varying** index. Therefore **a whole scan is contiguous** (`n_ch * slot_stride`
bytes, one read) and **a whole channel is strided** with period `n_ch * slot_stride`. At
`slot_stride = 8, n_ch = 4`: 8 useful bytes per 32 — identical amplification to layout A. At
`n_ch = 64`: 8 per 512, i.e. one useful `u64` per cache line, **64× amplification and 100 % of pages
touched**. The matrix is optimised for exactly the opposite query.

There is a free transpose: the dimension→`MatrixKey` binding is positional
(`block.dimensions[0]` ← `key.scan`), so declaring `dims = [ch, scan]` and passing
`MatrixKey { scan: channel_id, ch: scan_id }` makes the file channel-major and each channel
physically contiguous — at the cost of making cross-channel scans the strided direction. It also
breaks `per_channel_count` (`matrix.rs:5039-5051`), which picks the dimension literally *named*
`"ch"|"channel"|...`. So it is an undocumented reinterpretation, not a feature.

**Disqualifying, independent of layout:** matrix dimensions are fixed at create
(`create_layout`, `matrix.rs:2190`, is create-only; open re-validates persisted lengths against
recomputed dimensions, `matrix.rs:2438-2453`). **A matrix cannot represent an indefinitely growing
stream.** `max_matrix_cells` defaults to 16 M (`format.rs:114`) — at one sample per cell that is
16 s of one 1 MS/s channel — and `max_matrix_slot_region_len` is 8 GiB (`format.rs:118`), so even
making a cell a whole per-channel chunk caps the file at 8 GiB.

Also: single-cell reads only, all `&mut self` (`read_matrix_cell`, `file.rs:4313`, because
`matrix::read_cell` takes `&mut File` and does `seek`+`read_exact`, `matrix.rs:2727-2749`); no row,
column, range or iterator API; open eagerly loads commit bitmaps (`matrix.rs:2518`) so `cell_status`
never faults from disk and a reader can never observe a writer's later commits; and writing one scan
of `N` channels costs `N` allocations and 3N–5N syscalls even though those `N` cells are one
contiguous extent.

**Verdict: D is out of scope for this stream.** The matrix is a preallocated analysis grid with an
append log after it (`matrix.rs:2278-2284`), and it should stay that. Its `&mut self` and
single-cell limits are worth fixing on their own merits — replacing `seek`+`read_exact` with
`snapshot.read_exact_at` makes `read_matrix_cell` `&self` with no other change, the smallest
high-value edit in the subsystem — but fixing them does not make the matrix a channel store for an
unbounded stream, because the fixed extent is the blocker and that is a different subsystem.

---

## 6. Layout E — hybrids

**E1 — interleaved write, background reorganisation to planar.**
Cost: read every byte back and write every byte again. At 32 MB/s ingest that is a permanent
+32 MB/s read +32 MB/s write and 2× storage during the overlap. Worse, the crate **refuses
whole-file replace on footer specs**: `file.rs:3568-3573` returns
`InvalidFormatSpec("replace is not supported for record-footer formats")`, and `block_offset_chain`
forces a footer (`format.rs:1992-1994`). So reorganisation must produce a *new file*, not rewrite in
place. Verdict: viable only as an explicit offline "seal and transcode cold data" tool, never as a
background task on the hot path. Worth having; not the answer.

**E2 — interleaved payload + sparse sample→block directory.**
This does **not** fix amplification (§1) and must not be sold as if it did. What it fixes is the
*seek*: without it, finding the block holding sample 10⁹ costs `O(k)` record-header `pread`s
(`NativeStreamScanner` starts at `header_len`, `file.rs:7987-7988`, and there is no
`scanner_at(offset)`), i.e. ~244 000 syscalls in the example. With a directory it is 1–2 `pread`s.
Cost: **16 B per datablock** (`first_sample_ordinal: u64`, `record_offset: u64`) = 0.012 % of a
128 KiB block. Verdict: **mandatory regardless of which payload layout wins** — E2 and B are
orthogonal and should both ship. See §7.

**E3 — planar payload + interleaved shadow for hot windows.** 2× storage. Only if a real workload
needs both patterns at full speed on the same recent data. Do not build speculatively.

---

## 7. The seek directory (needed by A, B and C alike)

Neither the extent layout nor any index removes the need to answer *"which record holds sample k?"*
in O(1). Three candidate mechanisms, costed:

| mechanism | per-record cost | lookup cost | format change |
|---|---|---|---|
| walk record headers | 0 | O(k) `pread`s of 32 B — **fatal** | none |
| `prev_same_block_offset` chain (`format.rs:485`) | +32 B footer on **every** record of **every** block type (`spec_needs_record_footer`, `format.rs:1992-1994`) | O(j) 32 B `pread`s backward from the tail; **already persisted, zero reader exists** | none — already written |
| **chunk-embedded directory record** | ~8–16 B per datablock, **0 extra syscalls** | O(1)–O(log) | **none** — it is just another block id |

The third is the right one and it is nearly free. `push_iter_inner` accumulates complete prepared
records into one reused `bytes` buffer (`stream.rs:1117, 1165`) and flushes with a single
`write_all` (`stream.rs:1279`). Appending **one extra record** to that buffer before the flush — a
`DatablockDirectory` record whose payload is `[(first_sample_ordinal: u64, record_offset: u64)]` for
the datablocks in this chunk — costs:
- 0 extra syscalls (same `write_all`),
- 32 B header + 16 B per datablock + 32 B footer, amortised at the default 16 384 records/chunk
  (`stream.rs:157`) to **~16.004 B per datablock = 0.012 %** of a 128 KiB block,
- one extra `set_tail` (O(log B)) and one `advance_coverage_with_tail` (O(1)) per chunk.

Directory records chain to each other via `prev_same_block_offset`, whose head is already persisted
as `DiskIndexTail { block_id, record_offset, sequence }` in the sidecar `TAILS_TABLE`
(`disk_index.rs:44, 1187-1192`). So "find sample k" is: read the tail directory record, binary
search its ordinal range, hop backward if too new. A second-level directory (one entry per
directory record) makes it O(1) at 16 B per 16 384 datablocks — free.

Two pre-existing hazards a design leaning on this must flag rather than inherit silently:
- `stage_state_records` (`stream.rs:1390-1392`) is `records[index+1..].iter().all(...)` per record →
  **O(chunk_len²)** integer compares per chunk; at the default 16 384 records that is up to ~2.7×10⁸
  compares per chunk whenever `block_offset_chain` is on with a sidecar. Trivial fix (one reverse
  pass computing last-index-per-block-id), but it must be fixed *before* this design leans on chains.
- On Windows, `read_exact_at` uses `FileExt::seek_read` (`snapshot.rs:257-261`), which reads at an
  explicit offset but also moves the handle cursor; `try_clone_file` duplicates a handle that
  **shares** the cursor. Any two-syscall `seek`-then-`read_exact` on a shared handle
  (`matrix.rs:2747-2749`, `file.rs:8779-8784`, `read_record_entry_at`) can be interleaved by another
  thread's positional read and silently return wrong bytes. **Design rule: on any handle reachable
  from more than one thread, `read_exact_at` only — never `cursor_at` / `try_clone_file` + `seek`.**
  This is what makes the `&self` multi-thread requirement real rather than nominal.

---

## 8. Comparison table

Query = "CH1 samples 1 000 000..1 000 100" (800 B useful), 4 ch × f64, S=4096, 128 KiB blocks,
directory present in every row so the comparison is layout-only.

| | A interleaved | B planar | C rec/(ch,chunk) | D matrix | E1 reorg |
|---|---|---|---|---|---|
| bytes read (byte gran.) | 3 200 (4.0×) | **800 (1.0×)** | 32 768 (41×) | 3 200 (4.0×)† | 800 |
| bytes read (4 KiB pages) | 8 192 | **4 096** | 36 864 | 8 192 | 4 096 |
| bytes read today (whole-payload API) | 131 072 | 131 072 | 32 768 | 3 200 | — |
| 1 000-sample query | 32 000 | **8 000** | 32 768 | 32 000 | 8 000 |
| full-CH1 scan of 1 TB | 1 TB | **256 GB** | 256 GB | 1 TB† | 256 GB |
| decode work | stride gather ×4 | **slice copy** | slice copy | stride gather | slice copy |
| alloc / sample (append) | **0** | **0** | **0** | n/a | 0 |
| syscall / sample (append) | **0** | **0** | **0** | ~3–5 **per cell** | 0 |
| syscall / block (chunk path) | **0** | **0** | **0** (4× records) | n/a | 0 |
| write streams | 1 | C (tile if C>64) | C | 1 | 1 |
| extra bytes / block | 0 | **16 (uniform) / 80 (variable)** | +192 framing | 0 | 2× storage |
| resident state / block | 104 B ×1 | 104 B ×1 | 104 B ×**4** + 1 sidecar row/record | bitmap bits | ×2 |
| buffering latency | 4.096 ms | **4.096 ms (same)** | 4.096 ms | n/a | +reorg lag |
| memory to serve query | O(len) | **O(len)** | O(chunk) | O(cell) | O(len) |
| TB scale | yes | **yes** | sidecar grows O(N) | **no — fixed extent, 16 M cells, 8 GiB** | 2× cost |
| wire format change | none | **none (payload convention)** | none | n/a | none |
| user-implementable today | write yes / read no | write yes / **read no** | **yes, fully** | n/a | yes |

† D at `n_ch = 4`; at `n_ch = 64` it is 64× and one useful `u64` per cache line.

---

## 9. Recommendation

**Ship B (planar extents inside the datablock payload, geometry in a 16 B in-payload prologue)
+ E2/§7 (chunk-embedded datablock directory record), and keep A as a declared alternative for
time-window-dominant workloads.**

Rationale, in one line each:
- B is the **only** layout that reduces bytes-read, and it does so by 4× (by `C×` in general) on the
  full-channel scan that is the owner's actual question.
- B costs the writer **the same instruction count per sample as A** and **the same buffering memory
  and latency as A**, because the owner's block-as-one-record shape already forces whole-block
  buffering. The famous "planar makes the writer buffer" objection does not apply here.
- B needs **no wire format change**: it is bytes inside a payload field. Schema hash unchanged,
  format version unchanged, old readers still see opaque payload bytes.
- The directory is 0.012 % of the data and rides an existing `write_all` at **zero extra syscalls**.
- C stays available and is the honest answer for anyone who needs channel access on the API as it
  exists today, with its sidecar-growth ceiling stated.
- D is out.

### What the recommendation costs the writer, explicitly

1. **Per sample: nothing measurable.** One store, one address computation, 0 allocations,
   0 syscalls — identical to A. Only the address arithmetic changes
   (`c*S*W + i*W` instead of `(i*C+c)*W`).
2. **Cache/TLB: `C` open write streams instead of 1.** Free to ~8 streams (L1), fine to ~64 (L2).
   Above ~64 channels, tiled transpose at `T = 64`: +1 load/store pair per sample (~1 ns), and a
   `64*C*W` scratch buffer (16 KiB at C=32).
3. **Per block: 16 B** (uniform geometry) **or 80 B** (variable geometry, includes per-extent CRC32)
   of directory, plus **16 B** in the block directory record. Total **≤ 96 B per 131 072 B = 0.073 %**.
4. **Fixed block geometry must be chosen before the block is emitted.** Already true today.
5. **Variable per-channel rates**, if needed, cost one concatenating `memcpy` of the payload at block
   close: +8 B/sample of copy (~1 % of a core at 32 MB/s), `C` reusable staging buffers, still
   0 allocations and 0 syscalls.
6. **If the hardware DMAs interleaved buffers**, the writer pays a transpose: one extra load/store
   pair per sample, 0 allocations, 0 syscalls, ~1–2 ns/sample. At 4 MS/s aggregate that is < 1 % of
   one core. This is the only case where B is not literally free, and it is still cheap.
7. **Choose `S` so extents are page-aligned for free.** `W=8` and `S ≡ 0 (mod 512)` makes every
   extent length a multiple of 4 096, so extents stay mutually page-aligned; alignment relative to
   the *device* additionally needs `payload_offset ≡ 0 (mod 4096)`, and `payload_offset =
   record_offset + 32` (`file.rs:1472-1478`) — so full page alignment costs up to 4 095 B of
   inter-record padding (**≤ 3 % at 128 KiB blocks**). Cheaper alternative: guarantee only 8 B
   alignment (`record_offset ≡ 0 mod 8`, ≤ 7 B padding, **≤ 0.005 %**), which is what zero-copy
   `&[f64]` extents actually require. Recommend the 8 B variant; make full page alignment opt-in.

### What must be added to the library (read side) — the whole gap is one function

```rust
// on VarveReader / VarveStreamReader / VarveIndexedReader
pub fn read_payload_range(
    &self,                              // &self — many threads, one handle
    entry: &RecordIndexEntry,
    offset: u64,                        // relative to payload_offset
    buffer: &mut [u8],                  // caller-owned: zero allocation
) -> Result<()>;
```

Backed directly by `SnapshotFile::read_exact_at` (`snapshot.rs:74-96`) which is already `&self`,
already bounds-checked, already `pread`/`seek_read`, already allocation-free. **One syscall,
zero allocations, `&self`.** Bound `offset + buffer.len() ≤ entry.payload_len` and reject compressed
records (a partial read of a compressed payload is meaningless — `RECORD_FLAG_COMPRESSED`,
`file.rs:43`).

The blocker is integrity, and it has exactly one honest answer: today
`read_payload_snapshot`/`verify_snapshot_record` (`file.rs:494-508, 8885-8916`) verify a checksum
over the **whole** payload; a partial read cannot. So a partial read must carry its own integrity:
**per-extent CRC32 in the B-variable directory**, verified over the extent actually read. That makes
B-variable (80 B/block) the recommended directory shape rather than B-uniform, and it means
`read_payload_range` returns bytes whose integrity is guaranteed by the payload's own directory, not
by the record checksum. The record checksum remains available and unchanged for whole-payload reads.

Two secondary additions, both small and both independently justified:
- `VarveStreamReader::refresh(&mut self)` — re-read the sidecar frontier and
  `snapshot.with_len(new_eof)` (~3 lines, no scan; `stream.rs:461` already does this once at open).
  Without it a channel reader cannot follow a live writer except by reopening.
- a public `scanner_at(offset)` so a reader resumes instead of restarting at `header_len`
  (`file.rs:7987-7988`); `read_stream_entry_at` (`file.rs:8057`) already exists as `pub(crate)`.

### Sequencing

1. Fix `stage_state_records` O(chunk_len²) (`stream.rs:1390-1392`) — one reverse pass. Prerequisite.
2. `read_payload_range(&self, ...)` + per-extent CRC32. Unblocks *everything*, including
   user-implemented channel extraction, and is independently useful.
3. Datablock directory record + second-level directory (§7). Fixes seek for A, B and C alike.
4. Planar payload convention: prologue + extent table, documented, with a writer helper that
   scatters samples into extents and a reader helper that returns `&[T]` or fills a caller buffer.
5. Optional: zero-copy `&[Sample]` extents via `slice_from_bytes` (today only `ref_from_bytes` /
   single `&T` exists — `file.rs:1256-1298, 1409-1436`), gated on the 8 B record alignment of item 7.
6. Independently: make `read_matrix_cell` `&self` by replacing `seek`+`read_exact`
   (`matrix.rs:2747-2749`) with `snapshot.read_exact_at`. Not part of this design, but it is the
   single smallest high-value matrix edit and it is on the same primitive.

### The answer to the owner's question

*Can a caller query only the CH1 data across the whole file?*

**Yes — but only if CH1's samples are written contiguously.** With the payload as drawn
(interleaved), a CH1-only query is possible logically and costs **the same I/O as reading all four
channels**; the only savings are decode CPU (~4×) and page-cache-vs-decode-buffer pressure. With
planar extents plus the directory, a CH1-only query reads **exactly CH1's bytes** — 1/C of the file,
one `pread` per datablock, `&self`, O(query) memory, unbounded file size. The library should expose
the extent-directory convention and `read_payload_range` so that a user can do this themselves even
before any higher-level channel API exists.
