# API Changes — 0.3.0 through 0.7.0

Migration document. Companion to [Known Limitations](known-limitations.md)
and the [Changelog](../CHANGELOG.md).

Sections run newest first, and the letters ascend with the release they
describe. Section **C** is the 0.6.0 → 0.7.0 migration: a writer open that does
not scan, an editable written matrix chunk, and one renamed error. Section **B**
is the 0.5.0 → 0.6.0 migration: two changed signatures from the reader no longer
keeping a copy of the file's index. Section **A** is the 0.4.0 → 0.5.0
migration: four changes, all about matrix commit-metadata residency and
verification. Everything numbered 1 through 5 is the
0.3.0 → 0.4.0 migration and is unchanged except where a section says 0.5.0
corrected it — §5.12 in particular now describes a bug that **no longer exists**,
and says so in place.

No on-disk byte changes in 0.5.0. A 0.4.0 file reads unchanged; no encoder,
decoder, header field or version constant was touched. If you are coming from
0.3.0 or earlier, read "Read this first" below — that guidance is unchanged.

---

## C. From 0.6.0 to 0.7.0: a writer open that does not scan, an editable written chunk, and one renamed error

### C.1 `VarveFile::open_lazy` / `VarveWriter::open_lazy` — a writer that does not scan

New, additive; nothing to migrate. Every other writer open frames every record
in the file, at any size, because it builds the resident index. `open_lazy`
reads the digest instead — the same route `open_readonly_lazy` takes, with
write access and the writer lock.

Measured on this tree, 200 records: the scanning open frames **202**, the lazy
open frames **1**. The difference is the file, not the constant: at a billion
records the lazy open still frames one.

    let mut writer = VarveWriter::open_lazy(spec, path)?;   // no scan
    writer.push(&row)?;                                     // appends normally

Two things it does not have, both inherited from the read-only digest open:

* **No resident directory.** `blocks::<T>()` refuses with
  `NoResidentDirectory`; `record_map` is the way to walk records. Appending is
  unaffected — the append path maintains the block tails and the sequence
  itself, and the digest supplied both.
* **No checkpoint or segment.** A spec with `checkpoint_on_flush` or
  `segment_on_flush` is refused with the new `Error::LazyWriterIndexPolicy`.
  Both records serialize the resident index this handle does not keep, so there
  is nothing to write; refusing at open beats writing nothing and leaving a
  file that opens slower than its spec says. A segment and a digest answer the
  same question — declare one.

`open_lazy_with_report` returns `LazyOpenSource` like its read-only twin, and
the fallback is the same: any file whose digest is not usable opens by scanning.

### C.2 A written matrix chunk can now be edited, and `Error::MatrixChunkNotReopenable` is new

**A write to a row of an already-written chunk used to fail with
`Error::MatrixChunkClosed`. It now succeeds.** If your code matched on that
error to detect "too late, this row is gone", delete the arm: the write lands.
Nothing else in the signature changed — `write_matrix_cell`,
`write_matrix_cell_payload` and `clear_matrix_cell` take the same arguments and
return the same type.

The refusal had nothing behind it. A varve file's matrix *region* is rewritten
in place on every write; a chunk is an ordinary record, and rewriting one of
those in place is a route the writer has had since 0.5.0. So a reader-writer
could edit a written row of a fixed matrix and not a written row of a growing
one, for a reason that existed only in the chunk code. Worse, the refusal also
caught rows of a chunk that was merely *skipped* — one where nothing was ever
committed, so no record was written at all — and made them permanently
unwritable.

What happens now: the open chunk is written out, the addressed chunk's record is
read back into the buffer, the edit applies, and the next chunk transition
rewrites that record **where it already sits**. The file does not grow, and no
second record for the same chunk index is ever created.

Two costs, stated plainly:

* **Memory is unchanged, latency is not.** The reload reads one chunk's commit
  map, checksum table and slot region — the same ceiling `rows_per_chunk`
  already sets on holding a chunk open. But it is a read plus, on the way out, a
  rewrite, so a workload that alternates between two far-apart chunks pays for
  both on every alternation. Append-only streaming, the primary workload, never
  takes this path at all.
