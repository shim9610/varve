# Varve Performance and Defensive-Engineering Review

## Round 1 Candidate Draft

- Date: 2026-07-19 (Asia/Seoul)
- Target: `4e07a3f44fef51cbc5da55595aa5bf60184f012f`
- Branch: `codex/dev-next`
- Status: **unverified candidate report**
- Method: four independent agents started with empty conversation context and
  reviewed performance, input/resource safety, storage correctness, and API/
  macro/release behavior. They were forbidden from reading prior review reports
  and Git history before forming findings.

This document deliberately preserves Round 1 claims before adjudication. A
second, entirely different clean-context team must classify every candidate as
`confirmed`, `narrowed`, `rejected`, or `unverified`. Severity labels in this
draft are therefore not final release decisions.

## Candidate Summary

| ID | Round 1 severity | Candidate |
| --- | --- | --- |
| PERF-R1-01 | High | Matrix commit-map CRC updates may hash the whole bitmap per bit update, producing quadratic total work. |
| PERF-R1-02 | High | CRC-enabled matrix metadata may require linear physical initialization and large resident bitmaps at PB scale. |
| PERF-R1-03 | High if advertised as PB-capable | Resident keyed merge/compact materializes all historical/live keys and sorts them. |
| PERF-R1-04 | Medium | Shared sidecar registry pruning may make opening many live identities quadratic and globally serialized. |
| PERF-R1-05 | Medium | Resident append with block offset chains may reverse-scan the resident index for every append. |
| DEF-R1-01 | Medium | Distinct large variable-field IDs may allocate in a `HashSet` without charging the decoder materialization budget. |
| API-R1-01 | High | First-use manual block registration may bypass the generated format identity fingerprint. |
| API-R1-02 | High | Resident `push` may emit incomplete keyed predecessor chains when keyed offset chaining is enabled. |
| API-R1-03 | High | Disk-index descriptors may bypass the common block fingerprint registration gate. |
| API-R1-04 | High | Computed schema hashes may collide for different custom nested codecs with identical source type tokens. |
| API-R1-05 | Medium | Renamed-facade token rewriting may rewrite user-owned absolute `::varve` type paths. |
| DUR-R1-01 | Medium documentation concern | Matrix sidecar documentation may describe two orderings without binding `write_matrix_sidecar` to one contract. |

## Performance Candidates

### PERF-R1-01: Matrix commit-map CRC update cost

Round 1 claims that CRC-enabled matrix bit mutations call a helper that hashes
the complete `ceil(cell_count / 8)` bitmap after each write/commit/clear bit
change. If true, committing `C` cells after writing them processes roughly
`2 * C * ceil(C / 8)` bitmap bytes and is `Theta(C^2)`.

Evidence locations:

- [`matrix.rs`](../crates/varve-core/src/matrix.rs), especially the write,
  commit-bit, clear, and bitmap-CRC helpers around lines 730, 847, 1535, and
  3097.
- [`perf_smoke.rs`](../crates/varve/tests/perf_smoke.rs), whose matrix exercise
  is ignored by default and does not assert a scaling ratio.

Required Round 2 check: trace every call and distinguish data-slot CRC from
commit-bitmap CRC; add an instrumentation or bounded scaling proof without
creating a large physical file.

### PERF-R1-02: Matrix metadata initialization and residency

Round 1 claims that, per matrix block, creation writes four CRC bytes and one
validity bit per cell and that commit maps add another bit per cell. Open may
retain commit, CRC-valid, written, and current-write bitmaps. The submitted
estimate for one 1-PiB, 4-KiB-cell block was about 1.0625 TiB of initialized
metadata writes and 128 GiB of resident bitmaps.

Evidence locations include matrix layout creation/open code around lines 393,
525, 657, 1880, 2125, 2290, and 2358 in
[`matrix.rs`](../crates/varve-core/src/matrix.rs).

Required Round 2 check: independently recompute each byte/bit term, identify
which regions are sparse versus explicitly written, and determine whether the
API documents a lower practical matrix ceiling.

### PERF-R1-03: Resident merge/compact memory bound

