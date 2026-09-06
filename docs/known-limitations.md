# Known Limitations

Status as of 0.5.0. Numbers measured 2026-07-22, re-measured 2026-07-25, and
re-measured again on 2026-07-26 against the residency/verification split described
below; every entry was checked against the code in this repository. Where an earlier
document and the code disagreed, the code won and the document was corrected.

**Corrections made for 0.5.0**, listed first because a reader of the 0.4.0 version
of this file — or of an earlier 0.5.0 draft of it — was told several things that are
no longer true:

- **Matrix commit metadata is now demand-filled by default, and §1.2 and §1.3 used
  to say the opposite.** `MatrixMetadataResidency::EagerVerified` was **removed**
  and `DEFAULT` is `Lazy { DEFAULT_CACHE_BYTES }`. An undeclared open therefore
  retains **no** commit-map payload (measured 0 bytes at 1 and at 64 live pages,
  where it was 4,096 and 262,144), residency afterwards tracks the working set
  exactly with LRU eviction at the declared bound, and
  `max_matrix_bitmap_bytes` bounds a **cache** rather than admitting a whole live
  set. The failure §1.3 used to describe — a matrix whose committed state exceeds
  that ceiling **cannot be opened at all** — is fixed, and both directions of the
  cure are pinned by tests so the section cannot silently go stale again.
- **§1.4 no longer describes a defect, because the defect was fixed rather than
  re-explained.** The 0.5.0 draft of this file said `Lazy` was "opt-in rather than
  the default", called that a known defect, and stated that what blocked the flip
  was a lazy open contributing no commit-map findings to `MatrixRecoveryReport`.
  Both statements are now false. Residency and *verification* were one option and
  are now two: `MatrixMetadataVerification` decides whether pages are
  authenticated at open, defaults to `AtOpen`, and produces the whole report
  independently of how much stays in memory. Nothing blocks the residency default;
  it is `Lazy`.
- **§1.1's subject changed from residency to I/O.** Open still is not `O(1)`, but
  the reason is now that opening a matrix *verifies* it. The old ~131 KB / 32-page
  figure for a 1-live-page matrix is superseded by **69,800 bytes over 17 pages**
  under the verifying default and **32 bytes over 0 pages** with verification
  declared off. Do not size resident metadata from either figure: the verification
  pass retains one reusable 4,096-byte buffer and nothing else.
- **The residual is stated where the improvement is claimed.** Open still reads the
  persisted page index in full — 8 bytes per live page — and mirrors it resident at
  ~96 bytes per live page, so open remains `O(live pages)` and that mirror is now
  the only term that can refuse an open on `max_matrix_bitmap_bytes` (§1.2, §1.3).
- **§1.6 described a bug that is now fixed.** Through 0.4.0,
  `*_with_resource_limits` silently discarded a spec-declared
  `matrix_metadata_residency`. It composes correctly as of 0.5.0, and
  `matrix_metadata_verification` composes identically. The section is retitled
  around what remains a limitation — there is still no `varve_format!` DSL key for
  either policy — and the fixed behaviour is recorded there as history. **Any
  workaround written against the old §1.6 is now unnecessary.**
- **§1.4 gained the per-bitmap correction:** `cache_bytes` bounds each bitmap, not
  each reader, so a format with `n` demand-loaded maps bounds itself at
  `n * cache_bytes`. 0.4.0's rustdoc claimed otherwise. Behaviour unchanged since
  0.4.0; only the claim was wrong. It is carried in §1.2 now.
- **A matrix reader is no longer a snapshot** (§1.4, consequence 3, and §4.2).
  Whole-live-set residency was the only mechanism that ever pinned one instant
  across a whole commit map, and it was removed with no replacement.

The 2026-07-25 corrections below are kept as the record of the previous pass. Two
of them have themselves been superseded by the split above: residency is no longer
"refunded when a mutation clears a page's last bit" as the *only* release path
(read-driven LRU eviction is the primary one now, and the mutation refund coexists
with it), and the "open reads every candidate page but retains only pages holding a
set bit" correction now understates the change — an open retains nothing at all.

Corrections made on 2026-07-25, listed because a reader who saw the earlier
version was told six things that were wrong: open **reads** every candidate page
but retains only pages holding a set bit (§1.1), residency *is* refunded when a
mutation clears a page's last bit (§1.2), the page-index term is 96 bytes per live
page rather than 48 (§1.3), `IndexPolicy::CheckpointOnFlush` does **not** let
an open start from a checkpoint (§2.1 — that was the only mitigation previously
offered for resident open cost, and it does not exist), the `max_matrix_crc_bytes`
resource string is `"matrix checksum bytes"` rather than `"matrix CRC bytes"`
(§1.5), and `matrix_cell_status` was **not** relaxed to `&self` by this release —
it was already `&self` in 0.3.0, so three matrix read entry points changed
receiver, not four (§4.1). Five things were missing
entirely and are now §1.5 (the default 16,000,000-cell ceiling), §1.6 (a residency
policy silently discarded by `*_with_resource_limits`), §1.7 (hole-punch
dependence), §2.4 (uncommitted-tail truncation) and §6.7/§6.8 (no old artifact is
tested; platform support).

Also corrected on 2026-07-25, because the citations in this file changed shape:
the project's internal working documents — the adversarial review reports, the
contributor invariant checklist, the architecture, performance, requirements and
design notes, the fuzz/fault-injection and test-hygiene records, the
channel-selective-access design study, and the routing/draft/validation artifacts
of the hardening rounds — have been moved out of `docs/` and are not published, so
this file no longer cites them. Every such citation was replaced by the substance
it was citing: the platform cost model in §1.7 is now stated in place rather than
pointed at, §5.1 states what the channel-access study establishes instead of
naming its files, and §6.4 carries the fuzz evidence dates and target counts
directly. **No limitation, measured number or caveat was removed in that
pass; only the pointers changed.** Where an entry below still says "open item
N", that is the internal checklist's tracking number, kept so that this file and
the changelog refer to the same item by the same number — the item's substance is
stated here in full, and nothing in this document depends on being able to read
that checklist.

This file is organised by what a **user** runs into, not by internal structure.
Each entry states what it is, who it affects, the workaround if one exists, and
whether it is planned work.

Companion documents:

- [API Changes](api-changes.md) — what to edit when upgrading, and what happens
  to files written by an older version.
- [Changelog](../CHANGELOG.md) — the release entry.

The consciously accepted *internal* items — the ones a downstream crate cannot
express, previously listed in a separate contributor checklist — are carried in
§9 of this file; the ones a user can actually reach are in §1-§8 with everything
else.

---

## 1. Matrix: opening a matrix is not O(1), because opening it verifies it

Residency is no longer part of this section's problem. As of 0.5.0 matrix commit
metadata is demand-filled and bounded by a declared cache — §1.2 and §1.3, which
used to record the opposite, now record what is left. What remains is I/O: the
default open **authenticates the whole commit map**, and that read is
`O(candidate pages)`. It is a declared policy with an off switch, and the numbers
below are what it costs.

### 1.1 Open cost is proportional to candidate pages, not to the working set

**What it is.** A matrix's commit metadata is stored as 4096-byte bitmap pages,
each page covering 32,768 cells of one category. Under the default
`MatrixMetadataVerification::AtOpen` policy, `open` **reads and authenticates**
every page in the *candidate set*: the union of the pages named by the persisted
page index (`L`, the pages actually holding state) and the pages the platform's
allocation map reports as written (`A`). Cost is `O(L + A)` bytes read.

**Reading is not retaining, and since 0.5.0 nothing is retained.** The
verification pass reads each candidate page into a single reusable 4096-byte
buffer, authenticates it against its stored page digest, folds the finding, and
moves on. So the I/O is `O(L + A)` and the residency contribution is **zero** — do
not size resident metadata from the bytes read, and do not expect the reads to
bound memory in either direction.

`O(L + A)` is not `O(1)`, and neither `L` nor `A` is the working set. A reader
that intends to touch one cell and leaves verification on pays for the whole
candidate set in I/O.

**Measured on this host, 2026-07-26** (`crates/varve/tests/matrix_lazy_residency.rs`
and `matrix_integrity_scaling.rs`, all tests passing):

| Fixture | Cells | Live pages | Bytes read, `AtOpen` | Pages visited | Bytes read, `OnDemand` | Resident bitmap bytes |
| --- | --- | --- | --- | --- | --- | --- |
| small | 2,097,152 | 1 | 69,800 | 17 | **32** | 0 |
| large | 8,388,608 | 1 | 69,800 | 17 | **32** | 0 |
| large | 8,388,608 | 64 | 267,800 | 65 | **1,040** | 0 |

Read those rows in this order:

1. **Open is independent of file size.** Two matrices differing 4x in cell count
   and 4x in file size, with the same one-page live set, read the same 69,800
   bytes. This part of the design goal is met — **on a sparse NTFS volume.** See
   the platform precondition below, which is not a footnote.
2. **Open is not O(1) while it verifies.** A matrix with **one live page** still
   reads **69,800 bytes** and visits **17 pages**. On NTFS the allocation map
   reports the bitmap region in ~128 KiB runs, so `A` is tens of pages even when
   `L` is 1. That is a floor, not an average.
3. **The whole of it is verification.** The same opens under
   `MatrixMetadataVerification::OnDemand` read **32 bytes** and **1,040 bytes**
   and visit **0** pages: an 8-byte page-index entry per live page, and no
   allocation-map query at all. The gap between those two columns is the price of
   detecting commit-map damage at open, and it is a one-line declaration.
4. **Residency is zero in every row**, at any live-page count, under either
   policy. That column used to read `8,192 * live_pages`.

#### The numbers above are Windows/NTFS numbers, and `A` is filesystem-defined

This matters enough to state before the mitigations, because on two of the three
cases below the headline result is *different*, not merely less precise.

- **Windows and Linux, sparse volume (measured case).** `A` comes from
  `FSCTL_QUERY_ALLOCATED_RANGES` / `SEEK_HOLE`. On NTFS the ~128 KiB run
  granularity gives tens of candidate pages around a single live page, which is
  where the ~70 KB floor comes from. Linux extent granularity differs, so the
  constant differs; the shape does not.
- **Every other target.** `query_allocated_extents` returns `None`
  (`matrix.rs`, `#[cfg(not(any(windows, target_os = "linux")))]`), so the
  candidate set has **no `A` term at all** and verification visits `L` pages only.
  Open is *cheaper* there than the table above shows. It is also blinder: the `A`
  term is the only thing that detects stray bytes in a page nothing ever
  published. This is untested — see §6.1.
