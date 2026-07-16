# Varve Adversarial Security Review

Status: Published review; remediation implemented and mechanically verified.
Review date: 2026-07-10. Target: the pre-0.2 security-hardening implementation.
Disposition: findings remediated for 0.2.0; retained as public security
rationale and release evidence.

## Executive Summary

This review treats the format declaration, generated Rust code, and custom codec
implementations as trusted. File bytes, matrix metadata, sidecars, file paths,
and concurrent external filesystem activity are untrusted.

Three high-impact behaviors were dynamically reproduced through public APIs:

1. A lazy collection opened from file A reads file B after the pathname is
   replaced, even with `crc32_with_header` enabled.
2. `replace_fixed` changes the value later observed by an already-open reader,
   while that reader still owns the old index and checksum snapshot.
3. Duplicate record sequences are interpreted differently by keyed collection
   lookup and merge materialization.

The review also confirmed unbounded allocation paths during CRC-protected open,
matrix open, generated typed reads, sidecar reads, and lock-file parsing. A
structurally valid file can therefore terminate a process through memory or CPU
exhaustion. No safe-API memory corruption or direct code-execution path was
identified in this review. That is not a proof of absence. A 2026-07-11
follow-up added arbitrary-byte fuzzing, ASan, strict Miri checks, and real
Windows replacement fault injection; finite campaigns still cannot prove
absence of defects.

## Remediation Result

The remediation preserved existing valid wire bytes and schema hashes. The
following table records the integrated result; the original findings remain
below as the evidence and threat rationale.

| Finding | Result | Primary regression evidence |
| --- | --- | --- |
| ADV-001 mutable-path lazy reads | Remediated | `native_reader_and_lazy_collection_stay_on_original_path_generation`, `layout_reader_payloads_stay_bound_to_the_open_object` |
| ADV-002 safe in-place fixed replacement | Remediated | safe `replace_fixed`/`FixedCopyOnWrite`; unsafe `replace_fixed_in_place_exclusive`; old/new snapshot tests |
| ADV-003 whole-payload CRC allocation | Remediated | bounded 64 KiB streaming CRC and CRC+COW/footer regressions |
| ADV-004 matrix-derived bitmap allocation | Remediated | `matrix_limits` covers dimension, aggregate cells, bitmap, CRC, metadata, slot, aux, quarantine, and sidecar budgets |
| ADV-005 duplicate sequence ambiguity | Remediated | duplicate sequences rejected; conflict order is shard, sequence, physical ordinal |
| ADV-006 generated limit gap | Remediated, policy revised | optional runtime `ReadLimits`, generated `*_with_resource_limits`, tightening and trusted boundaries, trybuild coverage |
| ADV-007 whole sidecar/lock reads | Remediated | metadata-first length checks, bounded lock `take`, strict sidecar read plan |
| ADV-008 32-bit field-length truncation | Remediated | checked `u64 -> usize` before field access; oversized-field regression |
| ADV-009 non-canonical accepted bytes | Remediated | zero flags, duplicate fields, ordered map keys, exact internal envelopes, strict native/matrix reserved fields |
| ADV-010 hostile aggregate work | Caller-controlled | append totals are intentionally uncapped by default; services can set runtime record, segment, scan, and index budgets while one-shot materialization remains bounded |

Verification completed with workspace all-feature tests, strict Clippy, Cargo
audit/deny, npTDMS/Pillow compatibility harnesses, performance comparison,
ASan fuzzing, Miri, and Windows replacement fault injection.
Exact commands, measurements, and residual assurance boundaries are recorded in
`security-remediation-validation.md` and `fuzzing-and-fault-injection.md`.
For hostile-input services, aggregate work quotas are a call-time deployment
choice rather than a format-schema requirement. Standard one-shot allocation
limits remain active; custom codecs and explicitly unsafe mmap/raw or exclusive
in-place APIs retain their documented caller obligations.

## Threat Model

### In scope

- An attacker knows the exact format declaration and wire layout.
- The attacker can supply a complete file or sidecar to a service.
- In the stronger local/shared-directory scenario, the attacker can rename,
  replace, truncate, or mutate the pathname after the service opens it.
