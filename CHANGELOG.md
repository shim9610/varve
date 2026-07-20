# Changelog

All notable repository releases are documented here. Varve follows semantic
versioning; while the crates remain below 1.0, incompatible Rust API changes
increment the minor version.

## Unreleased

### Added

- Stable codec identities for the two built-in public value types (API-03,
  API-04). `ChunkedBytes` and `PackedBitmap` now declare a non-zero, structurally
  derived `SCHEMA_ID` on both `VarveEncode` and `VarveDecode`
  (`ChunkedBytes` = `0x0b01_19b7_650d_1366`, folded from the `chunked_bytes` tag,
  `CHUNKED_VERSION`, and `Vec<u8>`'s identity; `PackedBitmap` folded the same way
  from the `packed_bitmap` tag and the `u64` + `Vec<u8>` it emits). Both are const
  expressions over compile-time constants, so they are build- and
  platform-stable. **This restores documented public API:** the mandatory
  non-zero field-codec identity contract had made `blob: ChunkedBytes` and
  `mask: PackedBitmap` fail to compile as derived fields. Encoded bytes and
  `WIRE_TYPE` are unchanged.
- `tools/public-api-fixture`: a downstream crate, outside the workspace like
  `tools/rename-fixture`, that derives fixed, variable, and matrix blocks over
  **every** public codec and field type the documentation lists — the scalars,
  `bool`, `String`, `Vec<u8>`, the typed vectors, fixed and nested arrays, 2/3/4
  tuples, `Option`, `BTreeMap`, `HashMap`, `()`, `ChunkedBytes`, and
  `PackedBitmap` — and round-trips each one, plus a real file round-trip through
  a `varve_format!` declaration. It runs as its own CI job (`public-api
  fixture`). This is the gate whose absence let the identity contract silently
  reject public API: nothing in the workspace derived a block over the public
  surface the way a consumer does.
- `KeyedMergeEstimate::peak_resident_bytes()` and the new public field
  `largest_input_open_transient_bytes` (PERF-05): the `8N` sequence-uniqueness
  temporary an input's open can hold alongside its resident index was previously
  omitted from the estimate, understating the peak. `KeyedMergeEstimate` is
  `#[non_exhaustive]`, so the added field is not breaking.
- `VarveFile::block_tail_entries_moved()`, a `#[doc(hidden)]` fault-injection
  counter gated on `scalable-fault-injection`, mirroring
  `block_tail_index_touches`. It counts tuples *displaced* in the tail vector
  rather than index visits, which is the cost the old counter could not see.
- A persisted per-page index region in the matrix layout (PERF-01): one 8-byte
  entry per published bitmap page, stored as `page + 1` so a zero entry
  terminates the array and no count field can tear. It sits between the static
  aux regions and the `MCRC` region, and open builds its visit set from it
  rather than from the logical page count. See Breaking for the layout version.
- CI jobs: `public-api fixture` (above); `test (compression-without-integrity)`
  and `test (integrity-without-compression)`, which run the **test** suite for
  the two singleton feature configurations that own `cfg`-exclusive behaviour
  and previously ran in no test job at all (CI-01); `msrv (1.95.0)`, which pins
  the declared `rust-version` floor and both checks and tests against it, and
  fails if the pin and the manifests disagree (CI-02); and `package contents`,
  which asserts that all three published `.crate` file lists contain
  `README.md`, `LICENSE-MIT`, and `LICENSE-APACHE` (DOC-01).
- Packaged documentation and licenses (DOC-01). All three crates set
  `readme = "../../README.md"` and carry `LICENSE-MIT`/`LICENSE-APACHE` in their
  own directory, so `cargo package --list` now includes them. The `varve` facade
  gained crate-level rustdoc (entry points, a runnable getting-started example,
  the complete Cargo feature table, and the guide index), and `VarveBlock` and
  `varve_format!` gained entry-point rustdoc covering their attributes, the
  generated items, the schema-fingerprint derivation, and the mandatory field
  codec identity rule.
- The API feature table documents the two previously omitted exported features
  and labels them: `high-cardinality-dev` is **experimental** (surface and
  sidecar layout may change without a major version) and
  `scalable-fault-injection` is **test infrastructure**, not a production
  feature.
