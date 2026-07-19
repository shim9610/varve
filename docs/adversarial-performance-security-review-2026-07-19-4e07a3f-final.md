# Varve Performance and Defensive-Engineering Review

## Final Fact-Checked Report

- Date: 2026-07-19 (Asia/Seoul)
- Target commit: `4e07a3f44fef51cbc5da55595aa5bf60184f012f`
- Branch: `codex/dev-next`
- Review mode: two independent clean-context rounds plus main-agent mechanical
  verification
- Code changes made by this review: none

## Executive Verdict

The Opus changes fixed several previously reported release blockers. In
particular, checkpoint-on-flush no longer rescans the complete index, scalable
open uses a valid sidecar without a native startup scan, CRC rebuild does not
perform a second indexed-payload traversal, hard-link aliases share the native
object writer lock, Windows publication results are classified conservatively,
parent-sync failure is propagated, matrix sidecars carry a creation nonce, and
the test runner now removes successful test sessions.

The target is nevertheless **not ready for a PB-scale/general-format freeze**.
The fact-check retained four high-impact design/API defects and three high-
impact scale gaps:

1. CRC-enabled matrix cell updates still perform quadratic aggregate bitmap
   hashing.
2. CRC-enabled matrix creation/open has linear per-cell metadata I/O and bitmap
   residency that becomes impractical at PB scale.
3. There is no bounded-memory scalable merge/compact counterpart; the public
   merge/compact implementation retains historical distinct keys.
4. First-use generic block registration can accept a type whose schema identity
   disagrees with the format registry.
5. Disk-index descriptors can invoke a mismatched concrete decoder before any
   schema-identity rejection.
6. Computed schema hashes do not distinguish different custom nested codec
   implementations with the same source type spelling.
7. Stream/index sidecars do not carry a logical creation nonce, so a same-object,
   same-length primary rewrite is accepted at open as the old generation.

There is no confirmed Rust memory-safety violation in this review. The retained
risks concern asymptotic denial of service, memory-budget bypass, schema/wire
confusion, index correctness/availability, and release reproducibility.

## Method

### Round 1

Four agents started with empty conversation context. They independently read
the target source for:

- PB-scale performance and I/O complexity;
- input, allocation, and decoder bounds;
- storage lifecycle and durability;
- public API, macros, schema identity, CI, and dependencies.

They were not allowed to read previous review reports or Git history before
forming candidates. Their unverified output is preserved in
[`adversarial-performance-security-review-2026-07-19-4e07a3f-r1-draft.md`](adversarial-performance-security-review-2026-07-19-4e07a3f-r1-draft.md).

### Round 2

A different clean-context team received only the Round 1 candidate IDs and the
target source. It traced counterarguments and compiled small isolated fixtures.
Two initial workers were stopped by a platform filter; their outputs were
discarded and their scratch directories removed. Replacement workers received
no prior conversation context.

### Main Verification

The main agent independently ran clean-archive, formatting, feature-matrix
Clippy, default/all-feature tests, dependency gates, a release smoke benchmark,
and a bounded same-object sidecar-generation probe. No physically large file
was created against the target commit.

## Final Findings

| ID | Severity | Status | Scope |
| --- | --- | --- | --- |
| PERF-01 | High | Confirmed | CRC matrix commit maps |
| PERF-02 | High | Confirmed | CRC matrix create/open metadata |
| PERF-03 | High for PB claim | Confirmed and narrowed | Resident merge/compact |
| PERF-04 | Medium | Confirmed and narrowed | Many live sidecar identities |
| PERF-05 | Medium | Confirmed and narrowed | Resident block-offset chains |
| RES-01 | Medium | Dynamically confirmed | Variable-field ID bookkeeping |
| API-01 | High | Dynamically confirmed | First-use generic type registration |
| API-02 | Medium | Dynamically confirmed and narrowed | Generic resident keyed push |
| API-03 | High | Dynamically confirmed | Disk-index descriptor type identity |
| API-04 | High | Dynamically confirmed | Custom nested codec schema identity |
| STO-01 | Medium | Dynamically confirmed and narrowed | Stream/index sidecar generation identity |
| REL-01 | Release blocker | Clean-tree reproduction | Fuzz lockfile reproducibility |
| TEST-01 | Low | Observed | Windows local fault-test ergonomics |