- The application may run with permissions the attacker does not have.
- Availability, snapshot integrity, parser differentials, replay, and unsafe
  boundary misuse are security-relevant outcomes.

### Trusted or caller-owned

- The Rust format declaration and macro expansion.
- Custom `VarveEncode`/`VarveDecode` implementations.
- Unsafe raw-layout marker implementations.
- The numeric values selected for domain limits.

Varve is still responsible for providing places where those limits can be
enforced before allocation, especially on generated and open-time paths.

## Severity Model

| Severity | Meaning |
| --- | --- |
| Critical | Safe-API memory corruption, direct code execution, or equivalent impact without a strong filesystem precondition. |
| High | Practical process termination, snapshot/integrity bypass, privileged path read, or conflicting public interpretations. |
| Medium | Conditional denial of service, cross-platform parser differential, or non-canonical input with meaningful downstream risk. |
| Low | Hardening gap whose direct security effect requires additional application mistakes. |

## Findings

### ADV-001: Lazy reads are rebound to a mutable pathname

Severity: High. Evidence: dynamically reproduced.
Affected surfaces: native typed collections, metadata, migration, merge, and
custom-layout range reads

`BlockVec` stores a `PathBuf` and reopens it when `get` is called
(`crates/varve-core/src/collections.rs:11-14,50-65`).
`RecordIndexEntry::read_payload` performs a fresh `File::open(path)`
(`crates/varve-core/src/file.rs:212-214`). `LayoutReader` follows the same model:
it stores a path, then `read_metadata`, `read_raw`, and range methods reopen that
path (`crates/varve-core/src/layout.rs:215-220,673-686,716-759,2130-2135`). Merge
opens a file, discards the handle relationship, and passes its path back into
per-record reads (`crates/varve-core/src/file.rs:3548-3560`).

The index, checksum, and schema validation therefore belong to the originally
opened object, while payload bytes can belong to a later object at the same
pathname. The reproduction created two valid files with identical structure,
opened the first, replaced its path with the second, and then called `get`. The
second value was returned under `crc32_with_header`; the captured checksum was
not revalidated.

Impact:

- snapshot and validation bypass;
- semantic substitution after a successful open;
- possible privileged local-file disclosure when a less-privileged attacker can
  replace an upload path with a link and the application exposes raw/layout
  ranges;
- merge or rewrite can consume bytes from an object that was never indexed.

Required correction:

- Bind lazy reads to a clone of the already-open file object and use positional
  reads against that object.
- Carry the snapshot handle through `BlockVec`, `KeyedBlockVec`, layout readers,
  migration, diagnostics, merge, and compact paths.
- Keep path-based `RecordIndexEntry` methods only as explicitly non-snapshot
  low-level APIs, if compatibility requires them.
- Add pathname-replacement and symlink-swap regression tests. Comparing file
  identity before each read is weaker than retaining the handle and still races.

### ADV-002: Safe append-log readers are not immutable across `replace_fixed`

Severity: High. Evidence: dynamically reproduced.
Affected surfaces: fixed block replacement and every lazy owned reader

`replace_fixed` overwrites the existing record header and payload in place
(`crates/varve-core/src/file.rs:1677-1745`). An already-open reader retains its
old index entry, sequence, and checksum, but its later `BlockVec::get` reads the
new payload and does not compare it with the captured checksum. The reproduction
opened a CRC-with-header reader, performed a same-size replacement from a writer,
and observed the replacement through the old reader.

During a concurrent write, a reader can also observe torn fixed-field bytes.
Canonical numeric payloads often decode successfully for arbitrary byte
combinations, so decode success does not establish snapshot consistency.

This contradicts the documented append-log snapshot-on-open model. Matrix slot
writes already document this limitation; fixed append-log replacement does not.

Required correction:

- Decide whether immutable snapshots or in-place replacement has precedence.
- For immutable snapshots, implement replacement as versioned append or atomic
  file generation replacement and bind old readers to their original handles.
