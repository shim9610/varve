# Channel-selective access: the case enumeration

Scope: every access case the channel design must serve, consciously decline, or
declare physically impossible. Repo `C:\git\varve`, branch `codex/dev-next`, 0.4.0.
No repository file was modified while producing this.

---

## 0. Notation, constants, and the cost model every case below is derived from

Symbols used throughout:

| symbol | meaning |
|---|---|
| `N` | total records in the file (datablocks + metablocks + internal) |
| `D` | datablock records |
| `M` | metablock records |
| `C` | channels present in a given datablock |
| `S` | samples per channel in a given datablock |
| `w` | sample width in bytes (fixed-width case) |
| `P` | datablock payload bytes ≈ `C·S·w` |
| `K` | chunk size of the batch append path, default 16 384 records / 4 MiB (`stream.rs:156-159`) |

Verified constants:

- `RECORD_HEADER_LEN = 32`, `RECORD_FOOTER_LEN = 32` (`crates/varve-core/src/file.rs:41-42`).
  Footer present iff `commit_policy.requires_record_footer() || index_policy.requires_record_footer()`
  (`format.rs:1992-1994`) — so turning on `block_offset_chain` costs **+32 B on every record of
  every block type**, not just the chained ones.
- `size_of::<RecordIndexEntry>() ≈ 104 B` (fields `file.rs:385-401`), and
  `index_bytes_for_count` (`file.rs:8966-8979`) is exactly `count · 104`.
- `SnapshotFile::read_exact_at(&self, offset, buf)` (`snapshot.rs:74-95`, verified verbatim) —
  positional, `&self`, loops on partial reads, bounds-checked. **This is the only correct
  primitive for every case below, and it is `pub(crate)`.**
- `ordinal = key.scan · dim1 + key.ch` (`matrix.rs:2146-2164`, verified verbatim) —
  matrix is **scan-major / channel-minor**. `MatrixKey { scan, ch }` at `matrix.rs:834-838`.
- `VarveFile::blocks::<T>()` (`file.rs:3848-3851`) → `clone_matching_entries` (`file.rs:9063-9084`,
  verified): **two** full linear passes over the resident index, then `M·104 B` of memcpy.
- `StreamingBlocks::<T>::next` (`stream.rs:598-645`, verified): `if entry.block_id != T::ID { continue; }`
  at `stream.rs:620` — foreign records are skipped **after a 32-byte header read and before any
  payload read**. This is the cheapest existing type filter that does not need a resident index.
- `MmapPayloads::payload_window(&self, entry) -> &[u8]` (`file.rs:1219-1232`, verified):
  binary search on `record_offset` + full entry equality compare, no allocation, zero-copy.

### The three candidate layouts every case is costed against

- **L0 — INTERLEAVED** (what the owner described): datablock payload is
  `s0.ch1 s0.ch2 s0.ch3 s0.ch4 s1.ch1 …`. CH1's bytes are strided with period `C·w`.
- **L1 — PLANAR + IN-PAYLOAD EXTENT DIRECTORY** (the layout that actually fixes channel I/O):
  payload = `[dir][CH_a extent][CH_b extent]…`, each extent contiguous.
- **L2 — RECORD-PER-(CHANNEL,CHUNK)** (the existing idiomatic answer, `crates/varve/tests/tdms_model.rs:70-81`,
  `TdmsChannelChunk(id=103, key=[group,channel,chunk_index])`): de-interleave at write time
  into separate records. A modelling convention, not an API.

### The physics, stated once and not softened

For **L0**, a page is 4 096 B and holds `4096/(C·w)` stride units; **every one of them contains
CH1 bytes**. Reading CH1 alone therefore faults in **100 % of the pages of every datablock**.
For `C=4, w=8`: 32 B period, 128 stride units per page, 1 024 useful bytes out of 4 096.

> **No index, directory, chain, or sidecar can change this.** An index removes the *decode* of
> CH2..CH4 (which is the larger CPU cost), never the *read* of their bytes. Any design that
> claims 1/C I/O on interleaved payloads is false. The only fix is a writer-side layout change.

This is the identical failure the matrix subsystem already has: because
`ordinal = scan·dim1 + ch`, channel `k`'s cells are strided by `slot_stride·n_ch`, so a
whole-channel read of a TB matrix touches every page of the slot region. The matrix is
optimised for the *opposite* query (a whole scan is contiguous, one pread).

### L1 cost derivation (used by most cases below)

Proposed in-payload directory, 8 B fixed header + 16 B per channel entry
(`channel_id: u32, sample_count: u32, byte_offset: u32, byte_len: u32`):

- **Per datablock, on disk:** `8 + 16·C` bytes, plus ≤ 7 B alignment padding per extent.
  For `C=4, S=1024, w=8` (`P = 32 KiB`): `72 B + ≤28 B ≈ 0.3 %` overhead.
- **Per datablock, on write:** zero extra syscalls (the directory is part of the same payload
  buffer already being written), zero extra allocations if the writer reserves the directory
  region up front and back-patches it. `C` `u32` stores + one `memcpy` per channel.
- **Per record, on append:** **unchanged** — no new per-record syscall, no new per-record
  allocation, no new O(N) work. (Note the append path already fails the
  "no per-record heap allocation" rule for a different reason: `prepare_stream_user_record`
  returns `PreparedStreamRecord { bytes: Vec<u8> }`, `file.rs:8105-8113`, then
  `stream.rs:1165` copies it into the chunk buffer — 2 allocations + 1 redundant copy per
  record on the *batch* path today. That is pre-existing and orthogonal.)
- **Per datablock, on read of one channel:** 1 pread of `8+16C` B for the directory
  (≤ 72 B, single page), 1 pread of `S·w` B for the extent. **2 preads, `S·w` useful bytes,
  ≤ 2 partial pages of waste at the extent edges.** Amplification `1 + 8192/(S·w)`;
  at `S·w = 8 KiB` that is 2.0×, at `S·w = 1 MiB` it is 1.008×.

### Substrates (which machinery a case can actually run on)