### PERF-01: CRC matrix bit updates hash the complete category bitmap

For `IntegrityPolicy::Crc32`, a matrix commit-bit mutation reaches
`prepare_commit_bit`, which calls `crc32_bytes_with_replacement`. The helper
hashes the prefix, replacement byte, and suffix of the complete commit bitmap.
Writing a cell first clears its bit, and committing it sets the bit, so the
category bitmap is normally hashed twice.

Relevant code:

- [`matrix.rs`](../crates/varve-core/src/matrix.rs), `prepare_commit_bit` around
  line 1535 and `crc32_bytes_with_replacement` around line 3097;
- write-side clear around line 730 and commit path around line 847;
- public call path in [`file.rs`](../crates/varve-core/src/file.rs) around line
  3217.

For a category with `M` bits, each mutation costs
`Theta(ceil(M / 8))` CPU. Writing and committing all `M` cells therefore
processes `Theta(M^2)` bitmap bytes. It hashes only the affected category and
does not apply when integrity is disabled, so the Round 1 claim was narrowed
to CRC-enabled matrix commit maps. The asymptotic defect remains.

Existing tests validate corruption detection and rebuild correctness, not a
scaling ratio. This is a release blocker for the PB-scale + integrity claim.

### PERF-02: CRC matrix metadata scales per cell in I/O and RAM

For a block with `N` cells, creation explicitly writes:

```text
4*N + ceil(N/8)       CRC values plus CRC-valid bits
      ceil(N/8)       cell commit map
-----------------
4*N + 2*ceil(N/8)     approximately 4.25 bytes per cell
```

The slot payload extent itself is extended with `set_len`, but these metadata
regions are explicitly initialized. See matrix layout construction around
lines 547-548, 2290, and 2358 in
[`matrix.rs`](../crates/varve-core/src/matrix.rs).

A normal CRC-enabled open retains four logical bits per cell: commit, written,
current-write, and CRC-valid. The exact storage is the sum of commit-category
maps plus three per-block maps; non-cell categories and quarantine state can
add more. See matrix open around lines 1893 and 2115.

For one 1-PiB matrix block with 4-KiB cells (`2^38` cells), the cell-scaled
figures are approximately:

- 1.0625 TiB of explicitly initialized metadata;
- 128 GiB of resident bitmap memory.

Runtime limits reject configurations above policy, but the current physical
representation itself is not PB-practical. A paged/lazy metadata design is
required; raising limits cannot solve this.

### PERF-03: Merge/compact is resident and retains `K-ever`

`merge_keyed_files`, `compact_keyed_files`, and `compact_keyed_file` use the
same collector in [`file.rs`](../crates/varve-core/src/file.rs) around lines
5429-5587. It scans resident `VarveFile` inputs and stores:

```text
HashMap<Key, (MergeOrder, Option<T>)>
```

The map retains every historical distinct key, including tombstoned keys, but
not every historical value. Live values are moved to a vector and sorted by
final merge order.

The corrected bound is:

- time: `Theta(records + decoded bytes) + O(K-live log K-live)`;
- memory: `O(K-ever + largest resident input index + retained values)`.

Scalable stream/indexed APIs exist, but this commit exports no scalable
bounded-memory merge/compact equivalent. Therefore the feature is correct for
resident workloads but does not satisfy the PB merge/compact requirement.

### PERF-04: Registry pruning is linear while holding the global mutex

`shared_sidecar_slot` and invalidation lock the process-global registry and run
`HashMap::retain` over all slots. See
[`disk_index.rs`](../crates/varve-core/src/disk_index.rs) around lines 1275,
1294, and 1303.

