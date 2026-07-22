# Known Limitations

Status as of 0.4.0 (2026-07-22). Every entry below was checked against the code
in this repository, and every number was measured on the host that produced this
document. Where an earlier document and the code disagreed, the code won and the
document was corrected.

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
`MatrixMetadataResidency::EagerVerified` policy, `open` reads, authenticates and
materialises every page in the *candidate set*: the union of the pages named by
the persisted page index (`L`, the pages actually holding state) and the pages
the platform's allocation map reports as written (`A`). Cost is `O(L + A)`.

`O(L + A)` is not `O(1)`, and neither `L` nor `A` is the working set. A reader
that intends to touch one cell pays for the whole candidate set.

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
   The ratio is 0.97x. This part of the design goal is met.
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

**Who it affects.** Anyone who opens matrix files frequently — short-lived
processes, request handlers, CLI invocations, per-file workers — or who opens
many matrices in one process. A long-running process that opens one matrix and
keeps it open pays this once and does not care.

**Workaround.** Two, both partial:

- Keep the handle open. Matrix reads take `&self` (§4), so one handle can serve
  the whole process, including concurrent threads.
- Opt into `MatrixMetadataResidency::Lazy { cache_bytes }` (see §1.3), which
  brings open down to 32 bytes read and 0 pages visited for the same fixtures —
  but has its own consequences, and is still `O(live pages)` in the page index.

**Planned.** Recorded as open item 30 in `docs/invariant-checklist.md`. Not
scheduled.

### 1.2 Resident metadata does not track the working set and is never evicted

**What it is.** Under the default `EagerVerified` policy, the bytes a matrix
holds resident after open are fixed by the candidate page set at open time.
Touching cells does not raise residency and not touching them does not lower it.
There is **no eviction path** under this policy: nothing that was materialised at
open is ever released while the handle lives.

Measured: 8,192 resident bitmap bytes for a 1-live-page matrix and 524,288 for a
64-live-page matrix, in both cases unchanged by which cells the caller
subsequently reads. Residency is roughly **8,192 bytes per live commit-map page**
(one commit page and one CRC-validity page, 4096 bytes each), plus 48 bytes per
page-index entry.

**Who it affects.** Anyone whose live page count is large relative to their
process memory budget, and anyone who assumed `max_matrix_bitmap_bytes` would
cap a cache. It does not.

**Workaround.** `MatrixMetadataResidency::Lazy { cache_bytes }` (§1.3) is the
only path that bounds residency by a declared ceiling and evicts LRU. Under the
default there is nothing to tune.

**Planned.** No eviction is planned for `EagerVerified`; eager verification and
eviction are contradictory by construction — a page dropped after verification
would have to be re-verified, which is what `Lazy` does.

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
budget_bytes  ~= 8,192 * live_pages + 48 * live_pages
              ~= 8,240 * live_pages
