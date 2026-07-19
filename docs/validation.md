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

## Resident Merge/Compact Scale Contract

- Adversarial review found `merge_keyed_files`, `compact_keyed_files`, and
  `compact_keyed_file` documented without a memory bound while
  `docs/petabyte-io-main-draft.md` listed merge/compact inside the
  petabyte-permitted operation set.
  - Resolution: the family is documented as resident-only and explicitly not
    PB-scale (`Theta(records + decoded bytes) + O(K-live log K-live)` time,
    `O(K-ever + largest resident input index + retained live values)` memory,
    tombstoned keys retained, nothing spilled to disk, no bounded-memory
    external merge/compact exported). `estimate_keyed_merge` gives a decode-free
    pre-flight bound and `merge_keyed_files_with_key_limit`,
    `compact_keyed_files_with_key_limit`, and `compact_keyed_file_with_key_limit`
    fail with `Error::LimitExceeded { resource: "merge distinct keys", .. }` at
    the key boundary and publish no output. Covered by
    `crates/varve/tests/high_cardinality.rs::resident_merge_estimate_reports_the_k_ever_bound`,
    `::resident_merge_and_compact_fail_typed_at_the_key_ceiling`, and
    `::resident_single_input_compact_fails_typed_at_the_key_ceiling`.
- Adversarial review found the generic `push`/`push_info` writing a truncated
  keyed offset chain for keyed blocks.
  - Resolution: those entry points refuse a keyed block on a `keyed_offset_chain`
    format with `Error::KeyedChainRequiresKeyedApi`, and the maintaining
    `push_keyed`/`push_keyed_info` (plus a chain-maintaining `delete`) are the
    documented replacement. Covered by
    `crates/varve/tests/resident_contracts.rs`.

## 2026-07-19 Performance And Defensive-Engineering Review Remediation

Findings from
[`adversarial-performance-security-review-2026-07-19-4e07a3f-final.md`](adversarial-performance-security-review-2026-07-19-4e07a3f-final.md)
and where each is now pinned by a test:

- PERF-01, matrix commit-bit mutation hashed the whole category bitmap.
  - Resolution: per-4-KiB-page commit digests; a mutation rehashes one page.
    `crates/varve/tests/matrix_integrity_scaling.rs::commit_bit_mutation_hashing_cost_is_independent_of_cell_count`
    and `::a_single_mutation_hashes_exactly_one_page_anywhere_in_the_map`.
- PERF-02, matrix create/open metadata scaled per cell in I/O and RAM.
  - Resolution: no per-cell metadata written at create, sparse in-memory
    bitmaps, and open bounded by the filesystem allocated-range map.
    `::create_metadata_bytes_do_not_scale_with_cell_count`,
    `::resident_bitmap_bytes_after_open_do_not_scale_with_cell_count`,
    `::untouched_matrix_holds_no_resident_bitmap_pages`,
    `::open_bitmap_bytes_read_do_not_scale_with_cell_count`,
    `::whole_category_clear_cost_does_not_scale_with_cell_count`. Detection
    strength is separately pinned by
    `::stray_bytes_in_a_skipped_region_are_still_detected`,
    `::published_page_corruption_is_detected_and_rebuild_recovers`, and
    `::never_written_and_zero_written_pages_are_distinguishable`; the layout
    break by `::previous_layout_version_artifact_is_rejected_typed`.
- PERF-03, resident merge/compact retained `K-ever` while docs implied PB scale.
  - Resolution: see the previous section (contract plus caller-side guards).
- PERF-04, registry pruning was linear on every open/invalidate.
  - Resolution: amortized sweep after a doubling threshold.
    `crates/varve/tests/index_shared_readers.rs::registry_cost_per_open_does_not_scale_with_live_identities`.
- PERF-05, resident predecessor lookup reverse-scanned the index per append.
  - Resolution: maintained sorted block tails, `O(log B)` and no index reads.
    `crates/varve/tests/resident_contracts.rs::append_time_predecessor_lookup_never_reads_the_resident_index`.
