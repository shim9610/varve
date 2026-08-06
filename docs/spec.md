# Varve 0.3 Implementation Spec

## Goal

Implement the first stable core of Varve: a Rust workspace that can define typed binary block formats with macros, create/read append-friendly files, support lazy typed access, keyed merge semantics, explicit durability, recovery policy, and compile-time schema validation.

## Workspace

- `varve`: facade crate re-exporting runtime and macros.
- `varve-core`: runtime format spec, file reader/writer, codec, collections, merge, and errors.
- `varve-macros`: `#[derive(VarveBlock)]` and `varve_format!`.

## Wire Contract

- `LayoutSpec` is the physical layout layer. If no layout is declared,
  `LayoutPreset::VarveNative` is used and the existing Varve header/record
  bytes remain unchanged.
- `FormatSpec::effective_layout()` exposes the active physical plan with the
  same file-header, segment, lead-in, raw-region, and footer vocabulary for both
  native and custom formats. For `LayoutPreset::VarveNative`, the plan is
  synthetic and mirrors the existing native writer/reader byte contract without
  changing native bytes or schema hashes.
- The Varve-native file header, record lead-in, and optional record footer are
  generated from the same internal native layout field contract used by
  `effective_layout()`. The native append-log semantics remain in the native
  reader/writer; this is a parity extraction, not a semantic rewrite.
- `FormatSpec::inspect_layout_file(path)` validates an existing native or
  custom file and returns `LayoutFileInfo` with the effective plan, file-header
  length, and physical segment ranges. Native files are projected from strict
  `RecordIndexEntry` scans into the same `LayoutSegmentInfo` shape used by
  custom segment layouts.
- `LayoutPreset::None` lets a declared layout own byte zero. This is the first
  custom physical-layout path and is intended for append segment formats whose
  lead-in is not `VARVE1/2/3`, such as TDMS-style `TDSm` segments.
- Non-native layout descriptors can declare a file header, segment lead-in
  fields, metadata regions, raw regions, footer descriptors, field constants,
  caller-supplied fields, and finalized/backpatched offset fields.
- The first runtime slice supports one optional file header followed by repeated
  segments with lead-in + caller metadata bytes + contiguous raw bytes +
  optional footer. The writer backpatches finalized offsets after the segment is
  written, and the reader validates literals, finalized offsets, forward
  progress, footer bounds, and region bounds.
- File header includes user magic, Varve container marker, format version, endian, flags byte, and schema hash.
- Record header includes block id, block version, flags, sequence, payload length, checksum, and reserved bytes.
- The final record header word is `uncompressed_len_hint` for compressed records and `0` otherwise. `payload length` is always the physical stored byte length.
- `VARVE1` is used for plain records, `VARVE2` is used for file-header extensions such as file-explicit compression, and `VARVE3` is used when `record_footer`/`transaction_marker` commit policy or block/keyed offset chains are enabled.
- A `VARVE3` record has a fixed 32-byte footer after the stored payload: `b"VRF1"`, footer version, footer flags, `prev_same_block_offset`, `prev_same_key_offset`, reserved `footer_crc32`, and reserved bytes. Offset fields are `0` when absent.
- User block ids are explicit `u32` values below `0xFFFF_FF00`; higher ids are reserved for internal records. `0xFFFF_FFF7` is the segment record and `0xFFFF_FFF5` is the open digest.
- Fixed blocks use canonical field encoding, not raw Rust memory layout.
- Variable blocks encode fields as `field_id + wire_type + length + payload`, allowing unknown fields to be skipped.
- Variable user blocks may be compressed after canonical field encoding and before record write. A format may set one global variable-block compression policy, or opt individual variable block ids into record-explicit compression with `BlockCompressionDescriptor`. Fixed blocks and internal records are not compressed.
- `ChunkedBytes` is a value-level helper for caller-managed blobs. With
  `compression-zstd` and `integrity`, it stores zstd-compressed chunks with
  per-chunk CRC32 validation and can be used inside variable fields or aux
  payloads.
- `RecordIndexEntry::read_payload`, `scan()`, and mmap payload windows expose physical stored bytes. Typed reads, migrations, and merge/materialization use logical payloads and transparently decompress when needed.
- Endian priority is block override, then format setting, then little-endian default in macros.

## Preallocated Matrix Wire Contract

- Matrix support is a separate storage mode from the append-log record region.
- Files whose static spec contains matrix blocks store a 24-byte `VMNC`
  creation-nonce region and then a `VMAT` layout region immediately after the
  normal Varve file header.
- `VMAT` layout version 4 stores runtime dimensions, matrix block layout,
  commit bitmap offsets, slot region offsets, derived static aux region
  placement, optional offset-table metadata, region CRC metadata,
  `append_log_start`, and the persisted page-index region
  (`page_index_off`, `page_index_len`).
- The two page-index fields are header fields 13 and 14, appended after
  `append_log_start` so every previously defined field keeps its index; the
  reserved tail shrank from 32 to 16 bytes. The region order is
  `VMAT header | dimension table | block table | commit categories | commit maps
  | slot region | static aux | page index | MCRC | append log`.
- The page index is an array of little-endian `u64` slots, each 8 bytes. Slot `0`
  is an occupancy header: the live entry count in the low 48 bits and a check
  value derived from it in the high 16, so the header carries its own redundancy
  and a torn or corrupted count is detectable rather than believed. A matrix
  whose page count would exceed `2^48 - 1` is refused at layout time instead of
  being written with an unrepresentable header. Slots `1..=count` are the
  entries, each holding `page + 1`, so a zero entry *inside* the counted prefix
  is provably damage rather than an ambiguous end-of-array — that distinction is
  the whole reason the count exists. `u64::MAX` in the header is the
  rebuild-in-progress marker; it is not a representable valid header at any
  capacity, so a reader that encounters it fails closed rather than reading a
  half-rebuilt index as authoritative. Entries are written before the page and
  digest they describe, so the worst a torn append can do is name a page that
  still reads as uninitialised zeros, which open already accepts. Open derives
  the pages it visits from this index unioned with the allocated ranges the
  platform reports, never from the logical page count.

  Layout version 3 used a terminator-scanned array with no count field, which is
  why a v3 artifact is refused rather than migrated (see the version contract
  below): under that encoding a damaged entry was indistinguishable from the end
  of the array, so damage silently hid every later page.
- The append-log scanner starts at `append_log_start` for matrix files and at the
  normal header length for non-matrix files.
- Matrix slot payloads are fixed-stride, bounded, and addressed directly by
  deterministic key formulas such as `scan * n_channels + ch`.
- Matrix commit bitmaps are the normal read-time validity authority. Slot bytes
  are ignored when the matching commit bit is clear.
