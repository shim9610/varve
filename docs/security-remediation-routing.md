# Security Remediation Work Routing

Status: accepted routing; clean validator required edits integrated

## Final Objective

Implement the frozen security-remediation specification, close actionable
ADV-001 through ADV-010, preserve valid wire bytes, and pass functional,
adversarial, dependency, license, and performance gates.

## Current-Turn Objective

Complete the entire remediation in this workspace. No public release, push, or
cryptographic-authentication work is part of this turn.

## Trusted Assets And Contamination Boundaries

- Authority: `docs/security-remediation-spec.md` after validation,
  `docs/adversarial-security-review-2026-07-10.md`, existing byte assertions,
  and current public API docs.
- Trusted: format declarations, custom codecs, unsafe raw marker implementations,
  and author-selected ceilings.
- Untrusted: all file/sidecar/lock bytes, path bindings, runtime dimensions, and
  concurrent external storage activity.
- Worker output outside its owned write scope is rejected unless the orchestrator
  explicitly re-routes it.
- No worker may change wire versions, schema-hash inputs, dependency versions,
  unsafe mmap preconditions, or external TDMS/BMP fixtures.

## Stop Conditions

- Any valid-byte or schema-hash change without an approved migration.
- Any safe API claim that requires an unenforceable external condition without
  naming that condition.
- Any unbounded claim-sized allocation on an ordinary untrusted-input path.
- More than 15% unexplained large-case regression on an unchanged hot path.
- New dependency or unsafe block without focused audit, tests, and license check.
- Any CRC, sequence, recovery, sidecar, or limit implementation that needs a new
  persisted field, record, version, or schema input returns to design.

## Serial Precursor: Shared Contracts

Owned files:

- `crates/varve-core/src/format.rs`
- `crates/varve-core/src/error.rs`
- `crates/varve-core/src/snapshot.rs` (new)
- `crates/varve-core/src/lib.rs`
- `crates/varve-macros/src/lib.rs`
- compile/UI tests dedicated to limits and generated strict decoding

Deliverables:

- final limits structs, tightening rules, builder methods, exports, DSL syntax,
  generated finite/trusted open helpers, API-to-limit enforcement helpers, and
  errors;
- internal positional `SnapshotFile` contract with frozen ownership,
  generation identity, checked range API, `Send`/`Sync` expectations, and
  replacement behavior;
- checked variable-field length and duplicate-known-field behavior;
- every worker-required public export predeclared in `lib.rs`; downstream
  workers do not edit serial-owned exports, errors, or macro files;
- no runtime consumer implementation outside the owned files.

Acceptance:

- API/DSL focused tests pass;
- existing declarations compile unchanged unless final spec explicitly requires
  an opt-in marker;
- schema hashes and emitted bytes remain unchanged;
- downstream worker briefs can compile against the frozen contracts.

## Parallel Unit A-Core: Native Snapshot And Storage

Owned files:

- `crates/varve-core/src/file.rs`
- `crates/varve-core/src/collections.rs`
- native/storage/security integration tests assigned in the final brief

Read-only contracts:

- shared `ReadLimits` and `SnapshotFile` APIs;
- final replacement and sequence policy.

Deliverables:

- handle-bound lazy reads for collections, metadata, migration, diagnostics,
  merge, compact, and rewrite;
- streaming CRC and native open/materialization limits;
- snapshot-preserving fixed replacement and any explicitly approved expert mode;
- unsafe exclusive in-place replacement with no safe strategy routing;
- consistent sequence/tombstone ordering;
- bounded sidecar and lock parsing;
- recovery only for classified recoverable tails;
- fatal preservation for limit/canonical/duplicate/schema/committed-prefix
  failures before and after transaction markers;
- native mmap count/length gates.
- file-backed sidecar, lock, mapping, alignment, lifetime, and unsafe wrapper
  ownership. Matrix helper implementations remain read-only during fan-out.

Forbidden:

- editing native-layout/diagnostics/layout/matrix/codec implementations;
- changing `file.rs` matrix wrapper behavior before Unit C is integrated;
- changing wire bytes or decompression timing during index scan;
- whole-file replacement buffering.

Validation:

- path replacement, old/new generation, duplicate sequence, sparse payload,
  lock, sidecar, recovery, merge/compact, mmap, and replacement performance tests.

## Parallel Unit A-Native: Native Layout And Diagnostics

Owned files:

- `crates/varve-core/src/native_layout.rs`
- `crates/varve-core/src/diagnostics.rs`
- a new dedicated native diagnostics/security integration test

Deliverables:

- strict native reserved fields and exact canonical header/footer parsing;
- diagnostics that consume frozen bounded/snapshot APIs without path reopening;
- no persisted metadata, wire bytes, layout fields, or schema inputs added.

Forbidden:

- editing `file.rs`, mmap constructors, sidecars, locks, matrix, custom layout,
  shared tests, or exports;
- inventing a second snapshot or limits abstraction.

Validation:

- native header/footer golden bytes, malformed reserved fields, bounded
  diagnostics, and no-wire-change evidence.

## Parallel Unit B: Custom Layout

Owned files:

- `crates/varve-core/src/layout.rs`
- new custom-layout security integration tests

Deliverables:

- `LayoutReader` bound to `SnapshotFile`;
- file/segment/metadata/raw range limits;
- path replacement and many-segment regressions;
- unchanged TDMS/BMP physical layout behavior and streaming writer semantics.

Forbidden:

- changing layout declaration syntax except consuming shared limit policy;
- buffering streamed writes or changing external adapter fixtures.

Validation:

- layout suites, external harnesses, path replacement, bounded whole/range reads,
  and layout performance smoke.

## Parallel Unit C: Matrix And Matrix Mmap

Owned files:

- `crates/varve-core/src/matrix.rs`
- new matrix-limit tests plus narrowly assigned matrix test updates

Deliverables:

- pre-allocation cell/slot/bitmap/CRC/resident checks;
- bounded aux access and matrix-sidecar contract hooks;
- reduced redundant bitmap ownership where safe;
- unchanged O(1) cell access, commit ordering, CRC, recovery, mmap, and zero-copy
  semantics.

Forbidden:

- redesigning VMAT, changing slot/bitmap bytes, or weakening mmap unsafe terms;
- editing native file wrappers, sidecars, file-backed mmap constructors, exports,
  or shared tests. C owns pure matrix shape/index/range logic over frozen
  snapshot byte windows; A-Core owns storage objects and unsafe lifetimes.

Validation:

- matrix functional/hardening/property tests, sparse derived-length probes, mmap
  and zero-copy tests, matrix performance smoke.

## Parallel Unit D: Canonical Codec

Owned files:

- `crates/varve-core/src/codec.rs`
- codec-focused tests

Deliverables:

- final strict/permissive field-flag and map-order policy;
- strict increasing map input with `HashMap` `Ord` decode and O(n) pending-entry
  validation;
- no duplicate map keys or non-canonical values accepted beyond final spec;
- bounded chunk decode remains available and canonical output stays identical;
- no avoidable allocation-before-validation.
- `crates/varve-core/src/chunks.rs` is read-only in this remediation; its existing
  bounded API remains covered by main integration tests.

Forbidden:

- macro generation, file framing, compression policy, or public format DSL edits.

Validation:

- canonical byte assertions, hostile count termination, ordering/flag tests,
  compression feature on/off checks, and codec micro performance.

## Integration Order

1. Freeze and land shared contracts.
2. Launch A-Core, A-Native, B, C, and D with clean contexts and disjoint write
   scopes.
3. Review each diff as a bounded result; reject scope leakage.
4. Integrate A-Core and C, then perform one main-owned `file.rs` matrix-wrapper
   continuation against C's frozen helpers. B, A-Native, and D can integrate in
   any order after the precursor.
5. Resolve exports and shared tests centrally; workers never edit them.
6. Run a clean implementation verifier over final spec, briefs, diffs, and logs.
7. Apply required corrections, synchronize docs, and run all mechanical gates.

## Exact Test And Artifact Ownership

| Owner | Existing writable tests/examples |
| --- | --- |
| Serial precursor | `tests/compile.rs`, `tests/ui/*` limits/macro fixtures only |
| A-Core | `tests/compression.rs`, `format_dsl.rs`, `mmap_zero_copy.rs`, `policies.rs`, `properties.rs`, `roundtrip.rs`, `storage_hardening.rs`, `tdms_model.rs`, plus new native storage tests |
| A-Native | new dedicated native diagnostics/security test only |
| B | `tests/adapter_toolkit.rs`, `layout_adapter_hardening.rs`, `layout_dsl.rs`, layout/TDMS/BMP examples and their common adapter module |
| C | `tests/matrix.rs`, `matrix_hardening.rs`, plus new matrix-limit tests |
| D | `tests/codec_hardening.rs` only |
| Main integration | `tests/self_check.rs`, `perf_smoke.rs`, `examples/perf_bench.rs`, Cargo manifests/lockfile, scripts, all docs, and any cross-slice test |

Fixture and expected-output files follow their owning test. Generated trybuild
stderr belongs only to the serial precursor. Workers may request, but not make,
changes to a main-owned artifact.

## Specialized Verification

- Architecture verifier: snapshot and replacement semantics, recovery
  classification, mmap boundary, and no-wire-change evidence.
- Performance verifier: before/after five-run medians for unchanged hot paths and
  separate reporting for policy-changed fixed replacement, using the baseline,
  workload, sample count, and 15% rule frozen in the Final Spec. Add focused
  allocation/copy counts for CRC streaming and copy-on-write replacement.
- Dependency/license verifier: `cargo audit`, `cargo deny check`, lockfile diff,
  and no unapproved direct dependency.
- Unsafe/concurrency verifier: mmap and exclusive replacement stress, plus Miri
  or sanitizer execution where the installed toolchain supports it; unavailable
  tooling is reported rather than silently skipped.

## Final Mechanical Evidence

- formatting, all-target/all-feature check, full tests, clippy with warnings
  denied, property/trybuild tests, audit, deny, external Python harnesses,
  performance comparison, `git diff --check`, and final worktree review.
