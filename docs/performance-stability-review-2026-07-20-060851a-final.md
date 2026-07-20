# Varve Performance and Stability Review

## Final Fact-Checked Report

- Review date: 2026-07-20 (Asia/Seoul)
- Target commit: `060851ad9b38438f40dd7830febf2a6f77512196`
- Branch: `codex/dev-next`
- Rust toolchain: `rustc 1.95.0 (59807616e 2026-04-14)`
- Review type: read-only implementation review plus deterministic validation
- Library source changes made by this review: none

## Executive Verdict

The Opus revision fixes the dominant defects from the previous review. In
particular, `checkpoint_on_flush` has a linear-growth regression test, matrix
v3 no longer scans the entire logical bitmap width on ordinary open, sparse
bitmap payload pages can be evicted, block identities are stronger, and the
full workspace suite is green.

The commit is not yet ready for a public `0.4.0` release. Six runtime issues
remain release blockers:

1. generic keyed append/delete can return `Err` after the record was appended
   and published in writer state;
2. the small-`HashMap` allocation model can charge less than `std::HashMap`
   reserves;
3. the matrix page-index set is not charged to the resident-memory limit and
   retains historical page ids;
4. `KeyedMergeEstimate::peak_resident_bytes()` is not a true upper bound;
5. Unix self-test cleanup has an identity-check-to-path-unlink window;
6. damaged matrix page-index entries can hide later committed pages when no
   filesystem allocation map is available.

None of the confirmed issues establishes unchecked payload acceptance or
memory unsafety. They concern mutation-result atomicity, bounded-resource
contracts, corruption visibility, pathname cleanup, or published API claims.

## Review Method

Round 1 used four reviewers that each started with empty conversation context.
They covered disjoint areas: matrix/performance, codecs/APIs, storage/durability,
and CI/packaging/documentation. Candidate findings were frozen in
`docs/performance-stability-review-2026-07-20-060851a-r1-draft.md` before Round
2 began.

Round 2 used a different set of reviewers, also with empty conversation
context, to confirm, narrow, or reject each candidate from source. A reviewer
that did not return a conclusion was discarded; no partial output from that
context was used. Its scope was reassigned to four new clean, single-claim
reviewers. The main reviewer then reconciled the reports with deterministic
tests, a local standard-library capacity probe, dependency checks, and release
benchmark runs.

No new randomized campaign or process fault campaign was started during this
review. The fuzz workspace was compiled and its independent lockfile was
audited, but this report does not claim a fresh fuzz-execution result.

## P0 Runtime Findings

### F-01: Generic keyed mutation can fail after authoritative append

**Verdict: confirmed.**

Generic keyed push appends at `crates/varve-core/src/file.rs:2349` and only then
updates the resident keyed-tail cache at `file.rs:2351`. Delete has the same
ordering at `file.rs:2465` and `file.rs:2467`. Cache insertion can still fail
during allocation calculation or `try_reserve` at `file.rs:2402-2409`.

The append is already authoritative by then: bytes are written at
`file.rs:4431-4441`, the record is indexed at `file.rs:4469`, and sequence and
snapshot state are published at `file.rs:4477-4479`.

Under allocation failure the caller can therefore receive `Err` although the
record or tombstone exists. The old cached predecessor remains, so a later
generic keyed mutation on the same writer can link around the successful
record. Generated scalable/indexed APIs are not proven to share this exact
fallible post-write path.

**Required correction:** reserve all fallible cache capacity before append, or
make a post-append cache failure invalidate/rebuild the cache while returning a
typed published outcome. Add a deterministic failure-injection test for both
push and tombstone paths.

### F-02: Small HashMap reservations are undercharged

**Verdict: confirmed on the review toolchain.**

`hash_table_reservation_bytes` at
`crates/varve-core/src/codec.rs:1171-1184` models one entry as two buckets. For
`T = ((), ())` it charges 19 bytes. The local Rust 1.95 standard library was
probed directly: `HashMap::<(), ()>::with_capacity(1)` and
`try_reserve(1)` both report capacity 14, implying the 16-bucket small-table
layout. The matching hashbrown layout requires 32 bytes of control storage for
this zero-sized entry case. The comment at `codec.rs:1166` that small-table
specializations allocate less is therefore false for this toolchain.

