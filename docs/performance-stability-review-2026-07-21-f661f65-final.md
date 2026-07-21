# Varve Performance and Stability Review

## Final Fact-Checked Report

- Review date: 2026-07-21 (Asia/Seoul)
- Target commit: `f661f65fa49a68f2ae0a6cefdc3040d1b39eecc9`
- Branch: `codex/dev-next`
- Review type: two-round clean-context review plus mechanical validation
- Library source changes made by this review: none
- Frozen Round 1 draft:
  `docs/performance-stability-review-2026-07-21-f661f65-r1-draft.md`

## Executive Verdict

The Opus revision closes the previously reported generated-key post-publication
trait calls, whole-map rebuild marker gap, first-page mutation allocation order,
docs.rs feature mismatch, lock-marker alias handling, custom-codec guidance,
dependency-policy explanation, and successful-test cleanup issue. The broad
test, documentation, package, and dependency gates are green, and the release
benchmark shows no broad performance regression.

The exact commit is nevertheless **not ready for public 0.4.0 publication**.
Five correctness defects are release blockers:

1. `replace_rewrite<T>` can write one block version's payload under another
   version's record header when schema locking is disabled.
2. Exclusive in-place replacement can change a key without invalidating the
   resident keyed-tail cache, after which generated keyed chains use stale
   predecessors.
3. Matrix page-index compaction writes a new disk order before a fallible mirror
   reservation; allocation failure leaves a usable writer with a stale mirror.
4. Forensic CRC recovery can rebuild from an incomplete CRC-validity page index
   and publish false negatives as the new commit map.
5. Indexed single-record writes can continue through an internally poisoned
   stream after append rollback itself failed.

Two medium post-publication result issues and one low diagnostic issue should be
fixed in the same stabilization batch. The remaining confirmed fuzz-script gap
is release-assurance work, not a runtime file-format defect.

No Rust memory-safety defect, unchecked offset-to-slice path, or ordinary-open
acceptance of fatal matrix state was confirmed in this review. Default matrix
open remains fail-closed. The confirmed risks are type/version integrity,
persistent/in-memory agreement, recovery evidence, and accurate failure-state
delivery. This statement is scoped to the reviewed code and tests; it is not a
claim of exhaustive absence.

## Review Method

Round 1 used five reviewers with empty conversation context. They independently
covered generated/resident APIs, matrix storage, stream/indexed durability,
large-scale performance, and release/docs gates. Their 18 candidates were
written to the frozen draft before Round 2 began.

Round 2 used a different set of reviewers, again with empty conversation
context, to classify each frozen ID as confirmed, narrowed, or rejected from
source, public contracts, and focused temporary harnesses. One matrix reviewer
was terminated by the platform before returning a result. Its incomplete work
was discarded in full and replaced by a new clean-context reviewer. No partial
claim from the terminated context appears here.

The main reviewer then checked the high-severity control flow directly and
reconciled both rounds with formatting, compilation, Clippy, rustdoc, the full
workspace runner, dependency checks, package archives, the downstream public
API fixture, performance smoke, and a five-run release benchmark.

## Release Blockers

### F-01: Rewrite replacement preserves the wrong block version

**Confirmed. High correctness. Scope: schema locking disabled.**

`replace_rewrite<T>` validates that `T` is registered, but selects its target by
block ID alone at `crates/varve-core/src/file.rs:3390-3399`. It encodes `T` at
`file.rs:3402-3403`, then clones the old index entry and changes sequence,
flags, and payload only at `file.rs:3429-3436`. The old `block_version` is
therefore emitted by the rewrite helper.

This differs from `replace_fixed`, which explicitly rejects a target version
mismatch at `file.rs:3187-3193`. The mismatch is reachable because
`schema_hash = 0` is a supported opt-out from header-level schema locking.

A clean external harness created a v1 file, opened it with a compatible v2 spec
whose schema hash was disabled, and called v2 rewrite replacement. The call
succeeded with sequence 1, the physical header remained version 1, and the v2
typed read failed. This is persistent type/version disagreement, not merely a
diagnostic mismatch.

