# Varve Performance Checks

## Purpose

Performance checks are regression guards, not product benchmarks. Run them whenever a change touches indexing, scanning, codecs, compression, commit/offset-chain footers, replacement, recovery, merge, compact, mmap, or zero-copy paths.

## Smoke Commands

```powershell
cargo test -p varve --test perf_smoke -- --ignored --nocapture
cargo test -p varve --features compression-zstd --test compression -- --nocapture
cargo test -p varve --features mmap --test perf_smoke -- --ignored --nocapture
cargo test -p varve --features mmap,zero-copy --test perf_smoke -- --ignored --nocapture
cargo test -p varve --all-features --test perf_smoke -- --ignored --nocapture
```

## Benchmark Example

For a local release-mode timing pass without adding benchmark dependencies:

```powershell
cargo run -p varve --example perf_bench --release -- 10000
```

The optional numeric argument is the record count. This example reports
encode/decode, append, open/scan, merge, compact, and direct base+delta compact
paths.

## Reproducible Comparison Protocol

For security or storage changes, build once, run one unrecorded warmup, then
record five executions of `target/release/examples/perf_bench.exe 10000`.
Compare medians and retain the five-run range. Run on the same machine, power
profile, feature set, and storage volume. The pre-remediation baseline is:

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