The corrected scope is create/open/invalidate registry access, not ordinary
indexed reads and writes. With `S` live identities, each such registry access
does `Theta(S)` mutex-held work; opening `S` live identities sequentially can
therefore accumulate `Theta(S^2)` slot checks. Database initialization occurs
later under a per-identity lock, so the earlier claim of global serialization
during database open was too broad.

This is Medium severity unless applications open very many simultaneous
sidecars. Existing concurrency tests cover four identities, not cardinality
scaling.

### PERF-05: Resident predecessor lookup is distance-dependent

Resident block-offset chaining calls
`self.index.iter().rev().find(...)` for each append in
[`file.rs`](../crates/varve-core/src/file.rs) around line 3936. It stops at the
first matching block.

One lookup costs `Theta(g)`, where `g` is the distance to the previous record
of the same block. Across `N` appends and `B` block IDs, this is `O(N*B)` and
can reach `Theta(N^2)` when `B = Theta(N)`. Repeated appends of one block remain
linear in total. The scalable writer maintains sorted block tails and uses
`O(log B)` lookup in [`stream.rs`](../crates/varve-core/src/stream.rs) around
line 1191.

This is a resident-API performance limitation, not a regression in the scalable
path. It should either be documented or replaced with maintained resident
tails.

### RES-01: Field-ID `HashSet` allocation bypasses materialization accounting

`Decoder::note_field_id` reserves and inserts IDs above 63 into a `HashSet`,
but does not call the decoder's materialization charge function. Other decoder-
owned containers charge before reserving. See
[`codec.rs`](../crates/varve-core/src/codec.rs) around lines 186-193 and
381-405.

Round 2 compiled and ran a zero-budget fixture:

```text
headers=64
wire_bytes=1024
materialization_remaining=0
result=success, HashSet capacity allocated
```

The allocation is not unbounded independently of input: each variable field
needs a 16-byte header, IDs are `u32`, and normal file APIs enforce physical
and logical record limits before decode. With standard limits, however, the
header count can still reach millions. Direct slice decoding and deliberately
raised/trusted limits rely only on input length and `u32` cardinality.

This is a Medium resource-accounting defect. Charge the set's capacity growth
or replace it with bounded/canonical field-order validation that needs no
per-ID set.

### API-01: First generic type can seed the wrong schema identity

`ensure_registered_block` checks ID, version, and kind, but first registration
stores the caller's own `SCHEMA_FINGERPRINT` and `IS_KEYED` in the process
registry without comparing them to the format's generated block identity.
See [`collections.rs`](../crates/varve-core/src/collections.rs) around lines
227-310 and the authoritative identities in
[`format.rs`](../crates/varve-core/src/format.rs) around line 1832.

Isolated Round 2 result:

```text
first manual type: accepted
record written: yes
generated type used afterward: BlockSchemaFingerprintMismatch
```

The scope is generic typed entry points that accept caller-selected `T`.
Generated writer methods do not accept arbitrary manual types. This is High
severity because call order decides which wire type is initially accepted.

The first-use path must compare against the immutable `FormatSpec` identity;
the process registry can remain only a cache of an already validated result.

### API-02: Generic keyed push omits the predecessor chain

Two equal-key records were written through three resident facades:

```text
VarveFile::push_info       second predecessor = None
VarveWriter::push_info     second predecessor = None
generated keyed writer    second predecessor = Some(first offset)
```

All three reopened and returned the latest value, so this is not a general
lookup failure. It is an incomplete physical keyed-offset chain in the generic
facades. The relevant generic path is around lines 2075-2099 and 3936-3954 in
[`file.rs`](../crates/varve-core/src/file.rs); generated writer tail handling is
in [`varve-macros/src/lib.rs`](../crates/varve-macros/src/lib.rs) around line
5428.

