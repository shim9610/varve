# Varve Performance and Stability Review

## Round 1 Candidate Report

- Date: 2026-07-20 (Asia/Seoul)
- Target commit: `4ed32808c1f99c46da4babcd371453e373b2de06`
- Branch: `codex/dev-next`
- State: candidate findings awaiting independent Round 2 fact checking
- Library source changes made by reviewers: none

## Method

Round 1 used clean-context reviewers for matrix storage, codec/API behavior,
storage publication, and release evidence. Reviewer contexts that could not
read the repository because of the Windows sandbox, or that did not return a
conclusion, were discarded. Their partial output is not represented here.

The candidates below come only from reviewers that successfully read the
target source and returned line-anchored evidence, plus deterministic failures
observed by the main reviewer. They are not final findings. A different
clean-context group must confirm, narrow, or reject every item.

## Candidate Findings

### R1-MAT-01: Whole-category clear may leak resident-budget accounting

The clear refund appears to include commit/quarantine payload pages but omit
CRC-valid payload pages and persisted page-index mirrors. The structures are
cleared, while `resident_page_index_bytes` is not reduced.

Candidate evidence:

- `crates/varve-core/src/matrix.rs:2507`
- `crates/varve-core/src/matrix.rs:2578`
- `crates/varve-core/src/matrix.rs:2587`

Possible consequence: repeated populate/clear cycles can accumulate phantom
residency and eventually reject work even though the corresponding memory was
released.

### R1-MAT-02: A failed first-page mutation may retain its precharge

`apply_commit_bit` appears to charge the page before fallible page-index and
disk operations. The page is materialized only after those operations, with no
obvious rollback on the earlier `?` exits.

Candidate evidence:

- `crates/varve-core/src/matrix.rs:3483`
- `crates/varve-core/src/matrix.rs:3487`
- `crates/varve-core/src/matrix.rs:3498`
- write-failure injection at `crates/varve-core/src/matrix.rs:3175`

Possible consequence: a failed mutation consumes phantom budget and a retry
can be refused or charged again.

### R1-MAT-03: Allocation-map fallback documentation is contradictory

The implementation falls back to persisted index pages only when allocation
extents are unavailable. A never-indexed page changed outside Varve is then not
visited. Some source documentation says stray-byte detection is unchanged,
while the migration material acknowledges the limitation.

Candidate evidence:

- `crates/varve-core/src/matrix.rs:119`
- `crates/varve-core/src/matrix.rs:130`
- `crates/varve-core/src/matrix.rs:4626`

This is at least a documentation defect. Round 2 must decide whether the public
integrity contract also overpromises behavior.

### R1-MAT-04: The live-state matrix-open summary is not a worst-case bound

Allocation-derived candidates include every page overlapped by reported
extents, and `pages_to_visit` materializes and deduplicates those candidates.
A densely allocated bitmap region can therefore produce work proportional to
the region even when few bits are live.

Candidate evidence:

- `crates/varve-core/src/matrix.rs:216`
- `crates/varve-core/src/matrix.rs:4629`
- `docs/performance.md:156`

Candidate bound for `L` persisted entries, `A` allocation-derived candidates,
and `U` distinct pages is expected `O(L + A + 4096U)` work with
`Theta(L + A)` temporary candidates, not a bound in live pages alone.

### R1-TEST-01: Sidecar publication tests share one overwriteable global hook

The test hook is one process-global `Mutex<Option<(PathBuf, Hook)>>`.
Consumption checks the path, but registration replaces the entire option.
The indexed and stream tests arm different paths without a shared test guard,
so one can overwrite the other's hook.

Candidate evidence:

- `crates/varve-core/src/stream.rs:1441-1472`
- indexed registration at `crates/varve-core/src/indexed.rs:2043`
- stream registration at `crates/varve-core/src/stream.rs:2200`
- indexed expectation at `crates/varve-core/src/indexed.rs:2055-2057`

Observed mechanical evidence:

- the full workspace runner failed in the indexed test;
- the same test passed 20 of 20 focused all-feature runs;
- the parallel all-feature `varve-core --lib` run failed in the same test;
- the all-feature library passed `114 passed` with `--test-threads=1`.

This currently points to test-only parallel interference, not a demonstrated
product-path defect. It still blocks a reliable CI release gate.

### R1-REL-01: Root cargo-deny fails on a path-only dev dependency

`varve-macros` declares a path-only dev dependency on `varve`. The root policy
denies wildcard requirements, and the documented release gate fails.

Candidate evidence:

- `crates/varve-macros/Cargo.toml:38`
- `deny.toml:27`
- `.github/workflows/ci.yml:301`
- `README.md:209`

Observed result: root `cargo deny check` reported `bans FAILED`. Actual
`cargo package --locked -p varve-core -p varve-macros -p varve` still passed,
which is consistent with this being a deny-policy failure rather than an
archive-build failure.

### R1-DOC-01: CI feature-count comment is stale

The workflow says seven optional features and 128 combinations, while the
facade and README expose six optional switches. Six switches have 64 subsets.

Candidate evidence:

- `.github/workflows/ci.yml:57`
- `crates/varve/Cargo.toml:19`
- `README.md:196`

The configured nine representative jobs are a fixed coverage matrix, not the
full power set.

## Areas Round 2 Must Verify Even Without a Round 1 Defect

1. Initial and incremental resident keyed-tail accounting occurs before peak
   allocation and before authoritative append for generic and generated APIs.
2. HashMap and BTreeMap materialization charges dominate actual allocations
   across small capacity classes and the supported toolchain range.
3. `KeyedMergeEstimate` no longer claims an upper bound it cannot provide.
4. Matrix field aliases are accepted while variable-width or unsupported
   custom codecs still fail at compile time.
5. Matrix clear and failed-mutation accounting candidates are checked against
   the changed regression tests, not source inspection alone.

## Verified Improvements From Round 1

- VMAT v4 uses a checked occupancy header rather than terminator scanning.
- Invalid counted page-index entries create a fatal finding while later
  entries remain enumerable.
- Final-bit clearing evicts its payload page and persisted-index removal uses
  swap-with-last ordering.
- Matrix clear records whether the platform fallback streamed zero bytes.
- Product sidecar rebuild rechecks primary identity before and after atomic
  sidecar publication and uses identity-checked retirement.
- Windows same-object cleanup uses the verified handle; POSIX cleanup uses an
  opened directory, repeated identity checks, and `unlinkat`.
- Workspace and publishable crates are aligned on `0.4.0`.
- Verified `.crate` archives for all three `0.4.0` packages were produced.
- The staged-consumer CI job resolves the public fixture source against the
  extracted archives.

## Mechanical Evidence So Far

| Gate | Result |
| --- | --- |
| formatting | Pass |
| all-feature/all-target locked check | Pass |
| all-feature/all-target Clippy with `-D warnings` | Pass |
| full workspace runner | Fail, R1-TEST-01 |
| focused failing test, 20 repetitions | Pass 20/20 |
| serial all-feature varve-core library | Pass, 114 tests |
| zstd-only compression binary | Pass, 11 tests |
| integrity-only compression binary | Pass, 6 tests |
| public API fixture | Pass |
| three rustdoc builds with `-D warnings` | Pass |
| all-feature performance smoke | Pass |
| root dependency audit | Pass, 79 dependencies |
| root dependency policy | Fail, R1-REL-01 |
| fuzz locked all-target check | Pass |
| fuzz dependency audit | Pass, 50 dependencies |
| fuzz dependency policy | Pass |
| verified package archives | Pass |
| root/fuzz/rename/public fixture offline metadata | Pass |

Release benchmark medians remain within the documented 15 percent manual gate
relative to the previous same-day review. Absolute timings were measured under
the Windows Power saver scheme and must not be compared with another scheme.
