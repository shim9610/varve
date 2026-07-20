# Varve Performance and Defensive-Stability Review

## Final Fact-Checked Report

- Date: 2026-07-20 (Asia/Seoul)
- Target commit: `0d4b9c667503bb0df3960eaaae101218842671fa`
- Branch: `codex/dev-next`
- Compared with: `4e07a3f44fef51cbc5da55595aa5bf60184f012f`
- Review mode: two independent clean-context rounds plus main-reviewer checks
- Library source changes made by this review: none

## Executive Verdict

The Opus commit materially fixes most issues retained by the 2026-07-19
review. In particular, it removes whole-bitmap hashing from each matrix bit
update, stops eagerly writing matrix metadata at create, binds scalable
sidecars to a creation generation, validates disk-index schema identity before
decoder use, propagates custom codec identities, accounts large field ids,
keeps resident append tails, amortizes registry pruning, refreshes the fuzz
lockfile, and adds direct publication-result tests.

The commit is nevertheless **not ready for a PB-scale/general-format freeze or
an unqualified public release**. Five areas remain release-blocking:

1. A built-in `HashMap` decode path can reserve substantially more memory than
   its materialization charge represents. A bounded current-commit run ended
   at its RSS ceiling on this path.
2. Manual block registration does not validate the block endian declared by
   the authoritative format identity.
3. The process-global block-contract cache omits static slice lengths from its
   format identity key.
4. Matrix open still visits every logical bitmap page, and its extent fallback
   can turn that into full logical bitmap I/O.
5. The documented `ChunkedBytes` field and public `PackedBitmap` field no
   longer satisfy the new codec-identity compile contract; `PackedBitmap` is
   additionally misclassified as fixed width for matrix slots.

No Rust memory-safety violation, out-of-bounds access, or confirmed unchecked
offset panic was found. The highest safety finding is process availability via
incorrect memory-cost accounting, not undefined behavior.

## Review Independence

Round 1 used four empty-context reviewers with disjoint scopes:

1. performance and scale;
2. type, schema, codec, and decoder contracts;
3. durability, identity, publication, and artifact hygiene;
4. CI, release, dependency, benchmark, and documentation contracts.

Their claims were frozen in
`docs/performance-stability-review-2026-07-20-0d4b9c6-r1-draft.md` before Round
2. Round 2 used four different empty-context reviewers, each assigned a subset
of the frozen claims and instructed to reject or narrow claims before accepting
them. One Round 2 reviewer was stopped by the platform before producing a
result; that context and output were discarded and replaced with a new empty
context. Every retained finding below survived Round 2 and main-reviewer source
or bounded runtime checks.

## Finding Summary

| ID | Severity | Final status | Area |
| --- | --- | --- | --- |
| SAFE-01 | High | Confirmed runtime failure | HashMap materialization accounting |
| API-01 | High | Confirmed by isolated fixture | Manual block endian identity |
| API-02 | High | Confirmed by isolated fixture | Block-contract cache identity |
| PERF-01 | High for PB claim | Confirmed and narrowed | Matrix open page traversal |
| API-03 | Medium, public API blocker | Confirmed compile regression | `ChunkedBytes` schema identity |
| API-04 | Medium, matrix API blocker | Confirmed compile/design conflict | `PackedBitmap` identity and width |
| PERF-02 | Medium | Confirmed | Cleared sparse pages remain resident |
| PERF-03 | Medium for very wide schemas | Confirmed and narrowed | Resident block-tail construction |
| PERF-04 | Low/contract-dependent | Confirmed behavior | Per-operation descriptor searches |
| PERF-05 | Medium | Confirmed | Resident merge cost estimate |
| STO-01 | Low | Confirmed | Temporary rewrite lock marker |
| DUR-01 | Contract decision required | Narrowed | Initial create/sync pathname durability |
| CI-01 | Medium | Confirmed coverage gap | Singleton feature tests |
| CI-02 | Medium future risk | Narrowed | MSRV automation |
| CI-03 | Medium | Confirmed reproducibility gap | Mutable CI tool identities |
| DOC-01 | Medium release quality | Narrowed | Packaged documentation |
| BENCH-01 | Low | Confirmed | Merge/compact throughput labels |
| REL-01 | Low | Confirmed | Unlocked local completion command |

## Release Blockers

### SAFE-01: HashMap reservation exceeds the charged materialization cost

