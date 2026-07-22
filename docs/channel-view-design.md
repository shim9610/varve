# Channel-selective access in Varve — the option matrix (revision 4)

Target: Varve 0.4.0, branch `codex/dev-next`, repo `C:\git\varve`.
Status: design only. No repository file other than this one was modified in producing this document.

**What changed in revision 3.** Revision 2 ended with a section headed "the decisions only you can make" and
presented three things — planar payload emission, per-extent CRC, enabling `block_offset_chain` — as blocking
choices. That framing was wrong. This library is built on declarative option lists: the format DSL already takes
`index: [scan_on_open, checkpoint_on_flush, block_offset_chain, keyed_offset_chain]`
(`crates/varve/tests/tdms_model.rs:43`), `IndexPolicy` is a struct of four independent bools with named presets and
a `const fn new` (format.rs:481-533), and the macro already varies generated code by those options
(macros/lib.rs:2435, 2521, 3506-3518, 4444, 4669-4678). Channel access is the same shape of problem and gets the
same shape of answer: **a set of options, each independently selectable, each defaulting to off, each with a stated
cost.** The engineering content of revision 2 — physics, layouts, costs, case table, API — is preserved verbatim
except where re-verification proved a number wrong. The decision framing is deleted.

One thing did not become an option, because it is not policy: **an interleaved payload cannot be read
channel-selectively below full block I/O.** That is now stated as the documented consequence of choosing
`channels = interleaved`, not as a question anyone has to answer first. §5.

**What changed in revision 4.** Revision 3's framing survived review. Its *option model* did not, in nine places,
each of which is corrected here rather than defended. In summary:

1. **The `IndexPolicy::new(false, false, true, false)` "escape hatch" is deleted.** `scan_on_open` is consulted
   nowhere in `varve-core` — its only two uses are hash/manifest bytes (format.rs:2397, file.rs:6620) — and every
   `VarveFile::open*` path calls `load_index` unconditionally (file.rs:2798, 2844, 2884, 2935). The escape bought
   no behavioural change and cost a different schema hash and a different manifest byte. The Θ(N) open is the
   unconditional `load_index` call, and that is a separate defect. §2.3, §7.3.
2. **O1/O2 move the schema hash, and revision 3 said they did not.** The field type becomes
   `ChannelPayload<f64>`; `schema_fingerprint` hashes the literal type spelling (`type_identity`,
   macros/lib.rs:752) and the codec `SCHEMA_ID`s, folded in at format.rs:1863, and `field.wire_type` is hashed
   again at format.rs:1874. `ChannelPayload<T>` is now defined, costed and staged (§3.1), and O1/O2 are in §4's
   hash-moving group.
3. **"All options off ⇒ byte-identical" is now an implementation obligation with two named rules**, not a claim
   argued from codegen alone. §4.1.
4. **Three couplings were missing**: `channels` × compression, O5 × sidecar state store, and O4 removing
   `replace_rewrite`. §1.2 now lists seven.
5. **The recommended sets are declarable**, as `channels: selective | decode` preset tokens, mirroring the
   existing format-level `preset:` (macros/lib.rs:1315-1327). §1.3.
6. **The `VarveChannelBlock` trait is shown**, with both generated impls side by side. §3.1.
7. **§10.1 no longer calls `len()`** under a set that does not offer it, and C1's complexity column is restated
   for the O2-only case.
8. **`open_native` is `pub(crate)`** (stream.rs:463) and `events()` is feature-gated — the O0 row said otherwise.
9. **Four numbers were wrong**: 5 × 15 = 75 (not 80), the CRC cost, the blocks-per-TiB figure, and the
   "0 allocations" on O5's per-commit path. All corrected and propagated.

---

## Citation basis — read this before trusting a line number

A separate defect-fix workflow is editing `crates/` while this document is being written. `crates/varve-core/src/`
is dirty in the working tree; `file.rs` line numbers have moved by roughly **+300** since revision 2's verification
pass. Therefore:

- **The symbol name is the durable anchor. The line number is a convenience.**
- Citations re-verified against the current working tree in this revision carry their **current** line number and
  appear in the re-verification table below.
- Citations carried from revision 2 that were **not** re-verified in this revision are marked **‡**. Their symbol
  is correct; their line number is revision 2's and is probably stale.

### Re-verification table (revision 2 → current working tree)

| Symbol | Rev-2 cite | Current |
|---|---|---|
| `read_payload_snapshot` | file.rs:494-508 | **file.rs:816** |
| `read_payload` / `read_logical_payload` | file.rs:411 / 445 | **file.rs:733 / 767** |
| `RecordIndexEntry` | file.rs:385-401 | **file.rs:706** |
| `clone_matching_entries` | file.rs:9063-9084 | **file.rs:9418** |
| `VarveFile::blocks<T>` | file.rs:3848-3853 | **file.rs:4178** |
| `index_bytes_for_count` | file.rs:8966-8979 | **file.rs:9321** |
| `load_index` / `scan_records_from` | file.rs:8383-8395 / 8665-8771 | **file.rs:8738 / 9020** |
| `translate_record_offset` | file.rs:596-612 | **file.rs:919** |
| `BlockEvent` | file.rs:565-572 | **file.rs:887** |
| `prepare_stream_creation_nonce_record` | file.rs:8219-8237 | **file.rs:8574** |
| `PreparedStreamRecord` / `prepare_stream_user_record` | file.rs:8105-8113 | **file.rs:8462-8469 / 8472** |
| `read_stream_entry_at` | file.rs:8057 | **file.rs:8412** |
| `NativeStreamScanner` / `::from_snapshot` | file.rs:7984-7992 | **file.rs:8297 / 8332** |
| `nonzero_offset` | file.rs:8571 | **file.rs:8145** |
| `verify_snapshot_record` | file.rs:8885-8916 | **file.rs:9240** |
| `BlockTails` / `BlockTails::tail` | file.rs:800-804 / 836-841 | **file.rs:1122 / 1158** |
| `read_payload_file_validated` | file.rs:8779-8784 | **file.rs:9138** |
| `validate_record_entry` | file.rs:6631-6640 | **file.rs:6981** |
| `replace_fixed` / `replace_rewrite` / `replace_block` | file.rs:3540 / 3567 / 3256 | **file.rs:3653 / 3898 / 3492** |
| `rewrite_record_streaming` | file.rs:5739 | **file.rs:6031** |
| `compact` / `collect_merged_keyed_values` | file.rs:7040-7072 / 7085 | **file.rs:7395 / 7429** |
| `replace_path_atomically` | file.rs:3667 | **file.rs:10207** |
| `keyed_blocks` | file.rs:3888-3963 | **file.rs:4219** |
| `raw_fixed` / `raw_cell` | file.rs:1256-1298 / 1409-1436 | **file.rs:1586 / 1731** |
| `read_matrix_cell` | file.rs:1632 / 2153 / 4313 | **file.rs:1958 / 2479 / 4649** |
| `matrix_cell_payload` / `matrix_cell_status` | file.rs:4318 / 4356 | **file.rs:4654 / 4692** |
| `is_/set_matrix_channel_committed` | file.rs:4440, 1659, 2212 | **file.rs:1985, 2538, 2542, 4786, 4791** |
| `mmap_payloads` / `payload_window` | file.rs:1721-1757 / 1219 | **file.rs:2061, 5107 / 1541** |
| `read_matrix_aux` | file.rs:4328 | **file.rs:1970, 2491, 4664** |
| `payload_offset = record_offset + 32` | file.rs:1472-1478 | **file.rs:5620** |
| reserved ids / `RECORD_HEADER_LEN` / `_FOOTER_LEN` | file.rs:32-42 | **unchanged: file.rs:32-42** |
| `events()` / `StreamEvents` | stream.rs:516-522 / 563-581 | **stream.rs:521 / 563** |
| `open` / `open_native` (**both `pub(crate)`**) / frontier pin | stream.rs:439 / 458 / 450 | **stream.rs:444 / 463 / `pin_logical_len` 483** |
| `StreamingBlocks` skip | stream.rs:523, 620-622 | **stream.rs:528, 626** |
| `push_info` / `push_with_prev_key_info` | stream.rs:943 / 983 | **stream.rs:954 / 994** |
| `append_prepared` / `append_prepared_chunk` | stream.rs:1218 / 1233 | **stream.rs:1229 / 1248** (plus a new `append_prepared_chunk_summarized` at 1202) |
| `push_iter_inner` | stream.rs:1121-1122 | **stream.rs:1103** |
| `stage_state_records` + its O(n²) scan | stream.rs:1388-1395 | **stream.rs:1393, sidecar guard at 1394-1396, `chain` flag at 1397, scan at 1399-1409** |
| `commit_state_chunk` | — | **stream.rs:1433** (doc comment from 1425; takes `&StreamMutationPermit`) |
| `append_prepared_chunk` permit | — | **consumes `StreamMutationPermit` by value, stream.rs:1248-1253** |
| manifest `index_policy_byte` push / decode / `index_policy_from_byte` | — | **file.rs:6335 / 6387 / 6629-6641 (byte 0 rejected)** |
| `IndexPolicy` struct / `keyed_offset_chain` guard / block hash loop | format.rs:481-487 / 1905-1909 / 1843-1876 | **format.rs:482-488 / 1907 / 1845-1877** |
| layout hashed only when non-default | — | **format.rs:1841-1844** |
| `schema_fingerprint` `type_identity` | — | **macros/lib.rs:734-802, `type_identity` at 752** |
| `flush` / `sync` | stream.rs:1040 / 1047 | **stream.rs:1051 / 1058** |
| `MatrixKey` / ordinal | matrix.rs:834-838 / 2146-2164 | **matrix.rs:837 / 2447** |
| `per_channel_count` / `read_cell` / `cell_status` / `create_layout` | matrix.rs:5039 / 2747 / 2774 / 2190 | **matrix.rs:5329 / 3013 / 3053 / 2468** |
| `load_commit_bitmaps` / `load_crc_valid_bits` | matrix.rs:2518 / 2527 | **matrix.rs:6161 / 6237** |
| `SnapshotFile` / `with_len` / `try_clone_file` / `read_exact_at` / `read_at` | snapshot.rs:15-18 / 42 / 69 / 74 / 253 | **unchanged: 15-18 / 42 / 70 / 74 / 252-264** |
| `IndexPolicy` and everything in format.rs | as cited | **unchanged** |

Everything not in this table is marked ‡ in the body.

---

## 0. Two-minute summary — your question, in your terms

**Your file.** `datablock[CH1,CH2,CH3,CH4,CH1,…,CH4] metablock[…] datablock[…] datablock[…] metablock[…] …`,
forever. One datablock is one record. Its payload holds many interleaved samples of all four channels. Metablocks
are separate records interspersed at irregular intervals. The stream never ends.

**Can you read only CH1 across the whole file?**

**Yes. Turn on one option on the datablock, and it costs you this:**

```
variable DataBlock(id = 1, channels = planar) {
    samples: ChannelPayload<f64>,
}
```

- **What you get:** a CH1-only full scan reads **256 GiB of a 1 TiB file instead of 1 TiB** — one positional read
  per datablock instead of a whole-block read. `ChannelView::extents()` + `read_extent_into` is the scan path.
- **What it costs on the write side:** per sample, **nothing** — the same one address computation and one store
  you pay today, 0 allocations, 0 syscalls; the writer keeps 4 open cursors instead of 1. Per datablock, a
  **160 B** channel extent table written into the buffer the writer already owns — 0 extra syscalls, 0 extra
  allocations. On disk, **+0.12 %**.
- **What it costs on the compatibility side:** the field type changes from `Vec<f64>` to `ChannelPayload<f64>`,
  which **moves `computed_schema_hash`** (macros/lib.rs:752 → format.rs:1863). A spec pinned with
  `schema_hash: computed;` will not open a file written before the change. New file, or an unpinned hash. §4.
- **What it does not give you:** cheap random 100-sample peeks (that needs the seek directory, and even then a
  cold one costs ~336 KiB of device traffic for 800 B — §5.2); it does not compose with `compression:` on the
  same block (§1.2 coupling 5); and it does not make an *already written* interleaved file selective. Nothing can.

**If you want the savings but cannot change how samples land in the buffer**, turn on
`channels = interleaved` instead. You get the **decode** saving (~4× CPU, you skip decoding CH2–CH4) and **zero**
I/O saving: CH1's bytes recur every 32 bytes, a 4 KiB page holds 128 of those recurrences, so every page of every
datablock is a page CH1 needs. **You read 100 % of the file to get 25 % of it.** That is physics, not a
limitation of the index, and §5 refuses to soften it. It is the price of that option, stated up front.

**If you turn nothing on, nothing changes.** Every option in this document defaults to off. With all of them off
the format is **byte-identical to today**, the schema hash is unmoved, no generated method appears or disappears,
and the append path executes exactly the instructions it executes now. A user who does not opt in pays nothing —
not one byte, not one instruction, not one API change. That is not self-evident from "off ⇒ no codegen": it also
requires two rules inside `varve-core` that §4.1 states as implementation obligations, because
`computed_schema_hash` and the embedded manifest payload are both written as fixed unconditional sequences and
would otherwise move for every existing file.

**The full option set is §1.** Each row is a capability, the option that buys it, where you declare it, what the
generated code does differently, what it costs per record and per block and on disk, and what it explicitly does
not give you. Pick the rows you want.

---

## 1. THE OPTION MATRIX

Ground rules for every row:

- **Default is off.** The "nothing enabled" row is the current behaviour and is what you get by writing nothing.
- **Costs are per record and per block, derived, at C = 4 channels of `f64`, S = 4096 samples/channel/block
  (128 KiB payload), 1 TiB file ⇒ 2⁴⁰ / 131072 = 8 388 608 datablocks exactly.** That one figure is used
  everywhere in this document: it is what makes the 262 144 directory groups at D = 32 and the 2.9 MiB skip-node
  cache of §9.5 consistent. Revision 3 mixed 8.19 × 10⁶ with the 2²³ figure; the 2²³ figure is the correct one and
  the units are binary throughout (1 TiB, 256 GiB), never decimal TB.
- **Every cost column is also restated at C = 64** in §14.2.1, because the design's own recommendation changes
  shape there: the CET grows to 2 080 B (1.6 % of a 128 KiB block, 13× the headline), the BDR entry to 528 B, and
  the writer holds 64 concurrent write cursors.
- **Rows are independently selectable** unless the "Requires" line says otherwise, and where two options are
  coupled the coupling is a verified fact about the code, cited.
- Option names marked **(exists)** are current DSL syntax. All others are proposed additions to the existing
  grammar, following the precedent of `key_index = memory | disk` (macros/lib.rs:1892-1905), which is a per-block
  attribute that defaults to the inert value, is feature-gated, changes generated code, and moves no hash.

### 1.1 The matrix