Round 1 claims that `merge_keyed_files`, `compact_keyed_files`, and
`compact_keyed_file` open inputs through resident full-file indexes, collect
historical state in a `HashMap`, duplicate live values into a `Vec`, sort it,
and append output records one at a time. The proposed bound is `Theta(K-ever)`
resident memory plus `Theta(K-live log K-live)` sorting.

Evidence locations: keyed merge/compact implementations around lines
5429-5559 in [`file.rs`](../crates/varve-core/src/file.rs).

Required Round 2 check: confirm ownership/materialization behavior and compare
the public claim with the scalable stream/indexed APIs. A documented resident-
API limitation is not automatically a defect.

### PERF-R1-04: Shared sidecar registry cardinality

Round 1 claims each registry access prunes all live entries while holding a
process-global mutex. Opening `H` simultaneously live, distinct identities may
therefore inspect `0 + 1 + ... + (H - 1)` slots.

Evidence locations: shared-registry lookup, insertion, and pruning around lines
1260-1336 in [`disk_index.rs`](../crates/varve-core/src/disk_index.rs).

Required Round 2 check: verify whether pruning really runs on every hit and
insert, whether dead weak entries change the bound, and measure a bounded
multi-identity scaling ratio.

### PERF-R1-05: Resident block-chain predecessor lookup

Round 1 claims resident append reverse-searches the entire resident index to
find the prior record of the same block, while the scalable writer maintains a
tail table. This can be `Theta(N * B)` for `N` records distributed across `B`
block types and quadratic when each record has a distinct block type.

Evidence locations:

- resident append around line 3936 in
  [`file.rs`](../crates/varve-core/src/file.rs);
- scalable tail handling around line 1191 in
  [`stream.rs`](../crates/varve-core/src/stream.rs).

Required Round 2 check: verify actual index representation and enabled-policy
conditions. Classify against the documented resident-versus-scalable boundary.

## Input and Resource Candidate

### DEF-R1-01: Large field-ID bookkeeping is not budgeted

Round 1 claims `Decoder::note_field_id` inserts each distinct field ID above 63
into a `HashSet` but does not charge the decoder materialization budget.
Derived variable decoders visit unknown fields before skipping them. A proposed
bounded reproducer is a valid 160-KiB variable payload containing 10,000 empty
fields with IDs `64..10064`, decoded with materialization limit zero.

Evidence locations:

- field bookkeeping and variable-field parsing around lines 203-212 and
  381-448 in [`codec.rs`](../crates/varve-core/src/codec.rs);
- generated variable decode around lines 455-470 in
  [`varve-macros/src/lib.rs`](../crates/varve-macros/src/lib.rs);
- nearest existing prevalidation test around lines 292-300 in
  [`codec_hardening.rs`](../crates/varve/tests/codec_hardening.rs).

Required Round 2 check: compile and run the bounded reproducer, measure whether
allocation occurs with a zero budget, and account for record-size and scan
limits. Distinguish an uncharged allocation from an unbounded allocation.

## API, Macro, and Schema Candidates

### API-R1-01: First-use block registration

Round 1 claims `ensure_registered_block` validates ID/version/kind but seeds a
process registry from the first caller's own `SCHEMA_FINGERPRINT` and
`IS_KEYED`, without first comparing them to `FormatSpec::block_identities`.
If a manually implemented type reaches the API before the generated type, it
may establish the wrong identity for the registered block ID.

Evidence locations:

- registration around lines 227-310 in
  [`collections.rs`](../crates/varve-core/src/collections.rs);
- generated block identities around lines 2224-2231 in
  [`varve-macros/src/lib.rs`](../crates/varve-macros/src/lib.rs);
- manual invariant tests around lines 324-380 in
  [`manual_trait_invariants.rs`](../crates/varve/tests/manual_trait_invariants.rs).

Required Round 2 check: create a fresh process/spec where the impostor is the
first type registered, then prove acceptance or rejection at every public
entry point.

### API-R1-02: Resident keyed predecessor chain

Round 1 claims resident `push_info` always delegates with
`prev_same_key_offset = None`. With `keyed_offset_chain` enabled, two pushes of
the same key may both serialize a zero predecessor, while the scalable stream
API explicitly rejects an incompatible call path.

