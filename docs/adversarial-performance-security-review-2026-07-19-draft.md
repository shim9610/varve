# Varve Adversarial Performance and Security Review - Round 1 Draft

Date: 2026-07-19

Status: **UNVERIFIED ROUND-1 INPUT**. Every finding below is an adversarial claim, not a
release conclusion. A separate clean-context review team must independently inspect the
code, reproduce practical claims where possible, and mark each item confirmed, narrowed,
rejected, or unverified before it may appear in the final report.

## Scope and Ground Rules

- Workspace: `C:\git\varve`, branch `codex/dev-next`, dirty development tree.
- Review surfaces: native storage, custom layouts, codecs/compression, mmap/zero-copy,
  high-cardinality redb sidecars, matrix sidecars, generated APIs/macros, durability,
  recovery, dependencies, and test/release infrastructure.
- Threat model: an attacker knows a user-defined format and supplies malformed or
  adversarially large files. Safe public APIs must not permit memory unsafety or panic.
  Resource use may be policy-bounded, but the actual defaults and enforcement points must
  be reported precisely.
- Performance model: petabyte-scale address spaces and high-cardinality streams. Results
  must distinguish resident convenience APIs from scalable streaming/indexed APIs.
- Unknown process PID 18192 (`find.exe`) is consuming approximately one logical core.
  It was not terminated. Current wall-clock benchmark results are therefore invalid for
  comparison; asymptotic/code-path findings and earlier accepted baselines remain usable.

## Mechanical Evidence Already Collected

- Rust 1.95.0, Cargo 1.95.0, LLVM 22.1.2, Windows x86_64 MSVC.
- Root `cargo audit`: 77 dependencies, zero advisories reported.
- Root `cargo deny check`: advisories, bans, licenses, and sources passed.
- Fuzz `Cargo.lock` audit and deny checks passed.
- Dependency duplicates are limited to `getrandom` 0.3.4 and 0.4.3 in test/build graphs.
- Focused hostile-input/security suite: 84 tests passed.
- Compile/API/high-cardinality/policy/self-check suite: 32 tests passed.
- Previous crash matrix after the latest implementation changes: passed in 510.96 s.
- Existing sanitizer/fuzz evidence: four ASan fuzz targets for 30 s each, strict Miri
  three-test gate, all-feature core ASan library gate, and 7,195 sidecar fuzz executions
  with no sanitizer finding or retained artifact.
- A 1 TiB sparse typed/CRC probe passed with 65,536 allocated bytes. A real 1 PiB sparse
  file remains untested because NTFS rejected creation with Windows error 87.
- No `.github` directory or CI workflow exists in this checkout.
- Test-session cleanup was checked and no `varve-test-session-*` directory remained.

## Existing Performance Baseline

The last accepted release-mode one-million-key run on 2026-07-17 recorded:

- append: 4.072 s
- final sync: 116.2 ms
- reopen: 9.35 ms
- 10,000 warm point lookups: 141.5 ms
- native bytes: 128,000,026
- sidecar bytes: 134,746,112
- peak allocator delta: 12,346,781 bytes
- native write calls: 62

It is a single warm/sequential observation, not a statistical benchmark. Later values
collected under a power-saving plan and/or external CPU load are excluded.

## Round-1 Performance Claims

### PERF-01 - Scalar indexed append may commit redb durably per record (candidate Critical)

The scalar stream `push_info` path appears to call `append_prepared_chunk`, then
`commit_state_chunk`, with redb `Durability::Immediate`. If true, scalar use performs a
durable sidecar transaction per record. `push_iter` amortizes this per chunk. Verify with
call-path inspection and transaction/flush or syscall instrumentation; do not infer only
from wall-clock time.

### PERF-02 - `checkpoint_on_flush` may produce quadratic total work (candidate High)

Each flush may serialize the entire resident index and reopen/rebuild associated state.
For a growing file with frequent flushes, total checkpoint bytes/work may be O(F^2). The
current benchmark performs only one final checkpoint and cannot detect the slope. Measure
multiple file sizes and flush intervals, including bytes written by checkpoint creation.

### PERF-03 - Resident convenience APIs are not PB-scale (candidate High, conditional)

Ordinary `VarveFile` and generated typed APIs appear to scan the complete native file and
retain every record plus keyed maps in memory. This is acceptable only if explicitly
documented as a bounded/resident API. Verify whether all generated APIs route this way and
identify the actual scalable stream/indexed alternatives.

### PERF-04 - Sidecar cardinality may grow with every key ever seen (candidate High)