- Documented resident scale contract and caller-side guards for the keyed
  merge/compact family (PERF-03). `merge_keyed_files`, `compact_keyed_files`,
  and `compact_keyed_file` are resident operations and are explicitly **not**
  PB-scale: time is `Theta(records + decoded bytes) + O(K-live log K-live)` and
  memory is `O(K-ever + largest resident input index + retained live values)`,
  where `K-ever` counts every distinct key ever seen including tombstoned keys.
  Nothing spills to disk and Varve exports no bounded-memory external
  merge/compact. New entry points let a caller predict or fail typed instead of
  exhausting memory: `estimate_keyed_merge::<T, P>(spec, base, deltas) ->
  KeyedMergeEstimate` (decode-free pre-flight; pass an empty delta slice to size
  a single-input compact) and `merge_keyed_files_with_key_limit`,
  `compact_keyed_files_with_key_limit`, `compact_keyed_file_with_key_limit`,
  which refuse a run exceeding `max_distinct_keys` with
  `Error::LimitExceeded { resource: "merge distinct keys", .. }` at the key
  boundary and publish no output. The unbounded entry points delegate with
  `u64::MAX`, so their behaviour is unchanged. `estimate_keyed_merge` itself
  opens each input as a resident file, so it costs `O(largest input index)`.
- `VarveFile::push_keyed` / `push_keyed_info` and `VarveWriter::push_keyed` /
  `push_keyed_info`: maintaining generic keyed append paths that resolve and
  link the keyed offset-chain predecessor. See the corresponding Breaking entry
  for `push` / `push_info`.

- Per-create nonce for stream and disk-indexed primaries (STO-01). Every
  `VarveStreamWriter::create` / `VarveIndexedWriter::create` stamps a 128-bit
  nonce as the primary's first record, under the new reserved
  `CREATION_NONCE_BLOCK_ID`, and folds it into the sidecar's primary-generation
  witness. A sidecar is therefore refused with
  `DiskIndexError::PrimaryGenerationMismatch` when its primary is replaced in
  place by another equal-length primary of the same format, regardless of how
  much leading content the two generations share; `rebuild_disk_index` is the
  recovery. The nonce costs one record at create and nothing per append.
  **Breaking, stale-regenerable:** stream/indexed primaries now carry one extra
  internal leading record, so record counts and sequence numbers shift by one
  relative to 0.3.0, and every existing sidecar's recorded generation witness is
  refused at open until rebuilt. Primaries bootstrapped from a resident
  `VarveFile` carry no nonce and keep the weaker content-window witness only.

- `DiskIndexDescriptor` (exported as `DiskIndexedBlock`) carries
  `schema_fingerprint`, the block schema fingerprint of the concrete type whose
  decode and key-extraction pointers it holds (API-03).
- CI runs a blocking locked fuzz-workspace job: `cargo metadata --locked` before
  any caching or tooling step, then `cargo check --locked --all-targets`,
  `cargo audit --file fuzz/Cargo.lock`, `cargo deny`, and a final
  `git diff --exit-code -- fuzz/Cargo.lock`. The ordering is deliberate — both
  rust-cache and cargo-deny run an unlocked `cargo metadata` that would
  regenerate a stale lockfile and hide the defect the job exists to catch — and
  the trailing diff makes the job fail closed. The clean-archive job now also
  runs `cargo metadata --locked` and `cargo check --locked --all-targets`
  against the archived `fuzz/` workspace (REL-01).
- Windows crash-fault test children suppress interactive error reporting
  (`SetErrorMode`, `SetThreadErrorMode`, and process-level `WerSetFlags`) before
  inducing a fault, so the release gate cannot raise a WerFault dialog on a
  developer machine with default WER settings (TEST-01). On the measured host,
  where interactive WER was already disabled system-wide, suite wall time was
  unchanged within noise; the durable benefit is unattended behaviour, not
  speed.

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
- `ReplacePublicationFailure` and the pure, platform-independent
  `classify_replace_publication_error(raw_os_error)` classifier for Windows
  `ReplaceFileW` failures, plus `Error::ReplacePublicationIndeterminate` for OS
  errors 1176/1177, where the target pathname state is unknown. On that error
  the replacement temp file is preserved for reconciliation and a writer bound
  to the target is poisoned; blind retry is forbidden.
- Exclusive-create matrix constructors `VarveFile::create_new_with_dims` and
  `VarveWriter::create_new_with_dims` that hold the single exclusively created,
  lock-bound handle from claim through matrix initialization (no pathname
  re-open window). The matrix self-test uses this path.
