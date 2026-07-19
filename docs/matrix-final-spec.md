# Matrix Storage Final Spec

This is the implementation-facing spec for the first matrix/preallocated storage
wave.

## P0

- Matrix storage is a second storage mode beside the existing append log.
- Runtime dimensions are a P0 dependency and are persisted at create time.
- Matrix files store a 24-byte `VMNC` creation-nonce region and then a `VMAT`
  v2 layout region immediately after the normal Varve header. A `VMAT`/`MCRC`
  version 1 artifact is refused at open with
  `Error::FormatVersionMismatch { expected: 2, actual: 1 }`.
- Dense cells use deterministic addressing:
  `ordinal = scan * n_channels + ch` and
  `offset = slot_region_start + ordinal * slot_stride`.
- Slot payloads are fixed-stride, bounded, canonical encoded values.
- Commit bitmaps are the only normal read-time validity source.
- Readers snapshot layout metadata and commit maps on open. Slot bytes remain
  live in-place storage, so VMAT v2 does not promise immutable concurrent reads
  across an overwrite of the same slot.
- Same-size overwrite writes the existing slot range and must not change file
  length.
- A same-size cell overwrite clears that cell's commit bit and CRC-valid evidence
  before writing the replacement slot; callers must commit the replacement
  payload before a newly opened reader exposes it again.
- Partial matrix I/O leaves the cell uncommitted and poisons the writer. Bitmap
  state is published in memory only after the corresponding disk write succeeds.
- Append-log blocks may coexist after matrix regions; scanning starts at
  `VMAT.append_log_start`.
- Generated API exposes format dims, cell key types, `create_writer_with_dims`,
  `write_*`, `commit_*`, `clear_*`, `*_status`, and read helpers.
- P0 does not require large caller-provided empty matrices. Writers encode one
  cell into a scratch buffer and write the fixed slot.
- P0 `commit_*` is a logical visibility operation within Varve's explicit
  durability model. It sets the commit bit after the slot bytes have been
  written to the file handle, but it does not by itself promise crash durability
  unless the caller also uses `flush`/`sync` or the P1 ordered barrier API.
- P0 exposes durable helper shape as an extension point, but ordered
  `data/index/commit` durability is a P1 requirement.
- P0 includes storage primitives and generated helpers for cell-category bits,
  single global bits, and per-channel bits. Matrix cell reads use cell-category
  bits; singles/per-channel are status flags until a domain block binds them.
- In VMAT v2 each matrix block owns one unique cell commit category. Sharing a
  cell category across multiple matrix blocks is rejected until multi-block
  category rebuild semantics are explicitly defined.
- `commit_*` before a successful slot write is rejected with
  `MatrixCellNotWritten`, unless the target is a single/per-channel flag that
  has no slot payload.
- The current P0 implementation treats an all-zero slot as unwritten when
  reopening. This preserves commit-before-write safety without a separate
  written bitmap, but formats that need valid all-zero cells should add an
  explicit nonzero sentinel field until the persistent written bitmap lands.

## P1

- Region and per-entry CRCs protect layout, metadata, commit maps, and slots.
- Recovery reports classify corruption and expose primitive actions.
- `RebuildCommitMap` is allowed only with per-entry CRC or equivalent stronger
  evidence.
- Ordered durability barriers enforce data/index before commit-map sync.
- Post-commit hooks run only after durable commit.
- Sidecar/resume APIs report structured resume/restart/discard signals.

Current implementation status:

- Ordered durable write helper and post-commit hook event are available through
  `write_matrix_cell_durable`.
- `write_matrix_cell_durable_with_barrier` accepts an injectable durability
  barrier so tests and policy adapters can verify `data sync -> commit sync ->
  hook` ordering without simulating a crash.
- Resume/progress advisory reporting is available through
  `matrix_resume_signal` and `matrix_recovery_report`.
- Safe recovery clear primitives are available through
  `clear_matrix_cell_by_category`, `clear_matrix_category`, and
  `apply_matrix_recovery_action` for clear/no-op recovery actions. Typed
  commit-map rebuild remains `rebuild_matrix_commit_from_crc::<T>()`.
- `matrix_sidecar_resume_signal` combines commit-map progress with companion
  file presence and returns clean/resume/restart/discard advisory signals while
  leaving sidecar contents and merge policy to the caller.
- `write_matrix_sidecar`, `read_matrix_sidecar`, and
  `matrix_verified_sidecar_resume_signal` provide an optional Varve sidecar
  envelope with parent format identity, category, caller generation, payload
  length, and payload CRC32. Corrupt or mismatched verified sidecars produce a
  discard recommendation, while sidecar semantics remain caller-owned. The
  CRC32 envelope path requires the `integrity` feature. Callers that need
  generation equality checks use
  `matrix_verified_sidecar_resume_signal_with_generation` or
  `read_matrix_sidecar_with_generation`.
