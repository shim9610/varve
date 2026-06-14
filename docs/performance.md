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