Severity is Medium after narrowing. Generic pushes should either maintain the
tail or reject keyed-chain formats and direct users to the generated API.

### API-03: Disk-index plan invokes a mismatched decoder

`DiskIndexDescriptor` stores ID/version, key codec identity, and concrete
decode/key-extraction function pointers, but no block schema fingerprint.
Canonical plan validation checks ID/version and mode. Rebuild extraction calls
the captured decoder directly. See
[`disk_index.rs`](../crates/varve-core/src/disk_index.rs) around lines 637-662,
760-801, and 1142-1173.

Round 2 result:

```text
mismatched descriptor plan: accepted
concrete decoder calls during first rebuild use: 1
rebuild result: error emitted by that decoder
```

The descriptor must carry and validate the block schema fingerprint before it
can decode primary bytes. This is High severity for the disk-index API contract.

### API-04: Custom nested codecs can collide in computed schema identity

Derive fingerprint generation hashes the source token text of each field type
and its coarse `WIRE_TYPE`; it does not include the nested codec implementation
or an associated codec schema identity. See
[`varve-macros/src/lib.rs`](../crates/varve-macros/src/lib.rs) around lines
356-371 and 594-619. `computed_schema_hash` then hashes that outer fingerprint
and descriptors in [`format.rs`](../crates/varve-core/src/format.rs) around
lines 1796-1827.

Round 2 compiled two modules whose outer field type was spelled identically but
whose custom codec emitted different bytes:

```text
outer fingerprint equal: true
computed format hash equal: true
encoded bytes equal: false
```

This defeats the purpose of computed schema compatibility for custom codecs.
Introduce an explicit associated codec/schema identity and fold it transitively
into block and format fingerprints. This is High severity for wire-format
stability.

### STO-01: Same-object primary rewrite is accepted with a stale sidecar

Matrix sidecars now include a per-create nonce. Stream/index identity contains
schema hash, OS object identity, and deterministic file-header bytes, but no
logical creation nonce or primary-content generation. See
[`stream.rs`](../crates/varve-core/src/stream.rs) around lines 590-618 and
1434-1460, and [`disk_index.rs`](../crates/varve-core/src/disk_index.rs) around
lines 523-527, 1638-1655, and 2516-2549.

The main agent ran a bounded two-record probe in a clean source archive:

1. Create two valid indexed primary files with different keys but equal lengths.
2. Keep the first file's sidecar.
3. Rewrite the first primary in place with the second primary's equal-length
   bytes, preserving the first OS file object.
4. Open the first path with its old sidecar.

Observed result:

```text
open: accepted
lookup old key: CheckpointMismatch("put candidate key mismatch")
lookup new key: None
historical distinct key count: 1 (stale sidecar state)
```

Candidate key revalidation prevents silently returning the wrong value, so the
finding is narrowed from silent data corruption to generation/availability
failure and stale metadata exposure. It remains a correctness defect: open
declares the sidecar valid and avoids rebuild even though it belongs to another
logical primary generation.

Add a per-create native generation nonce, include it in stream/index sidecar
identity, and test same-object recreation and same-length rewrite. Severity is
Medium; raise it if external in-place primary replacement is inside the
supported trust model.

### REL-01: Committed fuzz lockfile is stale

The root clean archive builds. The independent fuzz workspace does not:

```text
cargo check --locked --all-targets
error: cannot update ...\fuzz\Cargo.lock because --locked was passed
```

The new `proc-macro-crate` dependency of `varve-macros` and its transitives are
absent from committed [`fuzz/Cargo.lock`](../fuzz/Cargo.lock). An unlocked
`cargo-deny` metadata resolution updated the local lockfile and initially hid
the problem. That generated change was restored after reproduction; this
review leaves the lockfile untouched.

Update the fuzz lockfile intentionally and run fuzz check/audit/deny with
locked resolution in CI. Until then, the claimed reproducible fuzz gate is
broken.

### TEST-01: Windows crash tests invoke WER and dominate suite time

