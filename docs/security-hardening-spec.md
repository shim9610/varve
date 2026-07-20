# Security And API Hardening Spec

> **Scope.** This document is the record of one completed hardening pass, fixed
> at the baseline commit named below. Every format version it states — including
> `VMAT` v1 — is the version that was current *at that baseline*, not the version
> this build writes. `docs/spec.md` is the authority for the shipped layout.

## Status

- Baseline commit: `f7a9369` from `dev/main`.
- Working branch: `codex/security-api-hardening`.
- This work is development-only. It must not be merged or rebased with the
  unrelated `origin/main` history, and it must not be published until the
  final verification gates pass.

## Objective

Produce a Rust 1.95-compatible release candidate with:

- no ignored or unresolved RustSec findings;
- current compatible direct and locked dependencies;
- checked hostile-input arithmetic and extent validation;
- secure temporary adapter inputs;
- an honest file-backed mmap safety boundary;
- contained append and streamed-layout write failures;
- a deliberate pre-public API evolution policy; and
- unchanged valid wire bytes, schema hashes, durability semantics, and hot-path
  complexity.

## Non-Goals

- No wire-version, schema-hash, manifest, checkpoint, compression-envelope, or
  VMAT redesign.
- No live tailing, async I/O, multi-writer support, or cryptographic
  authentication.
- No TDMS-specific behavior.
- No blanket string, map, sequence, payload, or matrix limit. Domain ceilings
  remain format-author policy; Varve provides structural validation and
  caller-selected limits where they are meaningful.

## Threat Model

Treat file headers, records, checkpoints, manifests, compression envelopes,
custom-layout offsets, VMAT metadata, sidecars, and adapter input bytes as
untrusted. Defend against overflow, panic, claim-driven allocation before
bounds validation, path traversal, temp-file collision/truncation, ambiguous
canonical values, partial append tails, and unsafe file-backed mappings.

Compile-time descriptors, custom codecs, unsafe marker implementations, and
domain resource ceilings remain caller-owned contracts.

## Frozen Decisions

### Dependencies

The direct target versions are:

| Dependency | Target |
| --- | --- |
| `crc32fast` | `1.5.0` |
| `memmap2` | `0.9.11` |
| `proc-macro2` | `1.0.106` |
| `proptest` | `1.11.0` |
| `quote` | `1.0.46` |
| `syn` | `2.0.118` |
| `thiserror` | `2.0.18` |
| `trybuild` | `1.0.117` |
| `zerocopy` | `0.8.54` stable series |
| `zstd` | `0.13.3` |
| `windows-sys` | `0.61.2` |
| `tempfile` | `3.27.0` |

`tempfile` is accepted as a normal dependency for exclusive, restrictive,
random temporary adapter files. If its MSRV, license, or deny checks fail, the
change stops rather than falling back to truncating or predictable temp paths.
No advisory ignore is permitted.

### Error Contract

Before crates.io publication, `Error` becomes `#[non_exhaustive]`. The
coordinated additions are:

- `InvalidCanonicalEncoding(&'static str)`;
- `LimitExceeded { resource, actual, limit }`;
- `SequenceExhausted`;
- `WriterPoisoned(&'static str)`;
- `WriteRollbackFailed { operation, source }`;
- `InvalidAdapterExtension(String)`;
- `MatrixCommitQuarantined(String)`.

This one-time transition and the mmap `unsafe` qualifiers are the only
allowlisted source breaks against `f7a9369`.

### Sequence State

The private state is logically either `Available(next)` or `Exhausted`.

- A new or reopened empty append log starts at sequence `0`.
- If the maximum observed sequence is below `u64::MAX`, the next value is
  `max + 1`.
- If the maximum observed sequence is `u64::MAX`, readers and writers may open,
  but append operations return `SequenceExhausted` before any mutation.
- `u64::MAX` may be appended once when it is the available next value. The
  state becomes exhausted only after the complete record is written and
  published to the in-memory index.
- A failed append that is fully rolled back does not consume its sequence.
- Matrix-only writes remain available when the append sequence is exhausted.

### Payload And Arithmetic Bounds

