# Varve Performance and Stability Review

## Final Fact-Checked Report

- Review date: 2026-07-21 (Asia/Seoul)
- Target commit: `8732e8314d6df57940994fccc5b97944f88b290a`
- Branch: `codex/dev-next`
- Review type: two-round clean-context source review plus deterministic validation
- Library source changes made by this review: none
- Frozen Round 1 draft: `docs/performance-stability-review-2026-07-21-8732e83-r1-draft.md`

## Executive Verdict

The Opus revision fixes the eight findings recorded against `4ed3280`: matrix
clear accounting, precharge rollback, generated delete clone ordering, the
parallel sidecar-test hook, root dependency-policy execution, matrix fallback
wording, matrix-open complexity wording, and the CI feature count.

The revision is substantially healthier and shows no broad performance
regression. It is nevertheless **not ready for public `0.4.0` publication**.
Three correctness defects remain release blockers:

1. generated keyed writers still execute user-defined `Hash`/`Eq` after a
   record or tombstone is authoritative;
2. matrix commit-map rebuild clears the persisted page index before safely
   publishing its replacement, so interruption can hide committed pages;
3. a bitmap-page allocation failure after disk persistence leaves an ordinary
   matrix writer usable with stale in-memory state.

No confirmed finding is a Rust memory-safety defect or a path that accepts
unchecked attacker-controlled payload bytes. The release blockers concern
mutation-result atomicity and persistent/in-memory state agreement.

## Review Method

Round 1 used five reviewers with empty conversation context. Their scopes were
matrix storage, generated/resident APIs, storage publication and locking,
large-scale performance, and release assurance. They were not given each
other's findings. Candidates were frozen before fact checking.

Round 2 used four different reviewers, also with empty context, to classify the
frozen candidates from source, tests, and public contracts. They covered API,
matrix, performance, and release/locking candidates separately.

Two application restarts destroyed reviewer sessions. The first Round 1 group
and the first Round 2 group returned no usable result and were discarded in
full. No partial claim from those contexts appears here. The final report uses
only completed results from the replacement clean-context groups.

The main reviewer then checked the high-severity call ordering directly and
reconciled the reports with formatting, compilation, Clippy, rustdoc, the full
workspace runner, focused tests, package archives, dependency checks, the
public API fixture, and a release benchmark.

This review compiled the fuzz workspace but did not run a new randomized fuzz
or sanitizer campaign. Historical campaign evidence is not treated as fresh
evidence for this commit.

## Release-Blocking Findings

### F-01: Generated keyed cache update executes user code after publication

**Verdict: confirmed. Severity: high correctness, trusted-type scope.**

Generated keyed push obtains the key and predecessor, reserves the map slot,
appends the authoritative record, and only then calls `HashMap::insert` at
`crates/varve-macros/src/lib.rs:5699-5706`. Generated delete now correctly
clones its borrowed key before the tombstone, but still inserts after the
authoritative delete at `lib.rs:5715-5725`. The generated trait implementation
duplicates the same ordering near `lib.rs:5809-5833`.

`VarveWriter::reserve_keyed_tail_slot` calls `contains_key` and reserves table
capacity before append at `crates/varve-core/src/file.rs:1727-1740`. That avoids
post-append table growth, but it cannot guarantee that the later `insert` will
not invoke `Hash` or `Eq` again. A stateful or panicking user implementation can
succeed before append and panic during insertion after the persistent mutation.

If unwinding is caught, the writer remains reachable with a stale tail map. A
later same-key mutation can link around the already committed event. Without
catching, the caller still observes a panic after a persistent side effect.
Existing `generated_keyed_atomicity` tests cover panicking `Clone`, not
post-publication `Hash` or `Eq`.

This is not directly reachable from malformed file bytes when key traits are
ordinary derived implementations. Format-author code is trusted, but the API
and comments explicitly aim to prevent post-publication user-code failure, so
the remaining path should be closed before release.

**Required correction:** ensure no user trait executes after publication. The
strongest design is to maintain generated tails by canonical internal key bytes,
like the generic resident path. Otherwise stage a cache update whose final
post-append commit is allocation-free and user-code-free. Add inherent and
trait-route tests for independently panicking `Hash` and `Eq` on push/delete.

### F-02: Matrix rebuild destructively republishes its primary page index

**Verdict: confirmed. Severity: high.**

`write_commit_map_pages` collects old and rebuilt candidate pages, then zeroes
the complete persisted page-index region at
`crates/varve-core/src/matrix.rs:3652-3660`. It resets the in-memory index and
republishes materialized pages one by one at `matrix.rs:3661-3679`.