`Decoder::preflight_map_count` derives the materialization charge from
`size_of::<(K, V)>()`. `preflight_count` changes a zero-byte entry to a one-byte
charge, so the standard 1 GiB decoder budget can admit close to one billion
declared entries for a zero-sized key/value pair. `HashMap::try_reserve(len)`
then reserves a hash table whose buckets and control storage cost substantially
more than one byte per entry.

Evidence:

- `crates/varve-core/src/codec.rs:289` defines the standard 1 GiB budget.
- `crates/varve-core/src/codec.rs:432` charges `max(size_of(entry), 1)`.
- `crates/varve-core/src/codec.rs:459` applies that model to maps.
- `crates/varve-core/src/codec.rs:957` reserves the complete declared HashMap
  capacity before decoding its first key.
- `crates/varve/tests/codec_hardening.rs:170` checks only a count above the
  standard budget; it does not cover a very large count below that budget.

A bounded current-commit run completed the native target, then stopped in the
codec target when this reservation crossed the configured 1 GiB RSS ceiling.
The failure was an out-of-memory termination, not an ASan memory-corruption
finding. The 11-byte diagnostic input is retained under `fuzz/artifacts` as a
failed-test artifact; the five later targets were not run.

Required change:

- Charge a conservative hash-table capacity cost, including control/bucket
  overhead, before `try_reserve`; or reserve incrementally only as validated
  entries are decoded.
- Add a low-budget deterministic regression that proves a zero-sized map count
  cannot request capacity beyond the budget.
- Rerun every bounded target after the deterministic test passes.

### API-01: Authoritative block endian is not checked

Generated format identity contains `(block_id, endian, keyedness,
fingerprint)`, and computed schema hashing includes endian. `BlockContract`
retains only keyedness and fingerprint, however, and typed encoding/decoding
uses the manual type's `T::ENDIAN`.

Evidence:

- `crates/varve-core/src/format.rs:1848` exposes endian in block identity.
- `crates/varve-core/src/collections.rs:249` omits endian from
  `BlockContract`.
- `crates/varve-core/src/collections.rs:267` compares only keyedness and
  fingerprint.
- `crates/varve-core/src/collections.rs:293` discards the authoritative endian.
- `crates/varve-core/src/collections.rs:130` decodes with `T::ENDIAN`.

The isolated fixture wrote a generated big-endian block and requested the same
block id/fingerprint through a manual little-endian type. Registration
succeeded and the value was byte-swapped on read. This is a typed correctness
failure independent of file corruption.

Required change: carry `Option<Endian>` in `BlockContract` and compare it with
`T::ENDIAN` before caching or performing any typed I/O.

### API-02: Block-contract cache key omits slice lengths

The cache key is `(blocks.as_ptr(), block_identities.as_ptr(), block_id)`.
Static slices with the same starting address and different lengths therefore
share a key.

Evidence:

- `crates/varve-core/src/collections.rs:258` defines the three-component key.
- `crates/varve-core/src/collections.rs:303` records pointers but no lengths.
- `crates/varve-core/src/format.rs:973` and `:990` expose arbitrary static
  descriptor and identity slices.

The isolated fixture first registered a manual fingerprint through an empty
view of a static identity array, then used a full view of the same array whose
authoritative fingerprint disagreed. The second registration reused the first
cache entry and succeeded.

Required change: include both slice lengths in the key at minimum. A stronger
design would cache a validated immutable format-identity object rather than
infer identity from raw slice addresses.

### PERF-01: Matrix open remains proportional to logical bitmap pages

The new layout successfully makes each bit mutation hash one 4 KiB page instead
of the whole bitmap. Open is not extent-driven, however:

- `load_paged_bitmap` loops over `0..page_count` at
  `crates/varve-core/src/matrix.rs:3276`.
- It queries allocation extents only inside that loop at `:3285`.
- Commit bitmaps and every CRC-validity bitmap use the same loader at `:3342`
  and `:3395`.
- The extent list is capped at 8,192 entries at `:42`; unsupported allocation
  maps or a file exceeding that cap fall back to treating every logical page as
  readable data.
- `matrix_integrity_scaling.rs:288` counts bytes read, not pages visited.

With the fixed extent cap, open CPU is `Theta(sum(logical bitmap pages))`.
Where the allocation map is unavailable or exceeds the cap, I/O also becomes
proportional to the complete logical bitmap regions.

For scale: a 1 PiB payload composed of 8-byte cells has `2^47` cells. One bit
per cell spans `2^32` 4 KiB bitmap pages, about 4.29 billion loop iterations per
bitmap before considering multiple commit categories or validity maps.