| # | Capability you want | Option(s) to enable | Declared where | What the generated code does differently | Per-record / per-block cost | On-disk cost | What it does NOT give |
|---|---|---|---|---|---|---|---|
| **O0** | **Nothing — today's behaviour** | *(none)* | — | Nothing. No CET encoder, no `ChannelSet`, no directory records, no footer. All channel code paths are absent from the binary. | **Zero.** Same instructions, same syscalls, same allocations as 0.3.0 | **0 B** | No channel addressing at all. You can still: read whole payloads (`read_payload`, file.rs:733); enumerate every record header-only via `events()` (stream.rs:521, no payload read) **on a build with `feature = "high-cardinality-dev"`, which is not in `default = []`** (varve/Cargo.toml:29-34) — the same gate §11 flags for `StreamingBlocks`, and it applies to `events()` too; and de-interleave yourself at write time into one record per (channel, chunk) — the TDMS-model convention, which works today (tdms_model.rs:46-81‡). **What you cannot do today: open header-only.** `open_native` (stream.rs:463) and `open` (stream.rs:444) are both `pub(crate)`; the public constructors go through the wrappers in `varve`. Revision 3 sold `open_native` as user-reachable; it is not |
| **O1** | **Channel-aware decoding, payload stays interleaved** | `channels = interleaved` on the block, **and the payload field's type becomes `ChannelPayload<T>`** (§3.1) | Per-block attribute on the datablock | Writer back-patches a **CET** (channel extent table, §8.2) at payload offset 0 with one strided entry per channel. Reader gains `ChannelSet` / `ChannelView`; `read_extent_into` decodes one channel's lane with stride `byte_len / sample_count`. Trait `VarveChannelBlock` is implemented for the block with `LAYOUT = Interleaved` (§3.1 shows the trait and both impls) | **Per sample: 0.** Per block: `32 + 32·C` = **160 B** into the buffer the writer already owns — 0 syscalls, 0 allocations, no reallocation (back-patch in place, **given the reservation rule of §8.6**) | **160 B/block = +0.12 %** | **No I/O saving whatsoever.** A CH1-only scan still reads **1 TiB of a 1 TiB file**. Savings are CPU only: ~4× on decode at C = 4. No zero-copy slice — no contiguous `&[f64]` exists in an interleaved payload. **Moves the schema hash** (§4): `ChannelPayload<f64>` is a different type spelling and a different codec `SCHEMA_ID` from `Vec<f64>`. **Incompatible with `compression:` on the same block** (§1.2 coupling 5) |
| **O2** | **Channel-selective I/O** | `channels = planar` on the block, **and the payload field's type becomes `ChannelPayload<T>`** | Per-block attribute on the datablock | Same CET, but each entry points at a **contiguous** per-channel extent. Writer generates a scatter helper (C write cursors, or a tiled transpose above roughly a dozen channels, §8.6). Reader's `read_extent_into` becomes **one pread of exactly that channel's bytes**. `LAYOUT = Planar` | **Per sample: 0** — one address computation, one store, identical instruction count to interleaving; the writer holds C concurrent store streams instead of 1. The binding resource is **store-buffer and write-combining-buffer occupancy (typically ~10–12 fill buffers) and DTLB reach**, not L1 capacity: free at C = 4, fine at C ≈ 8, and above roughly a dozen concurrent streams the streaming stores begin to thrash write-combining, which is why the tiled transpose exists (§8.6). **If your hardware DMAs an interleaved buffer you cannot influence:** +1 load/store pair per sample, ~1–2 ns, a real added pass, still 0 alloc / 0 syscall. **If channel rates differ:** one concatenating memcpy at block close, +8 B moved/sample, ~1 % of a core at 32 MB/s. Per block: 160 B CET, 0 syscalls, 0 allocations | **160 B/block = +0.12 %** at C = 4; **2 080 B = +1.6 %** at C = 64 (§14.2.1) | **No seek.** Reaching the k-th datablock is still a forward walk without O5, and `ChannelView::len()` is **not offered** without O5. **No cheap small window** — below ~128 samples/block at C = 4 (the §5.2 crossover) planar and interleaved cost the same one page. **Does not retrofit** onto already-written interleaved data. **Moves the schema hash. Incompatible with `compression:` on the same block** |
| **O3** | **Verify a partial payload read without the whole-record checksum** | `extent_integrity = crc32` | Per-block attribute. **Requires O1 or O2** — genuinely coupled: the CRC field lives in a CET entry, and with `channels = off` there are no extents to checksum | Writer computes one CRC32 per extent and sets CET header `flags` bit0. Reader's `read_extent_into` verifies before returning; with the option off that branch is a `const EXTENT_CRC: bool = false` and is compiled out entirely | Per block: **C CRC32 passes over payload bytes already hot in cache** — the C passes together cover the payload once, so ~1 B/cycle with hardware CRC ⇒ ~131 K cycles per 128 KiB block. At a 32 MB/s ingest rate that is 32 × 10⁶ cycles/s, i.e. **~1 % of one 3 GHz core** (revision 3 said ~4 %; that was ~4× too high and made the only integrity story for partial reads look worse than it is). At 320 MB/s it is ~10 %. 0 syscalls, 0 allocations | **0 B** — the `crc32 u32` field is already inside the 32 B CET entry O1/O2 pay for | Does not replace the whole-record checksum for whole-payload reads — those keep `verify_snapshot_record` (file.rs:9240) unchanged. Meaningless for `RECORD_FLAG_COMPRESSED` records (file.rs:43): a partial read of a compressed payload has no meaning, and the API must reject it |
| **O4** | **Fast per-type enumeration (datablock-only or metablock-only)** | Baseline needs **nothing**: `events()` already ships. For M-proportional cost: `index: [block_offset_chain]` **(exists)** | Format-level `index:` clause | Core already writes `prev_same_block_offset` into every record footer (native_layout.rs:122-158‡, `nonzero_offset` file.rs:8145) and `BlockTails` maintains it (file.rs:1122, 1158). What is **new is read-side only**: a traversal that follows the chain. Nothing follows it today | Per record: a **32 B footer** on **every record of every block type**, not only chained ones (`spec_needs_record_footer`, format.rs:1992-1994) + one `set_tail`, O(log B) (file.rs:1158). Per commit: the sidecar tail write in `stage_state_records` (stream.rs:1393-1414) | **32 B/record = +0.024 %** at 128 KiB blocks | **Three things bite.** (a) It **moves `computed_schema_hash`** (`index_policy_hash_byte`, format.rs:2396-2405, hashed at 1809) — §4. (b) It **removes whole-file replace for the format**: `replace_rewrite` is refused outright for footer specs (file.rs:3898), and O4 forces a footer. That is a capability you lose by enabling an option, and it belongs here rather than buried in §13.6. (c) Through the DSL, `block_offset_chain` also sets `scan_on_open` (macros/lib.rs:928-932, mirroring format.rs:546-551 and the `BlockOffsetChain` preset at 505-511) — but **that flag is inert**, so it costs you a hash byte and a manifest byte and nothing else. It does **not** cause the Θ(N) open; the unconditional `load_index` call does (§2.3, §7.3). Do not work around it |
| **O5** | **Random access by global sample ordinal (seek)** | `directory: seek(commit)` | **Format-level clause**, parallel to `commit:` / `manifest:` / `compression:`, and taking a bare ident in parentheses exactly as `transaction_marker(on_flush)` does (macros/lib.rs:1505-1540). **Requires O2, O4, and an attached sidecar state store** | Writer emits two reserved-internal records per directory group — **BDR** (block directory) and **CHK** (chunk summary with a base-16 skip list) — at the sidecar commit boundary (§8.3). Reader gains `ChannelView::read_into(start, ..)`, `ChannelSet::datablock(ordinal)`, and an O(1) `ChannelView::len()` | **Per record: 0 extra syscalls, 0 extra allocations.** Per **commit**: build BDR+CHK in one O(D) pass over descriptors already recorded (O(1) amortised/record), **+1 `seek` + 1 `write_all` = +2 syscalls**, **+1 heap allocation** — `append_prepared_chunk` takes `bytes: &[u8]` borrowed (stream.rs:1248-1253), so the directory bytes must be materialised into their own buffer; that is 1 alloc per commit = 1/D per record (0.03 at D = 32), and revision 3's table said 0 — +2 `set_tail`, +2 `advance_coverage_with_tail` (O(1)‡). Syscall overhead by commit cadence D: **D = 1 → +200 %; D = 8 → +25 %; D = 32 → +6.3 %; D = 256 → +0.8 %** (§8.7). Resident writer state: 40 B of skip pointers per chained block id + `(16+8C)·D` reused descriptor bytes (~1.5 KiB at D = 32) | BDR per-entry `16 + 8C` = **48 B/block**; BDR header + CHK (`32 + 168`) ÷ D = **6.25 B/block at D = 32**, 200 B at D = 1. **Total with O2+O4: ~246 B/block = 0.19 % at D = 32; ~440 B = 0.34 % at D = 1** | **Not cheap for a cold small window.** A cold 100-sample read costs ~84 positional reads ⇒ **~336 KiB of device traffic for 800 B, ~430× amplification, all of it directory** (§5.2). Warm (skip-node cache, 2.9 MiB at 1 TiB) it is ~24 reads / ~96 KiB. **Below D = 8 it is not worth its syscalls** — use O4's chain and accept O(groups) forward seek. **Silently does nothing on a sidecar-less stream**: the chain head is a `DiskIndexTail` written by `stage_state_records`, which returns immediately when `self.state` is `None` (stream.rs:1394-1396), so such a stream has neither an emission boundary nor an O(1) entry point. The DSL must reject the combination rather than degrade |
| **O6** | **Tail-follow for a live reader** | **No format option at all.** Reader-side: `feature = "high-cardinality-dev"` + the `scanner_at(offset)` primitive (P3, §11) | Per-reader, at construction; nothing on disk | `VarveStreamReader::refresh(&mut self)` re-reads the sidecar frontier and calls `snapshot.with_len` (snapshot.rs:42) — the same line already executed once at stream.rs:483. `ChannelCursor` is a plain value carrying a `GenerationWitness` | Per poll: **O(1)** + the new data. Writer: **0** | **0 B** | **Latency floor is the writer's sidecar commit cadence** — chunk boundary, `flush` (stream.rs:1051), `sync` (stream.rs:1058) — **not** record cadence. Sub-chunk follower latency is not offered. **P3 is not thin**: `read_stream_entry_at` (file.rs:8412) does `try_clone_file` + seek and `NativeStreamScanner` (file.rs:8297, 8332) holds its own seeking handle — both banned by the Windows cursor rule (§7.8) and both must be ported onto `read_exact_at` first. Not available on resident `VarveFile` (no refresh; re-opening is Θ(N)) or on the matrix |
| **O7** | **Zero-copy channel slices for fixed-width samples** | `record_align: 8` **plus** features `mmap` + `zero-copy`. **Requires O2** | `record_align:` is a **format-level** clause (it is a framing property of every record); the features are crate-level | Generates `ChannelView::extent_slice<S: RawSample>(&self, map: &SealedMap, e: &Extent) -> Result<&[S]>`. Needs a `slice_from_bytes`-style accessor — both existing raw accessors return a single `&T` (`raw_fixed` file.rs:1586, `raw_cell` file.rs:1731) | Per record: **≤ 7 B of inter-record padding** at 8 B alignment. 0 syscalls, 0 allocations. Page alignment instead: ≤ 4095 B | **≤ 0.005 %** at 8 B on 128 KiB blocks; **≤ 3 %** at 4 KiB page alignment | **Not available on an actively appending file.** `mmap_payloads`' unsafe precondition (file.rs:2061, 5107) requires the caller to prevent mutation, truncation and replacement through *every handle, thread and process* for the mapping's lifetime — **false by construction for a growing stream**. Only over a `SealedMap`. Also needs a copying sibling for cross-endian files (the `RAW_ENDIAN` check errors rather than byte-swapping‡). **Changes framing ⇒ moves the hash** (§4) |
| **O8** | **Metablock ↔ datablock correlation (epochs)** | `channel_epoch = on` | Per-block attribute. **Requires O1 or O2** (the epoch lives in the CET header) | Writer increments a `u32` epoch whenever a metablock is emitted and stamps it into the CET header. `Extent::epoch` is exposed. With O5 also on, the CHK carries a trailing `epoch_starts: [(epoch u32, meta_back_delta u64)]` | **0 syscalls, 0 allocations.** The `epoch u32` is already a field of the fixed 32 B CET header O1/O2 pay for. With O5: **+12 B per metablock** | **0 B** beyond O1/O2; +12 B/metablock with O5 | **datablock → epoch is O(1); epoch → the governing metablock is not.** Without the CHK epoch column it is an O(M) chain walk; with it, O(log). Revision 2's claim of "O(1) both directions" was false and is corrected (§13.1) |
| **O9** | **Block-ordinal / scan-ordinal ranges** | `scan_axis = block_scan_base` | Per-block attribute. **Requires O1 or O2** | Writer populates the CET header's `block_scan_base u64` with a monotone block-level scan ordinal; reader exposes ordinal-range queries. Off ⇒ the field is written as 0 and range-by-ordinal methods are not generated | **0 syscalls, 0 allocations, 0 extra bytes** — the `u64` is already in the fixed CET header | **0 B** | **Not time.** No record carries a timestamp (`RecordIndexEntry`, file.rs:706). True time ranges need an added `start_time` (8 B/block, 0 syscalls) which is a further format addition |

### 1.2 The couplings, stated exactly

Seven exist. Revision 3 listed four and missed three; the three it missed are 5, 6 and 7, and each is a
combination a reader will plausibly construct. Everything else in the matrix is independently selectable.

1. **O3, O8, O9 require O1 or O2.** They are fields of the CET header/entry. With `channels = off` there is no
   CET, so there is nothing to put them in. This is definitional, not a policy choice.
2. **O5 requires O2.** A seek directory over an interleaved payload buys you the *ordinal → block* map and
   nothing else, because the block read is still a whole-block read (§5). It would be a directory whose payoff
   the layout throws away.
3. **O5 requires O4.** Verified, not assumed: the CHK chain head is persisted as `DiskIndexTail` in the sidecar
   `TAILS_TABLE`‡, and `stage_state_records` only writes a tail when `self.spec.index_policy.block_offset_chain`
   is true (stream.rs:1397, tail construction at 1406-1414). Without the chain, `ChannelSet` construction has no
   O(1) entry point to the CHK list, and the §8.3.2 fallback for a `replace_block`-straddling hop — which walks
   `prev_same_block_offset` — does not exist.
4. **O7 requires O2.** There is no contiguous `&[S]` inside an interleaved payload to hand out.
5. **O1 and O2 are incompatible with compression on the same block** — format-level `compression:` or a
   per-block `BlockCompressionDescriptor` (format.rs:673, hashed at format.rs:1815-1820). The whole design rests
   on the CET being readable by one positional read at payload offset 0. In a compressed record that byte range
   is compressed output: you must inflate the entire payload to see the CET, which is precisely the I/O the
   design exists to avoid, and every O1/O2/O5 read path collapses to a whole-payload read. §3.2 already rejects
   `RECORD_FLAG_COMPRESSED` on the partial-read path (file.rs:43); this lifts that from a runtime error to a
   spec-validation error, because `channels = planar` + `compression: zstd` is a combination that should not
   compile. Compression of the *extents themselves* is expressible — the CET entry already carries an
   `encoding u16` (§8.2) — and is not proposed here.
6. **O5 requires an attached sidecar state store, not merely O4.** `stage_state_records` opens with
   `let Some(state) = self.state.as_mut() else { return Ok(()) };` (stream.rs:1394-1396). A stream with no state
   store never stages a `DiskIndexTail` and never commits a chunk, so O5 would have no emission boundary and no
   O(1) entry point — and would fail silently rather than loudly. Spec validation must reject it.
7. **O4 removes `replace_rewrite` from the format.** Not a prerequisite but a *loss*: whole-file replace is
   refused outright for footer specs (file.rs:3898) and O4 forces a footer (format.rs:1992-1994). Enabling O4
   therefore takes a capability away, which is why it appears in O4's "does NOT give" column (§13.6 has the
   detail).

Each of 1–7 gets a compile-fail fixture in the style the crate already uses (varve/tests/compile.rs:54, 63-72),
so the coupling is enforced by the compiler and not by this document.

And one coupling the library already enforces that is worth knowing about, because it is the same pattern:
`keyed_offset_chain` requires `block_offset_chain`, checked at spec validation with an explicit error
(format.rs:1907). Custom/none layout presets reject `checkpoint_on_flush`, `block_offset_chain` and
`keyed_offset_chain` outright (format.rs:2022-2029) — so **none of O4, O5 is available under a custom layout.**

### 1.3 Recommended option sets (pick one; each is a complete, shippable configuration)

These are not rankings. They are named points in the matrix so a user can adopt one line instead of eight.

| Set | Options | What it buys | What it does NOT offer | Total on-disk cost | Total syscall cost |
|---|---|---|---|---|---|
| **`off`** | none | today | channel addressing | 0 | 0 |
| **`decode`** | O1 + O3 | ~4× CPU on single-channel decode; verifiable partial reads | any I/O saving; `len()`; seek | +0.12 % | 0 |
| **`selective`** | O2 + O3 + O8 | 1/C **I/O** on channel scans; verifiable partial reads; epoch correlation | **`ChannelView::len()`** (O5 only) and random access by ordinal. Scans run to the frontier via `extents_from`, not `extents(0..len())` — §10.1 | +0.12 % | 0 |
| **`selective+seek`** | O2 + O3 + O8 + O9 + O4 + O5 | all of the above, plus O(log) seek by sample ordinal, O(1) `len()`, M-proportional metablock enumeration | in-place upgrade: it moves the hash twice over (field type and index policy) | +0.19 % at D = 32 | +6.3 % at D = 32 |
| **`archive`** | `selective` + O7 + `record_align: 8` | zero-copy `&[f64]` over a sealed, finished file | anything on an actively appending file | +0.13 % | 0 |

**These sets are declarable, not just describable.** The owner asked for "a final option set the user can enable",
and prose that expands to six hand-written tokens is not that. The library already has the precedent: the
format-level `preset: varve_native | none | custom` clause (macros/lib.rs:1315-1327) expands one ident into a
whole `LayoutSpec`. `channels` takes the same shape — the attribute value is either a **layout ident**
(`interleaved | planar`) or a **set ident** (`decode | selective`) that expands to a fixed option combination:

```rust
// These two block declarations are exactly equivalent.
variable DataBlock(id = 1, channels = selective) { samples: ChannelPayload<f64> }

variable DataBlock(
    id = 1,
    channels = planar,
    extent_integrity = crc32,
    channel_epoch = on,
) { samples: ChannelPayload<f64> }
```

`selective+seek` additionally needs the two format-level clauses, which cannot be folded into a per-block ident
because they are framing (§2.1); the complete working declaration is §2.2. `archive` is `selective` plus
`record_align: 8` plus two crate features, and likewise cannot be one token.

The expansion is one match arm in `parse_inline_block` (macros/lib.rs:1871-1928) and one in the validation block
at 1929-1975, and it is deliberately **not** open-ended: exactly two set idents exist, so the expansion table is
readable in full and a user is never surprised by what a preset turned on.

---

## 2. HOW THE OPTIONS ARE DECLARED

### 2.1 The grammar these options live in

`varve_format!` already accepts, at **format level**: `magic:`, `version:`, `limits { … }` or
`limits: trusted_unbounded`, `endian:`, `schema_hash:`, `extension:`, `index:`, `commit:`, `integrity:`,
`recovery:`, `manifest:`, `compression:`, `preset:`, `dims { … }`, `aux { … }`, `blocks { … }` or `blocks: [ … ]`,
`layout { … }` (macros/lib.rs:1159-1230, 1272-1330). `index:` takes **either a single ident or a bracketed list**
(`parse_index_choice`, macros/lib.rs:1454-1483; idents at 1485-1502).

And at **per-block level**, inside the parentheses after the block name: `id`, `version`, `key`, `key_index`,
`dims`, `category` (macros/lib.rs:1875-1921).

**The placement rule this design follows:** a property of the *file's framing or index* is a format-level clause;
a property of *one block's payload* is a per-block attribute. That is why:

- `channels`, `extent_integrity`, `channel_epoch`, `scan_axis` are **per-block**. They describe the bytes inside
  one block type's payload. A format can carry a planar datablock and an ordinary metablock side by side, and
  must be able to.
- `index:` (O4) is **format-level** because it decides whether *every record of every type* carries a 32 B footer
  (`spec_needs_record_footer`, format.rs:1992-1994). It cannot be per-block; the footer is a framing property.
- `directory:` (O5) is **format-level** because BDR/CHK are file-scoped records emitted on the writer's commit
  path, and their cadence is the file's commit cadence.
- `record_align:` (O7) is **format-level** because inter-record padding is framing.