A crash or write failure after zeroing leaves a valid zero or partial occupancy
header. The loader reports only malformed headers or malformed entries as fatal
at `matrix.rs:4660-4710`; a valid incomplete count has no self-authenticating
witness that entries are missing. When allocation extents are unavailable,
`pages_to_visit` returns indexed pages only at `matrix.rs:4751-4755`.

Consequently, still-present commit-map pages omitted by the interrupted rebuild
are not visited. Previously committed cells can reopen as `NotCommitted`
without a fatal finding. Slots and CRC metadata may permit a later manual
rebuild, so this is loss of state visibility rather than payload erasure, but it
is silent until recovery is explicitly repeated.

`rebuild_matrix_commit_from_crc` poisons the current writer on I/O error, but
writer poisoning does not repair the persisted partial index. Existing tests
cover successful rebuild without an allocation map, not interruption between
index reset and complete republication.

**Required correction:** publish a new page-index generation atomically, or
persist and authenticate an explicit rebuild-in-progress/generation marker that
makes reopen fail closed and resume/rebuild. Add process-level interruption and
injected entry/header-write tests with allocation maps forced unavailable.

### F-03: Post-persistence allocation failure leaves stale matrix memory usable

**Verdict: confirmed. Severity: high.**

`apply_commit_bit` charges the logical resident budget, publishes the page-index
entry, writes the page digest, and writes the bitmap byte at
`crates/varve-core/src/matrix.rs:3576-3598`. Only afterwards does it call
`SparseBitmap::set_byte` at `matrix.rs:3599-3602`.

For the first nonzero byte on a page, `set_byte` still performs fallible page
allocation and map reservation at `matrix.rs:1359-1383`. If that returns
`AllocationFailed`, disk contains the new bit while the in-memory bitmap still
contains the old byte. The payload precharge is refunded, but state agreement is
not restored.

Ordinary matrix mutation handling poisons the writer only for `Error::Io` at
`crates/varve-core/src/file.rs:4621-4625`. A subsequent mutation through the
same writer derives its new byte and digest from stale memory and can overwrite
the persisted bit. This can silently remove a commit while writing a checksum
consistent with the destructive replacement.

Current fault tests cover I/O boundaries before materialization, not a typed
allocation failure after bitmap persistence.

**Required correction:** either prepare the detached page allocation before
disk mutation and install it only after persistence, or poison/reload the writer
on every error after on-disk mutation begins. Add an injected post-persistence
allocation failure followed by a same-byte mutation and reopen verification.

## Medium and Low Findings

### F-04: Durable hook failure has no typed published outcome

**Verdict: narrowed. Severity: medium API ambiguity.**

The durable matrix helper syncs data, commits, syncs the commit, and then calls
the user hook at `crates/varve-core/src/file.rs:3899-3907`. A hook error is
returned unchanged even though the cell is durable. Tests intentionally prove
this behavior at `crates/varve/tests/matrix.rs:745-794`, and
`docs/durability-model.md` documents the hook as post-publication.

The behavior is therefore not an accidental durability defect. The remaining
problem is that ordinary `Result<()>` does not distinguish pre-publication
failure from “published, hook failed.” A retry based only on the result can
duplicate external work.

**Recommended correction:** return a typed post-commit outcome/error carrying
the committed event, or separate durable write from notification into two
explicit calls.

### F-05: CRC rebuild charges one sparse page after allocating it

**Verdict: confirmed and narrowed. Severity: low resource-bound precision.**

CRC rebuild calls `rebuilt.set` before charging `delta.materialised` at
`crates/varve-core/src/matrix.rs:3028-3030`. A limit rejection can therefore
occur after one sparse bitmap page has already been allocated.

The overshoot is transient and bounded to one bitmap page, at most 4 KiB plus
map overhead. The temporary rebuild map is dropped and disk publication has not
started. This does not produce persistent inconsistency, but it means the limit
is not a strict pre-allocation ceiling.

### F-06: CI rustdoc surface differs from docs.rs publication

**Verdict: confirmed. Severity: medium release-documentation mismatch.**

The CI rustdoc job says `--all-features` matches docs.rs and runs the workspace
with all features. The three publishable manifests have `default = []` and no
`[package.metadata.docs.rs] all-features = true`. Docs.rs therefore uses its
default feature selection, not CI's all-feature surface.

Both all-feature and no-default-feature rustdoc builds passed locally with
`-D warnings`, so no current warning failure exists. The mismatch instead means
optional public APIs can be absent from the published docs and the exact docs.rs
surface is not the surface CI claims to gate.

**Recommended correction:** add explicit docs.rs metadata to all publishable
crates and keep the all-feature gate, or gate both the default docs.rs surface
and the intentionally broader all-feature surface with accurate wording.

### F-07: Writer-lock marker can modify an aliased empty file

**Verdict: confirmed and narrowed. Severity: low local hardening issue.**

