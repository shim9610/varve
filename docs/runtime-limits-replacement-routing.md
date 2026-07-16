# Runtime Limits And Replacement Work Routing

## Core Runtime Policy

Owned files:

- `crates/varve-core/src/format.rs`
- `crates/varve-core/src/layout.rs`

Deliverables:

- standard fallback and overlay resolution;
- new `*_with_resource_limits` constructor family;
- compatibility-preserving tightening methods;
- focused unit tests for resolution.

## Core Replacement

Owned files:

- `crates/varve-core/src/file.rs`
- `crates/varve-core/src/traits.rs`
- `crates/varve-core/src/error.rs`
- `crates/varve-core/src/lib.rs`
- `crates/varve/tests/roundtrip.rs`

Deliverables:

- `ReplacementInfo`, `VarveReplaceBlock`, and `replace_block`;
- footer/checkpoint/CRC-aware streaming rewrite;
- keyed validation and sequence-preserving semantics;
- snapshot, failure, grow/shrink, chain, and transaction tests.

## Macro And Typed API

Owned files:

- `crates/varve-macros/src/lib.rs`
- `crates/varve/tests/compile.rs`
- `crates/varve/tests/ui/*limits*`
- `crates/varve/tests/format_dsl.rs`

Deliverables:

- optional and partial `limits` grammar;
- generated resource-limit constructors;
- generated replacement trait implementations and typed methods;
- in-place keyed-tail offset translation;
- compile and generated-API tests.

## Integration And Documentation

Main-agent ownership:

- cross-scope API integration;
- `README.md`, `docs/spec.md`, guides, examples, and changelog;
- formatting, clippy, full tests, property tests, performance smoke, audits, and
  independent implementation verification.