- **A volume that is not sparse.** Sparseness is requested best-effort. On
  Windows the file must carry the sparse attribute *before* it is extended, and
  `mark_file_sparse` **ignores failure** by design ("the matrix is then simply
  dense, which costs performance and nothing else"). On exFAT/FAT32, some
  network and virtual volumes, or after a copy by a tool that expands holes, the
  declared extent is genuinely allocated — the allocation map then reports the
  **whole** bitmap region as data, every page becomes a candidate, and a verifying
  open degrades to `Theta(declared cells / 8)` of reads. **On a non-sparse volume,
  open cost is proportional to the declared cell count, not to live state, and the
  "independent of file size" result in point 1 does not hold.** There is no error
  and no warning; the only signal is that open is slow and reads a lot. Memory is
  unaffected — the pass still retains one page.

The same precondition governs the create-time claim: metadata I/O at create is
bounded by live state because the unwritten extent is a hole. Where it is not a
hole, `set_len` allocates what it reserves.

**Who it affects.** Anyone who opens matrix files frequently — short-lived
processes, request handlers, CLI invocations, per-file workers — or who opens
many matrices in one process. A long-running process that opens one matrix and
keeps it open pays this once and does not care.

**Workaround.** Three, and the first is complete:

- Declare `MatrixMetadataVerification::OnDemand`, and call
  `verify_matrix_metadata()` on whatever schedule suits you — once per file, on a
  scrub, never. Read §1.4 first: what you give up is the matrix *announcing*
  damage at open, the whole-category quarantine, and the strict-recovery writer
  gate.
- Keep the handle open. Matrix reads take `&self` (§4), so one handle can serve
  the whole process, including concurrent threads.
- Nothing needs doing about memory: an open retains no commit-map payload.

**Planned.** No further reduction of the verifying open is scheduled; a pass that
did not read the candidate set would not be a verification. The residual cost of a
*non*-verifying open is the persisted page index — 8 bytes per live page
(`PAGE_INDEX_ENTRY_LEN`), enumerated into a resident map at open
(`SparseBitmap::indexed_pages`) — so it is `O(live pages)` too, 512x smaller per
entry than the 4,096-byte page a verifying open reads for the same entry, but not
gone. Making that index demand-paged would mean making it searchable on disk
rather than enumerated, which would also put §1.4's "not cached is not the same as
not published" guarantee behind an I/O, since that guarantee rests on the index
being complete in memory. It is a different structure, not a tuning change
(open item 30).

### 1.2 What residency still costs: the page-index mirror, and a per-bitmap cache bound

> **This section used to say the opposite.** Through 0.5.0-dev residency was fixed
> at open by the live page count, reading released nothing, and
> `max_matrix_bitmap_bytes` could not cap a cache. That was the `EagerVerified`
> policy, which was removed — see
> [API Changes §A.3](api-changes.md#a3-matrixmetadataresidencyeagerverified-is-removed-and-default-is-now-lazy).

**What it is.** Commit-map payload residency now tracks the working set exactly:
zero after open, one 4096-byte page per distinct commit-map page addressed,
evicted least-recently-used at the declared bound. Measured at 16 live pages:
`after_open = 0`, after touching 1 page `4,096`, after touching 4 pages `16,384`;
and at 64 live pages under the derived 2 MiB default, `262,144` — 12.5% of the
bound, so nothing was evicted.

Three costs remain, and they are the honest content of this section:

1. **The persisted page-index mirror is `O(live pages)` and is not evictable.**
   Roughly **96 bytes per live commit-map page** with checksums on: 48 in the
   commit map's index and 48 in the CRC-validity map's
   (`PAGE_INDEX_SLOT_RESIDENT_BYTES` 16 + `PAGE_INDEX_MAP_RESIDENT_BYTES` 32).
   Measured `matrix_resident_page_index_bytes()`: **96 bytes at 1 live page**,
   **384 at 4**, **1,536 at 16**. This is what makes "this page was never
   published" answerable without I/O, so it cannot be dropped without changing
   what a read may conclude from an absent page (§1.4, consequence 2).
2. **`cache_bytes` is a per-bitmap bound, not a per-reader one.** Each commit
   category and each block validity map owns a cache of its own, so a format with
   `n` demand-loaded maps bounds itself at `n * cache_bytes`. The bound is still
   independent of the file's size. 0.4.0's rustdoc claimed "at most `cache_bytes`
   ... for the session", which was wrong; 0.5.0 corrected the wording rather than
   the behaviour. Whether the bound should instead be shared across a reader's
   bitmaps is open.
3. **Eviction is read-driven now, but a mutation still refunds eagerly.** When a
   mutation clears a page's last set bit that page is released and its bytes are
   refunded (`release_resident_bitmap` / `settle_page_delta`; PERF-02, asserted
   over four set/clear cycles in
   `crates/varve/tests/matrix_integrity_scaling.rs`). The two mechanisms coexist
   and neither can be turned off.

**Who it affects.** Anyone whose live page count is large relative to their
process memory budget — one million live pages is ~96 MB of index mirror before
any payload — and anyone sizing `max_matrix_bitmap_bytes` from the payload alone.

**Planned.** The index mirror is open item 30; see §1.1's "Planned".

### 1.3 `max_matrix_bitmap_bytes` bounds a cache, and the residual admission limit is the index mirror

> **This section used to describe an unopenable matrix.** Under the removed
> `EagerVerified` policy the ceiling was an admission limit on a matrix's whole
> live set, so a matrix whose committed state exceeded the configured ceiling
> **could not be opened at all**. That is fixed: the ceiling bounds the demand
> cache, and a live set wider than it opens and is served.
> `matrix_lazy_residency.rs::a_live_set_over_the_bitmap_ceiling_opens_under_the_derived_default_cache`
> and `matrix_integrity_scaling.rs::measured_resident_bitmap_bytes_against_the_ceiling`
> assert the cure, so this section cannot silently go stale in either direction.

**What it is.** `max_matrix_bitmap_bytes` binds in three places, and only the
third can still refuse an open:

1. **The derived default cache** is `MatrixMetadataResidency::DEFAULT_CACHE_BYTES`
   (2 MiB) **clamped down** to this ceiling, with a one-page floor
   (`ReadLimits::default_matrix_metadata_cache_bytes`). A tightened ceiling
   therefore means a smaller cache, never a failed open.
2. **A declared `cache_bytes` above the ceiling is refused**, because a cache
   larger than a declared memory ceiling would be a way to raise it. The asymmetry
   with (1) is deliberate: a caller's own contradiction is theirs to resolve, and a
   default is never the reason an open fails.
3. **The page-index mirror is charged against it as the index is enumerated.**
   That is the one term an open still makes resident, so a ceiling below
   `~96 bytes * live_pages` refuses the open with
   `Error::LimitExceeded { resource: "matrix bitmap bytes", .. }`. At the 64 MiB
   `STANDARD` default that is roughly 700,000 live pages; a caller who tightens
   the ceiling to bound the *cache* should keep the mirror in mind.

**Arithmetic you can apply to your own matrix**, for one category, with
`IntegrityPolicy::Crc32`:

```text
cells_per_page = 32,768
live_pages     = number of distinct 32,768-cell windows containing >= 1 committed cell
resident_bytes ~=   96 * live_pages          # page-index mirror, in *two* bitmaps
                + min(cache_bytes, 4,096 * pages_actually_touched)
```

With checksums off there is no validity bitmap, so the mirror term halves to about
48 bytes per live page.

**Who it affects.** Anyone with a very large live page count under a tightened
ceiling. The failure mode this section used to carry — a scattered matrix with a
few thousand committed cells becoming unopenable — is gone.

**Planned.** No change to the admission semantics of the mirror. Removing it means
making the page index searchable on disk (open item 30, §1.1).

### 1.4 `Lazy` is the default; verification is what still happens at open

**As of 0.5.0 an undeclared open is
`MatrixMetadataResidency::Lazy { DEFAULT_CACHE_BYTES }` plus
`MatrixMetadataVerification::AtOpen`.** The project's standing policy — open reads
a header, pages fault in on demand, memory is bounded by the working set and never
by total file content — is met for residency. What is *not* bounded by the working
set is the verification read (§1.1), and that is now a separate declaration
rather than a consequence of how much is kept in memory.

Numbers below are measured on Windows/NTFS by `matrix_lazy_residency.rs`. The left
column is what a user who has never heard of either option gets.

| | undeclared (`Lazy` + `AtOpen`) | `Lazy` + `OnDemand` |
| --- | --- | --- |
| Open bytes read, 1 live page | 69,800 | **32** |
| Commit-map pages visited at open, 1 live page | 17 | **0** |
| Open bytes read, 64 live pages | 267,800 | **1,040** |
| Commit-map pages visited at open, 64 live pages | 65 | **0** |
| Resident bitmap bytes after open | **0** | **0** |
| Resident after touching 4 distinct live pages | 16,384 (exactly 4 x 4,096) | same |
| Resident after touching all 64 live pages | 262,144 (exactly 64 x 4,096) under the 2 MiB derived cache — 12.5% of it, so nothing was evicted; 16,384 under a declared 16,384-byte cache, an exact LRU cap | same |
| Eviction | LRU, to the declared bound | same |
| Allocation-map query | yes (where the platform has one) | never |
| Commit-map damage reported by | `open` | `verify_matrix_metadata()`, when called |
| Damaged category failed closed as a whole | yes (`Error::MatrixCommitQuarantined`) | **no** — only the pages a read touches refuse |
| `RecoveryPolicy::Strict` writer gated on a damaged category | yes | **no** |
| A live set over `max_matrix_bitmap_bytes` | opens | opens |

**What you give up with `OnDemand`, stated as the loss it is.** The commit-map half
of `MatrixRecoveryReport` is produced by the verification pass and by nothing else:

- the `Recoverable` `MatrixCorruptionKind::CommitMap` finding is not produced;
- `Error::MatrixCommitQuarantined` does not fire, so a damaged category is not
  failed closed **as a whole** — measured: with one damaged commit-map page a
  verifying open refuses a *clean* page in that category with
  `MatrixCommitQuarantined("analysis")`, while a non-verifying open answers that
  clean page `Ok(Committed)` and refuses only the damaged one;
- the `RebuildCommitMap` / `ClearCategory` recommendations are absent, and with
  them the documented rebuild-recovery path;
- a `RecoveryPolicy::Strict` **writer** is not stopped from mutating a category
  whose commit map holds a damaged page;
- and in one case it is worse than deferral: the candidate set is the persisted
  page index **unioned with** the platform's allocation map, and it is that second
  term which catches stray bytes in a page nothing ever published. Such damage is
  never examined at all, not merely examined later.

`verify_matrix_metadata()` recovers the first three on demand — it is the same
pass, and it reports the same findings and the same recommendations — but not the
quarantine and not the writer gate, which are derived once, at open, from the
findings the layout is assembled with. **No read answers from unverified bytes
under any policy**, so none of this is a soundness hole: every page faulted in is
authenticated against its stored digest before a bit of it is reported, and so is
every page an aggregate count reads.

**What the default does not fix.** Open still reads the **persisted page index in
full**. That is 8 bytes per live page — 512x smaller than a page — but still
`O(live pages)`, not `O(1)`. At one million live pages that is 8 MB read and
~96 MB of resident mirror (§1.2) before a single cell is addressed.

**Three consequences of demand residency that are part of the declaration, not
defects:**

1. **A page is authenticated at fault-in.** A page whose bytes disagree with its
   stored digest is reported as `Error::MatrixFatalCorruption` by the read that
   touches it. That is a per-page refusal, not a verdict on the map; the verdict is
   `MatrixMetadataVerification`.
2. **A page absent from the persisted index still reads as clear.** That is a
   fact the file supplied, not a guess: the index is loaded in full at open and is
   authoritative. "Not cached" and "not published" stay distinct, verified across
   cache eviction.
3. **A reader does not see one consistent instant across a whole map.** A page's
   contents are as of the first touch that faulted it in, not as of open. The only
   mechanism that ever provided a pinned whole-map snapshot was whole-live-set
   residency, and it was removed; a reader that needs one must coordinate it.

### 1.5 The default limits cap a matrix at 16,000,000 cells — create fails above it

**What it is.** This is the first thing a multi-GB-matrix workload hits, and it
hits it at *create*, before any of the open-cost discussion above applies.

`ReadLimits::STANDARD` — the profile an ordinary `create`/`open` resolves to when
a format declares no `limits { }` block — sets:

| Limit | Default | Resource string in the error |
| --- | --- | --- |
| `max_matrix_cells` | **16,000,000** | `"matrix cells"` |
| `max_matrix_dimension` | **16,000,000** | `"matrix dimension"` |
| `max_matrix_slot_region_len` | 8 GiB | `"matrix slot region length"` |
| `max_matrix_bitmap_bytes` | 64 MiB | `"matrix bitmap bytes"` (§1.3) |
| `max_matrix_crc_bytes` | 128 MiB | `"matrix checksum bytes"` |
| `max_matrix_metadata_bytes` | 256 MiB | `"matrix metadata bytes"` |

A 4096 x 4096 matrix (16,777,216 cells) is therefore **refused by default** with
`Error::LimitExceeded { resource: "matrix cells", .. }`.

**The cell count is checked twice: per matrix block, and as a running aggregate
across every matrix block in the format.** A format with four 5,000,000-cell
matrix blocks fails on the aggregate even though no single block exceeds the
ceiling. Splitting one large matrix into several smaller ones does not evade the
limit.

`max_matrix_slot_region_len` is the second ceiling, and it aggregates the same
way: it binds on the **sum** of `cells * slot_stride` over every matrix block.
At the 8 GiB default a single 16,000,000-cell block is capped at a 536-byte slot
stride, and two such blocks at 268 bytes each.

**Who it affects.** Everyone declaring a matrix larger than about 4000 x 4000,
which is most of the workloads a preallocated matrix is attractive for. The
limits are runtime policy rather than a wire-format ceiling, so the file itself
imposes nothing.

**Workaround.** Raise them explicitly. They are not a safety property of the
format — `UNTRUSTED` inherits the same matrix values — so raising them for data
you produced is the intended use:

```rust
// per handle
let limits = ReadLimits::STANDARD
    .with_max_matrix_cells(4_000_000_000)
    .with_max_matrix_dimension(2_000_000);
let writer = AppFormat::create_writer_with_resource_limits(path, limits)?;
```

or as a format default in the declaration:

```text
limits {
    matrix_cells: 4_000_000_000;
    matrix_dimension: 2_000_000;
}
```

See §1.6 for how a spec-declared residency policy composes with the
`*_with_resource_limits` form.

**Planned.** No change. A default that admits an unbounded declared extent is not
wanted; the defaults are documented here instead.

### 1.6 The matrix residency and verification policies have no `varve_format!` DSL key

> **The bug this section used to describe was fixed in 0.5.0.** Through 0.4.0,
> `*_with_resource_limits` **silently discarded** a spec-declared
> `matrix_metadata_residency`, because `MatrixMetadataResidency` had no unset
> state and `ReadLimits::STANDARD` therefore carried a real eager declaration that
> `overlay` took outright. A caller raising an unrelated bitmap ceiling lost the
> only bound on matrix metadata memory (§1.4) and could then be refused at open by
> the very admission limit they were raising (§1.3) — measured, pre-fix:
> `LimitExceeded { resource: "matrix bitmap bytes", actual: 33536, limit: 32768 }`.
> **If you wrote a workaround for that, it is now unnecessary** (still harmless).
> See [API Changes §A.2](api-changes.md#a2-_with_resource_limits-composes-a-declared-residency-policy-instead-of-replacing-it).

**What it is.** `matrix_metadata_residency` — and, since 0.5.0,
`matrix_metadata_verification` — compose like every ceiling beside them: a silence
never overwrites a declaration.

| Entry point | Format declared, caller silent | Caller declared, format silent | Both declared |
| --- | --- | --- | --- |
| `*_with_resource_limits` (`overlay`) | format's policy **kept** | caller's policy taken | caller wins |
| `*_with_limits` (`tighten`, legacy fieldwise) | format's policy kept | caller's policy **taken** | format wins |
| plain `open`/`create` | kept | — | — |

`tighten` still lets a format's declaration outrank a runtime one, because a
policy has no "tighter" direction to meet. What changed in 0.5.0 is that a
*silence* no longer outranks anything, in either function, for either policy.

**What remains a limitation.** There is **no `varve_format!` DSL key** for either
policy. `limits { }` accepts `key_index`, `disk_index_plan` and `keyed_tail`, but
not these two, and no key was added: the grammar is `key: <integer literal>;` over
a fixed key list, which cannot express a variant, with or without a payload. Both
are reachable two ways — from a spec, through `FormatSpec::with_read_limits` /
`with_resource_defaults`, or at a call site by passing them in the same
`ReadLimits` value you hand to the entry point:

```rust
let limits = ReadLimits::STANDARD
    .with_matrix_metadata_residency(MatrixMetadataResidency::Lazy { cache_bytes: 1 << 20 })
    .with_matrix_metadata_verification(MatrixMetadataVerification::OnDemand)
    .with_max_matrix_bitmap_bytes(n);
```

As of 0.5.0 the settings no longer have to travel together — raising the ceiling
alone will not destroy a spec-declared policy — but a `ReadLimits` value is still
the only place a *call site* can name one. Both are reader-side policies, so the
call site is a legitimate home for them: nothing about the file depends on either.

**Who it affects.** Anyone who wants these declared in the `varve_format!` macro
rather than in Rust. Macro-declared formats emit `ReadLimits::missing()`, so they
carry neither policy and inherit `MatrixMetadataResidency::DEFAULT` and
`MatrixMetadataVerification::DEFAULT` (§1.4).

### 1.7 Clearing a category costs `Theta(cells / 8)` unless the platform can punch holes

**What it is.** `clear_category` and a whole-map page-index rebuild remove the
byte range rather than writing it, which is `O(1)` in the range length — but only
where the platform supports it. `punch_zero_range_native` is implemented for
Windows (`FSCTL_SET_ZERO_DATA`) and Linux (`FALLOC_FL_PUNCH_HOLE`) and returns
`false` on **every other target**. Every failed attempt, and every unsupported
target, streams `len` zero bytes instead — for a whole-category clear that is
`Theta(declared cells / 8)` bytes written.

On macOS, and on any Windows/Linux filesystem without sparse support, clearing a
16,000,000-cell category writes 2 MB where the supported path writes nothing.
Scale that to the cell counts §1.5 lets you raise the ceiling to and it is the
difference between an instant operation and a multi-gigabyte write.

**Who it affects.** Anyone calling `clear_category`, or triggering a whole-map
page-index rebuild, on a non-Windows/Linux target or a non-sparse volume.

**Workaround.** There is no fallback that avoids the write. There *is* detection:
the outcome is recorded unconditionally and neither counter is feature-gated. Two
scope rules decide whether a reading means anything, and both will otherwise be
read as a clean bill of health:

- **A clear issues several range requests** (validity bitmap, page indexes, page
  digests, commit map), and
  `MatrixRecoveryReport::matrix_last_zero_range_streamed_bytes` reports only the
  last of them, so `0` from it does not qualify the whole clear. Take
  `matrix_total_zero_range_streamed_bytes` before and after instead; a nonzero
  delta is exact proof that some range streamed.
- **Both counters are thread-local.** Sample them on the thread that ran the
  clear. A clear performed on a worker thread reads as `0` from anywhere else.

`matrix_sparse_zeroing_supported()` reports the compile-time platform capability
only and is not sufficient on its own. Check the counters rather than inferring
from the target triple.

**Planned.** No additional platform is scheduled. The cost model is stated above
in full — `O(1)` where `punch_zero_range_native` succeeds, `Theta(declared cells
/ 8)` bytes written everywhere else — because this file is the one organised by
what a user hits.

### 1.8 Matrix dimensions are fixed at create time, with no grow path

**What it is.** A matrix's dimensions are supplied to
`create_with_dims` / `create_new_with_dims` and are part of the created layout.
There is no `grow`, `resize`, or `extend` operation anywhere in the public API.
A matrix that runs out of rows cannot be enlarged; it must be recreated and its
contents copied.

**Who it affects.** Anyone modelling an open-ended stream — a growing acquisition,
an append-only log of scans, anything whose extent is not known when the file is
created. **A matrix cannot represent an indefinitely growing stream.**

**Workaround.** Use the append-log APIs for unbounded growth:

- Resident: `VarveFile` / generated typed writers, for data that fits in RAM.
- Scalable (`high-cardinality-dev`): `VarveStreamWriter` / `VarveIndexedWriter`
  and their readers, for bounded-memory ingest and point lookup over data far
  larger than RAM. See [Scalable I/O](scalable-io.md).

**Over-provisioning is the answer for a dimension whose extent you can bound,
and it is close to free.** Declare the ceiling, not the expectation. The region
is a hole: `create_layout` marks the file sparse, writes only the fixed-size
descriptor tables, and establishes the commit map, slot region, bitmaps and
checksum table with one `set_len`. Nothing scales with the declared cell count.

**Measured on this host, 2026-08-04**, ext4, `rustc 1.95` debug build. Each row
creates the matrix, writes and commits one cell at the **far end** of the scan
dimension, then reopens:

| declared | cells | apparent size | **allocated on disk** | create | open |
| --- | --- | --- | --- | --- | --- |
| 1,000 x 128 | 128,000 | 1.06 MB | 28,672 B | 0.5 ms | 0.2 ms |
| 10,000 x 128 | 1,280,000 | 10.6 MB | 24,576 B | 0.3 ms | 0.2 ms |
| 1,000,000 x 128 | 128,000,000 | 1.06 GB | 36,864 B | 0.3 ms | 0.2 ms |
| **100,000,000 x 128** | **12,800,000,000** | **105.6 GB** | **40,960 B** | **0.2 ms** | **0.2 ms** |

A hundred million scans costs 40 KB and 0.2 ms more than a thousand does. So
"the dimension is fixed at create" is a limit on *knowing a ceiling*, not a
limit on size — pick a number you will not reach and the cost of being wrong by
five orders of magnitude is four pages.

Three things still bound it, and only the second is likely to stop you:

- The table above is **ext4, with holes**. On a filesystem that does not give
  you them, `set_len` allocates the whole declared extent — see the precondition
  in §1.1. That turns row four from 40 KB into 105 GB.
- `max_matrix_cells` defaults to **16,000,000**, checked per block *and*
  aggregated across blocks (§1.5). Every row above the second needs an explicit
  raise; the table was produced with the ceilings lifted. This is the gate you
  will actually hit, and it refuses at create rather than costing anything.
- The allocation-map term in §1.1 and the scatter arithmetic in §1.3 both
  respond to a larger declared extent.

**What over-provisioning still cannot do** is represent a stream with no ceiling
at all. Some number has to be named. If there genuinely is not one, that is the
append log's job, not the matrix's.

**Planned.** No grow path is planned. A matrix cell's address is arithmetic —
`slot_region_off + ordinal * stride` — with no indirection to update, and that is
what makes it random-access; a growable dimension would need exactly the
indirection layer the append log and its index already are.

---

## 2. Resident APIs: open scans the file, and merge/compact is not petabyte-scale

### 2.1 `VarveFile` scans the whole file at open and holds a record index

**What it is.** The resident family — `VarveFile`, `VarveReader`, `VarveWriter`,
the generated typed readers and writers, and the keyed collections — builds a
**resident record index at open**. Cost is `Theta(records + decoded bytes)` plus
an `O(N log N)` per-open sequence-uniqueness sort over `N` records. (For a file a
Varve writer produced, the sort degrades to `Theta(N)`; `O(N log N)` is the
guaranteed bound for a reordered or hostile input.) The whole index stays in
memory for the life of the handle.

**One policy changes that, and it changes the walk rather than the residency.**
`IndexPolicy::segment_on_flush` makes each commit point append an internal
*segment* record covering the records that commit point added, chained to the
previous segment through the record footer's `prev_same_block_offset`. Open
confirms a record footer at the end of the file and walks that chain backwards,
so it frames one record per commit point and reads no data record.

**Measured**, on one Linux host, `rustc 1.95` release build, 4 KiB payloads, a
flush every 256 records **and a final flush**, page cache dropped between the
write and the open:

| records | file | open, scanning | open, chained | records the open framed |
| --- | --- | --- | --- | --- |
| 50,000 | 213 MB | 1,272 ms | **60 ms** | 196 |
| 100,000 | 426 MB | 2,930 ms | **149 ms** | 391 |
| 150,000 | 639 MB | 5,005 ms | **172 ms** | 586 |
| 200,000 | 852 MB | 5,867 ms | **370 ms** | 782 |

The framed-record count is the load-bearing column: it is exactly the number of
segment records, so the open read no data record at any size. The wall-clock
ratio is 16x-29x here, but this host's scan timings vary by about 2x run to
run — read it as an order of magnitude, not a factor. Nothing above was measured
on Windows, and nothing above was measured beyond 200,000 records.

What it does **not** change is the residency below: the index it produces is the
same one a scan produces, so the 16-byte directory slot per record is still what
a handle holds for the blocks that are in it. What it costs instead is **disk** —
a segment carries an index entry per record it covers, so the file grows with
the record count. Measured on a 31 MB, 50,000-record file with a commit point
every 500 records: **+3.67 MB**.

**`BlockResidencyDescriptor` is the mechanism that reduces that**, per block. A
block declared `resident: false` is written, sequenced, chained and recovered
exactly as before and is not mirrored in memory, so `16 * records` counts only
the blocks you kept. It requires `block_offset_chain`, and it gives up
`blocks::<T>()` (a typed `Error::BlockNotResident`, not an empty collection),
replacement, and whole-generation rewrite for the whole format. Reach the block
through `VarveFile::block_chain(block_id)`, which walks its records newest-first
by positional reads, and `read_block_at::<T>(offset)`, which decodes one. Both
take `&self` and materialise one entry at a time, so the walk costs the working
set rather than the record count.

It is opt-in, off by default, and a file written with it off is byte-identical
to one written before the option existed. Enabling it costs:

- a segment record per commit point, holding 73 bytes per record it covers —
  measure this against your flush cadence, since a flush per record roughly
  triples the bytes written;
- `block_offset_chain`, which the chain *is*, so every record carries a 32-byte
  footer;
- a `crc32` integrity policy. A scan reads each record's own header, so damage
  is local; a segment payload describes many records, so without a checksum on
  it damage would misindex records whose own bytes are intact;
- `checkpoint_on_flush`, which it supersedes — declaring both is refused with
  `Error::InvalidFormatSpec`. The chain answers the same question incrementally,
  is the thing open actually reads, and has no entry ceiling;
- in-place fixed replacement (`replace_fixed`,
  `replace_fixed_in_place_exclusive`), which is refused with
  `Error::InvalidFormatSpec`. It restamps a record a segment already describes.
  What that refuses is the route, not the capability: `replace_block` publishes
  a whole new generation and re-encodes every segment payload against the new
  offsets, so it still works — and since a segment format also declares
  `block_offset_chain`, that is the route `replace` picks anyway.

The chain is derived, so anything it cannot account for falls back to the full
scan and produces the identical index — never an error. That covers a file
written before the option was enabled, a writer that appended past its last
commit point, a truncated or corrupt tail, and a broken chain link. Two opens
never take it at all: `open_recover`, whose contract is to verify every record
and truncate on the mismatch, and any open under
`IntegrityVerification::AtOpen`, which asks for the same verification.

> **The last write before you stop must be a commit point, or you get none of
> this.** Open starts the chain from the record at the end of the file. A writer
> that pushes records and then drops without a final `flush` (or `commit`)
> leaves a *data* record there, so there is no chain to start from and the open
> scans the whole file. This is the fallback working as designed, and that is
> exactly what makes it dangerous: it is silent, it costs the entire benefit,
> and it needs only a loop whose record count is not a multiple of the flush
> interval. In the table above, the same 50,000-record file opens in 60 ms with
> a final flush and 1,272 ms without one — the measurement that produced this
> warning was a benchmark of ours that omitted it and appeared to show the
> feature doing nothing. End a writing session with `flush`, and check that a
> loop like `if i % 256 == 255 { flush() }` is followed by one.

**A second policy changes the walk, and this one does not build an index at
all.** `IndexPolicy::open_digest_on_flush` makes each commit point append an
internal *open digest* record carrying the only three facts an open needs that
live in no single record: where the committed file ends, what sequence the next
append takes, and where each block's newest record sits. `open_readonly_lazy`
reads it and frames **one record**.

Measured, 50,000 records / 31 MB, same host:

| open | read syscalls | bytes read | file overhead |
| --- | --- | --- | --- |
| `open_readonly` (scan) | 100,316 | 3,207,102 | — |
| `open_readonly` (segment chain) | 516 | 7,332,522 | +3.67 MB |
| `open_readonly_lazy` (digest) | **20** | **516** | **+12.8 KB** |

The last column is the difference between the two tail records, and it is the
whole reason both exist. A segment writes an entry per record it covers, so its
overhead tracks the record count; a digest writes 12 bytes per *distinct block
id*, so it is constant. Measured directly: the same format at 200 and at 2,000
records carries a digest of exactly the same 128 bytes. What a segment buys for
its size is an open that *builds the index*, which a digest deliberately does
not.

The handle a digest open returns keeps **no directory**, because none was built.
`blocks`, `scan` and `keyed_blocks` return `Error::NoResidentDirectory` and are
answered through `with_directory`; `block_chain`, `block_tail_offset`,
`read_block_at` and every entry-taking `_into` read need no directory and work
directly. The index the caller actually wants is built with `record_map`, which
walks forward only as far as the question — measured, finding the first record
of a block in that same 50,000-record file: **13 read syscalls**.

Like the chain, the digest is derived: anything it cannot account for falls back
to the full scan and produces the identical answer. Unlike the chain, the
fallback is *reportable* — `open_readonly_lazy_with_report` returns
`LazyOpenSource::{Digest, FullScan}`, because four orders of magnitude is not
something a caller should have to infer from a stopwatch.

It requires `block_offset_chain` and composes with `segment_on_flush` and
`checkpoint_on_flush` alike; the three answer different questions.

It also refuses in-place fixed replacement (`replace_fixed`,
`replace_fixed_in_place_exclusive`) with `Error::InvalidFormatSpec`, for a
reason worth stating because it is not the segment's. A segment is refused
because the replacement restamps a record it already describes by offset. The
digest is refused because of the *sequence* it records: an in-place replacement
takes a fresh sequence and rewrites no derived record, so the digest's mark is
left one below the file's true maximum — and a lazy open trusts that mark
instead of recounting, seeding its writer onto a number some record already
used. The next scanning open then refuses the file with
`InvalidCanonicalEncoding("duplicate native record sequence")`, so the failure
is a file that stops opening rather than a stale index. As with the segment,
what is refused is the route and not the capability: a digest format also
declares `block_offset_chain`, so `replace` picks `replace_block`, which
republishes and rebuilds the digest.

`index: header_tails`, which answers the same three facts from the file header,
is not refused. Its region has a defined cold state — framed, checksummed,
claiming nothing — so an in-place replacement resets it rather than being turned
away. A digest has no such state: it is a record, trusted or absent. The cost of
the reset is one scanning open, after which the next commit warms the region
again.

**Otherwise every open scans the whole record region.** This correction matters
because the previous version of this document offered a mitigation that does not
exist:

- `IndexPolicy::CheckpointOnFlush` does **not** seed an open from a checkpoint.
  Every open path calls `load_index`, which calls `scan_records_from` for any
  format without the segment chain, and that walks from `header_len` to the end
  of the file unconditionally. A checkpoint met during that walk is *validated*
  (`inspect_index_checkpoint`) and its decoded entries are discarded. There is no public checkpoint-seeded open. What the policy actually
  bounds is the writer side: it spaces full index checkpoints geometrically, which
  bounds cumulative checkpoint **bytes written**, not open cost.
  `docs/spec.md` describes the checkpoint-seeded open as a design target; it is
  not implemented.
- The `scan_on_open` flag is **not a behaviour switch** either: it is folded into
  the schema manifest and hash bytes and is consulted nowhere else in
  `varve-core`. Clearing it does not produce a non-scanning open.

**How much RAM an open takes, so you can answer this before running it.** An
open handle keeps a **record directory**: one 16-byte slot per record — the
record's offset and its committed bit, the two facts that are not in the record
itself. Everything else an index entry carries is rebuilt from the record's own
header and footer when a read asks for it.

```text
resident_directory_bytes ~= 16 * records
```

So 10,000,000 records is about 160 MB, and 400,000,000 records about 6.4 GB.

This used to be `104 * records` — a full `Vec<RecordIndexEntry>` — which is
where the figures in older copies of this document come from. Measured across
2,000 → 20,000 records: **177.5 bytes per record retained before, 16.00 after.**

**Three ways to pay for it, and you choose:**

| | who holds the directory | per-record cost to the handle |
| --- | --- | --- |
| `open_readonly` / `open` | varve | 16 B |
| `open_readonly_without_directory(spec, path, &mut index)` | you, in full | **0** |
| `open_readonly_lazy` + `record_map` (needs `open_digest_on_flush`) | you, only the part you walked | **0** |
| `VarveStreamReader` (`high-cardinality-dev`) | nobody — sequential walk only | 0 |

The middle row is not a reduction in capability. `blocks`, `scan`,
`keyed_blocks`, `metadata`, `verify_all` and the rest all still work — through
`file.with_directory(&index)?`, where `index` is the `Vec<RecordIndexEntry>` that
same call handed you. Calling one of them on the handle itself returns
`Error::NoResidentDirectory { operation }`, naming both the read and the way to
answer it; it does **not** answer as an empty file would, which would be
indistinguishable from the truth for a caller who forgot.

Measured: opening a 20,000-record file allocates **320,218 bytes with a
directory and 218 without** — the 320,000-byte slot array is not built, not
built-and-freed.

**A directory belongs to the generation it was built from.** `with_directory`
reads at the offsets it is handed, and until it was made fallible nothing bound
those offsets to a *file*. The replacement paths publish by renaming a new file
over the pathname, so this sequence — build a directory, let a `replace_*` run,
`reopen_readonly`, read through the directory you already had — pointed the
reads at byte ranges where the records had moved. Measured on an
`integrity: none` format: eight `Reading` values came back with no error at all,
one of them a slice of the replacement's text reinterpreted as a `u64` and seven
of them zero.

`with_directory` now returns `Result` and re-frames the directory's first and
last entries against the file, refusing with
`Error::DirectoryDoesNotDescribeThisFile` when the record header at an offset is
not the record the entry claims. Two record headers, once per `with_directory`
rather than per read.

Be exact about what that is: a **disagreement detector, not an authenticator**.
A directory wrong only in the middle passes, and a caller who fabricates
entries that frame correctly is not stopped. A supplied directory is still a
trusted input — the check converts the common accident into a typed refusal.
A subset or a prefix of a directory is legitimate and is accepted; so is an
empty one, which makes no claim to disagree with.

**The peak of a single open is a separate figure and is larger.** The scan still
materialises one `RecordIndexEntry` per record before the directory is taken
from it, so an open transiently reaches about `170 * records` live. Pass your own
buffer — `open_readonly_with_scratch`, or `open_readonly_without_directory`,
which take one — and that allocation happens once for any number of files rather
than once per open: measured 29,824,680 bytes over eight opens against
**5,969,576** through one buffer.

That peak is a property of the *scan*, so the third row above is the only one
that avoids it rather than amortising it: `open_readonly_lazy` from a digest
never builds the array, because it never frames a record.

`ReadLimits::STANDARD` leaves `max_records`, `max_index_bytes` and
`max_scan_bytes` at `u64::MAX`, so the **default profile places no ceiling on
resident index size**. `ReadLimits::UNTRUSTED` makes them finite (16,000,000
records, 1 GiB index, 16 GiB scan, 65,536 segments, 256 MiB keyed tail) and is
the right default for attacker-supplied input.

**`max_file_len` is no longer enforced anywhere, and no longer has to be
declared.** A ceiling on how large a *file* may be bounded nothing the reader
allocates: what it allocates is bounded by `max_records`, `max_index_bytes`,
`max_scan_bytes`, `max_record_payload_len` and `max_logical_payload_len`, each
of which has a check that consults it. All twenty-two enforcement sites were
removed and `file_len` came off both required-declaration lists. The field and
the DSL's `limits { file_len: .. }` remain so that existing format definitions
keep parsing; the value is inert. A format that relied on it to refuse a large
file must state one of the limits above instead.

**This is not the petabyte-scale path.** The petabyte-scale path is the
`high-cardinality-dev` **stream/indexed** family: `VarveStreamWriter`,
`VarveStreamReader`, `VarveIndexedWriter`, `VarveIndexedReader`, and their
`.vks`/`.vki` sidecars. Those open in `O(1)` from a clean sidecar and never
bootstrap, repair, rebuild, verify, truncate or scan on a normal open.

**Who it affects.** Anyone pointing `VarveFile` at a file whose record count does
not fit comfortably in RAM alongside the application, and anyone opening a large
file in a latency-sensitive path.

**Workaround.** Use the scalable family (§3) — and read §3 first: it is behind a
`dev` feature flag, has never shipped, and its four modules are the least audited
code in the tree. There is no third option. Keeping the handle open is the other
half of the answer, since the scan is per open, not per read.

**Planned.** The resident family is intentionally resident. No change planned.

### 2.2 A keyed writer's construction cost scales with block types times index size

**What it is.** A generated writer primes one keyed-tail map per keyed block type
at construction, and each priming pass walks the resident record index.
Construction is `Theta(M * N)` for `M` keyed block types and `N` index entries.

`max_keyed_tail_bytes` binds **per block id, not summed**, so one writer can
retain about `M * max_keyed_tail_bytes`. Additionally, the ceiling is charged at
the *build peak* of the map, which is larger than the map it produces — a ceiling
sized from the observed steady-state map can refuse an open that previously
succeeded.

**Who it affects.** Formats with several keyed block types over large files.

**Workaround.** Reduce keyed block types, or size `max_keyed_tail_bytes` from the
build peak with margin rather than from the resting map.

**Planned.** Not scheduled (open item 3). Making the ceiling bind on the total
would need a per-file running total across the per-block-id tail maps, which do
not have one; until then the per-block-id semantics are the contract, and they
are stated on the limit's own rustdoc and in
[API Reference](api-reference.md).

### 2.3 Keyed merge and compact are resident-only and explicitly not petabyte-scale

**What it is.** `merge_keyed_files`, `compact_keyed_files` and
`compact_keyed_file` are resident-only. Memory is
`O(K-ever + largest resident input index + 8N uniqueness temporary + retained live values)`,
where **`K-ever` counts every distinct key ever seen, tombstoned keys included**.
Nothing spills to disk.

**Varve exports no bounded-memory external merge or compact.** The scalable
family covers ingest and point lookup only; it does not cover merge or compact.

**Who it affects.** Anyone compacting a file with high key cardinality, and
especially anyone compacting a file with a long tombstone history — the deleted
keys are still charged.

**Workaround.**

- Size the run in advance with `estimate_keyed_merge` (itself
  `O(largest input index)`).
- Bound it with `merge_keyed_files_with_key_limit`,
  `compact_keyed_files_with_key_limit` or `compact_keyed_file_with_key_limit`,
  which fail with a typed limit error at a chosen key ceiling and publish no
  output.

`KeyedMergeEstimate::peak_resident_structural_bytes()` is a **structural
estimate, not an upper bound**. It excludes heap owned by `Key` and `T` values,
`HashMap` load-factor slack and control bytes, decode scratch, and allocator
metadata.

It is named that way because the earlier spelling, `peak_resident_bytes()`,
documented an "upper bound" contract that was false by an arbitrarily large
margin for any heap-owning key or value type. **Both the type and the earlier
name were introduced and renamed inside the 0.4.0 development cycle — no released
version ever exposed `peak_resident_bytes()`,** so there is nothing to migrate.
The history is recorded so the rationale for the longer name is not lost.

**Planned.** An external, bounded-memory merge is not implemented and is not
scheduled.

### 2.4 Under a transaction-marker commit policy, reopening for write discards the uncommitted tail

**What it is.** With `CommitPolicy::TransactionMarker`, records are visible only
behind a commit marker. Two consequences that surprise a first-week user, both
deliberate and both silent:

- **Opening for write truncates.** `truncate_uncommitted_tail_if_needed` sets the
  file length back to the end of the last commit marker. Records appended after
  the last `flush()`/`commit()` in a previous session are **deleted from the file
  on the next write open**, not merely hidden.
- **A file with no marker at all reads as empty.** `scan_records_from` returns an
  empty index when it finds no marker, whatever the file contains. A process that
  appended 10,000 records and exited without flushing reopens to zero records, and
  a write open then truncates them away.

There is no error, no warning and no diagnostic on either path — the index is
simply short. This is the correct behaviour for the policy (an uncommitted tail
must not become committed by a later append landing next to it), but it is a
data-loss shape if you assumed `push` was durable.

**Who it affects.** Anyone on a `transaction_marker` format who appends without
flushing, and anyone who treats process exit as a checkpoint. `on_flush` markers
make every `flush()` a commit point, which is why that is the common declaration.

**Workaround.** `flush()` (or `commit()` under `transaction_marker(explicit)`)
before you rely on anything being in the file, and treat an unflushed tail as
lost. For the exact ordering guarantees see [Durability Model](durability-model.md)
and [Recovery Model](recovery-model.md).

**Planned.** No change. The alternative — retaining an uncommitted tail across a
write open — is the defect this prevents.

---

## 3. The scalable family is behind a feature flag named `dev`

**What it is.** `VarveStreamWriter`, `VarveIndexedWriter`, their readers,
`disk_index`, `scan_control` and the `.vks`/`.vki` sidecars are all gated on the
`high-cardinality-dev` cargo feature. **This family has never shipped in any
released version.** Its wire artifacts are at their first public versions
(`.vks` sidecar metadata version 3, `.vki` disk-index metadata version 3), and
its API surface has had no external users to stabilise against.

**Who it affects.** Anyone who needs bounded-memory ingest or point lookup over
data larger than RAM — which is to say, everyone the resident family cannot
serve.

**Workaround.** None. Enable the feature and accept that the surface may change
before it loses the `dev` suffix.

**Planned.** Stabilisation is intended but not scheduled. Also note that the
round-1/2 module set — `disk_index.rs`, `stream.rs`, `indexed.rs`,
`scan_control.rs` — has **not** been walked against the project's five internal
invariants (open item 7). The matrix and resident paths have been; these have not.

Operational notes for users who enable it anyway:

- `sync()`, **not** `flush()`, publishes a clean generation that can be reopened
  normally.
- Missing, dirty, stale and identity-mismatched sidecars are errors with named
  recovery operations (`bootstrap_stream_checkpoint`, `rebuild_disk_index`), not
  silent fallback scans.
- Long scans are cancellable via `ScanCancellationToken`; cancellation returns
  `Error::ScanCancelled { progress }` and publishes no new sidecar.

---

## 4. Concurrency: what is `&self`, and what a reader sees of a writer

### 4.1 What takes `&self` today

**Every matrix read entry point takes `&self`.** All six of them, on all three
handle types — that is the complete inventory, not a sample:

| Matrix read entry point | 0.3.0 | 0.4.0 |
| --- | --- | --- |
| `read_matrix_cell::<T>` | `&mut self` | `&self` |
| `matrix_cell_payload::<T>` | `&mut self` | `&self` |
| `read_matrix_aux` | `&mut self` | `&self` |
| `matrix_cell_status::<T>` | `&self` | `&self` (unchanged) |
| `matrix_aux_len` | `&self` | `&self` (unchanged) |
| `matrix_resume_signal` | `&self` | `&self` (unchanged) |

Each exists on `VarveReader`, `VarveWriter` and `VarveFile`, so 18 signatures.
`crates/varve/tests/matrix_concurrent_reads.rs` pins 15 of them ("5 entry points
x 3 handle types") by holding two shared borrows of a non-`mut` handle;
`matrix_resume_signal` is the sixth and was already `&self`.

**Three entry points relaxed, not four.** Payload and aux reads are named
explicitly because an earlier version of this table listed only three entry
points and omitted them, which read as though `matrix_cell_payload` and
`read_matrix_aux` still needed exclusive access. They do not. In the same
correction, `matrix_cell_status` moves out of the relaxed group: it was `&self`
on all three handle types in **0.3.0** and at every commit of the 0.4.0 cycle
(checked against the `varve-core 0.3.0` source package and against the parent of
the round-15/16 commit), so listing it as a relaxation overstated what this
release changed. What is true either way is the conclusion: all six are
concurrently issuable through a shared handle.

| Subsystem | Reads take | One handle, many threads |
| --- | --- | --- |
| Matrix — all six read entry points above, on `VarveReader`, `VarveWriter`, `VarveFile` | `&self` | yes — the handle is `Send + Sync` |
| Resident record reads (`blocks`, `keyed_blocks`, `materialized_keyed_blocks`, `scan`, `index_entries`, `metadata`) on `VarveReader` | `&self` | yes |
| The same reads resolved through a caller-supplied directory (`with_directory(..)`) | `&self` | yes |
| All mutation (`push*`, `delete*`, `replace_*`, `write_matrix_cell*`, `commit_matrix_cell`, `clear_matrix_*`, `flush`, `commit`, `sync`) | `&mut self` | no |

The three relaxations are source-compatible — an existing call through a `&mut`
binding still compiles — but they are what makes shared-handle concurrent reading
possible.

**Measured concurrent matrix read scaling** (wall clock, lower is better).
Only the second row is asserted on every run, at `<= 1.0x`, i.e. "does not
serialise", by
`concurrent_lazy_readers_are_not_serialised_behind_the_page_store`. The first
row is measured and printed by `report_the_scaling_of_one_shared_handle` and
decides nothing; its `<= 1.0x` form is an `#[ignore]`d manual benchmark,
demoted because two `ubuntu-latest` runs of healthy code reported 2.14x and
1.43x. What gates the eager path instead is counted, not timed: matrix reads
issued while a commit-map page-store lock was held must be zero
(`page_store_lock_audit_tests`).

| Scenario | Run A | Run B (independent) |
| --- | --- | --- |
| 24,000 reads, one shared handle, 1 vs 4 threads | 0.138s vs 0.046s — **0.33x** | 0.076s vs 0.032s — **0.42x** |
| Hostile lazy config (1-page cache, 32 live pages, 40,960 fault-in reads) | 3.814s vs 1.667s — **0.44x** | 2.135s vs 0.746s — **0.35x** |

Two runs are shown because these are wall-clock numbers on a shared desktop and
they move by tens of percent between runs. **Treat the absolute times as
illustrative and only the "well under 1.0x" conclusion as the result.** Anything
sized on the exact ratio is sized on noise.

**These numbers are Windows numbers, and the mechanism behind them is
Windows-only.** Windows `ReadFile` serialises on the kernel file object, so each
reading thread is given a private file object derived with `ReOpenFile`
(`MatrixReadPool`). Without it, four threads measured **1.54x slower** than one.
That pool is `#[cfg(windows)]`; on Unix, positional `pread` does not serialise on
the file object, so a shared handle is expected to be adequate. The two
contracts that still gate — the lazy `<= 1.0x` ratio and the counted
page-store-lock invariant — **do** run on Unix, because CI runs the suite on
`ubuntu-latest`. What has never run there is the `#[ignore]`d eager threshold,
and **no Unix throughput has ever been measured**. See §6.

One lock does exist in the matrix read path (open item 31): `SparseBitmap` holds
a `Mutex<PageStore>` so a demand fault-in can happen under `&self`. It is taken
only for O(1) map operations, is released across the fault-in read, and is never
taken on the write path. Since 0.5.0 every persisted map is demand-filled, so this
lock is on the path of every reader of a category on every read; session-only maps
(a writer's current-write map, a rebuilt map) have nothing to fault in and only
ever take it for a hash lookup.

### 4.2 Where a reader gets a snapshot rather than live state

Varve is **single-writer**, enforced by a native object lock on the target file.
Within that:

- **Resident record reads are a snapshot as of open, until the handle is asked
  to advance.** The record index is built at open, and records a writer appends
  afterwards are not visible to that handle until it calls
  [`VarveFile::follow`]/[`VarveReader::follow`], which frames the bytes past the
  end it holds and adopts them to the same commit boundary an open would stop
  at. A handle never advances on its own — that is what keeps a `&self` read
  from observing a record mid-write.
- **Matrix commit status is not a snapshot at all.** Each commit-map page is as
  of the first touch that faulted it in, so different pages can reflect different
  instants and a page faulted in late can reflect a writer's later work. This is
  consequence 3 in §1.4. Before 0.5.0 the removed `EagerVerified` residency policy
  materialised every published page at open and gave a reader a whole-map snapshot
  as of its own open; nothing provides that now.
- **Matrix cell payload bytes are read positionally at read time**, so payload
  reads see current file contents even where the commit bitmap does not.
- **Stream and indexed readers open from a published sidecar generation** and see
  that generation, not the writer's in-flight state. `sync()` publishes.

**Who it affects.** Anyone assuming a Varve handle is a live view of a file
another handle is writing. It is not.

**Workaround.** `follow()` advances a resident reader at a cost proportional to
what was appended rather than to the file. It does not cross a generation, so
when the pathname has been republished the pair to use is `is_current()` — which
compares the OS object identity and costs one `open` — followed by
`reopen_readonly()`. Both take `&self`, so a handle shared behind an `Arc` can
be asked and replaced without any reader stopping. For a matrix there is no
"advance": a reopen gives a fresh handle whose pages will again be as of whenever
each one is first touched, so a reader that needs one instant across a whole map
must coordinate that with the writer itself.

**Planned.** No live-view mode is planned. `follow()` is an explicit advance,
not a live view: between two calls the handle is still a fixed snapshot.

### 4.2b Nothing tells a reader that its generation was replaced

`follow()` advances a handle along the object it opened. The replacement paths
(`replace_fixed`, `replace_block`, `replace_rewrite`, and any future compaction)
publish by writing a new file and renaming it over the pathname — `replace_fixed`
included, which copies the generation and patches one same-length record in the
copy, so it moves no offset but still leaves the old object behind. A handle open
across that keeps reading the old object — which is what makes a republish safe
for readers in flight, and is measured: a handle holding 21 records read them all
back, unchanged and without error, after the pathname had been replaced.

**There is no notification, and there is no design that would give one for
free.** A handle learns its generation was replaced only by asking, and the
question costs a syscall. Two mechanisms that look like they would avoid it do
not:

- **A marker record appended to the old object before the rename.** Only a
  handle that is following the tail can see it, because a handle's snapshot
  length is fixed and a record past that length is outside it. A handle that
  never calls `follow()` never sees the marker.
- **A flag or magic word in the file header.** No read a live handle performs
  goes near it. Of the eight sites that read the header, five are the opens
  themselves; the other three are a generation check on the `replace_*` paths
  and the two disk-index sidecar windows behind `high-cardinality-dev`, and all
  three re-read the file rather than consulting a handle. So a flag there would
  inform only a *new* open, which already resolves the pathname to the current
  generation anyway.

Both cover exactly the handles that are already in a polling loop, and neither
covers the rest. What each shape of reader actually needs:

| Reader | Calls `follow()` | How it learns |
| --- | --- | --- |
| Streaming loop | yes | `follow()` returning `0` does not say which of its reasons applies — "nothing appended", "appended but not yet committed", or "this generation is finished"; `is_current()` separates the last from the other two, on a path that is idle by definition |
| Opens, reads, closes | no | **Not affected.** Every open resolves the pathname to the current generation |
| Long-lived, read-only, no loop | no | **Nothing informs it.** The application decides when freshness matters and calls `is_current()` — a timer, a user action, a filesystem watch |
| Shared `Arc<VarveFile>` | owner only | The owner polls and swaps in `reopen_readonly()`; readers hold `&self` throughout |

**Why varve does not check on every read.** It could, and the check is one
`open` plus two metadata calls — per read, on a path whose cost is the reason
positional reads exist. That collides with the standing requirement that reads
stay cheap and take `&self`, and it would charge every format for a capability
most of them never use. The check is offered, not imposed.

**What staleness costs.** Two things, and neither is corruption. The data is old
but whole: the superseded generation is complete and self-consistent, and every
offset the handle holds still means what it meant. And the object stays on disk
— an unlinked file is freed when the last descriptor closes, so a forgotten
read-only handle holds a whole generation's bytes for as long as it lives. A
compaction that halves a file frees nothing until the readers of the old one let
go.

**Who it affects.** Any long-lived reader that must not serve stale results, and
any deployment where a compaction's disk saving is expected promptly.

**Workaround.** `is_current()` before the reads that matter, and
`reopen_readonly()` when it answers `false`. Drop handles that are no longer
being read, rather than keeping them for a possible later question.

**Planned.** No push notification is planned, for the reason above: every
candidate mechanism reaches only readers that are already asking.

### 4.3 Writer-lock edge cases

- `WriterLock::drop` discards the error from `clear_writer_lock_info` because
  `Drop` cannot report (open item 20). A caller that never runs a self-test gets
  no signal that lock-info cleanup failed. Closing this needs an explicit
  `release()`/`close()` — an API addition, not planned.
- On Windows, writer-lock **marker** removal has no directory confinement (open
  item 5). The marker's identity is captured by opening the same unverified
  pathname, so the check is self-referential: it closes the
  capture-to-unlink window but proves nothing about ownership, and a marker
  swapped in before the capture is still deleted. Unix has exclusive-directory
  confinement; Windows has no equivalent. This never affected authoritative
  single-writer exclusion, which is a native lock on the target file object.
- `clear_stale_writer_lock` recovers a stale lock in `O(1)` without opening or
  scanning native data.

---

## 5. Features that are designed but not implemented

### 5.1 Channel-selective access

**What it is.** An internal design study — an option model for reading a subset
of channels without paying full block I/O, with its case analysis and its layout
variants — exists as working notes. It proposes a set of independently selectable
options, each defaulting to off, in the same shape as the existing `index: [...]`
DSL keys.

**It is a design study, not a feature.** There is no `channel_view` type, no
DSL key, and no generated method anywhere in `varve-core` or `varve-macros`.
Nothing that study describes is callable, and the notes are not published.

One conclusion from it is worth carrying here because it is a property of the
current on-disk layout rather than of the unimplemented feature: **an interleaved
payload cannot be read channel-selectively below full block I/O.** If your data
is interleaved, no future option will make a single-channel read cheaper than a
whole-block read without re-emitting the payload.

**Who it affects.** Anyone who saw the design notes and planned around them.

**Workaround.** Read whole blocks, or lay the data out planar yourself in
separate blocks.

**Planned.** Designed; not scheduled.

### 5.2 There is no way to discard unverifiable matrix visibility

**What it is.** The F-04 refusal shipped without its named companion operation
(open item 17). `rebuild_matrix_commit_from_crc` now returns
`Err(Error::MatrixFatalCorruption)` rather than publishing a commit view built on
evidence it could not read — correctly — but an operator who genuinely wants to
discard unverifiable visibility and start from a known state has no supported way
to do it.

The refusal also reuses `Error::MatrixFatalCorruption` (open item 16), and its
message recommends `with_matrix_fatal_forensics` — which is useless advice to a
caller that has already enabled forensics and is being refused anyway.

**Who it affects.** Operators recovering a damaged matrix.

**Workaround.** Repair or regenerate the validity evidence, or recreate the
matrix.

**Planned.** The discard operation is acknowledged as missing. Not scheduled.

### 5.3 A cleared category cannot be rebuilt until reopen

`clear_category` deliberately does not reset completeness, so a writer that
clears a category still cannot `rebuild_matrix_commit_from_crc` for that category
in the same session (open item 24). Fail-closed, but operator-visible. Reopen the
file.

---

## 6. Not verified

Almost everything in this section is a statement about **assurance** rather than a
known defect: it is here because a user is entitled to know which claims rest on
executed tests and which rest on inspection.

This section used to carry two exceptions — defects in the project's own gates
rather than in the library: a Linux Clippy failure and a default-feature test
failure. Both are fixed, and every gate is green. What replaced them is not a
shorter list of caveats but a sharper one: the suite now runs on Linux (§6.1),
and running it there found three defects that every Windows run had passed. The
distinction that matters throughout this section is **executed** versus
**measured** — Linux is now the first, and is still not the second.

### 6.1 The Unix code paths, and what the first Linux run found

This section used to say the Unix paths had never been executed. **That stopped
being true on 2026-07-27, and what the first run found is why the section is
still here.** The suite is executed on Linux as well as
Windows: CI runs `ubuntu-latest` and `windows-latest` on every push to `main`,
on every pull request, and on demand. The three paths this section used to name
as unexecuted — `diagnostics.rs`'s `openat`/`unlinkat` exclusive-directory
cleanup, `file.rs`'s `O_NOFOLLOW` lock-marker open, and the matrix
concurrent-read path without the Windows private-handle pool (§4.1) — all run
there now, `compression-zstd` included under `--all-features`.

The first Linux run found **three defects that every Windows run had passed**,
which is the reason to state the platform behind a claim rather than the claim
alone:

- a writer lock leaked across `fork`: a POSIX advisory lock belongs to the open
  file *description*, `Command::spawn` forks and duplicates it, and dropping the
  guard released nothing until the child execed. Dropping is not releasing, so
  the release is now explicit.
- identity-checked deletion compared recycled inode numbers. Identity bytes read
  from a handle that is then closed name nothing: ext4 hands a just-freed inode
  straight back to the next create, and the check said "same object" about a file
  it had never seen. A handle is now held open for as long as the identity is
  used, which pins the inode.
- an open-cost assertion hard-coded NTFS's ~128 KiB allocation-run granularity.
  ext4 reports extents precisely, so the same file measured 2 pages and 8,240
  bytes where Windows measured at least 4 and 64 KiB.

What this section still says, unchanged: **the numbers are Windows numbers.**
Executed on Linux is not measured on Linux. Every syscall count, residency
figure and open cost in these documents was taken on Windows x86_64, and the
third defect above is exactly what a different filesystem does to such a
constant. The only Linux numbers on record are two runs of the eager 1-vs-4
shared-handle ratio, 2.14x and 1.43x, published as the evidence that that ratio
is dominated by the host rather than as a result; no Linux throughput, syscall
count, residency figure or open cost has been measured.

`cargo clippy --target x86_64-unknown-linux-gnu -p varve --no-default-features
--lib -- -D warnings` previously failed with `field 'handles' is never read` at
`crates/varve-core/src/matrix.rs`, because `MatrixReadPool::reopen` is
`#[cfg(windows)]`-only and the field's other readers are `#[cfg(test)]`. The
field is an ownership anchor rather than dead weight — it holds the `Arc<File>`s
whose `Weak`s live in the thread-local cache — so it now carries
`#[cfg_attr(not(any(windows, test)), allow(dead_code))]` with that reason
recorded at the declaration. That was a cross-compiled lint pass when it was
written; Clippy now runs natively on Linux in CI, across nine feature
configurations, and is green.

### 6.2 The petabyte-scale positional-I/O probe has never run

`pib_probe::real_file_positional_io_at_one_pib` is `#[ignore]`d and additionally
soft-passes: `ProbeError::Unsupported` prints "skipped" and returns success unless
`VARVE_REQUIRE_PIB_SPARSE=1` is set. **The 1 PiB sparse-offset gate has never been
executed.** Its sibling `real_file_positional_io_at_one_tib_smoke` is also
`#[ignore]`d and has no required-mode escape at all — `Unsupported` always passes.

The practical statement: Varve's petabyte-scale claims rest on the cost model and
on tests at far smaller scales, not on a demonstration at one petabyte.

### 6.3 Performance and scale probes do not run in any job

- `crates/varve/tests/perf_smoke.rs` is entirely `#[ignore]`d. The performance
  smoke suite runs in no job.
- The one-million-key RSS/allocator stress probe
  (`crates/varve/tests/high_cardinality.rs`) is `#[ignore]`d.

### 6.4 No fuzz, Miri or ASan run covers this release

- CI runs **no fuzzing**. The supply-chain job does `cargo audit`, `cargo deny`,
  `cargo metadata`/`check` and a lockfile drift check.
- The most recent recorded fuzz evidence for this project is dated
  2026-07-11 (four core targets) and 2026-07-18 (three sidecar targets). Both
  predate several rounds of change to `matrix.rs`, `file.rs`, `codec.rs` and
  `format.rs`. The cited ASan run covered 21 library tests; the library suite is
  now 139.
- **This changed in 0.7.0, and the change is smaller than it sounds.** All three
  now run, weekly, in `.github/workflows/sanitizers.yml`: `fuzz-smoke` builds and
  runs **every** target for 90 seconds each, `asan` runs the `varve` and
  `varve-core` test suites under AddressSanitizer with `--all-features`, and
  `miri` runs `varve-core`'s no-default-feature unit tests. Executed on Linux
  before the workflow landed: Miri 28 tests green under strict provenance and
  symbolic alignment checking, ASan 163 tests green with zero reports, and a
  bounded campaign on two targets producing no artifact.

  Ninety seconds per target is a smoke test, not a campaign. It proves the
  harness runs and catches what is shallow; it explores almost nothing. The
  disparity is measurable rather than theoretical: `codec_arbitrary` executes
  about 300,000 inputs per second, while `sidecar_state_machine` -- which builds
  a redb sidecar per input and `fsync`s it -- manages 64, so the same 90 seconds
  buys 27 million executions on one target and under 6,000 on another. **No long
  fuzz campaign has been run against this code**, and the modules that most need
  one are the slowest to fuzz.
- A libFuzzer OOM reproducer from 2026-07-20 sat unpromoted in
  `fuzz/artifacts/codec_arbitrary/` until 0.7.0, blocking
  `scripts/run-security-fuzz.ps1` (exit 2) the whole time. It is now a test --
  `codec_hardening.rs::the_2026_07_20_oom_reproducer_stays_refused`, with the
  eleven bytes inlined so deleting the artifact did not delete the regression --
  and the artifact is gone. Replayed through the real fuzz target on Linux before
  removal: executed in 0 ms, no OOM.

  **It has now been triaged, and it does not reproduce against this release's
  code.** The 11 bytes select `decode_from_slice::<HashMap<(), ()>>` (selector
  105, `105 % 12 = 9`) with a declared entry count of 587,203,068. Replayed
  through that public entry point today, every outcome is a typed error returned
  in microseconds:

  | Declared count | Outcome | Time |
  | --- | --- | --- |
  | 587,203,068 (the reproducer) | `LimitExceeded { resource: "HashMap entries", actual: 1073741888, limit: 1073741824 }` | 7.5 µs |
  | 293,601,534 (largest the 1 GiB ceiling admits) | `InvalidCanonicalEncoding("duplicate HashMap key")` | 6.5 µs |
  | `u64::MAX` | `InvalidCanonicalEncoding("collection count cannot make bounded input progress")` | 0.1 µs |

  No admitted count can produce a large allocation on this path: the map reserves
  `min(count, MAP_PREALLOCATION_ENTRIES = 1024)` entries before a single entry has
  been proven decodable (SAFE-01), so the real allocation is bounded by 1024
  regardless of what the file claims. The remaining gap is **test hygiene, not an
  unbounded allocation**: the reproducer was never promoted to a deterministic
  regression test, so nothing in the suite pins the behaviour that fixed it.
  **This release still makes no fuzz-pass claim** — one triaged reproducer is not
  a campaign.

### 6.5 Tests that can pass without proving anything

Named here so that a green run is not over-read:

- Both `pib_probe` tests (§6.2).
- The Windows lock-marker reparse-point test skips loudly where the host does not
  grant `SeCreateSymbolicLinkPrivilege`; the hard-link branch does run (open item
  10).
- The convoy scaling assertions in `matrix_concurrent_reads.rs` and
  `matrix_lazy_residency.rs` are skipped, with an announcement, on a single-core
  host.

### 6.6 What was verified, and what "green" does not cover

Every gate in the table below is green as of 0.5.0. The heading used to read "and
the one gate that is red"; that gate — a default-feature-only test defect — is
fixed, and the paragraphs after the table state exactly what a full green run still
does not establish.

Re-run in full for **0.5.0** on Windows x86_64 MSVC, 2026-07-25. Everything below
was executed; numbers are from that run. The 0.4.0 column is kept where a count
moved, because a count that moves without explanation is how a suite quietly
shrinks.

| Gate | Result (0.5.0) | Was (0.4.0) |
| --- | --- | --- |
| `cargo fmt --all --check` | clean | clean |
| `cargo check --workspace --all-features --all-targets --locked` | clean | clean |
| Clippy `-D warnings`, 9 configurations (all-features, default, no-default-features, and each of the 6 optional features alone) | clean, 9/9 | clean, 9/9 |
| `cargo doc`/`rustdoc -D warnings`, 3 crates x {all-features, default} | clean, 6/6 | clean, 6/6 |
| `varve-test-runner test --workspace --all-features` | **651 passed, 0 failed, 8 ignored** across 49 binaries; artifact cleanup verified empty | 648 passed |
| `varve-test-runner test --workspace` (default features) | **418 passed, 0 failed, 5 ignored** across 48 binaries | 168 passed, **1 failed** |
| `cargo test -p varve-core --all-features --lib`, five consecutive runs | 139 passed each time, 0 failed; no flake | same |
| `rename-fixture` and `public-api-fixture`, built and run | both OK | both OK |
| `cargo deny check`, root **and** fuzz workspace | `advisories ok, bans ok, licenses ok, sources ok` for both | clean |
| `cargo audit` (root) | clean, 79 crate dependencies scanned | clean |
| fuzz workspace `cargo metadata --locked` + `cargo check --locked --all-targets` | clean | clean |
| `cargo package --locked`, verification enabled | **verified archives for all three crates** (`varve-core`, `varve-macros`, `varve`) | verified |
| Clean-tree copy (no `target/`, no VCS, no `mck-*`/`slipway-*`, no fuzz corpus), `cargo metadata` + `cargo check --workspace --all-features` | clean | clean |

**Two notes on this run, both about honesty rather than results.**

The default-feature count rose from 168 to 418 because the 0.4.0 figure was taken
from a run that aborted at the first failing binary; 418 is the whole default
suite. Nothing was added to the suite to produce that number.

`cargo package` was run in a clean copy of the tree rather than in the repository,
because the repository working tree was dirty (this release's own uncommitted
changes) and `cargo package` refuses a dirty tree without `--allow-dirty`. The
archives were verified from exactly the release content, but the run that produced
them was not a run in a committed repository. It should be repeated after the
release commit. `cargo package -p varve` alone still cannot verify — `varve-core`
is not on crates.io, so the facade's dependency cannot be resolved from the index
— and `cargo package --workspace`, which unpacks the sibling archives into a
temporary local registry first, is the form that works.

**The red one, and its fix.** In the run tabulated above, under the **default**
feature set,
`crates/varve/tests/matrix_concurrent_reads.rs::threads_sharing_one_handle_read_every_cell_correctly`
failed with `Error::IntegrityFeatureDisabled`: the test looped over
`[IntegrityPolicy::None, IntegrityPolicy::Crc32]` unconditionally, and `Crc32` is
refused when the `integrity` feature is off. It was a **defect in the test, not in
the library** — no library behaviour was wrong, and the same test passed under
`--all-features`. The loop is now gated on `cfg!(feature = "integrity")`, so the
default run covers `None` only. Re-measured 2026-07-25: the binary passes 4/4
under default features and 4/4 under `--all-features`.

Both gates that were red are now green — this one and the Linux Clippy lint in
§6.1. **As of the 0.5.0 run the whole table above was re-executed**, so those two
are no longer individually-remeasured exceptions; the default-feature suite in
particular is now a full green run rather than an abort at the first failure. The
Linux result in §6.1 remains a cross-compiled lint rather than an executed Linux
test, and that has not changed. This is stated precisely because "all green" was
the shape of every previous over-claim in this project's history.

One thing the 0.5.0 run does **not** cover: it was executed against a working tree
that a separate, unrelated documentation reorganisation was being written into at
the same time. The numbers above are real and were taken from the release content,
but they are not a certification of a committed, quiescent tree. Re-run this table
after the release commit.

Also verified in this run, and quoted in §1 and §4: the 13 `matrix_lazy_residency`
tests (10 at 0.4.0; the three added for 0.5.0 pin `MatrixMetadataResidency::DEFAULT`,
the derived cache, and §1.3's still-occurring refusal), the 4
`matrix_concurrent_reads` tests, and
`matrix_integrity_scaling::measured_resident_bitmap_bytes_against_the_ceiling`.

### 6.7 The backward-compatibility table: what is now executed, and what still is not

**No test in the suite opens an artifact written by an older version.** The
repository stores none: there is no tracked `.varve`, `.vks` or `.vki` file
anywhere and no fixtures directory, so every row of the compatibility table in
[API Changes](api-changes.md) is *derived from the current source*.

Three of those rows were checked by hand on 2026-07-25 against files produced by
genuinely older builds, and all three behaved as documented:

| Claim | How it was checked | Result |
| --- | --- | --- |
| A plain native append-log file written by **0.1.0** opens normally | a producer built against the real `varve-core 0.1.0` source package wrote an 8-record fixed-block file; a 0.4.0 consumer opened it | **opens; all 8 records and every field byte-for-byte correct** |
| The same, written by **0.3.0** | producer built against the real `varve 0.3.0` / `varve-core 0.3.0` / `varve-macros 0.3.0` source packages | **opens; all 8 records and every field correct** |
| A file pinned with a **computed schema hash** from 0.3.0 is refused | the same declaration compiled under 0.3.0 (hash `0xbaf33c482858b060`) and under 0.4.0 (hash `0x4bf7fa8d7d7e9e4f`) | **refused with `Error::SchemaHashMismatch`**, carrying the 0.4.0 value as the wanted hash and 0.3.0's as the stored one |

That is a **manual, out-of-tree check, not a regression test.** It was run once, on
one host, over one small format with two `u32` fields. It does not cover variable
blocks, compression, keyed collections, matrices or sidecars, and nothing prevents
a future change from breaking it silently — there is still no committed fixture and
no test.

The rows that remain unexecuted:

| Row | Evidence | What it does not cover |
| --- | --- | --- |
| Matrix `VMAT` v1/v2/v3 refused | `matrix_integrity_scaling.rs` writes a current file and **patches its version field** | that a genuinely old matrix file's other regions also refuse rather than misparse |
| Pre-nonce matrix refused at the nonce region | **no test, no manual check** | the whole row |
| Matrix and disk-index sidecar version refusals | source inspection only | the whole rows |

The positive claim is also sound in code: `read_file_header`,
`file_header_extensions`, `write_record_header` and `encode_record_footer` are
byte-identical to v0.1.2 and the record-footer constants match. The correct summary
is: **the append-log compatibility claim now has one executed demonstration behind
it; the matrix and sidecar refusal claims still rest on inspection.**

### 6.8 Platform support, stated once

| Target | Status |
| --- | --- |
| Windows x86_64 MSVC | executed, and the platform every number in this document is from. |
| Unix (Linux) | **executed**, including `compression-zstd`, and green: tests, Clippy and fmt run natively on `ubuntu-latest` in CI (§6.1). The first such run found three defects every Windows run had passed. **No Linux throughput, syscall count, residency figure or open cost has been measured** — the numbers above are not Linux numbers, and the only Linux figures on record are the two eager scaling ratios in §6.1. |
| macOS / other targets | compile paths exist, nothing executed. Two behaviours differ by construction rather than by accident: no allocation map (§1.1) and no hole punch (§1.7). |

### 6.9 CI reproducibility is bounded

Actions and cargo tools are pinned. `ubuntu-latest`, `windows-latest` and
`stable` are rolling by design; `msrv (1.95.0)` is the only fixed-toolchain job.
The Clippy feature matrix is a fixed list — no-default-features, default, each
optional feature alone, all-features — so a defect requiring a specific *pair* or
*triple* of features is not covered.

---

## 6.9 The editable header region: what its fixed size actually forbids

`header_slots` reserves a run of bytes in the file header that a caller can
rewrite for the life of the file. Its size is fixed and folded into the schema
hash, and three consequences follow that are limitations rather than details:

- **The capacity cannot be changed for an existing file.** Reopening under a
  different capacity is a `SchemaHashMismatch`. There is no grow path and no
  migration for it; growing the region would move the append log, which means
  moving every record in the file — which is republishing it. Size the region
  when you declare the format, using `header_slots_used()` after one write of
  each block you intend to hold.
- **The ceiling on the capacity is a reader-side declaration, so two readers of
  the same file can disagree about it.** The region shares the file-header
  extension budget, `ReadLimits::max_file_header_extension_len` — 64 KiB when
  nothing declares it, raised with `limits { header_extension: .. }`. A capacity
  above the budget is refused at create. But `ReadLimits` are deliberately *not*
  folded into the schema hash, so a reader that declares the default ceiling and
  opens a file whose region is 256 KiB is refused at open with no schema
  mismatch to explain it. The refusal is correct — an unbounded reader would let
  a hostile `u32` length field name a 4 GiB allocation before a block is parsed
  — but it means the ceiling has to be declared in every copy of the format
  declaration, and nothing checks that it was. Measured in
  `crates/varve/tests/header_extension_limit.rs`: the undeclared ceiling is
  still 64 KiB, an over-budget region without a declaration is refused at
  create, a declared ceiling carries a 256 KiB region through create, write,
  reopen and read, and a reader that declares the default ceiling is refused at
  open by the larger region with `Error::InvalidCompressionHeader` — before the
  schema-hash comparison, which is what makes "no schema mismatch to explain it"
  literal rather than figurative.
- **A format declaring matrix blocks cannot declare a region.** Refused at
  validation with `Error::InvalidFormatSpec`. The matrix creation nonce and the
  matrix layout header sit at fixed offsets *after* the file header, and a
  reserved region moves both. Lifting this is a matter of establishing that
  every consumer of those two offsets derives them from `header_len` rather than
  storing a constant; that walk has not been done, so this refuses rather than
  guesses — the same refusal, for the same reason, that `index: header_tails`
  carries.
- **An already-open handle sees the region as it was at its open, not live.**
  `read_header_block` decodes from the header bytes the handle read when it
  opened, which is exactly what lets it take `&self` and touch no I/O. A reader
  opened before a write keeps answering with the old value until it is reopened;
  a reader opened after it sees the new one. Measured, not inferred
  (`a_reader_opened_before_a_write_does_not_see_it`). This is the same snapshot
  model the rest of the read surface has, and it means the region is not a
  channel for pushing a changed value to live readers.
- **The region has no crash-atomicity guarantee across a write.** A write is a
  positional overwrite of the region followed by `sync_data`. A crash between
  the two leaves a region whose checksum does not match its bytes: under
  `integrity: rolling` and a sealed `sealed` region, reads report
  `Error::ChecksumMismatch` — which is the correct answer, but it is *detection*,
  not recovery. There is no second slot to fall back to, unlike the `VBTT`
  header-tail region, which carries two. A caller who needs the region to
  survive a crash mid-write with its previous value intact does not have that
  here.

What has been measured: the seventeen cases in
`crates/varve/tests/header_slots.rs`, plus the four in
`crates/varve/tests/header_extension_limit.rs` for the capacity ceiling, over all
three integrity policies —
reserve at create, survive a reopen, rewrite without growth, removal, refusal on
overflow, refusal on an undeclared block, refusal of a write after a seal,
verification on read under `rolling` and under a sealed `sealed`, non-verification
under `none` and under an unsealed `sealed`, the file length and record offsets
unchanged across an edit, and the region carried verbatim across a republishing
`replace`, and the reader-snapshot semantics above. What has **not** been
measured: the crash behaviour above — no fault injection covers this region, so
the statement that a torn write is *detected* rests on the checksum's
construction and on the corruption tests, not on an interrupted write.

## 7. Resource limits: what they do and do not bound

### 7.1 Limit accounting is nominal, not peak RSS

`ReadLimits` ceilings are charged against modelled sizes. They do not fully
account for container bucket and node overhead, the `8 * N` sequence-uniqueness
temporary, mmap index/map duplication, or auxiliary maps built during keyed
materialization. **A true peak-RSS ceiling belongs to an external process, cgroup
or job-object policy**, not to `ReadLimits`.

### 7.2 Some allocations abort instead of returning an error

Published deliberately. Content-sized allocations — those whose size a file can
choose — are charged to a `ReadLimits` ceiling and reserved fallibly, surfacing as
`Error::AllocationFailed` or `Error::LimitExceeded`. Shape-sized allocations —
those bounded by the program's own compile-time shape — use ordinary infallible
allocation and **abort the process** if the allocator refuses.

The consequence, written down rather than discovered: an allocator refusal *after*
a publication terminates the process rather than substituting a different outcome.
No caller ever observes a wrong outcome, and the on-disk state is the published
one.

`AllocatedExtents` is capped by a compile-time constant rather than by a
`ReadLimits` charge (open item 6) and grows with infallible `Vec::push`, so an
allocator refusal there aborts rather than returning a typed error.

### 7.3 `max_keyed_tail_bytes` is per block id

See §2.2. A format with `M` keyed block types can hold about `M *
max_keyed_tail_bytes`.

---

## 8. `unsafe` API contracts that cannot be checked

**`replace_fixed_in_place_exclusive` cannot verify that a key is unchanged**
(open item 15). At a `T: VarveBlock` signature there is no way to check it. The
API does guarantee unconditionally that the block's keyed-tail map is dropped
before the first byte is written. **If the contract is violated, the physical
`prev_same_key_offset` chain is unrepairable** — this path publishes no new
generation in which the chain could be rebuilt.

**Who it affects.** Callers of the `unsafe` in-place replacement on a
`keyed_offset_chain` format.

**Workaround.** Use `replace_rewrite` or `replace_block`, which publish a new
generation.

---

## 9. Contributor-facing notes (not reachable by users)

These are recorded for completeness and because they are self-reported in the
source rather than hidden. **A downstream crate cannot express any of them**, so
they are not user-facing limitations and are listed separately for that reason.

Three enforcement bypasses still compile *inside* `varve-core`. The catalogues are
at `crates/varve-core/src/matrix.rs:9022-9207`
("What still compiles from inside this file"),
`crates/varve-core/src/file.rs:12349-12447`, and
`crates/varve-core/src/writer_permit.rs:301`.

1. **`SparseBitmap` and its prepared values are declared at file scope**
   (`matrix.rs:1895`). `BitmapByteUpdate`, `CommitBitUpdate`, `PreparedByteWrite`
   and `PreparedWriteBit` are constructible by literal from anywhere in
   `matrix.rs`; a fabricated `PreparedByteWrite { fresh: None, .. }` reaches
   `commit_byte_write` for a non-resident page. Open item 26. The fix is a
   `mod sparse_bitmap`, which is a large refactor of the hot bitmap path.
2. **Commit maps are not behind an evidence type.** `MatrixCommitLayout::bits`
   (`matrix.rs:3488`) is a plain `SparseBitmap` field of a file-scope struct, so
   `layout.commits[i].bits = attacker_bits;` compiles from anywhere in
   `matrix.rs` and publishes a commit view nothing committed. Open items 25 and
   28. This is the one with teeth: it is the same class as F-04 — a fact a reader
   consumes as proof — with nothing between the fact and the assignment.
3. **`Box::leak(Box::new(PoisonFlag::healthy()))` still yields a
   `&'static PoisonFlag`** that a `GuardedWriter::poison_flag` implementation
   could return in place of the writer's own field, permitting every guarded
   operation on a poisoned writer. The `static DECOY` spelling was removed;
   the heap spelling is caught only by a source gate over the four `poison_flag`
   implementations. Open items 21 and 27.

**Why these are unreachable from outside the crate**, verified: `SparseBitmap` and
`MatrixCommitLayout` are private (`struct`, no `pub`). `MatrixLayout` is
`pub struct` at `matrix.rs:3440` but is **not** in `lib.rs`'s
`pub use matrix::{...}` list, so it has no external path. `PoisonFlag` is `pub`
inside the private `mod writer_permit` and escapes only via the `#[doc(hidden)]`,
`scalable-fault-injection`-gated `enforcement_probe` — whose own rustdoc says
"This is not API" — where `PoisonFlag::issue` is private and `GuardedWriter` is
not exported at all.

Two further catalogued items — `CrcValidEvidence::newly_created` and a
`MatrixBlockLayout` literal — launder in the **fail-closed** direction. They
cannot make a cell look verified, and are recorded for completeness rather than as
live risk.

Other contributor-facing items, each confirmed present in code:

- `MutationPermit` binds the writer's *type*, not its instance (open item 21). A
  permit held across an intervening poisoning call would still be accepted. Every
  cross-module entry point consumes it by value and re-takes it before each
  disk-touching step, so no live code holds a stale one — but that is a
  convention, not a type.
- `ResidentIndex::adopt_generation` is unchecked (open item 29). The cheap fix
  does not work: an extent-check replacement still admits
  `adopt_generation(vec![RecordIndexEntry { committed: true, .. }])` against a
  zero-length file, and a constructor that would refuse it adds `O(records)` I/O
  to every open that does not already scan.
- Three mirrors are safe by inspection rather than by type (open item 23):
  `disk_index.rs::store_tail_cache`, `stream.rs::set_tail`, and the matrix
  quarantine map. All are installed infallibly today; nothing stops a fallible
  step being added after their disk write.
- `matrix.rs`'s prepared page-index values have no `tests/ui` proof (open item
  22), deliberately: exposing them to prove them would destroy the module privacy
  that *is* the enforcement.
- `SparseBitmap: Clone` still exists (open item 19). The hand-written
  `impl Clone for SparseBitmap` is retained because **`MatrixCommitLayout`
  derives `Clone`** and holds a `SparseBitmap` field. It is *not* retained for
  `MatrixLayout`, which was deliberately stripped of its `Clone` derive in round
  16 (a clonable layout made `FatalAccessGate` clonable, and a clonable gate is an
  installable gate); the comment above `struct MatrixLayout` records that. The
  item is live, the reason previously given for it was stale.
- `diagnostics.rs::classify_error` is an exhaustive match over `Error` (open item
  11), so every new error variant forces an edit to a file the fixing round does
  not otherwise own. Kept on purpose as a forcing function.
- `format.rs`'s manifest region has never been walked by any review round (open
  item 12).
- The `#[cfg(test)]` sidecar publication interposition hook in `stream.rs` is
  process-global state keyed by canonical primary path (open item 4). It is
  compiled out of every non-test build. A test that arms it and then fails before
  the publication point leaves its entry armed for the rest of that binary's run;
  this is now visible — the next registration for the same path asserts — rather
  than silently changing another test's behaviour.
