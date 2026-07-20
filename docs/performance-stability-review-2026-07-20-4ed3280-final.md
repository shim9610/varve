# Varve Performance and Stability Review

## Final Fact-Checked Report

- Review window: 2026-07-20 to 2026-07-21 (Asia/Seoul)
- Target commit: `4ed32808c1f99c46da4babcd371453e373b2de06`
- Branch: `codex/dev-next`
- Review type: read-only implementation review plus deterministic validation
- Library source changes made by this review: none
- Round 1 draft: `docs/performance-stability-review-2026-07-20-4ed3280-r1-draft.md`

## Executive Verdict

The Opus revision closes most of the previous review's large defects. In
particular, generic keyed mutations now reserve their resident tail-map slot
before append, small `HashMap` capacity classes are tested, matrix page-index
occupancy is checked, matrix primitive aliases compile, merge memory reporting
is explicitly structural rather than an upper bound, sidecar publication
rechecks primary identity, and real `0.4.0` package archives can be built.

The target is still **not ready for public release**. Three runtime defects and
two deterministic release-gate failures remain:

1. whole-category matrix clear drops resident structures without refunding all
   associated counters;
2. a failed first-page matrix mutation can retain a payload precharge;
3. generated keyed delete clones an owning key after the tombstone is already
   authoritative;
4. the full parallel test suite is nondeterministic because two sidecar tests
   share one overwriteable process-global hook;
5. the documented root `cargo deny check` gate fails on a path-only dev
   dependency.

Two additional matrix statements overpromise integrity visibility and
worst-case open complexity. They do not establish memory unsafety or unchecked
payload acceptance, but they are public-contract defects and should be fixed
before publication.

## Review Method

Round 1 used clean-context reviewers for matrix storage, codec/API behavior,
storage publication, and release evidence. Every reviewer started without the
main conversation history and received only its assigned scope. Contexts that
could not read the repository or did not return a conclusion were discarded.
The surviving candidates were frozen in the Round 1 draft before Round 2.

Round 2 used a different clean-context group. It independently classified each
candidate as confirmed, narrowed, or rejected from source and tests. Narrow
replacement reviewers were used where a broad review could not establish a
claim. No partial result from a discarded context is included here.

The main reviewer reconciled those reports with the full workspace runner,
focused reproductions, formatting, check, Clippy, rustdoc, package archives,
dependency policy, the public API fixture, and five release benchmark runs.

This review did not start a fresh randomized fuzz campaign or sanitizer run.
The fuzz workspace was compiled and its independent lockfile was audited, so
this report does not claim fresh fuzz or sanitizer coverage for this commit.

## Release-Blocking Runtime Findings

### F-01: Matrix category clear leaves phantom resident charges

**Verdict: confirmed. Severity: high.**

`clear_category` computes `released` from the commit bitmap payload and its
quarantine payload at `crates/varve-core/src/matrix.rs:2507-2517`. For a cell
category it also clears the CRC-valid bitmap and its persisted page index at
`matrix.rs:2535-2547`, clears the in-memory CRC-valid bitmap at
`matrix.rs:2578-2580`, and clears the commit bitmap and quarantine at
`matrix.rs:2582-2585`.

Only `resident_bitmap_bytes` is reduced at `matrix.rs:2587-2591`.
CRC-valid payload residency omitted from `released`, plus commit/CRC page-index
residency represented by `resident_page_index_bytes`, is not refunded.

Repeated populate/clear cycles can therefore accumulate phantom residency and
eventually return `LimitExceeded` after the corresponding memory has been
released. This is a bounded-resource accounting failure, not on-disk data loss.

**Required correction:** compute all category-owned payload and index residency
before clearing, subtract both counters with checked arithmetic, and add a
CRC-enabled repeated populate/clear test that asserts both counters return to
their baseline after every cycle.

### F-02: Failed first-page mutation can retain a payload precharge

**Verdict: confirmed and narrowed. Severity: high.**

`apply_commit_bit` computes a new page's materialization cost and charges it at
`crates/varve-core/src/matrix.rs:3483-3486`. Page-index publication, digest
write, and bitmap-byte write remain fallible at `matrix.rs:3487-3497`. The
payload page is not materialized in memory until `set_byte` at
`matrix.rs:3498-3501`.