Required change: derive the bitmap/digest page set from allocated ranges (or a
bounded persisted page index) and visit only relevant pages. An extent-count
cap must not reinterpret a highly sparse but fragmented PB file as dense.

### API-03: Documented ChunkedBytes fields do not compile

`ChunkedBytes` implements `VarveEncode` and `VarveDecode` but inherits the new
default `SCHEMA_ID = 0`. The derive macro now rejects every field codec whose
encode or decode identity is zero.

Evidence:

- `crates/varve-core/src/chunks.rs:155` and `:163` omit `SCHEMA_ID`.
- `crates/varve-core/src/codec.rs:140` and `:152` define the zero default.
- `crates/varve-macros/src/lib.rs:698` rejects zero identities.
- `docs/spec.md:52` documents use in variable fields.

An offline downstream compile of a variable block containing `ChunkedBytes`
failed at the derive-time identity assertion.

Required change: assign stable encode/decode schema identities to this built-in
codec and add a downstream compile-pass plus roundtrip test.

### API-04: PackedBitmap identity and matrix width contracts conflict

`PackedBitmap` has the same missing `SCHEMA_ID`, so an ordinary derived field
does not compile. Separately, the matrix macro identifies the spelling
`PackedBitmap` as fixed width while the actual type owns a `Vec<u8>` and encodes
`bit_len` plus variable bytes. Inline matrix stride generation uses
`size_of::<T>()`, which is not the canonical encoded width of this value.

Evidence:

- `crates/varve-core/src/matrix.rs:550` stores variable bytes.
- `crates/varve-core/src/matrix.rs:586` omits codec identity.
- `crates/varve-macros/src/lib.rs:2092` accepts the type name as fixed width.
- `crates/varve-macros/src/lib.rs:3654` derives stride from Rust object size.

Required change: remove it from fixed-width matrix fields or redesign the
public primitive with an encoded width fixed by its type. Type classification
must not rely only on the final source identifier spelling.

## Confirmed Performance Findings

### PERF-02: Individual zero pages remain resident after clearing

`SparseBitmap::set_byte` materializes and mutates pages but does not remove a
page when its final set bit is cleared. Whole-map clear, rebuild, or reopen does
release it. Long-running sparse set/clear activity can therefore consume the
bitmap budget according to historically touched pages even when `ones == 0`.

Bound: payload residency is approximately `4096 * H` bytes for `H` retained
full pages, plus uncharged HashMap/Arc/Vec metadata, up to the complete bitmap.

Evidence: `crates/varve-core/src/matrix.rs:799`, `:878`, `:1074`, and `:1779`.

### PERF-03: Resident tail construction has a schema-width quadratic term

`BlockTails::from_index` makes one forward record pass, but first-seen block ids
are inserted into a sorted vector. Reverse first appearances move
`0 + ... + (B - 1)` tuples.

Actual bound: `O(N log B + B^2)` time and `O(B)` memory. This is narrow because
`B` is schema width and scalable stream tails are predeclared. The internal
`O(N log B)` comment is incomplete, and the current test counts index visits
rather than vector movement. `FormatSpec::validate` also contains nested
schema duplicate checks, so very wide schemas already have an `O(S^2)` open
component.

Evidence: `crates/varve-core/src/file.rs:687`, `:710`; validation loops at
`crates/varve-core/src/format.rs:1874` and `:1907`.

### PERF-04: Typed operations retain descriptor-linear checks

Every typed registration first performs linear `FormatSpec::block` lookup.
Indexed operations add linear membership in indexed block ids; matrix
operations add a linear matrix descriptor search.

Representative bound: `O(S + D + log C)` for indexed access and
`O(S + M + log C)` for matrix access, where `S` is block count, `D` indexed
descriptor count, `M` matrix descriptor count, and `C` cached contracts.

This is a documented/accepted descriptor-width cost in part of the design, so
it is a scale limitation rather than an unconditional correctness defect.

Evidence: `collections.rs:227`, `format.rs:1644`, `indexed.rs:1133`, and
`matrix.rs:2223`.

### PERF-05: Resident merge estimate omits open-time sort and memory

Every resident input open copies all `N` sequences to a temporary `Vec<u64>`
and sorts it for uniqueness. Input open therefore costs `O(N log N)` and at
least the resident index plus an explicit `8N` temporary.

`KeyedMergeEstimate::largest_input_index_bytes` does not include that temporary,
and the published merge time formula omits the sort. The resident-only and
`K-ever` limits are otherwise now documented honestly.