```

`live_pages` is driven by **scatter, not by cell count**. A dense matrix is cheap:
16,000,000 cells fully committed is 489 live pages, about 4 MB — comfortably
inside the 64 MiB default. A scattered matrix is expensive: 8,192 committed cells
that happen to land one per page is also 8,192 live pages, about 67 MB, and
**exceeds the default ceiling with 8,192 committed cells in the file**. If your
write pattern scatters, size the ceiling from the page count, not the cell count.

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
| Eviction | none | LRU, to the declared ceiling |
| Allocation-map query | yes | never |
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

### 1.5 Matrix dimensions are fixed at create time, with no grow path

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

Over-provisioning the matrix at create time is possible — create cost is bounded
by live state rather than by declared cell count — but be aware that the
allocation-map term in §1.1 and the scatter arithmetic in §1.3 both respond to a
larger declared extent.

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

`IndexPolicy::CheckpointOnFlush` lets an open start from a checkpoint instead of
a full scan. Note that the `scan_on_open` flag is **not a behaviour switch**: it
is folded into the schema manifest and hash bytes and is consulted nowhere else
in `varve-core`. Clearing it does not produce a non-scanning open.

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

**Workaround.** Use the scalable family. Note the caveats in §2.3 before doing so.

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
metadata. The previous method name, `peak_resident_bytes()`, was removed rather
than deprecated because its documented "upper bound" contract was false by an
arbitrarily large margin for any heap-owning key or value type.

**Planned.** An external, bounded-memory merge is not implemented and is not
scheduled.

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

| Subsystem | Reads take | One handle, many threads |
| --- | --- | --- |
| Matrix (`read_matrix_cell`, `matrix_cell_status`, `matrix_resume_signal`) on `VarveReader`, `VarveWriter`, `VarveFile` | `&self` | yes — the reader handle is `Send + Sync` |
| Resident record reads (`blocks`, `keyed_blocks`, `materialized_keyed_blocks`, `scan`, `index_entries`, `metadata`) on `VarveReader` | `&self` | yes |
| All mutation (`push*`, `delete*`, `replace_*`, `write_matrix_cell*`, `commit_matrix_cell`, `clear_matrix_*`, `flush`, `commit`, `sync`) | `&mut self` | no |

Matrix reads changed from `&mut self` to `&self` in this release. That is
source-compatible — an existing call through a `&mut` binding still compiles —
but it is what makes shared-handle concurrent reading possible.

**Measured concurrent matrix read scaling** (wall clock, lower is better; both
contract-asserted at `<= 1.0x`):

- 24,000 reads through one handle: 1 thread 0.138s vs 4 threads 0.046s — **0.33x**.
- Hostile lazy configuration (1-page cache, 32 live pages, 40,960 fault-in
  reads): 1 thread 3.814s vs 4 threads 1.667s — **0.44x**.

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

Everything in this section is a statement about **assurance**, not about a known
defect. It is here because a user is entitled to know which claims rest on
executed tests and which rest on inspection.

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

Additionally, `cargo clippy --target x86_64-unknown-linux-gnu -p varve
--no-default-features --lib -- -D warnings` currently **fails** with
`field 'handles' is never read` at `crates/varve-core/src/matrix.rs:220`, because
`MatrixReadPool::reopen` is `#[cfg(windows)]`-only and the field's other readers
are `#[cfg(test)]`. The same invocation passes on Windows. This is a lint failure,
not a behaviour defect, but it means the Linux build has not passed the project's
own gate.

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
  `fuzz/artifacts/codec_arbitrary/` from 2026-07-20. It is gitignored and will not
  be published, but it means an OOM in `codec_arbitrary` was found and has not
  been turned into a deterministic regression, and that
  `scripts/run-security-fuzz.ps1` refuses to start (exit 2) while it is present.
  **This release makes no fuzz-pass claim.**

### 6.5 Tests that can pass without proving anything

Named here so that a green run is not over-read:

- Both `pib_probe` tests (§6.2).
- The Windows lock-marker reparse-point test skips loudly where the host does not
  grant `SeCreateSymbolicLinkPrivilege`; the hard-link branch does run (open item
  10).
- The convoy scaling assertions in `matrix_concurrent_reads.rs` and
  `matrix_lazy_residency.rs` are skipped, with an announcement, on a single-core
  host.

### 6.6 What was verified

On Windows x86_64 MSVC, rustc 1.95.0, on 2026-07-22:

- `cargo test --workspace --all-features`: all green, ~490 tests across 42 test
  binaries plus 3 doctests.
- `cargo clippy --locked --all-targets --workspace --all-features -- -D warnings`:
  clean.
- The 10 `matrix_lazy_residency` tests and the 4 `matrix_concurrent_reads` tests,
  producing the numbers quoted in §1 and §4.

### 6.7 CI reproducibility is bounded

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
- `SparseBitmap: Clone` still exists (open item 19), retained only because
  `MatrixLayout` derives `Clone`.
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