**Required correction:** perform the same target-version check as
`replace_fixed` before any rewrite work. Add a cross-version test with schema
locking disabled and assert `BlockVersionMismatch`.

### F-02: Exclusive in-place key replacement leaves stale keyed tails

**Confirmed. High correctness. Scope: unsafe API used within its documented
exclusivity contract.**

The resident cache stores canonical key bytes to tail offsets. Maintained keyed
push reads its predecessor from that cache at `file.rs:2535`. After
`replace_fixed_in_place_exclusive` succeeds, only index sequence/checksum and
sequence state are updated at `file.rs:3362-3365`; the cache is not invalidated
or rebuilt.

The safety contract at `file.rs:3272-3278` requires exclusive access but does
not require the key field to remain unchanged. Generated writers prime and use
the same cache.

An external harness changed the key at record offset 26 from 1 to 2 while
satisfying exclusivity. The next key-2 append reported no predecessor, while a
later key-1 append incorrectly reported offset 26. The physical keyed chains no
longer represented the values actually stored.

**Required correction:** reject key-changing in-place replacement for keyed
blocks, or invalidate and safely rebuild the affected block's resident tails
after success. The former is simpler and matches ordinary keyed replacement's
same-key contract. Also add the missing block-version check to this path.

### F-03: Page-index compaction can leave disk and mirror in different orders

**Confirmed. High persistent-state correctness.**

`SparseBitmap::index_slots` is the mirror of the persisted entry array.
`compact_page_index` writes sorted entries first at
`crates/varve-core/src/matrix.rs:3740-3742`, then performs a fallible
`try_reserve_vec` for that mirror at `matrix.rs:3751-3755`. Only after the
reservation does it publish the shorter header and replace the mirror.

If reservation returns `AllocationFailed`, the old occupancy count still names
a safe superset, but the disk order has changed while the in-memory slot map has
not. `finish_matrix_mutation` poisons only `Error::Io` at
`crates/varve-core/src/file.rs:4848-4852`, so this writer remains usable.
`release_page_index_entry` later trusts stale slot positions at
`matrix.rs:3845-3869`; with a duplicate-slot state such as `[2, 1, 2]`, it can
overwrite and shorten the array so the only entry for another live page is no
longer in the counted prefix.

**Required correction:** reserve and prepare every mirror update before the
first disk entry write, then publish disk and install the prepared mirror. As a
secondary guard, poison the matrix writer on every returned error after disk
mutation has begun. Add a deterministic allocation-failure test starting from
a duplicate-entry recovery state and verify all live pages after reopen without
allocation-map assistance.

### F-04: Forensic CRC rebuild can publish from incomplete validity evidence

**Confirmed and narrowed from R1-05. High recovery correctness.**

The Round 1 wording blamed a discarded `intact` return. The exact issue is
different: CRC-validity maps are loaded with `digest_base: None` at
`matrix.rs:5489-5501`. A damaged validity page-index is separately recorded as
a fatal finding by `load_page_index` at `matrix.rs:5168-5225`, but its omitted
pages remain absent from the in-memory validity bitmap.

Default open correctly blocks access. Explicit forensic mode removes that gate
at `matrix.rs:4679-4683`. `rebuild_commit_map_from_crc` then treats
`crc_valid_bits.get(ordinal)` as authoritative at `matrix.rs:3342-3346` and
publishes the rebuilt map at `matrix.rs:3389-3422`. With no allocation-map
assistance, valid pages missing from the damaged index become false and can be
published as uncommitted.

The recovery contract says a rebuilt bit is set only when slot-valid evidence
says the CRC is meaningful. It does not authorize missing evidence to erase the
previous commit view.

**Required correction:** track the completeness/fatal status of each
CRC-validity map separately and refuse CRC commit-map rebuild when that evidence
is incomplete. Offer a distinct explicit clear operation if the operator wants
to discard unverifiable visibility. Add a damaged-validity-index forensic test
with allocation-map assistance disabled.

### F-05: Indexed single-record writes bypass internal stream poison

**Confirmed. High fault-recovery correctness.**

