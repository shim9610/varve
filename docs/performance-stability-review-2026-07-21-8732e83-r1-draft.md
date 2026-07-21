# Varve Performance and Stability Review

## Round 1 Candidate Report

- Review date: 2026-07-21 (Asia/Seoul)
- Target commit: `8732e8314d6df57940994fccc5b97944f88b290a`
- Branch: `codex/dev-next`
- State: unverified candidates awaiting independent Round 2 fact checking
- Library source changes made by reviewers: none

## Method

Round 1 used five reviewers that started with clean conversation context. Their
scopes were matrix storage, generated/resident APIs, storage publication and
locking, large-scale performance, and release assurance. The reviewers were
not given another reviewer's output and were instructed not to trust commit
messages or earlier review reports.

An application update destroyed the first set of reviewer sessions before any
result could be recovered. Those contexts were discarded in full. Five new
clean-context reviewers repeated the work. Exploration was later stopped and
they were required to return only evidence they had actually read. Claims
without source anchors are not included below.

This document freezes Round 1 candidates. It does not assert that any candidate
is true. A different clean-context group must confirm, narrow, or reject every
item against the target source and tests.

## Runtime and API Candidates

### R1-API-01: Generated keyed cache update can panic after publication

The generated keyed push/delete methods reserve capacity before append but call
`HashMap::insert` after the record or tombstone is authoritative. Moving or
pre-cloning the key removes allocation/clone failure, but `insert` can still
invoke user-defined `Hash` or `Eq`, which may panic. If unwinding is caught, the
writer may remain accessible with a stale predecessor cache.

Candidate evidence:

- `crates/varve-macros/src/lib.rs:5706`
- `crates/varve-macros/src/lib.rs:5725`
- duplicated trait implementation near `lib.rs:5816` and `lib.rs:5832`

Round 2 must verify exact call ordering and whether pre-append calls already
exercise every `Hash`/`Eq` path that the post-append insert could invoke.

### R1-API-02: Migration may bypass the registered source-type contract

`blocks_migrated` appears to check matching IDs and filter by source version,
then decode `From` without calling the registration/fingerprint gate used by
ordinary typed reads. An unrelated user implementation claiming the same ID
and version may therefore be accepted as a migration source.

Candidate evidence:

- `crates/varve-core/src/file.rs:3546-3568`
- ordinary typed registration at `crates/varve-core/src/collections.rs:227`

### R1-CODEC-01: Custom decoder allocations are not mechanically charged

`VarveDecode` implementations receive a decoder with a materialization-charge
API, but the trait and custom-codec guide may not require allocating custom
decoders to call it. A custom codec can allocate from an input count without
decrementing `max_materialized_bytes`.

Candidate evidence:

- `crates/varve-core/src/codec.rs:150`
- charge API at `codec.rs:353-369`
- `crates/varve-core/src/collections.rs:57`
- `docs/custom-codec-guide.md:103`

Round 2 must distinguish a library-enforced security promise from a documented
unsafe/custom-implementation responsibility.

## Matrix Candidates

### R1-MAT-01: Commit-map rebuild zeroes the primary index before republishing

Rebuild appears to zero the persisted page index and then republish live pages
one by one. An interruption in that interval may leave an incomplete index.
Without a filesystem allocation map, reopen enumerates only that incomplete
index and may omit still-present commit-map pages.

Candidate evidence:

- `crates/varve-core/src/matrix.rs:3658`
- republish loop at `matrix.rs:3666`
- no-map enumeration at `matrix.rs:4756`

Round 2 must determine whether rebuild is allowed to destroy the old map, what
state is poisoned on failure, and whether reopen necessarily emits a fatal
finding rather than silently reporting cells uncommitted.

### R1-MAT-02: Allocation failure after bitmap persistence may leave stale memory

`apply_commit_bit` appears to persist the digest and bitmap byte before the
fallible in-memory `set_byte` allocation. The public wrapper reportedly poisons
the writer only for `Error::Io`. A later operation on the same byte could then
derive from stale memory and overwrite the persisted bit.

Candidate evidence:

- `crates/varve-core/src/matrix.rs:3597-3600`
- allocation in `matrix.rs:1371`
- writer poisoning at `crates/varve-core/src/file.rs:4621`

### R1-MAT-03: Durable hook error is ambiguous after durable publication

The durable matrix helper appears to commit and sync the cell, then return the
post-commit hook's ordinary error. Callers cannot distinguish a failed write
from a durable write whose notification failed.

Candidate evidence:

- `crates/varve-core/src/file.rs:3902-3907`
- existing expectation at `crates/varve/tests/matrix.rs:745`

Round 2 must check the public contract and whether the hook is explicitly
defined as post-publication with an expected partial-success outcome.

### R1-MAT-04: CRC rebuild may allocate before applying the resident limit

The CRC rebuild loop appears to materialize a sparse bitmap page before
charging the resulting resident delta.

Candidate evidence:

- `crates/varve-core/src/matrix.rs:3030-3031`

Possible consequence: transient memory can exceed the configured resident
ceiling before a typed limit error is returned.

## Storage Candidate

### R1-LOCK-01: Writer-lock marker follows a filesystem alias

The writer opens `<target>.lock` with ordinary path-following behavior, accepts
an empty object as an unused marker, and truncates/writes through the captured
handle. A hard link, or an accepted symbolic link, to an unrelated empty file
may therefore cause writer acquisition to modify that file.

Candidate evidence:

- `crates/varve-core/src/file.rs:9603`
- empty-marker handling at `file.rs:9623`
- rewrite at `file.rs:9646`
- truncating helper at `file.rs:9791`

Round 2 must separate single-writer aliasing from modification of a foreign lock
marker object and check platform-specific final-component protections.

## Performance Candidates

### R1-PERF-01: Stream sidecar staging has a quadratic suffix scan

`stage_state_records` reportedly searches the remainder of the batch for each
record to determine the final record per block. For `R` block-diverse records,
this is `Theta(R^2)` comparisons; across chunks of size `K`, `Theta(NK)`.

Candidate evidence:

- `crates/varve-core/src/stream.rs:1229-1240`
- default chunk size `crates/varve-core/src/disk_index.rs:28`
- use near `stream.rs:1435`

### R1-PERF-02: Matrix CRC recovery lacks progress and cancellation

The public rebuild API appears blocking and scans every matrix cell, hashing
each slot and reading stored CRC state without an observer or cancellation
check. This is a feature/operability candidate rather than an algorithmic
superlinear claim.

Candidate evidence:

- `crates/varve-core/src/file.rs:4014-4020`
- `crates/varve-core/src/matrix.rs:3020-3031`
- slot hashing at `matrix.rs:3852-3864`

### R1-PERF-03: Matrix cell operations repeatedly scan schema vectors

Block, category, and dimension lookup appears to use repeated vector scans per
cell. For `N` operations, `B` blocks, `D` dimensions, and `G` categories, the
candidate overhead is `O(N(B + D + G))`.

Candidate evidence:

- `crates/varve-core/src/matrix.rs:1879-1907`
- write path `matrix.rs:2320`
- descriptor lookup near `matrix.rs:3150`

### R1-PERF-04: Clearing an already-clear commit bit still hashes and writes

Writing a fresh/uncommitted matrix cell appears to prepare and apply
`value=false` before payload write even when the commit bit is already clear.
The path may still hash a 4 KiB page and write its digest and bitmap byte.

Candidate evidence:

- `crates/varve-core/src/matrix.rs:2359-2367`
- page hash at `matrix.rs:3292-3303`
- apply path `matrix.rs:3570-3599`

## Release and Assurance Candidates

### R1-REL-01: Wildcard-path exception is broader than its comment

`deny.toml` keeps `wildcards = "deny"` but enables the global
`allow-wildcard-paths = true`. The surrounding explanation claims a narrow
exception for one published proc-macro dev dependency, while the boolean may
allow every wildcard path dependency in the graph.

Candidate evidence:

- `deny.toml:25-48`
- path-only dev dependency at `crates/varve-macros/Cargo.toml:38`

The root command currently passes. Round 2 must verify the cargo-deny semantics
with controlled temporary manifests or primary documentation rather than infer
scope from the option name.

### R1-DOC-01: Rustdoc CI configuration may not match docs.rs

CI describes the all-feature rustdoc build as matching the project's docs.rs
configuration, but the publishable manifests may lack
`[package.metadata.docs.rs] all-features = true` while default features are
empty.

Candidate evidence:

- rustdoc job in `.github/workflows/ci.yml`
- published manifests under `crates/*/Cargo.toml`

### R1-CI-01: CI compiles fuzz targets but does not execute dynamic campaigns

The fuzz job appears to run metadata, compile, audit, and deny only. Seven fuzz
binaries exist, but no CI step appears to run a bounded fuzz campaign or a
sanitizer-enabled test.

Candidate evidence:

- fuzz job in `.github/workflows/ci.yml`
- targets in `fuzz/Cargo.toml`

Round 2 must compare this with the exact public assurance claim. Absence of a CI
campaign is a gap only if release documentation says the campaign is enforced
or the project requires it as a release gate.

## Round 1 Verified Corrections

The reviewers found source evidence for these prior corrections, subject to
the normal limits of a read-only review:

- category clear totals commit, quarantine, CRC-valid payload, and page-index
  residency before clearing and subtracts both counters with checked arithmetic;
- ordinary commit and validity mutations refund precharged payload residency on
  failures before in-memory materialization;
- generated delete clones its borrowed key before the authoritative tombstone;
- invalid counted matrix index entries do not truncate enumeration of later
  entries;
- built-in variable codecs checked in Round 1 validate wire extents and charge
  materialization before reservation;
- generic resident keyed mutation performs fallible tail reservation before
  append;
- resident checkpoint cadence avoids a reverse index scan per flush;
- indexed sidecar rebuild contains bounded transaction, progress, and
  cancellation checks;
- merge/compact remains explicitly resident rather than claiming PB-scale
  memory;
- six facade features and workspace crate version `0.4.0` are aligned.

## Mechanical Evidence Before Round 2

| Gate | Result |
| --- | --- |
| formatting | Pass |
| all-feature/all-target locked check | Pass |
| all-feature/all-target Clippy with `-D warnings` | Pass |
| three rustdoc builds with `-D warnings` | Pass |
| full `varve-test-runner` workspace suite | Pass |
| generated keyed atomicity focused test | Pass, 4 tests |
| matrix integrity/scaling focused binary | Pass |
| root dependency audit | Pass, 79 dependencies |
| root dependency policy | Pass |
| fuzz locked all-target check | Pass |
| fuzz dependency audit | Pass, 50 dependencies |
| fuzz dependency policy | Pass |
| verified package archives | Pass, three `0.4.0` crates |
| public API fixture compile | Pass |

Release benchmarks and independent fact checking were not complete when this
candidate report was frozen.