* **A rewrite is not crash-atomic**, and what that costs depends on your
  integrity policy. It overwrites the only copy, and the write is not `fsync`ed
  until the next `sync`, so power loss before that, a `write_all` that fails
  part-way on `ENOSPC`/`EIO`, or a process crash can each leave the file holding
  a mix of the old chunk and the new one. The difference from an append is
  **recoverability**: a torn append lands past the committed end and open
  truncates it, leaving already-readable data untouched, while a torn rewrite
  mixes data that was already committed.

  Under `IntegrityPolicy::Crc32` or `Crc32WithHeader` a torn cell is *detected*
  — the per-cell checksum is stored beside the cell, and reading one fails with
  `ChecksumMismatch`. Under **`IntegrityPolicy::None` nothing detects it**: the
  record checksum is a constant zero and the chunk carries no per-cell table, so
  the mixed cells are served as ordinary values. If you enable chunk editing on
  data you cannot re-derive, declare an integrity policy.

  The blast radius is bounded to cell values inside that one chunk. The rewrite
  is byte-length-identical and in place, so record framing, the offset chains,
  the index and every other record are untouched: the file still opens and still
  parses, with some cells at their previous values. This is the bargain the
  matrix region has always made, and editing a written chunk is opting into it
  for chunked rows.

`Error::MatrixChunkNotReopenable { chunk, reason }` is the new refusal, and it
fires only where the rewrite is impossible because the payload length would
change:

| `reason` | when |
| --- | --- |
| `"chunk compression"` | the format declares `chunk_compression`, so a chunk's payload length is a function of its contents |
| `"segment_on_flush"` | the same formats `replace_fixed` already refuses in-place replacement for |

Both are off by default, so a format that declares neither never sees this
error. Reads of such a chunk are unaffected; only modification is refused.

`clear_matrix_cell_by_category(category, key)` changed with the rest of them.
It clears **one cell**, exactly as `clear_matrix_cell::<T>(key)` does — the only
difference is that the block is named by category string instead of by Rust type
— so the two accept and refuse the same things, and a test pins that. An earlier
draft of this note claimed it walks a whole category and was therefore left
refusing; that was a confusion with `clear_matrix_category` below, and it made
the same cell clearable under one spelling and not the other.

`clear_matrix_category(category)` — the bulk one, no key, returns how many cells
it cleared — reaches written chunks too, and its count includes them. A draft of
this note said it was left refusing because reaching them costs a reload and a
rewrite per chunk; that is what clearing a category *is*, not a reason to refuse
it. The version before 0.6.0 cleared the matrix region only and returned a count
that omitted the chunked rows it had silently left committed, and a refusal
replaced that; this replaces the refusal.

Two properties worth knowing before you call it on a large file:

* **Cost is proportional to the written chunks holding the category** — one
  reload and one record rewrite each — while memory stays one chunk at a time.
  A growing matrix that has not written a chunk yet is unchanged: a memset over
  a buffer, no I/O.
* **It is not atomic, and idempotence is what replaces that.** Each chunk is its
  own record write, so the fifth can fail after four have landed. Clearing an
  already-clear category clears nothing and counts nothing, so the answer to a
  failure part-way is to call it again; the two counts sum to the total. A torn
  write inside a single record poisons the handle, as on every other write path.

It refuses with `MatrixChunkNotReopenable` for the same two formats as above
(`chunk_compression`, `segment_on_flush`), and asks that question **before**
clearing anything, so such a format gets a refusal rather than a region that has
been cleared and chunks that have not.

### C.3 `Error::MatrixChunkSealed` is now `Error::MatrixChunkClosed`

Same fields (`chunk`, `open`), same meaning, honest name. Match arms and any
`matches!` on the variant need the new spelling; nothing else changes.

The old name said the chunk had been written out as a record. That is true only
when the chunk held a committed cell — a chunk the writer moved past with
nothing committed is dropped without any record ever existing, and the error
still fired. Anyone debugging that case went looking through the file for a
record that was never there. What the refusal means is that the chunk is closed
to writes, which is true either way, so that is what it now says. The `Display`
text changed with it: "matrix chunk 1 is sealed" is now "matrix chunk 1 is
closed".