When native append fails and either truncate or seek rollback also fails,
`VarveStreamWriter::append_prepared_chunk` sets its own `poisoned` flag and
returns `WriteRollbackFailed` at `crates/varve-core/src/stream.rs:1203-1217`.
`VarveIndexedWriter` has a separate poison flag and its guard checks only that
outer flag at `crates/varve-core/src/indexed.rs:1191-1196`.

Indexed `push_info` and `delete_info` pass the outer guard, prepare an update,
then call `stream.append_prepared_chunk` directly at `indexed.rs:566-568` and
`:619-621`. That internal method does not call the stream guard. A second
single-record mutation can therefore proceed after rollback failure, despite
the public contract that subsequent mutation is refused with `WriterPoisoned`.
Batch failures poison the outer writer; this finding is limited to indexed
single-record put/delete.

The existing process fault hook aborts at the native-write boundary and cannot
inject the required write-plus-rollback return pair, so this was confirmed from
the complete control flow rather than a dynamic double-I/O-failure test.

**Required correction:** make every prepared append enforce stream poison, or
propagate stream poison into the indexed writer before returning. Add a
deterministic writer abstraction/fault hook for write failure plus truncate or
seek failure, then assert every later mutation is rejected.

## Other Product Findings

### F-06: Typed post-commit matrix outcomes allocate after publication

**Confirmed. Medium robustness; not a normal malformed-input path.**

After the cell is authoritative, both durability-barrier and hook errors build
their typed variants with two `Box::new` calls at `file.rs:4073-4089`. The
public durability contract says these variants are the only post-publication
outcomes.

A clean subprocess harness failed the next allocation at each branch. Both
children terminated on a 56-byte allocation while a reopened file contained the
committed values (71 and 72). The fixed-size allocation is not controlled by a
file length and ordinary Rust programs often treat allocator exhaustion as
process-fatal, so this is not classified as a malformed-file security defect.
It does, however, disprove the absolute structural claim.

**Correction choices:** pre-stage a nonallocating outcome representation before
commit, redesign the API around a nonrecursive post-commit outcome type, or
narrow the documentation to exclude process-wide allocator exhaustion.

### F-07: Combined parent-sync and rebind failure loses one durability fact

**Confirmed. Medium durability diagnostics.**

In all three replacement paths, `ParentSyncPending(sync_error)` calls rebind
with `?` before constructing the parent-sync result at `file.rs:3121-3128`,
`:3259-3266`, and `:3488-3495`. If rebind also fails, only
`PublishedButRebindFailed` is returned. Its shape at `file.rs:4725-4730` cannot
carry the already-known parent-directory sync failure.

Publication and writer poisoning remain explicit, so the target is not treated
as unchanged. The missing fact is whether the published pathname is durable
against power loss. This requires two independent failures and was confirmed
from control flow because the rebind injector is private to crate tests.

**Required correction:** add a combined published outcome, or let the rebind
variant carry both rebind and optional parent-sync errors. Add a double-fault
test for every replacement API.

### F-08: Self-test cleanup can report pass while leaving its lock marker

**Confirmed. Low diagnostic correctness.**

`WriterLock::drop` ignores marker-clear failure at `file.rs:10385-10388`.
`remove_unowned_lock_marker` says cleanup refusal is reported, but any failure
to reacquire the marker returns silently at
`crates/varve-core/src/diagnostics.rs:1330-1343`.

An external harness observed `passed=true`, zero cleanup failures, artifact
removed, but a 20-byte marker still present. This can make a self-test appear
fully clean while its next run is refused.

**Required correction:** convert reacquisition failure into an Environment
cleanup step failure, preserving the marker without claiming success.

### F-09: The fuzz script does not enforce its artifact gate

**Confirmed. Low release-assurance gap, not a runtime defect.**

`docs/fuzzing-and-fault-injection.md:54-55` says any file under
`fuzz/artifacts` fails the gate. `scripts/run-security-fuzz.ps1:33-58` checks
only corpus-generation and campaign exit codes. A temporary harness placed an
existing artifact under that path and ran the real script with a successful
Cargo stub; the script succeeded and left the artifact in place.