- `FormatSpec::block_identities` (with `with_block_identities` and the builder
  setter): a per-block identity table `(block_id, endian override, keyedness,
  generated codec fingerprint)` that `varve_format!` emits and
  `computed_schema_hash()` folds into the hash.
  `FormatSpec::SCHEMA_HASH_ALGORITHM_VERSION` names the hash algorithm
  revision (now 3). `validate()` rejects duplicate or unregistered identities.
- `VarveEncode::SCHEMA_ID` / `VarveDecode::SCHEMA_ID`: the structural identity
  of a codec's bytes. Built-in scalars declare leaf identities and built-in
  containers fold their elements transitively (an element that declares no
  identity propagates outward as "no identity", so a container cannot launder
  one). Derived blocks set it to their `SCHEMA_FINGERPRINT`, which now folds
  every field's encode and decode identity, so nesting propagates identity.
  Hash algorithm version 2 -> 3 (domain tag `varve-schema-v3`): every computed
  value changes and pre-v3 pinned files fail closed with `SchemaHashMismatch`.
  **Breaking for hand-written codecs:** the derive rejects at compile time any
  field whose codec leaves `SCHEMA_ID` at the default `0`, regardless of wire
  type.
- `MatrixSidecarManifest::matrix_creation_nonce`: the 16-byte per-create nonce
  that binds a sidecar to one logical matrix creation, not just one OS file
  object.
- `Encoder::encode_nested_to_vec`, which caps a child encoder at the parent's
  remaining logical-payload budget; generated variable-block field encoding
  uses it so nested fields cannot stage bytes past the writer's limit.
- GitHub Actions CI (Ubuntu + Windows): rustfmt; a per-feature Clippy matrix
  (`-D warnings`, `--locked`) covering no-default-features, default, each
  optional feature alone (`integrity`, `mmap`, `zero-copy`,
  `compression-zstd`, `high-cardinality-dev`, `scalable-fault-injection`), and
  the all-feature workspace union; default and all-feature test runs through
  `varve-test-runner` so leaked test artifacts fail the build; a **blocking**
  `cargo deny`/`cargo audit` supply-chain job; a renamed-dependency fixture
  build (`vv = { package = "varve", ... }`); and a clean-archive job that
  unpacks `git archive HEAD` and runs `cargo metadata`/`cargo check --locked`
  so an uncommitted workspace member can never pass CI again.

### Changed

- `HashMap` decoding charges the hash table it actually allocates (SAFE-01).
  Decoding used the shared per-entry map model, whose floor of one byte for a
  zero-sized entry let a declared count near `2^30` pass the standard 1 GiB
  materialization budget and then reserve a multi-hundred-megabyte table; a
  bounded randomized run ended at its RSS ceiling on exactly this path.
  `HashMap` now uses its own `preflight_hash_map_count`, which keeps the
  wire-length screen and the "a collection count cannot make bounded input
  progress" rejection and then charges a deliberate over-estimate of hashbrown's
  allocation: `next_power_of_two(ceil(len * 8 / 7)) * (size_of::<T>() + 1)`, plus
  a 16-byte control group and the entry alignment. The `+ 1` per bucket is the
  control byte, which is why a zero-sized entry can no longer be charged one
  byte. All of it is checked arithmetic; an overflowing model returns
  `Error::LimitExceeded` instead of attempting the reservation. Independently,
  `try_reserve` is now given `len.min(1024)`, so a hostile count pre-allocates
  nothing before an entry is proven decodable, while maps of at most 1024
  entries — the common case — reserve exactly as before.
- Typed block registration validates the block's byte order (API-01).
  `BlockContract` carries the `Option<Endian>` override from the authoritative
  `FormatSpec::block_identities` entry, and every typed entry point (push,
  `blocks`, `keyed_blocks`, merge, replace, matrix cell read/write, mmap windows,
  indexed and stream) compares the *resolved* byte order —
  `declared.unwrap_or(spec.endian)` on both sides — before any I/O. A manual type
  that mirrors a generated block's id and fingerprint but declares the opposite
  endian used to register successfully and byte-swap every value it read; it is
  now rejected with `Error::EndianMismatch`. Two declarations that resolve to the
  same byte order are still accepted, because they genuinely produce identical
  bytes.