- When `integrity: crc32` is enabled, VMAT writes an `MCRC` v2 CRC table after
  the slot region and any static aux regions. It covers matrix metadata tables,
  every 4 KiB page of each commit map (one `crc32` plus a state word per page,
  where an uninitialized page must still read as all zeros), and each dense cell
  slot. A commit-bit mutation rehashes exactly one page, creation writes no
  per-cell integrity metadata, and the bitmaps are held sparsely after open. Open classifies metadata/commit-map CRC
  mismatches, committed cell reads verify slot CRCs, and writers can call
  `rebuild_matrix_commit_from_crc::<T>()` to reconstruct a damaged cell commit
  map from per-cell CRC evidence.
- Broader destructive recovery workflows beyond clear/no-op actions remain
  future P1 work.

## P2

- Checked borrowed views and packed bitmap/numeric positional access.
- Per-block and per-chunk compression with explicit random-access semantics.
- Auxiliary noncommit matrix-adjacent regions.
- Migration scaffolds that copy compatible bytes and leave semantic conversion
  to user code.

Current implementation status:

- `PackedBitmap` provides the LSB-first packed bitmap primitive.
- `matrix_cell_payload` exposes checked positional slot payload reads for view
  builders.
- With the `mmap` feature, unsafe `mmap_matrix()` exposes committed matrix slot
  payload windows from a read-only snapshot. The caller keeps the backing file
  immutable and valid through every handle and process for the mapping lifetime.
- `MmapMatrix::cell_numeric` and `cell_numeric_at` provide safe endian-aware
  numeric scalar reads from checked committed payload windows.
- With the `zero-copy` feature, `VarveRawMatrixBlock` allows explicit raw matrix
  cell views from mmap-backed slots when the implementor guarantees layout,
  endian, size, and alignment compatibility.
- Static matrix auxiliary regions are declared in `FormatSpec` or
  `varve_format! { aux { ... } }`, preallocated after slot payloads and before
  optional MCRC, and exposed as noncommit `matrix_aux_len`,
  `read_matrix_aux`, and `write_matrix_aux` APIs. Generated format APIs expose
  typed helpers such as `thumbnail_aux_len`, `read_thumbnail_aux`, and
  `write_thumbnail_aux`.
- Matrix byte-copy migration scaffolding is available through
  `copy_matrix_cell_bytes_from::<From, To>()`, which copies a committed source
  slot into a target matrix block when dimensions and slot stride match.
- `ChunkedBytes` provides an `integrity` + `compression-zstd` gated value
  helper for zstd chunk compression with per-chunk CRC32 validation. It can be
  stored in variable fields or aux payloads when callers want chunked blobs.
- VMAT-native direct-slot chunk compression, numeric slice batches, and bulk
  migration publication remain the next P2 implementation slices.

## Rejected Choices

- Treating matrix cells as append-log records.
- Using record footers or transaction markers as matrix validity.
- Inferring validity from nonzero slot bytes.
- P0 sparse matrices, variable-stride matrices, or offset-table-backed normal
  reads.
- P0 sidecars, recovery decisions, compression, default zero-copy, or migration.
- Per-cell fsync by default.

## P0 Acceptance Criteria

- Create with dimensions, reopen, and verify dimensions and layout.
- Write `(scan, ch)` cells in random order.
- Read committed cells by key without scanning sibling cells.
- Return `NotCommitted` for uncommitted cells, even when slot bytes are nonzero.
- Toggle visibility with commit/clear without modifying slot bytes.
- Reject committing a cell that has never been written.
- Support single and per-channel commit flags as storage primitives.
- Same-size overwrite keeps file length unchanged.
- Wrong-size writes return `MatrixSizeMismatch`.
- Bounds and overflow checks reject invalid keys/layouts.
- Mixed matrix plus append-log files scan append records from
  `append_log_start`.

## P1 Acceptance Criteria

- CRC failures are classified by region.
- Damaged commit maps rebuild only from per-entry evidence.
- Cells and categories can be cleared safely.
- Ordered barrier sync ordering is testable with an injectable recorder.
- Hooks run only after durable commit.
- Sidecar partial progress reports structured resume/restart/discard signals.

## P2 Acceptance Criteria

- Checked borrowed views avoid default unsafe zero-copy.
- Compression preserves random-access contracts.
- Auxiliary noncommit regions are readable by layout/presence.
- Migration scaffolds copy compatible data while semantic conversion remains
  user code.
