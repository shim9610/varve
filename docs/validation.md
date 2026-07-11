# Validation Notes

## Review Findings

- Merge order was unstable when delta shard local sequence numbers were lower than base records.
  - Resolution: merge ordering now compares shard ordinal first, then local sequence, then record ordinal.
- `keyed_blocks::<T>()` did not apply tombstones or ops.
  - Resolution: `keyed_blocks::<T>()` now applies tombstones to latest-put lookup; `materialized_keyed_blocks::<T>()` applies puts, ops, and tombstones for full state materialization.
- Endian mismatch was accepted.
  - Resolution: file open now rejects header endian mismatch.
- Macro field id validation was incomplete.
  - Resolution: variable fields now reject zero and duplicate field ids at compile time.
- `replace_rewrite` rewrote the original file in place.
  - Resolution: rewrite now completes a same-directory temporary file, syncs it, and atomically replaces the original path.
- Mutating APIs allowed unregistered or version-mismatched block types in some paths.
  - Resolution: push, delete, op append, fixed replace, rewrite replace, typed access, materialization, and merge now enforce the format registry block id and version.
- Normal `open` could truncate corrupt tails when the format recovery policy was `TruncateTail`.
  - Resolution: normal `open` and `open_readonly` are strict; truncation is only performed by explicit recovery open APIs.
- Merge/materialized keyed state decoded user records without checking block version.
  - Resolution: merge/materialization now returns `BlockVersionMismatch` before decoding mismatched records.
- Index checkpoints were written but not consumed on open.
  - Resolution: open now consumes the latest valid checkpoint and scans forward from the covered offset, with structural fallback to full scan.
- Next-wave API contracts were underspecified for checkpoint payloads, manifest/migration, compact, writer lock stale handling, unknown fields, mmap, and zero-copy.
  - Resolution: `docs/spec.md` now pins concrete next-wave contracts before worker implementation.
- Optional embedded schema manifest was missing.
  - Resolution: `ManifestPolicy`, `MANIFEST_BLOCK_ID`, macro `manifest: embedded`, and `VarveFile::schema_manifest()` are implemented.
- Migration had no explicit user-function scaffold.
  - Resolution: `VarveMigration<From, To>` and `blocks_migrated::<From, To, M>()` are implemented; normal typed reads still reject version mismatch.
- Compact was only available through base+delta merge.
  - Resolution: `compact_keyed_file` materializes one keyed block type and writes a clean output file.
- Performance checks were only documented.
  - Resolution: `crates/varve/tests/perf_smoke.rs` provides an ignored runnable smoke suite.
- Independent re-verification found checkpoint completeness, CRC checkpoint handling, and compact durability concerns.
  - Resolution: checkpoint entries must now exactly match the observed prefix before the checkpoint record; CRC mismatch is checked before checkpoint fallback; `compact_keyed_file` now writes through a same-directory temp file, flushes/syncs, then atomically publishes.
- Independent zero-copy/mmap validation found underspecified writer lock metadata, independent `zero-copy`/`mmap` feature behavior, and forgeable mmap payload entries.
  - Resolution: lock metadata is now text version `varve-lock-v1`; `inspect_writer_lock` reports malformed locks instead of breaking them; `zero-copy` implies `mmap`; `MmapPayloads::payload_window` accepts only entries present in its cloned snapshot index.
- Mmap and zero-copy scaffolds were deferred.
  - Resolution: `VarveFile::mmap_payloads`, `MmapPayloads` payload windows, unsafe marker traits, and unsafe raw-reference calls are implemented behind opt-in features.
- Explicit stale-lock handling was deferred.
  - Resolution: `WriterLockInfo`, `WriterLockBreakPolicy`, `FormatSpec::inspect_writer_lock`, and `FormatSpec::open_with_lock_policy` are implemented; existing open/create/recover paths still refuse locks by default.
- Performance smoke caught an accidental O(n^2)-style mmap typed ordinal lookup path.
  - Resolution: `MmapPayloads` now precomputes a snapshot entry set and block-id-to-index map so forged-entry checks and typed block ordinal lookup do not scan the full index on every access.
- Schema manifests lacked field-level metadata for useful debugging and schema fingerprinting.
  - Resolution: derive now emits `VarveBlock::FIELDS`; manifest payload v2 stores field descriptors; `FormatSpec::schema_debug_dump` and `computed_schema_hash` expose deterministic diagnostics.
- `HashMap` would be non-deterministic if encoded in hash iteration order.
  - Resolution: `HashMap<K, V>` encoding sorts cloned keys before writing; tests compare byte output from different insertion orders.