- Static matrix aux regions are declared in the format spec, preallocated after
  slot payloads, excluded from commit bitmap validity, and exposed through
  `matrix_aux_len`, `read_matrix_aux`, and writer-only `write_matrix_aux`.
- With `integrity: crc32`, `VMAT` v4 includes an `MCRC` v2 table after the slot
  region, any static aux regions, and the page index. It stores a CRC32 over the metadata tables,
  an 8-byte digest (`crc32` plus a state word) per 4 KiB page of every commit
  map, and a CRC32 plus a validity bit per dense cell slot. A commit-bit
  mutation rehashes only the affected page, and a page whose state word says
  "uninitialized" must still read as all zeros, which is what distinguishes a
  never-written page from one deliberately written with zeros.
- Matrix creation writes only the descriptor tables and the `MCRC` header; the
  commit maps, per-cell checksums, and validity bitmaps are a sparse zero
  extent, and those bitmaps are held sparsely in memory after open, so neither
  create-time metadata I/O nor post-open bitmap residency scales with cell
  count.
- A `VMAT` layout version 1, 2, or 3 artifact is refused at open with
  `Error::FormatVersionMismatch { expected: 4, actual: <1, 2, or 3> }`; it is
  stale and regenerable, not migratable. Version 1 describes the pre-paging
  physical representation, version 2 carries no page-index region, and version 3
  carries the superseded terminator-scanned page-index encoding. The `MCRC`
  integrity table remains version 2.
- P0 commit operations are logical visibility operations under the explicit
  `flush`/`sync` durability model, not implicit per-cell fsync operations.
- Same-size matrix overwrites update the existing slot range and must not change
  file length. The overwrite clears the affected commit bit until the caller
  commits the replacement payload.
- Recovery may rebuild a damaged commit bitmap only from stronger evidence such
  as per-entry CRCs; normal reads do not infer validity from slot bytes.

## Runtime Policy

- Writer model is single-writer per file, enforced with a sidecar lock file.
- Reader model is snapshot-on-open; live tailing is out of scope for 0.2.
- `FormatSpec::create_layout_writer`,
  `FormatSpec::create_layout_writer_with_header`, and
  `FormatSpec::open_layout_writer`, and `FormatSpec::open_layout_reader` are
  the custom physical-layout entry points. They do not write or expect the
  Varve-native container header.
- Custom layout files use `LayoutWriter::write_segment` with
  `SegmentWrite { fields, footer_fields, metadata, raw }`; incomplete
  backpatch windows are treated as corrupt physical data by strict open in the
  first slice.
- `open_layout_writer` validates the existing file header, segment literals,
  finalized offsets, and bounds before seeking to EOF for additional appends.
- `LayoutSegmentInfo` exposes validated lead-in and footer field values so
  callers can inspect ToC masks, versions, offset fields, and footer metadata
  without re-parsing bytes manually.