- If in-place replacement remains, add a read lease/exclusion contract and state
  clearly that it is not snapshot-safe.
- At minimum, revalidate the captured header/payload checksum on lazy reads and
  fail closed on mutation. This detects change but cannot return the old value.

### ADV-003: CRC-protected open allocates the complete claimed payload

Severity: High. Evidence: code-path confirmed.
Affected surfaces: native `open`, `open_readonly`, and recovery with CRC enabled

During index scanning, CRC policy causes `read_record_entry_at` to load the
entire payload (`crates/varve-core/src/file.rs:3875-3904`). The allocation occurs
as `vec![0; payload_len]` in `read_payload_file_validated`
(`crates/varve-core/src/file.rs:4273-4280`). File extent validation prevents
out-of-bounds reads, but it does not make the allocation affordable.

An attacker can provide a structurally valid large record or a sparse file whose
logical extent satisfies the length check. Rust's normal allocation failure can
abort the process. CRC calculation itself can be streamed and does not require
payload-sized memory.

Required correction:

- Stream CRC through a bounded buffer.
- Add open-time limits for physical payload length, logical file length, record
  count, and total scan work.
- Apply the limit before allocation and use fallible reservation where a full
  owned buffer is genuinely required.

### ADV-004: Runtime matrix dimensions can force multiple unbounded bitmaps

Severity: High. Evidence: code-path confirmed.
Affected surfaces: matrix reader/writer open

Matrix open validates arithmetic and verifies that declared regions fit within
the file (`crates/varve-core/src/matrix.rs:514-580,2395-2445`). It then reads the
complete commit map and CRC-valid maps. `layout_from_parts` copies commit-map
slices and creates up to three cell-count-sized bitmaps per block
(`crates/varve-core/src/matrix.rs:1593-1602,1636-1645`). `read_range` performs an
infallible-sized `Vec` allocation after only a `usize` conversion
(`crates/varve-core/src/matrix.rs:2448-2453`).

Runtime dimension values have no declaration-level or open-call maximum. A
logically large sparse matrix can therefore cause multi-gigabyte allocation
before the caller can inspect dimensions.

Required correction:

- Add format/caller policies for maximum dimension values, cell count, slot
  region size, commit-map bytes, CRC-table bytes, and aggregate matrix metadata.
- Validate these limits immediately after decoding the small dimension table and
  before reading any derived region.
- Avoid duplicate bitmap ownership where a view or compact state representation
  is sufficient.

### ADV-005: Duplicate sequences produce conflicting public interpretations

Severity: High. Evidence: dynamically reproduced.
Affected surfaces: keyed lookup, tombstones, merge/materialization, compaction

Open accepts duplicate and non-monotonic record sequences. `keyed_blocks` replaces
an existing value only when the new sequence is strictly greater
(`crates/varve-core/src/file.rs:1979-2013`). Merge materialization uses
`MergeOrder { shard_ordinal, sequence, record_ordinal }` and accepts a later
record when that full order is greater or equal
(`crates/varve-core/src/file.rs:3563-3645`).

The reproduction changed the second of two same-key records to the first
record's sequence. `keyed_blocks().get()` returned the first value, while
`materialized_keyed_blocks()` returned the second. A block followed by an
equal-sequence tombstone similarly remains visible in one API and deleted in the
other.

Parser differentials are dangerous when one code path performs validation or
authorization and another persists, merges, or serves the result.

Required correction:

- Use one shared conflict-order comparator in every keyed API.
- Prefer rejecting duplicate/non-canonical sequences in native files produced
  under Varve's monotonic writer contract.
- Add block/block, block/tombstone, and op ties to adversarial tests.

### ADV-006: Generated typed APIs cannot apply the available payload limits

Severity: Medium. Evidence: code-path confirmed.
Affected surfaces: `BlockVec`, `KeyedBlockVec`, migration, metadata, merge, and
compressed variable blocks

`RecordIndexEntry` exposes physical and logical limited read methods, but generated
typed reads always call unbounded `read_logical_payload`. There is no
`BlockVec::get_limited`, reader policy, or declaration-level resource policy.
Keyed collection construction also decodes every matching block to derive keys,
then decodes the selected block again on `get`.

