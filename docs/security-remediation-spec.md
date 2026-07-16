# Security Remediation Specification

Status: Superseded by `runtime-limits-replacement-spec.md`

> Historical design record. Its mandatory finite declaration limits and
> tightening-only runtime policy are no longer current. Limits are optional
> operational defaults, ordinary APIs resolve safe one-shot limits at runtime,
> append totals are uncapped by default, and `*_with_resource_limits` may set
> the policy for a particular open/create operation.

## Objective

Resolve every actionable item in
`docs/adversarial-security-review-2026-07-10.md` without changing valid v0.1
wire bytes, schema hashes, or append durability. Make hostile-input resource
ceilings declarable by format authors and automatically enforced by generated
readers. Preserve an explicitly named expert path for coordinated in-place
replacement while making the ordinary fixed replacement snapshot-preserving.

## Non-Goals

- Cryptographic authentication, encryption, or anti-replay protocols.
- Live tailing, async I/O, multi-writer support, or immutable VMAT generations.
- Guessing one universal domain limit for every format.
- Treating custom codecs or unsafe raw-layout implementations as untrusted.
- Changing existing valid encoded block, record, footer, manifest, checkpoint,
  compression, custom-layout, or VMAT bytes.

## Frozen Compatibility Direction

- Ordinary open requires every limit applicable to that storage preset. Missing
  ceilings fail with `MissingResourceLimit` before attacker-sized work.
- Existing trusted-input use remains available only through explicitly named
  `*_trusted_unbounded` APIs or a declaration that visibly selects the trusted
  unbounded policy.
- Security-sensitive formats can declare limits or apply a runtime-tightened
  `FormatSpec` before open.
- Limits are runtime policy and do not participate in the schema hash or file
  manifest.
- Bytes emitted by canonical writers remain unchanged.
- Previously accepted non-canonical bytes may be rejected where the writer could
  never have emitted them.
- Pre-public Rust APIs may change when needed to make the safe default honest.
- The implementation adds no dependency and uses only `std` for snapshot I/O
  and resource enforcement.

## Public API

Add one non-exhaustive, copyable `ReadLimits` policy. Do not introduce layered
format/open/matrix limit types. A limit is either missing, finite, or explicitly
trusted-unbounded. Runtime limits tighten declaration limits by field-wise
minimum and cannot silently widen them.

The scalar state is explicit: `Missing`, `Finite(u64)`, or
`TrustedUnbounded`. Zero is a valid finite ceiling. Ordinary entry points accept
only `Finite`; named trusted entry points treat `Missing` and
`TrustedUnbounded` as unbounded while still enforcing any finite fields. The
component-wise meet is `min` for two finite values, finite wins over either
unbounded state, and no operation can turn a finite value into an unbounded one.

The policy contains at least these ceilings:

- `max_file_len`
- `max_records`
- `max_index_bytes`
- `max_scan_bytes`
- `max_record_payload_len`
- `max_logical_payload_len`
- `max_materialized_bytes`
- `max_segments`
- `max_matrix_dimension`
- `max_matrix_cells`
- `max_matrix_bitmap_bytes`
- `max_matrix_crc_bytes`
- `max_matrix_metadata_bytes`
- `max_matrix_slot_region_len`
- `max_sidecar_len`
- `max_mmap_len`

`FormatSpec` stores `read_limits` and exposes `with_read_limits`. Generated
formats expose `open_reader_with_limits`, `open_writer_with_limits`, and the
corresponding recovery helper where meaningful. Generated and low-level APIs
also expose clearly named trusted-unbounded entry points; ordinary open never
falls back to them.

Format-first syntax:

