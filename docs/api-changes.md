# API Changes — 0.3.0 to 0.4.0

Migration document for 0.4.0. Companion to [Known Limitations](known-limitations.md)
and the [Changelog](../CHANGELOG.md).

---

## Read this first: files written by an older version

**Matrix files, matrix sidecars, disk-index sidecars, and any file pinned with a
computed schema hash must be regenerated. They cannot be opened by 0.4.0 and
there is no migration path.** This is the crate's stated pre-1.0 policy: stale
persisted artifacts are refused with a typed error rather than migrated in place.

Plain append-log native files with no matrix, no disk index, and no pinned
computed schema hash **remain readable**. The record footer, record framing and
file header are unchanged since 0.1.

Nothing in the list below is silently misread. Every stale artifact class is a
typed refusal:

| Artifact | Outcome under 0.4.0 | Recovery |
| --- | --- | --- |
| Plain native append-log file | opens normally | none needed |
| Native file pinned with a **computed** schema hash from 0.3.0 or earlier | `Error::SchemaHashMismatch` | recreate the file, or re-derive the pinned literal |
| Matrix native file (`VMAT` v1/v2/v3) | `Error::FormatVersionMismatch { expected: 4, actual }` | recreate the matrix |
| Matrix native file created before the creation-nonce region | `Error::InvalidMatrixLayout` at the nonce region | recreate the matrix |
| Matrix sidecar (v1/v2) | `Error::MatrixSidecarMismatch("sidecar version")` | regenerate — sidecars are resume state |
| Disk-index sidecar (metadata v2, or a stale plan digest) | `DiskIndexError::MetadataLength` — the v2 record is 260 bytes where 300 is required (never shipped; see below) | `rebuild_disk_index` |

**The two rows that cost real work are the first two, in this order.** The
distinction is not cosmetic:

1. A **matrix native file** and a **file pinned with a computed schema hash** hold
   your data. They must be recreated and their contents copied by you. Varve has
   no migration tool for either.
2. A **matrix sidecar** and a **disk-index sidecar** are regenerable resume state.
   One named call rebuilds them and nothing is lost.

On the disk-index row specifically: `decode_metadata` checks the record **length**
before magic and version, so a v2 record (260 bytes) is refused as
`MetadataLength`, not `MetadataVersion`. The refusal is typed either way and
nothing is misread, but code matching the variant should match `MetadataLength`.
This row is hypothetical in any case — `disk_index.rs` is new in 0.4.0, so no
released version ever wrote a v2 sidecar.

