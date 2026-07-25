# Known Limitations

Status as of 0.4.0. Numbers measured 2026-07-22 and re-measured 2026-07-25; every
entry was checked against the code in this repository. Where an earlier document
and the code disagreed, the code won and the document was corrected.

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

This file is organised by what a **user** runs into, not by internal structure.
Each entry states what it is, who it affects, the workaround if one exists, and
whether it is planned work.

Companion documents:

- [API Changes](api-changes.md) — what to edit when upgrading, and what happens
  to files written by an older version.
- [Changelog](../CHANGELOG.md) — the release entry.
- [Invariant Checklist](invariant-checklist.md) — the contributor-facing list of
  consciously accepted internal items.

---

## 1. Matrix: opening a matrix is not O(1), and its metadata residency is not a cache

This is the largest single gap between what the matrix subsystem is designed for
and what it currently delivers. It is stated here in full because a user who
sizes a deployment on the design intent rather than on these numbers will be
wrong by a large factor.

### 1.1 Open cost is proportional to candidate pages, not to the working set

**What it is.** A matrix's commit metadata is stored as 4096-byte bitmap pages,
each page covering 32,768 cells of one category. Under the default
`MatrixMetadataResidency::EagerVerified` policy, `open` **reads and
authenticates** every page in the *candidate set*: the union of the pages named
by the persisted page index (`L`, the pages actually holding state) and the pages
the platform's allocation map reports as written (`A`). Cost is `O(L + A)`.

Reading is not the same as retaining. Of the pages it reads, open makes resident
**only those holding at least one set bit** (`matrix.rs`'s `insert_loaded_page`
returns early for an all-zero page). So the I/O is `O(L + A)` and the residency
is `O(L)`. This is why the 1-live-page fixture below reads 131,240 bytes but
holds only 8,192 resident — do not size resident metadata from the bytes read.

`O(L + A)` is not `O(1)`, and neither `L` nor `A` is the working set. A reader
that intends to touch one cell pays for the whole candidate set in I/O.

**Measured on this host, 2026-07-22** (`crates/varve/tests/matrix_lazy_residency.rs`,
all 10 tests passing):

| Fixture | Cells | File size | Live pages | Open bytes read | Pages visited | Resident bitmap bytes |
| --- | --- | --- | --- | --- | --- | --- |
| small | 2,097,152 | 17.3 MB | 1 | 135,336 | 33 | 8,192 |
| large | 8,388,608 | 69.2 MB | 1 | 131,240 | 32 | 8,192 |
| large | 8,388,608 | 69.2 MB | 64 | 591,384 | — | 524,288 |

Read those three rows in this order:

1. **Open is independent of file size.** Two matrices differing 4x in cell count
   and 4x in file size, with the same one-page live set, both read about 131 KB.
   The ratio is 0.97x. This part of the design goal is met — **on a sparse
   NTFS volume.** See the platform precondition below, which is not a footnote.
2. **Open is not O(1).** A matrix with **one live page** still reads
   **131,240 bytes** and visits **32 pages**. On NTFS the allocation map reports
   the bitmap region in ~128 KiB runs, so `A` is 32 pages even when `L` is 1.
   That 131 KB is a floor, not an average.
3. **Open grows with live pages.** Going from 1 live page to 64 takes the read
   from 131,240 to 591,384 bytes and residency from 8,192 to 524,288 bytes —
   exactly 64x the per-page figure. Nothing about the read is bounded by how many
   cells the caller intends to look at.

The gap to an O(1) open is about **32x in pages** at the smallest live set, and
grows linearly with the live set from there.

#### The numbers above are Windows/NTFS numbers, and `A` is filesystem-defined

This matters enough to state before the mitigations, because on two of the three
cases below the headline result is *different*, not merely less precise.

- **Windows and Linux, sparse volume (measured case).** `A` comes from
  `FSCTL_QUERY_ALLOCATED_RANGES` / `SEEK_HOLE`. On NTFS the ~128 KiB run
  granularity gives `A = 32` pages around a single live page, which is where the
  131 KB floor and the "~32x gap" come from. Linux extent granularity differs, so
  the constant differs; the shape does not.
