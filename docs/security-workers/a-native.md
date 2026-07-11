# Worker Brief: A-Native Layout And Diagnostics

## Objective

Harden native header/footer canonicality and make diagnostics consume bounded,
snapshot-bound runtime APIs without changing native wire bytes.

## Owned Files

- `crates/varve-core/src/native_layout.rs`
- `crates/varve-core/src/diagnostics.rs`
- one new dedicated native diagnostics/security integration test

## Deliverables

- Reject nonzero reserved native file-header flags and preserve all canonical
  header/footer bytes.
- Diagnostics use frozen bounded/snapshot helpers; no path reopen or unbounded
  materialization introduced.
- Golden and malformed-reserved-field evidence.

## Forbidden

- Editing `file.rs`, exports/errors/macros, mmap constructors, sidecars, locks,
  layout/matrix/codec, shared tests/docs, or dependencies.
- New fields, versions, schema inputs, or a second limits/snapshot abstraction.

## Validation

- Focused new test, native/layout golden tests, all-feature check, fmt, diff check.

Report changed files, tests, and any API request to the orchestrator.
