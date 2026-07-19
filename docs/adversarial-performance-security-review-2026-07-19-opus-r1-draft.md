# Varve Post-Opus Review - Round 1 Candidate Report

Date: 2026-07-19

Target: commit `28a1b688f9bb03b1af5a56ebdb6d333f29b426eb`

Status: **UNVERIFIED ROUND-1 CANDIDATES.** This document records independent
first-round findings after the Opus remediation commit. A completely different
clean-context team must reproduce, narrow, reject, or leave each claim
unverified before it can appear in the final report.

## Baseline

- Tracked working tree was clean; unrelated untracked `mck-*`, `slipway-*`, and
  `tools/` entries were preserved.
- Rust 1.95.0, Cargo 1.95.0, Windows x86_64 MSVC.
- No external `find.exe` CPU load remained.
- Local `cargo fmt`, all-feature/all-target `cargo check`, and Clippy with
  `-D warnings` passed, but the local tree contains untracked files that affect
  workspace validity.
- First-round agents used clean contexts and did not intentionally consult the
  prior review before forming findings.

## Release Candidate

### REL-01 - Committed workspace cannot load (candidate Blocker)

Root `Cargo.toml` includes `tools/varve-test-runner`, but its `Cargo.toml` and
`src/main.rs` are untracked and absent from commit `28a1b68`. A clean `git
archive` failed at `cargo metadata` because the workspace member did not exist.
The local check succeeds only because those untracked files are present.

Verify from a pristine archive/clone, check CI and packaging implications, and
confirm whether the runner should be committed or removed from all references.

## Performance Candidates

### PERF2-01 - Sidecar retention has multiple unbounded axes (candidate High)

Round 1 observed:

- One dirty generation with a persistent savepoint grew linearly under repeated
  overwrite of one key: 4,214,784 bytes at 32 updates and 67,375,104 at 512.
- Sync intervals no larger than 64 plateaued near 8,425,472 bytes.
- A long-lived read snapshot pinned pages while a writer synced every update:
  1,056,768 to 14,290,944 bytes after 64 overwrites, then 5,074,944 after drop.
- Tombstones intentionally retain historical distinct keys (`K-ever`).
- Every published chunk still uses a durable two-phase redb commit.

Candidate paths: `DiskIndexStore::begin_generation`, `publish_clean`,
`DiskIndexSnapshot`, `begin_quick_immediate`, and `historical_distinct_keys`.
Verify exact redb semantics, bounded reproduction, whether growth is reclaimed,
and distinguish intended capacity policy from a leak.

### PERF2-02 - Checkpoint bytes are O(N), but flush predicate CPU remains O(N^2) (candidate High)

`needs_index_checkpoint()` reverse-scans for the latest checkpoint and then
scans the growing suffix on every `flush()`. Instrumentation reported 179,392
entry touches at 1,024 records and 11,253,678 at 8,192, a 62.7x increase for
8x input. Serialized checkpoint entries grew only 2,582 to 20,031, so the Opus
fix appears to have corrected bytes but not predicate CPU.

Verify the counter and whether all flush/commit paths call this predicate.

### PERF2-03 - CRC disk-index rebuild reads indexed payloads twice (candidate High)

`rebuild_index` scans each record through `NativeStreamScanner`, whose CRC path
reads the complete payload, and `extract_plan_update` then materializes the
payload again to decode the key. A 64 MiB indexed record may therefore cause
128 MiB payload reads before decode/decompression effects.

Verify OS read-transfer bytes for integrity on/off and indexed/unindexed blocks.

### PERF2-04 - redb batch retains per-record table lifecycle and full-tail work (candidate Medium)

`apply_update_with_tail` reportedly opens/drops `LATEST_TABLE` per record, with
per-cycle allocation and transaction mutex work. Each batch also reloads and
rehashes all tails, approximately proportional to `batches * tails`.

Verify using current redb 4.1 APIs/counters and quantify practical magnitude.

### PERF2-05 - Descriptor width causes O(D^2) construction and O(D) hot checks (candidate Medium)

`validate_descriptors` linearly finds each descriptor in format blocks, and
generated constructors plus core open repeat validation. At 10,000 descriptors,
Round 1 estimated 100,010,000 comparisons before `FormatSpec::validate`.
`indexed_blocks.contains` is also linear despite sorted input.

Verify the loop counts and realistic public maximum/compile limits.

### PERF2-06 - Scan cancellation is record-granular (candidate Medium)

Cancellation is first checked after CRC, extraction, decompression, and redb
insertion. A 64 MiB CRC record can perform 1,024 64 KiB checksum reads and a
large decompression before observing cancellation.

Verify the exact polling points and maximum latency under standard limits.

### PERF2-07 - Resident open repeats sequence sorting; mmap duplicates index memory (candidate Medium)