- Base+delta compact previously required merge output plus a second compact pass.
  - Resolution: `compact_keyed_files` materializes base and ordered deltas directly and publishes through the same atomic temp/sync path.
- Merge and compact had separate output publication paths.
  - Resolution: `merge_keyed_files`, `compact_keyed_file`, and `compact_keyed_files` now share the atomic publish helper.
- Clean API/UX review found weak macro diagnostics around malformed key strings and misleading `#[varve(default)]` metadata on fixed blocks.
  - Resolution: key strings are now fallibly parsed and validated; empty key lists, empty segments, invalid identifiers, duplicates, missing fields, and fixed-block defaults are covered by trybuild failures.
- Unknown-field behavior was covered by property tests but not by a direct policy regression.
  - Resolution: policy tests now explicitly assert skip-known-wire and reject-unknown-wire behavior.
- Custom codec hooks were implicit through public traits.
  - Resolution: policy tests now use a custom field codec and assert its `WIRE_TYPE` reaches schema metadata; docs describe trait-based custom codecs as the 0.2 hook.
- Public read/write API intent was still centered on the broad `VarveFile` type.
  - Resolution: additive `VarveReader` and `VarveWriter` wrappers are exported, macro helpers expose `create_writer/open_writer/open_reader`, and roundtrip tests cover the common handle workflow.
- File-explicit and format-contract compression modes had no per-record `u64` logical length slot.
  - Resolution: no-envelope compression modes use the record header `uncompressed_len_hint` and require `max_uncompressed_len <= u32::MAX`; record-explicit keeps a `u64` length in the `VCMP` envelope.
- Compression metadata could be lost through checkpoint and rewrite paths.
  - Resolution: record headers, `RecordIndexEntry`, checkpoint payload v2, and rewrite all preserve `uncompressed_len_hint` and compressed flags.
- Format-contract compression could reuse an arbitrary nonzero schema hash with an incompatible static spec.
  - Resolution: contract mode requires the pinned file header hash to equal `FormatSpec::computed_schema_hash()`, and `FormatSpec::with_computed_schema_hash()` is provided for authoring.
- Compression risked hidden work on scan paths.
  - Resolution: `scan()`, `RecordIndexEntry::read_payload`, and mmap windows remain physical-byte APIs; logical decompression is only used by typed reads, migrations, and merge/materialization.
- Independent implementation verification found no blockers but recommended direct coverage for disabled-backend reads and compression+mmap physical windows.
  - Resolution: compression tests now include a hand-written compressed-record fixture for no-backend read failure and an mmap test asserting compressed payload windows expose the physical `VCMP` envelope while typed reads decompress.

## Mechanical Checks

- `cargo test -p varve --test compile`
- `cargo test -p varve --test compression`
- `cargo test -p varve --test compression --features compression-zstd`
- `cargo test -p varve --test compression --features compression-zstd,mmap`
- `cargo fmt`
- `cargo test`
- `cargo test --all-features`
- `cargo test -p varve --features mmap --test mmap_zero_copy`
- `cargo test -p varve --features mmap,zero-copy --test mmap_zero_copy`
- `cargo test -p varve --features mmap --test perf_smoke -- --ignored --nocapture`
- `cargo test -p varve --features mmap,zero-copy --test perf_smoke -- --ignored --nocapture`
- `cargo check --tests`
- `cargo check --all-features --tests`
- `cargo test -p varve --test policies`
- `cargo test -p varve --test roundtrip`
- `cargo test -p varve --test compile`
- Performance smoke/benchmark checks for major feature slices before integration.

## License Check

The actual dependency graph from `cargo metadata --all-features` was reviewed. Direct and transitive dependencies expose commercially usable permissive licenses or permissive alternatives, including MIT, Apache-2.0, BSD-2-Clause, Zlib, and Unlicense expressions. Entries with LGPL in an `OR` expression also include MIT/Apache alternatives.

`windows-sys` is used directly for Windows atomic replace behavior and conservative process-liveness checks for explicit lock-breaking policy. It is covered by the same permissive dependency audit.

`zstd` is optional under `compression-zstd` and is included in the same all-features license audit.

## Deferred Work

- Add richer zero-copy policies for aligned multi-record layouts if a later use case needs them.
- Add explicit stale-lock policy variants for create/recovery paths if normal `open_with_lock_policy` is not enough for real workflows.
- Add broader public API polish: macro convenience helpers for lock inspection/open-with-policy if needed, and richer error ergonomics.
- Extend manifest policy with strict mismatch validation if needed; current manifest is diagnostic and optional.
- Extend migration beyond explicit block migration vectors if later required.
- Add live tailing reader if later required.