**Required correction:** define whether the gate means newly produced or any
artifact, snapshot/clear intentionally before a campaign, and check the
post-campaign directory accordingly.

### F-10: Rebuild clones one bitmap through infallible `Clone`

**Narrowed. Low robustness gap.**

`SparseBitmap` derives `Clone`, and CRC rebuild clones the old map at
`matrix.rs:3367`. That container allocation does not return typed
`AllocationFailed`. The separate page-key vector is fallibly reserved.

This is not a `MatrixBitmapBytes` contract violation: public docs explicitly
define limits as nominal accounting rather than a hard peak-RSS bound and name
temporary/container overhead exclusions. Only graceful allocator-failure
coverage is missing. Treat it consistently with the policy chosen for F-06.

## Performance and Capacity Boundaries

These three Round 1 candidates describe real behavior but do not violate the
current published PB contract, which is scoped to stream/indexed APIs.

### P-01: Resident generated writer priming is per keyed block

Generated writer construction calls `prime_keyed_tails` once per keyed block.
Each call walks the resident index, so CPU work is `Theta(MN)` for `M` keyed
block types and `N` resident entries. Ordinary payload reads are filtered to the
current block, while tombstones can be reread for every keyed type. The retained
limit is intentionally per block ID, so aggregate cache memory can reach roughly
`M * L`.

This is documented, and resident APIs are explicitly not the PB path. It should
still be made harder to misuse: generated docs should route high-cardinality
formats to disk-indexed APIs, and a future single-pass multi-block primer would
remove the avoidable constant-factor scan.

### P-02: Matrix CRC rebuild performs two reads per small cell

Rebuild loops over every cell and separately reads slot bytes and the stored
4-byte CRC. A Windows I/O-counter harness measured exactly 512 reads for 256
four-byte cells and 2,048 reads for 1,024 cells. This matches the documented
`O(cells)` and two-read contract, but it is operation-bound for small cells and
is not suitable as a PB-scale recovery path.

Batch sequential slot reads and CRC-table reads if matrix recovery is ever added
to the PB contract.

### P-03: Matrix CRC rebuild has no progress or cancellation API

The public and generated wrappers expose only `Result<u64>` and have no scan
options, observer, or cancellation token. Existing progress/cancellation
contracts cover verification, stream bootstrap, and disk-index rebuild, not
matrix rebuild. This is a real long-running-operation feature gap, but not a
violation of the current documented scope.

## Test Coverage Gap

The Windows reparse-point rejection test returns success when symlink creation
is unavailable. That conditional gap is documented. On this review host the
exact test did create the link and passed, so the branch was exercised locally;
CI hosts without the privilege can still report green without that coverage.

## Rejected Round 1 Candidates

- **R1-03:** rejected. Migration deliberately trusts the caller-supplied
  historical `From` codec and contracts it by ID/version, not the current
  registry fingerprint. A harness confirmed ordinary reads reject the mismatch
  while explicit migration accepts it.
- **R1-08:** rejected. Rebuild does not promise implicit crash durability. The
  durable marker is written before destructive work, a lost final count reopens
  fail-closed, and explicit `sync()` owns final durability.
- **R1-16:** rejected. Runner/docs consistently promise deletion and verified
  absence after successful tests, not failure merely because a test created a
  temporary file. A successful-child file was removed and the runner passed.
- **R1-18:** rejected. The cargo-deny 0.19.9 text records the version used for a
  scoped historical policy validation; it does not claim that CI's pinned action
  embeds that same version. Baseline and negative policy mutations behaved as
  documented under 0.19.9.

## Round 2 Reconciliation