Existing coverage at
`crates/varve/tests/codec_hardening.rs:477` exercises large zero-sized maps but
not capacities 1 through 14.

**Required correction:** model the small-table capacity classes explicitly and
add exact low-budget tests for zero-sized and one-byte entries at capacities
1 through 14. The calculation must remain conservative across the declared
MSRV and supported stable toolchains.

### F-03: Matrix page-index history is outside the resident budget

**Verdict: narrowed but release-blocking for PB-scale claims.**

Published page ids are retained in `indexed_pages` at
`crates/varve-core/src/matrix.rs:923`, even after the corresponding 4 KiB
payload page is evicted. The set is excluded from the bitmap-byte accounting at
`matrix.rs:964` and `matrix.rs:1114`; `try_reserve_set` at `matrix.rs:4780`
handles allocator failure but does not apply a `ReadLimits` charge.

Let `H` be distinct pages published since the last whole-map reset, `L` the
currently live pages, and `P` the total possible pages. Memory is
`Theta(4096 * L + HashSet(H))`, with `H <= P`. This is not mathematically
unbounded for a fixed matrix, but it is unbounded relative to live state and
the advertised resident bitmap budget. Reopen also materializes and visits all
`H` entries at `matrix.rs:3939`.

The eviction test at
`crates/varve/tests/matrix_integrity_scaling.rs:499` observes payload-byte
accounting only and does not exercise historical page churn.

**Required correction:** include page-index storage in a runtime resource
limit. If resident cost must track live state, remove or compact historical ids
when pages become empty. Add churn-and-reopen tests that assert both index and
payload residency.

### F-04: Merge peak-resident estimate is not an upper bound

**Verdict: confirmed.**

`max_state_bytes` at `crates/varve-core/src/file.rs:5966` counts only
`size_of::<(Key, (MergeOrder, Option<T>))>()` per key-bearing record. It omits
HashMap bucket slack/control bytes and heap storage owned by keys and values.
The output `Vec` is also reserved at `file.rs:6157` while the map and its values
remain alive.

Despite those omissions, `peak_resident_bytes()` is described as an upper bound
at `file.rs:5927` and `docs/api-reference.md:799`. The underestimate can be
arbitrarily large for heap-owning `Key` or `T`.

**Required correction:** either rename and document it as a structural count
estimate, or require type-provided heap bounds and include a conservative map
allocation plus the overlapping output vector. Tests must compare the estimate
with controlled allocated peaks, not only arithmetic terms.

### F-05: Unix self-test cleanup has a pathname replacement window

**Verdict: confirmed, Unix and hostile/co-writable-directory scope only.**

Unix cleanup opens the target at
`crates/varve-core/src/diagnostics.rs:1081`, checks handle identity at
`diagnostics.rs:1084`, and then unlinks by pathname at `diagnostics.rs:1088`.
The source acknowledges the interval at `diagnostics.rs:1076`. A non-cooperating
actor able to replace names in the parent directory can substitute a different
file between the check and unlink. Varve's Unix writer locks are advisory.

Windows performs the check and deletion through the same non-delete-shared
handle at `diagnostics.rs:1048-1059` and does not share this exact issue.

**Required correction:** on Unix, allow destructive self-test cleanup only in
an exclusively owned directory, or use a same-object directory-relative
deletion mechanism where supported; otherwise refuse cleanup. Add an internal
interposition test immediately before deletion.

### F-06: Matrix page-index damage can hide later committed pages

**Verdict: confirmed.**

The page-index loader treats a zero or out-of-range entry as a successful
terminator at `crates/varve-core/src/matrix.rs:3967`. Open visits indexed pages
plus allocation-map pages at `matrix.rs:4001`; without a usable filesystem
allocation map, later valid pages following a damaged earlier entry are never
visited. Findings are produced only for pages that are visited at
`matrix.rs:4152`.

The result is a visibility/integrity failure: affected cells can be reported as
`NotCommitted` at `matrix.rs:2052`, and the omitted digest pages produce no
finding. This does not make Varve return unchecked payload bytes. Existing
corruption coverage changes a page still named by an intact index, not the
earlier index entry itself.

**Required correction:** authenticate or redundantly encode page-index
occupancy, or protect it with a validated high-water mark. Add a two-page test
with the first index entry unavailable and allocation-map discovery disabled.