- The first-use block-contract cache can no longer alias two formats (API-02).
  For any block id the format declares an identity for — which is every block of
  every `varve_format!`-generated spec — the contract is now validated directly
  from the spec's immutable `&'static` data and the process-global cache is
  neither read nor written. That is also strictly cheaper on the per-record
  append path: it removes a global `RwLock` shared-lock acquire and a binary
  search that grew with every format in the process. The cache survives only for
  hand-built specs that declare no identity, and its key gained both slice
  lengths (`blocks` pointer + length, `block_identities` pointer + length, block
  id), so an empty or prefix view of a static array can no longer share an entry
  with the full view. Registration order is now provably irrelevant for
  identity-bearing formats.
- Matrix open is index- and extent-driven, not page-count-driven (PERF-01).
  `load_paged_bitmap` no longer loops `0..page_count`. Its visit set is the union
  of the persisted page index and, where the platform can answer, the pages the
  filesystem allocation map reports as written — enumerated by walking allocated
  *ranges*, not pages. Neither term derives from the logical page count, and an
  unavailable or over-cap allocation map now falls back to the index instead of
  treating every logical page as readable data. The index scan grows
  geometrically from a 64-byte request, so an empty index costs one small read
  regardless of map width. Index entries are written before the page and digest
  they describe, so a torn append can only name a page that still reads as
  uninitialised zeros — a state open already accepts. Recorded trade-off: with no
  allocation map available, a stray byte written out of band into a page the
  matrix never published is no longer detected at open. With an allocation map
  present — the normal case — detection is unchanged.
- Sparse bitmap pages are evicted when their last bit clears (PERF-02). A page is
  now `BitmapPage { bytes, ones }` with a maintained set-bit count, so "is this
  page all zero" is `O(1)` on the mutated page; a page that loses its final set
  bit is dropped immediately and its bytes refunded to the resident bitmap
  budget. Nothing scans the page or the map on the hot path. Long-running sparse
  set/clear activity no longer holds residency proportional to historically
  touched pages.
- `BlockTails::from_index` no longer has a schema-width quadratic term
  (PERF-03). Tail construction records the newest offset per block id in a map
  and sorts the distinct ids exactly once: `O(N + B log B)` time and `O(B)`
  memory, replacing the incremental form's `O(N log B + B^2)` tuple movement.
  The append path deliberately keeps sorted-vector insertion — at most one
  insertion per distinct id for the whole life of a file, `O(log B)` lookups, and
  no hashing on the hot path.
- Keyed merge/compact publish honest cost formulas (PERF-05). The rustdoc for
  `merge_keyed_files`, `compact_keyed_files`, and `merge::compact_keyed_file` now
  states the per-input open's `O(N log N)` sequence-uniqueness sort and the `8N`
  temporary it can hold alongside the resident index. `validate_unique_sequences`
  also gained an allocation-free fast path: the writer hands out sequences
  monotonically, so a file scanned in offset order is proven unique in one pass
  with no `Vec` and no sort, and only a reordered or hostile input pays the copy
  and sort. The estimate still charges the transient unconditionally, because the
  sort is the guaranteed bound — `peak_resident_bytes()` is therefore a true
  upper bound that is loose by `8N` in the common case.
- Keyed compact/merge removes its own internal rewrite-temp lock marker
  (STO-01). `write_keyed_values_atomically` unlinks `<temp>.lock` on every exit,
  after the temp's `VarveFile` (and therefore its `WriterLock`) is dropped and
  while the caller still holds the output's writer lock. This is safe precisely
  because the pathname is generated with `create_new` and the output's writer
  lock excludes any competing rewrite that could regenerate it — none of which is
  true of a user path, whose markers remain persistent stable identities and are
  untouched. If the temp itself is preserved (the `ReplacePublicationIndeterminate`
  case) its marker is preserved with it.
- `sync()` and `commit_durable()` make a *created* pathname durable (DUR-01,
  decision recorded). The atomic replacement path already synced the parent
  directory and reported a pending parent sync, so a create path that did not was
  internally inconsistent with a public contract that promises durable
  persistence. A handle that created its pathname now also fsyncs the parent
  directory on its first successful `sync`/`commit_durable`: one directory fsync
  per created file, never repeated, never on the append path, and never for a
  handle that merely opened an existing pathname. See Breaking for the error a
  refused directory sync produces.
- The test runner defaults to locked dependency resolution (REL-01). The
  documented no-argument invocation and every forwarded command now run Cargo
  with `--locked`, inserted after the subcommand and never after a literal `--`,
  so the local completion gate resolves exactly as CI does and a stale
  `Cargo.lock` fails the command instead of being silently refreshed by it. An
  explicit `--locked`, `--frozen`, or `--offline` from the caller is preserved.
- The benchmark reports each operation against its own denominator (BENCH-01).
  Merge and compact consume the base file plus every delta record (updates,
  deletes, inserts) and emit the surviving live values; all three previously
  reported "records/sec" against the original base record count, which was
  neither. Each line now prints input events per second plus the live values
  emitted, and the emitted count is asserted against the file the run actually
  produced. Codec, append, and open/scan lines are unchanged — `records` was
  already their true denominator.
- CI gates are pinned to immutable identities (CI-03). Every action is
  referenced by commit id with its release in a trailing comment
  (`actions/checkout` v4.2.2, `Swatinem/rust-cache` v2.9.1,
  `EmbarkStudios/cargo-deny-action` v2.1.1, `dtolnay/rust-toolchain` master with
  an explicit `toolchain` input), and `cargo-audit` is installed with an exact
  `--version`, so a future run of the workflow evaluates the same code it
  evaluates today.
- Matrix commit-map integrity is paged (PERF-01). The single per-category commit
  CRC is replaced by an array of 8-byte per-page digests (`crc32` plus a state
  word, one per 4 KiB of bitmap). A commit-bit mutation rehashes only the page
  holding the mutated byte, so writing and committing `M` cells hashes
  `Theta(M)` bitmap bytes instead of `Theta(M^2)`. There is deliberately no
  composition checksum over the digest array: maintaining one per mutation would
  reintroduce a size-dependent hot-path cost. Whole-map verification remains
  "verify every page against its digest", which is what open does.
- Matrix creation and open no longer scale with cell count (PERF-02). Creation
  writes only the descriptor tables plus a 16-byte `MCRC` header; the per-cell
  checksum array, the per-cell validity bitmaps, and the commit bitmaps are
  established as a sparse zero extent. Uninitialized pages are encoded
  explicitly (`state = 0` asserts "never published, must still read as zero"),
  so a page written with zeros is distinguishable from a page never written, and
  a per-cell checksum is trusted only when its persistent validity bit is set.
  Commit, CRC-valid, and current-write bitmaps are held sparsely after open: a
  page is materialized only when it carries a set bit, an absent page is
  provably zero, and the maintained set-bit totals make committed-cell counting
  `O(1)` instead of a full scan. The provably-constant `written` bitmap was
  removed outright. Open is bounded the same way: instead of reading every page
  to prove never-published pages still read as zero, it takes that proof from
  the filesystem's allocated-range map
  (`FSCTL_QUERY_ALLOCATED_RANGES` on Windows, `SEEK_DATA`/`SEEK_HOLE`
  elsewhere) and skips reported holes, so open costs `O(bytes actually
  written)`. Detection strength is unchanged, because writing a stray byte into
  an untouched page allocates that page and brings it back into the read set,
  and a skipped page's digest is still read when the digest slot itself is
  allocated. Where the platform or filesystem cannot answer, or the file is
  fragmented past the tracked extent ceiling, every page is read exactly as
  before. Clearing a whole commit category punches a hole over the map and its
  digests instead of writing zeros, restoring the uninitialized encoding in
  `O(1)` writes.
- The `matrix_bitmap` resource limit now charges the bitmap pages actually
  materialized, checked before each growth, instead of a dense cell-count-scaled
  worst case. It still fails closed, but a large matrix with few committed cells
  is no longer refused on a figure describing a representation that no longer
  exists.
- Generic block registration validates against the format, not against whichever
  type arrived first (API-01). When the `FormatSpec` declares an identity for a
  block id, that immutable `(keyedness, fingerprint)` pair is the authority and
  a disagreeing `T` is rejected with `BlockSchemaFingerprintMismatch` /
  `BlockKeyednessMismatch` before anything is cached or written, so call order
  can no longer decide which wire type a process accepts. The process registry
  is now only a cache of an already-validated result, and its key includes the
  spec's identity table so two specs sharing a descriptor table but declaring
  different identities cannot alias one cached contract. Block ids that a
  hand-built spec declares no identity for keep the documented first-use
  behaviour.
- Disk-index plans validate descriptor schema identity before any decode
  (API-03). Plan construction and plan validation run the registration gate once
  per descriptor, so a descriptor that matches a declared block's id and version
  while being a different type is refused with
  `Error::BlockSchemaFingerprintMismatch` with zero decoder calls. The
  fingerprint is folded into the plan digest, so a sidecar published for one
  block schema is refused as stale for a plan that decodes another.
- Shared sidecar registry access is amortized `O(1)` (PERF-04). A lookup or
  invalidation is one map probe; dead slots are swept only after the map grows
  past a doubling threshold, so opening `S` live identities in sequence costs
  `O(S)` slot checks instead of `Theta(S^2)`. The liveness rule is unchanged.
- Resident block-offset chaining uses maintained sorted block tails (PERF-05).
  Per-append predecessor lookup is `O(log B)` in the number of distinct block
  ids, with no resident-index reads and no allocation once a block id has
  appeared, replacing a reverse scan of the resident index that cost
  `Theta(distance to the previous record of that block)`. The table is rebuilt
  with one forward pass wherever the resident index is loaded or replaced
  wholesale.
- Decoding charges the materialization budget for variable field-id bookkeeping
  (RES-01). Each distinct field id above 63 charges 8 bytes before the tracking
  set reserves, surfacing as
  `Error::LimitExceeded { resource: "variable field ids" }`. Duplicate detection
  still runs first, so a repeated id cannot drain the budget.
- The authoritative single-writer guard is now an OS lock on the open native file
  object (Windows `LockFileEx` on a reserved non-data byte, Unix advisory lock),
  bound at create and re-bound to each published generation. A hard-link or
  reparse alias can no longer open a second concurrent writer; the `.lock` marker
  is diagnostic/break-policy metadata only.
- Atomic replacement reports post-publication durability explicitly: a
  parent-sync failure after a successful rename rebinds the writer and returns
  `PublishedButParentSyncPending`; a failed rebind poisons the writer with
  `PublishedButRebindFailed`. Any other replacement error means publication did
  not happen — with one Windows exception: `ReplacePublicationIndeterminate`
  (`ReplaceFileW` errors 1176/1177) means the pathname state is unknown. The
  Windows path first captures the replacement's OS object identity and, on
  1176/1177, reconciles: if the target already resolves to the replacement
  object the publication is treated as complete; otherwise the typed
  indeterminate error is returned with the temp preserved and the writer
  poisoned.
- Windows parent-directory sync is honest: `FlushFileBuffers` requires
  `GENERIC_WRITE`, so the parent directory is now opened with write access, and
  open/flush refusals (`PermissionDenied`, `InvalidInput`, `Unsupported`) are
  no longer promoted to a `Durable` result. A read-only directory handle fails
  the flush with `ERROR_ACCESS_DENIED` on NTFS (verified live on an NTFS
  host), so the former code silently misreported `Durable` there; publications
  on filesystems that refuse a directory write-open/flush now surface
  `PublishedButParentSyncPending` instead.
- The redb sidecar publication sites (stream/indexed sidecar create, stream
  bootstrap, disk-index rebuild) no longer discard the replacement durability
  state: a parent-sync failure surfaces as `PublishedButParentSyncPending`
  while the already-published sidecar is preserved and usable.
- Every matrix create stamps a fresh 24-byte creation-nonce region (`VMNC`
  magic, version, 128-bit nonce) between the native file header and the matrix
  layout header, cached in the handle at open/create (zero per-operation
  cost). The nonce is folded into the sidecar identity, so recreating a matrix
  into the same pathname/file object with the same dimensions can no longer
  adopt the previous generation's sidecar: stale sidecars are refused as
  `MatrixSidecarMismatch("creation nonce")`, and a caller-supplied generation
  cannot substitute for the native creation identity.
- File creation binds the single-writer object lock before destructive
  initialization: create paths open without truncate, bind the native object
  lock, then `set_len(0)` and write the header, closing the window where a
  losing concurrent creator could truncate the winner's freshly initialized
  file.
- Matrix sidecar reads validate all fixed-header identity fields and the small
  payload magic/category prefix before any payload allocation, read, or hash,
  so an obviously foreign sidecar is rejected without doing
  configured-limit-sized work.
- `needs_index_checkpoint` is O(1): the writer keeps a counter of eligible
  records since the last checkpoint and a precomputed geometric threshold,
  maintained incrementally at the append site, restored on rollback, recovered
  once at open, and recomputed at generation rebind. The former per-flush reverse
  index scan made flush-per-record workloads O(N²) in CPU even after the
  checkpoint byte growth was linearized. The adjacent per-flush
  uncommitted-tail scan is also O(1) now.
- Matrix cell read, write, and mmap access enforce the common typed
  registration gate (`ensure_registered_block`) first, so a manual matrix
  block with the same shape and stride but a different schema fingerprint or
  keyedness is rejected (`BlockSchemaFingerprintMismatch` /
  `BlockKeyednessMismatch`) instead of decoding foreign cells.
- The compile-time keyedness contract (`KeyedBlockContract::<T>::OK`) is now
  evaluated at every public keyed generic entry point — stream delete, indexed
  lookup/get/push/delete, merge/compact, low-level `VarveFile`/reader/writer
  delete, `keyed_blocks`, `key_tail_offsets`, disk-index descriptors, and the
  self-test keyed case — with first-seen runtime registration retained as the
  backstop. A `VarveKeyedBlock` impl declaring `IS_KEYED = false` fails
  compilation at each of these sites.
- All resident writer entry points (push, metadata, replacements, keyed op
  envelopes) encode through the limit-bounded encoder: an oversized value
  fails with the typed `LimitExceeded { resource: "logical payload length" }`
  error and the encoder stops buffering at the limit instead of materializing
  the full encoding first. Matrix cell writes bound the encode by the slot
  stride and keep the exact-size `MatrixSizeMismatch` contract.
- Matrix self-test cleanup is identity-checked: the native target is deleted
  only while the pathname still resolves to the file object this run created
  (race-free delete-by-handle on Windows; check-then-unlink with a documented
  one-syscall residual window on Unix), and the `.lock` marker is removed only
  after re-acquiring it through the standard writer-lock protocol, so
  foreign-owned or populated markers survive.
- `varve_format!`-generated code works when the `varve` dependency is renamed
  in `Cargo.toml` (resolved via `proc-macro-crate`); the
  `extern crate vv as varve` workaround is no longer needed.
- Integration tests own per-test temporary directories (`tempfile::tempdir()`
  guards), so native files, sidecars, and `.lock` markers are collected on
  drop even under plain `cargo test`, on panic, or early return; nothing is
  left in the system temp root. See `docs/test-artifact-hygiene.md`.
- Matrix `Fatal` recovery findings fail-close every default read/write/aux/
  resume/rebuild accessor with `Error::MatrixFatalCorruption` via an `O(1)` flag
  precomputed at open; `matrix_recovery_report()` stays readable.
- The matrix sidecar manifest is version 3 (104-byte fixed header) binding a
  native object fingerprint, matrix layout generation, and the matrix creation
  nonce, published atomically through a same-directory temp with
  native-then-sidecar ordering and parent sync.
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

- **Wire-breaking, stale-regenerable:** the matrix layout is `VMAT` version 3.
  Two header fields, `page_index_off` and `page_index_len`, are appended as
  fields 13 and 14 — after `append_log_start`, so every previously defined field
  keeps its index — and the reserved tail shrinks from 32 to 16 bytes. The region
  order becomes `… | slot region | static aux | page index | MCRC | append log`,
  so `append_log_start` and the total matrix file length change. A version 2
  artifact is refused at open with the typed
  `Error::FormatVersionMismatch { expected: 3, actual: 2 }`, the same
  stale-regenerable contract version 1 already had; recreate it.
- **Breaking (compile):** `PackedBitmap` is no longer accepted as a fixed-width
  matrix field. It owns a `Vec<u8>` and encodes a `bit_len` plus a variable byte
  string, so it has no encoded width fixed by its type and therefore no
  compile-time slot stride; the previous acceptance matched on the source
  *spelling* and took a stride from `size_of::<PackedBitmap>()`, which is the
  size of a `Vec` plus a `u64`, not the width of the bytes it emits. It remains
  fully usable as an ordinary variable field. Relatedly, inline matrix
  `SLOT_STRIDE` is now generated from each element's `VarveEncode::WIRE_TYPE`
  rather than `size_of`, so a *user* type merely spelled `u32` or `PackedBitmap`
  can no longer be laundered through the syntactic pre-filter: anything without a
  fixed encoded width fails const evaluation.
- **Breaking (runtime, intentional tightening):** a `VarveBlock` implementation
  whose `ENDIAN`, resolved through `FormatSpec::endian`, disagrees with the
  format's own declaration for that block id is now rejected at typed
  registration with `Error::EndianMismatch` (API-01). This only affects code that
  declares a byte order contradicting the format's — that is, exactly the silent
  byte swap this catches. Manual mirrors that copy the generated block's `ENDIAN`
  alongside its `SCHEMA_FINGERPRINT` are unaffected, and generated code always
  agrees with itself.
- **Breaking (runtime):** the first `VarveFile::sync()` or `commit_durable()` on
  a handle that *created* its pathname can now return
  `Error::PublishedButParentSyncPending` where it previously returned `Ok(())`,
  on a filesystem that refuses the directory sync (DUR-01). The file's contents
  are durable and the pathname is visible; only its directory entry's durability
  is unconfirmed, and the request stays pending so a later `sync` retries it.
  Subsequent syncs, and handles that merely opened an existing pathname, are
  unchanged.
- **Breaking (runtime):** decoding a very large `HashMap` under a tight explicit
  materialization limit can now fail with `Error::LimitExceeded` where it
  previously succeeded (SAFE-01). That is the fix: the old charge under-counted
  the allocation actually performed. The error variant and its
  `resource: "HashMap entries"` string are unchanged.
- Wire-breaking, stale-regenerable: the matrix layout is `VMAT` version 2 with
  an `MCRC` version 2 integrity region. Region lengths, `append_log_start`, and
  total matrix file length all change. A version 1 artifact is refused at open
  with the typed `Error::FormatVersionMismatch { expected: 2, actual: 1 }`
  (previously the generic `InvalidMatrixLayout`); recreate it.
- Wire-breaking, stale-regenerable: the disk-index sidecar metadata record is
  version 3 (length 260 -> 300, carrying the primary-generation witness) and the
  plan-digest domain was bumped. A version 2 sidecar is refused with
  `DiskIndexError::MetadataVersion`, and a sidecar published against an older
  plan digest is refused as stale. `rebuild_disk_index` is the recovery for
  both; neither is migrated in place.
- New public error variant `DiskIndexError::PrimaryGenerationMismatch` (the enum
  is `#[non_exhaustive]`, so this is additive) and new public
  `Error::KeyedChainRequiresKeyedApi { block_id }`.
