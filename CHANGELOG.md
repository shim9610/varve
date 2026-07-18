# Changelog

All notable repository releases are documented here. Varve follows semantic
versioning; while the crates remain below 1.0, incompatible Rust API changes
increment the minor version.

## Unreleased

### Added

- Experimental `high-cardinality-dev` stream and disk-index handle family with
  required `.vks`/`.vki` checkpoints, generated typed batch APIs, explicit
  restore/bootstrap/rebuild operations, and indexed point lookup plus lazy
  sequential iteration on one reader.
- Checked file-offset, length, snapshot-bound, and record-pointer types for
  validating sidecar-derived extents before native I/O or allocation.
- Synchronous progress and cooperative cancellation for explicit scalable
  verification, stream bootstrap, and disk-index rebuild scans.
- O(1) explicit stale-writer-lock clearing that does not open or scan the data
  file, with process-aware policies on Windows and Unix.
- Reproducible scalable persistence-boundary, sparse-offset, and redb sidecar
  robustness harnesses.
- Workspace `varve-test-runner` with atomic fresh-session creation, failure
  retention, success cleanup verification, and collision/refusal tests.
- `ReadLimits::UNTRUSTED` / `ReadLimits::untrusted()`, a finite companion to
  `STANDARD` (16 GiB file, 16M records, 16 GiB scan, 1 GiB index, 65,536
  segments) for resident opens of input from untrusted sources.
- Exclusive-create constructors `VarveFile::create_new` and
  `VarveWriter::create_new` that never truncate or reuse an existing path.
- `Error::MatrixFatalCorruption` and `FormatSpec::with_matrix_fatal_forensics()`
  for the matrix fatal-finding fail-closed default and explicit forensic opt-in.
- `historical_distinct_keys()` (`K-ever`) sidecar capacity metric on
  `DiskIndexStore`, `DiskIndexSnapshot`, `VarveIndexedReader`, and
  `VarveIndexedWriter`.
- Required `VarveBlock::SCHEMA_FINGERPRINT` with a `#[derive(VarveBlock)]`-computed
  value, `KeyedBlockContract`, and `Error::BlockSchemaFingerprintMismatch` /
  `Error::BlockKeyednessMismatch` for typed registration.
- `Error::PublishedButParentSyncPending` typed post-publication replacement state
  distinct from a rollback.
- GitHub Actions CI (Ubuntu + Windows) running fmt, Clippy, all-feature tests,
  and a non-blocking `cargo deny`/`cargo audit` job.

### Changed

- The authoritative single-writer guard is now an OS lock on the open native file
  object (Windows `LockFileEx` on a reserved non-data byte, Unix advisory lock),
  bound at create and re-bound to each published generation. A hard-link or
  reparse alias can no longer open a second concurrent writer; the `.lock` marker
  is diagnostic/break-policy metadata only.
- Atomic replacement reports post-publication durability explicitly: a
  parent-sync failure after a successful rename rebinds the writer and returns
  `PublishedButParentSyncPending`; a failed rebind poisons the writer with
  `PublishedButRebindFailed`. Any other replacement error means publication did
  not happen.
- Matrix `Fatal` recovery findings fail-close every default read/write/aux/
  resume/rebuild accessor with `Error::MatrixFatalCorruption` via an `O(1)` flag
  precomputed at open; `matrix_recovery_report()` stays readable.
- The matrix sidecar manifest is version 2 (88-byte header) binding a native
  object fingerprint and matrix layout generation, published atomically through a
  same-directory temp with native-then-sidecar ordering and parent sync.
- `CheckpointOnFlush` spaces full index checkpoints geometrically, bounding
  cumulative checkpoint bytes to `O(N)` instead of the former `O(N²)`.
- CRC typed point lookups and streaming scans read each covered payload once and
  no longer checksum or read skipped foreign-block payloads; `verify_all()`
  remains the whole-file integrity pass.
- Tombstone rebuild resolves the descriptor once per record by block id, making
  rebuild cost independent of the plan descriptor count.
- Stream scalar `push_*` calls share one bounded sidecar transaction committed at
  the chunk bound or on `flush()`/`sync()` instead of committing per record;
  `keyed_offset_chain` tail maintenance uses binary search with per-chunk tail
  collapse.
- Indexed handles for one file share a process-local backing database, so
  independent readers (and a reader beside a writer) coexist; contention surfaces
  as a typed `Error::IndexBusy` rather than blocking. Cross-process exclusivity is
  unchanged.

- Clean scalable open no longer scans native records, and append/`sync()` no
  longer rereads newly written native chunks. Bounded batches coalesce native
  writes while disk-key cardinality remains in redb instead of an in-memory
  Varve key map.
- Generated keyedness metadata permits unkeyed batch blocks inside a
  `keyed_offset_chain` format while rejecting chain-unsafe keyed stream calls.
- Scalable restore now validates format tails before native truncation;
  bootstrap/rebuild refuse dirty sidecars, rebuild honors arbitrarily small
  configured transaction bounds, and paged `.vks`/`.vki` length is no longer a
  lifetime append ceiling.
