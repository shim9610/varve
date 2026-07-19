# Varve Performance Checks

## Purpose

Performance checks are regression guards, not product benchmarks. Run them whenever a change touches indexing, scanning, codecs, compression, commit/offset-chain footers, replacement, recovery, merge, compact, mmap, or zero-copy paths.

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
- `merge_keyed_files`
- `compact_keyed_file`
- direct base+delta `compact_keyed_files`
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

The smoke suite reports wall time, records/sec, and file size. Compare small, medium, and large cases. A suspicious change is usually visible as large-case growth that is much worse than the smaller cases, especially on open/scan, typed ordinal lookup, materialized keyed state, merge, or compact.

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