Tombstones appear to replace values in the redb table rather than remove composite keys;
rebuild may replay the same tombstones. Sidecar size would then be O(K-ever), not O(K-live).
Verify with churn (insert/delete/reinsert) and inspect table counts and file size after
rebuild/compaction.

### PERF-05 - Sidecar rebuild may be O(records x descriptors) (candidate High)

Rebuild appears to test each record against every block descriptor. Tombstones may require
additional reread/decode per descriptor. Verify loop nesting and instrument a matrix over
record count, descriptor count, and tombstone ratio.

### PERF-06 - Integrity point lookup may process the native payload twice (candidate Medium)

A successful point lookup may read/stream the payload once for CRC validation and again for
typed decode. Distinguish logical payload passes, OS reads, page-cache effects, and the
integrity-disabled path. A prior duplicate frame read was already removed, so this claim
must not be reported without current-code confirmation.

### PERF-07 - Indexed readers may be unavailable while an indexed writer is active (candidate Medium)

The reader appears to open redb in a writable mode; redb then reports `IndexBusy` while the
writer owns it. The repository may intentionally test this behavior. Verify whether this is
a documented single-writer/snapshot-reader limitation or an accidental API regression.

### PERF-08 - Tail maintenance may add linear scans (candidate Low/Medium)

Block-tail operations may scan O(block-count) per commit/chunk and O(record-count) inside
the chunk. Confirm actual data structures and bound the effect at high block cardinality.

## Round-1 Hostile-Input Security Claims

### SEC-01 - Standard read limits allow unbounded open-time work (candidate High)

`ReadLimits::STANDARD` reportedly uses `u64::MAX` for file length, record count, index
bytes, scan bytes, and segment count; omitted limits resolve to the same. Native/custom
open can therefore scan, checksum, retain, clone, and sort attacker-controlled cardinality
until host resources are exhausted. Finite runtime limits appear to be enforced correctly.
Verify defaults, exact complexity, generated-format behavior, and whether this is intended
caller policy or a misleadingly safe default.

### SEC-02 - redb sidecar length limit is exposed but not enforced (candidate High)

`DiskIndexOptions::limits.max_sidecar_len` reportedly is not checked before normal or
read-only redb open. A repository test allegedly proves that a sidecar larger than one byte
opens successfully with `max_sidecar_len = 1`. Persistent savepoints may also be enumerated
without a Varve resource ceiling. Reproduce the targeted test and determine whether the
limit is documented as diagnostic-only or is a real contract breach.

### SEC-03 - Advertised memory limits are not peak-resident limits (candidate Medium)

Sequence uniqueness copies/sorts, mmap index duplication, duplicate-field hash sets,
`BTreeMap` node overhead, keyed materialization maps, and cloned keys may not be charged to
the configured allocation/index budgets. Verify each allocation and separate unavoidable
container overhead from a violated public guarantee. Quantify upper bounds where possible.

### SEC-04 - CRC is corruption detection, not authenticity (policy, not a defect)

CRC32 can be recomputed by an attacker. Applications needing provenance must authenticate
files externally. Verify that documentation never promises cryptographic authenticity.

### SEC-05 - Matrix metadata CRC may be fail-open (candidate Medium/Low)

A matrix metadata CRC mismatch reportedly becomes a `Fatal` recovery diagnostic but may
not prevent construction or cell reads. Verify the public contract and whether callers are
required to inspect recovery reports before data access.

### SEC-06 - Sidecar fuzz coverage is size-limited (test gap)

Sidecar fuzz input appears truncated to 1 MiB. Huge/sparse databases, enormous savepoint
sets, allocator pressure, concurrent backing-file mutation, and 32-bit targets therefore
remain under-tested. Confirm the harness and avoid presenting iteration count as complete
parser assurance.

## Round-1 Durability and Concurrency Claims

### DUR-01 - Successful rename followed by directory-sync error may strand the writer (candidate Critical)

During atomic replacement, the path rename may succeed and parent-directory sync may then
return an error before the writer rebinds to the replacement or becomes poisoned. The
writer could continue appending through an old, unlinked handle while the path refers to
the new file. Existing fault injection may abort instead of returning this exact error.
Verify the state transition and build a controlled mock/fault test if practical.

### DUR-02 - Path aliases may bypass the writer lock (candidate Critical)

The lock is based on `<path>.lock`. Native writers reportedly do not canonicalize aliases;
scalable writers may canonicalize symlinks but cannot collapse hard links. Two pathnames for
one file can then have different locks and both append at the same EOF/sequence. Verify on
Windows with a hard-link or alias reproduction and inspect corruption behavior.