An early `?` after the charge can therefore leave the payload counter charged
without a resident payload page. The broader Round 1 claim about every index
charge leaking was too wide: page-index reservation failure has an explicit
refund, and a successfully retained index entry represents real resident
memory even when a later disk operation fails.

**Required correction:** use a rollback guard for the payload precharge until
`set_byte` settles the page delta, or reorder the operation so no fallible step
can escape between charge and materialization. Exercise injected failures at
index, digest, and bitmap-write boundaries and assert counters and retry
behavior after each failure.

### F-03: Generated keyed delete clones after authoritative mutation

**Verdict: confirmed and narrowly scoped. Severity: high.**

The generated keyed writer reserves map capacity before mutation at
`crates/varve-macros/src/lib.rs:5717`, calls the authoritative delete at
`lib.rs:5718`, and evaluates `key.clone()` only during insertion at
`lib.rs:5719`. The generated writer-trait implementation repeats the order at
`lib.rs:5821-5823`.

`VarveWriter::reserve_keyed_tail_slot` reserves the `HashMap` slot but, by its
own contract, charges only inline `(K, u64)` storage and cannot reserve heap
owned by `K` (`crates/varve-core/src/file.rs:1710-1715`). A heap-owning clone
can allocate, and a user-defined `Clone` can panic. That happens after the
tombstone and writer state are authoritative but before the generated tail map
is updated. The comment claiming the post-append insert cannot allocate or fail
at `file.rs:1694-1702` is consequently too strong.

This is a panic/allocation-failure consistency defect, not a normal returned
`Result::Err` path. Generic `VarveFile::delete` does not share it because it
moves the already encoded `Vec<u8>` into its cache at `file.rs:2725-2730`.

**Required correction:** clone the key after reservation but before calling the
authoritative delete, then move the owned key into the map. Add a generated API
test with a controllably panicking `Clone`, catch the panic, and prove file
length, sequence, index, and visible state remain unchanged.

## Release-Gate Findings

### F-04: Sidecar publication tests overwrite one global hook

**Verdict: confirmed. Scope: test infrastructure only.**

The test hook is a single
`Mutex<Option<(PathBuf, Hook)>>` at
`crates/varve-core/src/stream.rs:1463-1466`. Registration replaces the entire
option at `stream.rs:1469-1472`. Consumption checks the path, but that does not
prevent a second test from overwriting the first test's still-armed hook.

The indexed and stream tests arm different paths concurrently. When the stream
test overwrites the indexed hook, indexed rebuild completes successfully and
the expectation at `crates/varve-core/src/indexed.rs:2055-2057` fails.

Mechanical evidence is deterministic at suite scope:

- the full workspace runner failed in the indexed test;
- the all-feature parallel `varve-core --lib` run failed in the same test;
- the focused all-feature test passed 20 of 20 repetitions;
- the all-feature library passed serially with 114 passed, 0 failed, 2 ignored.

No product-path regression was established. Product code rechecks primary
identity before and after sidecar publication and performs identity-checked
retirement. The defect still makes the required parallel CI gate unreliable.

**Required correction:** use a test-only path-keyed hook map that removes only
the matching path, or serialize all users behind one scoped test guard. Reject
duplicate registration for the same path so abandoned hooks are visible.

### F-05: Root dependency-policy gate fails

**Verdict: confirmed. Severity: release-blocking configuration defect.**

`crates/varve-macros/Cargo.toml:38` declares
`varve = { path = "../varve" }` as a dev dependency. Cargo treats the missing
version as a wildcard requirement for dependency-policy purposes, while
`deny.toml:27` sets `wildcards = "deny"`. The root `cargo deny check` therefore
reports `bans FAILED`, contradicting the blocking gate configured at
`.github/workflows/ci.yml:292-304` and documented in the README.

`cargo package --locked -p varve-core -p varve-macros -p varve` still passes
because this path-only dev dependency is stripped from the published manifest.
Packaging success does not make the root policy gate pass.

**Required correction:** give the path dev dependency an explicit matching
version, or document and narrowly configure an intentional policy exception.
The root command itself must pass before release.

## Public Contract Findings

### F-06: Allocation-map fallback overpromises corruption visibility

**Verdict: confirmed documentation and integrity-contract defect.**

