# Varve Performance and Stability Review

## Round 1 Candidate Report

- Date: 2026-07-20 (Asia/Seoul)
- Target commit: `060851ad9b38438f40dd7830febf2a6f77512196`
- Branch: `codex/dev-next`
- State: independent candidate findings awaiting Round 2
- Library source changes made by reviewers: none

## Method

Four reviewers started with empty conversation context and one disjoint scope:

1. matrix, resident performance, and published complexity bounds;
2. codecs, memory accounting, generated APIs, and manual type contracts;
3. durability, publication, identity, and cleanup;
4. CI, packaging, documentation, and benchmark contracts.

The findings below are deliberately frozen before a second group sees them.
They are candidates, not final conclusions. Round 2 must confirm, reject, or
narrow each item from code and small deterministic checks.

## Candidate Findings

### R1-API-01: Generic keyed append may return an error after writing

The generic keyed push/delete paths appear to append and update authoritative
file state before a fallible resident tail-map reservation. A reservation error
could therefore be returned after the record is already present, with the
cached predecessor state left stale.

Candidate evidence:

- `crates/varve-core/src/file.rs:2347`
- `crates/varve-core/src/file.rs:2396`
- `crates/varve-core/src/file.rs:2463`
- `crates/varve-core/src/file.rs:4469`
- `crates/varve-macros/src/lib.rs:5683`

### R1-SAFE-01: Small HashMap capacity accounting may undercharge reservation

The new conservative reservation formula appears to model the large-table
7/8 load rule but may not include hashbrown's minimum small-table capacity.
For a declared length of one it computes two buckets, while the implementation
may allocate at least four plus control/alignment storage. Round 2 must verify
this against the resolved hashbrown version and determine whether the public
materialization budget claims an upper bound.

Candidate evidence:

- `crates/varve-core/src/codec.rs:522`
- `crates/varve-core/src/codec.rs:1015`
- `crates/varve-core/src/codec.rs:1171`
- `crates/varve/tests/codec_hardening.rs`

### R1-API-02: Matrix field eligibility may still reject valid type aliases

The new encoded-width expression is type based, but an earlier syntactic
pre-filter accepts only a list of source identifier spellings. A type alias of
a supported scalar may be rejected before the generated const checks run.

Candidate evidence:

- `crates/varve-macros/src/lib.rs:2172`
- `crates/varve-macros/src/lib.rs:2203`

### R1-API-03: PackedBitmap exposes matrix-specific errors in ordinary fields

`PackedBitmap` is now supported in ordinary variable fields, but malformed
length and bounds conditions return `InvalidMatrixLayout`. This may be an API
semantics issue rather than a functional defect.

Candidate evidence:

- `crates/varve-core/src/matrix.rs:729`

### R1-PERF-01: Matrix page history may be unbounded and uncharged

The v3 `indexed_pages` set appears to retain every page ever published, even
after the final live bit is cleared and the 4 KiB payload page is released.
The set is rebuilt entry by entry at open and is not charged to the bitmap
budget. Long-lived sparse churn may therefore consume memory proportional to
historically touched pages rather than current live pages.

Candidate evidence:

- `crates/varve-core/src/matrix.rs:923`
- `crates/varve-core/src/matrix.rs:1114`
- `crates/varve-core/src/matrix.rs:3931`
- `crates/varve-core/src/matrix.rs:4780`
- `crates/varve/tests/matrix_integrity_scaling.rs:239`

### R1-PERF-02: Matrix open may sort all pages to visit

Open copies page ids into `pages_to_visit` and sorts them. If `P` pages are
known from the index and allocation map, enumeration is `O(P log P)` time and
`O(P)` temporary memory, while performance documentation states a linear bound.

Candidate evidence:

- `crates/varve-core/src/matrix.rs:3996`
- `crates/varve/tests/matrix_integrity_scaling.rs:329`
- `docs/performance.md:286`

### R1-PERF-03: Whole-category clear depends on hole-punch support

`zero_range` appears to fall back to writing an entire logical region when the
filesystem cannot punch holes. Clearing validity, page-index, digest, and commit
regions can then be proportional to cell count despite an unqualified bounded-
by-written-state performance claim.

Candidate evidence:

- `crates/varve-core/src/matrix.rs:411`
- `crates/varve-core/src/matrix.rs:2147`
- `crates/varve/tests/matrix_integrity_scaling.rs:587`
- `docs/performance.md:153`

### R1-PERF-04: Keyed merge peak estimate may still omit live allocations

The new public `peak_resident_bytes()` is described as an upper bound, but its
state component appears to use `count * size_of::<tuple>()`, excluding key/value
heap allocations and HashMap bucket/control overhead. Output vector allocation
may overlap the still-owned map allocation while values are moved out.

Candidate evidence:

- `crates/varve-core/src/file.rs:5921`
- `crates/varve-core/src/file.rs:5966`
- `crates/varve-core/src/file.rs:6135`
- `crates/varve-core/src/file.rs:6157`
- `crates/varve-core/src/file.rs:8007`
- `docs/api-reference.md:799`

### R1-STO-01: Unix self-test cleanup has a path replacement window

The Unix cleanup path appears to open and verify object identity, then delete by
pathname in a separate operation. A concurrently writable directory can change
the pathname between those operations. Windows performs identity check and
delete through the same handle.

Candidate evidence:

- `crates/varve-core/src/diagnostics.rs:1048`
- `crates/varve-core/src/diagnostics.rs:1073`
- `crates/varve-core/src/diagnostics.rs:1088`

### R1-STO-02: Matrix page-index damage may hide later pages

The page-index loader appears to treat zero or an out-of-range entry as a clean
terminator. Where allocation-map data is unavailable, later valid pages may not
be visited and their page digests may not contribute a recovery finding.

Candidate evidence:

- `crates/varve-core/src/matrix.rs:3967`
- `crates/varve-core/src/matrix.rs:3996`
- `crates/varve/tests/matrix_integrity_scaling.rs:438`

### R1-STO-03: Scalable sidecar rebuild publication may race primary replacement

Stream bootstrap checks object identity before building and publishing a
sidecar. Indexed rebuild performs a stronger logical check but releases the
checked handle before publication. A concurrent primary replacement in the
remaining interval may let an old-generation sidecar replace a newer valid
sidecar. Readers should reject it, making this an availability/rebuild issue
rather than wrong-data acceptance.

Candidate evidence:

- `crates/varve-core/src/stream.rs:209`
- `crates/varve-core/src/stream.rs:216`
- `crates/varve-core/src/indexed.rs:1293`
- `crates/varve-core/src/indexed.rs:1305`

### R1-STO-04: Native rewrite temps are manually cleaned rather than RAII-owned

`create_rewrite_temp_file` returns a plain path and file. At least one error
between creation and installation of later cleanup logic may leave a temp.
Normal error paths are otherwise manually cleaned, and uncertain publication
states intentionally retain their temp.

Candidate evidence:

- `crates/varve-core/src/file.rs:6180`
- `crates/varve-core/src/file.rs:8641`
- `docs/test-artifact-hygiene.md:82`

### R1-DOC-01: Allocation-map fallback documentation may describe v2 behavior

The performance guide says an unavailable allocation map reads every page,
while v3 appears to visit indexed pages only. The design guide instead records
that an unavailable map cannot discover unindexed page damage. One of these
contracts is stale.

Candidate evidence:

- `crates/varve-core/src/matrix.rs:4001`
- `docs/performance.md:286`
- `docs/matrix-storage-design.md:246`

### R1-CI-01: Benchmark assertions may not cover every reported output

The benchmark computes a live-value denominator and asserts one direct compact
output count, while documentation says every merge/compact line is asserted.

Candidate evidence:

- `crates/varve/examples/perf_bench.rs:162`
- `crates/varve/examples/perf_bench.rs:250`
- `docs/performance.md:31`

### R1-CI-02: Immutable-CI wording may exceed actual pinning

Action revisions and cargo-audit are pinned, but moving runner images and
stable toolchains remain in most jobs. Claims that future runs evaluate exactly
the same gate code may need narrowing.

Candidate evidence:

- `.github/workflows/ci.yml:32`
- `.github/workflows/ci.yml:37`
- `README.md:221`

## Evidence Gaps

- CI has no rustdoc warnings-denied job, and proc-macro examples are ignored.
- Package CI lists archive contents with `--no-verify` but does not build the
  generated package archives.
- The downstream API fixture uses checkout path dependencies, not packaged
  artifacts.
- Singleton CI runs the focused compression test binary, while release wording
  says the complete test suite.
- The unreleased changes still use package version `0.3.0`; a version bump is a
  publication task rather than a defect in this development commit.

## Round 1 Verified Improvements

- Manual block contracts now validate endian and include descriptor slice
  lengths in cache identity.
- `ChunkedBytes` and `PackedBitmap` carry nonzero codec identities.
- `PackedBitmap` is excluded from matrix slots and accepted in ordinary
  variable fields.
- The previous large zero-sized HashMap reservation case is rejected by the new
  accounting path.
- Matrix v3 open is page-index driven rather than a loop over logical cell
  count.
- Clearing the final bit releases the 4 KiB resident payload page.
- Resident block-tail reconstruction removes the previous insertion quadratic
  term for the intended construction path.
- The sequence-sort `8N` temporary is included in the merge estimate.
- Initial create/sync pathname durability and internal temp-marker cleanup are
  implemented.
- MSRV 1.95, isolated feature jobs, pinned actions/audit tool, package README
  and licenses, crate-level rustdoc, and a public API fixture are present.

The final report must not retain any candidate above without independent Round
2 verification.