| id | substrate | open cost | resident memory | `&self` reads | verdict for TB |
|---|---|---|---|---|---|
| S-RES | `VarveFile` / `VarveReader` resident index | Θ(N), ~2 syscalls/record (`file.rs:8665-8771`) | `104·N` B (10⁹ recs = 104 GB) | yes (`blocks` `file.rs:3848`, `BlockVec::get` `collections.rs:100`) | **disqualified** |
| S-STREAM | `VarveStreamReader` forward scan (`stream.rs:456-473`) | O(header), no scan | O(B) | iteration is `&mut` on the scanner, but the scanner is cheap to create per thread | **qualified**, forward-only, `high-cardinality-dev` gated |
| S-IDX | `VarveIndexedReader::lookup/get` (`indexed.rs:210-243`) | O(header) + 1 redb read txn | O(B) | **yes, `&self`** | qualified, **exact key only — no range/prefix** |
| S-CHAIN | `prev_same_block_offset` backward walk (`format.rs:483-485`, footer field `native_layout.rs:122-158`) | none | O(1) | would be | qualified, **no reader exists** |
| S-MMAP | `MmapPayloads` (`file.rs:1194-1330`) | Θ(N) index copy | `104·N` + mapping | yes | disqualified (resident index; and its unsafe precondition — no mutation through any handle/thread/process, `file.rs:1721-1757` — is **false by construction for an appending stream**) |
| S-DIR | *proposed* chunk-embedded directory record | — | O(1) | — | see §C2 |

`S-DIR` in one line: `push_iter_inner` accumulates prepared records into one buffer and flushes
with a single `write_all` per chunk (`stream.rs:1165`, `stream.rs:1279`). Appending **one extra
record per chunk** whose payload is `[record_offset u64, first_sample_index u64]` per datablock
in that chunk costs **0 extra syscalls**, `32 + 16·D_chunk + 32` bytes, one extra `set_tail`
(O(log B)) and one extra `advance_coverage_with_tail` (O(1)). Amortised at default `K`:
**≈ 16.004 B per datablock**, i.e. 0.05 % on an 8 KiB datablock. Because directory records are
themselves a block id, they are automatically chained by `prev_same_block_offset` and their head
is already persisted in the sidecar `TAILS_TABLE` (`disk_index.rs:44,1187-1192`).

---

## 1. The cases

### C1 — All CH1 samples from the start, streaming, no materialisation

**Scenario.** 6 TB acquisition file, 4 channels, operator wants to plot CH1 end to end;
process has 8 GB of RAM.

**Operation.** `for_each_channel_extent(ch=1, from=Start) -> impl Iterator<Item = Result<&[T]>>`
built on `S-STREAM`: forward scan, skip non-datablock ids on the 32-byte header, and for each
datablock read only CH1's extent.

**Cost.**

| | L0 interleaved | L1 planar | L2 record-per-channel |
|---|---|---|---|
| header reads | `N` × 32 B | `N` × 32 B | `N` × 32 B |
| payload bytes read | **`D·P` (100 %)** | `D·(dir + S·w)` ≈ `P/C` | `D_ch1 · S·w` |
| preads | `N + D` | `N + 2D` | `N + D_ch1` |
| decode | CH1 only (this is the only L0 saving) | CH1 only | CH1 only |
| allocations | 1/datablock unless read-into-buffer exists | 0 with caller buffer | 1/record (`BlockVec::get`, `collections.rs:120-124`) |
| memory | O(1) + one extent | O(1) + one extent | O(1) + one payload |

**Target:** O(N) header reads, O(`D·S·w`) payload bytes, **O(1) memory**.
**Blocker today:** the per-datablock payload read is all-or-nothing —
`read_logical_payload_snapshot` (`file.rs:512-529`) always materialises the whole payload into a
fresh `Vec`. There is no `(entry, byte_offset, byte_len, &mut [u8])` API on the record path.
**Verdict:** serve. Under L0, serve with an explicit "reads 100 % of datablock bytes" contract.

---

### C2 — CH1 samples in a global sample range `k .. k+n` (the "slice")

**Scenario.** UI scrubs to 4.2 h into a 12 h recording and wants 2 s of CH1.

**Operation.** `channel_slice(ch, k, n)`. Requires mapping a **global sample ordinal** to
(record_offset, extent, intra-extent offset). That mapping does not exist anywhere today.

**Cost, three ways of getting it:**

