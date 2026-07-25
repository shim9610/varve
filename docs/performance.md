# Varve Performance Checks

## Purpose

Performance checks are regression guards, not product benchmarks. Run them whenever a change touches indexing, scanning, codecs, compression, commit/offset-chain footers, replacement, recovery, merge, compact, mmap, or zero-copy paths.

**These checks run in no CI job.** `crates/varve/tests/perf_smoke.rs` is entirely
`#[ignore]`d, as is the million-key stress probe in `high_cardinality.rs`. Every
number in this document comes from a manual run on a named date and host; nothing
here is continuously enforced. The measurements dated 2026-07-22 in the Capability
Boundary section below are the most recent, and cover only the matrix residency
and concurrent-read contracts.

## Smoke Commands

```powershell
cargo run -p varve-test-runner -- test -p varve --test perf_smoke -- --ignored --nocapture
cargo run -p varve-test-runner -- test -p varve --features compression-zstd --test compression -- --nocapture
cargo run -p varve-test-runner -- test -p varve --features mmap --test perf_smoke -- --ignored --nocapture
cargo run -p varve-test-runner -- test -p varve --features mmap,zero-copy --test perf_smoke -- --ignored --nocapture
cargo run -p varve-test-runner -- test -p varve --all-features --test perf_smoke -- --ignored --nocapture
```

## Benchmark Example

For a local release-mode timing pass without adding benchmark dependencies:

```powershell
cargo run -p varve-test-runner -- run -p varve --example perf_bench --release -- 10000
```

The optional numeric argument is the record count. This example reports
encode/decode, append, open/scan, merge, compact, and direct base+delta compact
paths.

Read the throughput column with its unit (BENCH-01). Codec, append, and
open/scan lines are per **record**, and `records` really is their denominator.
Merge and compact are per **input event**: their input is the base file plus
every delta record — updates, deletes, and inserts — and their output is the set
of surviving live values, which is smaller than both. Each of those three lines
also prints the live values emitted, and the run reopens the file that line
produced and asserts the printed number against the blocks actually
materialized in it. Reporting all three against the original base record count,
as earlier revisions did, produced a rate that was neither the events consumed
nor the values emitted; historical tables below predate the fix, so their
merge/compact throughput figures should be read as elapsed time only.

## Reproducible Comparison Protocol

For security or storage changes, build once, run one unrecorded warmup, then
record five executions of `target/release/examples/perf_bench.exe 10000`.
Compare medians and retain the five-run range. Run on the same machine, power
profile, feature set, and storage volume. Record `powercfg /getactivescheme`
with Windows results; a different active scheme invalidates a direct timing
comparison. The pre-remediation baseline is:

| Metric | Median ms | Five-run range ms |
| --- | ---: | ---: |
| encode/decode fixed | 0.685 | 0.683-0.708 |
| encode/decode variable | 4.567 | 4.498-4.633 |
| append fixed | 120.066 | 116.653-126.322 |
| open/scan fixed | 39.645 | 38.495-42.052 |
| merge keyed files | 885.567 | 874.434-904.206 |
| compact merged | 693.973 | 670.536-705.526 |
| compact base+deltas | 882.096 | 864.031-907.436 |

An unchanged metric whose median regresses by more than 15% blocks integration
unless a focused profile attributes the difference to environmental variance
or an explicitly accepted security cost. Also run the all-features ignored
smoke test so mmap, zero-copy, compression, custom layout, and matrix paths are
not hidden by the core benchmark.

The post-remediation run on 2026-07-10 used the same release binary and record
count, one unrecorded warmup, and five recorded runs:

