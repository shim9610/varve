# Security Hardening Work Routing

## Provenance And Integration

- All work starts from `f7a9369` in the dev history.
- `origin/main` has unrelated history and must not be merged, rebased, or used
  for three-dot comparisons.
- Worker changes outside their declared write scope are rejected.
- Root manifests and the lockfile have one owner.
- Shared error/API contracts land before parallel behavioral work.

## Serial Precursors

1. Freeze this spec, routing document, validation method, tool versions, and
   baseline performance evidence.
2. Update dependency manifests and `Cargo.lock`.
3. Land the coordinated `Error` contract and no other production behavior.

## Worker Packets

| Unit | Exclusive write scope | Read-only context | Required evidence |
| --- | --- | --- | --- |
| A supply chain | root and crate `Cargo.toml`, `Cargo.lock`, `deny.toml` | architecture/spec docs | exact graph, MSRV, license, audit, deny |
| B codec | `crates/varve-core/src/codec.rs`, new `crates/varve/tests/codec_hardening.rs` | error/API contract | canonical bool/map tests, unchanged bytes, no panic |
| C storage/mmap | `crates/varve-core/src/file.rs`, new `crates/varve/tests/storage_hardening.rs` | error/API contract, traits, format, matrix APIs | sequence, extent, rollback, mmap contract and performance |
| D layout/adapter | `crates/varve-core/src/layout.rs`, `crates/varve-core/src/adapter.rs`, new `crates/varve/tests/layout_adapter_hardening.rs` | error/API contract, tempfile dependency | stream rollback, temp security/lifetime, count bounds |
| E matrix audit | `crates/varve-core/src/matrix.rs`, new `crates/varve/tests/matrix_hardening.rs` | format/file/error contracts | hostile offsets/counts and commit visibility; edit production only for reproduced defects |
| F macro/API compile | `crates/varve-macros/src/lib.rs`, `crates/varve/tests/compile.rs`, new mmap UI `.rs/.stderr` files | public runtime signatures | unsafe-call compile failure, hygiene, generated API parity |
| G docs | `README.md`, `docs/**` except these routing artifacts during implementation | accepted diffs and verifier reports | API, safety, migration, dependency and residual-risk sync |

Existing shared tests such as `roundtrip.rs`, `policies.rs`, `properties.rs`,
`layout_dsl.rs`, `matrix.rs`, and `mmap_zero_copy.rs` are read-only to workers.
The main orchestrator makes any required integration edits after reviewing
worker output.

## Boundary Contracts

- A alone changes dependency metadata.
- The orchestrator alone changes `error.rs` and both crate-root `lib.rs` files.
- B uses the frozen canonical error variants and cannot add public errors.
- C owns all sequence and file-backed mmap constructors. Every public mapping
  route must become unsafe, including reader forwarding methods.
- D owns `LayoutWriter` poison/rollback behavior and `AdapterInputFile`.
- E must preserve the existing commit-bit and explicit-sync ordering.
- F cannot redesign the DSL or generated reader/writer model.
- No worker changes wire constants, manifest/checkpoint versions, schema hash
  inputs, durability defaults, or performance thresholds.

## Fan-In Order

1. A dependency result.
2. Orchestrator error/API precursor.
3. B, C, D, E, and F from the same integrated precursor state.
4. Clean implementation verification.
5. Mechanical, security, compatibility, and performance gates.
6. G documentation sync.

## Stop Conditions

Stop integration for an ignored advisory, denied license/source, Rust version
above 1.95, undeclared worker file, unexplained wire/hash drift, implicit fsync,
an unproven safe mmap constructor, failed rollback reuse test, or a sustained
performance regression above the frozen threshold.

