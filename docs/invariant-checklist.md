# Varve Invariant Checklist

Status: living document. Last pass 2026-07-20, against the working tree on
`codex/dev-next` at the round-7 re-verification (parent commit `060851a`).

**Scope of the inventory below, stated honestly.** It is a rounds-3-to-5
inventory plus the round-6/7 work, not a rounds-1-to-5 inventory. It was built
from the review reports and `git log`, but the six modules that rounds 1-2
(`28a1b68`) added - `disk_index.rs` (the redb-backed sidecar persisted store),
`stream.rs`, `indexed.rs`, `scan_control.rs`, `pib_probe.rs`,
`scalable_extent.rs` - are covered only by the narrow slices named in the tables
(sidecar identity/generation versions, the shared `Database` registry, the
checkpoint cadence, the `TailCache`). **Not yet walked:** the sidecar's own
persisted state and damage semantics, the stream checkpoint sidecar,
`StreamResidentState` / `StreamReaderState`, and the `scan_control` budget
model. A future round inheriting this table must not read the absence of a row
as an audit. See open item 7.

## Why this document exists

Five consecutive adversarial reviews found defects almost exclusively in code
this project **added while fixing an earlier finding**, not in the original
codebase:

| Round | Report | Blockers landing in code added by an earlier round |
| --- | --- | --- |
| 1 | `docs/adversarial-performance-security-review-2026-07-19.md` | - |
| 2 | `docs/adversarial-performance-security-review-2026-07-19-opus-final.md` | - |
| 3 | `docs/adversarial-performance-security-review-2026-07-19-4e07a3f-final.md` | some |
| 4 | `docs/performance-stability-review-2026-07-20-0d4b9c6-final.md` | most |
| 5 | `docs/performance-stability-review-2026-07-20-060851a-final.md` | **all six** |

