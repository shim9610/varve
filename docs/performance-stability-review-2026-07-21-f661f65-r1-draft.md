# Varve Performance and Stability Review: Round 1 Draft

Date: 2026-07-21  
Target: `f661f65fa49a68f2ae0a6cefdc3040d1b39eecc9`  
Branch: `codex/dev-next`

## Status

This is the frozen output of the first, independent review round. Every item
below is an **unverified candidate**, not a final finding. A second set of
clean-context reviewers must confirm, narrow, or reject each candidate from
source and focused tests. The main reviewer will then reconcile those results
with the mechanical test evidence.

No library source was changed during this review.

## Round 1 Method

Five clean-context reviewers independently inspected generated APIs, matrix
storage, stream/indexed durability, PB-scale performance, and release/docs
gates. They were given the exact commit and a bounded ownership area. They did
not receive one another's conclusions. Candidate IDs below freeze their output
before Round 2 begins.

## Product Candidates

| ID | Initial severity | Candidate | Primary evidence |
|---|---:|---|---|
| R1-01 | High | `replace_rewrite<T>` may accept a target record with a different block version, encode `T`, then retain the old header version. | `crates/varve-core/src/file.rs:3392`, `:3402`, `:3433` |
| R1-02 | High | `replace_fixed_in_place_exclusive` may change a keyed value without invalidating the resident byte-key tail cache, allowing later keyed chains to use stale predecessors. | `crates/varve-core/src/file.rs:3279`, `:3362` |
| R1-03 | Medium | `blocks_migrated` may decode an unregistered `From` schema sharing only ID/version, bypassing the normal schema fingerprint contract. | `crates/varve-core/src/file.rs:3669`, `:3685` |
| R1-04 | High | Page-index compaction may mutate disk entries before a fallible `index_slots` allocation, leave a stale memory mirror, and continue without poisoning the writer. | `crates/varve-core/src/matrix.rs:3740`, `:3751`, `:3860`; `file.rs:4848` |
| R1-05 | High | CRC commit-map rebuild may ignore a damaged validity-index `intact` result and publish an empty/incorrect commit map from incomplete evidence in forensic mode. | `crates/varve-core/src/matrix.rs:5485`, `:5498`, `:3342`, `:3389` |
| R1-06 | Medium | CRC rebuild may clone sparse bitmap containers and allocate page-key vectors outside typed resource-budget/fallible-allocation paths. | `crates/varve-core/src/matrix.rs:3367`, `:4103` |
| R1-07 | Medium | Post-publication matrix outcome construction may allocate through `Box::new`, so allocation failure could prevent delivery of the typed committed outcome. | `crates/varve-core/src/file.rs:4073`, `:4086` |
| R1-08 | Low | The final page-index generation header may be written after the last `sync_data`, so a successful rebuild may return before that final marker is crash-durable. | `crates/varve-core/src/matrix.rs:4168`, `:4172` |
| R1-09 | High | Generated writer priming may scan the resident index once per keyed block and enforce keyed-tail bytes per block rather than in aggregate: `Theta(MN)` open and up to `Theta(M*L)` retained bytes. | `crates/varve-core/src/file.rs:2553`, `:2652`, `:3865`; `crates/varve-macros/src/lib.rs:4974`, `:5083` |
| R1-10 | High | Matrix CRC rebuild performs a full cell scan with point-oriented CRC and stored-CRC reads per cell, potentially making PB recovery I/O-operation-bound. | `crates/varve-core/src/matrix.rs:3317`, `:3342`, `:4352`, `:6075` |
| R1-11 | Medium | Matrix CRC rebuild exposes neither progress nor cooperative cancellation across its full `cell_count` loop. | `crates/varve-core/src/matrix.rs:3317`; `crates/varve-core/src/file.rs:4196` |
| R1-12 | High | Indexed single-record mutations may bypass an internally poisoned stream after a failed append rollback because the indexed poison flag is separate and the prepared append path does not recheck stream poison. | `crates/varve-core/src/stream.rs:1203`, `:1210`; `crates/varve-core/src/indexed.rs:566`, `:1191` |
| R1-13 | Medium | If atomic publication's parent-directory sync and subsequent rebind both fail, the returned rebind error may lose the distinct `ParentSyncPending` durability state. | `crates/varve-core/src/file.rs:3121`, `:3259`, `:3488`, `:4725` |
| R1-14 | Low | Self-test cleanup may silently return when a populated lock marker cannot be reacquired, despite documentation saying cleanup refusal is reported. | `crates/varve-core/src/diagnostics.rs:1330`, `:1341`; `file.rs:10385` |

## Test and Documentation Candidates

| ID | Initial severity | Candidate | Primary evidence |
|---|---:|---|---|
| R1-15 | Medium test gap | The Windows reparse-point rejection branch can be skipped when symlink creation lacks privilege, leaving that branch unexercised on such CI hosts. | `crates/varve/tests/storage_hardening.rs:823`, `:829`; `docs/invariant-checklist.md:452` |
| R1-16 | Medium docs | The test runner removes closed artifacts after a successful child command; wording that any successful-child artifact creation itself fails the gate may overstate the implementation. | `.github/workflows/ci.yml:117`; `README.md:199`; `docs/test-artifact-hygiene.md:43`; `tools/varve-test-runner/src/main.rs:54`, `:234` |
| R1-17 | Medium docs | Fuzz documentation may claim pre-existing `fuzz/artifacts` files fail the campaign, while the PowerShell script appears to check only campaign exit status. | `docs/fuzzing-and-fault-injection.md:54`; `scripts/run-security-fuzz.ps1:44`, `:53` |
| R1-18 | Low docs | `deny.toml` records validation with cargo-deny 0.19.9 while the pinned CI action appears to install 0.20.2. | `deny.toml:43`; `.github/workflows/ci.yml:316` |

## Round 1 Non-Findings

- No new `checkpoint_on_flush` cumulative `O(N^2)` path was found; the current
  cadence checks are constant-time and checkpoint serialization is geometric.
- Generated keyed push/delete paths were observed to encode/cache keys before
  publication and update a pre-reserved byte-key map afterward.
- The new page-index rebuild marker is persisted before destructive rewrite and
  malformed/rebuild marker headers are treated as fatal on ordinary open.
- No new unbounded buffering or repeated full scan was found in normal
  stream/indexed append paths in this commit.
- mmap/zero-copy remains an explicit unsafe contract requiring external mutation
  exclusion for the mapping lifetime.

## Mechanical Evidence Available at Freeze

- `cargo fmt --all -- --check`: pass.
- Workspace all-feature/all-target locked check: pass.
- Workspace all-feature/all-target clippy with `-D warnings`: pass.
- Rustdoc for all three public crates, all-features and no-default-features: pass.
- `cargo run --locked -p varve-test-runner`: pass in 197.5 s; all executed unit,
  integration, compile-contract, scalable fault, and doctests passed; the runner
  reported `Varve test artifact cleanup: verified empty`.

These broad passes do not resolve the candidates above; Round 2 must inspect the
specific failure paths and contracts.