- Callers that size a materialization budget to the exact payload byte count and
  use variable field ids above 63 must add 8 bytes per such distinct id.
- Source-breaking (no wire change): on a format whose `index_policy` enables
  `keyed_offset_chain`, the generic `VarveFile::push` / `push_info` and
  `VarveWriter::push` / `push_info` now refuse a keyed block with the new
  `Error::KeyedChainRequiresKeyedApi { block_id }` instead of silently writing
  `prev_same_key_offset = None` and truncating the keyed chain (API-02). The
  rejection is unconditional — it fires on the first push, not only on a push
  that would have had a predecessor — so the contract does not depend on call
  order. **Migration:** call the new maintaining `push_keyed` / `push_keyed_info`
  instead; they resolve the predecessor from the persisted keyed tail offsets
  and rebuild that cache after reopen. Unkeyed blocks and formats without
  `keyed_offset_chain` are unaffected, and the generated typed writers already
  take the maintaining path. `VarveFile::delete` / `VarveWriter::delete` had the
  same truncation defect and now maintain the chain; they gained a
  `T::Key: Eq + Hash` bound, already implied by `VarveKey`.
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
- The computed schema hash algorithm is version 2
  (`FormatSpec::SCHEMA_HASH_ALGORITHM_VERSION`): fields are hashed in
  declaration order with their encoding ordinal and a field-count frame, and
  per-block endian overrides, keyedness, and generated codec fingerprints are
  folded in via `FormatSpec::block_identities`. This closes the hole where two
  blocks with the same field id/name/type set in a different declaration order
  — and therefore different canonical bytes — hashed identically. **Every
  computed hash value changes.** A file created with a pinned v1 computed hash
  fails open with `SchemaHashMismatch` until recreated (pre-1.0 policy: no
  migration path). `schema_hash: computed` declarations recompute
  automatically at build time; release-pinned literal hashes must be
  re-derived. Omitting `schema_hash` still stores 0 and disables the open-time
  comparison, unchanged.
- Matrix native files gain the 24-byte creation-nonce region between the
  native file header and the matrix layout header. Matrix files created before
  this change are refused with `InvalidMatrixLayout` at the nonce region
  (pre-1.0 policy: recreate them). Non-matrix native files are unchanged.
- The matrix sidecar format is version 3 (fixed header grew from 88 to 104
  bytes for the creation nonce). Version-1 and version-2 sidecars are refused
  as `MatrixSidecarMismatch("sidecar version")`; regenerate them (sidecars are
  regenerable resume state, so no native file migration is required beyond the
  matrix-native recreation above).
- `FormatSpec` gained the public field `block_identities`; code constructing
  `FormatSpec` with an exhaustive struct literal must add it. Construction
  through `FormatSpec::new(...)`, the builder, or generated `spec()` is
  unaffected (defaults to empty).
- Under CRC policies, `rebuild_disk_index` no longer reads the payloads of
  records outside the index plan; a corrupt unindexed payload no longer fails
  a rebuild. `verify_all()` remains the whole-file integrity scan.

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
