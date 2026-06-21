# Varve Architecture Outline

## Summary

- Varve is not one fixed binary format. It is a Rust library and macro system for defining append-friendly, segment-based custom binary formats.
- The next storage wave adds `preallocated_matrix` beside the existing append
  log so bounded in-place matrix datasets can be hosted without changing
  append-log semantics.
- The workspace starts with three crates: `varve`, `varve-core`, and `varve-macros`.
- The v0.1 model is synchronous I/O, single-writer per file, and snapshot-read.
- The first stable goal is a conservative owned-decoding core, with mmap and zero-copy as explicit opt-in features.

## Core Model

- A block is the logical segment unit.
- `varve_format!` is format-first by default: it can generate block structs, typed reader/writer wrappers, and the static format registry from one declaration.
- `LayoutSpec` is the physical layout layer. The default preset is the current
  Varve-native container; custom layouts can own byte zero and declare
  file headers, append-oriented segment lead-ins, metadata regions, raw
  regions, footer descriptors, and finalized/backpatched offset fields.
- `FormatSpec::effective_layout()` is the bridge between the native preset and
  the custom layout DSL. Native formats expose a synthetic `VarveFileHeader` and
  repeated `VarveRecord` segment plan; custom formats expose their declared
  layout parts directly.
- The native file header, record lead-in, and optional `VARVE3` footer share an
  internal native layout codec with those synthetic native plans, so the default
  preset is no longer only a documentation projection for framing.
- `#[derive(VarveBlock)]` remains available for derive-first block definitions when structs need to live outside the format declaration.
- Block ids are explicit `u32` values below `0xFFFF_FF00`; higher ids are reserved for internal records.
- Same-type blocks are read lazily through `BlockVec<T>`.
- Keyed blocks are exposed through `KeyedBlockVec<K, T>` for put-only lookup and `materialized_keyed_blocks::<T>()` for put/op/tombstone state.
- `VarveReader` and `VarveWriter` provide clearer read-only and write-capable handle workflows while preserving the lower-level `VarveFile` API.
- Fixed blocks use canonical field encoding by default, not Rust memory layout copying.
- Variable blocks use `field_id + wire_type + length + payload`, so unknown field ids with known wire types can be skipped.
- Variable user blocks may opt into compression after canonical field encoding through a global policy or block-specific record-explicit descriptors. Fixed blocks and Varve internal records stay uncompressed.
- `VARVE3` record footers carry commit/offset metadata when `CommitPolicy` or offset-chain `IndexPolicy` requires it.
- `LayoutWriter` and `LayoutReader` provide the first non-native physical path
  for optional file header + repeated segments. The initial verified targets are
  TDMS-style `TDSm` lead-in + metadata bytes + contiguous raw channel bytes, and
  a generic framed layout with declared file header and segment footer.
- Matrix blocks use deterministic slot layout and commit bitmaps instead of
  append-log record footers or offset chains.
- Endian priority is block override, then format setting, then the macro little-endian default.

## Compression Policy

- Compression is opt-in through `CompressionPolicy::VariableBlocks`.
- The initial backend is `zstd`, exposed behind the optional `compression-zstd` feature.
- Metadata can be stored per record (`RecordExplicit`), in a `VARVE2` file header extension (`FileExplicit`), or in the static format contract (`FormatContract`).
- `BlockCompressionDescriptor` can opt one registered variable block id into
  compression even when the global policy is `None`; this override is limited
  to `RecordExplicit` envelopes.
- `RecordExplicit` uses a small `VCMP` envelope with algorithm, level, and `u64` uncompressed length.
- `FileExplicit` and `FormatContract` use raw compressed payloads plus the record header `uncompressed_len_hint`; these modes require the maximum logical payload size to fit `u32`.
- `FormatContract` requires the pinned `schema_hash` to equal `computed_schema_hash()` so the compression contract cannot be accidentally reused across incompatible specs.
- Physical APIs expose compressed bytes. Typed collections, migrations, and merge/materialization return logical decompressed values.

## Reader And Writer Policy

- Writers are append-oriented and protected by a sidecar writer lock.
- Default open/create/recover paths refuse an existing writer lock.
- Explicit stale-lock handling is available through `FormatSpec::inspect_writer_lock` and `FormatSpec::open_with_lock_policy`.
- Readers are snapshot-on-open. Live tailing is out of scope for v0.1.
- Durability is explicit: `flush` pushes buffered bytes to the OS, and `sync` performs durable fsync.
- `replace_fixed` is allowed only when the canonical payload size is unchanged.
- `replace_rewrite` performs a same-directory temp rewrite, syncs, and atomically publishes the new file.
- `replace(index, block, ReplaceStrategy)` exposes the selected replacement policy.
- `CommitPolicy::RecordFooter` uses the record footer as the per-record commit flag.
- `CommitPolicy::TransactionMarker(on_flush|explicit)` exposes only marker-covered snapshots; read-write open truncates uncommitted tail after the latest marker.