Compression has a format-selected `max_uncompressed_len`, which is useful, but a
large selected maximum remains a decompression-memory/CPU target and physical
payload size is still unbounded on generated paths.

Required correction:

- Introduce a `ReadLimits`/`OpenLimits` policy carried by generated readers.
- Add declaration syntax for domain-selected maxima without imposing one global
  value on all formats.
- Ensure generated collection, migration, merge, metadata, and diagnostic paths
  use those limits by default.

### ADV-007: Sidecar and lock files are read wholly before a size decision

Severity: Medium. Evidence: code-path confirmed.
Affected surfaces: matrix sidecars and writer acquisition

Matrix sidecar parsing starts with `std::fs::read`
(`crates/varve-core/src/file.rs:4487-4494`). Writer-lock parsing starts with
`std::fs::read_to_string`, including under the default refuse policy
(`crates/varve-core/src/file.rs:4764-4774`). An attacker who can place either
file can turn a clean rejection into large allocation and I/O.

Required correction:

- Bound lock metadata to a small constant and parse it through `Read::take`.
- Under `WriterLockBreakPolicy::Refuse`, return `WriterLockHeld` without reading
  attacker-controlled lock contents unless diagnostics were explicitly requested.
- Add a caller/format sidecar payload limit and validate metadata length before
  reading the body.

### ADV-008: Variable field length truncates on 32-bit targets

Severity: Medium. Evidence: code-path confirmed; 32-bit execution was not
available in this review.
Affected surface: macro-generated variable block decoder

Generated code converts the wire `u64` field payload length with
`header.payload_len as usize` (`crates/varve-macros/src/lib.rs:290-295`). On a
32-bit target, lengths above `u32::MAX` wrap. A file rejected as truncated on a
64-bit reader can consume a different number of bytes, or even zero bytes, on a
32-bit reader.

Required correction:

- Replace the cast with `usize::try_from` and return `LengthOverflow`.
- Add compile/run coverage for at least one 32-bit target or a unit-tested helper
  whose conversion behavior is architecture-independent.

### ADV-009: Non-canonical inputs remain accepted in several owned decoders

Severity: Low to Medium. Evidence: code-path confirmed.
Affected surfaces: variable fields, internal tombstone/op payloads, maps, and
reserved flags

- Repeated known variable field ids overwrite the previous decoded value rather
  than being rejected (`crates/varve-macros/src/lib.rs:228-299`).
- Variable field flags are read and ignored (`crates/varve-core/src/codec.rs:227-235`).
- Internal key and op payloads do not require the parsed value to end at the end
  of the payload (`crates/varve-core/src/file.rs:3660-3685,3703-3740`).
- The native file-header flags field is specified as literal zero but its decoded
  value is ignored (`crates/varve-core/src/native_layout.rs:248-251,689-693`).
- Map decoding rejects duplicates but does not require canonical sorted input
  (`crates/varve-core/src/codec.rs:502-568`).

These permit multiple byte strings for one logical value and create differential
risk with another implementation that rejects duplicates, uses first-value-wins,
or enforces reserved flags. The impact is highest when an application signs,
hashes, caches, or authorizes one representation and later re-encodes another.

Required correction:

- Decide and document strict-versus-permissive decode policy per layer.
- Reject duplicate known fields, nonzero unknown flags, and trailing internal
  bytes in strict native mode.
- If permissive interoperability is required, expose it as an explicit policy and
  canonicalize before security-sensitive hashing or signing.

### ADV-010: Record and segment counts have no aggregate budget

Severity: Medium. Evidence: code-path confirmed.
Affected surfaces: native scan, checkpoint scan, custom layout scan, and mmap
index construction

Native open stores one `RecordIndexEntry` for every record
(`crates/varve-core/src/file.rs:4209-4258`). Custom layout open stores one
`LayoutSegmentInfo` for every segment
(`crates/varve-core/src/layout.rs:1293-1332`). Mmap payload setup additionally
copies the index into a vector, hash set, and per-block vectors
(`crates/varve-core/src/file.rs:2412-2425`).