1. **No structure (walk).** O(#datablocks before k) header reads to accumulate counts —
   for the 4.2 h point on a 12 h file, ~35 % of the file's records, each a 32 B pread.
   Unacceptable for interactive use, but correct and O(1) memory.
2. **`S-CHAIN` backward from tail.** Same order of magnitude, opposite direction, and the chain
   is backward-only, so a from-the-head query first walks the whole chain. O(D) preads, O(1) memory.
3. **`S-DIR` chunk directory carrying `(record_offset, first_sample_index_per_channel)`.**
   Locate the chunk in O(log #chunks) if the directory records are themselves indexed by a
   second-level directory, else O(#chunks) pointer chases at 32 B + payload each. With
   `K = 16 384` and 8 KiB datablocks, #chunks ≈ file_size / 128 MiB — a 6 TB file has ~48 000
   chunks, so even a linear walk of directory records is 48 000 preads (~50 ms on NVMe) with
   O(1) memory, and a one-level skip structure makes it ~220. **This is the recommended answer.**

Then within the located datablock: 1 directory pread + 1 extent pread, and the slice is
`extent[(k - first_sample) · w ..]`.

**Target:** O(log D) locate + O(n·w / page) transfer, **O(1) memory**.
**Requires:** monotone per-channel sample counters persisted at chunk granularity. Under L1 the
per-block directory already carries `sample_count` per channel, so the chunk directory only needs
the running prefix sum — 8 B per channel per chunk, not per record.
**Verdict:** serve, but only with S-DIR. Without it, decline and say so: an append-only log with
variable-length records has **no** O(1) sample→offset map.

---

### C3 — Same slice, zero-copy, fixed-width sample type

**Scenario.** FFT over CH1 wants `&[f64]` with no copy and no decode.

**Operation.** `channel_extent_raw::<T>(...) -> &[T]`.

**What exists.** `MmapPayloads::raw_fixed::<T>(block_index) -> Option<&T>` (`file.rs:1256-1298`)
and `MmapMatrix::raw_cell::<T>` (`file.rs:1409-1436`). Both return **a single `&T`** via
`zerocopy::FromBytes::ref_from_bytes`. A channel extent is `&[T]` — needs
`slice_from_bytes`/`try_ref_from_bytes` plus an extent length. Not used anywhere in the crate.

**Four hard preconditions, each checkable:**

1. `KIND` match, `RAW_ENDIAN == T::ENDIAN.unwrap_or(spec.endian)`, size, alignment — the four
   existing checks (`ZeroCopyBlockKindMismatch`, `ZeroCopyEndianMismatch`,
   `ZeroCopyPayloadSizeMismatch`, `ZeroCopyAlignmentMismatch`).
2. **Alignment is a writer obligation.** Extent at payload byte offset `x` is aligned iff
   `(mmap_base + record_offset + 32 + x) % align_of::<T>() == 0`. `mmap_base` is page aligned and
   `payload_offset == record_offset + 32` (asserted `file.rs:1473-1484`), so it reduces to
   `(record_offset + 32 + x) % align == 0`. **The append path does not guarantee aligned record
   starts today.** Cost of fixing: ≤ 7 B inter-record padding + ≤ 7 B per extent.
3. **Cross-endian files cannot be zero-copy** — the equality check errors rather than falling
   back. The API needs a copying sibling (`channel_extent_into(&mut [T])`).
4. **mmap is unsound on a file being appended** (`file.rs:1721-1757` precondition). So zero-copy
   is available for **sealed generations only**, or via a *bounded* map over
   `[header_len, committed_eof)` where `committed_eof` is the sidecar frontier (`stream.rs:461`)
   — that region is immutable by the append-only invariant, and remapping to extend is cheap.

**Target:** 0 copies, 0 allocations, page faults proportional to `n·w`.
**Verdict:** serve for L1 + fixed-width + sealed-or-bounded map. **Decline for L0** — an
interleaved payload has no contiguous `&[T]` for one channel, full stop; the only zero-copy view
of L0 is a strided iterator over the whole payload, which is not zero-*I/O*.

---

### C4 — CH1 restricted to a datablock range, or a time/scan range

**Scenario.** "Show CH1 for datablocks 10 000..10 100", or "for scans 5e9..5.1e9".

**Operation.** `channel_range(ch, BlockRange | ScanRange)`.

**What the format expresses today: neither.**

- There is no time axis anywhere in the record format. Records carry `sequence`,
  `record_offset`, `block_id`, `block_version`, flags, checksum (`file.rs:385-401`) — no
  timestamp, no scan index.
- There is no "k-th record of type T" operator without the resident index. `S-STREAM`'s scanner
  always starts at `header_len` (`file.rs:7987-7988`); `read_stream_entry_at` (`file.rs:8057`) is
  the positional single-record read but is `pub(crate)` and feature-gated. There is **no public
  `scanner_at(offset)`**.

**Costs.** Block-ordinal range: O(k) header reads to reach ordinal k without a directory; O(1) +
O(range) with S-DIR. Scan/time range: reduces to C2 once the datablock carries a
`first_scan_index` (or `start_time`) — 8 B in the payload directory header, or better in the
metablock (see C9).

**Target:** O(log D) locate, O(range) transfer, O(1) memory.
**Verdict:** serve **block-ordinal** ranges (cheap, needs only S-DIR). Serve **scan/time**
ranges only if the design adds a monotone scan/time field to the datablock directory header —
that is a format decision, listed in open questions. Do not promise time-range queries on a
format that has no time.

---

### C5 — Several channels in one pass (CH1 + CH3), payload read once

**Scenario.** Cross-correlation of two of 64 channels.

**Operation.** `channels_slice(&[1,3], range) -> impl Iterator<Item = (ch, &[T])>`, one pass.

**Cost.**

- **L0:** free relative to C1 — you were already reading 100 % of the bytes, so reading `j`
  channels costs the same I/O as reading 1, and `j/C` of the decode. This is L0's *only*
  favourable case, and it should be stated as such.
- **L1:** `1 + j` preads per datablock (one directory + one per extent). For adjacent channel
  ids, coalesce contiguous extents into a single pread — the directory makes adjacency
  detectable for free. Bytes read `≈ j·S·w`. **Linear in `j`, which is the correct behaviour.**
- **L2:** `j` independent record streams; either `j` scanners (j× the header reads) or one
  scanner with a `block_id ∈ set` filter — the latter needs `StreamingBlocks` generalised from
  one `T::ID` (`stream.rs:620`) to a set. Cheap change, no format impact.

**Anti-requirement to enforce in the API:** the signature must take a **channel set**, not be
`n` calls to a single-channel function, or L1 pays `n` full passes. Target: **one pass, one
scanner, `j+1` preads per datablock, O(j) live extents in memory.**
**Verdict:** serve.

---

### C6 — All channels of one datablock (the natural row read)

**Scenario.** Time-domain viewer showing all 4 traces for one window; or the acquisition
consumer that just wants the block back.

**Operation.** `datablock(block_ordinal) -> Row` / the existing typed decode.

**Cost.** This is the **cheapest** case in every layout and the one the current code is already
good at: one pread of the whole payload, one decode.
- L0: 1 pread, `P` bytes, decode all — perfect locality.
- L1: 1 pread of `P` bytes (directory + all extents are contiguous), then `C` sub-slices at
  zero I/O cost. **L1 costs L0 nothing here** — this is the key point that makes L1 the
  dominant choice: planar layout does not penalise the row read at all, because the row read
  is a single sequential extent either way.
- L2: `C` preads across `C` records, likely non-adjacent → **L2 is the layout that penalises
  the row read**, by `C`× the syscalls and up to `C`× the seek distance.
- Matrix: contiguous (`ordinal = scan·dim1 + ch`), 1 pread of `n_ch·slot_stride` — but there is
  **no row API**; only `read_matrix_cell`, so today it is `C` positional reads.
  (*Correction, round 16:* this line originally said `&mut self` and "seek+read pairs". Both are
  now false — `read_matrix_cell` takes `&self` and goes through `MatrixRegionReader::read_exact_at`.
  The absence of a row API is unchanged, so the `C`-syscall verdict below still stands.)

**Target:** 1 pread, `P` bytes, 0 or 1 allocation.
**Verdict:** serve. Note explicitly in the design that L1 preserves this case and L2 damages it.

---

### C7 — Counts: samples per channel, and total scans, without walking payloads

**Scenario.** Open a file and populate "CH1: 4.2 G samples, 12 h" in the UI in under 100 ms.

**Operation.** `channel_stats() -> Map<ch, {samples, first_offset, last_offset}>`.

**Cost.**

- **Walk-free is impossible without a persisted aggregate.** Append-only means the total is not
  known until the last record; the only cheap place to keep it is a monotone counter written at
  chunk granularity (S-DIR) or in the sidecar `META_TABLE`
  (`disk_index.rs:42`, currently one row of `DiskIndexMetadata` holding an eof/sequence frontier
  and a generation witness — extending it with a per-channel counter is O(#channels) per commit,
  not per record).
- With S-DIR: read the **last** directory record (its offset is already in the sidecar
  `TAILS_TABLE`, `disk_index.rs:1187-1192`) → **1 lookup + 1 pread, O(1) memory, O(#channels) bytes.**
- Without it: O(D) header reads (L1 directory has per-channel `sample_count`, so payload reads
  are still avoidable if the directory is at a *fixed* offset within the payload — 1 extra pread
  per datablock, `8+16C` B) or full payload walk under L0.

**Target:** O(1) preads, O(#channels) memory.
**Verdict:** serve **only** with a persisted per-channel counter. Otherwise decline explicitly:
"counting is O(D)". Do not silently make `len()` an O(D) walk — that is the classic trap.

---

### C8 — Metablocks only, skipping every datablock (mirror image of the main query)

**Scenario.** Rebuild the configuration timeline of a 6 TB file: read the ~5 000 metablocks and
none of the ~800 M datablocks.

**Operation.** `stream.blocks::<MetaBlock>()`.

**This case is the litmus test for whether per-type separation is efficient. Three answers exist
and they differ by four orders of magnitude:**

1. **S-RES `blocks::<T>()`** — Θ(N) open scan (2 syscalls/record, 104·N resident) then **two**
   more linear passes (`file.rs:9063-9084`). For N = 8·10⁸: 83 GB resident. **Unusable.**
2. **S-STREAM `StreamingBlocks`** — `entry.block_id != T::ID → continue` (`stream.rs:620`) before
   any payload read. Cost: **`N` × 32 B header preads = 25 GB of header traffic**, 0 datablock
   payload bytes, O(1) memory. Correct memory behaviour, but still O(N) syscalls; ~800 M preads
   is minutes, not milliseconds.
3. **S-CHAIN `prev_same_block_offset`** — the metablock chain has exactly `M` links. Walking it
   is **`M` preads of 32 B = 5 000 preads, O(1) memory, O(M) time**, and the head is already in
   the sidecar `TAILS_TABLE`. **This is the right answer and it works on files written today**
   (`format.rs:483-485` + `native_layout.rs:122-158` persist it) — the write side maintains it,
   the read side has **no traversal function at all** (grepped: every occurrence is a write, a
   validation, or a clear).

**Target:** O(M) preads, O(1) memory, **independent of D**.
**Verdict:** serve via S-CHAIN. This single missing reader is the highest-value, lowest-cost item
in the whole design: no format change, 32 B/record already being paid whenever the policy is on.
**Caveat:** the chain is **backward only** (tail→head), so "metablocks in file order" requires
either buffering `M` offsets (`8·M` B = 40 KB here, fine) or a forward structure.
**Precondition:** metablocks and datablocks must be **distinct `block_id`s**. If they share an id
and differ by payload content, none of 2 or 3 works and every discriminator becomes a payload read.

---

### C9 — Correlating a metablock with the datablocks it governs

**Scenario.** Gain changes at t=3 h; samples before and after must be scaled differently. This is
the entire reason metablocks are interspersed rather than collected in a header.

**What the format guarantees today — precisely, and it is less than it looks:**

- Records are appended strictly at EOF (`stream.rs:1253-1257` asserts
  `records[0].record_offset == eof`), so **`record_offset` order == append order**, total and
  strict. This is the *only* intrinsic ordering guarantee.
- `sequence` values are validated **unique** (`validate_unique_sequences`, `file.rs:8760`), not
  necessarily dense or monotone-usable for arithmetic. **Use offsets for ordering, not sequences.**
- There is **no** "applies-to" relation in the format. Nothing scopes a metablock to anything.

**So the scoping rule must be created by the design, and there are exactly two shapes:**

- **(a) Implicit half-open interval.** A metablock governs every datablock with
  `record_offset ∈ [meta.record_offset, next_meta.record_offset)`. Costs 0 bytes. Forward lookup
  (meta → its datablocks) is a scan between two offsets: cheap. **Reverse lookup (datablock → its
  metablock) requires walking backward to the nearest preceding metablock** — under S-CHAIN that
  is O(M) worst case per query, because the metablock chain has no offset ordering shortcut other
  than the chain itself. Fragile under compaction and under any future out-of-order write.
- **(b) Explicit epoch pointer.** Each datablock carries `meta_offset: u64` (or a small
  `epoch_id: u32`) in its payload directory header. Costs **8 B (or 4 B) per datablock** —
  0.02 % on an 8 KiB block — and **0 extra syscalls, 0 allocations** (it is written into the same
  payload buffer). Reverse lookup becomes **O(1)**; forward lookup stays a range scan.
  With a 4-byte epoch id plus a small epoch table in the chunk directory, it is 4 B/datablock.

**Recommendation to state in the design:** (b). Half-open-interval scoping is the thing that
breaks first under compaction (§C14) and under the sparse-channel case (§C11), because both
change which datablocks sit between two metablocks.

**Target:** datablock→metablock O(1); metablock→datablocks O(range).
**Verdict:** serve, and **state the guarantee explicitly in the format docs** — "the metablock in
force for a datablock is the one it names; in its absence, the most recent metablock at a lower
record offset". Anything vaguer is unimplementable.

---

### C10 — Tail-follow: live reader consuming CH1 while the writer appends

**Scenario.** Real-time strip chart during acquisition.

**Cost / feasibility, per substrate:**

- **S-RES: impossible.** `open_readonly` (`file.rs:2533-2571`) pins the snapshot at
  `validated_snapshot_len` and there is **no refresh/reload/follow method anywhere** in the crate.
  Following means reopening, which is Θ(N) with a 104·N resident floor. Dead end.
- **S-STREAM: nearly free, and the design should build on it.** Visibility is decided by one
  line — `reader.snapshot = reader.snapshot.with_len(snapshot.committed_eof())?` (`stream.rs:461`)
  — the **sidecar-committed** eof, not the native eof. Records written but not yet folded into a
  committed sidecar transaction are invisible: correct, fail-closed. Visibility advances in
  **chunk-sized steps** (commit at `batch_records >= chunk_records`, `stream.rs:1341-1347`, plus
  on flush `stream.rs:1040-1045` and sync `stream.rs:1047-1065`), so the follower's latency floor
  is one chunk — at default `K = 16 384` records that may be seconds. **A live reader needs the
  writer to flush at the latency it wants; say so.**
- Two small additions make tail-follow real, neither a format change:
  1. `refresh(&mut self)` — re-read the sidecar frontier, `snapshot.with_len(new_eof)`. ~3 lines.
  2. **`scanner_at(offset)`** — resume where the previous poll stopped instead of restarting at
     `header_len` (`file.rs:7987-7988`). Without it, each poll is O(N) header reads and a
     following reader is quadratic in wall-clock time. **This is a blocker, not a nicety.**
- **Matrix: impossible.** `load_commit_bitmaps` (`matrix.rs:2518`) copies commit bits into RAM at
  open and `cell_status` (`matrix.rs:2774`) is a pure in-memory `bits.get(ordinal)` that never
  touches disk. A reader opened before a cell is committed reports `NotCommitted` **forever**,
  and `read_cell` refuses it (`matrix.rs:2734-2736`). No invalidation path exists.

**Target:** poll cost O(new records), O(1) memory, latency = writer flush interval.
**Verdict:** serve on S-STREAM after adding `refresh` + `scanner_at`. **Decline for matrix.**
**Also decline mmap-backed zero-copy for followers** (§C3) — the unsafe precondition is false
while appending; a follower may zero-copy only up to the last committed eof under a bounded map.

---

### C11 — A channel present only in some datablocks (channel set changes mid-stream)

**Scenario.** CH5 is plugged in 2 h into an 8 h run; CH2's amplifier is disabled at 5 h. Normal
in acquisition, and the design must not assume a fixed channel set.

**Implications, which are structural and reach further than they look:**

- **Kills the matrix outright as the storage model.** Matrix dimensions are fixed at create time
  (`create_with_dims`, `file.rs:1781,2368`; `create_layout`, `matrix.rs:2190` is create-only, and
  open validates persisted lengths against recomputed dimension-derived lengths,
  `matrix.rs:2438-2453`). A dynamic channel set cannot be expressed, and neither can an
  unbounded scan count. Combined with `max_matrix_cells = 16 M` (`format.rs:114`) and
  `max_matrix_slot_region_len = 8 GiB` (`format.rs:118`), the matrix is a preallocated grid with
  an append log after it (`matrix.rs:2278-2284`) — it is **not** a stream store.
- **Kills any position-based channel addressing inside the payload.** "CH1 is the first extent"
  is wrong the moment a channel appears or disappears. The in-payload directory must be keyed by
  a **stable `channel_id`**, and lookup must be a search over `C` entries (≤ 64 typically; a
  linear scan over 16 B entries in one cache line group — nanoseconds, no allocation).
- **A channel-selective read must tolerate absence, not error.** `channel_extent(block, ch)`
  returns `Option`; the iterator in C1 **skips** blocks lacking the channel. Cost: 1 directory
  pread per skipped block (`8+16C` B), no extent pread. This is why the directory must be
  readable **without** reading the extents — i.e. at a **fixed offset at the head of the payload**
  with a fixed-size header, so its length is known from one pread.
- **Per-channel counters (C7) become sparse.** `samples[ch]` is defined only over blocks
  containing `ch`; the "global sample index" of C2 must be **per-channel**, not shared.
  Two channels at different rates have different `k` axes. This must be explicit in the API:
  `channel_slice(ch, k, n)` where `k` counts CH-`ch` samples, never scans.
- **The channel-set change is exactly what a metablock announces** — which is the argument for
  the explicit epoch pointer of C9(b): "channel set for this epoch" lives in the metablock, and
  the datablock names its epoch in 4 bytes.

**Target:** absence detected in 1 pread of ≤ 72 B; no error; no per-file fixed channel table.
**Verdict:** serve. **Flag as a hard rejection of the matrix subsystem for this workload.**

---

### C12 — Ragged blocks: differing sample counts per datablock (and per channel within a block)

**Scenario.** DMA buffers deliver 1024, 1024, 917, 1024… samples; a slow channel delivers 128
while a fast one delivers 1024 in the same block.

**Implications:**

- The in-payload directory must carry **per-channel `sample_count` and `byte_len`**, not a single
  block-level count with computed offsets. `16 B` per channel entry already does this; the cost
  of raggedness is therefore **zero extra bytes** relative to the uniform case. Good.
- **This is precisely what `ChunkedBytes` forbids and why it cannot be reused as-is.**
  `validate_chunked_bytes` (`chunks.rs:200-250`) requires `chunk_count == ceil(uncompressed_len/chunk_len)`
  and entry `i`'s `uncompressed_len == chunk_len` for all but the last (`chunks.rs:226-235`) —
  uniform chunk size is *enforced*, even though the per-entry length field exists. Its **wire
  shape is exactly right** (32 B header + `16 B`/entry directory + concatenated bodies,
  `chunks.rs:46-64`, offsets = prefix sum over the directory) and should be reused verbatim as
  the model for the extent directory. What must change: variable per-entry lengths, a public
  directory accessor (`parse_header`/`parse_entry` are private, `chunks.rs:252,281`), a
  "decode extent `i`" operation (only `decode_to_vec[_limited]` exist, `chunks.rs:82,89`, both
  looping over **all** chunks), and a way to read the directory without materialising the blob
  (today `decode_varve` is `from_encoded(Vec::<u8>::decode_varve(..))`, `chunks.rs:186`, i.e.
  the whole payload is already in RAM before the directory is even parseable — selectivity is
  defeated before the container is consulted).
- **Raggedness breaks any closed-form sample→block arithmetic.** C2 must use the persisted prefix
  sums; there is no `k / samples_per_block`. Do not design one.
- Compression interacts: if extents are compressed, `byte_len` is the stored length and
  `sample_count` the logical one; both are needed, which the 16 B entry already provides.

**Target:** raggedness costs 0 extra bytes and 0 extra I/O; only closed-form arithmetic is lost.
**Verdict:** serve. Explicitly forbid any API that implies a uniform block size.

---

### C13 — Concurrency: one thread per channel; several threads on one channel

**Scenario.** 8 worker threads, each decoding one channel of the same file; and a parallel FFT
splitting CH1's range across 4 threads.

**What holds today (derived from fields, no `unsafe impl Send/Sync` exists anywhere in the crate):**

- `SnapshotFile` = `{ Arc<File>, SnapshotBounds }` (`snapshot.rs:15-18`) → `Send + Sync`.
  `read_exact_at` is `&self` (`snapshot.rs:74`, verified) and uses `pread`/`seek_read`.
- `RecordIndexEntry` is scalars + `Option<u64>` → `Send + Sync`, and `clone` is a flat copy.
- `BlockVec<T>` = `{FormatSpec, SnapshotFile, Vec<RecordIndexEntry>, PhantomData<T>}`
  (`collections.rs:62-68`) → `Send+Sync` iff `T` is; `len/get/iter` are all `&self`
  (`collections.rs:92,100,127`).
- `VarveIndexedReader::lookup/get` are `&self` (`indexed.rs:210,225`).
- **So: two threads holding `&VarveReader` can read different channels concurrently today**,
  provided the modelling is L2 (one record per channel). That is the one thing that works now.

**What blocks it:**

1. ~~**Matrix reads are `&mut self` for no logical reason.**~~ **LANDED, round 16 — this no longer
   blocks anything.** It was true when this document was written: `read_matrix_cell` threaded
   `&mut self.file` into `matrix::read_cell` (signature `file: &mut File`), which did `seek` +
   `read_exact`. It now takes `&self` on all three handle types (`VarveFile`, `VarveReader`,
   `VarveWriter`), as do `matrix_cell_payload`, `read_matrix_aux`, `matrix_aux_len` and
   `matrix_cell_status`. The seek/read pair is replaced by `MatrixRegionReader::read_exact_at`
   (`matrix.rs`, `mod region_reader`), a `pread`/`seek_read` over an immutable snapshot; there is
   one per-bitmap `Mutex` in the matrix read path, taken only for `O(1)` map operations and never
   held across a read (it exists so a demand fault-in can happen under `&self`), and `Sync` is
   *derived* from the fields rather than asserted. On Windows each reading thread also gets its
   own `ReOpenFile`-derived handle, because `ReadFile` serialises on the kernel file object. Pinned by `crates/varve/tests/matrix_concurrent_reads.rs`
   (`every_matrix_read_entry_point_takes_a_shared_borrow` fails the build if any of them takes
   `&mut self` again; `one_shared_handle_does_not_serialise_concurrent_readers` fails if a convoy
   is reintroduced). Point 2 below — the Windows cursor hazard — was confirmed and is the reason
   the escape hatch is `RecordFile::matrix_region_reader(&self)`, whose `&File` is private to its
   module, rather than a cloned handle.
2. **Windows cursor hazard — this is a correctness bug waiting, and the design must forbid the
   pattern.** `read_exact_at` on Windows uses `std::os::windows::fs::FileExt::seek_read`
   (`snapshot.rs:257-261`), which reads at an explicit offset **but also moves the handle's file
   pointer**. Separately `SnapshotFile::cursor_at` (`snapshot.rs:57`) and `try_clone_file`
   (`snapshot.rs:69-71`, verified) hand out cursor-based views, and on Windows `File::try_clone`
   duplicates the handle — **duplicated handles share one file pointer**. Any code doing
   seek-then-read as two syscalls on such a handle (`matrix::read_cell` `matrix.rs:2747-2749`;
   `read_payload_file_validated` `file.rs:8779-8784`; `read_record_entry_at` inside
   `NativeStreamScanner`) can be interleaved by another thread's positional read and **return
   the wrong bytes silently**. Consequence: **two concurrent `NativeStreamScanner`s derived from
   one `SnapshotFile` are unsafe on Windows today.** Design rule to publish: on any handle
   reachable from more than one thread, use `read_exact_at` only — never `cursor_at` /
   `try_clone_file` + seek. (Could not compile-verify the `seek_read` cursor side effect under
   the read-only constraint; treat as a constraint to confirm.)
3. **Scanners are `&mut`.** `StreamingBlocks::next` is `&mut self`. Per-thread scanners are the
   answer, and they are cheap (`NativeStreamScanner` = FormatSpec + File + SnapshotFile + u64s),
   but each thread then pays its own O(N) header walk unless `scanner_at(offset)` exists —
   the same missing primitive as C10. With `scanner_at`, an `n`-way split of C1 is
   `n` scanners each covering `1/n` of the file: **linear speedup, O(n) memory.**

**Target:** `&self` everywhere on the read path; `n` threads → `n`× throughput, O(n) memory;
zero shared mutable state beyond `Arc<File>`.
**Verdict:** serve. Two prerequisites: `read_matrix_cell` → `&self`, and `scanner_at`.

---

### C14 — What happens to every case after replacement, rewrite, compaction, generation change

Verified behaviour of the four mutations, and what each does to a channel query:

| mutation | offsets | chain | consequence for channel access |
|---|---|---|---|
| `replace_fixed` / in-place (`file.rs:3540-3555`) | unchanged | intact | **Nothing breaks.** Only `sequence` + checksum rewritten. Cached offsets stay valid; a concurrent reader may see old-or-new payload, never torn (checksum guards). |
| `replace_rewrite` / `ReplaceStrategy::RewriteFile` | — | — | **Refused for footer specs**: `file.rs:3568-3573` returns `InvalidFormatSpec("replace is not supported for record-footer formats")`. So enabling `block_offset_chain` (which C8 depends on) **removes whole-file replace entirely**. That is a real trade the owner must accept. Upside: `rewrite_record_streaming`'s unconditional `entry.prev_same_block_offset = None` (`file.rs:5739`) can never destroy a live chain. |
| `replace_block` (typed, `file.rs:3256` → `file.rs:5761`) | **all translated** by the size delta (`translate_record_offset` `file.rs:597-612`, applied `file.rs:5815-5818`), then `validate_replacement_predecessors` (`file.rs:5872-5900`) re-verifies each pointer | survives, renumbered | **Every cached offset a user holds becomes silently wrong.** Any exposed offset must be paired with a generation/epoch token that makes a stale read fail loudly. |
| compaction (`compact_keyed_files*`, `file.rs:7040-7072`) | all new | regenerated from scratch by `BlockTails` on the new file | Semantically survives. **But `collect_merged_keyed_values` holds the whole result set in a `HashMap` in RAM (`file.rs:7085`) — compaction is not TB-scale and must be declared out of scope for stream files.** Also: compaction can drop datablocks, which **breaks C9(a) interval scoping** (the metablock now governs a different set) while C9(b) epoch pointers survive intact. Another argument for (b). |
| generation change (atomic rename, `replace_path_atomically`, `file.rs:3667`) | new file object | offsets are raw absolute u64 → survive only where explicitly translated | The crate's own defences are `validate_source_generation` (`file.rs:3593`) and the sidecar `primary_generation` witness (`verify_primary_generation`, `stream.rs:463`, `indexed.rs:180`). **A user-held offset across a generation change reads translated-away bytes with no error.** |

**Design rules that follow, and they are non-negotiable if offsets are exposed at all:**

1. Any public handle carrying a `record_offset` must carry the generation witness, and every
   read through it must check it. Cost: 8–16 B per handle, one integer compare per read.
2. The persisted chunk directory (S-DIR) contains offsets — it must be **invalidated or rebuilt**
   by `replace_block` and by compaction. Since `replace_block` already walks and translates every
   pointer, adding directory records to that walk is the same O(records-after) cost it already pays.
3. Declare: **channel queries are defined against a single generation.** A follower that observes
   a generation change must re-open, not refresh.

**Verdict:** serve C1–C13 within a generation; **decline any cross-generation offset stability
guarantee.**

---

### C15 — The user-implements-it path

**What a user can build today on the public API, and at what cost:**

| case | buildable today? | how | cost |
|---|---|---|---|
| C6 row read | **yes** | model as L2 or decode the whole payload | 1 pread + decode |
| C1 stream CH1 | **partially** | `VarveStreamReader::blocks::<T>()` + hand-slice the decoded payload | needs `high-cardinality-dev`; **payload always fully materialised** (`file.rs:512-529`), so no I/O saving even under L1 |
| C1/C3 on a sealed ≤8 GiB file | **yes** | `MmapPayloads::payload_window` (`file.rs:1219`) returns the whole extent zero-copy; sub-slicing faults only the pages touched, so a caller who knows the channel's byte extent pays only that extent | requires `scan_on_open` (Θ(N) open, 104·N resident) + `mmap` feature + an immutable file |
| C8 metablocks only | **partially** | `StreamingBlocks` id filter | O(N) 32 B preads; the O(M) chain walk is not reachable |
| C13 concurrency | **yes** for L2 | `&VarveReader` + per-thread `BlockVec` | works; Windows cursor rule applies |
| C2 slice, C4 ranges, C7 counts, C10 follow | **no** | — | — |
| exact-key point lookup | **yes** | `VarveIndexedReader::get::<T>(&self, key)` (`indexed.rs:210-243`) — `&self`, no open scan, bounded memory | requires the full key known in advance (e.g. `chunk_index`); **no range or prefix API** exists (`DiskIndexSnapshot` exposes only `lookup`/`lookup_pointer`/`lookup_pointer_canonical`, `disk_index.rs:2164-2207`) even though redb supports ranges underneath |

**Conclusion: user-implemented channel extraction is NOT possible on the current public API for
the stated workload** (append-in-progress, TB scale). It is possible only on a frozen, ≤ 8 GiB,
`scan_on_open` file via mmap.

**The missing primitives, in dependency order. Each unblocks a named set of cases:**

| # | primitive | unblocks | why nothing else works |
|---|---|---|---|
| P1 | **`read_payload_range(&self, entry, offset, len, &mut [u8]) -> Result<()>`**, backed by `SnapshotFile::read_exact_at` (`snapshot.rs:74`) | C1, C2, C4, C5, C11, C12 | Every existing read materialises the whole payload (`read_payload` `file.rs:411`, `read_logical_payload` `file.rs:444`, `read_payload_snapshot` `file.rs:494-508`). `read_matrix_aux(name, offset, len)` (`file.rs:4328`) is the **only** offset+len read in the public surface and it addresses fixed aux regions, not record payloads. `SnapshotFile` itself is `pub(crate)` (`snapshot.rs:15`) and re-exported nowhere. The only workaround — take `entry.payload_offset`/`payload_len` and open a raw `std::fs::File` — bypasses every snapshot bound, checksum and read limit. |
| P2 | **per-extent CRCs in the payload directory** | makes P1 verifiable | Today `read_payload_snapshot` verifies the record checksum over the *whole* payload buffer; a partial read structurally cannot. Without P2, P1 is an unverified read. `ChunkedBytes` already carries `crc32` per entry (`chunks.rs:56-61`) — reuse it. |
| P3 | **`scanner_at(offset)`** (public wrapper over `read_stream_entry_at`, `file.rs:8057`, currently `pub(crate)` + gated) | C2, C4, C10, C13 | `NativeStreamScanner::from_snapshot` always starts at `header_len` (`file.rs:7987-7988`); without resume, every follower poll and every parallel split is O(N). |
| P4 | **chain traversal reader** for `prev_same_block_offset` | C8, and per-type iteration generally | The pointer is persisted (`format.rs:483-485`, `native_layout.rs:122-158`), the tail is in the sidecar (`disk_index.rs:1187-1192`), 32 B/record is already being paid — and **no code follows it**. Pure read-side addition, zero format change, works on existing files. |
| P5 | **channel-set filter on `StreamingBlocks`** (`stream.rs:620` is a single `!=` today) | C5 | Otherwise `j` channels cost `j` full passes. |
| P6 | **`&self` matrix reads** (replace `seek`+`read_exact` at `matrix.rs:2747-2749` with `read_exact_at`) | C13 for matrix files | Nothing about the read is logically mutating; `matrix_cell_status` beside it is already `&self` (`file.rs:1648`). |
| P7 | **zero-copy `&[T]` slice accessor** (`slice_from_bytes` + extent length) plus **record-start alignment** in the append path | C3 | Both `raw_fixed` (`file.rs:1256-1298`) and `raw_cell` (`file.rs:1409-1436`) return a single `&T`. Alignment reduces to `(record_offset + 32 + extent_offset) % align == 0`, and record-start alignment is not guaranteed today. |
| P8 | **persisted per-channel sample counters** (chunk directory or sidecar `META_TABLE`) | C2, C4, C7 | Append-only has no other way to answer "where is global sample k" or "how many are there" in sub-linear time. |

---

## 2. Cases the design must consciously decline

Each of these is either physically impossible or incompatible with append-only storage. Listing
them is part of the deliverable — a design that stays silent on them will be read as promising them.

| # | claim to refuse | why |
|---|---|---|
| D1 | **"Channel-selective reads cost 1/C the I/O on interleaved payloads."** | False. CH1's bytes are strided with period `C·w`; every page of every datablock contains CH1 bytes. Only *decode* is saved. Applies equally to the existing matrix (`ordinal = scan·dim1 + ch`, `matrix.rs:2146-2164`). |
| D2 | **"Existing interleaved files can be made channel-selective by adding an index."** | False, same reason. The only routes are: write new files planar, or transcode offline (a full rewrite, `O(file)` I/O, one pass). |
| D3 | **O(1) global-sample→offset without a persisted structure.** | Variable-length append-only records admit no closed-form arithmetic, and raggedness (C12) removes even the uniform-block shortcut. |
| D4 | **In-place insertion of a channel into already-written datablocks.** | Append-only. `replace_block` translates every subsequent offset (`file.rs:5815-5818`), i.e. O(bytes after) rewrite per insertion; and whole-file rewrite is *refused outright* for footer specs (`file.rs:3568-3573`), which is exactly the configuration C8 needs. |
| D5 | **Global reordering / "sort the file by channel".** | Requires a full rewrite, and the existing compaction path holds the whole result set in RAM (`file.rs:7085`). Out of scope at TB scale. |
| D6 | **Cross-generation offset stability.** | Offsets are raw absolute u64 and survive a generation change only where explicitly translated (§C14). |
| D7 | **Zero-copy on a file being appended.** | The mmap constructor's precondition (no mutation through *any* handle, thread or process for the mapping's lifetime, `file.rs:1721-1757`) is false by construction. Bounded map up to `committed_eof` only. |
| D8 | **Matrix as the store for an unbounded channel stream.** | Dimensions fixed at create time (`create_layout` `matrix.rs:2190` is create-only; open re-validates, `matrix.rs:2438-2453`), `max_matrix_cells` 16 M (`format.rs:114`), slot region ≤ 8 GiB (`format.rs:118`), matrix data lives *before* `append_log_start` (`matrix.rs:2278-2284`). It is a preallocated grid, not a stream. C11 (changing channel set) alone disqualifies it. |
| D9 | **Live tail-follow of a matrix.** | Commit bitmaps are loaded at open (`matrix.rs:2518`) and `cell_status` never touches disk (`matrix.rs:2774`); no invalidation path exists. |
| D10 | **Tail-follow on the resident `VarveFile`.** | No refresh/reload/follow method exists; reopening is Θ(N) with a 104·N resident floor. |
| D11 | **Per-channel prefix range scan over the sidecar.** | `encode_disk_key` writes the canonical key **little-endian** (`disk_index.rs:1161-1170`) while redb orders rows lexicographically, so `(channel, chunk)` sorts by the *least significant byte* of channel first. And no range/prefix API is exposed at all. |
| D12 | **Sub-chunk visibility latency for followers.** | Sidecar commit happens at chunk boundaries (`stream.rs:1341-1347`) / flush / sync. Follower latency floor = writer flush interval, not record interval. |

---

## 3. The three decisions that determine whether the whole list is servable

1. **L0 vs L1.** Every I/O-reduction case (C1–C5) turns on whether the writer may store channels
   planar. C6 (row read) is the case that would argue for interleaving, and **L1 does not harm it**
   (one pread either way), whereas L2 harms it by `C`× the syscalls. That asymmetry makes L1 the
   dominant layout — but only for files the library writes.
2. **Which substrate.** Only `VarveStreamReader`/`VarveIndexedReader` meet "open reads a header,
   memory bounded by working set" — and both are behind `high-cardinality-dev`
   (`crates/varve-core/Cargo.toml`, `default = []`). The resident `VarveFile` cannot serve any
   TB-scale case (Θ(N) open, 104·N resident, no follow).
3. **Whether offsets become public.** Every user-implemented path needs them; every mutation path
   invalidates them differently (§C14). Exposing them without a generation token is a silent
   correctness hazard.