```text
limits {
    file_len: 8_589_934_592;
    records: 4_000_000;
    index_bytes: 536_870_912;
    scan_bytes: 8_589_934_592;
    record_payload: 67_108_864;
    logical_payload: 268_435_456;
    materialized_bytes: 1_073_741_824;
    segments: 4_000_000;
    matrix_dimension: 16_000_000;
    matrix_cells: 16_000_000;
    matrix_bitmap: 64_000_000;
    matrix_crc: 128_000_000;
    matrix_metadata: 268_435_456;
    matrix_slot_region: 8_589_934_592;
    sidecar: 268_435_456;
    mmap: 8_589_934_592;
}
```

Every key relevant to the selected native, custom-layout, or matrix preset must
be finite for ordinary open. Unknown or duplicate keys are compile errors.
Trusted inputs use an explicit `limits: trusted_unbounded;` declaration and the
generated `*_trusted_unbounded` methods; omission is not an implicit policy.

### Limit Accounting

| Limit | Unit and scope | Earliest enforcement point |
| --- | --- | --- |
| `max_file_len` | bytes in the opened or prospective published file | immediately after metadata, or before writer growth |
| `max_records` | all native user and internal records in one file snapshot | before index insertion/append |
| `max_index_bytes` | cumulative resident bytes for native index/checkpoint entries | before reserve/decode/copy |
| `max_scan_bytes` | cumulative physical bytes advanced during one open/scan | before advancing to the next record/segment |
| `max_record_payload_len` | stored bytes for one record or whole layout region read | after header/range decode, before allocation |
| `max_logical_payload_len` | decoded/decompressed bytes for one record | after length hint/envelope decode, before allocation |
| `max_materialized_bytes` | cumulative logical payload bytes consumed by one keyed, metadata, migration, merge, compact, or diagnostic materialization | before each payload read |
| `max_segments` | custom-layout segments in one snapshot | before segment-info insertion |
| `max_matrix_dimension` | value of one runtime dimension | after the small dimension table is decoded |
| `max_matrix_cells` | derived cells for one matrix block/category and aggregate checked product | before bitmap/slot derivation |
| `max_matrix_bitmap_bytes` | aggregate resident commit/write/quarantine bitmap bytes, counting every owned copy | before bitmap reads or allocation |
| `max_matrix_crc_bytes` | aggregate on-disk and resident matrix CRC/validity bytes | before CRC-region reads/allocation |
| `max_matrix_metadata_bytes` | aggregate non-slot matrix descriptor/metadata bytes | before descriptor-table reads |
| `max_matrix_slot_region_len` | aggregate preallocated slot bytes | after checked dimension products, before create/open |
| `max_sidecar_len` | complete matrix sidecar bytes | after sidecar metadata, before body read |
| `max_mmap_len` | mapped bytes for one mmap owner | before mapping |

Every multiplication/addition is checked before comparing with a ceiling.
Claim-sized collections use `try_reserve`; allocation failure is a normal Varve
error. Index accounting uses the resident entry representation, not only wire
checkpoint bytes. Materialization accounting spans all input shards in one
operation.

### API-To-Limit Matrix

| Surface | Required finite limits |
| --- | --- |
| native open/writer-open/recovery | file, records, index, scan, physical payload, logical payload |
| native create/append/checkpoint | file, records, index, physical payload, logical payload |
| typed block get/migration | physical payload, logical payload, materialized bytes |
| keyed/metadata/merge/compact/diagnostics | physical payload, logical payload, materialized bytes, records |
| custom-layout open | file, scan, segments, index bytes |
| custom-layout whole/range read | physical payload; range calls also check requested length |
| matrix create/open | file, matrix dimension, cells, bitmap, CRC, metadata, slot region |
| matrix aux read/write | physical payload and file length |
| matrix sidecar | sidecar length and materialized bytes |
| native/matrix mmap | file, mmap length, records, index bytes, plus applicable matrix limits |

Generated APIs enforce the same matrix. Writer operations check the prospective
post-operation state before writing, including checkpoint and manifest records.
Trusted-unbounded APIs are visibly named at the format, reader/writer, layout,
matrix, recovery, and mmap entry boundaries; an ordinary method cannot call one
internally.

