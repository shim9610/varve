# Scalable I/O Stabilization Work Routing

## Main Agent

Owns cross-module integration and final acceptance:

- freeze final spec and resolve validation findings;
- wire scan control into `stream.rs`, `indexed.rs`, exports, generated APIs,
  and typed errors;
- wire named fault points into native/sidecar durability boundaries;
- integrate worker patches, update public documentation, and run all gates;
- preserve ordinary-path zero-scan and performance invariants.

## Worker A: Scan Primitives

- Owned: new `crates/varve-core/src/scan_control.rs` and focused unit tests in
  that file only.
- Deliver: progress/token/options state machine with monotonic cadence,
  per-record cancellation checks, exact final notification, and no per-record
  allocation.
- Read-only context: `stream.rs`, `indexed.rs`, `error.rs`.
- Main agent performs API wiring to avoid overlap.

## Worker B: Fault Harness

- Owned: new `crates/varve-core/src/scalable_fault.rs` and new
  `crates/varve/tests/scalable_crash_faults.rs` only.
- Deliver: explicitly armed trace/abort hook registry, occurrence selection,
  child-process scenarios/oracles, and a complete required point catalog.
- Read-only context: `stream.rs`, `indexed.rs`, `disk_index.rs`, `file.rs`.
- Main agent inserts hook calls and Cargo feature wiring.

## Worker C: PiB Probe

- Owned: `crates/varve-core/src/snapshot.rs` PiB test module additions and any
  new platform-only probe helper file agreed with main agent.
- Deliver: explicit sparse marking, real offset write/sync/reopen/read, checked
  past-EOF rejection, allocation report, required-mode semantics, and cleanup.
- Must not alter production positional-read behavior.

## Worker D: Sidecar Fuzzing

- Owned: `fuzz/**`, `scripts/run-security-fuzz.ps1`, and the sidecar-fuzz section
  of `docs/fuzzing-and-fault-injection.md` only.
- Deliver: scalable fuzz format, isolated harness utilities, arbitrary,
  mutation, and state-machine targets, deterministic seeds, target runner,
  and bounded campaign commands.
- Must cap raw bytes and operation counts before filesystem or model growth.

## Verification Agent

After integration, a clean-context verifier receives the final spec, this
routing file, worker summaries, and `git diff`. It checks API compatibility,
durability ordering, cancellation publication races, PiB evidence, fuzz
coverage, zero-scan invariants, and missing tests before final mechanical runs.