- RES-01, variable field-id bookkeeping bypassed materialization accounting.
  - Resolution: 8 bytes charged per distinct id above 63 before reserving.
    `crates/varve/tests/codec_hardening.rs::large_field_id_bookkeeping_is_charged_to_the_materialization_budget`
    and `::duplicate_large_field_ids_are_rejected_without_extra_charge`.
- API-01, first-use registration could seed a wrong schema identity.
  - Resolution: the `FormatSpec` identity is the authority; the registry is a
    cache of an already-validated result.
    `crates/varve/tests/manual_trait_invariants.rs::impostor_registered_first_is_rejected_against_the_format_identity`.
- API-02, generic keyed push omitted the predecessor chain.
  - Resolution: see the previous section.
- API-03, disk-index descriptors could invoke a mismatched decoder.
  - Resolution: descriptors carry the block schema fingerprint, the registration
    gate runs per descriptor before any decode, and the fingerprint is folded
    into the plan digest.
    `crates/varve/tests/scalable_identity.rs::mismatched_descriptor_plan_is_rejected_before_any_decode`
    (asserts zero decoder calls) and `::matching_descriptor_plan_still_builds_and_indexes`.
- API-04, custom nested codecs collided in computed schema identity.
  - Resolution: `SCHEMA_ID` on both codec traits, folded transitively into block
    fingerprints, with a compile-time rejection of identity-less field codecs.
    `crates/varve/tests/schema_hash_contract.rs::custom_nested_codecs_with_equal_spelling_cannot_share_an_identity`,
    `::nested_block_codec_identity_is_transitive`,
    `::built_in_codec_identities_are_structural`, and the `tests/ui`
    compile-fail fixtures.
- STO-01, a same-object equal-length primary rewrite was accepted with a stale
  sidecar.
  - Resolution: a per-create nonce inside the primary plus a bounded
    primary-generation witness in the sidecar; both reader and writer open
    refuse a mismatch with `DiskIndexError::PrimaryGenerationMismatch` and
    rebuild recovers.
    `crates/varve/tests/scalable_identity.rs::same_object_equal_length_rewrite_is_refused_and_rebuild_recovers`,
    `::stream_state_sidecar_refuses_a_rewritten_primary`, and
    `crates/varve/tests/stream_append_perf_contract.rs::primary_generation_witness_stops_scanning_once_its_window_is_full`.
- REL-01, the committed fuzz lockfile was stale and CI could not see it.
  - Resolution: the lockfile was regenerated and CI gained a blocking locked
    fuzz job whose step order prevents any tool from regenerating a stale
    lockfile, with a trailing `git diff --exit-code`. The clean-archive job also
    builds the archived fuzz workspace. Not executable locally; verified by
    running each underlying command against the committed lockfile.
- TEST-01, Windows crash tests could raise interactive error reporting.
  - Resolution: the crash child suppresses WER before inducing a fault.
    `crates/varve/tests/scalable_crash_faults.rs`. On the measured host WER UI
    was already disabled system-wide, so suite wall time was unchanged within
    noise; the change makes the gate unattended on a default machine.
- Coverage gap named by the review: direct public stream/indexed
  publication-failure result tests.
  - Resolution: `crates/varve/tests/publication_failure_results.rs`, covering
    parent-sync-pending on stream and indexed create, batch-failure poisoning
    on both writers, the retryable zero-record batch, and an unfaulted control.

## Deferred Work

- Add richer zero-copy policies for aligned multi-record layouts if a later use case needs them.
- Add explicit stale-lock policy variants for create/recovery paths if normal `open_with_lock_policy` is not enough for real workflows.
- Add broader public API polish: macro convenience helpers for lock inspection/open-with-policy if needed, and richer error ergonomics.
- Extend manifest policy with strict mismatch validation if needed; current manifest is diagnostic and optional.
- Extend migration beyond explicit block migration vectors if later required.
- Add live tailing reader if later required.
