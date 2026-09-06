# Matrix Recovery Model

This document defines the recovery vocabulary and primitive actions for
preallocated matrix storage. Recovery is structured as detect, classify, plan,
and apply. Varve reports facts and safe primitive options; callers choose
domain policy.

## Regions

Matrix recovery reasons about named regions:

- file header
- matrix layout header
- dimension table
- matrix block table
- commit map
- offset table
- slot region
- per-entry CRC table
- region CRC table
- append-log region
- sidecar metadata

`integrity: none` matrix files set the region CRC table offset and length to
zero. `integrity: crc32` matrix files write a VMAT `MCRC` table and enable
CRC-backed classification.

## CRC Coverage

Implemented CRC scopes are:

- dimension table
- matrix block table
- commit category table
- commit map regions
- per-cell slot payload CRCs

Commit map rebuild requires per-entry CRC or equivalent stronger evidence. A
CRC-free file can clear damaged commit maps or categories, but it cannot prove
slot validity during rebuild.

Each corruption report names the region, severity, recoverability, and
suggested actions.

## Classification

```rust
pub enum MatrixCorruptionSeverity {
    Fatal,
    Recoverable,
    Advisory,
}

pub enum MatrixCorruptionKind {
    Header,
    Layout,
    CommitMap,
    Slot,
    Sidecar,
}
```

Fatal examples:

- file magic mismatch
- unsupported matrix layout version
- impossible dimensions or overflowing layout math
- matrix block table CRC failure when no redundant source exists

Recoverable examples:

- commit map CRC failure when per-entry CRC can rebuild the map
- one cell slot CRC failure that can clear the cell bit
- category-wide damage that can clear one category
- incomplete append-log tail after matrix regions
- sidecar missing while the main file is intact

Advisory examples:

- sidecar exists but is inactive
- partial progress is detected and requires caller choice

## Recovery Plan

Open with recovery classification returns a report:

```rust
pub struct MatrixRecoveryReport {
    pub findings: Vec<MatrixRecoveryFinding>,
    pub recommended_actions: Vec<MatrixRecoveryAction>,
}

pub enum MatrixRecoveryAction {
    ClearCell { category: String, key: MatrixKey },
    ClearCategory { category: String },
    RebuildCommitMap { category: Option<String> },
    Resume,
    Restart,
    DiscardSidecar,
}
```

Applying a recovery action is explicit. The default strict open path reports
fatal or recoverable corruption without mutating the file. The current writer
API exposes `clear_matrix_cell_by_category`, `clear_matrix_category`, and
`apply_matrix_recovery_action` for safe clear/no-op actions. Typed CRC rebuild
continues to use `rebuild_matrix_commit_from_crc::<T>()` because the block type
is required to validate the matrix descriptor.

Open-time recovery classification currently reports metadata and commit-map CRC
failures. Committed cell reads verify the corresponding per-cell slot CRC and
return `MatrixChecksumMismatch` if slot bytes no longer match the committed CRC
evidence.

