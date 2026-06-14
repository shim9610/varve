# Varve Requirements Boundary

This document maps `varve-requirements.md` into the boundary between Varve
runtime responsibilities and caller responsibilities. It is intentionally
storage-layer focused: domain analysis, scheduling, and business semantics stay
outside Varve.

## Storage Modes

Varve now has two distinct storage paradigms:

- `append_log`: the existing append-only record log with fixed/variable blocks,
  footer commit policies, offset chains, checkpoints, merge, compact, mmap, and
  optional zero-copy raw fixed reads.
- `preallocated_matrix`: a bounded, runtime-sized matrix storage mode with
  deterministic slot addressing, in-place same-size overwrite, independent
  commit bitmaps, region integrity, and recovery classification.

These modes share a format registry, endian policy, schema hashing, generated
reader/writer API shape, and error model. They do not share the same record
layout or validity model.

## Boundary Table

| Req | Priority | Varve responsibility | Caller responsibility | Target |
| --- | --- | --- | --- | --- |
| REQ-1 matrix block/direct addressing | P0 | Declare matrix blocks, persist runtime dimensions, compute deterministic slot offsets, expose O(1) cell read/write. | Provide runtime dimension values and choose which cells to write. | Implement |
| REQ-2 commit bitmap SSOT | P0 | Store packed commit maps, validate reads from commit bits only, expose cell, single, and per-channel status APIs. | Choose categories, set/clear commit timing, interpret domain progress. | Implement |
| REQ-3 in-place bounded overwrite | P0 | Preallocate slot regions, enforce fixed stride, reject wrong-size writes, keep file size bounded. | Select max sizes such as `n_wells_max`; choose when overwrite is appropriate. | Implement |
| REQ-4 region CRC | P1 | Define region CRC scopes, recompute affected scopes after in-place writes, report region failures. | Decide whether a damaged region is acceptable for the domain. | Implemented for `integrity: crc32` matrix metadata, commit maps, and per-cell slot CRCs |
| REQ-5 corruption classification/recovery | P1 | Classify corruption, expose structured recovery plans, implement safe primitive actions. | Choose the recovery action to apply and decide user-visible policy. | Commit-map CRC classification and rebuild primitive implemented; broader recovery remains incremental |
| REQ-6 durability barrier/hook | P1 | Enforce data/index/commit ordering and expose post-commit hook after durable completion. | Provide hook body and decide what events to emit. | Durable helper implemented |
| REQ-7 sidecar/resume | P1 | Represent companion metadata, detect partial progress, expose resume/restart/discard signals. | Define sidecar meaning, merge/discard policy, and domain UX. | Progress + companion-file presence signal implemented |
| REQ-8 packed bitmap/numeric view | P2 | Provide packed bitmap codec and positional numeric view APIs for matrix payload regions. | Choose field shapes and consume slices safely. | Implement core shapes where practical |
| REQ-9 block/chunk compression | P2 | Allow per-block compression policy and chunk CRC metadata where random access remains defined. | Choose which blocks compress and algorithm settings. | Scaffold, then expand |
| REQ-10 runtime dynamic dims | P0 dependency | Persist dimensions in the file contract instance and feed layout/bitmap sizing. | Supply values at create time and validate domain constraints. | Implement with P0 |
| REQ-11 declarative migration | P2 | Generate migration scaffolds and byte-copy unchanged regions where layout-compatible. | Write semantic conversion functions and approve output publication. | Scaffold |
| REQ-12 noncommit auxiliary block | P2 | Permit matrix-adjacent regions that are read by presence/layout rather than commit bitmap. | Decide auxiliary semantics and lifecycle. | Scaffold |

## Varve Owns

- Binary layout and versioned wire contract.
- Runtime dimension persistence and validation against declared layout formulas.
- Matrix cell addressing and offset calculation.
- Preallocated slot and offset-table layout.
- Commit bitmap physical storage, read validation, and category addressing.
- Cell, single, and per-channel commit flag storage primitives.
- Same-size in-place write safety.
- Region CRC calculation and verification for `integrity: crc32` matrix files.
- Recovery classification primitives and mechanically safe recovery actions such
  as rebuilding a damaged commit map from per-cell CRC evidence.
- Durability phase ordering.
- Generated reader/writer APIs and trait contracts.

## Caller Owns

- Runtime dimension values such as scan count, channel count, and maximum wells.
- Domain category names and which categories matter for a workflow.
- When data is written, committed, cleared, or overwritten.
- Whether logical `commit_*` plus explicit `flush`/`sync` is enough or the P1
  ordered barrier is required for the workflow.
- Which recovery action is acceptable after Varve reports a plan.
- Post-commit hook behavior such as progress events or IPC notifications.
- Sidecar file semantics, merge policy, and user-facing resume decisions.
- Semantic migration code for values whose meaning changed.

## P0 Completion Contract

P0 is complete only when a format can declare at least one matrix block and:

- Create files with runtime dimensions.
- Preallocate a deterministic, bounded slot region.
- Write cells in arbitrary order.
- Read committed cells by direct key without scanning other cells.
- Reject uncommitted cells with `NotCommitted`.
- Overwrite the same cell in place when encoded payload size equals the slot
  stride.
- Reject wrong-size overwrite attempts.
- Keep physical file length unchanged across valid overwrites.

## Compatibility Notes

- Existing append-log files and APIs remain valid.
- Existing fixed/variable block declarations remain append-log blocks by default.
- Matrix block validity is commit-bitmap based, not record-footer or transaction
  marker based.
- Normal reads treat the commit bitmap as the authority. Recovery may rebuild a
  damaged commit bitmap only from stronger evidence such as per-entry CRCs; this
  does not make slot bytes a normal validity source.
- With matrix CRC enabled, a same-size write clears the affected cell commit bit
  until the caller commits the new payload; the commit step records the per-cell
  slot CRC, sets MCRC slot-valid evidence, and updates the commit-map CRC.
- Static matrix auxiliary regions are Varve-managed preallocated byte ranges,
  but their contents, schema, and domain-level recovery semantics remain caller
  responsibility unless a future typed aux codec is declared.
- Varve can write and verify an optional sidecar envelope containing parent
  format identity, category, caller generation, payload length, and payload
  CRC32 when the `integrity` feature is enabled. Exact generation matching is
  available through the `*_with_generation` APIs. Varve does not define the
  resume payload schema or decide whether a valid payload is semantically
  preferable to recomputation.
- Varve can apply safe matrix recovery clear actions by clearing commit
  evidence and related CRC-valid metadata. It does not recompute domain data or
  decide which clear/rebuild plan is semantically correct for the caller.
- Varve can byte-copy a committed matrix cell between compatible source/target
  matrix blocks with matching dimensions and slot stride. Iterating keyspaces,
  deciding compatibility beyond byte layout, and publishing full migrated
  outputs remain caller responsibility.
- Offset chains remain an append-log indexing feature. Matrix direct addressing
  must not depend on prev-offset chains.
- Unsafe zero-copy remains opt-in; default matrix reads use safe owned decoding
  or checked borrowed views.
