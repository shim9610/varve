# Security Hardening Validation

## Baseline

- Commit: `f7a9369`
- Tree: clean
- Host: `x86_64-pc-windows-msvc`
- `rustc 1.95.0 (59807616e 2026-04-14)`
- `cargo 1.95.0 (f2d3ce0bd 2026-03-21)`
- `clippy 0.1.95 (59807616e1 2026-04-14)`
- `rustdoc 1.95.0 (59807616e 2026-04-14)`
- `cargo-audit 0.22.2`
- `cargo-deny 0.19.9`
- `cargo-semver-checks 0.48.0`

Baseline results:

- all-feature workspace tests passed;
- all-target/all-feature Clippy passed with `-D warnings`;
- semver comparison to `f7a9369` passed 196 checks for both `varve` and
  `varve-core`;
- `cargo deny` found `RUSTSEC-2026-0186` in `memmap2 0.9.10`;
- `cargo audit` also warned about `RUSTSEC-2026-0190` in locked
  `anyhow 1.0.102`;
- compatible lock dry-run removes `anyhow` and updates `memmap2` to `0.9.11`.

## Frozen Performance Method

The existing `crates/varve/tests/perf_smoke.rs` harness and datasets are frozen
for this pass. Workers may not modify them.

1. Use the same Rust 1.95 toolchain, `--all-features`, release profile, local
   SSD, and machine power plan.
2. Run one warm-up, then five candidate runs. When a separate baseline checkout
   is available, alternate baseline and candidate runs.
3. Compare medians for operations whose baseline median is at least 100 ms.
4. A candidate median more than 10% slower, with at least four of five paired
   regressions, blocks integration pending profile-backed explanation.
5. Any file-size change, new per-record sync, changed asymptotic behavior, or
   obvious allocation growth blocks integration regardless of timing noise.

The initial debug-profile baseline run completed in 6.25 seconds. Representative
large-case timings were 113.807 ms append fixed, 32.200 ms open/scan fixed,
492.721 ms open VARVE3 chain, 743.712 ms materialized keyed, 674.199 ms layout
reopen/append, 405.712 ms matrix CRC write/commit, and 226.753 ms keyed merge.
These debug numbers are orientation only; the final gate uses the release
method above.

## Independent Validation Findings

The clean spec validator returned `SPEC_PASS_WITH_REQUIRED_EDITS`. Required
edits were incorporated by freezing exact dependencies, sequence transitions,
rollback precedence, mmap obligations, extent order, error variants, and the
adapter suffix grammar.

The clean routing validator returned `PASS_WITH_REQUIRED_EDITS`. Required edits
were incorporated by pinning dev provenance, introducing a serial public API
precursor, assigning exact files and new test files, separating documentation
from verification, freezing the performance harness, and forbidding merges
with the unrelated public history.

One recommendation was deliberately narrowed: generic maps reject duplicate
decoded keys using their Rust map identity, rather than separately encoding all
keys to detect a non-injective custom codec. The latter would add allocation
and work to every valid map and cannot repair an invalid user codec generally.

The clean implementation verifier initially withheld PASS for an unbounded
stored compressed-payload allocation behind the logical limit API. The API now
requires separate physical and logical ceilings before either allocation.
Follow-up clean passes found and verified complete footer extent validation and
category-preserving matrix quarantine errors. The final focused clean-context
recheck returned PASS for cell, single, and per-channel quarantine reads/writes
while preserving whole-category clear and typed rebuild recovery paths.

## Final Verification Matrix

| Domain | Required check |
| --- | --- |
| Supply chain | audit, deny advisories/bans/licenses/sources, exact inverse trees |
| API | semver diff; only mmap unsafe and Error non-exhaustive allowlisted |
| Wire | existing native/layout/compression/matrix byte assertions and schema hashes |
| Hostile input | overflow, truncation, extreme counts, duplicate keys, invalid bool/suffix |
| Failure containment | append and streamed callback error rollback, reuse, poison contract |
| Unsafe | compile failure without unsafe block and complete safety docs |
| Durability | no implicit sync; existing durable commit/matrix ordering tests |
| Performance | frozen release median method |
| Documentation | API, format-author, architecture, durability and migration guidance |

## Final Candidate Results

Supply-chain verification on the updated `Cargo.lock`:

- `cargo update --dry-run` reports zero compatible updates for Rust 1.95;
- `cargo audit --deny warnings` reports no vulnerabilities across 75 locked
  dependencies;
- `cargo deny check` passes advisories, bans, licenses, and sources;
- direct versions match the frozen table, including `memmap2 0.9.11`,
  `tempfile 3.27.0`, and stable `zerocopy 0.8.54`;
- `anyhow 1.0.102` is absent from the final graph;
- the remaining duplicate `getrandom` lines are split between test-only
  `proptest` and runtime/build users of `tempfile`/`zstd`, not unresolved
  advisories.

Mechanical verification:

- default-feature workspace tests pass;
- all-target/all-feature workspace tests pass;
- all-target/all-feature Clippy passes with `-D warnings`;
- all-feature rustdoc passes with `RUSTDOCFLAGS=-D warnings`;
- trybuild verifies all four file-backed mmap constructors fail outside an
  unsafe block and pass with an explicit unsafe contract;
- hostile codec, payload/footer extent, matrix descriptor, quarantine, append
  rollback, layout callback, tempfile lifetime, and suffix tests pass;
- matrix fault injection verifies partial slot overwrite and failed commit-map
  publication remain uncommitted and poison later mutation, flush, and sync;
- existing native, layout, compression, manifest, checkpoint, matrix, CRC,
  zero-copy, TDMS-model, property, and durability-order assertions pass.

`cargo-semver-checks` against `f7a9369` reports only the two allowlisted source
break families in `varve-core`: `Error` becoming non-exhaustive and the four
file-backed mmap constructors becoming unsafe. These changes require the next
pre-1.0 breaking API release boundary if `f7a9369` has already been published.
No wire layout, version, or schema-hash rule changes. The only canonical output
correction is that appending after reopening an empty file now uses sequence `0`
instead of the prior erroneous `1`.

Final isolated release performance medians:

| Large case | Baseline | Candidate | Change |
| --- | ---: | ---: | ---: |
| append fixed | 112.527 ms | 119.081 ms | +5.8% |
| open VARVE3 chain | 588.356 ms | 557.463 ms | -5.3% |
| materialized keyed | 1036.685 ms | 802.685 ms | -22.6% |
| layout reopen+append | 775.256 ms | 682.990 ms | -11.9% |
| layout open/scan | 141.715 ms | 106.676 ms | -24.7% |
| matrix CRC write+commit | 464.673 ms | 390.151 ms | -16.0% |
| merge keyed files | 323.733 ms | 275.997 ms | -14.7% |

The first candidate sample set was discarded because clean-context verification
was concurrently using the same workspace and produced unrelated disk/CPU
regressions. The table uses a later isolated warm-up plus five runs. No median
crosses the 10% regression gate, no file size changed, and no implicit sync was
added. The performance harness logic and datasets remained unchanged; only
required unsafe-call syntax changed with the mmap API.

## Explicit Residual Boundary

Append-log readers remain snapshot-on-open. VMAT v1 snapshots layout metadata
and commit maps but stores slot bytes in place. Applications must not overlap a
matrix reader with writes to slots it may read. True immutable concurrent matrix
snapshots require versioned slots/generations or read leases and are future
storage-architecture work, not a guarantee of this candidate.
