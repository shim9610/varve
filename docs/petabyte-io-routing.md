# Petabyte I/O Work Routing

## Phase A: Foundations

### Worker A - Typed extents

- Owns `crates/varve-core/src/scalable_extent.rs` and
  `crates/varve-core/src/snapshot.rs`.
- Implements checked offset/length/snapshot/span types and makes snapshot growth
  fallible.
- Adds focused hostile-range and PiB representational tests.
- Must not edit stream/index/disk-index/macro files.

### Worker B - Redb checkpoint protocol

- Owns `crates/varve-core/src/disk_index.rs`.
- Implements metadata v2, state-only versus disk-plan mode, quick-repair
  transactions, bounded batch metadata writes, persistent-savepoint
  begin/clean/restore, tails, typed errors, and unit fault tests.
- Must not edit stream/index/macro files.

### Worker C - Macro plan and API generation

- Owns `crates/varve-macros/src/lib.rs` and new macro UI fixtures only.
- Generates canonical disk plans, chain-safe method exposure, and typed batch
  method signatures against the frozen API.
- Must not edit runtime files.

## Phase B: Integration

The main agent exclusively owns `stream.rs`, `indexed.rs`, `file.rs`, core
exports/errors, Cargo integration, and cross-module reconciliation. It removes
append readback, adds lazy/constant open, batch writes, plan validation,
checkpointed state, explicit restore/rebuild/verify entrypoints, and counters.

## Phase C: Independent Gates

### Worker D - Test and benchmark harness

- Owns new integration/probe files under `crates/varve/tests`, scripts, and
  performance documentation after runtime APIs compile.
- Adds isolated allocation/open/write/read counters, million-key probes, and
  crash subprocess cases.
- Does not alter runtime behavior.

An independent clean-context verifier reviews final spec compliance and diffs
before final mechanical tests.
