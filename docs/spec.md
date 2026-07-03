# Varve v0.1 Implementation Spec

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
- User block ids are explicit `u32` values below `0xFFFF_FF00`; higher ids are reserved for internal records.
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
- Files whose static spec contains matrix blocks store a `VMAT` layout region
  immediately after the normal Varve file header.
- `VMAT` layout version 1 stores runtime dimensions, matrix block layout,
  commit bitmap offsets, slot region offsets, derived static aux region
  placement, optional offset-table metadata, region CRC metadata, and
  `append_log_start`.
- The append-log scanner starts at `append_log_start` for matrix files and at the
  normal header length for non-matrix files.
- Matrix slot payloads are fixed-stride, bounded, and addressed directly by
  deterministic key formulas such as `scan * n_channels + ch`.
- Matrix commit bitmaps are the normal read-time validity authority. Slot bytes
  are ignored when the matching commit bit is clear.
- Static matrix aux regions are declared in the format spec, preallocated after
  slot payloads, excluded from commit bitmap validity, and exposed through
  `matrix_aux_len`, `read_matrix_aux`, and writer-only `write_matrix_aux`.
- With `integrity: crc32`, `VMAT` v1 includes an `MCRC` table after the slot
  region and any static aux regions. It stores CRC32 values for metadata
  tables, each commit map, and each dense cell slot.
- P0 commit operations are logical visibility operations under the explicit
  `flush`/`sync` durability model, not implicit per-cell fsync operations.
- Same-size matrix overwrites update the existing slot range and must not change
  file length. The overwrite clears the affected commit bit until the caller
  commits the replacement payload.
- Recovery may rebuild a damaged commit bitmap only from stronger evidence such
  as per-entry CRCs; normal reads do not infer validity from slot bytes.

## Runtime Policy

- Writer model is single-writer per file, enforced with a sidecar lock file.
- Reader model is snapshot-on-open; live tailing is out of scope for v0.1.
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
- `IndexPolicy` is a bitset-style policy with `scan_on_open`, `checkpoint_on_flush`, `block_offset_chain`, and `keyed_offset_chain`. Offset-chain policies are written automatically in `VARVE3` footers.
- `CommitPolicy::RecordFooter` treats valid record footers as the commit flag for each record.
- `CommitPolicy::TransactionMarker(on_flush|explicit)` appends internal `COMMIT_BLOCK_ID` marker records. Readers expose the latest marker-covered snapshot; writer open truncates uncommitted tail after the latest marker.
- `IndexPolicy::CheckpointOnFlush` writes an internal checkpoint record. Open uses the latest valid checkpoint and scans forward from its covered offset, with full-scan fallback for structurally invalid checkpoints. Transaction-marker formats currently prefer a full scan to preserve marker visibility semantics.
- `CompressionPolicy::VariableBlocks` and block-specific compression descriptors are feature-gated by the selected backend. The first backend is optional `compression-zstd`; compressed records are rejected when the backend is not enabled.

## Update And Merge

- `replace_fixed` allows in-place replacement only when encoded payload size is unchanged.
- `replace_rewrite` rewrites through a completed temporary file and atomically replaces the original file path.
- `replace(index, block, ReplaceStrategy)` is the policy-facing wrapper over fixed in-place replacement and full-file rewrite replacement.
- Keyed deletes use a common internal tombstone record.
- Keyed ops use user-defined `VarveMerge::Op`.
- Merge conflict ordering is shard order, then local sequence, then record ordinal; later delta shards win over base.
- `keyed_blocks::<T>()` is latest-put indexed access after tombstones.
- `materialized_keyed_blocks::<T>()` applies same-file puts, ops, and tombstones into current state.
- `VarveReader` and `VarveWriter` are additive handle wrappers for clearer read-only and write-capable workflows. Existing `VarveFile` APIs remain available.

## Codec Contract

- Scalar, option, fixed array, selected vector, tuple, `BTreeMap`, and `HashMap` codecs are canonical and endian-aware.
- `BTreeMap<K, V>` encodes in native sorted key order.
- `HashMap<K, V>` encodes keys in sorted order, requiring `K: Ord + Clone`, so equivalent maps produce stable bytes independent of insertion or hash iteration order.
- Decoding `HashMap<K, V>` preserves values but not insertion order.
- Custom field codecs are supported by implementing `VarveEncode` and `VarveDecode` for the field type. The derive macro uses the type's `WIRE_TYPE` in field descriptors and manifests.
- Enum-like values should use explicit custom codecs in v0.1; automatic enum representation inference is out of scope.
- Length and count limits are format-author policy. Varve's built-in decoders
  avoid large allocation-before-validation patterns where the remaining bytes
  can be checked generically, but domain-specific maximum string, sequence,
  map, chunk, or decompressed sizes should be enforced by the format's custom
  codecs, compression policy, adapter, or caller validation.
- `ChunkedBytes::decode_to_vec_limited(limit)` is provided for callers that
  need an explicit decompressed-size ceiling.

## Macro Contract