`scan_records_from` and `load_index` reportedly both perform N-element sequence
uniqueness copies/sorts. On x64, mmap additionally clones the 104-byte entry
vector and builds a full hash set plus position vector. The reported lower bound
was about 320 MB index-only memory per million records including the resident
source index, before hash buckets and mapped pages.

Verify duplicate calls, actual structure sizes, and whether the accounting is
documented as nominal.

### PERF2-08 - Indexed `flush()` retains the active write batch and reader gate (candidate Medium)

`VarveIndexedWriter::flush` delegates to native stream flush without committing
the pending redb batch. A probe reported a reader `IndexBusy` after scalar append
plus `flush()`, while `sync()` released it.

Verify the intended `flush` contract, sidecar visibility, and reader coexistence.

### PERF2-09 - Global database registry lock convoys unrelated opens (candidate Low)

`open_shared_database` holds a process-global mutex while opening redb on cache
miss. Distinct slow sidecar opens serialize, and the first opener's cache setting
is retained. Verify lock scope and process-wide memory growth with live sidecars.

### PERF2-10 - Large record append still copies payloads repeatedly (candidate Low/Medium)

Stream append may build a logical vector, copy to stored bytes, copy to final
record, and copy into the chunk; compression adds bulk work. `max_bytes` is a
coalescing threshold rather than a hard one-record memory bound. Resident scalar
append also issues metadata queries, seek, and multiple writes.

Verify copy ownership and peak allocation for representative large records.

## Defensive Input Candidates

### DEF-01 - Matrix typed APIs bypass schema-fingerprint registration (candidate Medium)

`ensure_matrix_block` checks physical matrix properties but does not call
`ensure_registered_block`. Typed matrix read/write/status/mmap paths therefore
may accept a manual `VarveMatrixBlock` with matching size/layout identity but a
different fingerprint and semantic codec.

Verify with same-stride mismatched-fingerprint read/write tests and distinguish
schema confusion from memory safety.

### DEF-02 - Writer limits are checked after unbounded encoding allocations (candidate Medium)

Resident push, metadata, replacement, matrix write, and generated variable
fields may call unrestricted `encode_to_vec` before payload/slot limits are
checked. A bounded probe observed allocation before a 16-byte limit error.

Verify every safe writer path, scalable outer encoder behavior, nested generated
fields, file rollback, and realistic OOM exposure.

### DEF-03 - Matrix sidecar identity rejection occurs after payload allocation/hash (candidate Low)

`read_matrix_sidecar_file` appears to parse a fixed header, then allocate/read/hash
the payload, and only afterward validate native identity and generation. A known
mismatch may consume up to the configured sidecar/materialization ceiling first.

Verify ordering and whether early category/magic data can be checked safely.

### DEF-04 - Fingerprint documentation overstates authenticity (candidate Low)

Public wording says manual types “cannot impersonate” a registered block, but
the fingerprint is public, unkeyed, implementer-selected, process-local TOFU,
and not bound into the on-disk descriptor. It may prevent accidents, not
intentional local-code impersonation.

Verify exact wording and intended trust boundary.

## API and Compatibility Candidates

### API2-01 - Computed schema hash omits byte-layout inputs (candidate High)

`BlockDescriptor` lacks per-block endian, keyedness, codec/fingerprint, and
field ordinal. Generated fixed/matrix encoding follows declaration order, while
schema hashing reportedly sorts fields by ID. Reordering explicitly-ID'd fields
may change bytes without changing the computed schema hash. A zero default also
disables comparison.

Verify with a compile/runtime two-schema fixture and compare actual encoded
bytes and hashes.

### API2-02 - Self-test ownership remains TOCTOU-prone (candidate High)

Matrix self-test may claim a new path, close it, and reopen through truncating
creation. Cleanup removes native and lock paths by name without identity checks.
A concurrent path replacement between those operations could cause truncation
or deletion of a file not created by the self-test.

Verify the state machine with deterministic hooks or source proof. Do not perform
unsafe destructive races outside a dedicated temporary directory.

### API2-03 - Keyedness contradiction checks do not cover all keyed APIs (candidate Medium)

`KeyedBlockContract` may be evaluated only by `KeyedBlockVec`. Delete, stream,
indexed, and merge paths accept `VarveKeyedBlock` while still branching on
`T::IS_KEYED`. Round 1 compiled a manual contradictory type through a low-level
delete API.

Verify all entry points and existing UI fixture coverage.

### API2-04 - Manual block fingerprint remains self-attested (candidate Medium)

`VarveBlock` is a safe public trait whose implementer selects the fingerprint;
registration compares first-seen claims. Generated fingerprints also hash token
spelling, which may not represent resolved type identity. Verify safe schema
confusion and the intended manual-registration boundary.

### API2-05 - Proc macros still break renamed dependencies (candidate Medium compatibility)

