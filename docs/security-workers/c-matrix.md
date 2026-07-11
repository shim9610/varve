# Worker Brief: C Matrix

## Objective

Enforce pre-allocation VMAT limits and reduce avoidable resident bitmap copies
while preserving exact VMAT bytes, O(1) cell access, CRC, mmap, and zero-copy
semantics.

## Owned Files

- `crates/varve-core/src/matrix.rs`
- `crates/varve/tests/matrix.rs`
- `crates/varve/tests/matrix_hardening.rs`
- new matrix limit/security tests

## Deliverables

- Decode only fixed/small descriptors before checking dimension, cell, metadata,
  bitmap, CRC, slot, file, and resident-memory ceilings.
- Checked aggregate accounting counts every owned bitmap copy.
- Bounded aux reads/writes through existing interfaces.
- Pure matrix helpers keep existing file-wrapper signatures where possible;
  report exact wrapper continuation needed in `file.rs`.
- Sparse fixtures fail before dangerous allocation.

## Forbidden

- Editing `file.rs`, sidecar/lock/file-backed mmap wrappers, serial contracts,
  layout/codec, shared tests/docs/perf, dependencies, or VMAT bytes.

## Validation

- Owned matrix suites/property tests, mmap/zero-copy behavior through existing
  tests where reachable, matrix perf smoke, fmt, all-feature check, diff check.

Report changed files, tests, helper signatures, and wrapper needs.