The all-feature suite passed, but `scalable_crash_faults` intentionally started
crashing child processes. Windows launched `WerFault.exe` for those children.
The suite continued correctly, but this produced most of the 706-second wall
time and can surface system error UI on a developer machine.

This is not a Varve runtime defect. The test child should set an appropriate
Windows process error mode or otherwise suppress interactive WER reporting so
the release gate remains unattended and less disruptive.

## Rejected or Narrowed Round 1 Claims

### Matrix sidecar documentation ambiguity: rejected

The generic durability section describes a sidecar that participates in the
same logical commit. The matrix section explicitly documents native authority
first and sidecar publication second, and the API repeats that contract. They
describe different scopes rather than contradictory behavior.

### Renamed dependency support: ordinary case passes

An isolated downstream crate using `vv = { package = "varve", ... }` and a
local `crate::varve::Local` field compiled and round-tripped. The rewriter
preserves `crate`, `self`, and `super` paths while rewriting generated absolute
facade paths. The more unusual `extern crate self as varve` plus absolute
`::varve::Local` case was not independently compiled in Round 2, so it is not
retained as a final defect.

### No confirmed unknown-size read or unchecked offset panic

Round 1 and existing tests found checked extent arithmetic, captured snapshot
lengths, record/scan/index limits, and progress checks across native, layout,
matrix, mmap, and scalable paths. No new panic, unchecked slice, or allocate-
before-extent-validation path was confirmed. `RES-01` is specifically an
allocation-accounting omission after a valid field header, not an unknown-size
read.

## Storage Guarantees That Held

- Create/open binds the OS native-object lock before destructive truncation.
- Hard-link alias tests cover resident, stream, and layout writer paths.
- Windows `ReplaceFileW` result 1175 is treated as an intact/no-publication
  failure; 1176 and 1177 are treated as indeterminate unless object-identity
  reconciliation proves publication.
- Indeterminate publication retains the temporary replacement path and poisons
  or stops the writer as appropriate.
- Native, matrix, stream, indexed, and rebuild publication paths propagate
  `PublishedButParentSyncPending` instead of reporting full durability.
- Matrix sidecars bind schema, OS object, layout generation, and a per-create
  nonce.