| Candidate | Verdict | Final placement |
|---|---|---|
| R1-01 | Confirmed | F-01 |
| R1-02 | Confirmed | F-02 |
| R1-03 | Rejected | Documented migration trust boundary |
| R1-04 | Confirmed | F-03 |
| R1-05 | Narrowed | F-04, exact cause corrected |
| R1-06 | Narrowed | F-10 |
| R1-07 | Confirmed | F-06 |
| R1-08 | Rejected | Explicit sync policy |
| R1-09 | Narrowed | P-01, documented resident characteristic |
| R1-10 | Narrowed | P-02, documented matrix characteristic |
| R1-11 | Narrowed | P-03, feature gap |
| R1-12 | Confirmed | F-05 |
| R1-13 | Confirmed | F-07 |
| R1-14 | Confirmed | F-08 |
| R1-15 | Confirmed | Conditional test gap; exercised on this host |
| R1-16 | Rejected | Runner and docs agree |
| R1-17 | Confirmed | F-09 |
| R1-18 | Rejected | Historical version note is accurate |

## Mechanical Validation

All commands ran at the exact target commit.

- `cargo fmt --all -- --check`: pass.
- `cargo check --workspace --all-features --all-targets --locked`: pass.
- `cargo clippy --workspace --all-features --all-targets --locked -- -D warnings`:
  pass with zero warnings.
- Rustdoc with `-D warnings` for `varve-core`, `varve-macros`, and `varve`, both
  all-features and no-default-features: pass.
- `cargo run --locked -p varve-test-runner`: pass in 197.5 seconds. Unit,
  integration, compile-pass/fail, scalable crash-boundary, and doctests passed.
  The one-million-key RSS stress probe remains intentionally ignored.
- Successful test session cleanup: verified by the runner and by path absence
  after completion.
- `cargo check --locked --all-targets` in `fuzz/`: pass.
- `cargo audit`: pass against 1,166 loaded advisories for 79 root dependencies
  and 50 fuzz-workspace dependencies.
- `cargo deny check`: root and fuzz workspaces both pass advisories, bans,
  licenses, and sources.
- `cargo package --locked -p varve-core -p varve-macros -p varve`: all three
  archives packaged and verified successfully.
- `tools/public-api-fixture`: every advertised scalar/container codec, derived
  fixed/variable block, and fixed/variable/matrix file roundtrip passed.
- Ignored performance smoke run: pass across native, compression, checkpoint,
  keyed, layout, matrix, CRC, merge/compact, mmap, and recovery paths.

No new randomized fuzz/sanitizer or Miri campaign was run for this exact commit.
The fuzz workspace was compiled and audited, and the full deterministic fault
suite ran. Historical campaign results are not counted as fresh evidence here.

## Release Benchmark

Environment: Windows, active `Power saver` plan, no other Cargo/Rust processes
at start. Workload: release build, 10,000 records, one warmup plus five measured
runs. Values below are medians compared with the previous `8732e83` baseline on
the same plan.

| Metric | Current median ms | Previous ms | Delta |
|---|---:|---:|---:|
| Encode/decode fixed | 1.452 | 1.465 | -0.9% |
| Encode/decode variable | 10.318 | 10.617 | -2.8% |
| Append fixed | 319.669 | 354.444 | -9.8% |
| Open/scan fixed | 41.188 | 40.983 | +0.5% |
| Merge keyed files | 506.937 | 463.929 | +9.3% |
| Compact merged | 453.819 | 486.059 | -6.6% |
| Compact base+deltas | 517.830 | 513.062 | +0.9% |

Merge ranged from 447.780 to 510.067 ms, which crosses the prior point
baseline. Compact runs also varied materially. With those ranges and the power
plan, the isolated merge median is not enough to establish a regression. Codec,
append, open, and compact results show no broad slowdown. A dedicated benchmark
host and repeated confidence intervals are still required for release-grade
performance claims.

## Publication Gate

Do not publish `0.4.0` from this commit. Minimum closure criteria:

1. Fix F-01 through F-05 and add the focused regressions described above.
2. Resolve or explicitly narrow the guarantees for F-06 and F-10.
3. Preserve both durability facts in F-07 and report cleanup refusal in F-08.
4. Repair the fuzz artifact gate in F-09.
5. Re-run the full mechanical suite, a fresh randomized/sanitizer campaign, and
   the same release benchmark protocol.
6. Decide explicitly whether matrix recovery joins the PB contract. If yes,
   P-02 and P-03 become release blockers and require batched I/O plus
   progress/cancellation.