Expansions hardcode `::varve::__core`; a fixture importing the dependency as
`vv` reportedly failed with E0433. Verify from a minimal clean Cargo project.

### API2-06 - Matrix raw byte migration checks too little (candidate Medium)

`copy_matrix_cell_bytes_from::<From, To>` copies and commits raw payload bytes,
while compatibility validation may check only dimensions and `SLOT_STRIDE`.
Same-size schemas with different endian or codec semantics could be silently
misinterpreted.

Verify the public contract and a same-size incompatible pair.

### API2-07 - CI feature coverage and supply-chain gate are incomplete (candidate Low/Blocker interaction)

CI checks the all-features union but not individual feature combinations. The
`high-cardinality-dev`-only configuration reportedly fails under `-D warnings`.
Supply-chain CI is non-blocking and no committed `deny.toml` policy was found.
Some docs still say 0.2 while workspace version is 0.3.

This is secondary to REL-01, which prevents clean CI from reaching any job.

## Durability and Concurrency Candidates

### DUR2-01 - Some `ReplaceFileW` failures may be indeterminate publication (candidate High, Windows)

`replace_path_atomically` treats every `ReplaceFileW` failure as prepublication.
Windows error codes 1176/1177 may occur after names were moved or removed. A
caller may then return a plain error without rebinding or poisoning, potentially
continuing through an old handle whose pathname state is uncertain.

Verify Microsoft contract, exact code classification, and a controlled mock or
fault abstraction. Do not claim practical reproduction without one.

### DUR2-02 - Rejected Windows directory flush is reported Durable (candidate High)

Windows `sync_parent_directory` suppresses PermissionDenied, InvalidInput, and
Unsupported. On this host the operation returned access denied, yet the library
mapped it to `ReplaceDurability::Durable`. Verify actual handle access and API
semantics. A rejected flush must not be described as completed durability.

### DUR2-03 - Matrix sidecars do not distinguish in-place native recreation (candidate High)

Matrix create truncates/reinitializes an existing file object. Native identity
uses OS object identity plus stable layout offsets; recreating with identical
dimensions preserves both. A bounded probe reportedly recreated in place and
accepted the old sidecar (`generation=41`, old payload).

Verify exact reproduction, caller generation behavior, and whether a per-create
nonce is required. Also assess the repeated CRC32-lane fingerprint claim without
overstating collision security.

### DUR2-04 - Redb sidecar publication drops `ParentSyncPending` (candidate Medium)

Stream/indexed create and rebuild call `replace_path_atomically(...)?` while the
return type carries durability state. Injected checks reportedly returned `Ok`
when parent sync was pending.

Verify each caller and public durability contract.

### DUR2-05 - First creation has an alias race before object-lock binding (candidate Medium)

Create paths may truncate before acquiring the newly created native object lock.
Two distinct lock paths resolving through a dangling alias can both observe
NotFound; one may truncate before its later `bind_native` fails.

Verify whether Windows/Unix path semantics permit the proposed race and whether
create-without-truncate then bind then truncate closes it.

### DUR2-06 - Crash-left temporary metadata has no bounded scavenger (candidate Low)

Native rewrite and redb temp files are cleaned on normal error/drop but can
remain after process termination. Empty lock markers intentionally persist and
nonempty markers require explicit stale handling. Verify naming, ownership proof,
and operational impact before recommending automatic deletion.

## First-Round Confirmed Defenses

These are still second-round verification targets, not final conclusions:

- Existing native hard-link aliases are rejected by file-object locks in
  resident and scalable paths.
- Successful replacement rebinds; injected rebind failures poison the writer.
- Checkpoint serialized bytes are amortized O(N), typed scan/point CRC duplicate
  reads are removed, and rebuild no longer loops all descriptors per record.
- Independent indexed readers can share a process-local redb handle.
- Fatal matrix findings fail closed on default access; forensic access is
  explicit.
- Ordinary pre-existing self-test paths are refused without truncation.
- Checked extents and fallible allocation remain widespread; no parser panic or
  safe memory unsafety was found in Round 1.
- mmap/raw APIs retain explicit unsafe contracts and validation.
- CRC documentation says corruption detection, not authentication.

## Required Round-2 Output

For every `REL`, `PERF2`, `DEF`, `API2`, and `DUR2` ID:

1. Verdict: confirmed, narrowed, rejected, or unverified.
2. Exact current-code/test/doc evidence.
3. A bounded dynamic reproduction where practical, with cleanup status.
4. Correct severity, platform, feature, and safe/unsafe API scope.
5. The smallest remediation and an immediate operational mitigation.

Second-round reviewers must not accept a claim merely because it appears here.
Claims based on Windows API documentation must separate documented possibility
from a locally reproduced outcome. Claims about malicious local Rust code must
be classified as a trust-boundary or schema-integrity issue, not memory safety,
unless memory unsafety is independently demonstrated.