The Windows classifications match the official
[`ReplaceFileW`](https://learn.microsoft.com/en-us/windows/win32/api/winbase/nf-winbase-replacefilew)
contract. The parent-sync implementation now opens a write-capable handle,
which is required by
[`FlushFileBuffers`](https://learn.microsoft.com/en-us/windows/win32/api/fileapi/nf-fileapi-flushfilebuffers).

Existing tests cover cancellation/no-publication and many crash boundaries.
Direct public stream/indexed flush/sync/batch-commit failure-result tests remain
a coverage gap, not a confirmed implementation defect.

## Mechanical Results

| Gate | Result |
| --- | --- |
| `cargo fmt --all -- --check` | Pass |
| `cargo check --workspace --all-features --all-targets --locked` | Pass |
| Workspace all-feature/all-target Clippy with `-D warnings` | Pass |
| Default/no-default/integrity/mmap/zero-copy/compression/high-cardinality/fault feature Clippy matrix | Pass |
| Tracked clean archive metadata and all-feature check | Pass |
| Default workspace suite through hygiene runner | Pass, 84.3 s |
| All-feature workspace suite through hygiene runner | Pass, 706.3 s |
| Focused previous-leak matrix tests | Pass, 2/2; cleanup verified |
| Root `cargo audit` | Pass, 79 dependencies, 1,166 advisories loaded |
| Root `cargo deny check` | Pass |
| Fuzz audit/deny on committed graph | Not accepted as final evidence because the lockfile is stale |
| Clean fuzz `cargo check --locked --all-targets` | Fail; `REL-01` |
| `git diff --check` | Pass |

The all-feature runner ended with `Varve test artifact cleanup: verified empty`.
No `varve-test-session-*` directory remained.

## Performance Smoke Baseline

Release example, 10,000 records, this machine:

| Operation | Time | Throughput |
| --- | ---: | ---: |
| fixed encode/decode | 1.463 ms | 6,837,139 records/s |
| variable encode/decode | 10.867 ms | 920,175 records/s |
| fixed append | 341.470 ms | 29,285 records/s |
| fixed open/scan | 41.576 ms | 240,523 records/s |
| merge keyed files | 536.063 ms | 18,655 records/s |
| compact merged | 507.728 ms | 19,696 records/s |
| compact base+deltas | 604.028 ms | 16,556 records/s |

This benchmark is useful only as a regression smoke test. It is too small to
validate the asymptotic findings and is not a comparison with a commercial
storage engine.

## Dependency Posture

The root resolved graph produced no RustSec finding and passed configured
license/source/ban policy. The review used the current RustSec advisory
database from [`rustsec.org`](https://rustsec.org/). `redb 4.1.0` is correctly
pinned; its documented persistent-savepoint behavior means savepoint lifetime
must remain short because retained savepoints prevent unused pages from being
freed. The current code deletes/restores savepoints as part of its bounded
generation protocol; no new savepoint leak was confirmed.

The dependency conclusion for the fuzz workspace must be rerun after fixing
`REL-01`, because the committed lockfile does not describe the current graph.

## Artifact Hygiene

At review start, two non-sparse files from an older, terminated
`matrix_hardening` process occupied approximately 20 GiB. Their names contained
the exact test and dead PID (`39848`). After verifying that PID was absent, only
those two files and 58 matching zero-byte lock markers were removed. C: free
space rose from 37.33 GiB to 57.33 GiB.

Those artifacts predated target commit `4e07a3f`. Re-running the same tests on
the target used per-test `TempDir`, passed, and left no files. The default and
all-feature runner sessions were also removed successfully.

All Round 1/Round 2 scratch directories and the main clean archive were removed
after verification. Existing unrelated untracked workspace directories were
not modified.

## Release Gates

### Required before PB/general-format release

1. Replace matrix whole-bitmap-per-bit CRC maintenance with a paged or
   incrementally composable integrity structure.
2. Page/lazily materialize matrix CRC/commit metadata so create/open costs are
   bounded independently of total cell count.
3. Add bounded-memory scalable merge/compact, or explicitly remove PB support
   from the current merge/compact contract.
4. Validate all generic block types against immutable format identities before
   caching first use.
5. Carry block schema fingerprint in every disk-index descriptor and validate
   before invoking its codec.
6. Add transitive custom-codec schema identity to computed fingerprints.
7. Add a stream/index native creation nonce and bind sidecars to it.
8. Charge large-field-ID bookkeeping to the decoder materialization budget.
9. Update `fuzz/Cargo.lock` and add a locked clean-fuzz build to CI.

### Important follow-up

1. Provide scalable merge/compact performance contracts and cardinality tests.
2. Avoid linear global-registry pruning on every open/invalidate.
3. Maintain resident block tails or document the resident complexity boundary.
4. Reject generic keyed push when it cannot maintain predecessor chains.
5. Add direct public stream/indexed publication-failure result tests.
6. Suppress interactive Windows error reporting in process-boundary tests.

## Final Assessment

The Opus patch materially improved the library and the ordinary native/scalable
append paths now pass broad correctness and hygiene gates. The previous
checkpoint O(N^2), full native scan on valid scalable open, duplicate CRC
rebuild traversal, path-only writer lock, matrix stale-sidecar nonce, and false
parent-sync success issues did not recur.

The remaining blockers are concentrated rather than diffuse: matrix integrity
representation, scalable merge/compact, and three type/schema identity gates.
Varve can be evaluated for controlled moderate-scale use with generated APIs,
but it should not yet be advertised as a frozen PB-capable arbitrary-format
foundation until the Required gates above are implemented and reverified.