**Assurance note on this table.** No artifact written by an older version exists in
this repository and **no test in the suite opens one**, so the table is derived from
the current source. Three rows were checked by hand on 2026-07-25 against files
written by the real 0.1.0 and 0.3.0 source packages, and all three behaved as
documented: a plain append-log file written by 0.1.0 opens with every field intact,
the same holds for one written by 0.3.0, and a 0.3.0 file pinned with a computed
schema hash is refused with `Error::SchemaHashMismatch`. That was one manual run
over one two-field format — not a regression test, and it covers neither the matrix
rows nor the sidecar rows, which remain inspection-only. See
[Known Limitations §6.7](known-limitations.md#67-the-backward-compatibility-table-what-is-now-executed-and-what-still-is-not).

---

## 1. Wire-format changes

| Region | Constant | Value in 0.4.0 | Was |
| --- | --- | --- | --- |
| Matrix layout | `VMAT_VERSION` | **4** | 3 |
| Matrix header length | `VMAT_HEADER_LEN` | 160 | — |
| Matrix creation nonce | `MATRIX_CREATION_NONCE_VERSION` / `_MAGIC` | 1 / `VMNC`, 24-byte region | did not exist |
| Matrix sidecar | `MATRIX_SIDECAR_VERSION` | **3** (fixed header 88 → 104 bytes) | 2 |
| Disk-index sidecar metadata | `META_VERSION` | **3** (record length 260 → 300, carries a primary-generation witness) | 2 |
| Record footer | `RECORD_FOOTER_MAGIC` / `_VERSION` / `_LEN` | `VRF1` / 1 / 32 — **unchanged since 0.1** | same |
| Computed schema hash algorithm | `FormatSpec::SCHEMA_HASH_ALGORITHM_VERSION` | **3** | 1 (implicit; the constant did not exist) |

### 1.1 The computed schema hash changed twice

The algorithm went from an implicit version 1 in 0.3.0, through version 2, to
**version 3** in this release (domain tag `varve-schema-v3`).

- **v2 (API2-01):** fields are hashed in declaration order with their encoding
  ordinal and a field-count frame; per-block endian overrides, keyedness, and
  generated codec fingerprints are folded in via `FormatSpec::block_identities`.
  This closed the hole where two blocks with the same field id/name/type set in a
  different declaration order — and therefore different canonical bytes — hashed
  identically.
- **v3 (API-04):** the folded block fingerprints resolve each field's codec
  through `VarveEncode::SCHEMA_ID` instead of the field type's source spelling, so
  two custom nested codecs spelled identically but emitting different bytes no
  longer share a hash.

**Every computed hash value changed, twice.**

- `schema_hash: computed` declarations recompute automatically at build time. No
  source edit is needed, but existing files pinned with a pre-v3 value fail open
  with `Error::SchemaHashMismatch` until recreated.
- Release-pinned **literal** hashes must be re-derived from
  `FormatSpec::computed_schema_hash()`.
- Omitting `schema_hash` still stores 0 and disables the open-time comparison.
  Unchanged.

Also breaking for hand-written codecs: the derive now rejects at compile time any
field whose codec leaves `SCHEMA_ID` at the default `0`, regardless of wire type.

---

## 2. Removed items

**Nothing in this section requires a source edit by a 0.3.0 user.** Every entry
below either never existed at 0.3.0 or was already gone before it. They are
recorded because each was announced as a removal during the 0.4.0 cycle and a
reader who followed those notes will look for them here — not because any of them
is a migration step. §2.1 is the only one that is a *migration surface* item at
all, and only for someone tracking git rather than releases.

The **real** breaking changes are §3 (changed signatures) and §5 (changed
behaviour at an unchanged signature). If you are short of time, skip to §3.

### 2.1 `KeyedMergeEstimate::peak_resident_bytes()` → `peak_resident_structural_bytes()`

**Not a 0.3.0 → 0.4.0 removal.** Neither `KeyedMergeEstimate` nor
`estimate_keyed_merge` existed at 0.3.0; both were added inside the 0.4.0 cycle,
the method was first spelled `peak_resident_bytes()`, and it was renamed later in
the same cycle. **No released version ever exposed the old name**, so no released
API can break. (The same type is listed as *new* in §4.1, correctly.)

The rename is documented because the reason for the longer name is a contract
correction worth knowing: the old name's documented "upper bound" was false for
any heap-owning `Key` or `T`, by an arbitrarily large margin.

```rust
let estimate = estimate_keyed_merge::<Rec>(&inputs)?;
if estimate.peak_resident_structural_bytes() > budget { /* ... */ }
```

The value is a **structural estimate, not an upper bound**. It includes the
overlapping output-vector term, and it excludes heap owned by `Key`/`T` values,
`HashMap` load-factor slack and control bytes, decode scratch, and allocator
metadata. Size with margin; see
[Known Limitations §2.3](known-limitations.md#23-keyed-merge-and-compact-are-resident-only-and-explicitly-not-petabyte-scale).

### 2.2 `VarveWriter::reserve_keyed_tail_slot` — never shipped

**Added and removed inside the 0.4.0 cycle.** It has zero occurrences anywhere in
the tree at 0.3.0, so nothing released could have called it. It went when the
generated keyed writers' private tail maps were deleted (its own rustdoc admitted
it could not prevent a later `Hash`/`Eq` from running after publication).

(A *private* `VarveFile::reserve_keyed_tail_slot` still exists in the source; it is
not `pub` and is not this item.)

### 2.3 `DiskIndexError::BatchPoisoned` — never shipped, and never reachable

**Could not have existed at 0.3.0:** `disk_index.rs` is a new module in 0.4.0, as
are `stream.rs`, `indexed.rs` and `scan_control.rs`. The whole `DiskIndexError`
type is new. Beyond that the variant was unreachable even within the cycle — the
flag that would have set it was never set by any code path.

### 2.4 `ReplaceStrategy::FixedInPlace` — removed in 0.2.0

Already absent at 0.3.0. Listed only so the removal is not attributed to this
release.

---

## 3. Changed signatures

### 3.1 `VarveBlock` gains two required associated constants — **every manual `impl` stops compiling**

`const SCHEMA_FINGERPRINT: u64` has no default. `const IS_KEYED: bool` has no
default under the `high-cardinality-dev` feature (and defaults to `false`
otherwise).

Blocks produced by `#[derive(VarveBlock)]` or by `varve_format!` are unaffected —
the macros emit both constants.

```rust
// Before (0.3.0) — a hand-written block
impl VarveBlock for Reading {
    const ID: u32 = 7;
    const VERSION: u16 = 1;
    const KIND: BlockKind = BlockKind::Fixed;
    const ENDIAN: Option<Endian> = None;
}

// After (0.4.0)
impl VarveBlock for Reading {
    const ID: u32 = 7;
    const VERSION: u16 = 1;
    const KIND: BlockKind = BlockKind::Fixed;
    const ENDIAN: Option<Endian> = None;
    const IS_KEYED: bool = false;
    // Deterministic identity of this block's declared schema. Process-local;
    // deliberately not part of the wire format.
    const SCHEMA_FINGERPRINT: u64 = 0x5f3a_11c9_04d8_7b26;
}
```

**A manual implementation that mirrors a generated block must reuse that block's
`SCHEMA_FINGERPRINT` constant**, not invent a value. Typed registration rejects
two implementations claiming the same block id with different fingerprints, so an
invented value fails at registration with
`Error::BlockSchemaFingerprintMismatch`.

A new marker, `traits::KeyedBlockContract<T>`, turns
`impl VarveKeyedBlock` + `IS_KEYED = false` into a compile error at every keyed
use site.

### 3.2 `FormatSpec` gains two public fields — exhaustive struct literals break

```rust
pub block_identities: &'static [(u32, Option<Endian>, bool, u64)],
pub matrix_fatal_forensics: bool,
```

`FormatSpec::new`, the builder, and the `spec()` generated by `varve_format!` are
unaffected. Only code constructing `FormatSpec { .. }` as an exhaustive literal
needs an edit — add the fields, or use the builder.

### 3.3 `Error::PublishedButRebindFailed` gains a third field

The **enum** is `#[non_exhaustive]`; this **variant** is not, so destructuring
breaks.

```rust
// Before (0.3.0)
Err(Error::PublishedButRebindFailed { sequence, source }) => { /* ... */ }

// After (0.4.0) — either name the new field
Err(Error::PublishedButRebindFailed { sequence, source, parent_sync }) => { /* ... */ }
// or ignore it
Err(Error::PublishedButRebindFailed { sequence, source, .. }) => { /* ... */ }
```

`parent_sync: Option<Box<Error>>`. `Some(_)` means: the new generation is visible
at the pathname, **and** the writer is unusable, **and** the rename is not yet
proved durable against power loss. `None` means only the first two. `Display`
gains a trailing clause when the field is present.

### 3.4 Matrix reads relax from `&mut self` to `&self` — source-compatible

This is the complete matrix read surface — all six entry points, each on all three
handle types (18 signatures):

| Method | 0.3.0 | 0.4.0 | On |
| --- | --- | --- | --- |
| `read_matrix_cell::<T>` | `&mut self` | `&self` | `VarveReader`, `VarveWriter`, `VarveFile` |
| `matrix_cell_payload::<T>` | `&mut self` | `&self` | `VarveReader`, `VarveWriter`, `VarveFile` |
| `read_matrix_aux` | `&mut self` | `&self` | `VarveReader`, `VarveWriter`, `VarveFile` |
| `matrix_cell_status::<T>` | `&self` | `&self` (unchanged) | `VarveReader`, `VarveWriter`, `VarveFile` |
| `matrix_aux_len` | `&self` | `&self` (unchanged) | `VarveReader`, `VarveWriter`, `VarveFile` |
| `matrix_resume_signal` | `&self` | `&self` (unchanged) | `VarveReader`, `VarveWriter`, `VarveFile` |

**Three methods relaxed, and three were already `&self`.** `matrix_cell_payload`
and `read_matrix_aux` are listed explicitly because an earlier version of this
table omitted them, which read as though payload and aux reads still required
exclusive access. They do not. That same earlier version put `matrix_cell_status`
in the relaxed group, which was wrong in the other direction: it took `&self` on
all three handle types in 0.3.0 already (verified against the `varve-core 0.3.0`
source package), so it is not a signature change and needs no entry in your
migration. `crates/varve/tests/matrix_concurrent_reads.rs` pins 15 of the 18
signatures ("5 entry points x 3 handle types") by holding two shared borrows of a
non-`mut` handle; `matrix_resume_signal` is the sixth entry point and was also
already `&self`.

**No caller edit is required** — relaxing `&mut self` to `&self` is
source-compatible, and an existing call through a `&mut` binding still compiles.
What changes is what becomes *possible*: one handle can now serve concurrent
readers, and the reader handle is `Send + Sync`. See
[Known Limitations §4](known-limitations.md#4-concurrency-what-is-self-and-what-a-reader-sees-of-a-writer)
for the measured scaling and the Windows/Unix difference.

### 3.5 Smaller signature changes

- `VarveFile::delete` / `VarveWriter::delete` gained a `T::Key: Eq + Hash` bound.
  It is implied by `VarveKey`, so no practical caller breaks — **but the same call
  also changed what it writes to disk. See §5.11.**
- `ReadLimits` gained `max_keyed_tail_bytes` and `matrix_metadata_residency`.
  `ReadLimits` is `#[non_exhaustive]`, so the *type* change is additive — but
  `matrix_metadata_residency` is not a limit and does not compose like one. See
  §5.12.

### 3.6 `PackedBitmap` is no longer accepted as a fixed-width matrix field

`PackedBitmap` owns a `Vec<u8>` and therefore has no compile-time slot stride. It
is still usable as a **variable** field. Inline matrix `SLOT_STRIDE` is now
generated from `VarveEncode::WIRE_TYPE`, so a user type merely *spelled* `u32`
can no longer be laundered through the syntactic pre-filter.

### 3.7 Materialization budget arithmetic

Callers sizing a materialization budget to the exact payload byte count, and
using variable field ids **above 63**, must add 8 bytes per such distinct id.

---

## 4. New items

### 4.1 Always available

| Item | Notes |
| --- | --- |
| `format::MatrixMetadataResidency` | `EagerVerified` (default) \| `Lazy { cache_bytes: u64 }` |
| `ReadLimits::matrix_metadata_residency` + `with_matrix_metadata_residency` | reader-side policy; no on-disk byte depends on it |
| `ReadLimits::max_keyed_tail_bytes` + `with_max_keyed_tail_bytes` (DSL key `keyed_tail`) | resource name `"keyed tail bytes"` |
| `FormatSpec::block_identities` / `with_block_identities` | per-block identity table folded into the computed schema hash |
| `FormatSpec::matrix_fatal_forensics` / `with_matrix_fatal_forensics()` | opt-in to reading matrix data through `Fatal` recovery findings |
| `traits::KeyedBlockContract<T>` | compile-time keyedness proof |
| `VarveEncode::SCHEMA_ID` / `VarveDecode::SCHEMA_ID` | structural identity of a codec's bytes |
| `file::{CREATION_NONCE_BLOCK_ID, KeyedMergeEstimate, ReplacePublicationFailure, classify_replace_publication_error, clear_stale_writer_lock, compact_keyed_files_with_key_limit, estimate_keyed_merge, merge_keyed_files_with_key_limit}` | key-limited merge/compact and publication-failure classification |
| `merge::compact_keyed_file_with_key_limit` | |
| `Encoder::encode_nested_to_vec` | caps a child encoder at the parent's remaining logical-payload budget |
| `MatrixSidecarManifest::matrix_creation_nonce` | 16-byte per-create nonce binding a sidecar to one logical matrix creation |

New `varve_format!` DSL keys: `key_index = disk | memory`, `disk_index_plan`, and
`keyed_tail` inside `limits { }`.

### 4.2 Behind `high-cardinality-dev`

Whole modules, none of which existed in 0.3.0: `disk_index`, `indexed`, `stream`,
`scan_control`. See [Scalable I/O](scalable-io.md) and
[Known Limitations §3](known-limitations.md#3-the-scalable-family-is-behind-a-feature-flag-named-dev).

### 4.3 Error variants

Sixteen variants were added and none removed (94 → 110). `Error` is
`#[non_exhaustive]`, so additions alone are not breaking:

`BlockKeyednessMismatch`, `BlockSchemaFingerprintMismatch`,
`CommittedButDurabilityUnproven`, `DiskIndex`, `IndexBusy`,
`InvalidBatchOptions`, `KeyedChainRequiresKeyedApi`,
`MatrixCommittedButDurabilityUnproven`, `MatrixCommittedButHookFailed`,
`MatrixFatalCorruption`, `PublishedButIndexStale`, `PublishedButParentSyncPending`,
`ReplacePublicationIndeterminate`, `ScanCancelled`, `StreamingUnsupported`,
`WriterLockMarkerNotDedicated`.

---

## 5. Changed behaviour at an unchanged signature

**This is the dangerous group: code that still compiles and now behaves
differently.** Read all of it.

### 5.1 Replacement refuses a cross-version target

`replace_rewrite` and `unsafe replace_fixed_in_place_exclusive` now refuse a
target whose stored `block_version != T::VERSION` with
`Error::BlockVersionMismatch`. Previously they wrote `T`'s payload under the
stored older version header — a persistent type/version disagreement on disk,
reachable whenever `schema_hash = 0` disables header-level schema locking.

On `replace_fixed` and `replace_block` the check **moved ahead of** the size,
limit and generation checks, so a call that is wrong in two ways now reports
`BlockVersionMismatch` where it previously reported `ReplaceSizeMismatch`.

**What to do:** if you relied on replacing across versions, migrate the record by
appending a new one instead.

### 5.2 Generic `push` refuses keyed blocks on `keyed_offset_chain` formats

Unconditionally, from the first push:

```text
Error::KeyedChainRequiresKeyedApi { block_id }
```

Previously the generic path silently wrote `prev_same_key_offset = None` and
truncated the chain.

```rust
// Before (0.3.0) — compiled, ran, corrupted the chain
writer.push(&user)?;

// After (0.4.0)
writer.push_keyed(&user)?;        // or push_keyed_info
```

### 5.3 New durability outcomes where a bare `Err` used to be returned

Each of these means **the work is already in the file**. Do not retry the
transaction — retrying would append a second marker for work already recorded.

| Call | New outcome | Correct response |
| --- | --- | --- |
| `commit_durable` | `Error::CommittedButDurabilityUnproven { sequence, source }` | the commit marker is in the file; retry `sync` alone |
| `write_matrix_cell_durable` | `Error::MatrixCommittedButHookFailed` | the cell is durable; the hook did not run — issue the notification from the carried event |
| `write_matrix_cell_durable` | `Error::MatrixCommittedButDurabilityUnproven` | the cell is durable; reopen and `sync` |
| first `sync()` / `commit_durable()` on a handle that *created* its pathname | `Error::PublishedButParentSyncPending` where `Ok(())` was returned | the request stays pending; a later `sync` retries |
| stream/indexed chunk commit | `Error::PublishedButIndexStale { sequence, source }` where the failure surfaced as `Error::Io` | the records are already in the native file |

### 5.4 Matrix access is fail-closed on a `Fatal` recovery finding

Readers that relied on reading through fatal-state files must opt in:

```rust
// After (0.4.0)
let spec = AppFormat::spec().with_matrix_fatal_forensics();
```

Related, and also a behaviour change for already-damaged files: **a damaged matrix
page-index entry is no longer silent.** A file that used to open "clean" with a
zeroed index entry — hiding every page after it — now reports a `Fatal` finding.

And `rebuild_matrix_commit_from_crc` returns `Err(Error::MatrixFatalCorruption)`
instead of `Ok(count)` when the CRC-validity page index could not be enumerated in
full — **including** under `with_matrix_fatal_forensics`. Forensic mode relaxes
*reading* fatal state, not a destructive republication.

### 5.5 Typed registration rejects contradictory endianness

`Error::EndianMismatch` is raised at typed registration when a
`VarveBlock::ENDIAN` contradicts the format's declaration for that block id.
Previously a manual type could request a block written big-endian through a
little-endian implementation and read byte-swapped values.

### 5.6 Tighter and more accurate limit charges

- **`HashMap` decode under a tight explicit materialization limit can now fail**
  with `Error::LimitExceeded` where it succeeded. The old charge under-counted the
  real allocation (hashbrown small-table capacity classes). Raise the limit if the
  workload was genuinely within budget.
- **The keyed-tail ceiling is charged at *build*, not only at growth.** Opening a
  file now charges the structural peak of the keyed-tail map build, which is
  larger than the map it produces. A ceiling sized from the steady-state map can
  refuse an open that previously succeeded.
- **`PackedBitmap::new` / `get` / `set` / `decode_varve`** now return
  `Error::InvalidCanonicalEncoding` or `Error::LengthOverflow`, and a non-matrix
  allocation resource, instead of `Error::InvalidMatrixLayout`.

### 5.7 An indexed writer refuses after any stream poison

Including one raised by work it did not itself perform. It still reports
`WriterPoisoned("indexed")` when reached through the indexed API.

### 5.8 `FormatSelfTest::run` is non-destructive

- It refuses a pre-existing target path as a `CallerUsage` failed step instead of
  truncating it.
- `cleanup(true)` removes only files the run created.
- On Unix, destructive cleanup is **refused** in a directory that is not
  exclusively owned (for example `/tmp` without a sticky bit), with an
  `Environment` step failure; the artifact is left behind.

A `cleanup(true)` self-test can therefore now report a failed step where it
previously reported a clean pass.

### 5.9 `rebuild_disk_index` reads less

It no longer reads payloads of records outside the index plan under CRC policies,
so a corrupt *unindexed* payload no longer fails a rebuild. `verify_all()` remains
the whole-file scan.

### 5.10 `scripts/run-security-fuzz.ps1` exit codes

It can exit non-zero where it exited 0: **2** when `fuzz/artifacts` already holds
a reproducer before the run starts, **3** when any file is present after a target.
It never deletes anything.

### 5.11 `delete` now maintains the keyed offset chain instead of truncating it

The sibling of §5.2, and it is here for the same reason: **the on-disk chain shape
changed at an unchanged signature.**

`VarveFile::delete` / `VarveWriter::delete` previously wrote
`prev_same_key_offset = None` on the tombstone, truncating the physical chain for
that key. They now resolve the predecessor and maintain the chain, routing through
the same maintained keyed path as `push_keyed`.

- **Source impact:** none beyond the added `T::Key: Eq + Hash` bound (§3.5).
- **On-disk impact:** tombstones written by 0.4.0 carry a predecessor offset where
  0.3.0 wrote none. Anything that read `prev_same_key_offset` on a tombstone and
  relied on it being `None` — a hand-rolled chain walker, an external inspector,
  a test asserting the field — sees a different value.
- **Mixed-version files are self-consistent, not uniform.** A file written partly
  by 0.3.0 and partly by 0.4.0 has truncated chains through the old tombstones and
  intact chains through the new ones. Chain walks terminate correctly either way,
  because a truncated chain terminates; they just see fewer historical versions of
  a key through an old tombstone.

Unlike §5.2, this change does not refuse anything — it silently produces better
data. It is disclosed because §5 is the section for exactly this class.

### 5.12 `*_with_resource_limits` replaces a spec-declared matrix residency policy

New in 0.4.0 and easy to miss because the signature did not change:
`ReadLimits` now carries `matrix_metadata_residency`, which is a **declaration,
not a ceiling**. `ReadLimits::overlay` therefore takes the runtime value
unconditionally rather than composing it, and `FormatSpec::with_resource_limits` is
`resolve().overlay(limits)`.

```rust
// The spec declares Lazy. This call reverts it to EagerVerified, because
// ReadLimits::STANDARD carries EagerVerified.
let reader = AppFormat::open_reader_with_resource_limits(
    path,
    ReadLimits::STANDARD.with_max_matrix_bitmap_bytes(n),
)?;
```

| Entry-point family | Composition | Format's residency policy |
| --- | --- | --- |
| `*_with_resource_limits` | `overlay` | **replaced** by the passed value, always |
| `*_with_limits` (legacy fieldwise) | `tighten` | kept |
| plain `open` / `create` | — | kept |

**Who this breaks:** anyone who declared a `limits { }` block on a spec *and* also
passes `ReadLimits` at the entry point, if they later adopt `Lazy`. Before 0.4.0
the field did not exist, so no 0.3.0 code can regress — but code written against
0.3.0 that passes resource limits will silently pin `EagerVerified` forever.

**What to do:** put the residency policy in the same `ReadLimits` value you pass:

```rust
let limits = ReadLimits::STANDARD
    .with_matrix_metadata_residency(MatrixMetadataResidency::Lazy { cache_bytes: 1 << 20 })
    .with_max_matrix_bitmap_bytes(n);
```

There is no `varve_format!` DSL key for residency (`limits { }` takes
`key_index`, `disk_index_plan` and `keyed_tail`), so this is the only route to
`Lazy` at an entry point that also takes limits. See
[Known Limitations §1.6](known-limitations.md#16-_with_resource_limits-silently-discards-a-declared-residency-policy).

---

## 6. Not in the migration surface: internal constructor removals

`FatalAccessGate::new(bool)`, `FatalAccessGate::allow(&self)` and
`CrcValidEvidence::new(bits, complete)` were removed. They are listed here only to
forestall the question.

These types are reachable from outside `varve-core` **only** through the
`#[doc(hidden)]`, `scalable-fault-injection`-gated `enforcement_probe` module,
whose own rustdoc states "This is not API". No stable public surface changed, and
no downstream crate can have depended on them. See
[Known Limitations §9](known-limitations.md#9-contributor-facing-notes-not-reachable-by-users).

---

## 7. Migration checklist

1. Regenerate every matrix file, matrix sidecar and disk-index sidecar. Recreate
   or re-derive anything pinned with a computed schema hash.
   ("Read this first" above, and §1)
2. Add `IS_KEYED` and `SCHEMA_FINGERPRINT` to every **manual** `impl VarveBlock`.
   (§3.1)
3. Fix any exhaustive `FormatSpec { .. }` literal. (§3.2)
4. Add `parent_sync` or `..` to any `PublishedButRebindFailed` destructuring.
   (§3.3)
5. On `keyed_offset_chain` formats, switch keyed appends from `push` to
   `push_keyed`. (§5.2)
6. Handle the five new post-publication durability outcomes without retrying the
   transaction. (§5.3)
7. If you read matrix data from files with `Fatal` findings, add
   `with_matrix_fatal_forensics()`. (§5.4)
8. Re-check `max_keyed_tail_bytes` and any tightened materialization limit
   against the new charge points. (§5.6)
9. If you pass `ReadLimits` to a `*_with_resource_limits` entry point, put the
   matrix residency policy in that same value or lose it. (§5.12)
10. Read [Known Limitations §1](known-limitations.md#1-matrix-opening-a-matrix-is-not-o1-and-its-metadata-residency-is-not-a-cache)
    before sizing a matrix workload — in particular §1.5, since
    `max_matrix_cells` defaults to 16,000,000 and a larger matrix fails at
    **create** until you raise it.

Nothing in §2 is a checklist item: every removed name either never shipped or was
already gone at 0.3.0. §2 explains why.