- **Every other target.** `query_allocated_extents` returns `None`
  (`matrix.rs`, `#[cfg(not(any(windows, target_os = "linux")))]`), so the
  candidate set has **no `A` term at all** and open visits `L` pages only. Open is
  *cheaper* there than the table above shows, and the "32-page floor" does not
  describe those platforms. This is untested — see §6.1.
- **A volume that is not sparse.** Sparseness is requested best-effort. On
  Windows the file must carry the sparse attribute *before* it is extended, and
  `mark_file_sparse` **ignores failure** by design ("the matrix is then simply
  dense, which costs performance and nothing else"). On exFAT/FAT32, some
  network and virtual volumes, or after a copy by a tool that expands holes, the
  declared extent is genuinely allocated — the allocation map then reports the
  **whole** bitmap region as data, `pages_to_visit` returns every page, and eager
  open degrades to `Theta(declared cells / 8)`. **On a non-sparse volume, open
  cost is proportional to the declared cell count, not to live state, and the
  "independent of file size" result in point 1 does not hold.** There is no error
  and no warning; the only signal is that open is slow and reads a lot.

The same precondition governs the create-time claim: metadata I/O at create is
bounded by live state because the unwritten extent is a hole. Where it is not a
hole, `set_len` allocates what it reserves.

**Who it affects.** Anyone who opens matrix files frequently — short-lived
processes, request handlers, CLI invocations, per-file workers — or who opens
many matrices in one process. A long-running process that opens one matrix and
keeps it open pays this once and does not care.

**Workaround.** Two, both partial:

- Keep the handle open. Matrix reads take `&self` (§4), so one handle can serve
  the whole process, including concurrent threads.
- Opt into `MatrixMetadataResidency::Lazy { cache_bytes }` (see §1.4), which
  brings open down to 32 bytes read and 0 pages visited for the same fixtures —
  but has its own consequences, and is still `O(live pages)` in the page index.

**Planned.** Recorded as open item 30 in `docs/invariant-checklist.md`. Not
scheduled.

### 1.2 Resident metadata does not track the working set, and reading never releases it

**What it is.** Under the default `EagerVerified` policy, the bytes a matrix
holds resident after open are fixed by the live pages in the candidate set at open
time. **Reading does not change residency in either direction:** touching cells
does not raise it and not touching them does not lower it.

There is **no read-driven eviction**. Residency does fall on one path, and only
one: when a **mutation** clears a page's last set bit, that page is released and
its bytes are refunded (`release_resident_bitmap` / `settle_page_delta`; PERF-02,
asserted over four set/clear cycles in
`crates/varve/tests/matrix_integrity_scaling.rs`). So a writer that clears state
gets the memory back; a reader has no way to release anything, and a page whose
bits are still set is never dropped while the handle lives.

Measured: 8,192 resident bitmap bytes for a 1-live-page matrix and 524,288 for a
64-live-page matrix, in both cases unchanged by which cells the caller
subsequently reads. And measured on the *working-set* question directly, at 16
live pages: `resident_after_open = 131,072`, after touching 1 page `131,072`,
after touching 4 pages `131,072`. **Residency tracks live pages, not the working
set.**

Residency is roughly **8,192 bytes per live commit-map page** (one commit page and
one CRC-validity page, 4096 bytes each), plus **96 bytes** per live page of
page-index overhead — see the arithmetic in §1.3, and note that the 8,192 assumes
checksums are on.

**Who it affects.** Anyone whose live page count is large relative to their
process memory budget, and anyone who assumed `max_matrix_bitmap_bytes` would
cap a cache. It does not.

**Workaround.** `MatrixMetadataResidency::Lazy { cache_bytes }` (§1.4) is the
only path that bounds residency by a declared ceiling and evicts LRU. Under the
default there is nothing to tune. Note the trap in §1.6: passing a `ReadLimits`
value to a `*_with_resource_limits` entry point **silently reverts** a
spec-declared `Lazy` back to `EagerVerified`.

**Planned.** No read-driven eviction is planned for `EagerVerified`; eager
verification and read-driven eviction are contradictory by construction — a page
dropped after verification would have to be re-verified, which is what `Lazy`
does.

### 1.3 `max_matrix_bitmap_bytes` is an admission limit, so a matrix can become unopenable

**What it is.** Under `EagerVerified`, `max_matrix_bitmap_bytes` does not make
the resident set smaller. It decides whether the open is allowed at all. Setting
it below what the matrix's live set requires makes `open` **fail**:

```text
Error::LimitExceeded { resource: "matrix bitmap bytes", actual: 17152, limit: 16384 }
```

**The consequence, stated plainly: a matrix whose committed state exceeds the
configured ceiling cannot be opened at all.** Not opened slowly, not opened with
a smaller cache — refused. There is no read-only, reduced-fidelity, or partial
open under this policy. If the ceiling is a compiled-in declaration and the file
grew past it in production, the file is unreadable by that build.

Two details that will otherwise cost debugging time:

- The `actual` field is the running total **at the moment the ceiling was
  crossed**, not the amount the open needed. In the example above the matrix had
  16 live pages, needing roughly 131 KB; `actual` reports 17,152 because that is
  where the charge tripped. Do not size a ceiling from `actual` — compute it (see
  the arithmetic below) and add margin.
- The default `ReadLimits::STANDARD` ceiling is **64 MiB**, and
  `ReadLimits::UNTRUSTED` inherits it.

**Arithmetic you can apply to your own matrix.** For one matrix category:

```text
cells_per_page = 32,768
live_pages     = number of distinct 32,768-cell windows containing >= 1 committed cell
budget_bytes  ~= 8,192 * live_pages   # one commit page + one CRC-validity page
               +   96 * live_pages    # page-index overhead, in *two* bitmaps
              ~= 8,288 * live_pages
```

The 96 is not a typo for 48. Each live page carries a page-index entry in **both**
the commit map and the CRC-validity map, and each entry is modelled at 48 bytes
(`PAGE_INDEX_SLOT_RESIDENT_BYTES` 16 + `PAGE_INDEX_MAP_RESIDENT_BYTES` 32).
Measured `matrix_resident_page_index_bytes()`: **1,536 bytes at 16 live pages** =
96 per page. The correction is 0.6% of the total, so it will not change a sizing
decision — it is corrected because this section invites you to compute a ceiling
with it.

**Both terms assume `IntegrityPolicy::Crc32`.** With checksums off there is no
validity bitmap, so there is one page rather than two and one page index rather
than two: both the open bytes and the residency roughly **halve**, to about
`4,144 * live_pages`.

`live_pages` is driven by **scatter, not by cell count**. A dense matrix is cheap:
16,000,000 cells fully committed is 489 live pages, about 4 MB — comfortably
inside the 64 MiB default. (16,000,000 is also exactly the default
`max_matrix_cells` ceiling; see §1.5. It is used here as the largest matrix the
defaults admit, not as an arbitrary large number.) A scattered matrix is
expensive: 8,192 committed cells that happen to land one per page is also 8,192
live pages, about 68 MB, and **exceeds the default ceiling with 8,192 committed
cells in the file**. If your write pattern scatters, size the ceiling from the
page count, not the cell count.

The candidate set at open also includes the allocation-map term `A`, which is a
property of how the filesystem allocated the file rather than of your data. Treat
the computed figure as a lower bound.

**Who it affects.** Anyone with a scattered commit pattern, anyone running under
a tightened `max_matrix_bitmap_bytes`, and anyone whose matrices grow over time
under a fixed declared limit.

**Workaround.** Raise `max_matrix_bitmap_bytes` for the handle with
`*_with_resource_limits`, or switch that handle to
`MatrixMetadataResidency::Lazy { cache_bytes }`, under which a live set larger
than the ceiling becomes *openable* rather than refused.

**Planned.** No change to the admission semantics is planned. `Lazy` is the
supported answer.

### 1.4 What the `Lazy` policy does and does not fix

`MatrixMetadataResidency::Lazy { cache_bytes }` is **opt-in and not the default**.
It has never shipped in a released version. Measured on the same fixtures:

| | `EagerVerified` (default) | `Lazy { cache_bytes }` |
| --- | --- | --- |
| Open bytes read, 1 live page | 131,240 | **32** |
| Commit-map pages visited at open | 32 | **0** |
| Open bytes read, 64 live pages | 591,384 | **1,040** |
| Resident bytes after open | 8,192 (1 page) / 524,288 (64 pages) | **0** |
| Resident after touching 1 page | unchanged | 4,096 |
| Resident after touching 4 pages | unchanged | 16,384 (the declared ceiling) |
| Resident after touching all 64 live pages | unchanged | 16,384 (still the ceiling) |
| Eviction | none on the read path (a mutation that clears a page's last bit does refund it) | LRU, to the declared ceiling |
| Allocation-map query | yes (where the platform has one) | never |
| Corruption detected at | open | first touch of the damaged page |

**What `Lazy` does not fix.** Open still reads the **persisted page index in
full**, under both policies. That is 8 bytes per live page — 512x smaller than
the eager path, but still `O(live pages)`, not `O(1)`. At one million live pages
a lazy open reads 8 MB before returning.

**Three consequences that are part of the declaration, not defects.** Choosing
`Lazy` is choosing all three:

1. **Corruption detection moves from open to first touch.** A page whose bytes
   disagree with its stored digest is reported as `Error::MatrixFatalCorruption`
   by the read that touches it, not by `open`. Pages never touched are never
   checked. Use `EagerVerified` where `open` must be the detection point.
2. **A page absent from the persisted index still reads as clear.** That is a
   fact the file supplied, not a guess: the index is loaded in full under both
   policies and is authoritative. "Not cached" and "not published" stay distinct.
3. **A lazy reader does not see one consistent instant.** A page's contents are
   as of the first touch that faulted it in, not as of open. Use `EagerVerified`
   where a pinned snapshot across all pages is required.

`cache_bytes` above `max_matrix_bitmap_bytes` is refused at open, so the option
cannot be used to raise a declared ceiling.

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

Read §1.6 before choosing the `*_with_resource_limits` form.

**Planned.** No change. A default that admits an unbounded declared extent is not
wanted; the defaults are documented here instead.

### 1.6 `*_with_resource_limits` silently discards a declared residency policy

**What it is.** `MatrixMetadataResidency` travels inside `ReadLimits`, but it is
not a *limit* — it is a declaration, and `ReadLimits::overlay` takes the
runtime value **unconditionally** rather than combining it. `with_resource_limits`
is `resolve().overlay(limits)`. So:

```rust
// The format declares Lazy { cache_bytes: 1 << 20 } as its default.
// This call reverts it to EagerVerified, because ReadLimits::STANDARD
// carries EagerVerified and overlay takes the runtime value outright.
let reader = AppFormat::open_reader_with_resource_limits(
    path,
    ReadLimits::STANDARD.with_max_matrix_bitmap_bytes(n),
)?;
```

The caller raising a bitmap ceiling loses the only mechanism that bounds matrix
metadata memory (§1.4), and can then be refused at open by the very admission
limit they were raising (§1.3). Nothing reports this.

The two entry-point families differ, deliberately and asymmetrically:

| Entry point | What happens to the format's residency policy |
| --- | --- |
| `*_with_resource_limits` (`overlay`) | **replaced** by the value in the passed `ReadLimits`, always |
| `*_with_limits` (`tighten`, the legacy fieldwise form) | **kept** |
| plain `open`/`create` | kept |

**We do not think this is wrong as designed** — a residency policy has no
"tighter" direction to compose, so a runtime overlay has to choose one outright,
and `tighten`'s own source comment says so. It is listed as a limitation because
it is undiscoverable: the signature does not change, the failure is silent, and
the field the caller never mentioned is the one that moves.

There is also **no `varve_format!` DSL key for residency.** `limits { }` accepts
`key_index`, `disk_index_plan` and `keyed_tail`, but not
`matrix_metadata_residency`. `Lazy` is reachable only by passing it in the same
`ReadLimits` value you hand to the entry point:

```rust
let limits = ReadLimits::STANDARD
    .with_matrix_metadata_residency(MatrixMetadataResidency::Lazy { cache_bytes: 1 << 20 })
    .with_max_matrix_bitmap_bytes(n);   // both in one value, or the first is lost
```

**Who it affects.** Anyone who declares a residency policy on the spec *and*
passes resource limits at an entry point. This is also a behaviour change at an
unchanged signature for anyone who declared limits on a spec before 0.4.0, since
the field did not exist then; see [API Changes §5](api-changes.md).

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

**Planned.** No additional platform is scheduled. The cost model is stated in
[Performance](performance.md) as well; it is repeated here because this file is
the one organised by what a user hits.

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

Over-provisioning the matrix at create time is possible, but three things bound
how far:

- Create cost is bounded by live state rather than by declared cell count **only
  on a filesystem that gives you holes** — see the precondition in §1.1. Where it
  does not, `set_len` allocates the whole declared extent.
- `max_matrix_cells` defaults to 16,000,000 and is checked per block *and*
  aggregated across blocks (§1.5), so over-provisioning needs an explicit raise.
- The allocation-map term in §1.1 and the scatter arithmetic in §1.3 both respond
  to a larger declared extent.

**Planned.** No grow path is planned. Growth is what the stream/indexed family is
for.

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

**Every open scans the whole record region, and no policy changes that.** This
correction matters because the previous version of this document offered a
mitigation that does not exist:

- `IndexPolicy::CheckpointOnFlush` does **not** seed an open from a checkpoint.
  Every open path calls `load_index`, which calls `scan_records_from`, which walks
  from `header_len` to `file_len` unconditionally. A checkpoint met during that
  walk is *validated* (`inspect_index_checkpoint`) and its decoded entries are
  discarded. There is no public checkpoint-seeded open. What the policy actually
  bounds is the writer side: it spaces full index checkpoints geometrically, which
  bounds cumulative checkpoint **bytes written**, not open cost.
  `docs/spec.md` describes the checkpoint-seeded open as a design target; it is
  not implemented.
- The `scan_on_open` flag is **not a behaviour switch** either: it is folded into
  the schema manifest and hash bytes and is consulted nowhere else in
  `varve-core`. Clearing it does not produce a non-scanning open.

**How much RAM an open takes, so you can answer this before running it.** The
resident index is a `Vec<RecordIndexEntry>`, and `index_bytes_for_count` — the
same function that charges `max_index_bytes` — computes
`count * size_of::<RecordIndexEntry>()`. On a 64-bit target that is **104 bytes
per record**:

```text
resident_index_bytes ~= 104 * records
```

So 10,000,000 records is about 1.04 GB of index before any decoded payload, and a
log with 400,000,000 records needs about 41.6 GB. Add transient growth slack: the
`Vec` is grown incrementally, so the peak can be up to twice the resting size
during the scan. That figure, not the file size, is what decides whether a
resident open fits.

`ReadLimits::STANDARD` leaves `max_file_len`, `max_records`, `max_index_bytes`
and `max_scan_bytes` at `u64::MAX`, so the **default profile places no ceiling on
resident index size**. `ReadLimits::UNTRUSTED` makes them finite (16 GiB file,
16,000,000 records, 1 GiB index, 16 GiB scan, 65,536 segments, 256 MiB keyed
tail) and is the right default for attacker-supplied input.

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

**Planned.** Open item 3 in `docs/invariant-checklist.md`. Not scheduled.

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
| All mutation (`push*`, `delete*`, `replace_*`, `write_matrix_cell*`, `commit_matrix_cell`, `clear_matrix_*`, `flush`, `commit`, `sync`) | `&mut self` | no |

The three relaxations are source-compatible — an existing call through a `&mut`
binding still compiles — but they are what makes shared-handle concurrent reading
possible.

**Measured concurrent matrix read scaling** (wall clock, lower is better; the
contract asserted in the tests is only `<= 1.0x`, i.e. "does not serialise"):

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
the file object, so a shared handle is expected to be adequate — but **the two
concurrent-read scaling contracts have never been executed on Unix**. See §6.

One lock does exist in the matrix read path (open item 31): `SparseBitmap` holds
a `Mutex<PageStore>` so a demand fault-in can happen under `&self`. It is taken
only for O(1) map operations, is released across the fault-in read, and is never
taken on the write path. Under the default `EagerVerified` policy nothing is ever
faulted in, so it buys nothing there — but it is a shared point every reader of a
category touches on every read.

### 4.2 Where a reader gets a snapshot rather than live state

Varve is **single-writer**, enforced by a native object lock on the target file.
Within that:

- **Resident record reads are a snapshot as of open.** The record index is built
  at open. Records a writer appends afterwards are not visible to that reader
  handle without reopening.
- **Matrix commit status under `EagerVerified` is a snapshot as of open.** The
  commit bitmaps are materialised at open; a cell another handle commits
  afterwards continues to read as uncommitted through the open reader.
- **Matrix commit status under `Lazy` is not a snapshot at all.** Each page is as
  of the first touch that faulted it in, so different pages can reflect different
  instants and a page faulted in late can reflect a writer's later work. This is
  consequence 3 in §1.4.
- **Matrix cell payload bytes are read positionally at read time**, so payload
  reads see current file contents even where the commit bitmap does not.
- **Stream and indexed readers open from a published sidecar generation** and see
  that generation, not the writer's in-flight state. `sync()` publishes.

**Who it affects.** Anyone assuming a Varve handle is a live view of a file
another handle is writing. It is not.

**Workaround.** Reopen to advance a resident or eager-matrix reader.

**Planned.** No live-view mode is planned.

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

**What it is.** `docs/channel-view-design.md` (with
`channel-view-design-cases.md` and `channel-view-design-layouts.md`) describes an
option model for reading a subset of channels without paying full block I/O.

**It is a design document, not a feature.** There is no `channel_view` type, no
DSL key, and no generated method anywhere in `varve-core` or `varve-macros`.
Nothing in that document is callable.

One conclusion from it is worth carrying here because it is a property of the
current on-disk layout rather than of the unimplemented feature: **an interleaved
payload cannot be read channel-selectively below full block I/O.** If your data
is interleaved, no future option will make a single-channel read cheaper than a
whole-block read without re-emitting the payload.

**Who it affects.** Anyone who read the design document and planned around it.

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

Two exceptions, both defects in the project's own gates rather than in the
library: the Linux Clippy lint failure in §6.1, and the default-feature test
failure in §6.6. Neither changes any library behaviour, and both are named rather
than omitted.

### 6.1 The Unix code paths have never been executed

Nothing in this release has been pushed to any remote, so **CI has never run on
this code, on either operating system**. The Linux job is the only thing that
would exercise:

- `diagnostics.rs`'s `openat`/`unlinkat` exclusive-directory cleanup,
- `file.rs`'s `O_NOFOLLOW` lock-marker open,
- the matrix concurrent-read path without the Windows private-handle pool (§4.1).

These are **compile-verified only** (open item 8). Cross-compilation to
`x86_64-unknown-linux-gnu` succeeds except for `compression-zstd`, which needs a
Linux C toolchain that was not available on the host that produced this document —
so the zstd path on Unix is not even cross-compile-verified.

`cargo clippy --target x86_64-unknown-linux-gnu -p varve --no-default-features
--lib -- -D warnings` previously failed with `field 'handles' is never read` at
`crates/varve-core/src/matrix.rs`, because `MatrixReadPool::reopen` is
`#[cfg(windows)]`-only and the field's other readers are `#[cfg(test)]`. The
field is an ownership anchor rather than dead weight — it holds the `Arc<File>`s
whose `Weak`s live in the thread-local cache — so it now carries
`#[cfg_attr(not(any(windows, test)), allow(dead_code))]` with that reason
recorded at the declaration. The invocation passes on both targets as of
2026-07-25. That is a cross-compiled lint pass, not an executed Linux test: the
paths listed above are still compile-verified only.

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
- The recorded fuzz evidence in `docs/fuzzing-and-fault-injection.md` is dated
  2026-07-11 (four core targets) and 2026-07-18 (three sidecar targets). Both
  predate several rounds of change to `matrix.rs`, `file.rs`, `codec.rs` and
  `format.rs`. The cited ASan run covered 21 library tests; the library suite is
  now 139.
- **No fuzz campaign, no Miri run and no ASan run has been performed against this
  release's code.**
- An unpromoted libFuzzer OOM reproducer exists in the working tree at
  `fuzz/artifacts/codec_arbitrary/oom-53bc…` from 2026-07-20. It is gitignored
  and will not be published, and `scripts/run-security-fuzz.ps1` refuses to start
  (exit 2) while it is present.

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

### 6.6 What was verified, and the one gate that is red

Full gate run on Windows x86_64 MSVC, 2026-07-25. Everything below was executed;
numbers are from that run.

| Gate | Result |
| --- | --- |
| `cargo fmt --all -- --check` | clean |
| `cargo check --workspace --all-features --all-targets --locked` | clean |
| Clippy `-D warnings`, 9 configurations (all-features, default, no-default-features, and each of the 6 optional features alone) | clean, 9/9 |
| `cargo doc`/`rustdoc -D warnings`, 3 crates x {all-features, no-default-features} | clean, 6/6 |
| `varve-test-runner test --workspace --all-features` | **648 passed, 0 failed, 8 ignored** across 49 binaries; artifact cleanup verified empty |
| `varve-test-runner test --workspace` (default features) | **168 passed, 1 FAILED, 1 ignored** — see below |
| `cargo test -p varve-core --all-features --lib`, five consecutive runs | 139 passed each time, 0 failed; no flake |
| `rename-fixture` and `public-api-fixture`, built and run | both OK |
| `cargo deny check` + `cargo audit`, root and fuzz workspace; fuzz `cargo metadata --locked`, `cargo check --locked --all-targets`, lockfile drift | all clean; 79 and 50 crate dependencies scanned |
| `cargo package --locked` for all three crates, verification enabled | all three archives built and **verified**; each contains `README.md`, `LICENSE-MIT`, `LICENSE-APACHE` |
| Consumer compiled and run against the extracted `.crate` archives | OK |
| Clean-tree copy (no `target/`, no VCS), `cargo metadata --locked` + `cargo check --workspace --all-features --all-targets --locked` | clean |

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
§6.1 — but note what that does and does not mean. Each was re-measured
individually on this Windows host after its fix, not as part of a fresh full run
of the table above, and the Linux result is a cross-compiled lint rather than an
executed Linux test. The table's remaining rows are from the 2026-07-25 run and
have not been re-executed since. They are stated this precisely because "all
green" was the shape of every previous over-claim in this project's history.

Also verified in this run, and quoted in §1 and §4: the 10 `matrix_lazy_residency`
tests, the 4 `matrix_concurrent_reads` tests, and
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
| Windows x86_64 MSVC | the only platform on which anything in this release has been executed. Every number in this document is from here. |
| Unix (Linux) | **compile-verified only** (§6.1). Cross-compilation succeeds except `compression-zstd`. No test, gate or measurement has run. |
| macOS / other targets | compile paths exist, nothing executed. Two behaviours differ by construction rather than by accident: no allocation map (§1.1) and no hole punch (§1.7). |

### 6.9 CI reproducibility is bounded

Actions and cargo tools are pinned. `ubuntu-latest`, `windows-latest` and
`stable` are rolling by design; `msrv (1.95.0)` is the only fixed-toolchain job.
The Clippy feature matrix is a fixed list — no-default-features, default, each
optional feature alone, all-features — so a defect requiring a specific *pair* or
*triple* of features is not covered.

---

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