| Metric | Median ms | Five-run range ms | Change from baseline |
| --- | ---: | ---: | ---: |
| encode/decode fixed | 0.694 | 0.683-0.709 | +1.3% |
| encode/decode variable | 4.846 | 4.652-6.240 | +6.1% |
| append fixed | 133.964 | 120.965-182.452 | +11.6% |
| open/scan fixed | 23.105 | 22.881-26.058 | -41.7% |
| merge keyed files | 204.660 | 199.184-240.220 | -76.9% |
| compact merged | 193.961 | 191.500-199.659 | -72.1% |
| compact base+deltas | 202.936 | 198.407-223.292 | -77.0% |

The append median remains below the 15% integration gate. Its single high run
is retained in the range so later measurements can distinguish scheduler or
storage variance from a repeatable regression.

## Same-Machine Security Comparison (2026-07-11)

Absolute wall times above are historical observations, not portable baseline
values. A later check produced roughly two-times-larger absolute timings on the
same machine while preserving the earlier relative result. To remove that
environmental mismatch, a clean pre-remediation baseline and the 0.2.0 release
candidate were rebuilt and measured back-to-back.

Environment: Windows x86_64 MSVC, Intel Core i7-13700KF, Rust 1.95.0, optimized
`release` profile, 10,000 records, one warmup, five recorded runs per pass. The
order was current, clean HEAD, current. The table uses the second current pass,
which immediately followed the clean baseline. Both binaries reported exactly
1,210,343 bytes for append output and 2,795,483 bytes for merge/compact output.

| Metric | Clean HEAD median ms | Current median ms | Change |
| --- | ---: | ---: | ---: |
| encode/decode fixed | 1.456 | 1.448 | -0.5% |
| encode/decode variable | 9.371 | 9.879 | +5.4% |
| append fixed | 274.949 | 249.259 | -9.3% |
| open/scan fixed | 72.523 | 39.708 | -45.2% |
| merge keyed files | 1,984.248 | 419.949 | -78.8% |
| compact merged | 1,751.705 | 394.197 | -77.5% |
| compact base+deltas | 2,312.464 | 433.956 | -81.2% |

This comparison does not support a broad performance regression. The variable
codec's observed 5.4% increase is below the 15% gate and is not by itself proof
that canonical flag, checked-length, or duplicate-field validation caused the
difference. Fixed codec performance was unchanged, append improved slightly,
and scan/merge/compact improved substantially. Do not remove hostile-input
validation without a focused profile and a repeatable codec-only regression.

The fuzz/ASan harness, compile-time macro-hygiene fix, and Windows fault tests
are outside production hot paths. `PublishedButRebindFailed` handling affects
only the already-published replacement rebind/error path; this benchmark does
not measure `ReplaceFileW` replacement latency.

## Runtime Policy And Resized Replacement Check (2026-07-16)

The all-feature debug smoke test was run with the primary append format's
`limits` declaration omitted. Append, open/scan, merge, compact, and recovery
completed under the runtime standard policy. This specifically guards against
reintroducing a declaration-time or default total append ceiling.

Sequence-preserving resized replacement rewrites one complete generation and
is therefore O(file size), while ordinary append remains unchanged. Observed
single-run debug timings were:

| Existing records | Resized COW replacement ms | Records/sec |
| ---: | ---: | ---: |
| 128 | 34.597 | 3,700 |
| 2,048 | 143.766 | 14,245 |
| 10,000 | 671.040 | 14,902 |

These are smoke observations, not release-mode product benchmarks. The
approximately linear large-case behavior is expected for atomic whole-file
publication. A future implementation may add an append-only replacement event
strategy, but it must preserve sequence, keyed-event ordering, snapshot, CRC,
footer-chain, checkpoint, and transaction visibility semantics.

## Capability Boundary

Performance claims are not uniform across the API. Two families exist and they
have different bounds:

| Family | Entry points | Bound |
| --- | --- | --- |
| Resident | `VarveFile`, `VarveReader`, `VarveWriter`, keyed collections, `merge_keyed_files`, `compact_keyed_file(s)` | index and working state live in memory; sized by the file's record and key counts |
| Scalable | `high-cardinality-dev` `VarveStreamWriter`/`VarveIndexedWriter`/readers with their `.vks`/`.vki` sidecars | bounded resident state; sized by declared blocks and bounded buffers, not by record or key count |