## Update, Merge, And Compact

- Base+delta shard workflows are supported for keyed block types.
- Deltas can append user-defined `VarveMerge::Op` records and common tombstone deletes.
- Conflict order is shard ordinal, then local sequence, then record ordinal. Later delta shards win over earlier shards and base.
- `merge_keyed_files::<T>(spec, base, deltas, output)` materializes final keyed values and writes an output file.
- `compact_keyed_file::<T>(spec, input, output)` compacts one file into final keyed values only.
- `compact_keyed_files::<T>(spec, base, deltas, output)` compacts base plus ordered deltas directly, avoiding an unnecessary merge-then-compact intermediate file.
- Compact and merge output use same-directory temp files, flush/sync, and atomic publish.

## Schema And Compatibility

- `VarveBlock::FIELDS` records field id, field name, wire type, and required/defaulted presence.
- Embedded schema manifests are optional and diagnostic. Static `FormatSpec` remains authoritative for typed access.
- Manifest payload v4 includes extension, compression policy, commit/index/integrity/recovery/manifest policies, block descriptors, and field descriptors. v1-v3 manifests decode with missing newer fields defaulted.
- `FormatSpec::computed_schema_hash()` computes a deterministic schema fingerprint excluding the pinned header `schema_hash`.
- `FormatSpec::schema_debug_dump()` emits a human-readable view for inspection and support.
- Migration is explicit with `VarveMigration<From, To>` and `blocks_migrated::<From, To, M>()`.
- After v0.1, wire format changes should require a migration path.

## Codec Policy

- Encoding is canonical and endian-aware.
- Supported core shapes include scalars, option, fixed arrays, selected vectors, tuples, `BTreeMap`, and `HashMap`.
- `HashMap` encoding sorts cloned keys before writing, so equivalent maps produce stable bytes regardless of insertion or hash iteration order.
- Raw zero-copy representation is never the default codec.

## Optional Mmap And Zero-Copy

- `mmap` exposes read-only payload windows through `VarveFile::mmap_payloads()`.
- `MmapPayloads` owns a cloned snapshot index and rejects forged public index entries.
- `zero-copy` implies `mmap`.
- `VarveRawFixedBlock` is an unsafe opt-in trait for raw fixed blocks whose implementor promises layout, endian, and alignment compatibility.
- `MmapPayloads::raw_fixed::<T>()` validates registration, fixed kind, version, endian, payload size, and alignment before returning a raw reference.

## Performance Guardrails

- Every major implementation slice must include a runnable performance check before integration.
- `crates/varve/tests/perf_smoke.rs` is the current smoke suite and is intentionally ignored by default.
- The suite covers small, medium, and large cases for append/open/scan, checkpoint open, materialized keyed state, merge, compact, direct base+delta compact, recovery, mmap payload windows, matrix direct access, matrix aux regions, and zero-copy raw fixed reads.
- It also covers custom physical layout append and open/scan so layout DSL
  changes expose obvious framing or scan regressions.
- The purpose is regression detection, especially accidental O(n^2) scans, excessive allocation, or unexpected slow open/merge/compact paths.
- Performance smoke output is not a product guarantee before stabilization, but a large unexplained slowdown blocks integration.

## Dependencies And License

- Direct dependencies are permissive OSS candidates: `syn`, `quote`, `proc-macro2`, `thiserror`, optional `crc32fast`, `memmap2`, `zerocopy`, and `zstd`.
- Test dependencies include `trybuild` and `proptest`.
- The current direct and transitive dependency graph has been audited from `cargo metadata --all-features` and exposes commercially usable permissive license choices.

## Current Status

- Core runtime, macros, manifest, migration scaffold, checkpoint index, recovery, writer lock metadata, merge/compact, global and block-specific variable-block compression, mmap, zero-copy, read/write handles, property tests, compile tests, performance smoke tests, benchmark example, and practical guides are implemented.
- User-facing docs now include quickstart, API reference, implementation model,
  format-author, self-check, durability, recovery, migration, performance, and
  requirements-boundary guides.
- Matrix/preallocated storage now has a P0 implementation for dense direct
  addressing, commit bitmaps, same-size overwrite, generated DSL helpers, and
  performance smoke coverage. P1 matrix CRC is implemented for `integrity:
  crc32` metadata tables, commit maps, and per-cell slots, including commit-map
  rebuild from CRC evidence, injectable ordered durability barriers, and an
  optional `integrity`-gated sidecar identity/CRC envelope. Safe matrix
  recovery clear actions are public. P2 now includes static noncommit aux
  regions, safe mmap numeric scalar reads, and compatible cell byte-copy
  migration scaffolding. `ChunkedBytes` provides chunked zstd + per-chunk CRC
  for caller-managed blobs. VMAT-native chunk compression and bulk migration
  publication remain next-wave work.
- Remaining polish areas include stricter manifest validation if needed, richer error ergonomics, and future live tailing if it becomes a requirement.