Evidence: `crates/varve-core/src/file.rs:5781`, `:5806`, and `:7855`.

## Durability and Storage Findings

### STO-01: Successful keyed publication leaves an internal temp marker

Keyed compact/merge creates a rewrite temp and opens it through
`VarveFile::create`, creating `<temp>.lock`. Drop clears marker contents but
does not unlink it; publication renames only the data temp. A bounded compact
fixture confirmed that the empty internal marker remains after success.

Persistent markers for real user paths are intentional stable identities. This
finding concerns a generated temp pathname that no longer has a corresponding
native file. It is storage hygiene, not a writer-exclusion failure.

Evidence: `crates/varve-core/src/file.rs:6031`, `:6038`, `:6055`, `:8849`, and
`:9242`.

### DUR-01: Initial create/sync pathname durability is undefined

`VarveFile::sync` calls `sync_all` on the native file. Initial creation does not
sync the parent directory, while atomic replacement explicitly does and
reports a pending parent sync.

This guarantees synced file contents, but on platforms that require directory
sync it does not establish power-loss durability of the newly created
pathname. Public wording says `sync` requests durable persistence without
stating whether initial pathname creation is included. This is a policy and
documentation decision; it becomes an implementation defect only if the public
contract intends pathname survival.

Evidence: `crates/varve-core/src/file.rs:1906`, `:3166`, and `:8680`;
`docs/durability-model.md:13`.

## Release and Documentation Findings

### CI-01: Isolated feature behavior is not tested in CI

CI tests default and all-feature configurations. Singleton configurations get
Clippy only. Tests that are active for compression without integrity or
integrity without compression run in neither test job. Both isolated commands
passed locally in this review: 11 and 6 tests, with cleanup verified.

Evidence: `.github/workflows/ci.yml:34`, `:72`;
`crates/varve/tests/compression.rs:687` and `:696`.

### CI-02: The MSRV is not pinned as a CI job

Packages declare Rust 1.95, while CI requests moving stable. This review did run
on `rustc 1.95.0` and the complete all-feature suite passed, so current MSRV
compatibility is supported. Once stable advances, CI will no longer prove the
declared floor unless an explicit 1.95 job is retained.

### CI-03: CI tool identities are mutable

Workflow actions use mutable major/stable tags, and `cargo-audit` installation
does not pin an exact release. Current runtime dependency checks pass; this is
about reproducibility of future gate behavior. Pin action commit ids and audit
tool versions for a release-grade supply-chain gate.

### DOC-01: Packaged rustdoc and guides are incomplete

The facade crate has no crate-level rustdoc and the public proc macros have no
entry-point rustdoc. Offline locked `cargo package --list` succeeded for all
three packages, but the resulting file lists include no repository README,
license file, or `docs/` guides. The license expression remains present in
metadata, so this is discoverability and release quality rather than a license
violation.

The API feature table omits two exported features. One is explicitly
experimental elsewhere and one is test infrastructure, so the broad claim
that both are undocumented public features was rejected.

### BENCH-01: Merge/compact records-per-second labels are not operation rates

The benchmark adds updates, deletes, and inserts, then passes the original base
record count to all merge/compact reports. Elapsed times and fixed-workload
regression comparisons remain useful; the absolute throughput labels are not
the number of processed events or emitted live values.

### REL-01: The documented local completion command is unlocked

The runner's default child command and the documented no-argument invocation
omit `--locked`. CI supplies locked outer and inner commands. The local command
can therefore refresh a stale lockfile before the user notices. Make locked
resolution the runner default or document the exact locked release command.

## Prior Findings That Are Fixed

The following 2026-07-19 findings did not recur:

- checkpoint-on-flush cumulative rescanning;
- complete native scan on clean scalable open;
- duplicate indexed CRC rebuild traversal;
- path-only writer locking across hard links;
- missing matrix sidecar creation generation;
- missing stream/index creation generation;
- false parent-sync success after replacement;
- missing disk-index schema fingerprint before decoder use;
- source-spelling-only identity for custom nested codecs;
- uncharged large variable field ids;
- per-append reverse block-tail scanning;
- global sidecar-registry sweep on every open;
- stale committed fuzz lockfile;
- missing public stream/indexed publication-result tests;
- interactive Windows reporting during intentional process-boundary tests.

Resident merge/compact is now explicitly documented as resident-only and has a
typed distinct-key ceiling. That resolves the false PB contract, although no
bounded-memory external merge/compact implementation exists.

