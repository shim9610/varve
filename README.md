# Varve

Varve is a Rust library for building append-friendly, segment-oriented custom
binary formats from typed block definitions.

This repository is intentionally small at the moment: it contains the first
runtime, proc-macro, and facade crates described in `docs/architecture.md`.

## Self-Check

Generated formats expose diagnostics and self-test helpers:

```rust
let diagnostics = AppFormat::diagnostics();
let file_report = AppFormat::diagnose_file("data.varve");
let self_test = AppFormat::self_test("self-check.varve")
    .with_block(Point { x: 1, y: 2 })
    .cleanup(true)
    .run();
```

These reports classify failures as format definition, caller usage, feature
gate, file data, environment, or library invariant issues. See
`docs/self-check-guide.md`.

## Local Verification

```powershell
cargo fmt --check
cargo test
cargo test --all-features
cargo clippy --all-targets --all-features -- -D warnings
```