- Disk-index errors retain their typed source, database contention maps to
  `IndexBusy`, bounded encoders stop before over-limit payload/key allocation,
  and atomic replacement syncs the parent directory where supported.
- Under `high-cardinality-dev`, manual `VarveBlock` implementations must declare
  `IS_KEYED`; this closes the low-level keyed-chain bypass that a default
  `false` value would permit.
- Indexed point reads decode and validate the already-read native record, and
  indexed mutations canonicalize each sidecar key once before reusing it for
  previous-key lookup and insertion.
- Writer-lock stale recovery is serialized by an OS-level exclusive guard;
  concurrent recovery cannot remove or supersede a live writer's ownership.
  Windows recovery distinguishes a signaled terminated process object from a
  live process even while another handle temporarily keeps that object open.

### Breaking

- Manual `VarveBlock` implementations must now provide
  `const SCHEMA_FINGERPRINT: u64` (no default). Derive-generated blocks are
  unaffected; a manual block mirroring a generated one must reuse its fingerprint
  constant.
- `FormatSelfTest::run` is non-destructive: it refuses a pre-existing target path
  as a `CallerUsage` failed step instead of truncating it, and `cleanup(true)`
  removes only files the run created.
- Matrix access is fail-closed when recovery records a `Fatal` finding; readers
  that relied on reading through fatal-state files must opt in with
  `FormatSpec::with_matrix_fatal_forensics()`.
- The matrix sidecar format is version 2. Version-1 sidecars are refused as
  `MatrixSidecarMismatch("sidecar version")`; regenerate them (sidecars are
  regenerable resume state, so no native file migration is required).

## 0.3.0 - 2026-07-17

### Added

- Runtime `ResourceLimits` policy APIs that may raise or lower optional format
  defaults for a specific open or create operation.
- Sequence-preserving `replace_block` and generated typed replacement methods
  for native records whose encoded size grows or shrinks.
- Replacement validation for keyed identity, snapshots, offset chains,
  checkpoints, CRC/footer records, transaction visibility, and publication
  failures.

### Changed

- `limits { ... }` is optional and may be partial. It supplies operational
  defaults only and is not a wire-format or schema ceiling.
- Standard append-log totals are uncapped: file length, scan bytes, record
  count, segment count, and index bytes default to `u64::MAX`.
- Finite standard limits remain on one-shot payload decoding, decompression,
  materialization, mmap, sidecar, and matrix allocation surfaces.
- Ordinary native, custom-layout, merge, and compact entrypoints resolve the
  same runtime policy, including low-level reader and writer constructors.
- All owned read results now validate complete extents and finite one-shot
  limits before allocation. Nested codec decoding shares one materialization
  budget, adapter cursors account owned values cumulatively, custom-layout
  headers and payload reads honor it, and matrix CRC/zero scans stream through a
  fixed buffer.
- The TDMS byte-backed example captures and bounds an exact file extent instead
  of reading to a moving EOF.

### Compatibility

- Existing complete `limits` declarations remain accepted, and legacy
  `*_with_limits` methods retain tightening-only behavior.
- Runtime limits are not persisted and do not change existing valid wire bytes
  or schema hashes. No file migration is required from 0.2 solely for this
  release.

## 0.2.0 - 2026-07-11

### Added

- Required declaration-time `ReadLimits` with generated bounded open/read APIs
  and visibly named trusted-unbounded escape hatches.
- Open-object snapshot reads for native and custom layouts, preserving file
  identity and validated logical EOF across pathname replacement.
- Matrix, sidecar, mmap, index, scan, payload, and cumulative materialization
  limits enforced before claim-sized allocation.
- ASan-backed arbitrary-byte fuzz targets for native, codec, custom-layout, and
  matrix-plus-sidecar paths, plus strict Miri and Windows `ReplaceFileW` fault
  tests.
- `PublishedButRebindFailed` for the explicit state where copy-on-write
  publication succeeded but the writer could not bind to the new generation.

### Changed

- `replace_fixed` now uses copy-on-write publication. The former safe
  `ReplaceStrategy::FixedInPlace` variant was removed; the lower-copy operation
  is available only through `unsafe replace_fixed_in_place_exclusive`.
- Canonical decoding now rejects duplicate variable fields, nonzero field
  flags, noncanonical map order, duplicate map keys, and trailing internal
  envelope bytes.
- `HashMap<K, V>` decoding requires `K: Ord` so canonical key order can be
  validated.
- Native recovery distinguishes incomplete tails from fatal corruption and
  resource-limit failures.
- Merge and compact use bounded snapshot reads and shared atomic publication.

### Compatibility

- Existing valid 0.1 native wire bytes and schema hashes are preserved; no file
  migration is required solely for this release.
- Source users must add a complete `limits` declaration or `ReadLimits`, migrate
  fixed replacement calls, and satisfy the canonical map bound where relevant.
  See [Migration Guide](docs/migration-guide.md).
