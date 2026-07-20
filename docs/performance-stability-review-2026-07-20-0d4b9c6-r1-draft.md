# Varve Performance and Stability Review

## Round 1 Candidate Report

- Date: 2026-07-20 (Asia/Seoul)
- Target commit: `0d4b9c667503bb0df3960eaaae101218842671fa`
- Branch: `codex/dev-next`
- Review state: candidate findings awaiting an independent second round
- Source changes made by the reviewers: none

## Method

Four reviewers started with empty conversation context and received only the
target commit, a bounded read-only scope, and one independent subject area:

1. performance and scale behavior;
2. type, schema, codec, and decoder contracts;
3. durability, identity, publication, and artifact cleanup;
4. CI, release, dependency, benchmark, and documentation contracts.

This document intentionally preserves their candidate claims before the second
round. A claim appearing here is not yet a final finding. Round 2 must reproduce,
reject, or narrow every retained item from source and bounded tests.

## Candidate Findings

### R1-PERF-01: Matrix open may still visit every logical bitmap page

`load_paged_bitmap` appears to iterate from page zero through the complete
logical page count. Allocation extents avoid reads for holes, but they do not
appear to avoid the loop itself. The candidate complexity is therefore
`Theta(P log E)` CPU per bitmap, where `P` is the logical bitmap page count and
`E` is the bounded extent count. Existing scaling tests primarily count bytes
read and need a page-visit assertion.

Candidate evidence:

- `crates/varve-core/src/matrix.rs:3265`
- `crates/varve-core/src/matrix.rs:3276`
- `crates/varve-core/src/matrix.rs:3285`
- `crates/varve/tests/matrix_integrity_scaling.rs:288`

### R1-PERF-02: Cleared sparse bitmap pages may remain resident

`SparseBitmap::set_byte` updates an existing page but does not visibly remove
that page when it becomes all zero. The resident budget may therefore track
historically touched pages until reopen or category-wide clear rather than only
current set state.

Candidate evidence:

- `crates/varve-core/src/matrix.rs:799`
- `crates/varve-core/src/matrix.rs:4215`

### R1-PERF-03: Resident block-tail construction may retain a quadratic schema case

`BlockTails::from_index` walks the record index and inserts first-seen block ids
into a sorted vector. If first appearances arrive in reverse order, repeated
middle insertion may move `Theta(B^2)` tuples for `B` block ids. This concerns
resident open/rebuild and not the predeclared scalable stream tail table.

Candidate evidence:

- `crates/varve-core/src/file.rs:687`
- `crates/varve-core/src/file.rs:710`
- `crates/varve/tests/resident_contracts.rs:347`

### R1-PERF-04: Typed hot paths may remain linear in schema cardinality

Registration and selected indexed/matrix operations appear to use linear
descriptor searches. Record-count scaling may be fixed while operation cost is
still proportional to the number of declared blocks or descriptors.

Candidate evidence:

- `crates/varve-core/src/format.rs:1644`
- `crates/varve-core/src/collections.rs:227`
- `crates/varve-core/src/indexed.rs:1133`
- `crates/varve-core/src/matrix.rs:2223`

### R1-PERF-05: Resident merge estimates may omit input-open sort work

Resident merge/compact is now explicitly excluded from PB-scale guarantees,
which resolves the prior contract issue. A narrower candidate remains: opening
each resident input appears to copy and sort sequence values, adding an
`O(N log N)` step and an `8N`-byte temporary not clearly represented by the
published estimate.

Candidate evidence:

- `crates/varve-core/src/file.rs:7855`

### R1-API-01: Manual block contracts may omit endian identity

Generated format identity and schema hashing include a block endian override,
but the process-wide `BlockContract` candidate appears to store only schema
fingerprint and keyedness. A manually implemented type with the expected
fingerprint but a different `T::ENDIAN` may therefore pass registration and use
the manual endian during typed decode.

Candidate evidence:

- `crates/varve-macros/src/lib.rs:2304`
- `crates/varve-core/src/format.rs:1820`
- `crates/varve-core/src/collections.rs:249`
- `crates/varve-core/src/collections.rs:267`
- `crates/varve-core/src/collections.rs:294`

### R1-API-02: Block-contract cache keys may omit static slice lengths

The global cache key appears to use only the data pointers of the block and
identity slices. Different-length static views with the same first element can
share those pointers. If such `FormatSpec` values are constructible, one view
may reuse a contract resolved for the other.

Candidate evidence:

- `crates/varve-core/src/collections.rs:303`
- `crates/varve-core/src/collections.rs:320`
- `crates/varve-core/src/format.rs:973`
- `crates/varve-core/src/format.rs:990`

### R1-API-03: `ChunkedBytes` may violate the new nonzero codec identity rule

The codec traits default `SCHEMA_ID` to zero, while derive-time assertions now
require nonzero encode and decode identities. `ChunkedBytes` appears not to
override the constants even though documentation presents it as a variable
field helper.

Candidate evidence:

- `crates/varve-core/src/codec.rs:140`
- `crates/varve-core/src/codec.rs:152`
- `crates/varve-core/src/chunks.rs:155`
- `crates/varve-core/src/chunks.rs:163`
- `crates/varve-macros/src/lib.rs:698`
- `docs/spec.md:52`

### R1-API-04: `PackedBitmap` may have both identity and fixed-width classification conflicts

`PackedBitmap` also appears to inherit zero codec identities. Separately, the
macro classifies it as a fixed-width matrix field while its codec contains a
variable-length vector. Round 2 must determine whether the type is actually
publicly usable as a derived field or is only an internal helper outside that
contract.

Candidate evidence:

- `crates/varve-core/src/matrix.rs:586`
- `crates/varve-core/src/matrix.rs:590`
- `crates/varve-macros/src/lib.rs:2092`
- `crates/varve-macros/src/lib.rs:3652`

### R1-STO-01: Keyed compaction may leave a temporary-path lock marker

The atomic keyed rewrite creates a temporary native file through
`VarveFile::create`, which also creates a sibling marker for that temporary
pathname. Publication removes or renames the data file, while `WriterLock::drop`
appears to clear but not unlink the temporary marker.

Candidate evidence:

- `crates/varve-core/src/file.rs:6031`
- `crates/varve-core/src/file.rs:6038`
- `crates/varve-core/src/file.rs:6051`
- `crates/varve-core/src/file.rs:6055`
- `crates/varve-core/src/file.rs:9242`

### R1-DUR-01: Initial native creation durability needs contract clarification

Native `create` followed by `sync` appears to sync the file but not its parent
directory. Atomic replacement paths do perform and report parent-directory
sync. Round 2 must compare this with the documented meaning of initial
`create().sync()` before deciding whether this is a defect or an explicit
platform boundary.

Candidate evidence:

- `crates/varve-core/src/file.rs:1906`
- `crates/varve-core/src/file.rs:3166`
- `crates/varve-core/src/file.rs:8680`

### R1-CI-01: Singleton feature behavior is compiled but not tested in CI

CI appears to run behavior tests for default and all-feature configurations,
while singleton feature configurations receive Clippy only. Feature-exclusive
tests pass locally but may not execute in their intended isolated combinations
on CI.

Candidate evidence:

- `.github/workflows/ci.yml:34`
- `.github/workflows/ci.yml:72`
- `crates/varve/tests/compression.rs:687`
- `crates/varve/tests/compression.rs:696`

### R1-CI-02: The declared MSRV may not be exercised in CI

Workspace packages declare Rust `1.95`, while workflow setup appears to install
the moving stable toolchain. The declared compatibility floor may therefore
lack an automated gate.

Candidate evidence:

- `Cargo.toml:14`
- `.github/workflows/ci.yml:18`
- `.github/workflows/ci.yml:88`

### R1-CI-03: Some CI tool identities are mutable

Workflow actions use major-version tags and `cargo-audit` installation does not
pin an exact tool release. Gate behavior can change without a repository
change. This is a reproducibility concern, not a finding about current source
dependencies.

Candidate evidence:

- `.github/workflows/ci.yml:17`
- `.github/workflows/ci.yml:124`
- `.github/workflows/ci.yml:127`

### R1-DOC-01: Published API documentation may be incomplete

The facade crate and proc-macro entry points have little or no crate-level/API
rustdoc despite directing users to docs.rs. The standalone feature table also
appears to omit internal or advanced features. Round 2 must distinguish public
features from intentionally undocumented development features and inspect the
actual package file lists.

Candidate evidence:

- `crates/varve/Cargo.toml:9`
- `crates/varve/src/lib.rs:1`
- `crates/varve-macros/src/lib.rs:11`
- `crates/varve/Cargo.toml:19`
- `docs/api-reference.md:813`

### R1-BENCH-01: Merge/compact throughput labels may use the wrong denominator

The benchmark mutates and extends the base data, but reports merge and compact
throughput using the original input record count. Elapsed times remain usable;
the printed records-per-second values may not represent processed operations.

Candidate evidence:

- `crates/varve/examples/perf_bench.rs:159`
- `crates/varve/examples/perf_bench.rs:187`
- `crates/varve/examples/perf_bench.rs:196`
- `crates/varve/examples/perf_bench.rs:233`

### R1-REL-01: The documented local completion command may update lockfiles

The no-argument test runner defaults to an inner Cargo command without
`--locked`. CI supplies locked arguments explicitly, but the local completion
instructions may not be reproducible from a clean committed tree.

Candidate evidence:

- `docs/test-artifact-hygiene.md:9`
- `tools/varve-test-runner/src/main.rs:27`

## Missing Evidence Candidates

- The documented 15 percent integration threshold is manual; ignored smoke
  tests report measurements but do not enforce the threshold.
- CI builds fuzz targets but does not repeat long campaigns or sanitizer runs
  for every commit. Historical records should not be described as a per-commit
  gate.
- The clean-archive job does not appear to run `cargo package` verification.
- External branch-protection configuration cannot be established from the
  tracked repository.

## Round 1 Rejections and Confirmed Improvements

- Checkpoint-on-flush cumulative rescanning did not recur.
- Clean scalable stream/indexed open uses the sidecar and bounded generation
  witness rather than scanning the native log.
- Scalable append does not reread each appended native chunk.
- Disk-index key state remains bounded by configured batch/cache policy.
- Stream/index and matrix sidecars now carry generation identity appropriate to
  their native protocols.
- First generic schema registration, disk descriptor fingerprinting, custom
  codec identity propagation, and field-id accounting are materially present.
- Checked payload extents precede the inspected conversions and slice access;
  no untrusted-input panic or memory-safety violation was confirmed in Round 1.

## Preliminary Mechanical Results

These results were produced by the main reviewer and are not substitutes for
Round 2 claim verification:

| Gate | Result |
| --- | --- |
| `cargo fmt --all -- --check` | Pass |
| all-feature/all-target locked `cargo check` | Pass |
| all-feature/all-target Clippy with `-D warnings` | Pass |
| root `cargo deny check` | Pass |
| root `cargo audit` | Pass, 79 dependencies and 1,166 advisories loaded |
| fuzz workspace locked all-target check | Pass |
| all-feature workspace suite through cleanup runner | Pass, 211.2 s |
| runner artifact cleanup | Pass, empty |

The final report must not retain any candidate above without an independent
Round 2 source check and, where practical, a bounded compile/runtime witness.