Round 5's six release blockers map one to one onto patches from rounds 3-4:
F-01 to the resident keyed-tail cache (round 4's PERF-05), F-02 to the hash-cost
model (round 4's SAFE-01), F-03/F-06 to the matrix page index (round 4's
PERF-01), F-04 to the merge estimate (round 3's PERF-03), F-05 to the self-test
cleanup (round 3's API2-02).

The pattern is not carelessness in any single patch. It is that a fix was
reviewed against the finding it answered, and not against the properties every
structure in this library has to have. This checklist makes those properties
explicit and enumerable.

## The five invariants

Every **new** persisted structure, in-memory cache or registry, cost/limit
model, and published contract must satisfy all five before it merges.

### 1. BUDGETED

Every allocation is charged to a runtime resource limit **before** the memory is
taken - not merely guarded by `try_reserve`.

`try_reserve` converts an allocation the allocator *refuses* into a typed error.
It says nothing about an allocation the allocator will happily serve. A
structure whose size is chosen by file content, and which is only `try_reserve`
guarded, is unbounded by policy: no configured limit can refuse it. That is what
F-03 was.

The exception that does **not** need a charge is a structure whose size is
bounded by the program's own compile-time shape (declared block count, declared
type set) rather than by file content or by a caller-supplied count. Such
structures must be identified as such, with the bound stated, not assumed.

### 2. RESILIENT

Every persisted structure yields a **typed finding** on damage - never silent
truncation, never hidden data.

The failure shape to hunt for is *"damage looks like a clean end"*: a
terminator, a sentinel, a zero value, or a length that a corrupt region can
forge, such that a damaged prefix silently hides a valid suffix. That is what
F-06 was. A persisted index needs an occupancy count or high-water mark that is
itself validated, and enumeration must continue past a damaged entry and report
it rather than stopping.

### 3. ATOMIC

Every fallible step is ordered **before** the authoritative commit it supports,
or its failure returns a typed *published* outcome rather than a bare `Err`.

A caller that receives `Err` is entitled to believe nothing happened. A cache,
index, or registry updated after an append, publish, or rename must therefore
either be reserved beforehand and committed infallibly, or must report through a
typed published-outcome variant. That is what F-01 was, and the additional trap
it exposed: an update that fails *after* the append can leave a stale
predecessor that makes the next mutation link **around** the record that
succeeded.

### 4. TRUTHFUL

Every documented bound, complexity statement, or guarantee matches the
implementation exactly.

An "upper bound" that is an estimate is a defect, not a wording problem: callers
size buffers and set policy from it. Re-derive the real complexity and compare;
do not carry a claim forward because it was true when it was written. That is
what F-04, F-07 and F-08 were. Source comments count: a comment asserting a
property the code does not have is the same defect in a smaller font.

A correction pass is not finished when the claim is fixed in the file the
finding named. The same sentence usually exists in the design document, the
specification, the changelog, and the rustdoc that `cargo doc` publishes to
users, and a partial correction is worse than none: the surviving copies now
read as corroboration. Grep the whole tree for the retracted sentence, and add
the retraction to `crates/varve/tests/doc_claims.rs`, which is the gate that
turns invariant 4 into something CI can fail on. It forbids each retracted
phrase across `README.md`, `CHANGELOG.md`, `docs/` (excluding review documents,
which must quote the defect) and every `.rs` file under `crates/`, and it
compares the layout version stated in the documentation against `VMAT_VERSION`
in the source, so a version bump cannot update only some documents.

Third mechanical habit, alongside the two under *The rule*: when you write a
claim about a *runtime* counter, state its scope. Both matrix zero-range
counters are thread-local and one of them measures a single range request, not a
whole operation; documentation that omitted either qualification told callers
they could prove a cheap path they had not proved.

### 5. COMPLETE ACROSS PLATFORMS

Every platform-conditional fix is done on **every** platform, or explicitly
refused on the weak one with a typed error.

A source comment acknowledging a known window is not a fix - it is a record that
the fix was not done. That is what F-05 was. Audit every `cfg(windows)` /
`cfg(unix)` split for a weaker branch: locking, parent sync, hole punching,
deletion, identity.

## Inventory and audit status

Legend for **Audit**: `deep` = read the implementation and its tests against all
five invariants; `spot` = checked the specific invariant most at risk; `carried`
= accepted on a sibling's evidence this round without independent re-derivation.

### Persisted structures and regions

| Structure | Where | Added | 1 | 2 | 3 | 4 | 5 | Audit |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| Matrix page index (VMAT v4) | `crates/varve-core/src/matrix.rs` | round 4 (PERF-01), redesigned round 5 (F-03/F-06) | ok | ok | ok | ok | ok | carried - the round-5 fix charges `ReadLimitKey::MatrixBitmapBytes`, replaces the terminator scan with a self-checking occupancy header, and evicts entries when a page empties |
| Per-page digests | `crates/varve-core/src/matrix.rs` | round 4 | ok | ok | ok | ok | ok | spot - damage produces a `Fatal` `MatrixCorruptionKind` finding; the trust boundary (redundancy, not authentication) is documented |
| Matrix creation nonce | `crates/varve-core/src/matrix.rs`, `stream.rs` | round 3 (STO-01) | n/a | ok | ok | ok | ok | spot - a mismatch is a typed identity rejection, not a silent accept |
| Sidecar identity/generation versions | `crates/varve-core/src/disk_index.rs`, `indexed.rs` | rounds 3-4 | n/a | ok | ok | ok | ok | spot |
| Checkpoint cadence state | `crates/varve-core/src/file.rs` | round 3 | n/a | ok | ok | ok | ok | spot - growth is guarded by the linear-cost test the round-5 report verified |
| VMAT layout version gate | `crates/varve-core/src/matrix.rs` | rounds 3-5 | n/a | ok | n/a | ok | ok | carried - v1/v2/v3 are refused as stale-regenerable with `FormatVersionMismatch`; v4 is current |

### In-memory caches and registries

| Structure | Where | Added | 1 | 2 | 3 | 4 | 5 | Audit |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| Resident keyed-tail cache | `VarveFile`, `crates/varve-core/src/file.rs` | round 4 (PERF-05) | **fixed round 6, completed round 7** | n/a | ok | ok | ok | deep - was `try_reserve`-only; round 6 charged the incremental growth to `ReadLimits::max_keyed_tail_bytes` before the append (API3-02) but charged the *initial build* only after the whole map existed, so the charge gated retention rather than the peak. Round 7 (API3-05) moved the charge into the build. Ordering was fixed in round 5 (F-01) |
| Generated keyed writer tail maps | `crates/varve-macros/src/lib.rs`, `VarveFile::key_tail_offsets` | pre-existing, unaudited until round 6 | **fixed round 6 (growth), round 7 (initial build)** | n/a | **fixed round 6** | ok | ok | deep - round 6 fixed only the *incremental* insert. The dominant allocation is the map built at writer construction (`writer_tail_inits` -> `VarveFile::key_tail_offsets`), which had no charge at all: a file with `N` distinct keys forced an `N`-entry map whatever the ceiling said, and the ceiling only refused further growth within the session. Round 6's own test could not see this - it only ever created a fresh file. Round 7 charges the build as it proceeds (API3-05); regression: `generated_keyed_writer_refuses_to_open_a_file_over_the_tail_budget` |
| Resident block tails (`BlockTails`) | `crates/varve-core/src/file.rs` | round 3 (PERF3-03) | ok - bounded by the format's declared block count, not by file content; the post-append `Vec::insert` runs at most once per distinct block id | n/a | ok | ok | ok | deep |
| Process-global block-contract cache | `crates/varve-core/src/collections.rs` | round 4 | ok - bounded by the program's own `(spec, block id)` set, and only for hand-built specs; not chosen by file content | n/a | ok | ok | ok | deep |
| Shared sidecar `Database` registry | `crates/varve-core/src/disk_index.rs` | round 3 | ok - one slot per open sidecar path, bounded by handles the caller opens; pruning is amortised | n/a | ok | ok | ok | spot |
| Sparse bitmap page map / resident tails | `crates/varve-core/src/matrix.rs` | rounds 4-5 | ok | n/a | ok | ok | ok | carried |
| `TailCache` / `SharedSidecar` tail map | `crates/varve-core/src/disk_index.rs` | round 3 (`4e07a3f`) | ok - bounded by `tail_limit_for_spec` and the declared block count, validated at `disk_index.rs` `insert`/load; the bound is stated here rather than assumed | n/a | ok | ok | ok | spot - added round 7; was missing from this inventory entirely |
| `AllocatedExtents` (`query_allocated_extents`) | `crates/varve-core/src/matrix.rs` | round 4 (`0d4b9c6`), touched round 5 (F-07) | ok - a resident `Vec<(u64, u64)>` bounded by `MAX_TRACKED_EXTENTS` (8192, i.e. 128 KiB), which is a compile-time constant and not chosen by file content. The cap is checked *after* the pushes that cross it, so the true bound is `MAX_TRACKED_EXTENTS + 1` entries on Linux (one push per check) and `MAX_TRACKED_EXTENTS + 512` on Windows (one `FSCTL_QUERY_ALLOCATED_RANGES` batch per check), i.e. at most ~139 KiB either way; the growth uses infallible `Vec::push` rather than `try_reserve` | n/a | ok | ok | ok | spot - added round 7; was missing from this inventory entirely. See open item 6 |
| Fault-injection counters | `file.rs`, `matrix.rs`, `stream.rs` | rounds 4-5 | ok - thread-local `Cell`s, feature gated | n/a | ok | ok | ok | spot. See open item 4 below: one injector in `stream.rs` is `#[cfg(test)]` process-global |

### Cost and limit models

| Model | Where | Added | Status | Audit |
| --- | --- | --- | --- | --- |
| Hash-table reservation model | `crates/varve-core/src/codec.rs` | round 4 (SAFE-01), corrected round 5 (F-02) | ok - models the small-table capacity classes; documented as a deliberate over-estimate valid for control-group widths up to 32, not as a measurement of one build | carried |
| `BTreeMap` materialization charge | `crates/varve-core/src/codec.rs` | pre-existing, unaudited until now | **fixed this round** - was `len * size_of::<(K, V)>()`, i.e. entries packed end to end. A std node allocates eleven fixed slots whatever its fill, is only guaranteed to hold five, and adds a header and child pointers (API3-04) | deep |
| `KeyedMergeEstimate` | `crates/varve-core/src/file.rs` | round 3 (PERF-03), corrected round 5 (F-04) | ok - renamed to `peak_resident_structural_bytes()` and documented as a structural estimate, explicitly **not** an upper bound, with its four exclusions named | carried |
| `ReadLimits::UNTRUSTED` | `crates/varve-core/src/format.rs` | round 3 | ok | spot |
| `ReadLimits::max_keyed_tail_bytes` | `crates/varve-core/src/format.rs` | **this round** | new - the dimension the keyed-tail charge needs; `STANDARD` leaves it at `u64::MAX`, `UNTRUSTED` sets 256 MiB | deep |
| Matrix resident bitmap budget (payload + index) | `crates/varve-core/src/matrix.rs` | round 5 (F-03) | ok | carried |

### Published contracts and claims

| Claim | Where | Status | Audit |
| --- | --- | --- | --- |
| Merge sizing entry point | `docs/api-reference.md`, `docs/performance.md`, `docs/update-compact-guide.md` | ok - "upper bound" retracted everywhere; only the three count fields are labelled bounds | spot |
| Matrix open complexity | `docs/performance.md` | ok - stated as O(Q) time, Theta(Q) memory, no sort, over the candidate-page union | carried |
| Whole-category clear cost | `docs/performance.md`, `docs/matrix-storage-design.md` | ok - names the two range-removal mechanisms and states the Theta(cells/8) streaming fallback, with the runtime accessor that proves which ran | carried |
| Durability wording | `docs/durability-model.md` | ok | spot |
| Limits table | `docs/declaration-and-internals.md`, `docs/api-reference.md` | updated this round with `keyed_tail` | deep |
| Immutable-CI / feature-matrix wording | `README.md`, `.github/workflows/ci.yml` | ok - narrowed to what is actually pinned and to the nine configurations actually run | carried |

### Platform-conditional paths

| Path | Where | Status | Audit |
| --- | --- | --- | --- |
| Self-test artifact cleanup | `crates/varve-core/src/diagnostics.rs` | ok - Windows deletes through the verified non-delete-shared handle; Unix deletes via `openat`/`unlinkat` confined to an exclusively owned directory, or **refuses with a typed `Environment` step failure** | carried |
| Writer-lock marker cleanup | `crates/varve-core/src/diagnostics.rs` | **improved round 6, residual stated** - was `remove_file` by pathname on both platforms, guarded only by an advisory lock; now routed through `remove_path_if_same_object`, with `Refused`/`Failed` reported as a typed `cleanup` step (domain `Environment`) instead of swallowed (API3-03). The identity is captured by opening **the same unverified pathname**, so it closes the capture-to-unlink window but does not prove the marker is ours. On Unix the exclusive-directory confinement carries the real protection; on **Windows there is no directory confinement**, so a marker swapped in before the capture is still deleted. See open item 5 | deep |
| Replacement publication + parent sync | `crates/varve-core/src/file.rs` | ok - typed `PublishedButParentSyncPending` / `ReplacePublicationIndeterminate` on both platforms | spot |
| Sparse range removal / hole punching | `crates/varve-core/src/matrix.rs` | ok - qualified and runtime-detectable rather than claimed uniformly | carried |
| File identity | `crates/varve-core/src/file.rs` | ok | spot |

## The rule

**Any new persisted structure, cache, registry, cost model, or published claim
must be walked against all five invariants, and the walk recorded in this table,
before it merges.** A fix that answers a finding but is not walked is how the
last three rounds produced their blockers.

Two mechanical habits catch most of it:

1. When you add a structure whose size depends on file content or a
   caller-supplied count, ask which `ReadLimitKey` charges it. If the answer is
   "none, but it uses `try_reserve`", the answer is no.
2. When you add a step after an append, publish, or rename, ask what the caller
   sees if it fails. If the answer is a bare `Err`, the answer is no.

## Known open items

These are recorded rather than silently accepted. Each names the file and the
recommended remediation.

1. **`hash_table_reservation_bytes` is now `pub(crate)`**
   (`crates/varve-core/src/codec.rs`), which unblocks adding a
   `state_table_overhead_bytes` field to `KeyedMergeEstimate`. That would make
   the estimate closer, but still not a bound - `Key`/`T` heap is unobservable
   without a caller-supplied-bounds trait - so it must stay under the "structural
   estimate" contract. Not blocking.
2. **The generated keyed writers' charge is inline storage only.** They store
   `T::Key` directly rather than a canonical byte payload, so unlike the resident
   path they cannot observe heap owned by `K` (a `String` key's buffer is not
   charged). Closing this needs a `Key`-provided heap-size hook. The rustdoc on
   `VarveWriter::reserve_keyed_tail_slot` states the limitation exactly rather
   than implying a whole-structure charge.
3. **`max_keyed_tail_bytes` is checked per keyed block id, not summed across
   block ids.** A format with many keyed blocks can therefore hold that many
   times the ceiling. Summing would need a per-file running total in
   `KeyedTails`; the per-block-id semantics are documented in
   `docs/api-reference.md` and in the field's own rustdoc.
4. **The sidecar publication interposition hook in `crates/varve-core/src/stream.rs`
   is `#[cfg(test)]` process-global state**, keyed by the canonical primary path.
   It is compiled out of every non-test build, but a test that arms it and then
   fails before the publication point leaves it armed for the rest of that
   binary's run. A thread-local cell would be tidier.
5. **Windows writer-lock marker removal has no directory confinement.**
   `remove_unowned_lock_marker` (`crates/varve-core/src/diagnostics.rs`)
   captures the marker's identity by opening the same unverified pathname, so
   the identity check is self-referential: it closes the capture-to-unlink
   window but proves nothing about whose marker it is. On Unix the exclusive
   directory confinement in `remove_path_if_same_object` supplies the real
   protection; Windows has no equivalent, so a marker swapped in before the
   capture is still deleted. Closing it needs the marker's identity to be
   captured at creation time (in `file.rs`) and threaded to the removal, or a
   Windows directory-handle-relative delete.
6. **`AllocatedExtents` grows with infallible `Vec::push` and is capped after
   the fact** (`crates/varve-core/src/matrix.rs`). Bounded and small - see the
   inventory row for the exact bound on each platform - but the cap is a
   compile-time constant rather than a `ReadLimits` charge, and OOM there would
   abort rather than return a typed error.
7. **The rounds-1-2 module set is not walked.** See the scope note at the top of
   this document. Recommended: assign `disk_index.rs`, `stream.rs`,
   `indexed.rs` and `scan_control.rs` an explicit owner and walk each against
   the five invariants, adding rows here, before the next release.
8. **The Unix cleanup branch is compile-verified, not executed on this host.**
   `cargo clippy --target x86_64-unknown-linux-gnu ... -D warnings` passes for
   the lib and its tests, but the unix-gated tests in `diagnostics.rs` and
   `self_check.rs` have never been run. The Linux CI leg must confirm them,
   especially if it runs as root.

## Related reports

- `docs/performance-stability-review-2026-07-20-060851a-final.md` (round 5, the
  six blockers this checklist exists to stop recurring)
- `docs/performance-stability-review-2026-07-20-0d4b9c6-final.md` (round 4)
- `docs/adversarial-performance-security-review-2026-07-19-4e07a3f-final.md`
  (round 3)
- `docs/adversarial-performance-security-review-2026-07-19-opus-final.md`
  (round 2)
- `docs/adversarial-performance-security-review-2026-07-19.md` (round 1)
