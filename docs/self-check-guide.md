# Varve Self-Check Guide

This guide is for application developers who need to decide whether a failure is
caused by a Varve library bug, a bad format declaration, a wrong API call, a
feature flag mismatch, or damaged file bytes.

## Static Format Diagnostics

Run diagnostics before creating long-lived files:

```rust
let report = AppFormat::diagnostics();
assert!(report.passed(), "{report:#?}");
println!("computed schema hash: {:#018x}", report.computed_schema_hash);
```

`FormatSpec::diagnostics()` checks the static registry, schema hash state,
matrix declarations, and required feature gates. Each diagnostic has:

- `severity`: `Info`, `Warning`, or `Error`
- `domain`: where to look first
- `code`: stable machine-readable category
- `message` and optional `hint`

## File Diagnostics

Use file diagnostics when a real file fails to open or read:

```rust
let report = AppFormat::diagnose_file("data.varve");
for item in &report.items {
    eprintln!("{:?} {:?} {}: {}", item.severity, item.domain, item.code, item.message);
}
```

This opens the file read-only, validates record payloads logically, checks
compression and CRC feature gates, compares embedded manifests when present, and
reports matrix recovery findings.

## End-To-End Self Test

Self-tests create a disposable Varve file, write caller-provided sample values,
reopen it read-only, and verify that the generated/runtime APIs roundtrip those
values:

```rust
let report = AppFormat::self_test("self-check.varve")
    .with_block(Point { x: 1, y: 2 })
    .with_keyed_block(User { id: 7, name: "Ada".into(), flags: 0 })
    .cleanup(true)
    .run();

assert!(report.passed(), "{report:#?}");
```

For matrix formats:

```rust
let report = MatrixFormat::self_test("matrix-check.varve")
    .with_dims(MatrixDimensions::from_pairs([("scan", 2), ("ch", 2)]))
    .with_matrix_cell(MatrixKey::new(0, 0), Cell { value: 9 })
    .with_uncommitted_matrix_cell(MatrixKey::new(1, 1), Cell { value: 11 })
    .with_matrix_aux("scratch", 0, [1, 2, 3, 4])
    .cleanup(true)
    .run();
```

Use a temporary path. Self-test creation truncates the target file.

## Domain Meaning

- `FormatDefinition`: the declared schema or policy is suspicious.
- `CallerUsage`: API call, dimensions, key, block type, commit state, or sample
  setup is wrong.
- `FeatureGate`: enable the required Cargo feature, such as `integrity` or
  `compression-zstd`.
- `FileData`: the bytes on disk do not match the supplied static format or
  failed integrity/decode checks.
- `Environment`: filesystem permissions, writer lock, or concurrent process
  issue.
- `LibraryInvariant`: a self-test roundtrip mismatch after successful write and
  read. First check custom codecs; if those are simple/correct, minimize and
  report as a likely Varve bug.

## Practical Rule

If `diagnostics()` passes and a self-test using your actual generated format and
sample values passes, but your application path still fails, start by checking
caller-owned policy: dimensions, key construction, commit timing, sidecar
generation, migration functions, and custom codec semantics.