The diagnostic marker `<target>.lock` is opened with ordinary `OpenOptions` at
`crates/varve-core/src/file.rs:9603-9609`. A zero-length object is treated as an
unused marker at `file.rs:9623`; acquisition then truncates and writes through
the handle at `file.rs:9646` and `file.rs:9790-9802`. Drop clears that same
object again.

A local actor able to pre-place a hard link, or a followed symbolic link, from
the marker path to an unrelated empty file can therefore cause transient writes
and truncation of that object. A crash can leave marker text in it.

This does **not** bypass authoritative single-writer exclusion. The target file
itself receives a native object lock at `file.rs:9635-9638`. The issue is foreign
marker-object modification, not concurrent database writers.

**Recommended correction:** reject final-component reparse/symlink targets and
multi-link marker objects where supported, and verify the marker is a dedicated
regular file before mutation.

### F-08: Custom-codec guide omits the allocation-charging obligation

**Verdict: narrowed. Severity: low documentation/API ergonomics.**

`VarveDecode` cannot mechanically force a custom implementation to call
`Decoder::charge_materialization`. A custom decoder can allocate without
charging the materialization budget. Public architecture and assurance docs
correctly place custom codecs in trusted format-author code, so this is not a
broken sandbox guarantee against an untrusted codec implementation.

The custom-codec guide should nevertheless state that every owned allocation
derived from input must be charged before reservation/allocation, with a
minimal compliant example and a negative self-test.

### F-09: Wildcard-path exception is broader than one documented edge

**Verdict: narrowed. Severity: low policy precision.**

`deny.toml` describes one path-only proc-macro dev dependency but enables the
category-wide `allow-wildcard-paths = true`. The option permits more path/git
development/private dependency shapes than that single edge. It does not allow
registry wildcards or public regular/build path dependencies, so the Round 1
claim that every wildcard path is accepted was too broad.

The root `cargo deny check` passes and core publishable dependency controls
remain active. The comment should describe the actual category scope and CI
should contain a controlled negative policy fixture if this distinction is an
important assurance claim.

### F-10: Direct focused tests leave an empty session directory

**Verdict: observed. Severity: informational test hygiene.**

After a successful focused matrix test, one new
`varve-test-session-*` directory remained in the Windows temp directory. It was
empty and contained zero file bytes. The full test runner has its own cleanup
contract; the residue was from direct test execution, not a failed runner.

This has negligible storage impact but falls short of a literal “successful
test leaves no session path” policy. Existing failed-session directories were
preserved intentionally as failure evidence.

## Rejected or Reclassified Candidates

### Stream staging is not quadratic in reachable production batches

The suffix scan in `stage_state_records` exists, but production batch append is
generic over one block type and supplies one `T::ID` for the whole batch. For
same-block records, each non-final suffix scan stops at its first element, so a
batch of `R` records performs `R - 1` comparisons. Scalar append supplies one
record. The reachable bound is linear, not `Theta(R^2)`.

### Migration source decoding is an explicit trust boundary

`blocks_migrated` accepts a caller-supplied historical `From` type after ID and
version checks and does not apply current-type registration. Current
registration cannot represent historical versions, and the public spec defines
migration as user-supplied semantic conversion under this trust model. This is
not a bypass of ordinary typed-read registration.

### Matrix recovery progress/cancellation is a feature request

CRC rebuild is synchronous and scans every cell of the selected block. For `C`
cells of stride `S`, it hashes `Theta(C*S)` bytes and performs `Theta(C)` seeks,
CRC reads, and bitmap operations. Existing progress/cancellation promises name
scalable stream/index rebuild, not matrix recovery. A cancellable variant is
desirable for large matrices but no current public contract is violated.

### Matrix schema scans are fixed-schema overhead

Per-cell operations linearly scan registered blocks, matrix descriptors,
dimensions, and commit categories. For a fixed format schema this remains
`Theta(N)` over `N` cells and is constant with respect to total matrix cell
count, matching the documented claim. Cached maps would be an optimization.

### Already-clear commit writes perform bounded redundant work

Writing an uncommitted cell prepares `value=false` even when the bit is already
clear. With integrity enabled this can hash up to one 4 KiB page and write an
8-byte digest plus one bitmap byte, with an additional validity-byte write where
applicable. It does not materialize a page or update the page index. This is a
fixed per-operation optimization opportunity, not an asymptotic regression.

### Dynamic fuzzing is not currently a claimed CI gate

CI compiles all seven fuzz targets and checks their independent lockfile,
advisories, licenses, bans, and sources. Actual sanitizer campaigns are local
and historical. Public documentation accurately distinguishes compile/audit CI
from dated campaign evidence, so absence of a per-commit dynamic campaign is an
assurance limitation rather than a broken documented gate.