Evidence locations:

- resident push around lines 2075-2099 and footer emission around 3936-3954 in
  [`file.rs`](../crates/varve-core/src/file.rs);
- scalable guard around lines 758-762 in
  [`stream.rs`](../crates/varve-core/src/stream.rs);
- renamed fixture's single-record coverage in
  [`tools/rename-fixture/src/main.rs`](../tools/rename-fixture/src/main.rs).

Required Round 2 check: write two equal-key generated records through each
resident public writer facade, inspect physical predecessor offsets, reopen,
and verify lookup semantics.

### API-R1-03: Disk-index descriptor identity

Round 1 claims `DiskIndexDescriptor::of<T>` captures decode/key functions and
only checks that `T` declares keyedness; `DiskIndexPlan::canonical` checks ID
and version but not the generated fingerprint before rebuilding/extracting.

Evidence locations: descriptor creation around lines 646-662, canonical-plan
validation around 760-801, and extraction around 1142-1173 in
[`disk_index.rs`](../crates/varve-core/src/disk_index.rs).

Required Round 2 check: construct a manual keyed type with matching ID/version
and different fingerprint/codec, then prove whether plan construction or first
use rejects it before decoding file bytes.

### API-R1-04: Transitive custom-codec schema identity

Round 1 claims derive fingerprints use source token text for field types. Two
modules can each define a custom nested type named `Payload` with different
wire behavior while otherwise-identical outer blocks produce the same computed
schema hash.

Evidence locations:

- derive fingerprint generation around lines 594-619 in
  [`varve-macros/src/lib.rs`](../crates/varve-macros/src/lib.rs);
- computed format hash around lines 1796-1827 in
  [`format.rs`](../crates/varve-core/src/format.rs);
- current collision tests in
  [`schema_hash_contract.rs`](../crates/varve/tests/schema_hash_contract.rs).

Required Round 2 check: build the two-module compile/runtime fixture and compare
outer fingerprints, format hashes, and bytes. Decide whether custom codecs are
contractually required to provide a separate schema identity.

### API-R1-05: Renamed dependency versus user path rewriting

Round 1 claims renamed-facade support recursively rewrites every absolute
`::varve` token in the complete generated stream, including user-supplied type
tokens. A downstream crate that renames the dependency to `vv` while defining
its own valid `::varve::LocalType` namespace may have that user path rewritten
to `::vv::LocalType`.

Evidence locations: token rewriting around lines 102-178 and user field-type
emission around lines 352-377 in
[`varve-macros/src/lib.rs`](../crates/varve-macros/src/lib.rs).

Required Round 2 check: extend an isolated renamed-dependency fixture with the
claimed local absolute path and compile it. Separate unsupported namespace
tricks from ordinary valid Rust downstream code.

## Storage Documentation Candidate

### DUR-R1-01: Matrix sidecar ordering contract

Round 1 found no confirmed storage implementation failure before it was asked
to stop. It did identify a possible documentation ambiguity: one section says
a logically participating sidecar should be synchronized before native data
and commit publication, while the current `write_matrix_sidecar` description
states native sync followed by sidecar publication.

Evidence locations:

- sidecar ordering discussion around lines 204-217 and API behavior around
  253-259 in [`durability-model.md`](durability-model.md);
- public methods around lines 1715 and 3379 in
  [`file.rs`](../crates/varve-core/src/file.rs).

Required Round 2 check: identify whether the sections intentionally describe
different scopes. Trace the implementation and tests; classify this as a real
contract ambiguity, a correct distinction, or an implementation mismatch.

## Round 2 Storage Checklist

The first storage reviewer did not complete these checks and did **not** claim
them as defects. Round 2 must independently verify:

1. Native-object writer exclusion across hard-link/path aliases and replacement
   generations.
2. Windows `ReplaceFileW` outcomes 1175, 1176, and 1177, including temporary
   retention and writer poisoning for indeterminate publication.
3. Parent-directory synchronization result propagation after successful
   publication.