`key_index = memory | disk` is the exact precedent for the per-block attributes: it defaults to the inert value
(`KeyIndexChoice::Memory`, macros/lib.rs:1869, asserted by the macro's own test at 6120-6127), it is rejected on
blocks where it makes no sense (unkeyed: 1962-1968; matrix: 1930-1935), it is feature-gated (1969-1974), and it
changes generated code substantially (macros/lib.rs:4444, 4674-4678, 4736) while **moving no hash** — `key_index`
appears nowhere in `varve-core` except two error strings (indexed.rs:267, 1198).

**One way the precedent does not carry over, and it matters.** `key_index` is hash-neutral because it changes
*which methods are generated* and nothing about the bytes on disk. `channels` changes the bytes, so it cannot be
hash-neutral and should not try to be: the attribute itself is invisible to the fingerprint, but the field type
it requires (`ChannelPayload<T>`, §3.1) is not. Revision 3 took the precedent one step too far and concluded
O1/O2 were hash-neutral; §4 corrects that. The precedent that *does* carry over in full is the shape of the
attribute — inert default, rejected where meaningless, generated code varies — not its hash behaviour.

### 2.2 Your exact stream, with the options on

```rust
varve_format! {
    pub format AcqFormat {
        magic: b"VACQ";
        version: 1;
        limits { /* … as today … */ }
        endian: little;
        schema_hash: computed;
        extension: "vacq";

        // ---- O4: the per-block backward chain. Format-level, because the
        // ---- 32 B record footer it forces is charged to EVERY record of
        // ---- EVERY block type (format.rs:1992-1994). This also removes
        // ---- replace_rewrite for the whole format (file.rs:3898).
        // ---- Note: this sets scan_on_open too (macros/lib.rs:928-932).
        // ---- That flag is inert in varve-core today; see 2.3.
        index: [block_offset_chain];

        commit: transaction_marker(on_flush);
        manifest: embedded;
        integrity: crc32;

        // ---- O5: the seek directory. Format-level: file-scoped records on
        // ---- the writer's commit path. The bare `commit` ident ties
        // ---- directory cadence to visibility cadence (see 8.3.3), and
        // ---- follows transaction_marker(on_flush)'s bare-ident form
        // ---- (macros/lib.rs:1505-1540). No existing clause takes
        // ---- `name = value` inside parentheses, so this design does not
        // ---- either; revision 3's `group = commit` claimed a precedent
        // ---- that does not exist.
        directory: seek(commit);

        // ---- O7 (optional): framing property, so format-level.
        record_align: 8;

        blocks {
            // ---- O2 + O3 + O8 + O9, all per-block, all on ONE block.
            variable DataBlock(
                id = 1,
                channels = planar,
                extent_integrity = crc32,
                channel_epoch = on,
                scan_axis = block_scan_base,
            ) {
                // ChannelPayload<T> is the codec that encodes CET + extents.
                // It is a NEW type spelling and a NEW codec SCHEMA_ID, so it
                // moves computed_schema_hash (macros/lib.rs:752 ->
                // format.rs:1863, and field.wire_type at 1874). See 3.1, 4.
                samples: ChannelPayload<f64>,
            }

            // ---- The metablock declares NOTHING new. It is an ordinary
            // ---- variable block and its codegen is untouched.
            variable MetaBlock(id = 2, key = [name]) {
                name: String,
                value: String,
            }
        }
    }
}
```

The same format with every new option omitted is the format you would write today, and produces the file you
would write today.

The `decode`-only variant differs by exactly one token:

```rust
variable DataBlock(id = 1, channels = interleaved, extent_integrity = crc32) { … }
```

### 2.3 `scan_on_open` — what revision 3 got wrong, and what is actually true

Revision 3 told the reader to bypass the DSL and build the spec in Rust as
`IndexPolicy::new(false, false, true, false)` to get "the chain without the Θ(N) open", and repeated that
instruction in O4's row, in §7.3, and in §17's first migration procedure. **That instruction is deleted. It bought
nothing and cost two real things.** The correction, verified:

- **`scan_on_open` is consulted nowhere in `varve-core`.** Its only two uses outside `format.rs`'s own
  constructors are as a bit in a hash byte (`index_policy_hash_byte`, format.rs:2397) and as a bit in the embedded
  manifest (`index_policy_byte`, file.rs:6620). Grep confirms there is no third.
- **Every `VarveFile::open*` path calls `load_index` unconditionally** — file.rs:2798, 2844, 2884, 2935 — with no
  reference to the flag. The stream path never scans, also regardless of the flag.
- Therefore the Rust-built spec **behaves identically** to `index: [block_offset_chain]` while producing a
  **different `computed_schema_hash`** (format.rs:1809) and a **different byte in the embedded manifest**
  (pushed at file.rs:6335, decoded at 6387). Files written from the two specs are mutually unopenable, for no
  gain. Revision 3's claim that it "costs nothing but being set early" was false.

**The Θ(N) open is the unconditional `load_index` call, not the flag.** Making the resident open path honour
`scan_on_open` is a real defect fix with real value (§7.3), it is independent of every option in §1, and **nothing
in this design depends on it**: the whole read side lives on the stream path, which never scans at open (§9.1).
It is listed here so that the two are never conflated again — it is a dependency of nothing.

One genuine grammar limitation remains, and it is smaller than revision 3 made it: **there is no way to write
`block_offset_chain` without also setting `scan_on_open` in the recorded policy byte.** That costs one bit of
recorded metadata that no code reads. It is a grammar gap, not a design decision, and it changes no behaviour;
it appears in `openQuestions` only so that the eventual fix to `load_index` knows the flag is currently
unreliable as an *intent* signal — every existing chained file has it set whether the author wanted it or not.

---

## 3. WHAT THE GENERATED CODE DOES PER OPTION

The macro already varies codegen by option in a dozen distinct places and references the four index options across
46 lines of `macros/lib.rs`: parsing (912-937, 1454-1502), spec emission (`index_tokens`, 3506-3518), and codegen
gating (2435, 2521, 4431-4460, 4669-4678). The whole-subsystem gate is `typed_api` / `high_cardinality_api`
(macros/lib.rs:2514-2523), which emits `quote!()` — literally nothing — when off. **Every option below follows
that shape: off means the item is not generated at all, not generated-and-disabled.**

### 3.1 `channels = interleaved | planar` (O1 / O2)

**The trait, and the two impls the option chooses between.** Revision 3 asserted the consts without ever
declaring what implements them. The owner asked specifically for "the trait implementation should differ
according to the options", so here is the trait and here is the difference:

```rust
// crates/varve-core/src/traits.rs — sibling of VarveMatrixBlock (traits.rs:71-75),
// which is the exact precedent: a per-block trait carrying layout constants that
// the macro emits only for blocks declaring the attribute.
pub trait VarveChannelBlock: VarveBlock {
    /// The one field this block's payload consists of.
    type Sample: RawSample;

    /// Set by `channels =`. No default: a block either declares the attribute
    /// and gets an impl, or declares nothing and has no impl at all.
    const LAYOUT: ChannelLayout;           // Interleaved | Planar

    /// Set by `extent_integrity =`. Defaulted, so O3 is independently optional.
    const EXTENT_CRC: bool = false;        // O3
    /// Set by `channel_epoch =`.
    const EPOCH: bool = false;             // O8
    /// Set by `scan_axis =`.
    const SCAN_BASE: bool = false;         // O9

    /// Declared channel ids, when the block's channel set is static. Empty
    /// means "read the set from each block's CET" — the C11 case.
    const CHANNELS: &'static [ChannelDescriptor] = &[];
}
```

`LAYOUT` is the only required item, which is what makes the coupling of §1.2 rule 1 structural: O3/O8/O9 are
`const` overrides *on this trait*, so they cannot be expressed for a block that has no impl.

```rust
// channels = interleaved            |  // channels = planar, extent_integrity = crc32
impl VarveChannelBlock for DataBlock {   impl VarveChannelBlock for DataBlock {
    type Sample = f64;                       type Sample = f64;
    const LAYOUT = ChannelLayout::Interleaved;   const LAYOUT = ChannelLayout::Planar;
}                                            const EXTENT_CRC: bool = true;
                                         }
```

Everything downstream is a `const` branch on those two impls: the writer's scatter helper, the reader's
`read_extent_into` (strided gather vs one pread), the CRC loop and the CRC verify are all
`if <T as VarveChannelBlock>::EXTENT_CRC { … }` over a compile-time constant, folded away entirely when false.

**`ChannelPayload<T>`, defined.** The block's payload field type changes, and that is load-bearing:

- `ChannelPayload<T: RawSample>` is a **new codec** in `varve-core` implementing `VarveEncode` / `VarveDecode`
  with its own `SCHEMA_ID` and a `WIRE_TYPE` of the variable-bytes class. Its wire encoding **is** the CET plus
  the concatenated extents of §8.2 — which is why whole-record decode stays truthful: a reader that calls the
  ordinary derived decode for `DataBlock` gets a `ChannelPayload<f64>` back and can ask it for a channel, rather
  than getting silently misparsed `Vec<f64>` bytes.
- In memory on the write side it is a thin view over the writer's existing payload buffer plus a `&[ChannelSlice]`
  descriptor list; it owns nothing and allocates nothing per record beyond what the writer already allocates
  (§7.7's pre-existing per-record `Vec<u8>`).
- **It moves the schema hash**, by construction and correctly: `schema_fingerprint` hashes the literal type
  spelling `type={type_identity}` (macros/lib.rs:752) plus `<#ty as VarveEncode>::SCHEMA_ID` and
  `<#ty as VarveDecode>::SCHEMA_ID`, folded into `computed_schema_hash` at format.rs:1863, and `field.wire_type`
  is hashed again at format.rs:1874. `Vec<f64>` → `ChannelPayload<f64>` therefore moves it three ways over.
  **This is the right outcome**: the payload bytes genuinely changed shape, and a spec pinned with
  `schema_hash: computed;` should refuse to open the new file rather than misparse it. §4 places O1/O2 in the
  hash-moving group accordingly, and §17 rewrites the migration story.
- **A block declaring `channels` must declare exactly one field, of type `ChannelPayload<T>`.** Any other shape
  is a compile error, in the style of `key_index`'s rejections (macros/lib.rs:1930-1935, 1962-1974). §13.10 has
  the full rejection table.

**New generated items, per block that declares it:**

| Item | Kind | Off |
|---|---|---|
| `impl VarveChannelBlock for DataBlock` with `const LAYOUT: ChannelLayout` and `const CHANNELS: &[ChannelDescriptor]` | trait impl | not emitted; the trait is not implemented, so nothing generic over it compiles against this block |
| `<Format>StreamWriter::push_datablock_channels(&mut self, extents: &[ChannelSlice<'_>]) -> Result<AppendInfo>` | writer method, sibling of the existing `push_<name>` / `push_<names>` pair (macros/lib.rs:4677-4700) | not emitted |
| `<Format>StreamReader::datablock_channels(&self) -> Result<ChannelSet<'_>>` | reader method, sibling of the existing plural scan method (macros/lib.rs:4655-4667) | not emitted |
| CET encoder/decoder monomorphised for this block's sample width and channel count | private | not linked |

**The two entry points, reconciled.** Revision 3 put the entry point on the generated
`<Format>StreamReader::datablock_channels` in §3.1 and on core's `VarveStreamReader::channels::<T>()` in §10, and
never said how they relate. They are one surface with two spellings, exactly as the existing
`push_<singular>` / `push_<plural>` pair (macros/lib.rs:4657-4700) is a generated spelling of a core append:

- **`VarveStreamReader::channels::<T>() -> Result<ChannelSet<'_>>`** is the primitive, in `varve-core`, generic
  over `T: VarveChannelBlock`. All behaviour lives here. This is what §10 documents.
- **`<Format>StreamReader::datablock_channels()`** is the generated zero-cost alias, emitted once per block that
  declares `channels`, whose whole body is `self.inner.channels::<DataBlock>()`. It exists so the typed API reads
  the way the rest of the typed API reads, and it is the item that disappears when the option is off.

Nothing is duplicated: turning the option off removes the alias; the primitive is generic and is never
instantiated for a type with no `VarveChannelBlock` impl, so it dead-strips.

**What the writer emits differently.** With `Planar`: the scatter helper opens C cursors into the payload buffer
it already owns, writes each channel contiguously, then back-patches the 32 B CET header and `C × 32 B` entries at
payload offset 0. With `Interleaved`: the payload bytes are exactly what they are today; only the CET is added, and
each entry is marked strided, with the stride recoverable as `byte_len / sample_count`.

**What the reader gains.** `ChannelSet` → `ChannelView` → `Extent`, and the two primitives that matter:
`extents(range)` (resolve once, then walk) and `read_extent_into(&Extent, &mut [S])` (**exactly one pread**, no
directory work, no descent, no allocation). With `Interleaved`, `read_extent_into` reads the whole strided span
and gathers; with `Planar` it reads only the channel's bytes.

**What is compiled out when off.** Everything above, plus `ChannelSet`/`ChannelView`/`Extent` are never named, so
their code is dead-stripped in a build whose formats all leave `channels` off.

### 3.2 `extent_integrity = crc32` (O3)

Generates `const EXTENT_CRC: bool = true` on the block's `VarveChannelBlock` impl. The writer's per-extent CRC
loop and the reader's verify branch are both `if <T as VarveChannelBlock>::EXTENT_CRC { … }` over a const — folded
away entirely when false. Also generates the rejection of `RECORD_FLAG_COMPRESSED` records (file.rs:43) on the
partial-read path, since a partial read of a compressed payload is meaningless.

### 3.3 `index: [block_offset_chain]` (O4)

**This one already exists end to end on the write side.** `index_tokens` (macros/lib.rs:3506-3518) emits the
`IndexPolicy` fields into the const spec; core then writes the footer because `spec_needs_record_footer`
(format.rs:1992-1994) is true; `BlockTails` maintains the per-block-id tail (file.rs:1122, 1158);
`stage_state_records` hands the tail to the sidecar only under this flag (stream.rs:1397, 1406-1414); the read side
already **decodes** `prev_same_block_offset` into `RecordIndexEntry` (file.rs:706, `nonzero_offset` file.rs:8145)
and prints it in layout dumps — **and nothing follows it.**

What is new is therefore **read-side only**: a chain traversal (P4, §11) and a block-id-set filter on
`StreamingBlocks`, which is a single `!=` against one `T::ID` today (stream.rs:626).

### 3.4 `directory: seek(commit)` (O5)

Emits `const DIRECTORY: DirectoryPolicy` into the spec, and on the writer a call site inside the commit path that
builds and appends the BDR + CHK records. **The plumbing is cheap and the precedent is exact:**
`prepare_stream_creation_nonce_record` (file.rs:8574) is a thin wrapper over the generic `prepare_stream_record`
with `RECORD_FLAG_INTERNAL`, and it is appended through `append_prepared` (stream.rs:1229), which performs **no
block-id check** — the reserved-id rejection lives only in the public `push_*` entry points (stream.rs:954, 994).
So BDR/CHK emission is **two more `prepare_*` wrappers and one call site**, not a new subsystem. Internal records
are already handled downstream: `validate_record_entry` accepts `RECORD_FLAG_INTERNAL` (file.rs:6981), and
`StreamingBlocks` skips any `block_id != T::ID` after 32 B (stream.rs:626), so user scans never see them.

**The cost is the syscall, not the plumbing** (§8.7), and the **risk** is the location. Revision 3 named only
rollback and poison. There is a third, structural obstacle it omitted, and it is the reason "two wrappers and one
call site" understates the work:

**The mutation permit does not compose at the emission point.** `append_prepared_chunk` **consumes** a
`StreamMutationPermit` by value (stream.rs:1248-1253) — one permit, one chunk append. `commit_state_chunk`, by
contrast, runs *inside* an already-taken mutation and only **borrows** one (`&StreamMutationPermit`,
stream.rs:1433, with the reason spelled out in its doc comment at 1425-1432: demanding a fresh permit would
re-check a flag the call may be about to set). The directory append has to happen between those two, so it needs
a permit that the surrounding mutation has already consumed. The resolution is not a new subsystem but it is not
free either: the directory append must go through an internal path that takes the permit **by reference** and
performs the same poison check the by-value path performs, factored out of `append_prepared_chunk` so there is
exactly one implementation of that check rather than two that can drift. Getting that factoring wrong reopens
F-05, the defect the by-value permit exists to prevent. **Highest-risk item in the plan**, and this is the largest
part of the risk, ahead of rollback and poison.

Plus the allocation: `append_prepared_chunk` takes `bytes: &[u8]` borrowed, so the BDR+CHK bytes must be
materialised into a buffer of their own — **1 heap allocation per commit** (§8.7), reusable across commits from a
writer-owned scratch `Vec` after warm-up, but not zero on the first pass.

Reader gains: `ChannelSet::datablock(ordinal)`, `ChannelView::read_into(start, ..)`, `ChannelView::len()` in O(1).
With the option off, `extents(range)` still exists but resolves by forward walk from the last known point.

### 3.5 `channel_epoch` / `scan_axis` (O8 / O9)

Pure const flags on the `VarveChannelBlock` impl controlling whether the writer populates two fields that are
already in the fixed 32 B CET header, and whether `Extent::epoch` and the ordinal-range reader methods are
generated. Zero bytes either way.

### 3.6 `record_align` (O7)

Changes the writer's record-start computation and the scanner's advance. Generates `extent_slice` on `ChannelView`
under `feature = "zero-copy"`. **This one changes framing** — see §4.

---

## 4. DEFAULTS AND COMPATIBILITY

**Every option in §1 defaults to off. An all-off format is byte-identical to today, hashes identically, and
generates the same items.** Revision 3 justified that entirely with "off ⇒ `quote!()`" (macros/lib.rs:2514-2523).
That citation is real and it settles the *codegen* layer — but it proves nothing about `varve-core`, where two
structures are written as fixed unconditional sequences and would move for every existing file the moment a new
policy field is added. §4.1 states the two rules that make the headline true. Without them it is not a claim, it
is a hope.

### 4.1 The hash-and-manifest neutrality rules — implementation obligations, not claims

**Rule 1 — a new `FormatSpec` policy is hashed only when it differs from its inert default.**
`computed_schema_hash` (format.rs:1798-1877) writes its inputs in a fixed unconditional order. §3.4 adds
`const DIRECTORY: DirectoryPolicy` to the spec and O7 adds `record_align`. If either is hashed unconditionally —
even as a zero byte — **every pre-existing file's hash moves, with every option off.** The precedent that solves
this is already in the same function and revision 3 cited it without applying it: the layout spec is hashed only
inside `if !self.layout.is_varve_native_default()` (format.rs:1841-1844). Every new policy follows that shape:

```rust
if !self.directory.is_off() {
    hash.write_bytes(b"directory-v1");
    hash_directory_policy(&mut hash, self.directory);
}
```

A test asserting `computed_schema_hash` of a representative pre-existing spec against a hard-coded constant is
the enforcement; it belongs in the same commit as the field.

**Rule 2 — the embedded manifest payload is wire content, and appended bytes must be version-gated.** This is the
one §4 never mentioned at all. `index_policy_byte` is *pushed into the manifest record's payload* at file.rs:6335
and strictly decoded at 6387; `index_policy_from_byte` (file.rs:6629-6641) matches `1 | 2 | 3..=15 | _ => Err`,
so it **rejects byte 0 outright**. Any new policy byte appended to that payload changes the manifest record's
bytes for every format that sets `manifest: embedded`, and an older reader decoding it strictly will fail. The
precedent, again already in the code, is `decode_schema_manifest`'s version gating: `commit_policy` is read only
when `payload_version >= 4`, extension and compression only at `>= 3`, field lists only at `>= 2` (file.rs:6391+).
So: **a new manifest field bumps `MANIFEST_PAYLOAD_VERSION` (file.rs:62) and is read only above that version**,
and old payloads decode unchanged.

Two consequences worth stating plainly:

- A format with **no** new options set writes **no** new manifest bytes, at the old payload version. That is what
  makes "byte-identical when off" true for `manifest: embedded` formats, which is otherwise the case where it
  would quietly fail first.
- `index_policy_from_byte`'s rejection of 0 is a live constraint on any future "all index options off" policy: a
  spec with every index bit clear cannot round-trip through the embedded manifest today. No option in §1 produces
  that state (O4 sets bit 2, and the DSL default sets bit 0), but a `load_index` fix that makes `scan_on_open`
  honestly optional would, and it must widen that match arm.

### 4.2 What `computed_schema_hash` covers

`computed_schema_hash` (format.rs:1797-1878) hashes: format version, endian, extension, **index policy byte**
(2396-2405, written at 1809), commit policy (1810), **integrity policy** (1811), recovery (1812), manifest (1813),
compression policy and per-block compression (1814-1821), matrix dimensions/commits/blocks/aux (1822-1840), the
layout spec **only when it is not the native default** (1841-1844), and then every block's id, name, **version**,
kind, per-block identity fingerprint and every field (1843-1876). It **never hashes payload content**, and it never
hashes `key_index`.

| Option | Moves `computed_schema_hash`? | Changes the wire format? | Migration path for an existing file |
|---|---|---|---|
| **O1 `channels = interleaved`** | **Yes.** Revision 3 said "No — the CET is payload content", which was true of the CET and false of the design: the field type becomes `ChannelPayload<T>`, and the fingerprint hashes the literal type spelling (macros/lib.rs:752), both codec `SCHEMA_ID`s, and `field.wire_type` (format.rs:1863, 1874) | **Yes, inside the payload.** Old readers calling `read_payload` (file.rs:733) still get the whole payload; they just see 160 extra bytes at the front and a different field encoding | **New file, or an unpinned hash.** With `schema_hash: computed;` the old spec refuses the new file — correctly. With the default unpinned literal (`schema_hash` absent ⇒ literal 0 ⇒ the open-time equality check is disabled), a **mixed file is readable**: the CET's `magic = b"VCET"` at payload offset 0 is the per-record discriminator, and §10's `ChannelSet` reports a pre-CET block through `BlockChannels::Opaque` rather than an error (§10.3) |
| **O2 `channels = planar`** | **Yes** — same mechanism | **Yes, inside the payload**, and more consequentially: sample order changes. **A reader that assumes interleaved layout will misinterpret planar bytes.** The `VCET` magic plus the moved hash is what stops that; revision 3 relied on the magic alone | Same as O1. **Retrofitting selectivity onto already-written interleaved bytes is impossible** — only a transcode to a new file works (§15.1) |
| **O3 `extent_integrity`** | **No, independently** — it is a `const` on the trait impl and a flag bit inside the CET, so it adds nothing to the hash beyond what O1/O2 already moved. *Note:* declaring per-extent CRC as a new `integrity:` variant instead would move the hash a second, separate way (`integrity_policy_hash_byte`, format.rs:1811), which is the reason this design places it per-block | No, beyond what O1/O2 already changed (a flag bit and a field that is already reserved) | None beyond O1/O2's |
| **O4 `index: [block_offset_chain]`** | **Yes, unavoidably.** `index_policy_hash_byte` sets bit 2 from `block_offset_chain` (format.rs:2396-2405) and the byte is hashed at 1809. It is a policy bit, hashed by construction; there is no way to dodge it | **Yes** — a 32 B footer on every record of every type (format.rs:1992-1994) | **Set at file creation.** A spec pinned with `with_computed_schema_hash` (format.rs:1122) will refuse to open a file written before the flag changed. Existing file ⇒ transcode to a new file, which is required anyway because `replace_rewrite` is **refused outright for footer specs** (file.rs:3898) |
| **O5 `directory: seek`** | **Yes**, via its O4 dependency. The BDR/CHK records themselves do **not** move it, because they are allocated in the reserved internal id range and never enter `spec.blocks` — the only block collection hashed (format.rs:1843-1876). **Registering them as user block types would move it, which is why this design does not** (§8.3.1) | **Yes** — two new internal record types appear in the stream. They are skipped by every existing reader after a 32 B header read (stream.rs:626) | Same as O4: new file |
| **O6 tail-follow** | **No** | **No** | None — it is a reader capability |
| **O7 `record_align`** | **Yes, and it must.** Inter-record padding is framing: a reader that does not expect it walks into padding. Alignment must be in the hashed spec or it is a silent-corruption vector | **Yes** — up to 7 B (or 4095 B) of padding between records | New file |
| **O8 `channel_epoch`, O9 `scan_axis`** | **No, independently** — both are `const`s on the trait impl writing into fields already present in the CET header | No, beyond O1/O2 | None beyond O1/O2's |

**The hazard revision 3 built its migration story on, and how this revision closes it.** The per-block identity
fingerprint (macros/lib.rs:734-802, hashed at format.rs:1854-1866) covers `block_id`, `version`, `kind`, `endian`,
`keyed`, and each field's id/name/type-spelling/presence/codec `SCHEMA_ID`. It does **not** cover a codegen-only
attribute — that is why `key_index` is hash-neutral. Revision 3 assumed `channels` would be the same kind of
attribute, and concluded that **two files could carry the same schema hash and different payload byte order**,
with only a runtime `VCET` magic between a reader and a silent misread.

That conclusion was correct *given* revision 3's design, and it is a bad property to design in deliberately. This
revision removes it: because the payload encoding changed, **the field type changes with it**, and the type
spelling is inside the fingerprint. `interleaved` and `planar` are two different `ChannelLayout` values on the
same `ChannelPayload<T>` type, so those two still share a hash — and that is the residual case the `VCET`
header's `flags` bit 2 (strided vs planar, §8.2) discriminates per record. But `Vec<f64>` versus
`ChannelPayload<f64>` — the case that would misparse rather than mis-order — is now caught at open by the hash.
If a clean schema-level cut between interleaved and planar is also wanted, bump the block's `version =`, which
moves the hash (`hash.write_u16(block.version)`, format.rs:1849, and the fingerprint header carries `version=`
too). Both are available; the costs differ and are stated.

---

## 5. THE PHYSICS — the consequence of the layout option, not a question

**A physically interleaved payload cannot be read channel-selectively below full block I/O. Period.** This is
what `channels = interleaved` (O1) buys and does not buy. It is not softened anywhere in this document.

4 channels of `f64`: CH1's bytes recur with period `C × W = 32 B`. The smallest unit the storage stack transfers
is a 512 B sector, in practice a 4 KiB page. A 4 KiB page holds `4096 / 32 = 128` stride periods and **every one
of them contains CH1 bytes**. Every page of every datablock is a page a CH1-only query must fetch. Reading CH1
alone transfers 100 % of the datablock bytes and yields 25 % of them as useful.

An index is a map from a question to a byte offset. It cannot change the granularity at which the device moves
bytes. So:

- An index **removes** the O(k) walk needed to *find* the k-th datablock.
- An index **removes** the *decode* of CH2..CH4 — real, roughly 4× CPU on this shape.
- An index **never removes** the *read* of CH2..CH4's bytes.

**Any claim that an index gives 1/4 the I/O on strided interleaved storage is false, and this design does not
make it.** O5 over O1 would be a directory whose payoff the layout throws away — which is why §1.2 makes O5
require O2.

The same statement applies to the matrix subsystem, symmetrically. The matrix ordinal is

```rust
// crates/varve-core/src/matrix.rs:2447
key.scan.checked_mul(dim1).and_then(|base| base.checked_add(key.ch))
```

with byte offset `slot_region_offset + ordinal * slot_stride`‡. `ch` is fastest-varying, so the matrix is
**scan-major**: one *scan* is contiguous, one *channel* is strided with period `n_ch × slot_stride`. At
`n_ch = 64, slot_stride = 8` that is 8 useful bytes per 512. The matrix is optimised for exactly the query you are
not asking.

### 5.1 Which layout option, by dominant query

| Dominant query | Option | Why |
|---|---|---|
| "all channels over a time window" | `channels = interleaved` (or `off`) | one sequential read per block |
| "one channel over a long span" | `channels = planar` | 1/C of the bytes, one read per block per channel |

The case that would argue *for* interleaving — read every channel of one datablock — **is not harmed by planar**
(§12, case C6): both layouts read the same single payload with one positional read, and planar then sub-slices at
zero further I/O. That asymmetry, not a trade-off, is why planar has no downside on the read side.

Second-order consequence this design owns: planar's win assumes **SSD/NVMe**. On 7 ms-seek spinning media, one
32 KiB read per 128 KiB block across a whole file can be slower than streaming the file interleaved at high queue
depth. The design targets NVMe/SSD; cold HDD archival tiers need a per-tier recommendation (`openQuestions`).

### 5.2 The crossover — where planar stops helping, and what actually dominates a small query

The 1/C figure is a *full-scan* figure. It must not be read as a flat property of O2. Derivation, per datablock:

- Wanted: `n` samples of one channel from this block, width `W`, `C` channels, page size `P = 4096`.
- **Interleaved (O1):** those samples span `n × C × W` contiguous bytes → `ceil(n·C·W / P)` pages.
- **Planar (O2):** they span `n × W` contiguous bytes → `ceil(n·W / P)` pages.

Both equal **1 page** whenever `n·C·W ≤ P`, i.e. `n ≤ P/(C·W)`:

| C | W | Samples/block below which O1 and O2 cost the same |
|---|---|---|
| 4 | 8 (f64) | **128** |
| 8 | 8 | 64 |
| 64 | 8 | **8** |
| 4 | 2 (i16) | 512 |

Above that threshold the ratio climbs toward `C`, reaching it once `n·W ≫ P`. At the full-scan extreme
(`n = 4096`, the whole block) the ratio is exactly `C`: **256 GiB vs 1 TiB**, verified in §10.1.

**So: at 4 channels O2 matters for spans of ≥ a few hundred samples per block and is neutral below that. At 64
channels it matters almost immediately.** The recommendation survives at every channel count; the *magnitude* does
not, and §0 says so.

**What actually dominates a small window: the directory, not the layout.** Trace "CH1 samples 1 000 000..1 000 100"
with O2 + O5 on, 1 TiB, 8 388 608 datablocks, 32 datablocks per directory group, 262 144 groups. Sample 1 000 000
lands in datablock 244 (244 × 4096 = 999 424), offset 576 in the block; the window does not straddle a block.

| Step | Reads | Logical bytes |
|---|---|---|
| CHK skip-list descent from the tail, cold | ≤ 5 levels × ≤ 15 hops = **≤ 75** | ~12.6 KiB |
| BDR: header + ~5 binary-search probes on CH1's prefix column + 1 entry | ~7 | ~90 B |
| CET of the target datablock | 1 | 160 B |
| The CH1 extent itself | 1 | 800 B |
| **Total** | **~84 positional reads** | **~13.7 KiB** |

At 4 KiB page granularity those ~84 reads touch ~84 distinct pages ⇒ **~336 KiB of device traffic to deliver
800 B — a ~430× amplification, all of it directory, none of it layout.** (Revision 3 wrote 5 × 15 = 80 and
propagated 89 reads / 356 KiB / 445× through four sections. The product is 75; the conclusion is unchanged and
the numbers are now right.)

Three consequences, all reflected in the design:

1. **The cache belongs on the skip list, not on the CETs** (§9.5). Caching every CHK at level ≥ 1 costs
   `262144/16 = 16 384` nodes + higher levels (1 024 + 64 + 4) = 17 476 × 168 B ≈ **2.9 MiB** at 1 TiB, and
   collapses a warm descent to ≤ 15 level-0 hops: **~24 reads, ~96 KiB of device traffic, ~123× amplification**.
   Still amplified.
2. **Point queries are inherently amplified in an append-only variable-length file.** The design's answer is to
   make `extents()` — range iteration, which pays the descent once and then walks forward — the primary API, and
   to document `read_into(start, ..)` as random-access convenience carrying a descent.
3. If you need many small scattered CH1 windows, batch them into one ascending `extents()` pass. The design does
   not pretend a 100-sample random peek is cheap.

---

## 6. The requirement, restated

```
datablock[CH1, CH2, CH3, CH4, CH1, ..., CH4]  metablock[metadata]  datablock[...]  datablock[...]  metablock[...]  ...
```

Ground truth, as stated by the owner:

- **One datablock is one record.** The channels are *not* one record each.
- **The payload of that one record contains many samples of CH1..CH4 interleaved inside it.**
- **Metablock records are interspersed between datablocks**, at irregular intervals.
- **The stream continues indefinitely.** No known final length; the file grows while readers read.

The question: *can a caller query only CH1 across the whole file, and only CH2, and so on?* And if the library
will not do it directly, it must at minimum expose primitives with which the user can (§11).

Constraints carried through every cost derivation, and applied to every option in §1:

- **C1** Continuous high-rate append is primary. No per-record syscall, no per-record heap allocation, no new
  O(N) per-operation work on the append path.
- **C2** TB-scale. Open reads a header; pages fault in on demand; memory is bounded by the working set, never by
  total file content. Eager load at open is unacceptable.
- **C3** Reads are `&self`, so one handle serves several threads concurrently.

---

## 7. What exists today

### 7.1 The addressing concept is already right, on the wrong implementation

```rust
// crates/varve-core/src/matrix.rs:837
pub struct MatrixKey {
    pub scan: u64,
    pub ch: u64,
}
```

The matrix already addresses cells by (scan, channel), and channel is already a first-class *state* axis:
`MatrixCommitKind::PerChannel` keeps one commit bit per channel, `per_channel_count` (matrix.rs:5329) resolves it
by picking the dimension literally named `"ch" | "channel" | "channels" | "n_channels"`, falling back to
`dimensions[1]`. Public surface: `is_matrix_channel_committed` / `set_matrix_channel_committed` (file.rs:1985,
2538, 2542, 4786, 4791).

Every retrieval-side property is wrong for this workload:

- **Single cell only.** `read_matrix_cell` (file.rs:1958 on `VarveReader`, 2479 on the writer, 4649 on
  `VarveFile`) and `matrix_cell_payload` (file.rs:4654). *No row, no column, no range, no iterator exists anywhere
  in `matrix.rs`.* A row is physically contiguous and there is still no row API.
- ~~**`&mut self`**~~ **— FIXED, round 16. C3 is met.** It was `&mut self` purely because it threaded
  `&mut self.file` into `matrix::read_cell`, which did `file.seek(SeekFrom::Start(offset))?` then
  `file.read_exact(&mut payload)?`. That pair is now a single `MatrixRegionReader::read_exact_at`
  (`pread` / `seek_read`), and every matrix read entry point on all three handle types takes
  `&self`. No lock was introduced; `Sync` is derived. Pinned by
  `crates/varve/tests/matrix_concurrent_reads.rs`.
- ~~**Eager bitmap load at open.**~~ **— ADDRESSED BEHIND A DECLARED OPTION, round 16. C2 is met when
  the option is declared, and only then.** The eager path described here
  (`load_commit_bitmaps` / `load_crc_valid_bits`, with `cell_status` a pure in-memory
  `SparseBitmap::get`) is still the **default**, because it is what makes commit-map corruption a
  finding of `open` rather than of a later read, and that is behaviour, not performance.
  `ReadLimits::with_matrix_metadata_residency(MatrixMetadataResidency::Lazy { cache_bytes })` now
  selects the working-set-bounded path: open reads the persisted page index only (32 bytes for a
  one-live-page matrix, measured), pages fault in through `MatrixRegionReader` on first touch, and
  an LRU evicts at `cache_bytes` — so residency is bounded by the declared ceiling rather than by
  the written-cell count. The two consequences are declared on the enum, not inherited from the
  cache: detection moves to first touch, and a faulted-in page is as of that touch rather than as
  of open. Measured in `crates/varve/tests/matrix_lazy_residency.rs`.
- **Dimensions fixed at create.** `create_layout` (matrix.rs:2468) is create-only; open re-validates the persisted
  header against recomputed dimension-derived lengths‡. `max_matrix_cells` defaults to 16 M (format.rs:114‡),
  `max_matrix_slot_region_len` to 8 GiB (format.rs:118‡), and matrix data lives *before* `append_log_start`‡.
  **A matrix cannot represent an indefinitely growing stream.**

The concept exists on the wrong implementation. §13.11 keeps the one matrix fix worth doing anyway.

### 7.2 The repo's existing idiomatic answer: de-interleave at write time

```
variable TdmsChannelChunk(id = 103, key = [group, channel, chunk_index]) {
    start_index, data_type, values_f64: Vec<f64> = default, values_i64: Vec<i64> = default
}
```
(tdms_model.rs:46-81; `write_first_tdms_segment`‡ writes one chunk for channel "Amplitude" only.)

This is *planar at record granularity*. It works today with **no options at all** (it is available in the O0 row),
is the right stopgap, and is the wrong endgame (§8.5).

### 7.3 Per-type record access is O(total records)

```rust
// crates/varve-core/src/file.rs:4178
pub fn blocks<T: VarveBlock>(&self) -> Result<BlockVec<T>> {
    crate::collections::ensure_registered_block::<T>(self.spec)?;
    let entries = clone_matching_entries(self.spec, &self.index, |entry| entry.block_id == T::ID)?;
    Ok(BlockVec::new(self.spec, self.snapshot.clone(), entries))
}
```

`clone_matching_entries` (file.rs:9418) makes **two full linear passes** — one `.filter().count()` to size the
allocation, one `.filter().cloned()` to fill it — then copies `count × 104 B`. `RecordIndexEntry` (file.rs:706) is
4 × u64 + 3 × `Option<u64>` (48 B, no niche) + 3 × u32 + 2 × u16 + bool ⇒ **104 B**, exactly what
`index_bytes_for_count` (file.rs:9321) charges.

It only works because `self.index` exists, and every `VarveFile::open*` path calls `load_index` (file.rs:8738) →
`scan_records_from` (file.rs:9020) unconditionally. So **separating datablocks from metablocks on the resident path
costs Θ(N) at open plus 2N compares per call, with a 104·N resident floor** — 83 GB at 8×10⁸ records. Violates
**C2**. `IndexPolicy.scan_on_open` (format.rs:483) exists but is **consulted nowhere in `varve-core`** — its only
two uses are `index_policy_hash_byte` (format.rs:2397) and the manifest's `index_policy_byte` (file.rs:6620). So
the Θ(N) open is not something O4 "drags in": it is the unconditional `load_index` call, present with or without
any option in §1, and it is a defect that can be fixed independently of everything here (§2.3). **Nothing in this
design depends on that fix**, because the entire read side lives on the stream path, which never scans at open.

### 7.4 The persisted-but-unread per-block chain (what O4 turns on)

```rust
// crates/varve-core/src/format.rs:481-487
pub struct IndexPolicy {
    pub scan_on_open: bool,
    pub checkpoint_on_flush: bool,
    pub block_offset_chain: bool,
    pub keyed_offset_chain: bool,
}
```

With `block_offset_chain` on, every record footer carries `prev_same_block_offset`: an absolute `u64` at footer
byte 8, `0` meaning "no predecessor" (native_layout.rs:122-158‡; `nonzero_offset` at file.rs:8145). This is **a
backward per-block-id linked list, already on disk.** The write side maintains it (file.rs:1122, 1158 via
`BlockTails`, a `Vec<(u32,u64)>` with binary-search `tail()`). The read side decodes it into
`RecordIndexEntry.prev_same_block_offset` (file.rs:706) and prints it in layout dumps — **and nothing follows it.**

Two costs, both charged in O4's matrix row:

- The footer is written iff `commit_policy.requires_record_footer() || index_policy.requires_record_footer()`
  (`spec_needs_record_footer`, format.rs:1992-1994), so enabling the chain adds **32 B to every record of every
  block type**, not only chained ones.
- **The recorded policy byte gains a bit nobody asked for.** `IndexPolicy::BlockOffsetChain` (format.rs:505-511)
  is `{ scan_on_open: true, checkpoint_on_flush: false, block_offset_chain: true, keyed_offset_chain: false }`,
  and the DSL list form does the same (macros/lib.rs:928-932). That bit lands in the schema hash (format.rs:1809)
  and in the embedded manifest (file.rs:6335) and changes **no behaviour whatsoever** (§2.3). It is a cosmetic
  cost of one bit of recorded metadata, not the Θ(N)-open cost revision 3 attributed to it.

### 7.5 The right `&self` primitive exists and is unreachable

```rust
// crates/varve-core/src/snapshot.rs:74
pub(crate) fn read_exact_at(&self, offset: u64, buffer: &mut [u8]) -> Result<()>
```

Verified: `&self`; bounds-checked through `check_range` against `SnapshotBounds`; loops `read_at`, which is
`FileExt::read_at` (pread) on unix and `FileExt::seek_read` on Windows (snapshot.rs:252-264); writes into a
**caller-supplied buffer**. One syscall, zero allocation. `SnapshotFile { file: Arc<File>, bounds: SnapshotBounds }`
(snapshot.rs:15-18) is `Send + Sync` by fields. `with_len` re-stats the file (snapshot.rs:42), which is what makes
O(1) tail-follow (O6) possible.

It is `pub(crate)` and re-exported from neither crate root. Every *public* record read materialises the whole
payload — `read_payload` (file.rs:733), `read_logical_payload` (file.rs:767), backed by `read_payload_snapshot`
(file.rs:816). **There is no `(entry, byte_offset, byte_len) → bytes` API anywhere on the record path.** The only
offset+len read in the public surface is `read_matrix_aux(name, offset, len)` (file.rs:1970, 2491, 4664), which
addresses fixed aux regions, not payloads. Publishing it is P1 (§11).

### 7.6 The one subsystem that already meets C2 and C3

`VarveStreamReader::open_native` (stream.rs:463 — **`pub(crate)`**, so this is a property of the implementation,
not a capability a user has today; O0's row is corrected accordingly) opens the file, reads `metadata().len()`,
reads the file header, constructs a `SnapshotFile`. **No scan.** `VarveStreamReader::open` (stream.rs:444, also
`pub(crate)`; users reach it through the `varve` wrappers) additionally canonicalises the
path, reads the primary identity, opens the sidecar `DiskIndexStore`, begins a snapshot, calls
`verify_primary_generation`, and pins visibility with one line:

```rust
// crates/varve-core/src/stream.rs:483 — pin_logical_len, whose body is
//   self.snapshot = self.snapshot.with_len(logical_len)?;
// Revision 3 block-quoted a `reader.snapshot = ...with_len(snapshot.committed_eof())`
// line at 483; that spelling is the caller's, not the source at that line.
pub(crate) fn pin_logical_len(mut self, logical_len: u64) -> Result<Self>
```

`StreamingBlocks::next` skips foreign records after reading only the 32 B header:

```rust
// crates/varve-core/src/stream.rs:626
if entry.block_id != T::ID {
    continue;
}
```

**`events()` already exists and is public** (stream.rs:521). It yields
`BlockEvent { block_id, block_version, sequence, record_offset, payload_offset, payload_len }` (file.rs:887) from a
header-only scan that reads **no payload at all** (`StreamEvents`, stream.rs:563). This is what makes the O0 row's
"you can still enumerate every record" claim true, and it is the enumerator P1 pairs with.

Two limits. The scanner always starts at `header_len` (`NativeStreamScanner::from_snapshot`, file.rs:8332) —
**there is no public `scanner_at(offset)`, so there is no random-access entry into the stream** — and both readers
are gated behind `feature = "high-cardinality-dev"` with `default = []` (varve/Cargo.toml:29-34). Constants:
`RECORD_HEADER_LEN = 32`, `RECORD_FOOTER_LEN = 32` (file.rs:41-42).

**Open cost, stated with its unverified half.** `open_native` is verified O(header). `open` adds
`DiskIndexStore::open` + `begin_snapshot_with_mode` + `verify_primary_generation` (stream.rs:444-483). Sidecar size
is plausibly O(B + K) — `advance_coverage_with_tail` (disk_index.rs:2275-2296‡) only advances a monotone frontier
and stores nothing per record — but redb's own open-time work and any sidecar recovery pass were **not measured
under the read-only constraint of this task**. "Open is O(1) in N" is **verified for the native half and asserted
for the sidecar half**, and belongs on the benchmark list, not in a summary as fact.

### 7.7 Two pre-existing defects this design must not inherit silently

- **`stage_state_records` is quadratic per chunk.** stream.rs:1406-1409 computes, for every record,
  `records[index + 1..].iter().all(|(candidate, _)| candidate != block_id)` — O(chunk_len²) integer compares per
  chunk whenever `block_offset_chain` is on with a sidecar attached (the flag read is at stream.rs:1397); up to
  ~2.7×10⁸ compares at the default `max_records = 16_384`‡. Fix is one reverse pass computing
  last-index-per-block-id. **Still present in the current working tree. Must be fixed before O4 or O5 ships.**
- **The batch append path allocates per record.** `PreparedStreamRecord { bytes: Vec<u8>, .. }` (file.rs:8462-8469,
  built by `prepare_stream_user_record` file.rs:8472) is heap-allocated per record and copied again into the chunk
  buffer‡. **Constraint C1's "no per-record heap allocation" is not met today**, independently of anything
  proposed here. No option in §1 adds to that count and none fixes it. §14.1's per-record row says so explicitly.

### 7.8 Windows cursor hazard (load-bearing for C3)

`read_exact_at` on Windows uses `FileExt::seek_read` (snapshot.rs:258-262), which reads at an explicit offset *and*
moves the handle's file pointer; `try_clone_file` (snapshot.rs:70) hands out a duplicated handle, which on Windows
**shares one file pointer**. Any code doing seek-then-read as two syscalls on such a handle —
`matrix::read_cell` (matrix.rs:3013), `read_payload_file_validated` (file.rs:9138), `read_stream_entry_at`
(file.rs:8412) and `NativeStreamScanner` (file.rs:8297) — can be interleaved by another thread's positional read
and return the wrong bytes **silently**.

**Design rule, non-negotiable:** on any handle reachable from more than one thread, use `read_exact_at` only —
never `cursor_at` / `try_clone_file` + `seek`. (The precise cursor side-effect of `seek_read` could not be
compile-verified under this task's read-only constraint; treat it as a constraint to verify, not a resolved fact.)

**This rule is why O6's row says P3 is not thin.** `read_stream_entry_at` does `try_clone_file` + seek and
`NativeStreamScanner` holds its own seeking handle — the exact banned pattern. P3 requires porting both onto
`read_exact_at` first, and that cost propagates to C10 (tail-follow) and C13 (N threads).

---

## 8. The mechanisms the options switch on

### 8.1 One sentence

Keep one datablock = one record; under O2 make the payload **planar** — a fixed-position channel extent table
(CET) at payload offset 0, then each channel's samples contiguous — and under O5 emit a **seek directory** at the
writer's commit boundary so a reader can reach the k-th datablock without walking.

### 8.2 The datablock payload — Channel Extent Table (CET), emitted by O1 and O2

```
payload offset 0                                    CET header, 32 B
  0   magic            u8[4]  = b"VCET"
  4   version          u16    = 1
  6   flags            u16    bit0 = per-extent CRC32 present      (O3)
                              bit1 = extents are 8-byte aligned
                              bit2 = extents are strided           (O1) / clear = planar (O2)
  8   extent_count     u32           (channels present in THIS block)
 12   epoch            u32           (ordinal of the governing metablock)   (O8)
 16   block_scan_base  u64           (monotone block-level scan ordinal)    (O9)
 24   directory_crc32  u32           (covers bytes 0..24 and all entries)
 28   reserved         u32    = 0

payload offset 32                                   extent_count entries, 32 B each
  +0   channel_id      u32           STABLE id, not a position
  +4   encoding        u16           0 = raw LE, 1 = raw BE, 2 = zstd, ...
  +6   flags           u16
  +8   byte_offset     u32           relative to payload offset 0
 +12   byte_len        u32           planar: the extent. strided: the whole span,
                                     with stride = byte_len / sample_count
 +16   sample_count    u32
 +20   crc32           u32           over exactly [byte_offset, byte_offset+byte_len)  (O3)
 +24   first_sample    u64           this channel's GLOBAL sample ordinal of extent[0]

payload offset 32 + 32*extent_count                 padding to 8 B (0 B when entries are 32 B)
then                                                the extents, concatenated in channel order
```

Design points, each load-bearing:

- **Fixed header at offset 0 with a fixed size.** A reader locates and validates the directory with one positional
  read of `32 + 32 × extent_count` bytes knowing nothing about the block. This is what makes "channel absent from
  this block → skip with 1 read and 0 extent reads" possible (case C11).
- **`channel_id` is a stable id, not a position.** Channel sets change (§13.2); position-based addressing would
  silently misroute after a change. Lookup is a linear scan over ≤ 64 × 32 B entries inside one fetched buffer.
- **`sample_count` and `byte_len` are per entry.** Ragged blocks and per-channel rate differences cost **zero
  extra bytes** (§13.3, case C12).
- **`crc32` per extent (O3).** The integrity story for partial reads. `verify_snapshot_record` (file.rs:9240)
  checksums the whole payload and a partial read structurally cannot; the per-extent CRC replaces it *for partial
  reads only*. Whole-payload reads keep the existing record checksum unchanged.
- **`first_sample` is per channel, not per block.** Channels at different rates have different sample axes.
- **The `VCET` magic is the wire discriminator**, per §4 — the only thing that distinguishes a planar payload from
  an interleaved one, since the schema hash does not.

**This is the `ChunkedBytes` wire shape with the uniformity constraint removed.** `ChunkedBytes` is already
`[32 B header][16 B/entry directory][concatenated bodies]` (chunks.rs:46-64‡) with chunk *i*'s offset a prefix sum.
But `validate_chunked_bytes` (chunks.rs:200) *enforces* uniform chunk length even though the per-entry length field
exists; there is no per-chunk read (only `decode_to_vec` chunks.rs:82 and `decode_to_vec_limited` chunks.rs:89,
both looping over all chunks); `parse_header` / `parse_entry` are private (chunks.rs:253, 281); and `decode_varve`
(chunks.rs:187) materialises the whole blob before the directory is readable. **Reuse the shape, not the codec.**

### 8.3 The seek directory (O5) — reserved-id records, backward deltas, commit cadence

#### 8.3.1 Reserved internal block ids, not registered user blocks

`computed_schema_hash` hashes every entry of `self.blocks` — id, name, version, kind, per-block identity
fingerprint, every field (format.rs:1843-1876). Registered block types move the hash. **Therefore BDR and CHK are
allocated in the reserved internal range**, where `spec.blocks` never sees them:

```
BLOCK_DIRECTORY_BLOCK_ID = 0xFFFF_FFF7      // "BDR"
CHUNK_SUMMARY_BLOCK_ID   = 0xFFFF_FFF6      // "CHK"
```

Both are free: the range in use today is `TOMBSTONE 0xFFFF_FFFE, OP …FFFD, METADATA …FFFC, INDEX …FFFB,
MANIFEST …FFFA, COMMIT …FFF9, CREATION_NONCE …FFF8` (file.rs:32-39), with
`RESERVED_BLOCK_ID_START = 0xFFFF_FF00` (file.rs:40).

The plumbing cost is small and the precedent exact — see §3.4. The cost is the *syscall*, counted in §8.7.

#### 8.3.2 Backward deltas, never absolute offsets, inside payloads

`ReplacementInfo::translate_record_offset` (file.rs:919) is applied **only to index-entry fields** —
`prev_same_block_offset` and `prev_same_key_offset`‡ — and to the rewritten record's own header. It does not and
structurally cannot touch payload content. A `replace_block` that shifts later records would leave every embedded
skip pointer stale, and since `replace_block` does not change the generation, a reader holding a *valid* witness
would follow a stale pointer into the middle of a record. **A silent misread.**

**Therefore every intra-directory pointer is a BACKWARD DELTA from the containing record's own `record_offset`.**
`translate_record_offset` adds one uniform constant to everything after the replacement point, so a backward delta
between two records on the same side of the replacement is **invariant**. Only the single hop that straddles the
replacement point is wrong.

That one hop is caught by **validating the landing site before trusting it**: read the 32 B record header, check
the record's magic / `block_id == CHUNK_SUMMARY_BLOCK_ID`, check the CHK's `chunk_ordinal` equals the expected
value, check the CRC. On mismatch, fall back to the `prev_same_block_offset` chain (§7.4), which *is* translated —
**this is the second half of why O5 requires O4** (§1.2). Cost: one 32 B header read per hop, which the descent
already pays. **Extra bytes: zero.**

#### 8.3.3 Cadence: the writer's commit boundary, not "per chunk"

See §8.7 for the arithmetic. The directory group is closed and emitted when the writer commits its sidecar state
chunk (`commit_state_chunk`, stream.rs:1432), on `flush()` (stream.rs:1051) and on `sync()` (stream.rs:1058). This
ties directory cadence to **visibility cadence**, which is a cleaner invariant than tying it to a buffer-size
heuristic: a reader can never see a datablock that no directory covers, because both become visible at the same
commit. That is what the bare `commit` ident means in the DSL clause.

#### 8.3.4 The two records

**BDR — block directory record, one per directory group.** Column-major (planar, recursively):

```
32 B header: magic b"VBDR", version u16, flags u16, block_id u32, entry_count u32,
             channel_count u32, crc32 u32
section 0:   entry_count x 16 B  { back_delta u64, payload_len u32, epoch u32 }
                 back_delta = this BDR's record_offset - the datablock's record_offset
section c:   entry_count x 8 B   cumulative sample count for channel c WITHIN THIS GROUP
                 (one contiguous column per channel, in the order given by the CHK)
```

Column-major is deliberate: binary-searching channel *c*'s prefix column is `O(log entry_count)` ranged 8 B reads
at a computed offset instead of striding through interleaved rows — §5's argument applied to the directory itself.

**CHK — chunk summary record, one per directory group.** Small, chained, carries skip pointers:

```
32 B header: magic b"VCHK", version, flags, chunk_ordinal u64, bdr_back_delta u64, crc32
40 B       : skip[0..5] u64 — BACKWARD DELTA to the most recent CHK whose ordinal is 0 mod 16^l
then channel_count x 24 B { channel_id u32, pad u32, first_sample u64,
                            cumulative_sample_count u64 }
```

**`cumulative_sample_count` is FILE-CUMULATIVE, not group-local.** If the counter were group-local, `len()` would
be a walk over all groups. File-cumulative costs **zero extra bytes** and makes `len()` a genuine single read of
the tail CHK. The BDR columns stay group-local — they are the within-group binary-search key and should stay small.

The skip array is maintained as five resident `u64`s per chained block id (`last_at_level[l]`), updated O(1) per
group: **40 B of resident writer state, one store per level per group.** At 1 TiB and 32 datablocks per group,
2²³ / 32 = 262 144 groups: a plain backward chain is 262 144 pointer chases, a base-16 skip list is
≤ 5 × 15 = **≤ 75 positional reads of ~168 B** cold, ~15 warm (§9.5).

The CHK chain head is persisted for free: `DiskIndexTail { block_id, record_offset, sequence }` in the sidecar
`TAILS_TABLE`‡, written by `stage_state_records` **only under `block_offset_chain`** (stream.rs:1397, 1406-1414),
and CHK records auto-chain through `prev_same_block_offset` (§7.4).

**Channel-set change inside a group.** The BDR has one prefix column per channel for the whole group, so a channel
appearing or vanishing mid-group would need sentinels. **Rule: the writer closes the directory group whenever the
block's channel set differs from the group's.** The CHK then carries a single well-defined channel set for its
group. Cost: one extra directory record per change; changes are rare by construction. This is the place where a
fixed-channel-count assumption would otherwise sneak back in.

### 8.4 Schema hash and wire compatibility of these mechanisms

Moved to **§4**, where it belongs with the rest of the defaults-and-compatibility story. In one line: the CET is
payload content and the directory records are reserved-internal, so **neither structure moves the hash by
itself** — but the field type the CET requires does (§3.1), and so does the index-policy bit O5 depends on. Every
option that changes bytes on disk moves the hash. The two structures are hash-neutral; the options that emit them
are not.

### 8.5 Why not one record per (channel, chunk)

The TDMS-model convention (§7.2) is the right *stopgap* and the wrong *endgame*, for three derived reasons at
C = 4:

- 4× the records ⇒ 4× resident index entries (416 B per logical block at 104 B each), 4× `set_tail` calls, and
  12–16 syscalls per logical block on the resident path.
- **One sidecar `LATEST_TABLE` row per record** if keyed (`historical_distinct_keys`, indexed.rs:257‡) — ≥ 1.3 GB
  of sidecar per TB and unbounded. The sidecar is an O(B + K) recovery index today, storing *nothing* per record
  (`advance_coverage_with_tail`, disk_index.rs:2275-2296‡).
- Retrieval is still whole-payload, so a 100-sample query reads a whole 32 KiB chunk record: **41×
  amplification**. And `(group, "CH1", *)` cannot be enumerated — `DiskIndexSnapshot` exposes only `lookup`,
  `lookup_pointer`, `lookup_pointer_canonical` (disk_index.rs:2164-2207‡), and `encode_disk_key` is
  **little-endian** (disk_index.rs:1161-1170‡) while redb orders rows lexicographically, so a channel-prefix range
  is not merely unimplemented, it is *unsortable* under the current key encoding.

### 8.6 Append cost per sample and per datablock

**Per sample.** One address computation, one store — the identical instruction count as interleaving. The writer
keeps `C` open write cursors instead of one.

Revision 3 justified the cost of those cursors by L1 capacity ("free to ~8 in a 32 KiB 8-way L1, fine to ~64 in
L2"). That is the wrong mechanism and the ~64 threshold was asserted rather than derived. **The binding resource
for `C` concurrent sequential store streams is store-buffer occupancy, write-combining/fill-buffer count
(typically ~10–12 on current x86 cores), and DTLB reach — not cache capacity.** Each cursor writes into a
different page of the payload buffer, so `C` cursors occupy `C` distinct write-combining buffers and `C` DTLB
entries; once `C` exceeds the fill-buffer count, partially filled buffers get evicted and each line is written
more than once. So: **free at C = 4, fine at C ≈ 8, degrading from roughly a dozen streams upward** — well below
revision 3's 64. Above that, use a **tiled transpose**: hold `T ≈ 8` channels' cursors at a time and make
`⌈C/T⌉` passes over a tile of samples, +1 load/store pair per sample and ~16 KiB of reusable scratch, still
**0 allocations, 0 syscalls**. The exact fill-buffer count is microarchitecture-specific and the crossover is
therefore a benchmark item, not a derived constant; it is in `openQuestions`.

**Per datablock (record).**
- CET header + entries: `32 + 32 × C` bytes into the payload buffer the writer already owns. At C = 4: **160 B**.
  Back-patched in place after the block closes — a write into an already-allocated buffer, not a reallocation.
  **This requires the channel count to be known before the first sample is stored**, so that `32 + 32·C` bytes
  can be reserved at payload offset 0. Revision 3 asserted "no reallocation" while also making a per-block
  varying channel set a headline feature (C11, §13.2); those two are only compatible under a stated rule, and
  the rule is: **the channel set is declared when the block is opened, and a channel that appears afterwards
  closes the current block and starts a new one.** Cost of a mid-block arrival: one short block, which the format
  already tolerates (raggedness is free, §13.3), and one directory-group close under O5 (§13.2). The two
  rejected alternatives are named so nobody reaches for them later: a memmove of the whole payload (128 KiB
  moved per late channel — a real per-block cost, not zero), or reserving a worst-case `32 + 32·C_max` header
  (wasted bytes in every block, and it reintroduces a fixed channel ceiling). For the `push_*_channels` entry
  point the question does not arise: the caller hands over a complete `&[ChannelSlice]`, so `C` is exact.
- Per-extent CRC32 (O3 only): `C` CRC computations that together cover the payload once, over bytes already hot
  in cache. ~1 byte/cycle with a hardware CRC ⇒ ~131 K cycles per 128 KiB block; at 32 MB/s that is 32 × 10⁶
  cycles/s, **~1 % of one 3 GHz core** (~10 % at 320 MB/s).
- **0 extra syscalls. 0 extra heap allocations. 0 new O(N) work** — *from the CET*. The syscalls come from the
  directory (O5), and they are counted in §8.7, not here.
- **Not fixed by any option:** the pre-existing `Vec<u8>` allocation per record in `prepare_stream_user_record`
  (file.rs:8472). Every "0 alloc" claim in this document means "adds no allocation", never "allocation-free".

**What the writer must buffer.** Exactly what it buffers today. One datablock is one record; a record is written
whole; the writer already holds the entire block payload before the write. Planar changes *where inside that
buffer* each sample lands. At 4 ch × 4096 × f64 that is 128 KiB, 256 KiB double-buffered, **4.096 ms block latency
at 1 MS/s/ch — identical for interleaved and planar.** The only new buffering is `C` reusable staging buffers *iff*
channels have different rates and cannot be sized in advance: one concatenating memcpy at block close
(+8 B/sample moved, ~1 % of a core at 32 MB/s), zero allocations after warm-up.

### 8.7 The syscall arithmetic behind O5's cost column

"The directory rides the existing chunk `write_all` for 0 extra syscalls" is true *only* inside `push_iter_inner`,
and only if the producer hands it many datablocks per call. Verified:

- `push_iter_inner`'s `bytes` and `records` are **local variables** (stream.rs:1103), cleared on flush and flushed
  unconditionally at the end of each call. Nothing survives the call to accumulate into.
- A live acquirer holding one 128 KiB datablock at a time calls `push_info` (stream.rs:954) →
  `push_with_prev_key_info` (994) → `append_prepared` (1229) → `append_prepared_chunk` with a **single** record:
  **one `seek` + one `write_all` per datablock** (stream.rs:1248).
- `append_prepared_chunk` takes `bytes: &[u8]` and `records: &[(u32, AppendInfo)]` **borrowed** (stream.rs:1248).
  There is no buffer to append the directory into without constructing a new one.

So the options, and their costs:

| Emission strategy | Extra syscalls | Extra memcpy | Verdict |
|---|---|---|---|
| Append BDR+CHK to the same buffer as the datablock | 0 | **1 allocation + a 128 KiB copy per datablock** | Rejected — violates C1's no-per-record-allocation intent worse than the syscall does |
| Emit BDR+CHK as their own records per datablock | **+2 per datablock (3× total)** | 0 | Rejected — 3× the syscall count on the primary workload |
| **Emit BDR+CHK as one extra `append_prepared_chunk` at the commit boundary** | **+2 per commit** | **1 allocation + ~1.6 KiB copy per commit** | **Adopted** (§8.3.3) |

**The adopted strategy is not allocation-free, and revision 3's table said it was.** Rejecting the same-buffer
strategy *because* `append_prepared_chunk` borrows (`bytes: &[u8]`, stream.rs:1248-1253) means the adopted
strategy has to materialise the BDR+CHK bytes into a buffer of their own. That is **1 heap allocation and one
~1.6 KiB copy per commit** — 1/D per record, 0.03 allocations per record at D = 32. It is genuinely small, and
smaller than the pre-existing per-record `PreparedStreamRecord { bytes: Vec<u8> }` (file.rs:8462-8469) this
document discloses at §7.7 and §14.1 — but it is not 0, and after warm-up it should be a writer-owned scratch
`Vec` cleared and refilled per commit, which makes it 1 allocation for the file's lifetime rather than per commit.
The cost table charges the honest per-commit figure, not the warmed-up one.

The adopted strategy's cost as a function of the writer's commit cadence `D` (datablocks per commit):

| D | Directory syscalls per datablock | Syscall overhead vs today | Directory bytes per datablock | Total size overhead |
|---|---|---|---|---|
| 1 | +2 | **+200 %** | 48 + 200 = 248 B | 0.34 % |
| 8 | +0.25 | +25 % | 48 + 25 = 73 B | 0.20 % |
| 32 | +0.0625 | **+6.3 %** | 48 + 6.25 = 54 B | 0.19 % |
| 256 | +0.008 | +0.8 % | 48.8 B | 0.18 % |

(BDR = `32 + 16D + 8CD` = `32 + 48D` at C = 4; CHK = `32 + 40 + 24C` = **168 B** at C = 4; both amortised over D
datablocks. The per-block 48 B is the BDR's own per-entry cost and does not amortise.)

**O5 is worth its cost at D ≥ 8 and comfortable at D ≥ 32.** Below that, either raise the commit cadence or leave
O5 off and use O4's chain for coarse navigation, accepting O(groups) forward seek instead of O(log). Both are rows
in §1; neither blocks the other.

**And the latency O5 does not cost.** The directory rides the **commit** boundary, and the writer is already
committing at that cadence today. The acquirer keeps calling `push_info` per datablock at 4.096 ms latency;
nothing is held back for the directory. The only thing tied to `D` is how often the writer already commits — a
knob that already exists, not a new one.

**Per commit, then:** one BDR record + one CHK record built by one pass over the group's already-recorded
descriptors (O(D), i.e. O(1) amortised per record), one `seek` + one `write_all`, +2 `set_tail` (O(log B),
file.rs:1158), +2 `advance_coverage_with_tail` (O(1)‡).

**Caveat:** this is exactly the path with the O(chunk_len²) defect at stream.rs:1406-1409 (§7.7). Fix first.

**Total resident writer state added:** 40 B of skip pointers per chained block id, plus the group's descriptor
list (`16 + 8C` bytes × D, reused, ≈ 1.5 KiB at D = 32, C = 4), plus the CET scratch inside the existing payload
buffer. Nothing that scales with N.

---

## 9. The read access model

### 9.1 Substrate

Every option in §1 that has a read side lives on the **scalable stream path** (`VarveStreamReader` /
`VarveIndexedReader`), and only there, because it is the only subsystem satisfying C2 and C3 (§7.6). The resident
`VarveFile` cannot serve any TB-scale case: Θ(N) open scan, 104·N resident floor, no tail-follow (§7.3, §9.4). The
matrix cannot represent an unbounded stream at all (§7.1). This design does **not** unify the three subsystems.

### 9.2 Open

Header-only on the native half, one sidecar transaction on the other — with §7.6's honesty about which half is
verified. A `ChannelSet` handle costs, at construction: with O5 on, **one sidecar `TAILS_TABLE` lookup** for the
CHK tail plus **one positional read of the tail CHK** (~168 B), giving `channel_ids()`, per-channel `len()` and the
skip-list entry point in **two I/Os with O(C) memory** — never a walk. With O5 off, `channel_ids()` costs one CET
read of the most recent datablock and `len()` is not offered in O(1).

### 9.3 Demand paging and bounded memory

Every read is a `SnapshotFile::read_exact_at` (snapshot.rs:74) into a caller-supplied or view-owned buffer. Sample
buffers are caller-supplied. **Peak memory = caches (§9.5) + caller buffer + O(C).** Independent of file size, and
independent of query span when the caller uses the streaming iterators.

### 9.4 Visibility — stated precisely

**The frontier.** A reader's visible prefix ends at the **sidecar-committed EOF**, not the native EOF
(stream.rs:483). Records written natively but not folded into a committed sidecar transaction are **invisible**.
Fail-closed and correct: a half-written record can never be observed. `VarveIndexedReader::open` pins identically
via `stream.pin_logical_len(index.committed_eof())` (indexed.rs:179‡). Appends are asserted to begin at native
EOF‡, so `record_offset` order equals append order — the only intrinsic ordering guarantee the format gives, and
the only one this design uses. (`sequence` is validated *unique*‡ — it is not an arithmetic index and must not be
used as one.)

**Snapshot semantics.** A `VarveStreamReader`'s frontier is fixed for the handle's lifetime. Therefore:

- Every `ChannelView` from `&reader` sees **one immutable prefix**.
- `ChannelView::len()` is **constant** for the view's lifetime; a caller may cache it.
- Two channels read from one handle are **mutually consistent**: same prefix, so co-iteration cannot see CH1 from
  a block whose CH2 is not yet visible.
- Reads never observe torn records: the frontier is a committed-transaction boundary and, under O3, each extent is
  independently CRC-checked.
- And because the directory is emitted at the commit boundary (§8.3.3), every visible datablock is covered by a
  visible directory. There is no window in which data is readable but unindexed.

**Live vs snapshot (O6).** Advancing is explicit and requires `&mut`:

```rust
pub enum RefreshOutcome { Unchanged, Advanced { new_len: u64 }, GenerationChanged }
impl VarveStreamReader { pub fn refresh(&mut self) -> Result<RefreshOutcome>; }
```

`refresh` re-reads the sidecar frontier and calls `snapshot.with_len(new_eof)` — the same line already executed
once at stream.rs:483, with no scan, O(1). `with_len` re-stats the file (snapshot.rs:42), so a growing file
genuinely advances. Because `ChannelView<'r>` borrows `&'r reader`, **the borrow checker guarantees no view
straddles two frontiers**: taking `&mut` to refresh requires all views to be dead.

**Tail-follow (O6).** A follower keeps a `ChannelCursor` — a plain value, not a borrow:

```rust
pub struct ChannelCursor { pub channel: ChannelId, pub next_sample: u64,
                           pub resume_offset: u64, pub generation: GenerationWitness }
```

Loop: `refresh()` → rebuild the view → `view.iter_from_cursor(&cursor)` → drain → save cursor. Each poll is O(1)
plus the new data. **This requires a public `scanner_at(offset)` (P3), which per §7.8 is not thin.** Without a
resume point every poll restarts at `header_len` (file.rs:8332) and following becomes quadratic in wall-clock time.

**Latency floor.** A follower observes new datablocks at the writer's **sidecar commit cadence** — chunk boundary,
`flush` (stream.rs:1051), `sync` (stream.rs:1058) — **not** at record cadence. Sub-chunk follower latency is not
offered (§15.3).

**Generation.** The sidecar carries a `primary_generation` witness, verified at open (`verify_primary_generation`,
stream.rs:444-483; indexed.rs:180‡). Record offsets are raw absolute file offsets, and `replace_block`
**translates every offset after the replaced record** (`translate_record_offset`, file.rs:919, applied and
re-validated inside `replace_block`, file.rs:3492‡). Therefore:

- Every exposed offset and every `ChannelCursor` carries a `GenerationWitness`.
- A `refresh` observing a generation change returns `RefreshOutcome::GenerationChanged` and **does not advance**;
  the follower must **re-open**.
- A read with a stale witness fails loudly with a distinct error.
- **The witness does not cover `replace_block`.** `replace_block` does not change the generation. That is
  precisely why intra-directory pointers are backward deltas with a validated landing site (§8.3.2) — the witness
  was never going to save them.

### 9.5 Concurrency and caching

**The invariant: no lock is ever held across a `read_exact_at`.** Concretely:

1. The cache is read-mostly. A lookup takes a shared lock, copies out (≤ 200 B), and drops it.
2. **A miss performs its pread holding no lock at all.**
3. Insertion is a `try_lock`; on contention the value is **dropped, not queued**. A duplicated pread is always
   correct, so contention costs bandwidth, never latency or ordering.

**One thread per channel, analysed.** C threads walk the same block sequence in the same order and therefore miss
the same keys at nearly the same time. Under rule 2 they each issue their own pread and none of them blocks; under
rule 3 at most one wins the insert. **Cost: up to C redundant small preads per missed key, no convoy, no
serialization point.** A sharded LRU would have produced a hot-shard write-lock convoy at 8×10⁶ keys; this does
not. That is why the simpler design is chosen over sharding.

**What is cached.** On a sequential CH1 scan over 8×10⁶ datablocks, a small CET LRU has a **~0 % hit rate**: every
block is visited exactly once.

| Cache | Contents | Size at 1 TiB | Hit rate on a sequential channel scan | Hit rate on repeated point queries |
|---|---|---|---|---|
| **CHK skip nodes, level ≥ 1** (primary, O5 only) | `262144/16 + 262144/256 + … = 17 476` nodes × 168 B | **~2.9 MiB**, bounded and explicit | n/a — a scan pays one descent total | near 100 %; collapses a cold ~84-read descent to ~24 (§5.2) |
| CET directories (optional, off by default) | 1024 × `32 + 32C` | 160 KiB at C = 4 | ~0 % | useful only for repeated access to the same blocks |

The skip-node cache is the one that earns its place. The CET cache stays available for random-access workloads and
is **not** enabled by default, because on the primary workload it is pure overhead.

`&self` on every read over a `Send + Sync` `SnapshotFile` (snapshot.rs:15-18). One handle, N threads, no shared
cursor — per-thread state is the caller's buffer and a `ChannelCursor` by value. Subject to §7.8: `read_exact_at`
exclusively, never `cursor_at` / `try_clone_file`.

---

## 10. The public API

Every read method takes `&self`. Lifetimes tie views to the reader, which is what enforces §9.4. Items marked with
an option are generated only when that option is on (§3).

```rust
// ---- identity types -------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct ChannelId(pub u32);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct GenerationWitness(u64);

/// One channel's bytes inside one datablock. Fully resolved: holding one of
/// these means the sample -> offset question is already answered.
#[derive(Clone, Copy, Debug)]
pub struct Extent {
    pub channel: ChannelId,
    pub record_offset: u64,
    pub payload_offset: u64,   // absolute; == record_offset + 32 + cet_byte_offset
    pub byte_len: u32,
    pub sample_count: u32,
    pub first_sample: u64,     // this channel's global sample ordinal
    pub epoch: u32,            // O8 only; 0 when channel_epoch = off
}

// ---- entry point ----------------------------------------------------------

impl VarveStreamReader {
    /// O1/O2. With O5: two I/Os (one sidecar tail lookup + one CHK read).
    /// Without O5: one CET read. O(C) memory. No scan either way.
    pub fn channels<T: VarveChannelBlock>(&self) -> Result<ChannelSet<'_>>;

    /// O6. Advance the visible frontier. O(1), no scan. Requires &mut, which
    /// guarantees no ChannelView straddles two frontiers.
    pub fn refresh(&mut self) -> Result<RefreshOutcome>;
}

impl<'r> ChannelSet<'r> {
    pub fn channel_ids(&self) -> Result<&[ChannelId]>;          // cached, 0 I/O
    pub fn channel(&self, id: ChannelId) -> Result<Option<ChannelView<'r>>>;
    pub fn datablock_count(&self) -> Result<u64>;               // O5: 0 I/O (from CHK)
    pub fn generation(&self) -> GenerationWitness;

    /// Read every channel of one datablock: ONE positional read of the whole
    /// payload, then C zero-I/O sub-slices. Planar costs nothing here.
    /// Random access by ordinal requires O5; sequential does not.
    pub fn datablock(&self, ordinal: u64) -> Result<DataBlockView<'r>>;

    /// Co-iterate several channels in one pass, coalescing adjacent extents.
    pub fn co_iter(&self, ids: &[ChannelId]) -> Result<ChannelZip<'r>>;
}

// ---- the channel view -----------------------------------------------------

impl<'r> ChannelView<'r> {
    pub fn id(&self) -> ChannelId;
    pub fn sample_width(&self) -> usize;

    /// Total samples of THIS channel in the visible prefix.
    /// O5: O(1) — one read of the tail CHK's file-cumulative counter
    /// (§8.3.4), cached at ChannelSet construction. Never a walk.
    /// Without O5: not offered.
    pub fn len(&self) -> u64;

    // -- THE PRIMARY API: extent-granular iteration -------------------------

    /// Resolves once (with O5, one skip-list descent; without it, a forward
    /// walk from the last known point) and then yields already-resolved
    /// extents. This is the primitive; everything else is built on it.
    /// O5 only: a bounded range needs the ordinal -> block map.
    pub fn extents(&self, samples: Range<u64>) -> Result<ExtentIter<'r>>;

    /// Available with O1/O2 alone, and the scan primitive under the
    /// `selective` set: walk forward from a sample ordinal to the visible
    /// frontier. Needs no len(), no descent, no directory. From 0 it is a
    /// plain forward walk; from a nonzero start without O5 it walks to the
    /// start first, which is why O5 exists.
    pub fn extents_from(&self, start: u64) -> Result<ExtentIter<'r>>;

    /// Reads an ALREADY-RESOLVED extent. Exactly ONE pread, no directory
    /// work, no descent, no allocation. `out` must be at least
    /// `e.sample_count` long; returns the count written. Under O3 the extent
    /// CRC is verified before return; without O3 that branch does not exist.
    pub fn read_extent_into<S: Sample>(&self, e: &Extent, out: &mut [S]) -> Result<usize>;

    /// Raw-byte sibling for callers doing their own decode.
    pub fn read_extent_bytes_into(&self, e: &Extent, out: &mut [u8]) -> Result<usize>;

    // -- random access convenience (O5; each call carries a descent) --------

    /// Fills `out` with samples [start, start + out.len()).
    /// COST: one skip-list descent + one BDR binary search + one CET read +
    /// one extent read PER SPANNED DATABLOCK. Convenience, not the scan path.
    pub fn read_into<S: Sample>(&self, start: u64, out: &mut [S]) -> Result<usize>;
    pub fn read_bytes_into(&self, start: u64, out: &mut [u8]) -> Result<usize>;
    pub fn sample<S: Sample>(&self, ordinal: u64) -> Result<S>;

    // -- iteration ----------------------------------------------------------

    pub fn iter<S: Sample>(&self) -> ChannelIter<'r, S>;
    pub fn iter_from<S: Sample>(&self, start: u64) -> ChannelIter<'r, S>;
    pub fn iter_rev<S: Sample>(&self) -> ChannelIterRev<'r, S>;

    // -- follow (O6) --------------------------------------------------------

    pub fn cursor_at_end(&self) -> ChannelCursor;
    pub fn iter_from_cursor<S: Sample>(&self, c: &ChannelCursor)
        -> Result<ChannelIter<'r, S>>;

    // -- zero copy (O7), only where sound -----------------------------------

    #[cfg(feature = "zero-copy")]
    pub fn extent_slice<S: RawSample>(&self, map: &'r SealedMap, e: &Extent)
        -> Result<&'r [S]>;
}
```

**Why `read_extent_into` exists.** Looping `for extent in ch1.extents(..)` and then calling
`ch1.read_into(extent.first_sample, ..)` would re-resolve sample → offset from scratch, costing
`D × (descent + BDR search + CET + extent)` for a full scan. `read_extent_into` consumes an already-resolved
`Extent` and is the reason case C1 costs 2 preads per block.

**On the zero-copy variant (O7).** Offered only through a `SealedMap` — a mapping over a frontier-bounded,
non-appending file — because `mmap_payloads`' unsafe precondition (file.rs:2061, 5107) requires the caller to
prevent mutation, truncation and replacement through *every handle, thread and process* for the mapping's
lifetime, which is **false by construction for an actively appended stream**. It additionally requires
(a) a `slice_from_bytes`-style accessor, since both existing raw accessors return a single `&T` (`raw_fixed`
file.rs:1586, `raw_cell` file.rs:1731); (b) alignment
`(record_offset + 32 + extent_byte_offset) % align_of::<S>() == 0`, which given the enforced
`payload_offset = record_offset + RECORD_HEADER_LEN` (file.rs:5620) reduces to a **writer-side padding obligation**
on record starts — that is what `record_align: 8` is; and (c) a copying sibling for cross-endian files, since the
`RAW_ENDIAN` equality check errors rather than byte-swapping‡. Sound, but narrow — not the main path.

### 10.1 The owner's exact query, under the `selective` set — O2 + O3 + O8, no seek directory

This is the example under the set §1.3 recommends first, so it must run under exactly that set. Revision 3's
version was captioned "`channels = planar` on" and then called `ch1.len()` and `extents(0..ch1.len())`, both of
which §10 says are **not offered without O5**. Corrected: the scan runs to the visible frontier via
`extents_from`, which needs no length and no descent.

```rust
use varve::{VarveStreamReader, ChannelId};

let reader = VarveStreamReader::open(spec, "acq.varve", StreamOptions::default())?;
let set    = reader.channels::<DataBlock>()?;          // 1 I/O without O5, 2 with
let ch1    = set.channel(ChannelId(1))?.expect("CH1 present");

// No len(): under `selective` there is no O(1) length, and asking for one
// would be an O(D) walk. The iterator ends at the frontier instead.
// Metablocks are never read: different block id. Blocks may be ragged - the
// buffer is sized from the extent, never from a samples-per-block constant.
let mut buf: Vec<f64> = Vec::new();
for extent in ch1.extents_from(0)? {
    let extent = extent?;
    if buf.len() < extent.sample_count as usize {
        buf.resize(extent.sample_count as usize, 0.0);  // grows a few times, then never
    }
    let n = ch1.read_extent_into(&extent, &mut buf)?;    // exactly ONE pread
    consume(&buf[..n]);
}
```

Cost of that scan, derived, 4 ch × f64, 128 KiB blocks, 1 TiB: 8 388 608 datablocks. Under `selective` the
iterator is a **forward walk**: it reads each datablock's 32 B record header to find the next record and each
CET to find CH1's extent, then reads the extent — **2 preads per datablock plus the header stream**, and
`≈ 256 GiB read instead of 1 TiB`, in ~1.68×10⁷ positional reads, O(1) memory. The 1/C figure — the whole point
of O2 — **survives without O5**. What O5 buys on top is not the scan; it is starting the scan somewhere other
than the beginning, and `len()`.

With O5 also on (`selective+seek`), the same loop written as `ch1.extents(0..ch1.len())?` pays one skip-list
descent total (amortised to zero) and one BDR read per directory group (262 144 reads of ~1.6 KiB at D = 32), and
replaces the header walk with directory entries.

**The identical code on a `channels = interleaved` file reads 1 TiB**, irreducibly (§5). It returns the same
samples. It saves the decode of CH2–CH4 and nothing else. That is the difference between the two options, in one
sentence.

A bounded window anywhere in the terabyte — **`selective+seek` only** — and its price:

```rust
let mut window = [0f64; 100];
ch1.read_into(1_000_000, &mut window)?;                 // requires O5
// COLD: ~84 positional reads, ~13.7 KiB logical, ~336 KiB of device traffic
//       for 800 B of data (§5.2). WARM (skip nodes cached): ~24 reads, ~96 KiB.
// For many scattered windows, batch them into ONE ascending extents() pass.
```

### 10.2 Two threads, two channels, one handle

```rust
use std::thread;

let reader = VarveStreamReader::open(spec, "acq.varve", StreamOptions::default())?;
let set    = reader.channels::<DataBlock>()?;

thread::scope(|s| {
    for id in [ChannelId(1), ChannelId(3)] {
        let set = &set;                                  // &self, shared
        s.spawn(move || -> Result<()> {
            let view = set.channel(id)?.unwrap();        // &self
            let mut buf: Vec<f64> = Vec::new();          // per-thread, ragged-safe
            for e in view.extents_from(0)? {             // &self, no len() needed
                let e = e?;
                if buf.len() < e.sample_count as usize {
                    buf.resize(e.sample_count as usize, 0.0);
                }
                let n = view.read_extent_into(&e, &mut buf)?;   // one pread
                consume(id, &buf[..n]);
            }
            Ok(())
        });
    }
});
```

Each thread issues its own `read_exact_at` (snapshot.rs:74) against the shared `Arc<File>`; both see the same
immutable frontier (§9.4). **Locking, precisely:** no lock is held across any pread, and the skip-node cache is
touched once per descent per thread — so the two threads may each issue the same handful of small directory preads
once, and never block each other (§9.5). Per-thread memory is one buffer.

When both channels are wanted in one pass, `set.co_iter(&[ChannelId(1), ChannelId(3)])` is cheaper: it walks the
directory once and coalesces adjacent extents into single reads.

### 10.3 Mixed files — the API for "this block predates the CET"

§4 and §17 lean on "the `VCET` magic at payload offset 0 is the discriminator" for files that were cut over
mid-stream. Revision 3 asserted that outcome and gave no API that could express it: `channel(id)` returns
`Option`, but `None` already means "this block does not carry that channel" (case C11), and conflating the two
would report a pre-CET block as an empty channel set — a silent wrong answer, not a missing one.

The type that expresses it:

```rust
pub enum BlockChannels<'r> {
    /// Payload begins with a valid VCET header.
    Channels(DataBlockView<'r>),
    /// Payload does not begin with VCET. Written before the cutover, or by a
    /// writer with `channels` off. Not an error: the whole payload is still
    /// readable, just not channel-addressable.
    Opaque { record_offset: u64, payload_len: u32 },
}

impl<'r> ChannelSet<'r> {
    pub fn classify(&self, record_offset: u64) -> Result<BlockChannels<'r>>;
}
```

Cost: the one CET read the reader was going to issue anyway — the magic check is four bytes of a buffer already
fetched, so classification is **0 extra I/O**. The iterators (`extents_from`, `co_iter`) skip `Opaque` blocks and
expose a running count of them, so a caller scanning a mixed file gets CH1 from the post-cutover region and an
honest "n blocks were not channel-addressable" rather than a short read it cannot distinguish from end-of-data.

This path exists **only** when the schema hash is unpinned (`schema_hash:` absent ⇒ literal 0 ⇒ the open-time
equality check is disabled, format.rs:974-985‡, file.rs:7829-7835‡). With `schema_hash: computed;` the old spec
cannot open the new file at all, and mixed files do not arise. §17 states which of the two a migrating user is in.

---

## 11. The user-implements-it path

"Or make it possible for the user to implement" is itself a supported configuration: it is the O0 row plus two
primitives. The minimal primitive set, with the honest status of each.

| # | Primitive | Enables | Status today |
|---|---|---|---|
| **P1** | `read_payload_range(&self, event: &BlockEvent, offset: u64, buf: &mut [u8]) -> Result<()>` — positional, `&self`, caller-supplied buffer, one syscall, zero allocation, bounds-checked against `event.payload_len` | O1, O2, and the user-implemented path | **NEW.** Backed directly by `SnapshotFile::read_exact_at` (snapshot.rs:74), which is `pub(crate)` and re-exported nowhere. Nothing on the public record path takes a byte range; `read_matrix_aux` (file.rs:1970, 2491, 4664) is the only offset+len read and it addresses aux regions |
| **P2** | Per-extent CRC32 in the payload directory | O3 | **NEW** (format convention, §8.2). Required because `verify_snapshot_record` (file.rs:9240) checksums the whole payload and a partial read cannot |
| **P3** | `scanner_at(offset)` — resume a stream scan at a saved point | O6 | **NEW, and NOT thin.** `read_stream_entry_at` (file.rs:8412) exists as `pub(crate)` but does `try_clone_file` + seek, and `NativeStreamScanner` holds its own seeking handle (file.rs:8297, 8332) — both banned by §7.8. P3 requires porting those onto `read_exact_at` first. `NativeStreamScanner` also always starts at `header_len` |
| **P4** | Backward per-block-id chain traversal | O4's read side | **NEW read side only.** The data is already on disk — `prev_same_block_offset`, 32 B/record already paid — and **nothing follows it**. Requires the O4 option, declared the ordinary way as `index: [block_offset_chain]`. Do not hand-build the policy in Rust to dodge `scan_on_open`: that flag is inert and the workaround only changes the hash (§2.3) |
| **P5** | Block-id-set filter on `StreamingBlocks` | O4 | **NEW**, one line: stream.rs:626 is a single `!=` against one `T::ID` |
| **P6** | `&self` matrix cell reads | independent | **NEW**, ~5 lines: replace `seek` + `read_exact` in `matrix::read_cell` (matrix.rs:3013) with `read_exact_at` |
| **P7** | Zero-copy `&[S]` extent accessor + record-start alignment | O7 | **NEW.** Only single-`&T` accessors exist (file.rs:1586, 1731); alignment unenforced |
| **P8** | Persisted per-channel sample counters, file-cumulative | O5 | **NEW** — the CHK record (§8.3.4) |
| — | **Header-only record enumeration** | O0 | **EXISTS AND IS PUBLIC, behind `feature = "high-cardinality-dev"`** (`default = []`, varve/Cargo.toml:29-34): `VarveStreamReader::events()` (stream.rs:521) yields `BlockEvent { block_id, block_version, sequence, record_offset, payload_offset, payload_len }` (file.rs:887) from a scan that reads **no payload** (stream.rs:563). This is the enumerator P1 pairs with |
| — | Lazy typed scan, bounded memory | O0 | **EXISTS**: `StreamingBlocks` (stream.rs:528, 626) — gated behind `high-cardinality-dev`. **Header-only open does not exist as public API**: `open_native` (stream.rs:463) is `pub(crate)` |
| — | Exact-key `&self` point lookup, no open scan | O0 | **EXISTS**: `VarveIndexedReader::lookup` / `get` (indexed.rs:210, 225‡). No range or prefix API (disk_index.rs:2164-2207‡), and `encode_disk_key` is little-endian (disk_index.rs:1161-1170‡) against redb's lexicographic order, so a channel prefix range is unsortable, not merely unimplemented |
| — | Zero-copy payload window | O0 + mmap | **EXISTS**: `MmapPayloads::payload_window` (file.rs:1541) — sub-slicing faults only touched pages, so it is a genuine extraction primitive, but it needs the O(N) `scan_on_open` index, is bounded by the mmap limit, and its unsafe precondition (file.rs:2061, 5107) is false for an appending file |

**What the user can build with what ships today plus P1 alone**, without enabling any format option:

**First, the constraint revision 3 broke.** §7.8 declares it non-negotiable that a handle reachable from more
than one place is read only through `read_exact_at`, never through `try_clone_file` + seek. Revision 3 then wrote
its "smallest shippable increment" as a loop that iterates `reader.events()` **while** issuing `read_payload_range`
on the same reader. `events()` builds a `NativeStreamScanner` whose `from_snapshot` does
`snapshot.try_clone_file()?` and keeps the duplicate as `file: File` (file.rs:8330-8345, struct at 8297-8305),
and passes `&mut self.file` into `read_record_entry_at` (file.rs:8187-8195), which reads through that cursor.
`try_clone_file` is `self.file.try_clone()` (snapshot.rs:70) and on Windows a duplicated handle shares one file
pointer, which `seek_read` (snapshot.rs:257-262) moves. **That is the banned pattern, in the recommended path.**
The rule and the recommendation cannot both stand as written. Both are repaired:

- **The rule stands** — it is the conservative reading, and §7.8 is honest that the exact `seek_read` cursor
  semantics were not compile-verified under this task's read-only constraint.
- **The shipping-today form of the loop uses two handles.** Two `VarveStreamReader`s over the same path hold two
  independent `File`s and share no cursor, so the enumerator and the positional reads cannot interfere. That
  costs one extra open and one extra sidecar read transaction, and nothing per record.
- **Stage 1, not Stage 3, ports `NativeStreamScanner` and `read_stream_entry_at` off `try_clone_file`** onto
  `read_exact_at`. Revision 3 deferred that port to Stage 3 while recommending the one-handle loop in Stage 1;
  moving it forward is what makes the single-handle form legal, and §16 is restaged accordingly. This does make
  Stage 1 larger than revision 3 claimed, and the claim "smallest shippable increment" is qualified in §16.

```rust
// User-side channel extraction, given planar payloads. Needs P1 + P2 only.
// TWO handles until the NativeStreamScanner port lands (see above): `scan`
// owns the enumerator's cursor, `reads` issues only positional reads.
let scan  = VarveStreamReader::open(spec, path, StreamOptions::default())?;
let reads = VarveStreamReader::open(spec, path, StreamOptions::default())?;
for event in scan.events()? {                    // EXISTS today, header-only, no payload read
    let event = event?;
    if event.block_id != DATABLOCK_ID { continue; }
    let mut cet = [0u8; 32 + 32 * MAX_CH];
    reads.read_payload_range(&event, 0, &mut cet[..32])?;        // P1
    if &cet[..4] != b"VCET" { continue; }        // pre-cutover block (10.3)
    let n = u32::from_le_bytes(cet[8..12].try_into().unwrap()) as usize;
    reads.read_payload_range(&event, 32, &mut cet[32..32 + 32 * n])?;  // P1
    let (off, len, crc) = find_channel(&cet, n, MY_CHANNEL);
    reads.read_payload_range(&event, off as u64, &mut samples[..len])?; // P1
    verify_crc32(&samples[..len], crc)?;                                 // P2
}
```

That costs the `events()` walk plus 2–3 small preads and one extent pread per datablock. **The header walk is
N discrete unbuffered reads, and in fact two syscalls per record, not one:** `read_record_entry_at` does
`file.seek(SeekFrom::Start(offset))?` and then `read_record_header(file, ..)` on the scanner's raw `File`
(file.rs:8208-8209). It is *not* buffered — `SnapshotFile`'s 64 KiB `BufReader` belongs to `cursor_at`
(snapshot.rs:60-68), which this path does not use. So revision 3's `N × 32 B` charge was right in bytes and
understated in syscalls, and O4's M × 32 B chain therefore buys more than a re-derivation might suggest (C8).
This is also independent confirmation that §7.8's ban is well founded: the scanner is a literal seek-then-read
pair on a duplicated handle. Either way the walk is O(N) in *records seen*, not in bytes read, and it needs no
directory, no chain and no schema change. **This is the smallest shippable increment, once the
`NativeStreamScanner` port above is counted as part of it.**

**Without P1 they cannot.** The only workaround is to take `event.payload_offset` / `payload_len` and open a raw
`std::fs::File`, which bypasses every snapshot bound, every read limit, every checksum and the generation witness.
That is not an API.

---

## 12. Case table, annotated with the options each case requires

Notation: N total records, D datablocks, M metablocks, C channels, S samples/channel/block, W sample width,
D_g datablocks per directory group. "Memory" is peak resident beyond the caller's output buffer. **O1** =
interleaved payload with a CET, **O2** = planar payload. **A case needing an O(N) walk is not served.**

| # | Case | Options required | API | Complexity | Memory | Verdict |
|---|---|---|---|---|---|---|
| C1 | All of CH1 from the start | **O2** (O1 gives decode-only) | `extents_from(0)` + `read_extent_into` | **With O2 alone** (the `selective` set): forward walk — the header stream plus **2 preads/block** (CET + extent), no descent, no BDR. **With O2 + O5**: 1 descent total + 1 BDR read per D_g blocks, replacing the header walk. Either way O2 moves `D·S·W` bytes (1/C) against O1's `D·C·S·W` | O(1) | **Served with O2 alone** — revision 3's complexity column quoted descent and BDR machinery for a row whose "options required" said O2 only. The 1/C win does not need O5. With O1: logically served, **full I/O**, decode saving only. With neither: whole-payload reads only |
| C2 | Global slice `k..k+n` of CH1 | **O2 + O4 + O5** | `read_into(k, ..)` | ≤ 75 CHK skip reads cold / ~15 warm + O(log D_g) BDR probes + ⌈n/S⌉ extent reads | O(1) | **Served, with the §5.2 amplification stated**: ~336 KiB device traffic for an 800 B cold window. Without O5: **not served** |
| C3 | Zero-copy slice, fixed width | **O2 + O7** (+ sealed file, aligned) | `extent_slice` | 0 copies, page faults only | O(1) | **Served.** **Not served under O1**: no contiguous `&[S]` exists |
| C4 | Datablock-ordinal range | **O5**; scan range also needs **O9** | `set.datablock(o)` | O(log D) + O(range) | O(1) | **Served.** Time ranges are **not** offered: no record carries a timestamp (file.rs:706) |
| C5 | CH1 + CH3 in one pass | **O1 or O2** | `set.co_iter(&[1,3])` | O2: 1+j preads/block, adjacent extents coalesced; O1: free relative to C1 | O(j) | **Served.** O1's only favourable case |
| C6 | All channels of one datablock | **O1 or O2** | `set.datablock(o)` | **1 pread of the payload, then C zero-I/O sub-slices — identical under O1 and O2** | O(P) | **Served.** This is why O2 has no read-side downside |
| C7 | Per-channel sample counts | **O5** | `view.len()` | 1 tail lookup + 1 pread of the CHK | O(C) | **Served only because the CHK counter is FILE-CUMULATIVE** (§8.3.4). Group-local counters would make this an O(D/D_g) walk |
| C8 | Metablocks only | **none** for N × 32 B; **O4** for M × 32 B | `events()` filter, or P4 chain | `events()` (EXISTS, feature-gated): N × 32 B of headers, **unbuffered, one `seek` + one read per record** (file.rs:8208-8209 — the 64 KiB `BufReader` at snapshot.rs:60-68 belongs to `cursor_at`, which this path does not use), so 2N syscalls. P4 chain: M × 32 B, independent of D. Resident `blocks<T>()`: Θ(N) open + 104·N | O(1) | **Served today at N × 32 B with no options at all; improved to M × 32 B by O4**, and the syscall gap is wider than the byte gap — 2N against 2M. **Precondition: metablocks and datablocks are distinct `block_id`s** |
| C9 | Metablock ↔ datablock correlation | **O8**; reverse lookup also needs **O5** | `Extent::epoch` | datablock→epoch O(1); epoch→metablock O(log) with the CHK epoch column, O(M) without | O(1) | **Served in one direction O(1), the other O(log)** — not "O(1) both" |
| C10 | Tail-follow one channel | **O6** (+ O1/O2 to have channels at all) | `refresh` + `iter_from_cursor` | O(1)/poll + new data | O(1) | **Served on the stream path with P3** — and P3 is not thin (§7.8, §11). **Not served** on resident `VarveFile` or on the matrix (bitmaps resident at open, matrix.rs:6161; `cell_status` never touches disk, matrix.rs:3053) |
| C11 | Channel present in only some blocks | **O1 or O2** | `channel(id)` → `Option`, per-block skip | 1 CET pread (≤ 32+32C B), **0 extent preads** for absent blocks | O(C) | **Served** — CET at a fixed offset, fixed-size header, entries keyed by stable `channel_id`. Kills the matrix outright (dimensions fixed at create, matrix.rs:2468) |
| C12 | Ragged blocks | **O1 or O2** | any of the above | **0 extra bytes** — `sample_count`/`byte_len` are per entry | — | **Served**, and the §10 examples honour it (buffer sized from the extent). Exactly what `ChunkedBytes` forbids (chunks.rs:200) |
| C13 | N threads, N channels, one handle | **O1 or O2** (+ P3 for follow) | §10.2 | linear speedup | O(threads) | **Served.** Every read `&self` over `Send+Sync` `SnapshotFile`; §9.5 specifies that no lock is held across I/O. Requires the §7.8 rule |
| C14 | Reads after mutation | **O5** (this is where the hazard lives) | witness + validated hop | O(1) | O(1) | **Served as fail-loud** — but the witness alone is not enough. `replace_fixed` (file.rs:3653) leaves offsets intact; `replace_block` translates later index-entry offsets (file.rs:919, 3492) **but never payload content**, so directory pointers are backward deltas with a validated landing site (§8.3.2). `replace_rewrite` is **refused outright for footer specs** (file.rs:3898) |
| C15 | User implements extraction themselves | **none** — P1 + P2 only | `events()` + P1 + P2 | O(N) headers + 3 preads/block | O(1) | **Served after P1 + P2**, using the already-public `events()` as the enumerator (§11). **Not served today**: C2, C4, C7, C10 are unbuildable on the current public API; C1/C3 only on a sealed ≤ mmap-limit `scan_on_open` file |

### 12.1 Cases consciously declined — no option buys these

1. **1/C I/O on an interleaved payload.** False (§5). Equally false for the scan-major matrix. This is the stated
   consequence of O1, not a gap in it.
2. **A flat 1/C claim even under O2.** True only for spans above the §5.2 crossover.
3. **Retrofitting selectivity onto existing interleaved files by indexing.** Only a planar rewrite or an offline
   transcode works.
4. **A cheap cold random 100-sample read.** ~336 KiB of device traffic for 800 B (§5.2). Batch instead.
5. **O(1) global sample→offset without persisted structure.** Variable-length append-only; raggedness removes even
   the uniform shortcut.
6. **In-place channel insertion.** Append-only; `replace_block` rewrites O(bytes after); whole-file rewrite is
   refused for footer specs (file.rs:3898).
7. **Global reorder / sort.** `collect_merged_keyed_values` holds the whole result set in RAM (file.rs:7429).
8. **Cross-generation offset stability.**
9. **Zero-copy on an actively appending file.** The mmap precondition (file.rs:2061, 5107) is false by
   construction.
10. **Matrix as an unbounded stream store**, and **live tail-follow of a matrix**.
11. **Tail-follow on resident `VarveFile`.**
12. **Per-channel prefix range scan over the sidecar** (little-endian `encode_disk_key`‡ vs redb's lexicographic
    order; and no range API is exposed).
13. **Sub-chunk follower latency** (§9.4).
14. **Turning O4 or O5 on for an existing pinned spec without a transcode** (§4, §17).

---

## 13. Interactions

### 13.1 Metablock scoping (O8) — one direction is O(1), the other is not

Nothing in the format expresses "this metablock applies to those datablocks". Two shapes:

(a) **Implicit half-open interval** `[meta.record_offset, next_meta.record_offset)` — 0 bytes, but reverse lookup
is O(M) and **it breaks under compaction and under channel-set changes**.

(b) **Explicit `epoch: u32`** in the CET header, incremented whenever a metablock is emitted — **0 extra bytes
(the field is already in the fixed 32 B header), 0 syscalls, 0 allocations**, and it survives compaction because
the epoch travels with the data. This is what O8 does.

- **datablock → epoch: O(1).** The epoch is in the CET you already read.
- **epoch → the governing metablock's bytes: not O(1).** You still have to find the metablock whose ordinal is
  *k*. Without help that is the P4 chain walk, **O(M)**.

**The fix, at 12 B per metablock and zero cost per datablock, and it needs O5:** the CHK carries a small trailing
`epoch_starts: [(epoch u32, meta_back_delta u64)]` listing the metablocks that began in that directory group.
Epochs are monotone, so the existing skip-list descent binary-searches by epoch exactly as it does by ordinal.
**epoch → metablock becomes O(log D_g-groups)**, the same descent cost as any other seek. The scoping rule must be
published verbatim in the format documentation.

### 13.2 Changing channel sets

Handled by construction at the block level under O1/O2: `extent_count` is per block, entries are keyed by **stable
`channel_id`**, and `channel(id)` returns `Option`. A channel appearing at block 10⁶ and vanishing at 2×10⁶ costs
one CET read per block outside its span and nothing more. Per-channel counters in the CHK become sparse — which is
precisely why `first_sample` is **per channel**, never a shared scan index.

**At the directory level (O5) it is not free.** The BDR carries one prefix column per channel for a whole group.
§8.3.4 states the rule: **the writer closes the group whenever a block's channel set differs from the group's**, so
every BDR has a single well-defined channel set and needs no sentinels. Cost: one extra directory record per change
(200 B + 2 syscalls), and changes are rare. Without this rule the directory quietly reintroduces a
fixed-channel-count assumption.

### 13.3 Ragged blocks

Zero extra cost under O1/O2 (§8.2, case C12). **The API must never expose a "samples per block" constant** — and
§10's examples obey that rule, sizing every buffer from `extent.sample_count`.

### 13.4 A scan / time axis (O9)

`block_scan_base: u64` in the CET header and the epoch in the BDR entry give **block-ordinal** and
**block-scan-ordinal** ranges. **Time** ranges are not offered: no record carries a timestamp (`RecordIndexEntry`,
file.rs:706). Adding `start_time` alongside `block_scan_base` costs 8 B/block and 0 syscalls, and it is the
difference between block-ordinal ranges and true time ranges — a further format addition, and a further option row
if it is wanted.

### 13.5 Keyed blocks and tombstones

Datablocks are **not keyed**: keying them creates one sidecar `LATEST_TABLE` row per record (indexed.rs:257‡),
unbounded at TB scale (§8.5). Metablocks may be keyed — they are rare. Tombstones are irrelevant to unkeyed
datablocks. `keyed_blocks::<T>()` (file.rs:4219) decodes **every** matching record and every tombstone in one
pass; it must never appear on a channel path.

### 13.6 Replacement, rewrite, compaction, generation change

- `replace_fixed` (file.rs:3653): offsets unchanged, chain intact. Nothing breaks.
- `replace_rewrite`: **refused outright** for footer specs (file.rs:3898). Since O4 forces a footer
  (format.rs:1992-1994), **enabling O4 removes whole-file replace entirely** for stream files. That is a real cost
  of that row and it belongs in the "does NOT give" column. Upside: `rewrite_record_streaming`'s unconditional
  `prev_same_block_offset = None` (file.rs:6031) can therefore never destroy a live chain.
- `replace_block` (file.rs:3492): translates every later **index-entry** offset (`translate_record_offset`,
  file.rs:919). `translate_record_offset` touches index-entry fields and record headers; it cannot reach payload
  bytes, and rewriting embedded payload offsets would mean re-encoding and re-checksumming every directory record
  after the replacement point — a different and far heavier operation. **This design therefore stores no absolute
  offsets in payloads at all** (§8.3.2): backward deltas are invariant under a uniform shift, and the single
  straddling hop is caught by validating the landing site. `replace_block` needs **no** new obligation.
- **Compaction** (file.rs:7395): rebuilds the file, so directories are rebuilt and the chain regenerated from
  `BlockTails`. `collect_merged_keyed_values` holds the whole result set in RAM (file.rs:7429) — compaction is
  **not TB-scale**, independently of this design. Dropping datablocks would break implicit interval scoping;
  explicit epochs (O8) survive.
- **Generation change** (`replace_path_atomically`, file.rs:10207): raw absolute offsets survive only where
  explicitly translated; the witness makes this safe (§9.4).

### 13.7 mmap and zero-copy

O7 is feature-gated (`mmap`, `zero-copy`), off by default, offered only over a sealed frontier-bounded map (§10).
Not the main path.

### 13.8 Alignment

8-byte record-start alignment (≤ 7 B inter-record padding, ≤ 0.005 % at 128 KiB blocks) makes zero-copy `&[f64]`
extents deterministic rather than luck-dependent, given the enforced `payload_offset = record_offset + 32`
(file.rs:5620) and a CET whose size `32 + 32·C` is already a multiple of 8. Full 4 KiB device-page alignment
(≤ 4095 B, ≤ 3 %) would additionally guarantee one page per small query — which, per §5.2, is where the
small-window cost actually lives. Both are values of the same `record_align:` clause; the costs are in O7's row.

### 13.9 Which subsystem

The **scalable stream** subsystem, exclusively (§9.1). This design does **not** unify resident, scalable and
matrix. The resident path stays the small-file convenience API it is; the matrix stays the preallocated grid it is.

### 13.10 Where the new attributes are rejected

`key_index` is the precedent for the attributes and also the precedent for *refusing* them: it is rejected on
matrix blocks with an explicit error (macros/lib.rs:1930-1935) and on unkeyed blocks (1962-1968), and gated on a
feature (1969-1974). Revision 3 borrowed the shape and never wrote the rejection rules, which would have left
`channels = planar` on a `fixed` or `matrix` block undefined. The full table, each row a compile error with its
own fixture in `varve/tests/ui/`:

| Declaration | Verdict | Why |
|---|---|---|
| `channels` on a **matrix** block | **Rejected** | A matrix block's payload is a preallocated slot grid addressed by `SLOT_STRIDE` (matrix.rs:2365 etc.); there is no payload prefix to hold a CET and no per-block channel set. The matrix already has a channel axis, and §7.1 explains why it is the wrong one |
| `channels` on a **fixed** block | **Rejected** | A fixed block's payload width is a compile-time constant derived from its field types; a CET plus per-block-varying extents is variable-length by construction. `variable` only |
| `channels` on a block whose fields are not **exactly one `ChannelPayload<T>`** | **Rejected** | §3.1. Any other shape would mean the CET shares the payload with ordinary encoded fields at an offset the reader cannot compute with one positional read |
| `extent_integrity`, `channel_epoch`, `scan_axis` **without** `channels` | **Rejected** | §1.2 coupling 1. They are `const` overrides on `VarveChannelBlock`, and there is no impl to override |
| `channels` on a block with a per-block `BlockCompressionDescriptor`, or in a format with `compression:` set | **Rejected** | §1.2 coupling 5 |
| `directory: seek(commit)` without `index: [block_offset_chain]` | **Rejected at spec validation** | §1.2 coupling 3, following `keyed_offset_chain requires block_offset_chain` (format.rs:1907) verbatim |
| `directory: seek(commit)` on a stream with no state store | **Rejected at open**, not at compile time | §1.2 coupling 6 — whether a state store is attached is a runtime property of `StreamOptions`, not of the spec |
| `channels` under a **custom or none layout preset** | **Rejected** | Follows the existing rule that a custom preset rejects `checkpoint_on_flush`, the chains, embedded manifests and compression (format.rs:2022-2029). O4/O5 are already unavailable there; O1/O2 join them rather than being the one exception |
| `key = [..]` **plus** `channels` | **Allowed but warned** | Not unsound, but §13.5 shows why keying a datablock is a mistake at TB scale (one sidecar `LATEST_TABLE` row per record) |

Whether the attributes ship feature-gated as `key_index` does (macros/lib.rs:1969-1974) is not settled by any
precedent I could find in the repo — there is no written policy for when a new DSL option must be gated. It is in
`openQuestions`.

### 13.11 The one matrix change worth making regardless of every option above

Replace `seek` + `read_exact` in `matrix::read_cell` (matrix.rs:3013) with `snapshot.read_exact_at`. That makes
`read_matrix_cell` `&self` with no other change — `matrix_cell_status` beside it is already `&self`
(file.rs:4692) — removing a C3 violation and a Windows cursor hazard (§7.8), for about five lines. Unrelated to
whether the matrix ever serves channels, and unrelated to every option in §1.

---

## 14. Cost

Not sold. Every line is a real charge. §1 carries the same numbers per option; this section carries them per
subsystem.

### 14.1 Append path

| Item | Option | Cost |
|---|---|---|
| Per sample | O1/O2 | 1 address computation + 1 store — **identical to interleaved**. Adds 0 alloc, 0 syscall |
| Per sample, if hardware DMAs interleaved | O2 | +1 load/store pair, ~1–2 ns |
| Per sample, if channel rates differ | O2 | +8 B moved at block close, ~1 % of a core at 32 MB/s |
| Per datablock | O1/O2 | CET back-patch: `32 + 32·C` B (**160 B** at C = 4) into an owned buffer. Adds 0 alloc, 0 syscall |
| Per datablock | O3 | C CRC32 passes covering the payload once, over cache-hot bytes: ~131 K cycles per 128 KiB block = **~1 % of one 3 GHz core at 32 MB/s** with hardware CRC |
| Per record | O4 | 32 B footer on every record of every block type + 1 `set_tail`, O(log B) (file.rs:1158) |
| **Per commit** (not per chunk, not per record) | O5 | Build BDR + CHK: O(D_g) over already-recorded descriptors ⇒ O(1) amortised per record. **+1 `seek` + 1 `write_all` (2 syscalls)**, **+1 heap allocation for the borrowed-`bytes` buffer** (§8.7; reducible to one for the file's lifetime with a reused scratch `Vec`), +2 `set_tail`, +2 `advance_coverage_with_tail` (O(1)‡) |
| **Syscall overhead, by commit cadence** | O5 | D_g = 1: **+200 %**. D_g = 8: +25 %. D_g = 32: **+6.3 %**. D_g = 256: +0.8 %. §8.7 |
| Resident writer state added | O5 | 40 B skip pointers per chained block id + `(16 + 8C)·D_g` reused descriptor bytes (~1.5 KiB at D_g = 32, C = 4). Nothing scaling with N |
| **Pre-existing, NOT fixed and NOT caused by any option here** | — | 1 `Vec<u8>` allocation per record in `prepare_stream_user_record` (file.rs:8472, struct at 8462-8469) + a second copy into the chunk buffer‡ — so **C1's "no per-record heap allocation" is already violated today**; and the O(chunk_len²) `stage_state_records` scan (stream.rs:1406-1409) |

### 14.2 File size

At C = 4, S = 4096, W = 8 (128 KiB payload):

| Structure | Option | B/datablock at D_g = 32 | B/datablock at D_g = 1 |
|---|---|---|---|
| CET (`32 + 32×4`) | O1 or O2 | 160 | 160 |
| BDR per-entry (`16 + 8×4`) | O5 | 48 | 48 |
| BDR header + CHK (`32 + 168` ÷ D_g) | O5 | 6.25 | 200 |
| Record footer, charged on **every record of every type** (format.rs:1992-1994) | O4 | 32 | 32 |
| Inter-record padding | O7 | ≤ 7 | ≤ 7 |
| **Total, all options on** | | **~246 B = 0.19 %** | **~440 B = 0.34 %** |
| **Total, `selective` set (O2+O3+O8)** | | **160 B = 0.12 %** | 160 B = 0.12 % |
| **Total, nothing on** | | **0** | **0** |

Size is a non-issue at every cadence **at C = 4**. Revision 3 stated that unconditionally; §14.2.1 shows where it
stops being true.

### 14.2.1 At C = 64 — where the design's own recommendation changes shape

§5.2 says the 1/C win "matters almost immediately" at 64 channels and that the *magnitude* of the recommendation
does not survive unchanged. The cost tables were never restated there, so here they are. Held fixed: 128 KiB
payload, `f64` samples, D = 32. At C = 64 with the same block size, S drops to 256 samples/channel/block.

| Quantity | C = 4 | C = 64 | Comment |
|---|---|---|---|
| CET (`32 + 32·C`) | 160 B, **0.12 %** | **2 080 B, 1.6 %** | 13× the headline figure. Still small in absolute terms, no longer negligible |
| BDR per-entry (`16 + 8·C`) | 48 B | **528 B** | dominates the directory |
| CHK (`32 + 40 + 24·C`) | 168 B | **1 608 B** | ÷ D = 32 ⇒ 50 B/block |
| Directory total per block at D = 32 | 54 B | **578 B** | |
| **All options on, per block** | 246 B = **0.19 %** | **2 690 B = 2.1 %** | an order of magnitude, and the first cost in this document that a user might actually weigh |
| §5.2 crossover (samples/block below which O1 and O2 tie) | 128 | **8** | O2 helps at almost any span |
| Write cursors | 4 — free | 64 — **above the fill-buffer count**, needs the tiled transpose (§8.6) | the per-sample "0" becomes "+1 load/store pair per sample" |
| O3 CRC | ~1 % of a core at 32 MB/s | ~1 % — **unchanged**, it covers the payload once regardless of C | |

Two honest conclusions. **The read-side case for O2 gets stronger with C** — the crossover collapses to 8 samples
and the scan saving goes to 1/64. **The write-side and on-disk cases get weaker** — 2.1 % on disk and a mandatory
transpose pass. At C = 64 the design still recommends O2, but "costs nothing" is no longer the right summary;
"costs ~2 % of the file and one extra pass over each sample" is.

### 14.3 Memory

Reader: `SnapshotFile` + one redb read transaction + O(C) channel metadata + the skip-node cache (**~2.9 MiB at
1 TiB, bounded and explicit, O5 only**, §9.5) + an optional off-by-default CET LRU (160 KiB) + the caller's buffer.
**Independent of file size and of query span.** Writer: unchanged buffering (§8.6) + 40 B of skip pointers +
~1.5 KiB of group descriptors under O5, and literally nothing under O1/O2 alone.

### 14.4 Code complexity

- **P1 `read_payload_range`**: small, but it publishes a bounded positional-read capability and needs a documented
  integrity contract, a rejection of `RECORD_FLAG_COMPRESSED` records (file.rs:43), and bounds
  `offset + len ≤ payload_len`.
- **CET encode/decode + writer scatter helpers (O1/O2)**: moderate, self-contained.
- **BDR/CHK emission (O5)**: cheap on the plumbing — two `prepare_*` wrappers over the generic
  `prepare_stream_record`, following the `CREATION_NONCE` precedent (file.rs:8574, appended via
  `append_prepared`, stream.rs:1229) — and **expensive on the risk**, because the emission point sits between the
  native write and `commit_state_chunk` (stream.rs:1432) and must be correct under the rollback and poison paths.
  Highest-risk item in the plan; staged last but one.
- **P3 (`scanner_at`, O6)**: **not small.** Requires porting `read_stream_entry_at` (file.rs:8412) and
  `NativeStreamScanner` (file.rs:8297, 8332) off `try_clone_file` + seek onto `read_exact_at` (§7.8). C10 and C13
  depend on it.
- Chain traversal (P4), channel-set filter (P5): each small.
- Zero-copy slices (P7, O7): unsafe-adjacent, narrow, last.
- **Macro surface:** four new per-block attributes and two new format-level clauses, each following the
  `key_index` / `index:` precedent (macros/lib.rs:1875-1921, 1454-1502), each with a compile-fail test for the
  coupling rules of §1.2 — the crate already carries such tests (varve/tests/compile.rs:54, 63-65).
- Ongoing: two internal record types, an epoch semantic, a generation witness in the public API, and a documented
  backward-delta invariant that any future replacement path must not break (§8.3.2, §13.6) — permanent maintenance
  surface, but only for users who turn O5 on.

---

## 15. What it does not solve

1. **Existing interleaved files.** Never channel-selective below full-file I/O, with or without options. Remedies:
   an offline transcode — which must produce a *new* file, since whole-file replace is refused for footer specs
   (file.rs:3898) — or accepting O1's decode-only savings.
2. **Time queries.** No record carries a timestamp (file.rs:706). Block-ordinal and block-scan-ordinal ranges
   only (O9), unless §13.4 is adopted.
3. **Sub-chunk follower latency.** Bounded below by the writer's sidecar commit cadence.
4. **Cheap cold small random reads.** §5.2: ~336 KiB device traffic for 800 B. Mitigated by the skip-node cache to
   ~96 KiB warm, never eliminated.
5. **Enabling any of O1, O2, O4, O5, O7 on an existing pinned spec.** O1/O2 move the hash through the field type
   (macros/lib.rs:752 → format.rs:1863); O4 and O5 through the index-policy byte (format.rs:2396-2405 → 1809);
   O7 through framing. New file, or transcode. §17.
6. **The default feature build.** Everything TB-scale rides `high-cardinality-dev` (`default = []`,
   varve/Cargo.toml:29-34). Without promotion, no option in §1 that touches the stream path has a shipping vehicle.
7. **The two pre-existing append-path defects** (§7.7). Neither caused nor fixed by any option here.
8. **HDD tiers.** O2's win assumes NVMe/SSD (§5.1).
9. **Compaction at TB scale.** `collect_merged_keyed_values` holds the result set in RAM (file.rs:7429).
10. **Cross-generation cursors.** A generation change forces a re-open, by design (§9.4).
11. **The matrix subsystem**, other than the `&self` fix (§13.11).
12. **The sidecar half of open-time cost is asserted, not measured** (§7.6).
13. **Recording `block_offset_chain` without also recording `scan_on_open`.** Not expressible in the DSL, and
    not expressible usefully in Rust either, since the flag is inert (§2.3). Cosmetic.
14. **Channel selectivity on compressed blocks.** `channels` and `compression:` are mutually exclusive
    (§1.2 coupling 5). Per-extent compression via the CET's `encoding` field is expressible but not designed here.
15. **The unconditional `load_index` in `VarveFile::open*`** (file.rs:2798, 2844, 2884, 2935), which is what makes
    the resident path Θ(N) at open. A separate defect, fixable independently, and a dependency of nothing in §1.

---

## 16. Staging

Ordered so that each stage ships a usable option set. **Stage 1 is schema-hash-neutral; Stages 2, 3 and 4 are
not** — revision 3 said Stages 1–2 were neutral, which was wrong once `ChannelPayload<T>` is the field type (§4).

**Stage 0 — prerequisite owned by the defect-fix workflow, not by this design.**
Fix `stage_state_records`' O(chunk_len²) scan (stream.rs:1406-1409) with one reverse pass. *Blocks O4 and O5.*

**Stage 1 — P1 + P2 + the scanner port. Ships the O0-plus-primitives configuration.**
A public `&self`, zero-allocation, bounds-checked, caller-buffered positional payload read backed by
`SnapshotFile::read_exact_at` (snapshot.rs:74), plus the per-extent CRC convention that makes a partial read
verifiable. Paired with `events()` (stream.rs:521) this **unblocks the entire user-implements-it path (§11) on its
own**, with no format change and no schema-hash movement.

**This stage also carries the `NativeStreamScanner` port that revision 3 deferred to Stage 3.** §11's recommended
loop interleaves `events()` with positional reads on one handle, and `NativeStreamScanner` holds a
`try_clone_file` duplicate it reads through a cursor (file.rs:8297-8345, snapshot.rs:70) — the pattern §7.8 bans.
Either the port lands here, or Stage 1 ships with the documented two-handle workaround and the port moves to
Stage 1b. It is still the smallest useful increment; it is not as small as revision 3 said.
*Depends on nothing.*

**Stage 2 — `ChannelPayload<T>`, the `channels` attribute (O1 + O2) and `extent_integrity` (O3).**
The codec and its `SCHEMA_ID`; CET encode/decode; writer scatter and reader gather helpers; the
`VarveChannelBlock` trait (§3.1); the macro attributes, the `decode`/`selective` set idents (§1.3), and the
rejection fixtures of §13.10. No change to framing, index or sidecar. **Moves the schema hash** — the field type
changes — so this is a new-file or unpinned-spec stage, and §4.1's Rule 1 applies to nothing here because no
`FormatSpec` policy field is added. After this, channel-selective reads work end to end, one datablock at a time,
with seek by walking. **This is the stage that answers "can I read only CH1" with yes.** *Depends on Stage 1.*

**Stage 3 — O4 read side: P3 + P4 (`scanner_at`, chain traversal), and O6.**
Resume a scan at a saved offset; follow `prev_same_block_offset` backward. Smaller than revision 3 estimated *if*
Stage 1 carried the scanner port; otherwise it carries it. Improves C8 as re-derived there, and makes tail-follow
(C10) possible. **Moves the schema hash** via the index-policy bit — new files only, §17. *Depends on Stage 0.*

**Stage 4 — O5, the BDR + CHK seek directory.**
Delivers O(log) seek (C2, C4), O(1) `len()` (C7) and O(log) epoch reverse lookup (C9). Highest risk: surgery on
the durability-critical commit path (§14.4) and the **permit-factoring problem of §3.4**, which is the largest
single piece of that risk. Carries the §8.7 syscall arithmetic — **`directory: seek(commit)` should not be used
below D_g = 8, and the writer should warn below it** (the DSL cannot reject it, because commit cadence is a
runtime property of the writer, not of the spec). Also adds the first new `FormatSpec` policy field, so **§4.1's
Rule 1 applies here** and the hash-neutrality test lands in this commit. *Depends on Stages 0, 2 and 3.*

**Stage 5 — the `ChannelView` public API (§10) and `refresh`.**
Wraps stages 1–4 into the ergonomic surface, including `read_extent_into` as the primary scan primitive.
*Depends on 1–4.*

**Stage 6 — O7 (`record_align` + zero-copy slices); and separately P6, the `&self` matrix fix.**
P6 is independent of everything else and can land any time. *O7 depends on Stage 5.*

---

## 17. Compatibility and migration, per option

§4 has the table. This section has the procedures.

**Add with no hash movement and no reader break — read-side only:**
- P1, P5, P6. These publish or fix capabilities and change no bytes.
- O6 (tail-follow), which is a reader capability.

**Moves the computed schema hash, and therefore requires either a new file or an unpinned spec.** Revision 3 put
O1/O2/O3/O8/O9 in the group above; they belong here, because the payload field's type changes (§3.1, §4):
- **O1 / O2**, via `Vec<T>` → `ChannelPayload<T>`: the type spelling (macros/lib.rs:752), both codec
  `SCHEMA_ID`s, and `field.wire_type` (format.rs:1863, 1874).
- **O3, O8, O9** — not independently, but they cannot be enabled without O1/O2, so in practice they arrive with
  a moved hash.
- **O4** (`block_offset_chain = true`; format.rs:2396-2405 → 1809), and **O5** via its O4 dependency — **not**
  via BDR/CHK themselves, which live in the reserved id range (§8.3.1).
- **O7** (`record_align`), because padding is framing.

So: **every option in §1 that changes bytes on disk moves the hash.** That is a stronger and simpler statement
than revision 3's, and it is the correct one. It also means the "cut over mid-file" story only exists for specs
whose hash is *not* pinned.

**The migration procedures, in order of cost:**

1. **Create new acquisitions with the options you want.** There is no option that can be turned on later for free.
   Deciding at creation costs nothing; deciding afterwards costs a transcode. If the acquisition software may
   later want channel selectivity, `channels = selective` at creation is the cheap insurance — its steady-state
   cost is 160 B per 128 KiB block.
2. **Run the `selective` set with no framing options.** O1/O2/O3/O8/O9 plus the §10 read-side surface work with
   no chain and no directory: the CET gives channel selectivity at the full 1/C I/O saving (§10.1 shows the
   forward-walk scan), `events()` gives enumeration, seek degrades from O(log) to a forward walk, and
   `ChannelView::len()` is not offered. Everything in §11 still holds. **This is a genuine shipping
   configuration**, not a degraded fallback. It does not serve C2, C4 or C7 at TB scale.
3. **Cut over mid-file — only with an unpinned schema hash.** If the spec leaves `schema_hash:` unset (literal 0,
   open-time equality disabled), a writer may start emitting `ChannelPayload` blocks into an existing file and a
   reader handles the mix through `BlockChannels::Opaque` (§10.3). This is a real option and it is also a loaded
   gun: the same unpinned hash that permits the cutover permits a genuinely wrong spec to open the file. Prefer 4.
4. **Transcode.** Read the old file, write a new one with the new spec. Required in any case for existing
   interleaved payloads (§15.1), and impossible to do in place because `replace_rewrite` is refused for footer
   specs (file.rs:3898).

**The one invariant future maintainers must not break:** no directory record may ever store an absolute file
offset in its payload. Backward deltas only, with a validated landing site (§8.3.2). `replace_block` translates
index-entry offsets and record headers, never payload bytes, and it does not bump the generation — so a violation
of this rule produces a **silent misread**, not an error.