## Snapshot-Bound I/O

- `VarveFile` owns a positional-read handle bound to the object opened and
  validated. It must not reopen the pathname for indexed reads.
- `BlockVec` and `KeyedBlockVec` carry an `Arc<File>` snapshot handle, not a
  `PathBuf` used for I/O.
- Metadata, schema, migration, keyed state, diagnostics, merge, compact, and
  checkpoint consumers read through the bound handle.
- `LayoutReader` retains the opened file handle and performs positional range
  reads against it.
- The same captured handle propagates through `metadata`, `all_metadata`,
  `schema_manifest`, `blocks`, `blocks_migrated`, `keyed_blocks`,
  `materialized_keyed_blocks`, `key_tail_offsets`, diagnostics, merge, compact,
  native rewrite, and every generated wrapper over those methods.
- Sidecars are separate objects and are each opened exactly once per operation;
  no operation may combine a header from one sidecar generation with a body
  reopened by path.
- Public path-based `RecordIndexEntry` methods remain explicitly low-level and
  non-snapshot; generated and high-level APIs do not use them.
- Positional read helpers use checked offsets and work on Windows and Unix
  without sharing a mutable cursor.
- Checksum-enabled lazy reads recompute the captured record checksum and fail
  closed if the same open object was modified. Without checksums, external
  same-object immutability remains a documented storage contract.

After pathname replacement, an existing reader returns bytes from the original
object. An old reader must never consume the newly named object.

## Replacement Semantics

- `replace_fixed` becomes snapshot-preserving. It streams the original object
  into a same-directory temporary file, patches the same-size fixed record,
  syncs, and atomically replaces the path.
- Previously opened readers retain the original object and value.
- The implementation supports native records both with and without record
  footers. Same-size fixed replacement preserves record offsets and footer
  chains while updating the record sequence and checksum.
- The operation must not materialize the complete file in memory.
- Existing variable-size `replace_rewrite` is converted to a streaming rewrite
  and remains unsupported only where rebuilding its footer/offset contract is
  not yet defined.
- Retain `unsafe replace_fixed_in_place_exclusive` only as an expert API. Its
  safety contract requires exclusion across every reader, writer, mmap, raw
  reference, handle, thread, and process for the complete operation and lifetime
  of any affected view. No safe strategy variant or generated default selects
  it; the old safe `ReplaceStrategy::FixedInPlace` is removed before release.

The copy-on-write algorithm is normative:

1. Hold the existing single-writer lock and capture the original index/file
   length.
2. Validate fixed kind, exact payload size, sequence availability, all limits,
   and the source snapshot before creating output.
3. Create a unique same-directory temporary file and stream-copy the complete
   original generation with bounded memory.
4. Patch only the target record header and same-size payload. Record offsets and
   all footer bytes/chains remain unchanged. Recompute the record checksum over
   the new canonical header/payload and unchanged footer according to the active
   integrity policy.
5. Flush and `sync_all` the temporary file, reopen/validate its header, index,
   checksums, uniqueness, and limits, then atomically publish it.
6. On Unix publish with same-filesystem rename. On Windows use the existing
   `ReplaceFileW` path and file opens that permit delete sharing. If any external
   handle denies replacement, return the OS error, keep the original generation
   and current writer handle unchanged, and remove the temporary file when
   possible.
7. Only after publication succeeds replace the writer handle/index and publish
   the new sequence in memory.

Before atomic publication the original path remains authoritative. After a
successful publication the replacement path is authoritative; already-open
snapshot handles retain the original object. Durability is no weaker than the
existing atomic rewrite contract: file contents are synced before publication,
while unsupported directory-metadata sync is not claimed.

## Sequence Canonicality

- A native file may contain non-monotonic physical sequence order because a
  same-position replacement can receive the newest sequence.