The scalable family covers bounded *ingest and lookup*. It does not cover
merge/compact: the keyed merge/compact family is resident-only and explicitly
not PB-scale (see the entries below and `docs/api-reference.md`). Matrix storage
is a third, separate mode: its integrity metadata is paged and sparse rather
than resident-per-cell. Create is bounded by live state rather than by cell
count.

**Matrix open cost depends on `ReadLimits::matrix_metadata_residency`, and under
neither policy is it `O(1)`.**

| | `EagerVerified` (default) | `Lazy { cache_bytes }` (opt-in) |
| --- | --- | --- |
| Open visits | union of the `L` pages named by the persisted page index and the `A` pages the filesystem allocation map reports as written | the persisted page index only |
| Open cost | `O(L + A)` time, `Theta(L + A)` temporary memory, up to `O(4096U)` page-byte I/O for `U` distinct candidate pages | `O(L)` — 8 bytes per live page, no page payload, no digest, no allocation-map query |
| Post-open residency | `O(L)` — only pages holding a set bit are retained, so the `A` term is I/O but not memory; fixed at open; does not track the working set; **no read-driven eviction** (a mutation clearing a page's last bit does refund it) | 0 at open, then bounded by `cache_bytes` with LRU eviction |
| `max_matrix_bitmap_bytes` behaves as | an **admission** limit — a live set above it makes open fail | a cache bound; `cache_bytes` above it is refused at open |
| Corruption detected at | open | first touch of the damaged page |

Tracking live state is the *sparse-allocation operating case*, not an
unconditional bound: a densely allocated bitmap region makes `A` proportional to
that region's page count even when few bits are live.

**Measured absolutes** (Windows x86_64, 2026-07-22, reproduced 2026-07-25,
`crates/varve/tests/matrix_lazy_residency.rs`). One commit-map page covers 32,768
cells; residency is about 8,192 bytes per live page (one commit page and one
CRC-validity page) plus **96** bytes of page-index overhead per live page — 48 per
entry in each of the two bitmaps, measured as 1,536 bytes at 16 live pages. With
`IntegrityPolicy::None` there is no validity bitmap and both figures roughly halve.

| Fixture | Cells | File size | Live pages | Policy | Open bytes read | Pages visited | Resident bitmap bytes |
| --- | --- | --- | --- | --- | --- | --- | --- |
| small | 2,097,152 | 17.3 MB | 1 | eager | 135,336 | 33 | 8,192 |
| large | 8,388,608 | 69.2 MB | 1 | eager | 131,240 | 32 | 8,192 |
| large | 8,388,608 | 69.2 MB | 64 | eager | 591,384 | — | 524,288 |
| small | 2,097,152 | 17.3 MB | 1 | lazy | 32 | 0 | 0 |
| large | 8,388,608 | 69.2 MB | 1 | lazy | 32 | 0 | 0 |
| large | 8,388,608 | 69.2 MB | 64 | lazy | 1,040 | 0 | 0 |

Three conclusions, none of which should be softened when this file is next
edited:

1. Open is **independent of file size** — a 4x cell-count and file-size ratio
   produced a 0.97x read ratio under the eager policy.
2. Open is **not `O(1)`** — one live page still costs 131,240 bytes and 32 pages
   under the eager policy, because `A` follows NTFS's ~128 KiB allocation runs
   rather than the live set. The gap to `O(1)` is roughly 32x in pages at the
   smallest live set. **These are NTFS numbers**: `A` is filesystem-defined, is
   absent entirely on targets with no allocation map (open then visits `L` pages
   only), and becomes the whole bitmap region on a non-sparse volume, where eager
   open degrades to `Theta(declared cells / 8)`.
3. Eager residency **does not track the working set in either direction**; it is
   fixed at open by the **live** page count (measured 131,072 bytes at 16 live
   pages, unchanged after touching 1 page and after touching 4). Nothing on the
   read path releases it. A mutation that clears a page's last set bit does refund
   that page (PERF-02).

Whole-category clear is bounded by live state **only where the platform supports
range removal**, and streams `Theta(cells / 8)` zero bytes otherwise — see the
two entries below for the exact conditions.

## Covered Paths

- fixed append, open, scan, and lazy typed lookup
- fixed and variable encode/decode
- variable-block compression read/write, block-specific compression override,
  physical scan, checkpoint, and rewrite paths
- `ChunkedBytes` chunked zstd + per-chunk CRC functional path under
  `all-features`
- checkpoint open with `IndexPolicy::CheckpointOnFlush`
- VARVE3 footer scan, transaction-marker visibility, and block/keyed offset-chain append paths
- keyed put/op/tombstone materialization
- `merge_keyed_files` (resident: `O(K-ever + largest resident input index +
  retained live values)` memory, not PB-scale)
- `compact_keyed_file` (same resident bound over its single input)
- direct base+delta `compact_keyed_files` (same resident bound)
- `estimate_keyed_merge` pre-flight sizing and the `*_with_key_limit` guarded
  variants, which fail typed at a caller-chosen `K-ever` ceiling rather than in
  the allocator
- explicit tail recovery
- streaming CRC validation without allocating a whole claimed payload
- snapshot-bound lazy reads after pathname replacement
- copy-on-write fixed replacement plus the separately unsafe exclusive
  in-place replacement path
- sequence-preserving resized replacement for grow, shrink, CRC/footer chains,
  checkpoints, transaction markers, keyed identity, and old snapshots
- mmap payload window scans
- zero-copy raw fixed reads
- matrix random-order write/read smoke paths
- matrix same-size overwrite and commit bitmap operations
- matrix preallocated aux region read/write paths
- matrix mmap direct payload reads when the `mmap` feature is enabled
- matrix mmap safe numeric scalar reads when the `mmap` feature is enabled
- matrix durable hook and checked payload view paths are covered by functional tests
- matrix CRC commit/read/rebuild paths when the `integrity` feature is enabled
- chunk compression performance tracking should be added when matrix chunk
  compression lands

## Output To Watch

The smoke suite reports wall time, records/sec, and file size (see the note
above on what the benchmark's merge/compact denominators mean). Compare small, medium, and large cases. A suspicious change is usually visible as large-case growth that is much worse than the smaller cases, especially on open/scan, typed ordinal lookup, materialized keyed state, merge, or compact.

## Scale Contracts Worth Re-checking

These are the bounds the current implementation claims. A change that violates
one is a regression even if wall time happens not to move on a small fixture.

- Matrix open visits pages derived from the persisted page index and the
  filesystem allocated-range map — never `0..page_count`. An extent cap must
  never be allowed to reinterpret a sparse file as dense.
- A commit-bit mutation rehashes exactly one 4 KiB page, and a sparse bitmap page
  whose last set bit is cleared is evicted and refunded immediately, in `O(1)` on
  the mutated page. Neither costs a scan of the page or of the map.
- Resident block-tail construction is `O(N + B log B)` time and `O(B)` memory for
  `N` records over `B` distinct block ids. The append path pays at most one
  sorted-vector insertion per distinct id for the life of a file, and no hashing
  per record.
- Keyed merge/compact is resident: time
  `Theta(records + decoded bytes) + O(N log N) + O(K-live log K-live)` and memory
  `O(K-ever + largest input index + 8N uniqueness temporary + retained live
  values)`. Size it with `KeyedMergeEstimate::peak_resident_structural_bytes()`,
  which is a structural estimate and not an upper bound (see the API
  reference). The
  `O(N log N)` sort degrades to `Theta(N)` for a file whose sequences ascend with
  offset, which is what a Varve writer produces, but it is the guaranteed bound.
- Typed block registration for an identity-bearing format takes no process-global
  lock and does no address-keyed cache lookup on the append path.
- A created pathname costs exactly one parent-directory fsync, on the first
  `sync`/`commit_durable`, and never again.

## Current Regression Rules

- No new implementation path should scan the full index once per block ordinal.
- `HashMap` encoding must sort keys once per encoded map, not repeatedly per entry.
- Mmap typed ordinal lookup must use the precomputed block-id map.
- Direct base+delta compact should avoid writing a merge output before compacting.
- Atomic publish paths may sync for durability, but should not perform per-record fsync.
- Ordinary append must not inherit a default total file, record, segment, scan,
  or index ceiling. Finite aggregate quotas are selected per handle by callers.
- Resized replacement may scan and copy one complete generation once; it must
  not introduce per-record full-index scans or affect the append hot path.
- Compression should not decompress during index scan or `scan()`. Decompression belongs on typed logical read paths.
- Block-specific compression lookup must be per-record metadata work only and
  must not trigger payload reads or full-file scans.
- Chunked blob helpers should verify per-chunk CRC while decoding requested
  chunked payloads, not during append-log index scan.
- `only_if_smaller` should compare the final stored payload size, including record-explicit envelope bytes.
- Offset-chain append should update chain tails in O(1) for generated keyed writers and should not rescan the full file per append.
- Transaction-marker open should ignore/truncate uncommitted tail without making marker-covered reads slower than a linear scan.
- Matrix direct cell read/write should be O(1) with respect to total cell count
  and must not scan sibling cells.
- Matrix aux read/write should address the declared region directly and must
  not append records or grow the file for in-bounds writes.
- Hostile-input checks must remain O(number of descriptors) before any
  claim-sized allocation. A resource ceiling must not be implemented by first
  materializing the resource it is supposed to bound.
- Snapshot-bound reads should use positional I/O on the retained handle and
  must not add one path open per lazy record.
- `CheckpointOnFlush` must space full index checkpoints geometrically. A fresh
  full checkpoint is written only once the live tail grows by
  `max(INDEX_CHECKPOINT_MIN_RECORDS, records_at_last_checkpoint / 2)`, keeping
  cumulative checkpoint bytes `O(N)` across `O(log N)` checkpoints. Per-flush /
  per-record full-index checkpoints are a blocking regression.
- The flush-time checkpoint predicate itself must be O(1). The writer carries
  incremental cadence state (eligible records since the last checkpoint plus a
  precomputed geometric threshold) maintained at the append site, restored on
  rollback, recovered once at open, and recomputed on generation rebind. A
  predicate that reverse-scans the index per flush is a blocking regression:
  it makes flush-per-record workloads O(N²) in CPU even when checkpoint bytes
  are linear. The same applies to the per-flush uncommitted-tail check, which
  is a single newest-entry inspection, not a scan.
- CRC typed reads must decode the already-read, checksum-validated payload
  buffer once. A CRC point lookup or streaming typed scan must not read the
  covered payload twice, and a typed scan must not checksum or read the payloads
  of skipped foreign-block records.
- Tombstone rebuild must resolve the descriptor once per record by block id and
  decode each tombstone key once. Rebuild cost must be independent of the plan
  descriptor count, not `O(records × descriptors)`.
- Matrix commit-bit maintenance under `integrity: crc32` must hash a bounded
  amount of bitmap per mutation. Each mutation rehashes exactly the 4 KiB page
  holding the mutated byte (less for a map shorter than one page), so write +
  commit of `M` cells hashes `Theta(M)` bitmap bytes. Any structure that
  rehashes a whole category bitmap, or recomputes a composition over all page
  digests, per mutation is a blocking regression: both make the per-mutation
  cost depend on matrix size.
- Matrix create-time metadata writes and post-open bitmap residency must not
  scale with cell count. Creation writes only descriptor tables plus the `MCRC`
  header; commit, CRC-valid, and current-write bitmaps are sparse and
  materialize a page only when it carries a set bit. Committed-cell counting
  must stay `O(1)` off the maintained set-bit totals rather than scanning.

  Not scaling with *cell count* is not the same as being small, and this contract
  has never claimed the stronger property. Under `EagerVerified`, post-open
  residency scales with the **live page count** and is never released while the
  handle lives. Only `MatrixMetadataResidency::Lazy { cache_bytes }` bounds
  residency by a declared ceiling; see the Capability Boundary table above for
  the measured numbers.

  Matrix open under `EagerVerified` enumerates the union of the persisted page
  index and the filesystem allocation map in `O(Q)` time and `Theta(Q)` temporary
  memory,
  where `Q` is the number of candidate pages — the `L` pages currently holding
  state plus the `A` pages the allocation map reports as written, so
  `Q <= L + A`. Reading those candidates costs up to `O(4096U)` page bytes for
  `U` distinct candidate pages. It is independent of the logical matrix width and
  of the number of pages the matrix has published historically. There is no sort.
  Any reintroduction of a sort, or of a scan over `0..page_count`, is a blocking
  regression.

  `Q` is a candidate count, not a live-state count. `A` follows how densely the
  file is allocated, so a densely allocated bitmap region can make open's work
  proportional to that region even when few bits are live. Open tracking live
  state is the sparse-allocation operating case, which is what this design
  targets; it is not a worst-case guarantee.

  The persisted page index is the primary source and is authoritative on its own:
  when the filesystem cannot answer an allocated-range query, enumeration still
  costs `O(live pages)`. (Before `VMAT` v3 the fallback was a full logical scan
  over every page of every map, `Theta(cells / 8)`; that behaviour is gone and
  any documentation still describing it is stale.) A never-written page is
  proved zero either by the index not naming it or by the allocation map, not by
  reading it. Every persisted-index page is verified on every platform;
  never-indexed pages are additionally checked only where the platform supplies a
  usable allocation map, so where it does not, a stray byte written out of band
  into a page the matrix never published is not detected at open. That page is
  never loaded, so it is not trusted either — the loss is corruption visibility,
  not unchecked acceptance.

  Whole-category clear removes the byte range where the platform supports it —
  Windows via `FSCTL_SET_ZERO_DATA`, Linux via `fallocate`
  `FALLOC_FL_PUNCH_HOLE` — in which case its cost is independent of the cell
  count. On every other target, and whenever the call fails (for example on a
  filesystem without sparse-file support), the range is streamed as zero bytes
  and the cost is `Theta(cells / 8)`. Callers that depend on the cheap path must
  prove it at runtime, and must respect the exact scope of the two counters that
  let them: a clear issues several range requests (validity bitmap, page
  indexes, page digests, commit map), and both counters are **thread-local**.
  Take `MatrixRecoveryReport::matrix_total_zero_range_streamed_bytes()` before
  and after the operation, on the thread performing it; a nonzero delta means
  some range had to be streamed.
  `MatrixRecoveryReport::matrix_last_zero_range_streamed_bytes()` reports only
  the most recent single request, so `0` from it does not qualify a whole clear.
  `MatrixRecoveryReport::matrix_sparse_zeroing_supported()` reports the
  compile-time platform capability only and is **not** sufficient on its own.
- Resident block-offset chaining must resolve the previous record of a block id
  from maintained sorted block tails in `O(log B)` with no resident-index reads,
  never by reverse-scanning the index (`O(N*B)`, quadratic when block ids are
  as numerous as records). The tail table is rebuilt with one forward pass
  wherever the resident index is loaded or replaced wholesale.
- Opening or invalidating a shared sidecar identity must not sweep the whole
  process-global registry every time. The registry sweeps only after it grows
  past a doubling threshold, so `S` sequential opens cost `O(S)` slot checks in
  total instead of `Theta(S^2)`.
- The stream/indexed primary-generation witness must stop recomputing once its
  bounded leading window is full: steady-state appends do zero witness work, and
  the per-create nonce costs exactly one record at create and nothing per
  append.

### Checkpoint, CRC, And Rebuild Cost Fixes

These paths were measured in the 2026-07-19 review as regressions and are now
bounded; the review numbers are before-context:

- resident `checkpoint_on_flush` grew cumulative checkpoint bytes as
  `22 + 94N + 73N²` (per-flush full serialization). Geometric spacing removes
  the quadratic term. Open/recovery is unaffected because the scan reads every
  native record and merely validates whatever checkpoints it encounters, so a
  sparse or checkpoint-less tail still recovers.
- after the byte growth was linearized, the flush predicate still reverse
  searched the index per flush (the 2026-07-19 re-review simulated 179,392
  predicate entry touches at 1,024 records vs 11,253,678 at 8,192 — 62.7x for
  8x the input). The predicate is now two scalar compares against
  incrementally maintained cadence state; a fault-injection counter test pins
  near-linear growth of index entries touched.
- CRC typed point lookup and streaming scan read each covered payload twice
  (integrity-none `4,194,416 B` vs CRC `8,388,832 B` on a 4 MiB point lookup).
  The validated payload buffer is now handed straight to typed decode, so the
  covered payload is read once; `verify_all()` remains the full whole-file
  integrity pass.
- Tombstone rebuild decoded each tombstone key once per descriptor
  (`1` descriptor `2,061,267` read-transfer bytes vs `8` descriptors
  `5,732,403`). A single descriptor lookup by block id makes rebuild cost
  independent of descriptor count.

## High-Cardinality Development Profile

The unstable `high-cardinality-dev` profile has a generated integration probe:

```powershell
cargo test -p varve --features high-cardinality-dev --test high_cardinality
```

It compares allocator peaks for repeated and unique keys, then appends 10,000
unique composite keys through the generated indexed writer,
checks representative equality lookups, and asserts that both streaming and
indexed handles report zero retained record and key-map entries. The redb
cache is fixed at 1 MiB in this probe, so increasing unique-key cardinality
must grow the `.vki` sidecar rather than a Varve `HashMap`.

Derived-index updates share bounded redb write batches after a durable dirty
marker. Previous-key lookup reads the transaction's current B-tree state, so
the batch does not require an O(K) pending map. Native framing and sidecar
updates are derived from the same checked in-memory prepared record. The native
chunk is written once and is never reread during append or `sync()`. `sync()`
syncs the authoritative native file and then durably publishes clean sidecar
metadata. Reintroducing per-record native writes, rereading appended chunks, or
scanning during ordinary open is a blocking performance regression.

The ignored million-key probe was executed on Windows in the debug profile on
2026-07-17. It completed in 61.85 seconds with a measured allocator peak delta
of 9,513,224 bytes while using an 8 MiB redb cache. Treat this as a regression
baseline for this machine, not a cross-platform throughput guarantee.

The same probe in the optimized release profile produced the following more
useful baseline:

| Operation | Result |
| --- | ---: |
| append 1,000,000 unique composite keys | 4.072 s (about 245,600 records/s) |
| native sync plus clean sidecar publication | 116.2 ms |
| clean reopen with no native record scan | 9.35 ms |
| 10,000 warm equality lookups | 141.5 ms (about 14.2 us/lookup) |
| native file | 128,000,026 bytes |
| redb sidecar | 134,746,112 bytes |
| measured allocator peak delta | 12,346,781 bytes |

The previous implementation of this same indexed probe took 14.369 seconds to
append and 3.334 seconds to reopen because it reread appended native ranges and
scanned the native file at open. Those operations are now forbidden by tests.
The current numbers are local regression measurements, not claims against
other storage engines or hardware.
