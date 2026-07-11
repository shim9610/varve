# Worker Brief: Shared Security Contracts

## Objective

Create the compiling shared contracts required by every remediation slice.
Follow `docs/security-remediation-spec.md` and
`docs/security-remediation-routing.md` exactly.

## Owned Files

- `crates/varve-core/src/format.rs`
- `crates/varve-core/src/error.rs`
- `crates/varve-core/src/snapshot.rs` (new)
- `crates/varve-core/src/lib.rs`
- `crates/varve-macros/src/lib.rs`
- `crates/varve/tests/compile.rs`
- `crates/varve/tests/ui/*` files created or changed for limits/macro behavior

## Deliverables

- One public `ReadLimit` scalar state and one public `ReadLimits` policy with all
  Final Spec fields, component-wise tightening, finite requirement helpers, and
  trusted-unbounded handling.
- `FormatSpec`/builder support that does not affect schema hashing or manifests.
- Format DSL `limits { ... }` and `limits: trusted_unbounded`, with duplicate,
  unknown, missing-policy, pass, and hygiene trybuild coverage.
- Generated finite/open-with-limits/trusted-unbounded entry points, with ordinary
  entry points unable to invoke a trusted bypass.
- Internal `SnapshotFile` using `Arc<File>` and platform positional reads with
  checked ranges, exact-read loops, metadata length, and bounded streaming.
- Checked variable field length conversion and duplicate-known-field rejection.
- All errors and exports required by downstream workers.

## Boundary Contracts

- Do not consume limits in file/layout/matrix/codec runtime paths yet.
- No wire/schema/dependency change.
- `SnapshotFile` is cloneable, `Send + Sync`, remains bound to one open object,
  and never opens a path.
- Downstream workers may consume but not modify these contracts.

## Validation

- `cargo fmt --all -- --check`
- `cargo test -p varve --test compile`
- `cargo check --workspace --all-targets --all-features`
- focused unit tests for limit meet/require/overflow and snapshot positional reads
- `git diff --check`

Report changed files, tests, public signatures, and downstream integration notes.