The same reasoning renamed the internals this error reports on —
`seal_open_chunk` is `write_open_chunk_record`, and the surrounding prose says
"written" rather than "sealed" — because the word bundled two things that are
not the same: making a chunk durable, and closing it to further writes. Only
the first is what the operation does; the second is a consequence of records
being write-once today, not a property of the data model. The matrix region
itself is rewritten in place all the time.

## B. From 0.5.0 to 0.6.0: two changed signatures, one struct field, and changed behaviour at unchanged signatures

### B.0 The two changed signatures, and the new field

The two signatures come from the same change — the reader stopped keeping a copy
of the file's index — and are source-level only. The field is
`IndexPolicy::open_digest_on_flush` (B.4), which breaks struct-literal
construction and nothing else.

**No on-disk byte changed for a format that adds nothing.** The 0.6.0 round
does add one new on-disk record — the open digest, internal block id
`0xFFFF_FFF5` — but only for a format that declares
`open_digest_on_flush`, which is off by default. A spec that leaves it off
writes byte-identical files and hashes to the same `computed_schema_hash()` as
before. A reader whose spec does not declare the digest indexes it as
an internal record at a reserved block id, exactly as it indexes a segment
record: present in `scan()` and `index_entries()`, invisible to `blocks::<T>()`.
Measured — the same file indexed by an aware and an unaware reader produces the
identical entry list.

### B.1 `scan()` yields `Result<BlockEvent>`

```rust
// before
pub fn scan(&self) -> impl Iterator<Item = BlockEvent> + '_

// after
pub fn scan(&self) -> impl Iterator<Item = Result<BlockEvent>> + '_
```

On `VarveFile`, `VarveReader` and the generated readers.

```rust
// before
let ids: Vec<_> = file.scan().map(|event| event.block_id).collect();
assert_eq!(file.scan().count(), records);

// after
let ids = file.scan().map(|e| Ok(e?.block_id)).collect::<Result<Vec<_>>>()?;
assert_eq!(file.scan().collect::<Result<Vec<_>>>()?.len(), records);
```

**Why it could not stay.** Producing an event now means producing the index
entry behind it, and that is a read of a file. The two ways to hide that were to
materialise the whole scan into a `Vec` first — the eager load the change exists
to remove — or to end the iterator on a read error, which reports a truncated
file as a short one. Neither is acceptable, so the `Result` is on each item and
nothing is read until the item is asked for.

### B.2 `LayoutReader::segments()` yields owned segments

```rust
// before
pub fn segments(&self) -> &[LayoutSegmentInfo]

// after
pub fn segment_count(&self) -> usize
pub fn segment(&self, index: usize) -> Result<Option<LayoutSegmentInfo>>
pub fn segments(&self) -> impl Iterator<Item = Result<LayoutSegmentInfo>> + '_
pub fn segments_into(&self, out: &mut Vec<LayoutSegmentInfo>) -> Result<()>
```

```rust
// before
assert_eq!(reader.segments().len(), 5);
let name = reader.segments()[0].name;

// after
assert_eq!(reader.segment_count(), 5);
let name = reader.segment(0)?.expect("segment 0").name;
```

`.segments().len()` happens to still compile — the iterator is `ExactSizeIterator`
— so a mechanical sweep for `[..]` indexing is what finds the breakages.

**Why it could not stay.** A `&[LayoutSegmentInfo]` promises the caller that
every segment exists, contiguously, for as long as the borrow lives. That is the
one shape a reader which rebuilds a segment from the file cannot offer, and it
was what kept the whole array resident.

The generated per-kind accessors are unchanged in shape and gained `_into`
twins.

### B.3 Unchanged signatures, changed behaviour

- **`max_file_len` is inert.** Nothing enforces a file-length ceiling and no
  format is required to declare one. `limits { file_len: .. }` still parses; it
  no longer refuses anything. A format that relied on it must state
  `max_records`, `max_index_bytes`, `max_scan_bytes`,
  `max_record_payload_len` or `max_logical_payload_len` instead.
- **`TrustedUnboundedRequiresExplicitApi` names a different resource.** It used
  to name `file length`, which was the first required limit; it now names
  `record count` (native) or `scan bytes` (layout). The refusal itself is
  unchanged. Match on the variant, not on the string.