4. Native/checkpoint/sidecar ordering and stale-sidecar rejection after object
   recreation or replacement.
5. Flush, sync, commit, recovery, rollback, and cancellation result accuracy.
6. Panic, child-process failure, sidecar, and lock-marker test cleanup.

These are verification obligations, not findings.

## Round 1 Protections Reported

The independent reviewers reported the following protections as present, but
Round 2 should spot-check them before the final report:

- checkpoint-on-flush decision state is incremental rather than a full index
  rescan;
- scalable stream/indexed open uses sidecar frontiers instead of a native
  startup scan in the normal valid-sidecar path;
- rebuild performs one sequential native scan and reuses validated payload
  bytes for CRC/key extraction;
- native and custom-layout scans use checked extents, strict progress, and
  runtime scan/record/index limits;
- matrix dimensions and aggregate extents are checked before large reads;
- mmap/zero-copy constructors expose unsafe preconditions and perform size,
  endian, alignment, and range checks before producing references;
- create/open binds the native object lock before truncation;
- matrix creation uses a fresh nonce;
- CI contains feature-matrix Clippy, clean archive, renamed-dependency, audit,
  deny, and test-hygiene jobs.

## Main-Agent Mechanical Baseline

These results were produced independently of the Round 1 reviewers:

| Gate | Result |
| --- | --- |
| `cargo fmt --all -- --check` | Pass |
| `cargo check --workspace --all-features --all-targets --locked` | Pass |
| `cargo clippy --workspace --all-features --all-targets --locked -- -D warnings` | Pass |
| Clippy: default, no-default, integrity, mmap, zero-copy, compression-zstd, high-cardinality-dev, scalable-fault-injection | All pass with `-D warnings` |
| Clean `git archive` metadata | Pass; all four workspace members are tracked |
| Clean `git archive` all-feature/all-target check | Pass |
| Default-feature workspace tests through `varve-test-runner` | Pass in 84.3 s; session cleanup verified empty |
| Focused prior-leak matrix tests | 2/2 pass in 0.03 s; session cleanup verified empty |
| Root `cargo audit` | Pass; 79 dependencies, 1,166 advisory entries loaded |
| Fuzz `cargo audit` | Pass; 38 dependencies, 1,166 advisory entries loaded |
| Root and fuzz `cargo deny check` | Pass |

Release example benchmark, warm machine, 10,000 records:

| Operation | Time | Reported throughput |
| --- | ---: | ---: |
| fixed encode/decode | 1.463 ms | 6,837,139 records/s |
| variable encode/decode | 10.867 ms | 920,175 records/s |
| fixed append | 341.470 ms | 29,285 records/s |
| fixed open/scan | 41.576 ms | 240,523 records/s |
| merge keyed files | 536.063 ms | 18,655 records/s |
| compact merged | 507.728 ms | 19,696 records/s |
| compact base+deltas | 604.028 ms | 16,556 records/s |

This small benchmark is a smoke baseline, not evidence of PB-scale behavior or
a comparison with another storage engine.

## Artifact Note

Before this review, `%TEMP%` contained two non-sparse files from a terminated
older `matrix_hardening` test process (`PID 39848`) with logical and allocated
sizes of about 12 GiB and 8 GiB. Their exact test-owned paths and 58 matching
zero-byte lock markers were removed after confirming the process was absent;
C: free space rose from 37.33 GiB to 57.33 GiB. They predated target commit
`4e07a3f` and are not attributed to the current runner.

Re-running the same two test names on the target commit used per-test
`TempDir`, created only small fixtures, passed, and left no session directory.
The default workspace suite also left no session directory. The final
all-feature suite remains a Round 2/final mechanical gate.

## Round 1 Limitations

- Round 1 agents performed mostly static tracing and bounded proofs; the main
  agent, not those reviewers, ran the mechanical baseline.
- No real PB file was created.
- The first storage agent was stopped by a platform filter; its output was
  discarded. A new empty-context replacement produced the storage section.
- Windows power-loss semantics cannot be proven by ordinary unit tests; fault
  hooks can only verify state-machine and result propagation behavior.
- This draft must not be used as the final release verdict.
