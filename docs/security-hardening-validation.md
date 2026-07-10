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