When filesystem allocation extents are unavailable, `pages_to_visit` returns
persisted index pages only at `crates/varve-core/src/matrix.rs:4623-4628`. A
never-indexed bitmap page changed outside Varve is then not visited. Allocation
maps are unavailable on unsupported platforms/filesystems and are deliberately
discarded after `MAX_TRACKED_EXTENTS` at `matrix.rs:101-107`.

The migration guide and matrix storage design acknowledge this limitation, but
public rustdoc at `matrix.rs:1018-1025` says enumeration is unaffected and no
page could be skipped without reading it. Source comments at
`matrix.rs:119-124` also imply unchanged stray-byte detection without
qualifying allocation-map availability.

No unchecked payload acceptance was demonstrated: an omitted page remains
unloaded rather than trusted. The problem is incomplete corruption visibility
relative to the stated contract.

**Required correction:** state that every persisted-index page is verified and
allocation-map pages are additionally checked only when the platform supplies
a usable map. If complete stray-byte detection is required on every platform,
the format needs authenticated coverage independent of filesystem extent data.

### F-07: Matrix-open live-state summary is not a worst-case bound

**Verdict: confirmed documentation and performance-contract defect.**

`pages_to_visit` materializes persisted page ids and every page overlapped by
reported extents at `crates/varve-core/src/matrix.rs:4623-4653`, then visits the
result at `matrix.rs:4691-4695`. A densely allocated bitmap extent can therefore
produce work proportional to the bitmap region even when few bits are live.

For `L` indexed pages, `A` allocation-derived candidates, and `U` distinct
pages, candidate work is expected `O(L + A)`, temporary candidate memory is
`Theta(L + A)`, and page-byte I/O is up to `O(4096U)`. In a dense extent,
`A` and `U` can be proportional to the region page count.

The detailed statement at `docs/performance.md:292-302` is close to the
implementation. The high-level claim that create and open are bounded by live
state at `docs/performance.md:152-159` is not a worst-case guarantee.

**Required correction:** use the candidate-page bound in every public summary
and describe live-state behavior as the sparse-allocation operating case, not
an unconditional bound.

### F-08: CI feature-count comment is stale

**Verdict: confirmed. Severity: low.**

`.github/workflows/ci.yml:55-58` says seven optional features and 128 subsets.
The facade exposes six optional switches at `crates/varve/Cargo.toml:17-29`, so
the power set is 64. The configured nine jobs remain a deliberate representative
matrix; only the explanatory count is wrong.

## Narrowed or Rejected Candidates

### Keyed-tail admission order

Generic keyed push and delete encode one canonical key payload before checking
`max_keyed_tail_bytes` (`crates/varve-core/src/file.rs:2497-2503` and
`file.rs:2725-2730`). Cache rebuild also encodes one payload before its
accumulated keyed-tail check at `file.rs:2542-2556`.

This is not a post-mutation error path: encoding, limit, and reservation errors
all occur before append. The limit is documented as a retained keyed-tail
structure limit, while the individual key remains governed by its codec/read
limits. No behavioral defect is established unless `max_keyed_tail_bytes` is
intended to bound every transient serializer allocation. A targeted unchanged-
state rejection test is still advisable.

### Sidecar product publication

The failing sidecar test does not prove a product defect. The implementation
checks native identity before publication, atomically publishes the sidecar,
reopens and re-identifies the primary after publication, and retires a stale
sidecar by verified identity. A non-cooperating pathname replacement cannot be
made one filesystem-atomic transaction with sidecar publication, but the
implemented contract is detection and cleanup rather than silent acceptance.

## Independently Verified Corrections

- `checkpoint_on_flush` retains linear-growth regression coverage at
  `crates/varve/tests/checkpoint_growth.rs:72` and `:240`; the previous
  cumulative `O(N^2)` construction was not reintroduced.
- VMAT v4 uses checked occupancy rather than terminator scanning. Invalid
  counted entries are fatal findings while later entries remain enumerable.
- Clearing the final bit evicts its sparse payload page, and persisted index
  removal uses swap-with-last ordering.
- `HashMap` and `BTreeMap` structural allocation checks run before allocation.
  Small `HashMap` classes 1 through 14 and varied layouts through 64 entries
  have focused passing tests. Public contracts exclude key-owned heap,
  allocator metadata, and hash-table control/load-factor storage where those
  bytes cannot be observed.