## P1 Findings

### F-07: Matrix open is O(Q log Q), not linear

**Verdict: confirmed.**

`pages_to_visit` copies indexed page ids, appends allocation-map ids, then calls
`sort_unstable` and `dedup` at `crates/varve-core/src/matrix.rs:4001-4014`.
For `Q` candidate entries this is `O(Q log Q)` time and `Theta(Q)` temporary
memory. Page I/O remains independent of the logical matrix width, so the v3
dense-scan defect is fixed, but the `O(bytes actually written)` claim at
`docs/performance.md:286` is too strong.

### F-08: Whole-category clear has a linear fallback

**Verdict: confirmed and platform/filesystem dependent.**

Windows and Linux attempt sparse-range removal at
`crates/varve-core/src/matrix.rs:385-406`. Unsupported targets or a failed
operation fall back to streaming zeroes through `write_zeros` at
`matrix.rs:413` and `matrix.rs:4880`. That path is `Theta(cells)` in bitmap
bytes, not cell-count independent. The design guide qualifies this behavior,
but `docs/performance.md:155` and `docs/performance.md:289` do not.

### F-09: Sidecar publication can lose a race with non-cooperating replacement

**Verdict: narrowed to availability.**

Cooperating Varve writers are serialized by `WriterLock`. A pathname mutator
that ignores that protocol can replace the primary after the final identity
check and before sidecar publication (`stream.rs:209`, `indexed.rs:1293-1305`).
An old-generation sidecar can then replace a newer valid sidecar. Consumers
reject the stale sidecar through identity/generation checks, so this was not
shown to return wrong native data; it causes rejection/rebuild availability
cost.

### F-10: Matrix primitive aliases are rejected before type-based validation

**Verdict: confirmed API defect.**

The macro rejects a field when the spelling-based helper returns false at
`crates/varve-macros/src/lib.rs:2175`. The helper whitelists literal primitive
path names at `lib.rs:2206-2222`, so `type Word = u32;` is rejected before the
generated `<T as VarveEncode>::WIRE_TYPE` check at `lib.rs:2248` can decide the
actual width. This contradicts `docs/api-reference.md:451-455`.

### F-11: CI does not verify publishable archives

**Verdict: confirmed publication gap.**

The package job runs `cargo package --locked --no-verify --list` at
`.github/workflows/ci.yml:314`. This proves included file names but neither
creates nor builds the `.crate` archives. The public API fixture also consumes
the checkout by path (`tools/public-api-fixture/Cargo.toml:24`), not packaged
artifacts.

Before publication, add an archive-producing `cargo package --locked` flow and
a staged consumer that installs all three local packages. The workspace version
is still `0.3.0` while the changelog contains breaking unreleased changes; the
next pre-1.0 release must be `0.4.0`.

### F-12: Rustdoc warnings are not a CI gate

**Verdict: confirmed CI gap; current tree passes locally.**

CI has no `cargo doc`/`RUSTDOCFLAGS` job, and two proc-macro examples remain
ignored. This review successfully ran `cargo rustdoc ... -- -D warnings` for
all three crates, so no current warning was found. Add the equivalent workspace
CI gate and convert ignored macro examples to compilable `no_run` examples.

## P2 Findings

- **PackedBitmap error semantics:** confirmed. Ordinary malformed bitmap data
  can return matrix-specific `InvalidMatrixLayout` at
  `crates/varve-core/src/matrix.rs:729-733` and `matrix.rs:4891-4895`. This is
  error classification, not unchecked decoding.
- **Rewrite-temp cleanup:** narrowed. A permission-copy error after temp
  creation but before the manual cleanup scope at `file.rs:6180-6186` can leave
  an empty temp. The target is unchanged. An RAII guard would close the gap.
- **Benchmark assertion wording:** confirmed. Only direct base+delta output is
  opened and counted at `crates/varve/examples/perf_bench.rs:250`, despite
  `docs/performance.md:31` saying every merge/compact line is asserted.
- **Immutable CI wording:** confirmed. Actions and `cargo-audit` are pinned,
  but `ubuntu-latest`, `windows-latest`, and most `stable` toolchains are
  rolling. README and workflow comments overstate reproducibility.