- `#[derive(VarveBlock)]` supports fixed and variable structs.
- Variable fields require non-zero unique field ids.
- Keyed blocks support single-field and composite keys.
- Duplicate key fields, missing key fields, duplicate block ids, and reserved block ids fail at compile time where macro input makes that possible.
- `varve_format!` supports the legacy registry form `pub struct Format { blocks: [A, B]; }` and the format-first form `pub format Format { blocks { fixed A(...) { ... } } }`.
- In format-first form, block structs, `VarveBlock` implementations, typed reader/writer wrappers, and typed read/write traits are generated from the format declaration.
- `varve_format!` supports `magic`, `version`, `endian`, optional `schema_hash`, optional `commit`, optional `integrity`, optional `index`, optional `recovery`, optional `manifest`, and `blocks`.
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
- `index` accepts either a single legacy identifier or a list such as `[scan_on_open, checkpoint_on_flush, block_offset_chain, keyed_offset_chain]`.
- `commit` accepts `none`, `record_footer`, or `transaction_marker(on_flush|explicit)`.
- Generated typed writers expose both `commit()` and `commit_durable()`.
  `commit()` is a logical visibility marker and does not imply fsync.
  `commit_durable()` writes commit-covered metadata/checkpoints, flushes and
  syncs data, writes the marker, then syncs the marker.
- `varve_format!` also supports optional `extension` and global `compression`. Compression syntax is `compression: none;` or `compression: variable_blocks(zstd, level = default|fast|best|N, header = record_explicit|file_explicit|format_contract, min_len = N, only_if_smaller = true|false, max_len = N);`.
- Per-block compression overrides are a runtime `FormatSpec::with_block_compression` API in v0.1, not a macro DSL clause.
- Legacy registry formats expose raw `VarveReader`/`VarveWriter` handles. Format-first declarations expose typed `FormatReader`/`FormatWriter` wrappers where methods such as `push_user`, `delete_user`, `users`, `commit`, `commit_durable`, `flush`, and `sync` are generated from block declarations.
- Duplicate top-level `varve_format!` keys are compile errors. The derive
  macro rejects generic block structs in v0.1 with a direct diagnostic; use a
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
- CRC integrity detects payload/footer tampering when feature-enabled.
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
- `docs/nptdms-adapter-boundary.md` records the boundary between Varve generic
  API obligations and external TDMS adapter obligations.
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
  - entries are ordered by record offset and end no later than the covered offset,
  - no entry points to the checkpoint record itself,
  - CRC validation passes for the checkpoint payload when integrity is enabled.
- For `IndexPolicy::CheckpointOnFlush`, open should scan from the header until the latest valid checkpoint, then rebuild the index from the checkpoint and scan only records after the covered offset.
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

- `replace_fixed` remains same-size in-place only.
- `replace_rewrite` is explicit full-file rewrite for direct replacement.
- `replace(index, block, ReplaceStrategy::{FixedInPlace, RewriteFile})` exposes the policy choice directly.
- Delta/op append remains the recommended append-friendly update path.
- Compact API: `compact_keyed_file::<T, P>(spec, input, output)`.
- Compact reads one file, materializes final keyed state for `T`, drops tombstones and ops, writes only final values to a new output file, and writes metadata/checkpoint according to output format policy.
- Base+delta compact API: `compact_keyed_files::<T, P>(spec, base, deltas, output)`.
- Base+delta compact reads the base and ordered delta shards, applies the same shard-order conflict semantics as merge, and atomically publishes only final values.
- Compact is single keyed block type per call. Non-keyed blocks and unrelated block ids are not copied in this first API.

### Writer Safety

- The sidecar lock remains the default single-writer guard.
- Lock files use text payload version `varve-lock-v1` and contain process id, creation timestamp in Unix milliseconds, and target path for diagnostics.
- `FormatSpec::inspect_writer_lock(path)` returns `Ok(None)` when no lock exists, `Ok(Some(WriterLockInfo))` for a valid lock, and `WriterLockMalformed` for malformed metadata.
- Stale-lock breaking is not automatic by default. `FormatSpec::open_with_lock_policy(path, policy)` is the explicit opt-in entry point.
- `WriterLockBreakPolicy::Refuse` preserves the default behavior and returns `WriterLockHeld`.
- `BreakIfOlderThan` is timestamp based; `BreakIfProcessAbsent` is best-effort and conservative when process liveness cannot be determined.
- Malformed locks are never automatically broken in v0.1.
- Atomic rewrite/compact output uses same-directory temp files, flushes and syncs temp contents, then replaces or renames into place.
- Reader behavior remains snapshot-on-open; readers do not tail live writers.

### Unknown Fields

- Unknown variable field ids with known wire types are skipped and dropped by default.
- Unknown wire type values are malformed data and return `UnknownWireType`.

### Zero-Copy And Mmap

- Mmap and zero-copy are opt-in features only. The `zero-copy` crate feature implies the `mmap` feature because the first raw-read API is mmap-backed.
- Default typed decode remains owned canonical decoding.
- Initial mmap scope exposes read-only payload windows from `VarveFile::mmap_payloads() -> MmapPayloads`.
- `MmapPayloads` owns a read-only mmap plus a cloned snapshot index and `FormatSpec`.
- `MmapPayloads::payload_window(entry)` accepts only entries that exactly match the cloned snapshot index, preventing forged public offsets from exposing arbitrary file bytes.
- `MmapPayloads::block_payload_window::<T>(index)` returns the typed block ordinal payload bytes or `None` when out of range.
- `VarveFile::mmap_matrix()` and `VarveReader::mmap_matrix()` expose
  `MmapMatrix`, a read-only mmap plus a cloned VMAT layout snapshot.
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
- See `docs/adapter-toolkit-design.md` for the detailed design and the example
  of TDMS as one possible instantiation.