- `KeyedMergeEstimate` and `peak_resident_structural_bytes()` are documented as
  structural estimates, explicitly not upper bounds, in source rustdoc and the
  API reference.
- Matrix direct, chained, module-path, and array aliases compile. Variable-width
  `String`/`PackedBitmap`, unsupported shadowed types, and tuples fail through
  compile-time fixtures.
- Generic keyed push/delete reserves every fallible tail-map capacity step
  before append. F-03 is limited to the generated delete's post-append clone.
- Product sidecar rebuild performs pre-publication and post-publication identity
  checks plus identity-checked retirement.
- Workspace and publishable manifests are aligned on `0.4.0`; verified package
  archives for all three crates were produced and the staged-consumer fixture
  passed against them.

## Mechanical Validation

| Gate | Result |
| --- | --- |
| `cargo fmt --all -- --check` | Pass |
| workspace all-feature/all-target locked `cargo check` | Pass |
| workspace all-feature/all-target locked Clippy with `-D warnings` | Pass |
| three crate rustdoc builds with `-D warnings` | Pass |
| full `varve-test-runner` workspace suite | **Fail, F-04** |
| focused F-04 test repeated with all features | Pass, 20/20 |
| parallel all-feature `varve-core --lib` | **Fail, F-04** |
| serial all-feature `varve-core --lib` | Pass, 114 passed, 2 ignored |
| zstd-only compression binary | Pass, 11 tests |
| integrity-only compression binary | Pass, 6 tests |
| public API fixture | Pass |
| all-feature ignored performance smoke | Pass |
| root `cargo audit` | Pass, 79 dependencies scanned |
| root `cargo deny check` | **Fail, F-05** |
| fuzz workspace locked all-target check | Pass |
| fuzz lockfile audit | Pass, 50 dependencies scanned |
| fuzz dependency policy | Pass |
| verified `0.4.0` package archives | Pass |
| root/fuzz/rename/public-fixture offline metadata | Pass |
| `git diff --check` before report creation | Pass |

Successful test sessions reported cleanup verified empty. Three failed runner
sessions were retained intentionally as failure evidence under the local temp
directory. Build caches under `target` were retained.

## Release Benchmark

The release example used 10,000 records, one warmup, and five recorded runs.
The active Windows power scheme was **Power saver**. The comparison is useful
for broad regression screening only; it is not a hardware-normalized throughput
claim.

| Operation | Median ms | Five-run range ms | Versus `060851a` |
| --- | ---: | ---: | ---: |
| fixed encode/decode | 1.475 | 1.472-1.492 | -1.1% |
| variable encode/decode | 10.488 | 10.452-10.664 | -0.6% |
| fixed append | 345.084 | 331.166-383.286 | +11.9% |
| fixed open/scan | 40.492 | 40.062-43.558 | +1.0% |
| merge keyed files | 480.930 | 459.397-520.201 | +0.2% |
| compact merged | 501.629 | 444.526-544.632 | +10.0% |
| compact base+deltas | 509.431 | 500.121-599.727 | +5.0% |

Every median remains inside the documented 15 percent manual gate. There is no
broad performance regression in this sample, although append and compact-merged
are close enough to the gate to keep tracking after F-01 through F-03 are fixed.

## Security and Stability Assessment

No new memory-unsafety path, unchecked offset/length acceptance, or path that
returns attacker-controlled payload without validation was established in this
review. The confirmed runtime findings instead affect bounded-resource
accounting and mutation-result atomicity. F-06 affects corruption visibility on
allocation-map fallback paths.

Passing `cargo audit` means the checked lockfiles contained no advisory matched
by the local advisory database. It is not a general security certification.
Likewise, compiling the fuzz workspace is not equivalent to running a fresh
fuzz or sanitizer campaign.

## Release Decision

Do not publish `4ed3280` as `0.4.0` in its current state.

Fix F-01 through F-05 first, then rerun the parallel full workspace runner,
focused failure-injection regressions, root dependency policy, package staging,
and the release benchmark. Correct F-06 through F-08 in the same stabilization
branch so public integrity and performance contracts match the implementation.

After those corrections, rerun the already established fuzz/sanitizer and
fault-injection gates rather than treating this source review as a substitute.
