# Worker Brief: B Custom Layout

## Objective

Bind custom-layout readers to the opened file object and enforce all applicable
file, scan, segment, index, and region-read limits without changing physical
layout behavior.

## Owned Files

- `crates/varve-core/src/layout.rs`
- `crates/varve/tests/adapter_toolkit.rs`
- `crates/varve/tests/layout_adapter_hardening.rs`
- `crates/varve/tests/layout_dsl.rs`
- `crates/varve/examples/bmp_physical.rs`
- `crates/varve/examples/tdms_physical_adapter.rs`
- `crates/varve/examples/tdms_physical_reader.rs`
- `crates/varve/examples/tdms_physical_writer.rs`
- `crates/varve/examples/tdms_physical/common/adapter.rs`
- new layout security/limit tests

## Deliverables

- `LayoutReader` owns `SnapshotFile`; whole/range reads never reopen a path.
- Finite policy enforced before scan/index/range allocation; writer growth checked.
- Path replacement, many-segment, oversized region, and declaration-policy tests.
- TDMS/BMP byte behavior and streamed callback rollback remain unchanged.

## Forbidden

- Editing serial contracts, file/native/matrix/codec, shared tests/docs/scripts,
  dependencies, or external fixtures.

## Validation

- Owned Rust suites and examples; report external harness commands for main.
- Layout performance smoke, fmt, all-feature check, diff check.

Report changed files, tests, and integration notes.