## Independently Verified Corrections

- Whole-category matrix clear refunds commit, quarantine, CRC-valid payload,
  and page-index resident counters with checked arithmetic.
- Commit and validity mutations refund payload precharges on failures before
  materialization. F-03 is a different post-persistence allocation boundary.
- Generated delete clones borrowed keys before the authoritative tombstone.
- The sidecar test interposition hook is path-keyed, so indexed and stream tests
  no longer overwrite each other.
- Root cargo-deny executes successfully with the documented path-dependency
  exception.
- Matrix fallback and candidate-page complexity wording now match the
  implementation.
- Matrix page-index occupancy is checked, and invalid counted entries do not
  terminate enumeration of later entries.
- `checkpoint_on_flush` retains its linear-cadence implementation and tests; no
  cumulative `O(N^2)` regression was found.
- Built-in codecs inspected in Round 1 validate extents and charge
  materialization before reservation.
- Generic keyed mutation performs fallible tail reservation before append.
- Workspace, changelog, and publishable manifests align on `0.4.0`.

## Mechanical Validation

| Gate | Result |
| --- | --- |
| `cargo fmt --all -- --check` | Pass |
| all-feature/all-target locked `cargo check` | Pass |
| all-feature/all-target Clippy with `-D warnings` | Pass |
| all-feature rustdoc for all three crates with `-D warnings` | Pass |
| no-default-feature rustdoc for all three crates with `-D warnings` | Pass |
| full `varve-test-runner` workspace suite | Pass |
| generated keyed atomicity focused test | Pass, 4 tests |
| matrix integrity/scaling focused binary | Pass |
| zstd-only compression binary | Pass, 11 tests |
| integrity-only compression binary | Pass, 6 tests |
| public API fixture compile | Pass |
| public API fixture runtime roundtrip | Pass, codecs + blocks + file |
| all-feature ignored performance smoke | Pass |
| root `cargo audit` | Pass, 79 dependencies scanned |
| root `cargo deny check` | Pass |
| fuzz workspace locked all-target check | Pass |
| fuzz lockfile audit | Pass, 50 dependencies scanned |
| fuzz dependency policy | Pass |
| actual package archives for all three `0.4.0` crates | Pass |
| root/fuzz/public-fixture offline metadata | Pass |
| `git diff --check` before final report | Pass |

No compile warning or dependency advisory was observed. Passing `cargo audit`
means no lockfile dependency matched the local RustSec database at test time; it
is not a general security certification.

## Release Benchmark

The release example used 10,000 records, one warmup, and five recorded runs.
The active Windows power scheme was **Power saver**. One run had a large
append/merge tail, so medians and complete ranges are retained rather than
selectively deleting that observation.

| Operation | Median ms | Five-run range ms | Versus `4ed3280` |
| --- | ---: | ---: | ---: |
| fixed encode/decode | 1.465 | 1.445-1.471 | -0.7% |
| variable encode/decode | 10.617 | 10.533-10.754 | +1.2% |
| fixed append | 354.444 | 310.891-493.444 | +2.7% |
| fixed open/scan | 40.983 | 39.600-42.130 | +1.2% |
| merge keyed files | 463.929 | 451.721-689.904 | -3.5% |
| compact merged | 486.059 | 426.780-502.041 | -3.1% |
| compact base+deltas | 513.062 | 479.626-566.732 | +0.7% |

Every median remains well inside the documented 15 percent manual gate. The
current sample shows no broad regression attributable to the Opus fixes.

## Security and Stability Assessment

No memory-unsafety path, unchecked offset/length acceptance, or acceptance of
unverified compressed payload was established. Checked arithmetic, CRC-before-
decompression, chunk extent validation, and built-in materialization charging
were supported by source and passing tests.

The meaningful residual risks are:

- persistent mutation atomicity when trusted user key traits panic;
- state visibility after interrupted matrix rebuild;
- disk/memory divergence after a rare post-persistence allocation failure;
- a low-scope local lock-marker alias side effect;
- no fresh dynamic fuzz/sanitizer evidence for this exact commit.

Mmap/zero-copy safety and never-indexed corruption visibility without a usable
filesystem allocation map remain explicitly documented boundaries; this review
did not find evidence that the implementation exceeds those claims.

## Release Decision

Do not publish `8732e83` as `0.4.0` in its current state.

Fix F-01 through F-03 and add deterministic regressions before rerunning the
full suite. F-04 and F-06 are inexpensive API/documentation corrections worth
closing before the public release. F-05, F-07 through F-10, and the bounded
performance optimizations can follow in the same stabilization branch without
changing the wire format.

After the three blockers are corrected, run the established process-interrupt
matrix tests and a fresh bounded fuzz/sanitizer campaign on the final candidate
rather than relying on historical campaign results.