- **`diagnose_file` and `inspect_layout_file` surface a refused index.** Both
  previously reported a file refused by `max_index_bytes` as a file with no
  records / no segments.

### B.4 Nothing to migrate, but worth knowing

Everything else is additive: the `_into` family, `open_*_with_scratch`,
`read_payload_into` / `read_logical_payload_into` / `decode_block_into`,
`with_directory` / `RecordDirectory` / `DirectoryRead`,
`open_readonly_without_directory`, `BlockVec::get_into`, and the generated
`_entries_into` / `_decoded_into` / `_into` twins. See
[API Reference](api-reference.md#reads-that-fill-a-buffer-you-own).

So are the two that came after them, and they are the pair worth reading
together:

| New | What it is |
| --- | --- |
| `VarveFile::record_map(&mut Vec<RecordIndexEntry>)` → `RecordMap<'_>` | an index over your buffer, walked only as far as you ask; implements `RecordDirectory` |
| `VarveFile::open_readonly_lazy(spec, path)` | an open that frames one record — the digest — and builds no index |
| `VarveFile::open_readonly_lazy_with_report(..)` → `(Self, LazyOpenSource)` | the same, and which route it took |
| `IndexPolicy::with_open_digest_on_flush(bool)` | writes the digest the lazy open reads |
| `OPEN_DIGEST_BLOCK_ID`, `LazyOpenSource` | the internal block id and the report enum |

`IndexPolicy` gained a public field, `open_digest_on_flush`. It is `pub` on a
non-`#[non_exhaustive]` struct, so **code that constructs an `IndexPolicy` with
a struct literal will not compile** until the field is added; `IndexPolicy::new`
and the `with_*` builders are unaffected, and every associated constant
(`ScanOnOpen`, `SegmentOnFlush`, …) is unchanged. The field defaults to `false`
everywhere, and a file written with it off is byte-identical to one written
before it existed.

See [API Reference](api-reference.md#an-open-that-reads-no-record) for the
measurements and [Format Author Guide](format-author-guide.md) for how to choose
between `segment_on_flush` and `open_digest_on_flush`.

---

## A. From 0.4.0 to 0.5.0

Five changes. §A.1 and §A.2 are representation and composition; **§A.3 and §A.4
are the substantive ones**: residency and verification, which one option used to
conflate, are now two options, and the residency default changed. §A.5 lists what
changed behaviour at an **unchanged signature** — read it even if everything still
compiles.

**What an undeclared open does now.** It reads a header and 8 bytes per live
commit-map page, retains **no** commit-map payload, and can no longer be refused
because a matrix's live set is wider than `max_matrix_bitmap_bytes`. It still
authenticates the whole commit map and still reports commit-map damage at open,
because that work became `MatrixMetadataVerification`, which defaults to running
at open and retains one 4096-byte buffer. Measured on a matrix with one live page:
32 bytes read and 0 pages visited without verification, 69,800 bytes over 17 pages
with it, and 0 resident bitmap bytes either way (was 131,240 bytes read and one
page per live page resident for the session).

### A.1 `MatrixMetadataResidency` gains a `Missing` state and becomes `#[non_exhaustive]`

`MatrixMetadataResidency` had no way to say "nobody declared a policy". That is
the root cause of §A.2, so the fix is a representation change:

```rust
// Before (0.4.0)
pub enum MatrixMetadataResidency {
    EagerVerified,
    Lazy { cache_bytes: u64 },
}

// After (0.5.0)
#[non_exhaustive]
pub enum MatrixMetadataResidency {
    Missing,                      // new, and declared first
    Lazy { cache_bytes: u64 },    // EagerVerified was removed; see A.3
}
```

`Missing` is the `ReadLimit::Missing` idiom, not a second convention: it is the
state an unset field starts in, it never overwrites a policy someone else
declared, and it resolves to varve's own choice.

**Two things break.**

1. **Exhaustive `match`es.** Both the new variant and `#[non_exhaustive]` require
   a wildcard arm. The attribute went on in the same release deliberately —
   adding the variant already broke every exhaustive match, so paying for
   `#[non_exhaustive]` now costs nothing and prevents a second break on the next
   variant. Nothing is on crates.io, so this is the cheap moment.

   ```rust
   // Before (0.4.0)
   match limits.matrix_metadata_residency {
       MatrixMetadataResidency::EagerVerified => eager(),
       MatrixMetadataResidency::Lazy { cache_bytes } => lazy(cache_bytes),
   }

   // After (0.5.0) — match what you care about, wildcard the rest
   match limits.effective_matrix_metadata_residency() {
       MatrixMetadataResidency::Lazy { cache_bytes } => bounded(cache_bytes),
       _ => unreachable_today(),
   }
   ```

2. **The value in the public field changed.** `ReadLimits::STANDARD`,
   `UNTRUSTED`, `MISSING`, `TRUSTED_UNBOUNDED`, `all()`, `finite_all()`,
   `default()` and `FormatSpec::new` now all carry `Missing` where they carried
   `EagerVerified`. This is source-compatible — no signature changed — but any
   code that *compares* the field to `EagerVerified` now sees `Missing` and takes
   the other branch:

   ```rust
   // Before (0.4.0): true
   // After  (0.5.0): FALSE — the field is Missing until someone declares a policy
   limits.matrix_metadata_residency == MatrixMetadataResidency::EagerVerified

   // The fix: resolve the unset state. Never yields Missing.
   limits.effective_matrix_metadata_residency()
   ```

   The representation change of this section is behaviour-preserving on its own;
   what the resolved value *is* changed in §A.3.

**New items, all `const fn` or `const`:**

| Item | What it is |
| --- | --- |
| `MatrixMetadataResidency::DEFAULT` | the one place the default policy lives; `Lazy { DEFAULT_CACHE_BYTES }` since §A.3 |
| `MatrixMetadataResidency::DEFAULT_CACHE_BYTES` | 2 MiB, derived as `ReadLimits::STANDARD.max_matrix_bitmap_bytes / 32`, guarded by two `const` assertions |
| `ReadLimits::effective_matrix_metadata_residency()` | resolves `Missing`; **read this, not the field** |
| `ReadLimits::default_matrix_metadata_cache_bytes()` | `DEFAULT_CACHE_BYTES` clamped down to `max_matrix_bitmap_bytes` |

`pub(crate) MatrixMetadataResidency::cache_bytes()` was removed. It was never
public; the two methods above take its role.

### A.2 `*_with_resource_limits` composes a declared residency policy instead of replacing it

**This is the defect fix, and the only intended user-visible behaviour change in
0.5.0.** [§5.12](#512-_with_resource_limits-replaces-a-spec-declared-matrix-residency-policy)
described this as designed behaviour with a mandatory workaround. It was a bug,
caused by the missing unset state of §A.1, and it is gone.

This is the call that used to lose the policy — note that its only content is a
*larger* bitmap ceiling, and residency is never mentioned:

```rust
// The spec declares Lazy { cache_bytes: 16_384 } and a 16,384-byte bitmap ceiling.
let reader = AppFormat::open_reader_with_resource_limits(
    path,
    ReadLimits::STANDARD.with_max_matrix_bitmap_bytes(32_768),
)?;
```

| | 0.4.0 | 0.5.0 |
| --- | --- | --- |
| Spec's `Lazy { 16_384 }` | **discarded**, reverted to the eager policy | **kept** |
| Result of the call above, 16 live pages | `Err(LimitExceeded { resource: "matrix bitmap bytes", actual: 33536, limit: 32768 })` | `Ok` — 272 bytes read, 0 commit-map pages visited, 16,384 bytes cached |

In 0.4.0 the call was refused **by the very ceiling it was raising**: dropping
`Lazy` reverted the handle to the eager policy, under which
`max_matrix_bitmap_bytes` is an admission limit
([§1.3](known-limitations.md#13-max_matrix_bitmap_bytes-bounds-a-cache-and-the-residual-admission-limit-is-the-index-mirror)).
Nothing reported the discard.

`matrix_metadata_residency` now composes exactly like every ceiling beside it:

| Entry-point family | Format declared a policy, caller silent | Caller declared a policy, format silent | Both declared |
| --- | --- | --- | --- |
| `*_with_resource_limits` (`overlay`) | **format's policy kept** (was: discarded) | caller's policy taken | caller wins |
| `*_with_limits` (`tighten`) | format's policy kept | **caller's policy taken** (was: discarded) | format wins |
| plain `open` / `create` | kept | — | — |

Both changed cells are the same bug class: a silence was being treated as a
declaration. `tighten` keeps the rule that a format's declaration outranks a
runtime one — a policy has no "tighter" direction to meet — but a silence no
longer outranks anything.

**What to do.** If you wrote the §5.12 workaround — carrying
`with_matrix_metadata_residency` in the same `ReadLimits` value you pass to the
entry point — it still works and is still the way to *override* a format's policy.
It is no longer *required* to preserve one. If you avoided
`*_with_resource_limits` because of this bug, you no longer need to.

**No DSL change.** `varve_format!`'s `limits { }` grammar is
`key: <integer literal>;` over a fixed key list, so a policy with a variant
cannot be expressed there and no key was added. Macro-declared formats now emit
`ReadLimits::missing()`, so they carry `Missing` and pick up whatever `DEFAULT`
says. A format author who wants to declare a policy today does it through
`FormatSpec::with_read_limits` / `with_resource_defaults`, which now compose
correctly.

### A.3 `MatrixMetadataResidency::EagerVerified` is removed, and `DEFAULT` is now `Lazy`

**Breaking, deliberate, and the reason §A.4 exists.** The variant is gone from the
enum:

```rust
// Before (0.5.0-dev)
ReadLimits::STANDARD.with_matrix_metadata_residency(MatrixMetadataResidency::EagerVerified)
// After: does not compile. There is no eager residency mode.
```

`EagerVerified` materialised every published commit-map page for the whole session.
That made `max_matrix_bitmap_bytes` an *admission* limit — a matrix whose live set
exceeded it could not be opened at all
([§1.3](known-limitations.md#13-max_matrix_bitmap_bytes-bounds-a-cache-and-the-residual-admission-limit-is-the-index-mirror))
— and it made open cost scale with the file rather than the working set. What kept
it was not its cost: the complete `MatrixRecoveryReport` was a **side effect** of
that load, so removing it would have silently removed the `Recoverable`
commit-map finding, the category quarantine, the recovery recommendations, and the
`RecoveryPolicy::Strict` writer gate.

§A.4 separates those two things, so nothing is left that eager residency provides.
It was **removed rather than kept as an alias** for the bounded cache: a name
promising eager, verified, session-long residency for something that is none of
those is worse than a compile error, and an alias would also have been a second
name for one code path. There is exactly one page-loading path now.

`MatrixMetadataResidency::DEFAULT` is therefore `Lazy { cache_bytes: DEFAULT_CACHE_BYTES }`,
clamped to `max_matrix_bitmap_bytes` by
`ReadLimits::default_matrix_metadata_cache_bytes()`.

**Migration.**

| You had | Do this |
| --- | --- |
| `with_matrix_metadata_residency(EagerVerified)` for **detection at open** | delete it. Verification at open is the default (§A.4) |
| `with_matrix_metadata_residency(EagerVerified)` for a **pinned snapshot** across a whole map | no replacement; coordinate it yourself. Whole-live-set residency was the only mechanism and it is gone |
| `with_matrix_metadata_residency(EagerVerified)` to make `max_matrix_bitmap_bytes` refuse a large open | no replacement. The ceiling now bounds a cache; use `max_matrix_cells` to refuse a large matrix |
| `with_matrix_metadata_residency(Lazy { .. })` | unchanged |
| nothing | you get the bounded cache, and verification still runs at open |

### A.4 New: `MatrixMetadataVerification`, and `verify_matrix_metadata()`

Verification is now its own declared policy, in the same idiom as the residency
one — an unset `Missing` state, composed by `overlay`/`tighten`, resolved in
exactly one place, admitted in exactly one place:

```rust
pub enum MatrixMetadataVerification { Missing, AtOpen, OnDemand }  // #[non_exhaustive]
```

| New item | What it is |
| --- | --- |
| `ReadLimits::matrix_metadata_verification` | the declared policy; `Missing` until someone declares one |
| `ReadLimits::with_matrix_metadata_verification()` | declares it (`const fn`) |
| `ReadLimits::effective_matrix_metadata_verification()` | resolves `Missing`; **read this, not the field** |
| `MatrixMetadataVerification::DEFAULT` | `AtOpen` — one constant, one line to change |
| `VarveReader/VarveWriter/VarveFile::verify_matrix_metadata()` | runs the pass on demand, returns a `MatrixRecoveryReport` |

The pass reads every page of every commit map named by the persisted page index
**unioned with** the platform's allocation map and authenticates each against its
stored digest. That union is what detects stray bytes in a page the matrix never
published. It retains **one reusable 4096-byte page buffer** — nothing it reads
becomes resident — so the behaviour that used to require whole-live-set residency
now costs `O(1)` memory.

`AtOpen` is the default, so **an undeclared format behaves as it did before**: a
damaged matrix announces itself at open, the category is quarantined, the
recommendations are produced, and a strict-recovery writer is refused.
`OnDemand` moves the pass off the open (and skips the allocation-map query
entirely, since nothing would read it); a page faulted in by a read is still
authenticated against its digest under every policy, which is why there is no
`Never`.

`verify_matrix_metadata()` **reports; it does not quarantine.** The fail-closed
gate is derived once, at open, from the findings the layout is assembled with;
nothing may install one afterwards. A caller that needs a damaged category failed
closed reopens with `AtOpen`.

**No DSL change for this either**, for the reason §A.2 gives plus one more: the
policy is reader-side, and a runtime `ReadLimits` passed to
`*_with_resource_limits` now survives composition (§A.2), so a caller of a
macro-declared format can declare `OnDemand` at open without the format
mentioning it.

### A.5 Changed behaviour at an unchanged signature, in 0.5.0

[§5](#5-changed-behaviour-at-an-unchanged-signature) is that section for the
0.3.0-to-0.4.0 step. These are the 0.5.0 entries of the same class: **code that
still compiles and now behaves differently.**

#### A.5.1 `matrix_resume_signal` / `matrix_sidecar_resume_signal` refuse a quarantined category

```rust
// Before (0.4.0): a category whose commit map holds a damaged page
reader.matrix_resume_signal("analysis")?;         // Ok(MatrixResumeSignal::Clean)

// After (0.5.0)
reader.matrix_resume_signal("analysis")?;         // Err(Error::MatrixCommitQuarantined("analysis"))
```

Same change on `matrix_sidecar_resume_signal`, on all three receivers
(`VarveReader`, `VarveWriter`, `VarveFile`). An **unaffected** category is
unchanged.

The old answer was an artifact, not a verdict. Quarantine used to be two things:
the recovery finding, **and** an empty replacement bitmap installed over the
damaged map — so a progress query answered "nothing in progress" for a category
known to be damaged. Nothing is retained now, so there is no map to answer from and
no honest answer to give. A progress figure is an answer like any other and a
quarantined category refuses it.

**What to do:** treat `Error::MatrixCommitQuarantined` from these two calls the way
you already treat it from `matrix_cell_status` — as "this category needs recovery
first". The recovery path is unchanged: `rebuild_matrix_commit_from_crc` or
`clear_matrix_category`, exactly as `MatrixRecoveryReport::recommended_actions`
says.

#### A.5.2 `clear_matrix_category` reports 0 cleared on a quarantined category

`clear_matrix_category` is the *recovery* path for a quarantined category, so it
deliberately does **not** refuse. What it cannot do is count the bits it discards:
those are the damaged ones, and every count now authenticates what it reads. It
returns `Ok(0)`.

This is not a change in the value returned — the empty replacement map made it
report zero before 0.5.0 too — but it was emergent then and is stated in the code
now. It is listed because the *reason* changed, and because a caller who reasoned
about it from the old mechanism was reasoning from something that no longer exists.

#### A.5.3 A whole-map aggregate no longer counts unauthenticated bytes

`matrix_resume_signal` and the recovery report's partial-progress advisory read
pages a demand cache does not hold, and previously did so **without** checking their
stored digests. They now use the same per-page authentication every other read uses,
so a damaged page refuses instead of contributing a number. A caller can therefore
see an error from a progress query where a (wrong) number came back before.

#### A.5.4 An undeclared matrix open behaves differently

Covered above rather than repeated: §A.3 for residency and what an open retains,
§A.4 for verification. **No wire format changed** — no encoder, decoder, header
field or version constant was touched — so a file written by any 0.4.0 or 0.5.0
build reads under either policy, and switching policies is purely a reader-side
decision about when work happens and how much memory it uses.

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
| `format::MatrixMetadataResidency` | `Lazy { cache_bytes: u64 }`. **0.5.0 adds `Missing`** (the unset state) and `#[non_exhaustive]`, and **removes `EagerVerified`**; `Missing` resolves to `DEFAULT`, now `Lazy { DEFAULT_CACHE_BYTES }`. See [§A.1](#a1-matrixmetadataresidency-gains-a-missing-state-and-becomes-non_exhaustive) and [§A.3](#a3-matrixmetadataresidencyeagerverified-is-removed-and-default-is-now-lazy) |
| `format::MatrixMetadataVerification` | **new in 0.5.0.** `Missing` \| `AtOpen` \| `OnDemand`, `#[non_exhaustive]`; `Missing` resolves to `DEFAULT` = `AtOpen`. See [§A.4](#a4-new-matrixmetadataverification-and-verify_matrix_metadata) |
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

> **§5 covers the 0.3.0-to-0.4.0 step only.** The 0.5.0 entries of this same class
> are in [§A.5](#a5-changed-behaviour-at-an-unchanged-signature-in-050) — chiefly
> `matrix_resume_signal` / `matrix_sidecar_resume_signal` returning
> `Err(MatrixCommitQuarantined)` where they returned `Ok(Clean)`. If you are
> upgrading from 0.4.0, read that section as well as this one.

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

> **Fixed in 0.5.0. Do not implement the workaround below as a requirement.**
> This section describes 0.4.0 behaviour and is kept because 0.4.0 shipped it and
> the CHANGELOG links here. It was a bug, not a design, and
> [§A.2](#a2-_with_resource_limits-composes-a-declared-residency-policy-instead-of-replacing-it)
> is the current behaviour: a caller who does not mention residency no longer
> discards the format's policy. Three claims below are now false — the sentence
> beginning "We do not think this is wrong as designed", the table row
> "**replaced** by the passed value, always", and every mention of
> `EagerVerified`, which [§A.3](#a3-matrixmetadataresidencyeagerverified-is-removed-and-default-is-now-lazy)
> removed. Everything else, including the absence of a DSL key, still holds.

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
[Known Limitations §1.6](known-limitations.md#16-the-matrix-residency-and-verification-policies-have-no-varve_format-dsl-key).

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
   matrix residency policy in that same value or lose it. (§5.12 — **no longer
   required as of 0.5.0**, see §A.2)
10. Read [Known Limitations §1](known-limitations.md#1-matrix-opening-a-matrix-is-not-o1-because-opening-it-verifies-it)
    before sizing a matrix workload — in particular §1.5, since
    `max_matrix_cells` defaults to 16,000,000 and a larger matrix fails at
    **create** until you raise it.

Coming from 0.4.0, four more:

11. Delete every mention of `MatrixMetadataResidency::EagerVerified`; add a `_ =>`
    arm to any `match` on `MatrixMetadataResidency`, and read
    `effective_matrix_metadata_residency()` rather than the field. (§A.1, §A.3)
12. Decide whether you want verification at open. The default (`AtOpen`) is what
    0.4.0 did; `OnDemand` plus `verify_matrix_metadata()` is the cheap open, and
    §A.4 states what it does not give you. (§A.4)
13. Handle `Err(Error::MatrixCommitQuarantined)` from `matrix_resume_signal` and
    `matrix_sidecar_resume_signal`. (§A.5.1)
14. If you relied on an eager reader being a pinned whole-map snapshot, coordinate
    that yourself — there is no replacement mechanism. (§A.3)

Nothing in §2 is a checklist item: every removed name either never shipped or was
already gone at 0.3.0. §2 explains why.