- **Feature-matrix wording:** narrowed. CI covers default, no-default,
  singletons, and all-features, not every combinatorial feature subset.
- **Stale matrix-v2 changelog entry:** confirmed. The migration guide correctly
  describes v3, but `CHANGELOG.md:559` retains an intermediate v2 statement.
- **Allocation-map fallback docs:** confirmed stale. Matrix v3 uses indexed
  pages when allocation extents are unavailable; some performance/design text
  still describes the old full logical scan.

## Verified Improvements

The review independently confirmed these important corrections:

- `checkpoint_on_flush` growth is guarded by linear-cost tests; the previous
  cumulative `O(N^2)` behavior is not present in the tested path.
- Matrix v3 open is driven by persisted page ids and optional allocation
  extents rather than logical matrix width.
- Sparse 4 KiB bitmap payload pages are released and their payload budget is
  refunded when the final bit clears.
- Previous VMAT layouts are rejected with a typed migration requirement.
- `ChunkedBytes` has a stable nonzero schema identity; `PackedBitmap` is not
  accepted as a matrix cell type.
- Manual block contracts validate endian, keyedness, schema identity, and
  descriptor length identity.
- Create locks before truncation, replacement publication synchronizes the
  parent directory, cancellation occurs before sidecar publication, and stale
  sidecars are rejected by identity checks.
- Generated high-cardinality indexed open/append paths have tests proving that
  they do not construct the native scanner on clean state.

## Mechanical Validation

| Gate | Result |
| --- | --- |
| `cargo fmt --all -- --check` | Pass |
| workspace all-feature/all-target locked `cargo check` | Pass |
| workspace all-feature/all-target locked Clippy with `-D warnings` | Pass |
| full `varve-test-runner` workspace suite and doctests | Pass |
| successful runner cleanup | Pass, zero session directories |
| zstd-only compression test binary | Pass, 11 tests |
| integrity-only compression test binary | Pass, 6 tests |
| public API fixture | Pass, codecs, derived blocks, and file roundtrip |
| three crate rustdoc builds with `-D warnings` | Pass |
| all-feature ignored performance smoke | Pass |
| root `cargo audit` | Pass, 79 dependencies scanned |
| root `cargo deny check` | Pass |
| fuzz workspace locked all-target check | Pass |
| fuzz lockfile audit | Pass, 50 dependencies scanned |
| fuzz license/source/ban policy | Pass |
| package file lists for all three crates | Pass, README and both licenses present |
| `git diff --check` from prior reviewed commit | Pass |

The full runner reported `Varve test artifact cleanup: verified empty`. Every
additional runner invocation reported the same. Build caches under `target`
were retained; the one temporary standard-library capacity probe executable
was removed after use.

## Release Benchmark

The release example used 10,000 records, one warmup, and five recorded runs.
The active Windows power scheme was **Power saver**, so absolute values should
not be compared with results recorded under an unknown or different scheme.

| Operation | Median ms | Five-run range ms | Versus `0d4b9c6` recorded median |
| --- | ---: | ---: | ---: |
| fixed encode/decode | 1.491 | 1.456-1.496 | +1.2% |
| variable encode/decode | 10.549 | 10.496-10.910 | -2.4% |
| fixed append | 308.468 | 300.363-326.702 | -12.6% |
| fixed open/scan | 40.074 | 39.535-41.110 | -0.8% |
| merge keyed files | 480.133 | 458.064-489.484 | -9.8% |
| compact merged | 455.996 | 453.442-487.345 | -17.1% |
| compact base+deltas | 485.342 | 465.003-629.195 | -19.2% |

The comparison is indicative rather than causal because the previous report did
not record its power scheme. It shows no broad slowdown: only fixed codec time
increased, by 1.2 percent, while all other medians decreased. The single direct
compact high run is retained in the range.

## Release Decision

Do not publish this commit as-is. Fix F-01 through F-06 first, then rerun the
full deterministic matrix and focused regressions. Before crates.io publication,
also complete F-11, bump the three crates to `0.4.0`, build the actual package
archives, and test a consumer against those archives.

After those corrections, F-07 through F-12 and the P2 wording/hygiene items can
be closed in the same stabilization branch. The present commit is substantially
healthier than `0d4b9c6`, but its remaining bounded-memory and mutation-result
contracts are too important to waive for a PB-scale storage library.