A commit-map CRC mismatch never leaves the unverified bits reader-visible. The
verification pass that finds it (`MatrixMetadataVerification::AtOpen`, the
default) produces a `Recoverable` `CommitMap` finding, and the presence of that
finding **is** the quarantine: the category is failed closed as a whole. Status
and value reads return `MatrixCommitQuarantined(category)` rather than
`NotCommitted`; commits and per-cell clear operations reject the category; and
`matrix_resume_signal` / `matrix_sidecar_resume_signal` refuse it too, because a
progress figure is an answer like any other. Only an explicit whole-category
recovery action lifts it. Cell categories may use typed CRC rebuild; single and
per-channel categories have no per-entry reconstruction evidence and therefore
recommend `ClearCategory`. Neither route exists for a format that declares a
growing matrix dimension: `clear_matrix_category` refuses a quarantined category
before it touches anything (`Error::MatrixCommitQuarantined`), and
`rebuild_matrix_commit_from_crc` refuses such a format outright
(`Error::InvalidFormatSpec`, "a growing matrix's chunks carry no per-cell
checksum table"), so a growing-matrix quarantine has no primitive that lifts it.
Both refusals are measured in `crates/varve/tests/matrix_chunks.rs`, by
`a_quarantined_category_refuses_the_category_clear` and
`the_entry_points_that_do_not_apply_say_so_by_name`.

> **Changed in 0.5.0, and it is why the resume signals refuse.** Quarantine used
> to retain the damaged map as "recovery evidence" and substitute an all-clear
> internal map, and a few answers came from that substitute — a resume query on a
> quarantined category reported `Clean`, i.e. "nothing in progress" for a map known
> to be damaged. Verification retains nothing now, so there is no map to answer
> from. `clear_matrix_category` remains the recovery path and, for a format with
> no growing matrix dimension, still does not refuse; it reports 0 cells cleared,
> because it cannot count bits it is discarding when every count authenticates
> what it reads. See
> [API Changes §A.5](api-changes.md#a5-changed-behaviour-at-an-unchanged-signature-in-050).

## Rebuild Commit Map

`RebuildCommitMap` treats commit bits as damaged and reconstructs them from
entry-level evidence:

1. clear the target commit map or category in memory
2. for every logical cell in the keyspace:
   - compute the slot offset
   - validate slot bounds
   - validate per-entry CRC if present
   - optionally validate decode according to the block codec
   - set the bit only when all enabled checks pass
3. write the rebuilt commit map
4. recompute the commit map CRC
5. sync according to durability policy

The current write API exposes this primitive as
`rebuild_matrix_commit_from_crc::<T>()`. It sets a bit only when the current
slot CRC matches the stored per-cell CRC and the MCRC slot-valid bit says the
slot was committed. The slot-valid evidence distinguishes a legitimate
committed all-zero payload from an untouched all-zero slot.

### Incomplete Evidence Refuses The Rebuild

Because a bit is set only where slot-valid evidence says the CRC is meaningful,
evidence that could not be read is *not* neutral: a validity page missing from
a damaged page index reads as absent, and the rebuild would publish those cells
as uncommitted. That is a destructive republication of the commit view derived
from bytes nobody read, and until F-04 it was reachable.

Each block's CRC-validity map now carries its own completeness status,
separately from the "the bytes I loaded were authentic" statement, and
`rebuild_matrix_commit_from_crc::<T>()` checks it first: if that block's
validity page index could not be enumerated in full, the rebuild returns
`Error::MatrixFatalCorruption` and writes nothing.

The refusal is unconditional. `FormatSpec::with_matrix_fatal_forensics` relaxes
*reading* fatal state so an operator can inspect a damaged file; it does not
authorize publishing a new commit map from evidence the reader already flagged
as unreadable, which is the same rule the interrupted-rebuild poison follows.
Repair or regenerate the validity evidence and retry. There is deliberately no
implicit path that discards unverifiable visibility; an operator who genuinely
wants that outcome should clear the category explicitly, which says so in the
operation's own name.

The other consumer of the same evidence was checked and already fails closed:
a committed cell read whose validity bit is absent returns
`MatrixChecksumMismatch` rather than silently accepting the slot.

Successful rebuild writes the new map and CRC before publishing it in memory,
then clears the quarantine finding for that category.

The rebuild republishes the whole persisted page index, and it does so by
publishing a new generation rather than editing the live one: a reserved marker
goes into the occupancy header and is made durable before the entry region is
cleared, and the real count is written last. An interruption therefore leaves a
matrix that opens with a `Fatal` `MatrixCorruptionKind::CommitMap` finding whose
report recommends `RebuildCommitMap` — never a short index that silently hides
committed pages. The reserved marker is `u64::MAX` in the occupancy-header slot,
which is not a representable occupancy count at any capacity, so a reader that
predates the marker also rejects it as a damaged header rather than reading a
partial rebuild as authoritative. The scheme needs no layout-version bump; `VMAT`
stays at version 4.

Because fatal findings are fail-closed by default, acting on a recommended
`RebuildCommitMap` for a fatal finding requires reopening with
`FormatSpec::with_matrix_fatal_forensics()`. If the rebuild itself fails after
it has marked the index, the layout stays fail-closed for the rest of that
session, since the in-memory index mirror describes the previous generation;
reopen with forensics and rebuild again.

If no per-entry CRC or equivalent evidence exists, Varve must not claim that a
rebuilt bit proves the cell is valid. It may offer a weaker `ClearCategory`
recommendation instead.

## Cell And Category Clearing

Clearing a cell or category changes only the commit map and affected CRC
metadata. Slot bytes may remain on disk. Readers must reject cleared cells with
`NotCommitted`.

The current implementation exposes this as `clear_matrix_cell_by_category`,
`clear_matrix_category`, and `MatrixRecoveryAction::ClearCell` /
`ClearCategory` through `apply_matrix_recovery_action`.

This is the primary safe action for localized slot damage: preserve the file,
hide only the affected logical data, and leave domain recomputation to the
caller.

## Sidecar And Resume Signals

Sidecar support exposes signals, not domain policy:

```rust
pub enum MatrixResumeSignal {
    Clean,
    Partial { committed: u64, total: u64 },
    ResumeAvailable { committed: u64, total: u64 },
    RestartRecommended,
    DiscardRecommended,
}
```

Varve determines the signal from:

- sidecar file presence
- committed/total progress in relevant commit maps
- optional Varve sidecar envelope validation

The current API `matrix_sidecar_resume_signal(category, path)` intentionally
uses only sidecar presence plus commit progress. The verified path,
`matrix_verified_sidecar_resume_signal(category, path)`, reads the optional
Varve sidecar envelope written by `write_matrix_sidecar` and checks parent
format identity, category, payload length, and payload CRC32 when the
`integrity` feature is enabled. Parent schema identity uses
`computed_schema_hash()`, not the pinned file-header `schema_hash`, so manually
pinned release hashes do not change the sidecar schema fingerprint. Generation
is stored and returned by the base read API;
`matrix_verified_sidecar_resume_signal_with_generation` and
`read_matrix_sidecar_with_generation` additionally require an exact caller
generation match. A missing verified sidecar follows normal restart/clean logic;
a corrupt or mismatched verified sidecar returns `DiscardRecommended`.

The envelope validates bytes and parent identity. Domain merge semantics,
whether a generation is recent enough, and how resume payloads are interpreted
remain caller-owned.

The caller decides whether to resume, restart, or discard.

## Acceptance Tests

Recovery tests should cover:

- one-byte commit map corruption classified as recoverable
- rebuilding a commit map from valid entry CRCs
- slot corruption clearing only the affected cell
- fatal classification for header/layout corruption
- incomplete append tail classified separately from matrix region corruption
- sidecar partial progress signal after simulated interruption
- verified sidecar CRC mismatch produces discard recommendation
- recovery clear-cell and clear-category actions hide data without rewriting
  slot payloads
