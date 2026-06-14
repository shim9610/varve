# Work Routing

## Matrix P0 Implementation

Owner: main implementation worker.

Owned files:

- `crates/varve-core/src/matrix.rs`
- `crates/varve-core/src/lib.rs`
- `crates/varve-core/src/format.rs`
- `crates/varve-core/src/file.rs`
- `crates/varve-core/src/error.rs`
- `crates/varve-macros/src/lib.rs`
- matrix-focused tests under `crates/varve/tests`

Tasks:

- Add matrix descriptors, runtime dimensions, key/status/error types.
- Add `VMAT` create/open layout and append-log start routing.
- Add dense slot write/read/commit/clear APIs.
- Add format-first `matrix` block parsing and generated typed helpers.
- Add P0 acceptance tests and performance smoke paths.

## P1/P2 Extension Points

Owner: secondary worker or main after P0 lands.

Owned files:

- `crates/varve-core/src/matrix.rs`
- `docs/recovery-model.md`
- `docs/durability-model.md`
- targeted tests under `crates/varve/tests`

Tasks:

- Add region CRC metadata structures and classification enums. Implemented for
  `integrity: crc32` matrix metadata, commit maps, and per-cell slots.
- Add recovery action API skeletons with clear/rebuild boundaries. Commit-map
  rebuild from per-cell CRC evidence is implemented; destructive slot/category
  recovery remains caller-driven.
- Add ordered durability recorder/hook abstraction.
- Add sidecar/resume signal API scaffold.
- Add packed bitmap and borrowed numeric view scaffolds.

## Independent Verification

Owner: clean-context verifier.

Inputs:

- `docs/matrix-final-spec.md`
- worker summaries and diffs
- final test output

Checks:

- Append-log behavior remains compatible.
- P0 matrix semantics match the final spec.
- P1/P2 scaffolds do not overpromise incomplete behavior.
- Tests prove direct addressing, commit authority, file-size stability, and
  append-log `append_log_start` scanning.