- Sequences must nevertheless be unique within one native file snapshot. Open
  rejects duplicates with a dedicated error; same-file ties are never resolved
  as compatibility behavior.
- Uniqueness is the `u64 sequence` alone across user and internal records in one
  file; physical ordinal does not participate. Each base/delta shard validates
  its own uniqueness before cross-shard ordering.
- All keyed, metadata, tombstone, op, merge, and compact decisions use one shared
  order: shard ordinal, sequence, then physical record ordinal.
- Equal-sequence defensive behavior remains consistent even if a compatibility
  mode is added later.

## Resource Enforcement

### Native open and typed reads

- Reject `file_len`, record count, physical payload, and compressed logical
  length before allocating or indexing beyond their limits.
- Compute record CRC through a bounded streaming buffer.
- Validate commit marker size before reading it.
- Check checkpoint payload and entry count against both byte and record budgets.
- Generated collections, migration, metadata, merge, compact, and diagnostics
  apply `FormatSpec::read_limits` automatically.
- Use fallible reservation for attacker-sized collections where practical.

### Matrix

- Decode the small dimension table first.
- Check every derived cell count, aggregate bitmap size, slot region, CRC table,
  and file length before reading or creating derived vectors.
- Matrix aux reads and sidecar reads enforce the applicable limit.
- Avoid duplicate bitmap buffers when ownership is unnecessary; otherwise count
  every owned copy in the limit calculation.

### Custom layout and mmap

- Custom layout open enforces file and segment count budgets.
- Whole metadata/raw reads enforce the record-payload ceiling; range reads check
  the requested length.
- Mmap constructors reject mappings beyond `max_file_len` and indexes beyond
  `max_records` before building copied lookup structures.
- Unsafe external immutability requirements remain unchanged.

### Lock and sidecar files

- Lock metadata has an internal small fixed maximum and is parsed through a
  bounded reader.
- The default refuse policy does not parse an existing lock unless diagnostics
  are explicitly requested.
- Matrix sidecar length is checked from metadata before body allocation.

## Strict Decode Hardening

- Convert variable field `u64` lengths with `usize::try_from`.
- Reject duplicate known variable fields.
- Reject every nonzero variable field flag, including flags on unknown fields,
  until a defined flag exists.
- Reject nonzero reserved native file-header flags.
- Require internal key/op payloads to end exactly after their declared value.
- Require strictly increasing input keys for `BTreeMap` and `HashMap`; add `Ord`
  to the `HashMap` decode bound and use one pending entry so validation remains
  O(n) without cloning or sorting. Canonical writer bytes remain unchanged.

## Recovery Classification

- Add a narrow internal `RecoverableTail` classification rather than a new
  public recovery architecture.
- Recoverable input is limited to an incomplete/torn final append or an
  uncommitted suffix after the latest fully validated boundary permitted by the
  existing truncate-tail policy.
- Limit, overflow, canonicality, duplicate-sequence, schema/header, and
  committed-prefix checksum failures are fatal and never authorize truncation.
- Recovery uses the same bound handle and effective limits as ordinary open.

The scanner maintains `observed_eof`, `validated_prefix_end`, and the latest
fully validated transaction-marker end. It mutates the file only after the
prefix has been validated and one of these states is reached:

| State/error | Read-only behavior | truncate-tail recovery behavior |
| --- | --- | --- |
| fewer than a complete final header remain at observed EOF | expose validated committed prefix | truncate to the applicable validated boundary |
| checked record end exceeds observed EOF (partial payload/footer) | expose only a previously marker-covered prefix where transaction policy permits; otherwise error | truncate to the applicable validated boundary |
| complete valid records exist after the latest transaction marker but no later marker commits them | expose latest marker-covered prefix | truncate to latest marker end on writer recovery/open policy |
| checksum mismatch in an uncommitted suffix after a validated marker | expose latest marker-covered prefix | truncate to latest marker end |
| malformed complete header/footer, canonical error, duplicate sequence, limit/overflow, schema/version/endian error, committed-prefix checksum failure, or non-EOF I/O error | fatal, file untouched | fatal, file untouched |