Many minimum-size records or segments cause predictable memory and CPU growth.
This is proportional to input size rather than a compact-file amplification, but
it can still terminate a service that accepts arbitrarily large files.

Required correction:

- Include maximum records, segments, index bytes, and scan work in `OpenLimits`.
- Provide a streaming scan mode for applications that do not need a full random
  access index.

## Security Boundaries, Not Defects

### CRC and schema hash are not authentication

CRC32, the FNV-derived schema hash, commit markers, and sidecar CRCs detect
accidental corruption and torn writes. They do not establish who authored a
file. An attacker who knows the format can create arbitrary semantically valid
records and recompute every public checksum. Sidecar generation checks can detect
an accidental stale sidecar but do not prevent an attacker from forging or
replaying one.

Applications that require authenticity, anti-replay, or trusted provenance need
an external signature/MAC envelope and a trusted generation/nonce source. Every
decoded domain value must remain attacker-controlled until the application
validates it.

### Mmap and raw zero-copy require external immutability

The current mmap constructors are correctly marked `unsafe` and document that
all handles, threads, and processes must be prevented from mutating or truncating
the backing object. If an application maps an attacker-writable object, it has
violated that precondition; safe accessors inside the returned owner cannot repair
the external mutation race. Owned reads or an immutable copied object are required
for untrusted storage.

Mmap length and index count still need resource limits even when immutability is
properly guaranteed.

### Recovery and sidecar locks are operational policy

`open_recover` is explicitly destructive under truncate-tail policy. Automatically
running it on untrusted input lets malformed tails trigger truncation; inspect and
authorize recovery before applying it to valuable data.

The sidecar writer lock coordinates cooperating writers. It cannot defend against
a same-privilege process that can delete or replace files in the directory. File
and directory permissions remain the enforcement boundary.

## Dynamic Evidence

A temporary integration test was added, executed, and removed. It was not retained
in the worktree. The command was:

```text
cargo test -p varve --test security_probe_tmp --features integrity -- --nocapture
```

All three tests passed. In this probe, pass means the vulnerable behavior was
successfully reproduced:

- `lazy_collection_follows_replaced_path_after_open`
- `fixed_replace_changes_already_open_reader_value`
- `duplicate_sequence_has_two_public_interpretations`

The worktree was clean again after removing the probe.

## Prioritized Remediation

### P0 before public security claims

1. Bind every snapshot/lazy read to the originally opened file object.
2. Resolve `replace_fixed` versus immutable reader semantics and add concurrency
   tests.
3. Define one canonical conflict order and reject or consistently handle duplicate
   sequences.
4. Add open-time resource policies and stream CRC calculation.
5. Apply matrix dimension/bitmap limits before derived allocations.

### P1 API hardening

1. Carry read limits through generated reader and collection APIs.
2. Bound sidecar and lock-file reads.
3. Replace the generated `u64 as usize` field-length cast.
4. Select and enforce a strict canonical decode policy.
5. Add record/segment/index aggregate budgets and a streaming scan option.

### P2 assurance infrastructure

1. Completed: `cargo-fuzz` targets cover native scan/read/recovery, codecs,
   custom layout lead-ins/regions, matrix data, and sidecars under ASan.
2. Completed in part: deterministic valid seeds and a wire dictionary are
   retained; future regressions must be promoted into deterministic tests and
   seed generation.
3. Completed in part: strict Miri checks and Windows replacement/truncation
   stress tests pass. 32-bit CI remains outstanding.
4. Outstanding: test OOM-adjacent limits with sparse files without requesting
   dangerous real allocations.

## Release Recommendation

Do not describe current readers as immutable snapshot readers until ADV-001 and
ADV-002 are resolved or the API contract is narrowed. Do not advertise hostile
input robustness until ADV-003, ADV-004, ADV-006, and ADV-010 have enforceable
budgets on generated and open-time paths. ADV-005 should be fixed before two
different state APIs are used in security-sensitive application logic.
