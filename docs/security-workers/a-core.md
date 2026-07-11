# Worker Brief: A-Core Native Storage

## Objective

Implement native snapshot-bound I/O, resource enforcement, safe replacement,
ordering, recovery, sidecars/locks, merge/compact, and native mmap gates.

## Owned Files

- `crates/varve-core/src/file.rs`
- `crates/varve-core/src/collections.rs`
- `crates/varve/tests/compression.rs`
- `crates/varve/tests/format_dsl.rs`
- `crates/varve/tests/mmap_zero_copy.rs`
- `crates/varve/tests/policies.rs`
- `crates/varve/tests/properties.rs`
- `crates/varve/tests/roundtrip.rs`
- `crates/varve/tests/storage_hardening.rs`
- `crates/varve/tests/tdms_model.rs`
- new native storage security tests

## Read-Only Inputs

- Final Spec, routing, precursor brief
- `format.rs`, `error.rs`, `snapshot.rs`, `lib.rs`, macro code
- `matrix.rs`, `layout.rs`, `native_layout.rs`, `diagnostics.rs`

## Deliverables

- Every high-level indexed/materializing native read uses captured
  `SnapshotFile`; path entry methods remain low-level only.
- Streaming CRC, all applicable limits, fallible claim-sized reservation, and
  cumulative materialization accounting.
- Streaming same-size COW `replace_fixed`, validated before atomic publish;
  `unsafe replace_fixed_in_place_exclusive`; no safe in-place strategy.
- Sequence uniqueness and one shared order/reducer including tombstone evidence.
- Narrow recovery classification; fatal policy errors never truncate.
- Header-first bounded sidecars; refuse lock without parsing; bounded inspection.
- Native/matrix mmap wrapper length/index gates. Do not change matrix helper
  behavior during fan-out; flag wrapper needs for post-C integration.
- Streaming variable rewrite instead of whole-file payload materialization.

## Forbidden

- Editing serial contracts, native layout/diagnostics, custom layout, matrix,
  codec, shared tests/docs/perf, manifests, or dependencies.
- New persisted bytes or metadata.

## Validation

- Owned tests plus new path-swap, old/new generation, sparse limit, duplicate
  sequence, recovery fatality, sidecar/lock, mmap, and merge/compact tests.
- `cargo check -p varve --all-targets --all-features`
- Report performance probes and any post-C wrapper requests.

Report changed files, tests, failures, and integration notes.
