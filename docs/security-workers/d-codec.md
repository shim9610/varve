# Worker Brief: D Canonical Codec

## Objective

Make built-in map decoding strictly canonical without changing canonical output
or adding allocation-before-validation.

## Owned Files

- `crates/varve-core/src/codec.rs`
- `crates/varve/tests/codec_hardening.rs`

## Read-Only

- `crates/varve-core/src/chunks.rs`
- macro-generated variable field code and all storage modules

## Deliverables

- Reject nonzero variable-field flags at the shared field-header decoder.
- Require strictly increasing `BTreeMap` and `HashMap` input keys; HashMap decode
  adds `Ord` and uses one pending entry for O(n) validation without clone/sort.
- Preserve duplicate-before-value rejection, hostile zero-width termination,
  bool canonicality, and every writer byte.

## Forbidden

- Editing macros, chunks, storage, errors/exports, manifests, docs, dependencies,
  or wire output.

## Validation

- Owned codec tests, feature on/off check, fmt, clippy for owned slice, diff check.

Report changed files, exact bound changes, tests, and performance observations.