## Mechanical Results

| Gate | Result |
| --- | --- |
| Toolchain | `rustc 1.95.0`, `cargo 1.95.0` |
| `cargo fmt --all -- --check` | Pass |
| locked workspace all-feature/all-target check | Pass |
| locked workspace all-feature/all-target Clippy, warnings denied | Pass |
| complete all-feature workspace suite through cleanup runner | Pass, 211.2 s |
| isolated compression-only behavior | Pass, 11 tests |
| isolated integrity-only behavior | Pass, 6 tests |
| root dependency audit | Pass, 79 dependencies, 1,166 advisories loaded |
| root license/source/ban policy | Pass |
| fuzz workspace locked all-target check | Pass |
| fuzz dependency audit | Pass, 50 dependencies |
| fuzz license/source/ban policy | Pass |
| three offline locked package file lists | Pass |
| randomized current-commit run | Native target passed; codec target stopped at `SAFE-01`; later targets not run |
| successful test-session cleanup | Verified empty |

## Performance Smoke Comparison

The first compilation run and one run with an observed Cargo cache lock were
excluded. The table is the median of five remaining release executions with
10,000 records on this machine.

| Operation | `4e07a3f` baseline | `0d4b9c6` median | Change |
| --- | ---: | ---: | ---: |
| fixed encode/decode | 1.463 ms | 1.473 ms | +0.7% |
| variable encode/decode | 10.867 ms | 10.805 ms | -0.6% |
| fixed append | 341.470 ms | 353.094 ms | +3.4% |
| fixed open/scan | 41.576 ms | 40.383 ms | -2.9% |
| merge keyed files | 536.063 ms | 532.316 ms | -0.7% |
| compact merged | 507.728 ms | 550.337 ms | +8.4% |
| compact base+deltas | 604.028 ms | 600.792 ms | -0.5% |

All medians remain inside the documented manual 15 percent threshold. Tail
latency was noisy, especially for compact operations, and the benchmark is too
small to establish PB behavior or compare Varve with a commercial storage
engine. `BENCH-01` also means merge/compact throughput labels should not be
treated as absolute operation rates.

## Artifact Hygiene

- The all-feature suite and both singleton-feature suites reported successful
  session cleanup.
- The main-reviewer temporary downstream API crate was removed.
- Round 2 reviewers reported removal of their disposable fixtures.
- Build caches under `target` are retained by documented policy to avoid
  needless recompilation and SSD writes.
- The failed bounded codec run intentionally retains one 11-byte diagnostic
  file. Failed-test evidence was not deleted.
- Pre-existing unrelated untracked workspace paths were not modified.

## Required Before Release

### P0

1. Correct HashMap capacity accounting and add a deterministic low-budget
   regression.
2. Validate endian in every authoritative manual block contract.
3. Include static slice lengths in block-contract cache identity.
4. Replace logical-page matrix open loops with allocated-page/index-driven
   traversal and remove the dense fallback assumption.
5. Restore `ChunkedBytes` field compatibility with stable schema identities.
6. Resolve `PackedBitmap` identity and fixed-width matrix semantics.

### P1

1. Evict or refund individual sparse pages when their final bit clears.
2. Correct resident merge time and peak-memory estimates.
3. Remove internal rewrite-temp marker files after the temp lock is dropped.
4. Decide and document initial create/sync pathname durability.
5. Test singleton feature combinations and the pinned MSRV in CI.
6. Add package verification and ship discoverable crate/proc-macro rustdoc.
7. Rerun the complete bounded randomized and sanitizer matrix after P0 fixes.

### P2

1. Replace schema-width linear searches where very wide formats are supported.
2. Correct benchmark denominators and automate the stated regression gate.
3. Default the local completion runner to locked dependency resolution.
4. Pin CI actions and installed audit tooling to immutable identities.

## Final Assessment

Opus fixed the previously dominant native/scalable-path problems, and ordinary
generated append/read paths pass broad tests with no material median regression
in the current small benchmark. The new matrix representation also fixes the
old per-bit whole-bitmap hashing cost.

The replacement is incomplete at the PB boundary because open still enumerates
logical pages and can fall back to full logical bitmap reads. More importantly
for general use, the current commit has one confirmed memory-budget failure and
four type/API contract regressions. Varve is suitable for continued controlled
development and targeted evaluation, but this commit should not be published
as the stable general-purpose release until the P0 list is fixed and the full
validation matrix passes again.