- Decode offsets and lengths with checked conversion and checked arithmetic.
- Compare the complete range with metadata from the same open file handle
  before allocation.
- Apply a caller limit before allocation.
- A later truncation race is a normal short-read error.
- Keep `read_payload` for compatibility, but make it extent-safe.
- Add `read_payload_limited` and a logical-payload limited counterpart.
- Add a checked physical-end API and use it internally. Keep the existing
  saturating convenience method for compatibility.

### Canonical Codec Rules

- Boolean bytes are exactly `0` or `1`.
- Decoded map keys must be unique under the destination map's normal key
  identity (`Ord` for `BTreeMap`, `Eq`/`Hash` for `HashMap`).
- `HashMap` encoding sorts borrowed entries by the existing `Ord` key order,
  removing the `Clone` bound and second lookup without changing valid bytes.
- Detecting non-injective user codec implementations by separately encoding
  every key is out of scope; custom codec injectivity is a format-author
  contract.

### Temporary Adapter Inputs

`AdapterInputFile::from_bytes` uses an exclusively created `tempfile` guard.
Clones share the guard, and the path is removed only after the last clone is
dropped.

The extension accepts one optional leading dot followed by 1-32 ASCII bytes.
The normalized body must start and end with an ASCII alphanumeric character;
internal characters may be ASCII alphanumeric, `.`, `-`, or `_`; consecutive
dots are rejected. Separators, colon/ADS syntax, whitespace, `.` and `..` are
rejected.

### Mmap And Zero-Copy

Every public route that constructs a file-backed mapping is `unsafe`. The
caller must prevent mutation, truncation, replacement, or backing-object
invalidation through every handle, thread, and process for the mapping's full
lifetime.

Mappings clone the already-open file handle rather than reopening the path,
which closes path-substitution races but does not remove the unsafe lifetime
contract. Index/layout ranges are checked against the mapped length. Ordinary
owned reads remain safe, and raw fixed/matrix references retain their existing
additional unsafe representation contracts.

### Write Failure Containment

Native append and custom streamed-layout operations capture the original EOF,
cursor, sequence state, index length, and segment count as applicable.

- Before the complete record/segment is indexed, a returned write error
  triggers `set_len(original_eof)` and cursor restoration.
- Successful rollback returns the original operation error and keeps the
  writer reusable.
- Rollback failure supersedes the original error, returns
  `WriteRollbackFailed`, and permanently poisons the writer.
- A poisoned writer rejects later mutation, flush, and sync with
  `WriterPoisoned`.
- Rollback is process-level logical recovery, not crash-durable rollback, and
  performs no implicit fsync.
- Stream callbacks receive only `dyn Write`, so they cannot seek into existing
  bytes. Panic rollback is not promised in this pass; unwinding through a
  callback makes the handle unusable and must be documented.
- In-place replacement and matrix commit paths are audited and tested without
  broad refactoring unless a concrete defect is reproduced.

## Compatibility Contract

Valid `VARVE1`, `VARVE2`, `VARVE3`, VMAT v1, manifest v1-v5, checkpoint v1-v3,
compression envelopes, custom-layout bytes, and computed schema hashes remain
unchanged. Existing `flush`, `sync`, `commit`, `commit_durable`, and ordered
matrix barrier behavior remains unchanged. Strict boolean and duplicate-map
rejection applies only to bytes that canonical writers could not produce.

## Acceptance Gates

- `cargo fmt --all -- --check`
- `cargo test --workspace --locked`
- `cargo test --workspace --locked --all-features`
- individual `integrity`, `compression-zstd`, `mmap`, and `zero-copy` feature
  tests
- `cargo clippy --workspace --all-targets --all-features --locked -- -D warnings`
- rustdoc with `RUSTDOCFLAGS=-D warnings`
- `cargo audit --deny warnings`
- `cargo deny check`
- `cargo semver-checks --workspace --baseline-rev f7a9369`, with only the two
  allowlisted source breaks
- preserved wire/schema assertions
- hostile-input, rollback, sequence, temp lifetime, and unsafe-call compile
  tests
- release performance comparison with the frozen method in
  `security-hardening-validation.md`