- Durability is explicit: writes are buffered until `flush`, and durable fsync is exposed as `sync`.
- `RecoveryPolicy::Strict` rejects incomplete tails.
- `RecoveryPolicy::TruncateTail` truncates incomplete record header/payload tails and returns a recovery report through `open_recover_with_report`.
- Read-only open never truncates. Recovery truncation is explicit through `open_recover` or `open_recover_with_report`.
- Read-write open for `CommitPolicy::TransactionMarker` truncates uncommitted tail after the latest valid marker so appends cannot accidentally commit stale tail data.
- `IntegrityPolicy::Crc32` and `IntegrityPolicy::Crc32WithHeader` are feature-gated behind `integrity` and reject corrupted covered bytes.
- `IndexPolicy` is a bitset-style policy with `scan_on_open`, `checkpoint_on_flush`, `block_offset_chain`, `keyed_offset_chain`, and `segment_on_flush`. Offset-chain policies are written automatically in `VARVE3` footers.
- `CommitPolicy::RecordFooter` treats valid record footers as the commit flag for each record.
- `CommitPolicy::TransactionMarker(on_flush|explicit)` appends internal `COMMIT_BLOCK_ID` marker records. Readers expose the latest marker-covered snapshot; writer open truncates uncommitted tail after the latest marker.
- `IndexPolicy::segment_on_flush` makes every commit point append an internal segment record covering exactly the records that commit point added. See [Internal Segments](#internal-segments).
- `IndexPolicy::CheckpointOnFlush` writes an internal checkpoint record, spaced geometrically so cumulative checkpoint bytes stay bounded. **Checkpoint-seeded open is specified but not implemented**: as of 0.5.0 every open scans the full record region (`load_index` -> `scan_records_from`), validates any checkpoint it meets, and discards the checkpoint's decoded entries. See the design target below and `docs/known-limitations.md` §2.1.
- `CompressionPolicy::VariableBlocks` and block-specific compression descriptors are feature-gated by the selected backend. The first backend is optional `compression-zstd`; compressed records are rejected when the backend is not enabled.

## Update And Merge

- `replace_fixed` performs snapshot-preserving copy-on-write replacement and
  requires an unchanged encoded payload size. It is refused for a format with
  `segment_on_flush`, which is also true of
  `replace_fixed_in_place_exclusive`: both restamp a record that an already
  written segment describes.
- `replace_rewrite` rewrites through a completed temporary file and atomically replaces the original file path.
- `replace_block` is the generated/core sequence-preserving replacement path.
  It permits a native fixed or variable payload to grow or shrink, rebuilds
  headers, CRCs, checkpoints, and offset-chain footers, and publishes a new
  snapshot generation. Keyed replacement must preserve the key.
- `replace(index, block, ReplaceStrategy)` is the policy-facing wrapper over
  fixed copy-on-write replacement and full-file rewrite replacement.
- `unsafe replace_fixed_in_place_exclusive` is the explicitly unsafe expert
  path for coordinated callers that exclude all overlapping readers/writers.
- Keyed deletes use a common internal tombstone record.
- Keyed ops use user-defined `VarveMerge::Op`.
- Merge conflict ordering is shard order, then local sequence, then record ordinal; later delta shards win over base.
- `keyed_blocks::<T>()` is latest-put indexed access after tombstones.
- `materialized_keyed_blocks::<T>()` applies same-file puts, ops, and tombstones into current state.
- `VarveReader` and `VarveWriter` are additive handle wrappers for clearer read-only and write-capable workflows. Existing `VarveFile` APIs remain available.

## Codec Contract

- Scalar, option, fixed array, selected vector, tuple, `BTreeMap`, and `HashMap` codecs are canonical and endian-aware.
- `BTreeMap<K, V>` encodes in native sorted key order.
- `HashMap<K, V>` encodes borrowed entries in sorted key order, requiring
  `K: Ord` but not `Clone`, so equivalent maps produce stable bytes independent
  of insertion or hash iteration order.
- Decoding `HashMap<K, V>` preserves values but not insertion order.
- Custom field codecs are supported by implementing `VarveEncode` and `VarveDecode` for the field type. The derive macro uses the type's `WIRE_TYPE` in field descriptors and manifests.
- Every codec declares `SCHEMA_ID`, the structural identity of its bytes.
  Built-in scalars declare leaf identities and built-in containers fold their
  elements, so an element that declares none propagates outward as "no
  identity". `#[derive(VarveBlock)]` folds every field's encode and decode
  `SCHEMA_ID` into the block fingerprint and rejects at compile time any field
  whose codec left `SCHEMA_ID` at the default `0`. A hand-written codec used as
  a block field must therefore declare a value that changes whenever its emitted
  bytes change; identical source spelling is no longer enough to make two custom
  codecs share a schema identity.
- Every built-in public codec declares an identity, including the two value-level
  helpers: `ChunkedBytes` folds the `chunked_bytes` tag, its container-format
  version, and `Vec<u8>`'s identity; `PackedBitmap` folds the `packed_bitmap` tag
  with the `u64` bit length and `Vec<u8>` payload it emits. Both are const
  expressions over compile-time constants, so they are stable across builds and
  platforms, and both are usable as ordinary derived variable fields.
- `PackedBitmap` is **not** a fixed-width matrix field. A matrix slot needs a
  stride known at compile time, and a `Vec`-backed encoding has none. Inline
  matrix `SLOT_STRIDE` is generated from each element's `VarveEncode::WIRE_TYPE`
  rather than the Rust object size, so a type without a fixed encoded width is
  rejected during const evaluation whatever it is spelled.
- Enum-like values should use explicit custom codecs; automatic enum representation inference is out of scope.
- Length and count limits are format-author policy. Varve's built-in decoders
  avoid large allocation-before-validation patterns where the remaining bytes
  can be checked generically, but domain-specific maximum string, sequence,
  map, chunk, or decompressed sizes should be enforced by the format's custom
  codecs, compression policy, adapter, or caller validation.
- `ChunkedBytes::decode_to_vec_limited(limit)` is provided for callers that
  need an explicit decompressed-size ceiling.
- `RecordIndexEntry::read_payload_limited(path, limit)` validates the stored
  extent and caller byte ceiling before allocation.
- `RecordIndexEntry::read_logical_payload_limited(spec, path, physical_limit,
  logical_limit)` applies both caller ceilings before allocating the complete
  stored payload or decompressed logical payload.
- Boolean decoders accept only canonical bytes `0` and `1`. Map decoders reject
  duplicate destination keys before decoding a duplicate value.
- Every decoder-owned container charges the materialization budget before it
  reserves, including the set that tracks seen variable field ids above 63: each
  distinct such id charges 8 bytes and an exhausted budget fails with
  `Error::LimitExceeded { resource: "variable field ids" }`. Duplicate detection
  runs before the charge, so a repeated id cannot drain the budget.

## Macro Contract

- `#[derive(VarveBlock)]` supports fixed and variable structs.
- Variable fields require non-zero unique field ids.
- Keyed blocks support single-field and composite keys.
- Duplicate key fields, missing key fields, duplicate block ids, and reserved block ids fail at compile time where macro input makes that possible.
- `varve_format!` supports the legacy registry form `pub struct Format { blocks: [A, B]; }` and the format-first form `pub format Format { blocks { fixed A(...) { ... } } }`.
- In format-first form, block structs, `VarveBlock` implementations, typed reader/writer wrappers, and typed read/write traits are generated from the format declaration.
- `varve_format!` supports `magic`, `version`, optional `limits`, `endian`, optional `schema_hash`, optional `commit`, optional `integrity`, optional `index`, optional `recovery`, optional `manifest`, and `blocks`.
- `limits { ... }` is an optional, partial set of operational defaults. Unknown
  and duplicate keys are compile errors. Limits do not affect wire bytes,
  schema hashes, or manifests. Ordinary handles fill missing fields from
  `ReadLimits::STANDARD`; `*_with_resource_limits` may raise or lower defaults,
  while compatibility `*_with_limits` methods only tighten them. The explicit
  trusted-unbounded APIs remain separately named.
- `integrity` accepts `none`, `crc32`, or `crc32_with_header`. `crc32`
  covers payload plus native record footer when present. `crc32_with_header`
  additionally covers the native 32-byte record header with the checksum field
  normalized to zero.
- `varve_format!` supports `preset: varve_native|none|custom;`. Omitted preset
  means `varve_native` unless a custom layout is declared; with declared layout
  parts, omission means `none` so the declaration owns byte zero.
- `varve_format!` supports the first custom layout grammar:
  `layout { file_header Header { bytes sig = b"..."; } segment Name repeat
  until_eof { lead_in Name { bytes tag = b"..."; u32 caller_field; i64 offset =
  finalize(target = segment_end, relative_to = after_lead_in); } metadata Name;
  raw_region Name; footer Footer { bytes seal = b"..."; u64 len =
  finalize(target = segment_end, relative_to = segment_start); } } }`.
- Custom physical layout scanning accepts numeric finalized fields with the
  target format's required integer width. Segment `raw_region_start` may be
  resolved relative to `segment_start`, `after_lead_in`, or `metadata_start`;
  `segment_end` may additionally be resolved relative to `raw_region_start`.
- Format-first custom layout declarations generate typed
  `FormatLayoutWriter` / `FormatLayoutReader` wrappers, typed header-field
  structs, typed header info getters, typed segment write structs, and typed
  segment info getters. The generated wrapper still exposes the low-level
  `SegmentWrite` path, streamed segment writes, tolerant scan reports, and
  whole-region or range metadata/raw readers.
- Physical layout declarations are not `VarveBlock` payload declarations:
  `VarveBlock` is the logical native append-log record unit, while
  `file_header`, `lead_in`, `footer`, `metadata`, and `raw_region` declare the
  surrounding byte-level framing for custom external formats.
- Custom layouts may declare multiple physical segment descriptors. The scanner
  dispatches each segment by its leading literal lead-in prefix; immediately
  following literal numeric fields extend the dispatch key. Segment-specific
  generated reader indexes are per segment kind, while low-level layout reader
  indexes remain physical stream indexes.
- `schema_hash: computed;` asks the macro to call `with_computed_schema_hash()` after policies are attached.
- `index` accepts either a single legacy identifier or a list such as `[scan_on_open, checkpoint_on_flush, block_offset_chain, keyed_offset_chain]`. `segment_on_flush` is deliberately **not** one of them: a block is the unit a declaration names, a segment is varve's internal lookup unit, and its granularity is varve's decision. Enable it on the `FormatSpec` with `IndexPolicy::with_segment_on_flush`.
- `commit` accepts `none`, `record_footer`, or `transaction_marker(on_flush|explicit)`.
- Generated typed writers expose both `commit()` and `commit_durable()`.
  `commit()` is a logical visibility marker and does not imply fsync.
  `commit_durable()` writes commit-covered metadata/checkpoints, flushes and
  syncs data, writes the marker, then syncs the marker.
- `varve_format!` also supports optional `extension` and global `compression`. Compression syntax is `compression: none;` or `compression: variable_blocks(zstd, level = default|fast|best|N, header = record_explicit|file_explicit|format_contract, min_len = N, only_if_smaller = true|false, max_len = N);`.
- Per-block compression overrides are a runtime `FormatSpec::with_block_compression` API in 0.2, not a macro DSL clause.
- Legacy registry formats expose raw `VarveReader`/`VarveWriter` handles. Format-first declarations expose typed `FormatReader`/`FormatWriter` wrappers where methods such as `push_user`, `delete_user`, `users`, `commit`, `commit_durable`, `flush`, and `sync` are generated from block declarations.
- Duplicate top-level `varve_format!` keys are compile errors. The derive
  macro rejects generic block structs in 0.2 with a direct diagnostic; use a
  concrete block type or manual trait implementations.
- The derive macro emits `VarveBlock::FIELDS`, including field id, field name, wire type, and required/defaulted presence.
- `#[varve(default)]` is variable-block-only. Fixed blocks are positional canonical payloads and cannot omit fields.
- `key = "..."` must contain one or more valid Rust identifiers separated by commas. Empty key strings, empty segments, duplicates, and missing fields are compile errors.
- `FormatSpec::computed_schema_hash()` computes a deterministic schema fingerprint from format version, endian, policies, block descriptors, and field descriptors. It intentionally excludes the pinned header `schema_hash` value.
- Custom layout descriptors participate in `computed_schema_hash()`. The
  default `LayoutPreset::VarveNative` with no custom parts is intentionally not
  hashed so existing native format hashes and bytes remain stable.
- `FormatSpec::schema_debug_dump()` emits a human-readable registered schema view for diagnostics.
- `FormatSpec::diagnostics()`, `FormatSpec::diagnose_file(path)`, and
  `FormatSpec::self_test(path)` provide user-facing sanity checks that classify
  failures as format definition, caller usage, feature gate, file data,
  environment, or library invariant issues.

## Acceptance Criteria

- Roundtrip fixed, variable, nested, option, array, scalar vec, string vec, and bytes payloads.
- Roundtrip and byte-stability checks for deterministic map codecs.
- Lazy typed collections and keyed collections read back typed values.
- `keyed_blocks::<T>()` applies tombstones to latest-put lookup. It does not
  apply user-defined ops; use `materialized_keyed_blocks::<T>()` for put/op/
  tombstone materialization.
- Unknown variable fields are skipped.
- Header/version/endian/schema mismatch is rejected.
- Block version mismatch is rejected on typed read.
- Strict recovery rejects corrupt tails; recover mode truncates incomplete tails.
- CRC integrity detects payload/footer covered-byte modification when feature-enabled.
- `crc32_with_header` detects native record header tampering as well.
- Variable-block compression roundtrips under record-explicit, file-explicit, and format-contract metadata modes.
- Explicit `preset: varve_native` produces byte-identical output to the omitted
  preset for the same native format declaration.
- Omitted or explicit `preset: varve_native` exposes an effective layout plan
  containing `VarveFileHeader`, repeated `VarveRecord`, and, when applicable,
  `VarveRecordFooter`.
- `inspect_layout_file` works for both native preset files and `preset: none`
  custom physical layouts, including validated file-header fields.
- A TDMS-style custom layout can write files whose first bytes are `TDSm`, whose
  `next_segment_offset` and `raw_data_offset` fields are backpatched to actual
  metadata/raw boundaries, whose ToC/version fields are visible through
  `LayoutSegmentInfo`, and whose mixed raw channel bytes are sliced by
  adapter-owned chunk metadata.
- The TDMS-style physical writer example is an external adapter proof, not a
  Varve-provided TDMS feature. It produces a three-segment scalar type matrix
  using Varve's generated layout APIs plus adapter toolkit helpers, then
  reopens it with `open_layout_writer` to append another segment. The optional
  Python harness opens that file with `npTDMS` and verifies file properties,
  group/channel lookup, channel properties, changed raw-data-index segments,
  `same-as-previous` raw-index reuse, bool/string/integer/float tagged
  properties, sidecar inspection, byte-backed reading, and exact raw values for
  signed/unsigned integer widths, single/double floats, single/double floats
  with unit type ids, booleans, strings, timestamps, and complex single/double
  floats.
- The reverse TDMS-style harness generates a two-segment scalar type matrix
  with Python `npTDMS`, then parses it through a Varve-based example adapter
  that uses the generated layout reader plus caller-owned TDMS metadata/raw
  logic. It then appends one segment with Varve and verifies the result with
  both npTDMS and Varve.
- The boundary between Varve generic API obligations and external TDMS adapter
  obligations is fixed as follows. Varve owns physical-layout declaration and
  validation, segment framing and appends, and the exposure of offsets, lengths,
  field values, metadata and raw byte ranges, and tolerant scan reports. The
  adapter owns TDMS object paths, raw-data-index grammar, property typing,
  timestamp and waveform conversion, scaling, DAQmx raw scalers, sidecar policy,
  and export. If strict layout open succeeds and the adapter can read the
  required ranges, an object-assembly, typing, scaling, timestamp or export
  failure is an adapter issue; if a valid external file cannot be expressed with
  Varve layout declarations, or Varve rejects a file before the adapter can
  report a documented physical status, that is a Varve generic API issue.
- The BMP/Pillow harness verifies a non-TDMS custom physical layout in both
  directions: Varve writes a 24-bit BMP opened by Pillow, and Pillow writes a
  BMP whose header fields, row-level chunk index, and pixel payload are decoded
  by Varve.
- A custom physical layout with a declared file header and segment footer writes
  those bytes at the declared offsets, exposes `file_header_len` and validated
  file-header values, and excludes footer bytes from the segment raw-region
  read.
- Custom layout readers reject bad literal tags, truncated lead-ins, invalid
  file headers, invalid offset ordering, footer mismatches, and segment bounds
  beyond EOF.
- Block-specific record-explicit compression can compress one registered variable block while leaving other blocks uncompressed when the global policy is `None`.
- Compressed checkpoint entries and rewrite replacement preserve `uncompressed_len_hint`.
- `compression-zstd` disabled builds report `CompressionFeatureDisabled` when actual compression is attempted.
- Unknown variable field ids with known wire types are skipped, and unknown wire type values fail with `UnknownWireType`.
- Mmap payload windows, matrix slot windows, zero-copy raw fixed reads, and
  zero-copy raw matrix reads are opt-in, feature-gated, and do not change normal
  owned canonical decoding.
- Writer lock breaking is explicit only; default open/create/recover paths refuse an existing lock.
- Macro pass/fail UI tests cover valid formats and invalid schema declarations.
- Property tests cover append, rewrite, merge, tombstone, and op state transitions.
- Performance smoke tests are run during major implementation phases to catch accidental O(n^2) scans, excessive allocations, and slow open/merge paths.
- Benchmarks and smoke checks track encode, decode, append, open/scan,
  checkpoint open, materialized keyed state, merge/compact, direct base+delta
  compact, custom physical layout append/open-scan, recovery, mmap payload
  windows, matrix mmap windows, and zero-copy read paths.
- Compression checks track compressed append/read, physical scan behavior, checkpoint reuse, rewrite, file size, and backend-disabled failure paths.
- Dependency license audit reports only commercially usable permissive choices.

## Performance Guardrails

- Every major feature slice must include a lightweight performance check before it is considered integrated.
- Performance checks may start as ignored tests or benchmark examples, but they must be runnable from the workspace without external services.
- The dependency-free benchmark example is runnable with `cargo run -p varve --example perf_bench --release -- 10000`.
- The baseline dataset should include at least small, medium, and large record counts so regressions are visible before real-world scale.
- The first benchmark suite should measure wall time, records per second, file size, and approximate allocation-sensitive behavior where practical.
- Any implementation that changes indexing, scanning, merge, codec, compression, recovery, mmap, or zero-copy behavior must document the expected complexity and include a regression-oriented performance check.
- Any implementation that changes custom physical layout framing or scanning
  must include the layout append/open-scan smoke path.
- Direct base+delta compact is measured separately from merge-then-compact so unnecessary intermediate file costs are visible.
- A performance result is not a hard product guarantee during pre-stable versions, but a large unexplained slowdown is a blocking implementation finding.

## Next Wave Contract

This section pins the P0-P2 implementation contracts so worker agents can implement the same behavior.

### Compression

- Compression is opt-in and disabled by default.
- Runtime policy enum: `CompressionPolicy::{None, VariableBlocks(VariableCompression)}`.
- Runtime block override descriptor: `BlockCompressionDescriptor { block_id, compression }`.
- `VariableCompression` includes `algorithm`, `level`, `header_mode`, `min_uncompressed_len`, `only_if_smaller`, and `max_uncompressed_len`.
- The first algorithm is `CompressionAlgorithm::Zstd`, behind the `compression-zstd` feature.
- Block-specific compression applies only to registered `variable` block ids, rejects duplicates, and is limited to `CompressionHeaderMode::RecordExplicit` so each compressed record remains self-describing without changing file-header semantics.
- `only_if_smaller` compares the final stored payload size, including the record-explicit envelope.
- `max_uncompressed_len` is a hard write/read decompression limit and is checked before allocation.
- `CompressionHeaderMode::RecordExplicit` stores a `VCMP` envelope in each compressed payload with algorithm, level, and `u64` uncompressed length.
- `CompressionHeaderMode::FileExplicit` stores compression configuration in a `VARVE2` file header extension. It uses raw compressed record payloads plus `uncompressed_len_hint`, and therefore requires `max_uncompressed_len <= u32::MAX`.
- `CompressionHeaderMode::FormatContract` stores no compression metadata beyond the compressed record flag and length hint. It requires `schema_hash == computed_schema_hash()` and `max_uncompressed_len <= u32::MAX`.
- Existing uncompressed files keep using `VARVE1`. File-explicit compression uses `VARVE2` with a length-prefixed `VCHD` extension.
- Unknown record flag bits are rejected during scan.
- Compressed flags on fixed, internal, metadata, op, tombstone, index, or manifest records are invalid.

### Internal Segments

- A **segment** is varve's internal lookup unit: the records one commit point
  added. It has no declaration surface, and its granularity is not a knob — a
  block is what a declaration names, a segment is what varve indexes by.
- Enabled by `IndexPolicy::segment_on_flush`, which is off by default. A file
  written with it off is byte-identical to one written before the option
  existed, and `computed_schema_hash()` is unchanged.
- It requires `block_offset_chain` — the chain *is* `prev_same_block_offset` —
  and a `crc32` integrity policy, because the chain walk trusts a payload that
  describes records it never reads.
- It **supersedes `checkpoint_on_flush`**, and declaring both is refused. Both
  answer "what is the whole index?", but the checkpoint serialises every entry
  from scratch on a geometric cadence while the chain carries the delta a commit
  point added; the checkpoint stops fitting in a record past
  `(max_record_payload_len - 22) / 73` entries while a segment is sized by its
  commit point; and open *reads* the chain, whereas it merely validates a
  checkpoint it walks past and discards the entries. With the chain on, a
  checkpoint is a periodic full copy of the index that nothing reads.
- Segment records use internal block id `SEGMENT_BLOCK_ID` (`0xFFFF_FFF7`),
  block version `1`, and the internal record flag.
- The payload layout is:
  - magic bytes `b"VSEG"`,
  - payload version `u16 = 1`,
  - flags `u16`, currently `0`,
  - covered start offset `u64` — the first record this segment covers,
  - entry count `u64`,
  - preceding record count `u64` — records in the file before the covered start,
  - `entry count` index entries in the checkpoint version 3 entry layout,
  - the segment record's own start offset `u64`.
- The trailing self offset is what makes the record findable from the end of the
  file: the footer carries no self offset, and the header that would give one is
  `payload_len` bytes further back.
- A commit point writes its segment **last**, after any commit marker, and only
  when every record it would cover is committed. A segment record that follows
  the latest commit marker is inside the committed prefix, **whether or not it
  is the final record in the file** — it describes only records that marker
  already committed, so a writer's later unflushed appends do not make it
  uncommitted. Every other record after that marker is uncommitted.
- A commit point that added no record writes no segment.
- A writer that stops without a commit point leaves a data record at the end of
  the file, so the chain has no entry point and the next open scans. This is the
  fallback below, not an error, but it costs the whole benefit and is silent.
- Open reads the last 32 bytes, confirms the record footer magic and version,
  reads the eight bytes before them as the candidate record offset, and confirms
  a segment record there whose extent ends at the file length. It then follows
  `prev_same_block_offset` backwards. Predecessor offsets must decrease strictly
  and stay at or above the append-log start, so the walk is bounded by the
  segments in the file whatever the bytes claim.
- A chain is accepted only if each link ends exactly where its successor's
  coverage began, the entries of each link tile that coverage with no gap and no
  overlap, the oldest link reaches the append-log start, and the whole walk ends
  at the file length. No link may cover another segment record.
- **When `open_digest_on_flush` is also declared, "the file length" above means
  the digest's start offset**, and the walk indexes the digest record itself to
  reach the file length. The digest closes the same commit point the newest
  segment does and is written after it, so the newest segment does not end at
  the file length — a walk that demanded it would reject a perfectly good chain
  and fall back to the scan, which is the segment chain silently turning off.
  The digest is a resident record, so a walk that stopped at the newest segment
  would also hand back an index one entry shorter than a scan of the same bytes.
- Under a transaction-marker policy a chain is additionally refused unless the
  index it produces is **wholly inside the committed prefix**. A chain can tile
  the whole append log and still contain no commit marker — structurally
  perfect, and a lie: the writer open that follows cuts the file back to the
  header, and a handle holding the untruncated index would then append onto an
  offset its own index already claims.
- A chain that fails any of this is not an error: open falls back to the full
  record scan and produces the identical index. So does an open under
  `IntegrityVerification::AtOpen`, and every recovery open.
- **A resource refusal raised while walking is one of those failures, not an
  error.** The walk charges ceilings against bytes it has not validated yet —
  the segment count, and the payload length at an offset the trailer only
  claims is a segment — so a limit hit there is evidence about the chain, not
  about the file. Falling back bypasses nothing: the scan charges `Records`,
  `IndexBytes`, `ScanBytes` and `RecordPayloadLen` itself and refuses honestly
  if the file really is too large. Only `MissingResourceLimit` and
  `TrustedUnboundedRequiresExplicitApi` propagate, because those describe the
  caller's configuration rather than these bytes.
- `replace_block` re-encodes every segment payload against the published
  generation's offsets. In-place replacement is refused, because nothing rewrites
  the segment describing the record it restamps.
- **The append-log start above is the file's, not the header's.** In a matrix
  file the matrix region sits between the header and the append log, so both the
  coverage a writer records and the boundary the walk demands the oldest link
  reach are `MatrixLayout::append_log_start()`. A matrix format may declare
  `segment_on_flush` and gets the same chain; measured on a 4x4 matrix with 128
  append-log records over 8 commit points, open framed 8 records.

### Open Digest

- An **open digest** is the three facts an open needs that live in no single
  record: where the committed file ends, what sequence the next append takes,
  and where each block's newest record sits. Today they are the by-product of
  framing every record, and that walk is the only reason an open that builds no
  index still reads the whole file.
- Enabled by `IndexPolicy::open_digest_on_flush`, which is off by default. A
  file written with it off is byte-identical to one written before the option
  existed, and `computed_schema_hash()` is unchanged.
- It requires `block_offset_chain`. The digest hands out each block's tail
  offset and the only thing to do with a tail offset is walk back through
  `prev_same_block_offset`; without the chain it would be handing out an entry
  point to a dead end.
- It is **not** mutually exclusive with `segment_on_flush` or
  `checkpoint_on_flush`. The three answer different questions: a checkpoint and
  a segment chain both let open *build the index* cheaply, a digest lets open
  skip building one at all. A format may declare a digest with either.
- Digest records use internal block id `OPEN_DIGEST_BLOCK_ID` (`0xFFFF_FFF5`),
  block version `1`, and the internal record flag.
- The payload layout is:
  - magic bytes `b"VDIG"`,
  - payload version `u16 = 1`,
  - flags `u16`, currently `0`,
  - sequence high-water `u64` — the newest sequence in the file as it stands
    once this record is down, or `u64::MAX` for "no record carries one",
  - block tail count `u32`,
  - `count` pairs of block id `u32` and newest record offset `u64`, **strictly
    ascending by block id**,
  - the digest record's own start offset `u64`.
- **The committed file end is deliberately not stored.** The digest is the last
  record in the file, so the committed end is the digest's own end — a number
  open holds as soon as it has framed the record, and one that cannot disagree
  with the bytes the way a stored copy could.
- The digest does not carry its own block's tail. It cannot: the record does not
  exist when its payload is built. Nothing walks a digest chain — a digest is
  found at the end of the file, never by following one.
- The size is **constant in the record count**: 20 bytes of prefix, 12 per
  distinct block id, an 8-byte trailer, inside a 32-byte header and a 32-byte
  footer. A file with three block ids carries a 128-byte digest whether it holds
  two hundred records or two billion. A segment, by contrast, carries an entry
  per record it covers.
- A commit point writes its digest **last** — after any commit marker and after
  any segment record — because it is found by probing the end of the file and
  anything appended behind it hides it.
- A digest record that follows the latest commit marker is inside the committed
  prefix, on the same grounds a segment record is: it describes only records
  that marker already committed. The shapes a writer produces after a marker are
  `[SEGMENT]`, `[DIGEST]` and `[SEGMENT, DIGEST]`; a longer run is a file varve
  did not write, and the prefix is cut back to the marker.
- A commit point whose file already ends in a digest writes no new one, so idle
  flushes do not grow the file.
- A digest is exempt from the `checkpoint_on_flush` cadence's eligible-record
  tail, as the commit marker and the segment record are: it is written because a
  commit point closed, not because a record was added. It is **not** exempt from
  the checkpoint's geometric threshold, which is a function of index position —
  a digest is a resident record and enlarges the index the checkpoint must
  serialise, exactly as a commit marker does.
- `VarveFile::open_readonly_lazy` reads the digest and frames **no other
  record**. The handle it returns keeps no record directory, because none was
  built: position-based reads (`blocks`, `scan`, `keyed_blocks`) return
  `Error::NoResidentDirectory`, and the caller supplies a directory with
  `with_directory` — typically one built incrementally by `record_map`.
  `block_chain`, `block_tail_offset`, `read_block_at` and every entry-taking
  `_into` read need no directory and work directly, which is what the block
  tails are in the digest for.
- Open reads the last 32 bytes, confirms the record footer magic and version,
  reads the eight bytes before them as the candidate record offset, and confirms
  a digest record there whose extent ends at the file length and whose checksum
  verifies. The payload's trailing self offset must equal that record offset,
  the block ids must ascend strictly, and every tail offset must name a position
  at or after the append-log start and strictly before the digest.
- A digest that fails any of this is not an error: open falls back to the full
  record scan and produces the identical answer. Only `MissingResourceLimit` and
  `TrustedUnboundedRequiresExplicitApi` propagate, for the reason they propagate
  out of the segment walk. `open_readonly_lazy_with_report` returns
  `LazyOpenSource::{Digest, FullScan}` so the fallback is observable — the two
  differ by about four orders of magnitude in read syscalls and by nothing at
  all in the answer.
- A writer that stops without a commit point leaves a data record at the end of
  the file, so the probe finds no digest and the next open scans. This is the
  fallback, not an error, but it costs the whole benefit.
- `replace_block` re-encodes every digest payload against the published
  generation's offsets, as it does every segment payload. A digest payload is
  block tails and a self offset, and a rewrite moves both; copying it would
  publish a generation whose digest describes the file it replaced.
  `replace_rewrite` refuses record-footer formats outright, and a digest
  requires the footer, so that path is unreachable for a digest format.

### Checkpoint Index

- Checkpoint records use internal block id `INDEX_BLOCK_ID`.
- Checkpoint payload version is `u16 = 3`.
- Checkpoint payload layout is:
  - magic bytes `b"VIDX"`,
  - payload version `u16`,
  - covered record end offset `u64`,
  - covered record count `u64`,
  - repeated record index entries for all records covered by the checkpoint except the checkpoint record itself.
- v2 checkpoint entries include `uncompressed_len_hint`. v3 checkpoint entries additionally include footer offset, previous block offset, previous key offset, and committed state. v1/v2 checkpoints still decode with missing v3 fields as absent/defaults.
- A checkpoint is valid only if:
  - the payload decodes exactly,
  - covered offset is within the file length,
  - every entry has valid offsets and payload bounds,
  - entries tile the covered range exactly: each entry starts where the previous entry ended, the first starts at the append-log start, and the last ends at the covered offset,
  - no entry points to the checkpoint record itself,
  - CRC validation passes for the checkpoint payload when integrity is enabled.
- For `IndexPolicy::CheckpointOnFlush`, open should scan from the header until the latest valid checkpoint, then rebuild the index from the checkpoint and scan only records after the covered offset. **Not implemented as of 0.5.0** — this is a design target. The checkpoint is validated on the way past and its entries are discarded; open scans the whole region.
- Structurally corrupt checkpoints are ignored and full scan fallback is allowed.
- CRC mismatch remains fatal and must not be hidden by checkpoint fallback.
- Recovery open may truncate only incomplete header/payload tails after the checkpoint; it must not truncate complete-but-corrupt records.

### Integrity Policy

| Policy | Payload coverage | Header coverage | Checkpoint behavior | Recovery interaction |
| --- | --- | --- | --- | --- |
| `IntegrityPolicy::None` | No checksum validation. | Record headers are structurally parsed only. | Structurally invalid checkpoints can be ignored and full scan fallback is allowed. | Recovery can truncate incomplete tails only. Complete-but-wrong payload bytes may decode as data or fail codec validation. |
| `IntegrityPolicy::Crc32` | Per-record payload CRC32 when the `integrity` feature is enabled. For `VARVE3`, the checksum covers `payload + footer`. | Header fields are structurally parsed but not included in this CRC mode. | Checkpoint payload CRC mismatch is fatal and must not be hidden by fallback. | Recovery can truncate incomplete tails, and transaction-marker readers may ignore corrupt tail after the latest valid marker. Complete CRC mismatches in marker-covered or record-footer-visible records remain errors. |
| `IntegrityPolicy::Crc32WithHeader` | Same payload/footer coverage as `Crc32`. | Also covers the native record header with the checksum field normalized to zero. | Same as `Crc32`. | Same as `Crc32`. |

CRC integrity is a corruption-detection aid, not an authenticity or tamper-proofing mechanism.

### Manifest

- Manifest support is optional and disabled by default.
- Macro syntax: `manifest: none;` or `manifest: embedded;`.
- Runtime policy enum: `ManifestPolicy::{None, Embedded}`.
- Embedded manifests use internal block id `MANIFEST_BLOCK_ID`.
- Manifest payload version is `u16 = 5`.
- Manifest content includes format version, endian, schema hash, extension, index policy, commit policy, integrity policy, recovery policy, manifest policy, compression policy, registered block descriptors, and registered field descriptors.
- v2 field descriptors include field id, wire type, required/defaulted presence, and field name. v1 manifests can still be decoded with empty field lists for compatibility.
- `VarveFile::schema_manifest()` returns the latest embedded manifest if present.
- Files without a manifest still open.
- Static `FormatSpec` remains authoritative for typed access; the manifest is diagnostic/debug data, not dynamic schema loading.

### Migration Scaffold

- Normal typed reads continue to reject block version mismatches.
- Migration is explicit only.
- Runtime trait: `VarveMigration<From, To>` with a user-provided conversion function.
- Runtime API: `file.blocks_migrated::<From, To, M>() -> Result<Vec<To>>`.
- Migration requires `From::ID == To::ID`, reads records matching `From::VERSION`, decodes `From`, and applies `M`.
- Macro-generated code does not infer field renames, deletes, or defaults beyond normal `#[varve(default)]`; semantic conversion stays in user Rust code.

### Compact And Replace

- `replace_fixed` performs a same-size, snapshot-preserving copy-on-write
  publication.
- `replace_rewrite` is explicit full-file rewrite for direct replacement.
- `replace(index, block, ReplaceStrategy::{FixedCopyOnWrite, RewriteFile})`
  exposes the safe policy choice directly.
- `unsafe replace_fixed_in_place_exclusive` retains the lower-copy path under
  an explicit exclusivity contract.
- Delta/op append remains the recommended append-friendly update path.
- Compact API: `compact_keyed_file::<T, P>(spec, input, output)`.
- Compact reads one file, materializes final keyed state for `T`, drops tombstones and ops, writes only final values to a new output file, and writes metadata/checkpoint according to output format policy.
- Base+delta compact API: `compact_keyed_files::<T, P>(spec, base, deltas, output)`.
- Base+delta compact reads the base and ordered delta shards, applies the same shard-order conflict semantics as merge, and atomically publishes only final values.
- Compact is single keyed block type per call. Non-keyed blocks and unrelated block ids are not copied in this first API.
- Scale contract: `merge_keyed_files`, `compact_keyed_file`, and
  `compact_keyed_files` are resident operations and are explicitly not PB-scale.
  Time is `Theta(records + decoded bytes) + O(K-live log K-live)`; memory is
  `O(K-ever + largest resident input index + retained live values)`, where
  `K-ever` counts every distinct key ever seen including tombstoned keys.
  Nothing spills to disk and no bounded-memory external merge/compact is
  exported.
- Guarded APIs: `estimate_keyed_merge::<T, P>(spec, base, deltas)` returns a
  decode-free `KeyedMergeEstimate`, and
  `merge_keyed_files_with_key_limit`, `compact_keyed_files_with_key_limit`,
  `compact_keyed_file_with_key_limit` refuse a run that would exceed
  `max_distinct_keys` with `Error::LimitExceeded { resource: "merge distinct
  keys", .. }` before publishing any output.

### Writer Safety

- The sidecar lock remains the default single-writer guard.
- Lock files use text payload version `varve-lock-v1` and contain process id, creation timestamp in Unix milliseconds, and target path for diagnostics.
- `FormatSpec::inspect_writer_lock(path)` returns `Ok(None)` when no lock exists, `Ok(Some(WriterLockInfo))` for a valid lock, and `WriterLockMalformed` for malformed metadata.
- Stale-lock breaking is not automatic by default. `FormatSpec::open_with_lock_policy(path, policy)` is the explicit opt-in entry point.
- `WriterLockBreakPolicy::Refuse` preserves the default behavior and returns `WriterLockHeld`.
- `BreakIfOlderThan` is timestamp based; `BreakIfProcessAbsent` is best-effort and conservative when process liveness cannot be determined.
- Malformed locks are never automatically broken in 0.2.
- Atomic rewrite/compact output uses same-directory temp files, flushes and syncs temp contents, then replaces or renames into place.
- Reader behavior remains snapshot-on-open; readers do not tail live writers.

### Unknown Fields

- Unknown variable field ids with known wire types are skipped and dropped by default.
- Unknown wire type values are malformed data and return `UnknownWireType`.

### Zero-Copy And Mmap

- Mmap and zero-copy are opt-in features only. The `zero-copy` crate feature implies the `mmap` feature because the first raw-read API is mmap-backed.
- Default typed decode remains owned canonical decoding.
- Initial mmap scope exposes read-only payload windows from unsafe
  `VarveFile::mmap_payloads() -> MmapPayloads`.
- `MmapPayloads` owns a read-only mmap plus a cloned snapshot index and `FormatSpec`.
- `MmapPayloads::payload_window(entry)` accepts only entries that exactly match the cloned snapshot index, preventing forged public offsets from exposing arbitrary file bytes.
- `MmapPayloads::block_payload_window::<T>(index)` returns the typed block ordinal payload bytes or `None` when out of range.
- Unsafe `VarveFile::mmap_matrix()` and `VarveReader::mmap_matrix()` expose
  `MmapMatrix`, a read-only mmap plus a cloned VMAT layout snapshot.
- Every file-backed mmap constructor requires the caller to prevent mutation,
  truncation, replacement, or backing-object invalidation through every handle,
  thread, and process for the mapping's complete lifetime.
- Under that precondition, safe mmap accessors rely on a cloned existing handle,
  a read-only mapping, checked extents against mapped length, copied snapshot
  metadata, exact record membership, checked slices, and owner-bounded Rust
  lifetimes. Raw views additionally validate kind, version, endian, exact size,
  and alignment.
- `MmapMatrix::cell_payload_window::<T>(key)` returns a committed matrix slot
  payload window and verifies the per-cell CRC when matrix integrity is enabled.
- Initial zero-copy scope is limited to explicit raw fixed blocks whose implementor promises endian, alignment, and layout compatibility.
- Raw fixed access is exposed only as `unsafe MmapPayloads::raw_fixed::<T>(index)` where `T: VarveRawFixedBlock`.
- `VarveRawFixedBlock` is an unsafe opt-in trait over `VarveBlock + zerocopy::FromBytes + zerocopy::Immutable + zerocopy::KnownLayout`.
- `raw_fixed` requires fixed block kind, registered id/version/kind, record version match, matching raw endian, exact `size_of::<T>()` payload length, and valid `align_of::<T>()` alignment.
- Raw matrix access is exposed as `unsafe MmapMatrix::raw_cell::<T>(key)` where
  `T: VarveRawMatrixBlock`; it requires matrix block kind, committed status,
  matching raw endian, exact `size_of::<T>()` slot length, valid alignment, and
  per-cell CRC verification when enabled.
- Raw zero-copy calls require the caller to guarantee the mapped bytes are not
  mutated by any handle, thread, or process for the lifetime of returned
  references.
- Variable blocks are not zero-copy eligible in the first scaffold.
- No API may silently reinterpret canonical fixed encoding as Rust memory layout.

### External Adapter Toolkit

- The adapter toolkit is a generic layer above custom physical layouts.
  It must not hardcode TDMS. TDMS-style support should emerge from reusable
  declarations plus user-defined codecs, chunk builders, and reducers.
- `BinaryCursor` and `BinaryWriter` provide checked endian-aware
  primitive reads/writes, bounded byte windows, cursor position reporting, and
  length-prefixed bytes/string helpers.
- `TaggedValueCodec` lets adapters map format-specific
  type ids to user value enums without Varve owning the meaning of those type
  ids.
- `ChunkIndex` and `ChunkIndexBuilder` map physical segment
  raw regions to logical stream chunks. The builder validates bounds against
  `LayoutSegmentInfo`, while the adapter supplies stream keys, value counts,
  stride/interleaving rules, and semantic metadata.
- `SegmentReducer`, `reduce_segments`, and `reduce_segments_by_ref` run
  stateful segmented metadata reductions. Varve
  owns iteration and diagnostics; the adapter owns inheritance, replacement,
  same-as-previous, deletion, and version semantics.
- Sidecar policy helpers support companion index/cache files. Varve owns
  path derivation and basic identity diagnostics; the adapter owns sidecar wire
  grammar and rebuild policy.
- A future declarative `varve_adapter!` or nested `adapter { ... }` DSL may
  generate calls to this runtime layer. The
  DSL should generate adapter scaffolding from declarations but keep custom
  semantics in ordinary user Rust implementations.
- Adapter diagnostics should combine static declaration checks, physical layout
  reports, cursor parse errors, chunk-bound checks, reducer status, and sidecar
  status so users can decide whether a failure belongs to Varve mechanics,
  adapter declarations, user domain code, or damaged file bytes.
- The layer model is fixed: external file bytes, then the Varve physical layout,
  then the adapter toolkit primitives above, then user-defined domain semantics,
  then the public adapter API. Varve declares and verifies reusable binary
  mechanics; the adapter author defines domain meaning wherever the format
  requires it. TDMS is one branch produced by these generic pieces, not a
  hardcoded Varve feature.