### DUR-03 - Unix rebuild may publish a stale-file sidecar after pathname swap (candidate High)

An indexed rebuild may scan retained file handle A, while the pathname is replaced with B
during a callback, then publish A's sidecar next to B and return success. This is a
platform-conditional static claim on Windows; verify identity checks before publication and
mark untested platform behavior explicitly.

### DUR-04 - Matrix sidecars may not bind to a specific native generation (candidate High)

Verified matrix sidecars reportedly bind format/schema/category/caller generation/length
and CRC, but not a unique native file generation or object. Same-format sidecars might be
swappable and accepted. Reproduce with two compatible but different native files.

### DUR-05 - Matrix sidecar writes may be non-atomic and weakly ordered (candidate Medium)

Sidecars may use `File::create`/truncate rather than atomic replacement, lack parent
directory sync, and have no explicit ordering with native sync. Verify write code and
fault/crash semantics.

### DUR-06 - Initial file creation may omit parent-directory durability (candidate Medium)

`create` followed by native-file sync does not necessarily guarantee durable directory
entry creation across hard power loss on Unix filesystems. Verify platform behavior and
state the supported durability boundary without overgeneralizing Windows semantics.

### DUR-07 - Aborted replacements/rebuilds may leak full temporary generations (candidate Medium)

Process abort can leave full temporary files. If startup has no bounded scavenger, repeated
failures can consume disk proportional to file size. Verify naming, cleanup on normal error,
and startup recovery behavior.

### DUR-08 - Lock marker persistence may cause false stale-lock refusal (candidate Low)

Lock marker writes/clears may use `flush` instead of durable sync. After power loss an old
marker could reappear and cause availability failure. Verify because OS lock state may still
prevent safety violations even if the marker is stale.

## Round-1 API, Macro, and Supply-Chain Claims

### API-01 - `Format::self_test(path).run()` may destroy an existing target (candidate Critical)

The safe self-test API reportedly calls truncating `VarveFile::create` on the caller-supplied
path and cleanup may remove the target/lock even if creation fails. Existing tests pre-delete
their path. Reproduce against a sentinel file without risking repository data and inspect
the public documentation and naming.

### API-02 - Safe manual `VarveBlock` can impersonate a registered block (candidate High)

A manually implemented safe trait may reuse a registered block ID/version/kind while
providing different fields/codec. Registration checks reportedly do not compare type or
schema identity. Determine whether this enables schema/data confusion through safe APIs,
and whether it can affect memory safety (not alleged so far).

### API-03 - Manual keyedness traits may contradict each other (candidate High)

A type may safely implement `VarveKeyedBlock` while declaring `VarveBlock::IS_KEYED = false`,
potentially bypassing keyed-chain enforcement in low-level stream/indexed paths. Reproduce
with a compile-pass and runtime test; distinguish generated API behavior.

### API-04 - Schema hash defaults may silently disable compatibility checks (candidate Medium)

The macro reportedly emits schema hash zero unless `schema_hash: computed` is explicitly
declared. Native open then skips expected-schema comparison for zero. Verify generated code,
examples, and documentation wording; decide whether this is opt-in compatibility or an
unsafe default.

### API-05 - Generated macro paths may break dependency renaming (candidate Low)

Generated code reportedly hardcodes `::varve`, so `Cargo.toml` dependency renaming fails.
Reproduce as a compile-only compatibility defect.

### API-06 - Optional zero-copy/raw surfaces remain explicitly unsafe (boundary confirmation)

No round-1 reviewer found a Rust soundness defect in safe mmap/zero-copy APIs. Bounds,
alignment, block kind, endian, and `FromBytes` checks appeared present. External mutation or
truncation remains an unsafe caller obligation. Independently re-audit before preserving
this statement.

### SUPPLY-01 - Current dependency graph is clean under available advisory/license checks

The current lockfiles have no Git dependency and pass `cargo audit` and `cargo deny`.
Optional zstd still introduces a native C build/runtime surface. Re-run or validate recorded
commands and state clearly that advisory absence is not proof of vulnerability absence.

## Required Round-2 Output

For every ID above, return:

1. Verdict: `confirmed`, `narrowed`, `rejected`, or `unverified`.
2. Exact current-code evidence with path and line or symbol.
3. Dynamic reproduction/test evidence where practical, including cleanup.
4. Correct severity and affected API/configuration.
5. User-visible mitigation and the smallest plausible remediation.

No round-2 reviewer may rely on a round-1 conclusion merely because it appears in this
draft. Unix-only behavior that cannot be exercised on this host must remain explicitly
unverified unless a deterministic platform-independent unit test proves it.