Invalid footer magic/version/flags and complete malformed lengths are not
classified as torn merely because they occur last. Limit checks happen before
any truncation decision.

## Performance Contract

- Bound handle-based positional reads should remove the per-record pathname open
  from lazy collection reads.
- CRC open uses constant auxiliary memory.
- Snapshot-preserving fixed replacement is O(file size) by policy; the explicit
  exclusive in-place API remains O(record size).
- Sequence uniqueness validation must be O(n log n) with at most 8 bytes of
  temporary sequence storage per indexed record, or O(n) with measured and
  documented memory.
- Matrix checks are O(number of descriptors) before any large allocation.

Run and record existing perf smoke/bench results for scan, typed iteration,
keyed materialization, CRC open, matrix open, copy-on-write replacement, and
exclusive in-place replacement. No hot path may regress unexpectedly without a
documented policy reason.

The reproducible release gate uses baseline revision `a1843e1`,
`target/release/examples/perf_bench.exe 10000`, one discarded warmup, five
measured runs, and the median wall time. The pre-change medians on the review
host are:

| Metric | Median ms | Five-run range ms |
| --- | ---: | ---: |
| encode/decode fixed | 0.685 | 0.683-0.708 |
| encode/decode variable | 4.567 | 4.498-4.633 |
| append fixed | 120.066 | 116.653-126.322 |
| open/scan fixed | 39.645 | 38.495-42.052 |
| merge keyed files | 885.567 | 874.434-904.206 |
| compact merged | 693.973 | 670.536-705.526 |
| compact base+deltas | 882.096 | 864.031-907.436 |

An unchanged metric whose post-change median is over 15% slower is a blocking
regression unless a focused profile demonstrates environmental variance or an
approved security cost. The all-features ignored `perf_smoke` large dataset is
also run before and after for typed lookup, materialization, layout, matrix,
mmap, and zero-copy trends. New focused timings compare copy-on-write fixed
replacement with full rewrite, and the unsafe exclusive in-place path only with
its former O(record) behavior; those policy-different paths are not compared to
ordinary lazy reads.

## Tests

Required regression tests:

- pathname replacement after open retains original native and layout objects;
- fixed copy-on-write replacement preserves old reader values and exposes the
  new value only to a newly opened reader;
- exclusive in-place replacement is clearly tested as non-snapshot-safe;
- duplicate sequences are rejected and all conflict APIs share ordering;
- sparse/large claimed native payloads fail on limits before allocation;
- CRC open succeeds through streaming and detects corruption;
- matrix dimension, cell, bitmap, slot, aux, and sidecar limits fail before
  derived allocation;
- generated declarations parse every limit key and reject duplicate/unknown
  keys;
- generated typed reads enforce declared and runtime-tightened limits;
- lock parsing rejects oversized input without reading it wholly;
- variable duplicate fields, nonzero flags, trailing internal bytes, and
  32-bit-overflowing field lengths are rejected;
- mmap/zero-copy existing soundness tests remain green;
- all existing valid roundtrips and external TDMS/BMP harnesses remain green.

## Mechanical Acceptance

- `cargo fmt --all -- --check`
- `cargo check --workspace --all-targets --all-features`
- `cargo test --workspace --all-features`
- `cargo clippy --workspace --all-targets --all-features -- -D warnings`
- `cargo audit`
- `cargo deny check`
- performance smoke and benchmark comparison
- `git diff --check`
- documentation synchronized across architecture, spec, API reference, format
  author guide, durability model, performance guide, and security review

## Release Gate

No public hostile-input or immutable-snapshot claim is restored until all high
findings have regression tests and the independent implementation verifier has
accepted the integrated diff.
