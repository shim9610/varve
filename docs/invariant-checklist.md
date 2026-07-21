# Varve Invariant Checklist

Status: living document. Last pass 2026-07-21, against the working tree on
`codex/dev-next` at the round-12 pass (parent commit `f661f65`, review
`docs/performance-stability-review-2026-07-21-f661f65-final.md`). The previous
pass was the round-9-through-11 work on parent commit `8732e83`, and before
that the round-8 re-verification (parent commit `4ed3280`).

**Round 9's finding about this checklist.** All three of round 8's release
blockers were the *same* invariant: number 3, a fallible or user-code-invoking
step after the authoritative commit. Rounds 5 through 8 had each fixed one
*instance* of it and left the class open, which is why every round found the
next one. Round 9's rule was therefore to enumerate the class rather than patch
the named line: for every authoritative commit in the code being touched — an
append, a disk write, a rename/publish, a sequence or snapshot update — list
every step that runs after it and state whether it can fail, allocate, or
invoke user code. Those enumerations are recorded under
[The class-3 enumeration](#the-class-3-enumeration) below and must be extended,
not restarted, by the next round.

**Round 10's pass: the exhaustive class-3 sweep.** Round 9 enumerated the class
inside the three files its findings named. Round 10 extended the enumeration to
every module that holds a commit point, and the tables under
[The class-3 enumeration](#the-class-3-enumeration) now carry per-module
sections for `disk_index.rs`, `stream.rs`, `indexed.rs` and `layout.rs` as well.
Stated honestly, so the next round knows what it is inheriting:

- **Enumerated site by site, reading each one:** every `commit()`, `sync_all`,
  `sync_data`, `flush`, `set_len`, atomic replacement and native append in
  `file.rs`, `matrix.rs`, `disk_index.rs`, `stream.rs`, `indexed.rs`,
  `layout.rs` and `varve-macros/src/lib.rs`. One defect was found and fixed
  (`commit_durable`); three sites absent from the round-9 table were walked and
  classified safe (`write_keyed_values_atomically`, `write_matrix_sidecar_file`,
  `truncate_uncommitted_tail_if_needed`).
- **Confirmed to hold no commit point:** `native_layout.rs` (pure encoding),
  `snapshot.rs`, `scan_control.rs`, `scalable_extent.rs`, `adapter.rs`,
  `codec.rs`, `collections.rs` (read-side or in-memory only), and - added in
  round 11 so the cover is actually closed rather than merely presented as
  closed - `merge.rs`, `chunks.rs`, `traits.rs`, `error.rs` and the *writer*
  side of `format.rs`, all verified to hold no `commit`/`sync_all`/`sync_data`/
  `flush`/`set_len`/`rename`/`write_all` call at all. `diagnostics.rs` mutates
  the filesystem only through the cleanup paths already covered by open items 5
  and 8.
- **Correction to the round-10 statement (round 11).** `pib_probe.rs` was listed
  above as read-side or in-memory only. That is false: it writes a native frame
  and calls `file.sync_all()`. The consequence is nil - the file is a throwaway
  probe artifact and nothing follows the sync - but the claim contradicted the
  code, which is itself an invariant-4 defect, so it is corrected here rather
  than left standing. The two remaining modules appeared in neither list:
  `lib.rs` holds no durability call, and `scalable_fault.rs` holds one
  (`trace.sync_data()` in the fault-point writer) which is compiled only under
  the `scalable-fault-injection` test feature and writes a test trace file, not
  a persisted structure.
- **Sampled, not exhaustive:** the *reader* side of `format.rs` and
  `disk_index.rs`. They were checked for the specific shape (a persisted
  structure republished in place) and for nothing else.
- **A rejected fix, recorded rather than hidden.** The review's F-03 offered a
  second correction — "poison the writer on every error after on-disk mutation
  begins" — and round 10 implemented it, as a per-mutation flag set by every
  writing function in `matrix.rs` and read by `finish_matrix_mutation`. It was
  then **reverted on evidence**: it fails
  `matrix_integrity_scaling.rs::a_failed_page_allocation_cannot_leave_a_bit_on_disk`,
  because round 9 took the *first* correction instead, and that makes every
  matrix disk write pair with an infallible in-memory install. A typed refusal
  raised between two such sub-steps leaves file and memory agreeing, so
  poisoning there converts a clean, retryable refusal into an unusable writer.
  The narrow `Error::Io` predicate is therefore correct *given* the prepare-
  before-persist design, and `finish_matrix_mutation`'s rustdoc now says so and
  names the obligation that replaces it: keep the pairing. **Do not "fix" this
  by widening the predicate.**

**Round 11's pass: the two rows that were not true.** Round 10's verifier
accepted the `commit_durable` fix and the enumeration's coverage, and then found
two live instances of the class still open on published batch-append paths -
inside two functions the tables described as fully typed. The lesson recorded
here is narrower than "enumerate the class": *an enumeration row is a claim, and
a false row is worse than a missing one*, because the next round inherits it as
an audit. Round 11 therefore fixed the instances **structurally** - the
classification now wraps the whole body of `commit_state_chunk` and
`commit_pending_batch`, so a fallible step added to either later is covered
without anyone remembering to wrap it - rewrote both rows, and walked forward
from the same commit points to find and close a third instance nobody had named
(the batch-summary arithmetic that ran after the native write at four call
sites). The secondary corrections the verifier asked for are folded into the
statements above and the rows below.

**Round 12's pass: enumeration was retired in favour of enforcement.** Round 10
ran an "exhaustive invariant-3 sweep" whose stated purpose was to enumerate
every authoritative commit point and classify every step after it. It rewrote
736 lines of `matrix.rs`, reported one new defect, and the class was declared
closed. Round 12's F-03 was `compact_page_index` — in that file, introduced by
round 4's own fix, with exactly the shape the sweep was hunting, mentioned once
in passing by the sweep's diff.

The conclusion drawn, and the reason this document changes shape here: **reading
does not close this class.** A reading pass will keep missing instances, and
every function written next month is a fresh chance to reintroduce the shape.
So round 12 stopped adding rows that say "this site is safe" and started making
the unsafe shape impossible to express. The two shapes and the types that now
forbid them are in
[Mechanically enforced shapes](#mechanically-enforced-shapes) below, which is
the section to read *before* the enumeration: the enumeration is now a record of
what was walked, while that section is the thing that holds when nobody walks
anything.

Round 12's *enforcement pass* then made those mechanisms universal instead of
per-finding, and proved they bind. Each fix agent had built its type for the one
module its finding named, which is the same trap in a new coat — the class stays
open in every module nobody was assigned. The pass promoted the shape-B type
into a crate-level module and put `file.rs` and `layout.rs` on it, added the
shape-A type the append path was missing, found and deleted a fourth poison
flag that could never fire, and added three `trybuild` compile-fail fixtures
plus three source-level gates so that the properties are checked by CI rather
than by the next reviewer's attention. What remains enforced by review only is
listed explicitly at
[What is mechanically enforced, and what is not](#what-is-mechanically-enforced-and-what-is-not);
a named deferral is acceptable, a silent gap is not.

**Scope of the inventory below, stated honestly.** It is a rounds-3-to-5
inventory plus the round-6/7 work, not a rounds-1-to-5 inventory. It was built
from the review reports and `git log`, but the six modules that rounds 1-2
(`28a1b68`) added - `disk_index.rs` (the redb-backed sidecar persisted store),
`stream.rs`, `indexed.rs`, `scan_control.rs`, `pib_probe.rs`,
`scalable_extent.rs` - are covered only by the narrow slices named in the tables
(sidecar identity/generation versions, the shared `Database` registry, the
checkpoint cadence, the `TailCache`). **Not yet walked:** the sidecar's own
persisted state and damage semantics, the stream checkpoint sidecar,
`StreamResidentState` / `StreamReaderState`, and the `scan_control` budget
model. A future round inheriting this table must not read the absence of a row
as an audit. See open item 7.

## Why this document exists

Five consecutive adversarial reviews found defects almost exclusively in code
this project **added while fixing an earlier finding**, not in the original
codebase:

| Round | Report | Blockers landing in code added by an earlier round |
| --- | --- | --- |
| 1 | `docs/adversarial-performance-security-review-2026-07-19.md` | - |
| 2 | `docs/adversarial-performance-security-review-2026-07-19-opus-final.md` | - |
| 3 | `docs/adversarial-performance-security-review-2026-07-19-4e07a3f-final.md` | some |
| 4 | `docs/performance-stability-review-2026-07-20-0d4b9c6-final.md` | most |
| 5 | `docs/performance-stability-review-2026-07-20-060851a-final.md` | **all six** |

Round 5's six release blockers map one to one onto patches from rounds 3-4:
F-01 to the resident keyed-tail cache (round 4's PERF-05), F-02 to the hash-cost
model (round 4's SAFE-01), F-03/F-06 to the matrix page index (round 4's
PERF-01), F-04 to the merge estimate (round 3's PERF-03), F-05 to the self-test
cleanup (round 3's API2-02).

The pattern is not carelessness in any single patch. It is that a fix was
reviewed against the finding it answered, and not against the properties every
structure in this library has to have. This checklist makes those properties
explicit and enumerable.

## The five invariants

Every **new** persisted structure, in-memory cache or registry, cost/limit
model, and published contract must satisfy all five before it merges.

### 1. BUDGETED

Every allocation is charged to a runtime resource limit **before** the memory is
taken - not merely guarded by `try_reserve`.

`try_reserve` converts an allocation the allocator *refuses* into a typed error.
It says nothing about an allocation the allocator will happily serve. A
structure whose size is chosen by file content, and which is only `try_reserve`
guarded, is unbounded by policy: no configured limit can refuse it. That is what
F-03 was.

The exception that does **not** need a charge is a structure whose size is
bounded by the program's own compile-time shape (declared block count, declared
type set) rather than by file content or by a caller-supplied count. Such
structures must be identified as such, with the bound stated, not assumed.

### 2. RESILIENT

Every persisted structure yields a **typed finding** on damage - never silent
truncation, never hidden data.

The failure shape to hunt for is *"damage looks like a clean end"*: a
terminator, a sentinel, a zero value, or a length that a corrupt region can
forge, such that a damaged prefix silently hides a valid suffix. That is what
F-06 was. A persisted index needs an occupancy count or high-water mark that is
itself validated, and enumeration must continue past a damaged entry and report
it rather than stopping.

### 3. ATOMIC

Every fallible step is ordered **before** the authoritative commit it supports,
or its failure returns a typed *published* outcome rather than a bare `Err`.

A caller that receives `Err` is entitled to believe nothing happened. A cache,
index, or registry updated after an append, publish, or rename must therefore
either be reserved beforehand and committed infallibly, or must report through a
typed published-outcome variant. That is what F-01 was, and the additional trap
it exposed: an update that fails *after* the append can leave a stale
predecessor that makes the next mutation link **around** the record that
succeeded.

### 4. TRUTHFUL

Every documented bound, complexity statement, or guarantee matches the
implementation exactly.

An "upper bound" that is an estimate is a defect, not a wording problem: callers
size buffers and set policy from it. Re-derive the real complexity and compare;
do not carry a claim forward because it was true when it was written. That is
what F-04, F-07 and F-08 were. Source comments count: a comment asserting a
property the code does not have is the same defect in a smaller font.

A correction pass is not finished when the claim is fixed in the file the
finding named. The same sentence usually exists in the design document, the
specification, the changelog, and the rustdoc that `cargo doc` publishes to
users, and a partial correction is worse than none: the surviving copies now
read as corroboration. Grep the whole tree for the retracted sentence, and add
the retraction to `crates/varve/tests/doc_claims.rs`, which is the gate that
turns invariant 4 into something CI can fail on. It forbids each retracted
phrase across `README.md`, `CHANGELOG.md`, `docs/` (excluding review documents,
which must quote the defect) and every `.rs` file under `crates/`, and it
compares the layout version stated in the documentation against `VMAT_VERSION`
in the source, so a version bump cannot update only some documents.

Third mechanical habit, alongside the two under *The rule*: when you write a
claim about a *runtime* counter, state its scope. Both matrix zero-range
counters are thread-local and one of them measures a single range request, not a
whole operation; documentation that omitted either qualification told callers
they could prove a cheap path they had not proved.

### 5. COMPLETE ACROSS PLATFORMS

Every platform-conditional fix is done on **every** platform, or explicitly
refused on the weak one with a typed error.

A source comment acknowledging a known window is not a fix - it is a record that
the fix was not done. That is what F-05 was. Audit every `cfg(windows)` /
`cfg(unix)` split for a weaker branch: locking, parent sync, hole punching,
deletion, identity.

## Mechanically enforced shapes

Two defect shapes recurred across rounds 5, 7 and 12 despite being named,
enumerated and reviewed for each time. Both are now enforced by types rather
than by memory: the compiler refuses the bad shape, so a function written later
inherits the property without anyone remembering this document exists. This
section is the map — when a review asks "is this class closed", the answer is
these types and the gates below, not a table of sites.

**Round 12's enforcement pass made the mechanisms universal and proved they
bind.** The two fix agents each built one enforcement type for the module its
finding named. That leaves the same defect available in every module that was
not named, which is how the class survived rounds 5, 7, 9, 10 and 11. The
enforcement pass therefore did three things:

1. **Promoted the shape-B type out of `stream.rs`** into
   `crates/varve-core/src/writer_permit.rs`, and put `file.rs` and `layout.rs`
   on it. The crate previously held **four** independent hand-written poison
   booleans, not the three the F-05 fix report named; the fourth
   (`DiskIndexWriteBatch::poisoned` in `disk_index.rs`) was never set by any
   code path, so its guard could not fire and `DiskIndexError::BatchPoisoned`
   was unreachable — a guard that reads as protection and provides none. The
   field, the guard and the variant are deleted.
2. **Added the shape-A type the append path was missing**
   (`ReservedIndexSlot`). The resident record index is the in-memory mirror of
   the records on disk; its reservation sat eighty lines above its post-append
   `push`, in the correct order, held there by a source comment. That is the
   configuration F-03 was in.
3. **Proved the enforcement with compile-fail fixtures**, which is the part
   that distinguishes this pass from round 10's sweep. See
   [What is mechanically enforced, and what is not](#what-is-mechanically-enforced-and-what-is-not).

### Shape A — "disk before memory"

*A persisted structure with an in-memory mirror is mutated on disk, and only
afterwards is the mirror updated by a step that can fail or allocate. On
failure, disk and memory disagree and the writer stays usable.* Instances:
round 5's F-02, round 7's F-03, round 12's F-03.

The enforcement pattern is prepare/commit with the fallible half as a
precondition expressed in the type system: a `prepare_*` function performs every
fallible step and returns a prepared value; the `commit` takes that value **by
value**, writes disk, and installs the mirror infallibly. A caller cannot reach
the disk write without having done the fallible part.

| Type | Where | What it makes impossible |
| --- | --- | --- |
| `mod page_index` and its `PreparedAppend` / `PreparedRelease` / `PreparedRepublish` / `PreparedPublication` | `crates/varve-core/src/matrix.rs` | The two functions that put persisted page-index bytes on disk are declared inside a private module without `pub(super)`, so nothing in the other ~7000 lines of `matrix.rs` can call them. The only route is a prepared value, whose construction does the budget charge, the mirror `try_reserve`s, all offset/count arithmetic and the encoding of every byte. Residual property, relied on elsewhere: **after a prepared page-index mutation begins writing, the only error it can return is `Error::Io`**, which `finish_matrix_mutation` poisons on. Deliberate exception, named in the module doc: the whole-region `zero_range` erasures, which are paired with infallible mirror `clear()`s |
| `prepare_byte_write` / `commit_byte_write` (and `prepare_current_write_bit` / `commit_current_write_bit` / `abandon_current_write_bit`) | `crates/varve-core/src/matrix.rs` | The round-9 instance of the same pattern for bitmap page allocation: `commit_byte_write` returns a `PageResidencyDelta`, not a `Result`, and allocates nothing |
| `ReplacementTarget` (round 12, **corrected round 13**) | `crates/varve-core/src/file.rs`, `mod replacement_target` | Not a mirror ordering, but the same idea applied to a check: the struct has a private field, lives in its own module so the field is unnameable, and has exactly one constructor, `ReplacementTarget::resolve::<T>(&index, ordinal)`, which performs both the block-id selection and the `T::VERSION` refusal. **Round 12 claimed this made `self.index[..]` unreachable for a caller-chosen ordinal; re-verification compiled a new path that resolved its own ordinal and wrote through it, so that claim was false.** What is enforced is narrower and sufficient: *reading* the index is unrestricted, but the bytes of an already-indexed record can only be written by `RecordFile::overwrite_indexed_record`, which consumes a `RecordOverwrite`, whose only constructor consumes a `ReplacementTarget`. A path that resolves its own ordinal has nothing to hand the writer. Bound: a stack `usize`; allocates nothing. Proved by `crates/varve/tests/ui/fail_fabricated_replacement_target.rs` |
| `RecordOverwrite` and `mod record_file` (round 12 as `overwrite_record_bytes_in_place`, **restructured round 13**) | `crates/varve-core/src/file.rs` | Round 12 recorded that `overwrite_record_bytes_in_place` was "the crate's only in-place rewrite of an already-indexed record" and that it invalidated the keyed-tail map first. Both were true of the code as written and neither was enforced: re-verification compiled a new `self.file.seek(..)` / `write_all(..)` pair that skipped the function entirely. The primary handle is now a `RecordFile` whose `File` is unnameable outside `mod record_file`; it implements no general write and lends out no `&File` (`&File` implements `Write`). Exactly two operations put record bytes on disk: `append_record_at_end`, which seeks to the end itself and refuses any other offset, and `overwrite_indexed_record`, which consumes a `RecordOverwrite`. Producing a `RecordOverwrite` consumes a version-checked `ReplacementTarget` and drops the block's keyed-tail map, so both F-01's refusal and F-02's invalidation are preconditions of *addressing the bytes*, not steps inside a function a sibling can bypass. The invalidation is a `HashMap::remove`: infallible and allocation-free, so it adds no fallible step in either direction. One named exception: `RecordFile::matrix_region` lends the handle to `crate::matrix` and to the public `MatrixDurabilityBarrier`, gated by `enforcement_gates.rs::the_primary_handle_escape_is_only_for_the_matrix_region`. Proved by `crates/varve/tests/ui/fail_fabricated_record_overwrite.rs` |
| `ResidentIndex` / `ReservedIndexSlot` (round 12, **corrected round 14**) | `crates/varve-core/src/file.rs`, `mod resident_index` | The resident record index is the mirror of the records on disk, and it is the *append* path's mirror — the hottest one in the crate. **Round 12's row asserted that "the fallible half cannot be moved below the write"; re-verification disproved it by compilation.** The token proved that a reservation *existed*, not that it *preceded* the write, so F-03's exact shape — authoritative append, then `ReservedIndexSlot::reserve`, then `install` — compiled inside `file.rs`; and because the mirror was still an ordinary `Vec` field, `core::mem::take(&mut self.index)` plus `insert` compiled too, growing it around the token entirely. Two changes make the claim true: (a) the `Vec` is a private field of `ResidentIndex`, declared in this module, so `push` / `insert` / `extend` / `append` / `mem::take` are unnameable outside it and `ResidentIndex::install` — which consumes the token — is the only growth in the crate; (b) `RecordFile::append_record_at_end` **takes `&ReservedIndexSlot`**, so the authoritative write is unreachable until the charge and the `try_reserve` have succeeded. Ordering is now a signature. Deliberately still permitted, because none of them can add an entry the disk does not have: reading (the type derefs to a slice), `entry_mut` (the in-place path's sequence/checksum restamp), `truncate` (the append rollback) and `adopt_generation` (open, and the rewrite paths, which `try_reserve_exact` before the new file exists). All tokens zero-sized; no hot-path cost. Proved by `crates/varve/tests/ui/fail_fabricated_index_reservation.rs` and `crates/varve/tests/enforcement_gates.rs::the_resident_record_index_cannot_be_grown_or_outrun` |

### Shape B — "guard bypass"

*A poison/validity flag is checked by a public guard, but an internal method
reachable from another module performs the guarded operation without it.*
Instances: round 12's F-05, and the three further flags the enforcement pass
found in files F-05 did not name.

The enforcement pattern is a witness token that only the checking function can
produce. There is now exactly **one** implementation of it for the whole crate,
in `crates/varve-core/src/writer_permit.rs`, and
`crates/varve/tests/enforcement_gates.rs::no_writer_keeps_its_own_poison_boolean`
fails CI if a fifth hand-rolled `poisoned: bool` appears anywhere.

| Type | Where | What it makes impossible |
| --- | --- | --- |
| `PoisonFlag` and `MutationPermit` (round 12, **tightened round 14**) | declared in `crates/varve-core/src/writer_permit.rs`; used by `stream.rs`, `indexed.rs`, `file.rs`, `layout.rs` | `PoisonFlag` is the only holder of the boolean; `MutationPermit` has a private field and no constructor other than `PoisonFlag::issue`, and the module boundary makes the field unnameable even inside the file that owns the writer. **Round 14 closed a laundering bypass**: `issue` was `pub(crate)` and `PoisonFlag::healthy()` is a `const fn`, so three lines anywhere in the crate could mint a witness from a throwaway flag and spend it on a genuinely poisoned writer — the permit proved that *a* flag had been checked, not that *this writer's* had. `issue` now has no visibility modifier at all, so the only route out of the module is `GuardedWriter::writer_permit`, which takes `&self` of the writer and reads that writer's own flag; and the witness is typed by the writer it speaks for (`MutationPermit<VarveStreamWriter>` is not `MutationPermit<VarveFile>`). *Instance* identity is still unenforced — see open item 21. Every guarded operation takes a permit, by value where another module can reach it, so each such mutation is preceded by its own fresh check rather than by one check amortised over a loop. A caller that skips the check has nothing to pass and does not compile. Proved by `crates/varve/tests/ui/fail_fabricated_mutation_permit.rs` (the witness cannot be forged), `crates/varve/tests/ui/fail_cross_writer_mutation_permit.rs` (one writer's permit is not another's) and `crates/varve/tests/enforcement_gates.rs::a_permit_can_only_be_minted_from_the_writers_own_poison_flag` (the constructor stays private; the in-crate half of the bypass cannot be a `tests/ui` fixture, because every spelling of it is already unreachable from a downstream crate and the fixture would pass for the wrong reason) |
| `MutationInFlight` (round 12, enforcement pass) | `crates/varve-core/src/writer_permit.rs`, used by `layout.rs` | `LayoutWriter` does not use its flag as a one-way poison: it marks a segment write *in flight* and clears the mark when the body succeeds or is fully rolled back, so only a failed rollback leaves the writer refusing. That needs an un-set, which a one-way `poison()` cannot express and which nobody should be able to perform by assigning a field. `begin_mutation` **consumes a permit** (so a window cannot be opened on a refusing writer) and returns the token; `end_mutation` **consumes the token** (so nothing clears a mark it did not set) and touches only the in-flight field, so a real poison raised inside the window survives — asserted by `writer_permit::tests::ending_a_window_does_not_clear_a_poison_raised_inside_it`. Proved by `crates/varve/tests/ui/fail_unpermitted_mutation_window.rs` |
| `FatalAccessGate` / `FatalAccessAllowed` (**round 14**) | `crates/varve-core/src/matrix.rs`, `mod fatal_access` | The flag the shape-B inventory could not have found: round 12 built that inventory by grepping the crate for `poisoned`, and this one is called `fatal_access_blocked`. It records that a `Fatal` recovery finding blocks safe access to matrix state unless the spec opted into forensic reading, and it was consulted by an `ensure_fatal_access_allowed` helper at eleven call sites — a guard that protects the operations which remember to call it, and nothing else. The boolean is now a private field of `FatalAccessGate`, whose only output is `FatalAccessAllowed`: zero-sized, private field, single constructor `FatalAccessGate::allow`, which *is* the refusal. `MatrixLayout::block_index` and `MatrixLayout::commit_index` — the only two routes from a block id or a commit category to a position in the layout's state — demand the witness, so an accessor written next month cannot address what it wants to touch without the check. **Stated rather than implied:** code that already holds a resolved position can still index `blocks`/`commits` directly (that is how the existing prepare/commit pairs pass an index between their halves); the enforced claim is that no path can go from an *identifier* to matrix state without the refusal running. Bound: one `bool` per layout, no allocation. Proved by `crates/varve/tests/ui/fail_fabricated_fatal_access.rs` and `crates/varve/tests/enforcement_gates.rs::matrix_state_is_only_addressed_through_the_fatal_access_witness` |
| `CrcValidEvidence` / `CompleteCrcValidEvidence` (round 12 as a tracked boolean, **restructured round 14**) | `crates/varve-core/src/matrix.rs`, `mod crc_valid_evidence` | A block's CRC-validity bitmap answers "is the stored CRC for this cell meaningful", and a **clear** bit means either "nothing established one" or "the page carrying it was never loaded, because the validity page index was damaged". F-04 was the commit-map rebuild publishing the second as the first. Round 12 fixed it with a `crc_valid_complete: bool` beside the bitmap plus one added check; re-verification judged that **partial**, and compiled a new consumer that read `block.crc_valid_bits.get(ordinal)?` without it. The bitmap is now a private field of `CrcValidEvidence` and there is **no operation outside the module that yields a validity bit** except `CompleteCrcValidEvidence::get`, whose witness is produced solely by `CrcValidEvidence::complete` — which *is* the refusal (`Error::MatrixFatalCorruption`, unconditional; forensic mode does not relax it). The one reader allowed on incomplete evidence, `require_meaningful`, returns `Result<()>`: it hands back no bit, so an absent one can become a refusal but never published state. Bound: one `bool` per block, no content-sized allocation. Proved by `crates/varve/tests/ui/fail_fabricated_crc_valid_completeness.rs` |

Sites routed through the permit, per file:

- `stream.rs` — every mutating entry point (round 12's F-05 fix); unchanged by
  the enforcement pass except that the type now comes from the shared module.
- `indexed.rs` — has no flag of its own; reads and sets the stream's.
- `file.rs` — `ensure_not_poisoned` / `ensure_write` return the permit, and the
  three functions that put bytes into an already-published generation demand
  it: `write_record_with_prev_key` (the crate's single record-append core, which
  used to call the guard itself — a convention a sibling append helper could
  omit), `write_user_record`, and `overwrite_record_bytes_in_place` (the only
  in-place rewrite of an indexed record — round 13 made "only" a property the
  compiler holds rather than a description of today's code: see `mod
  record_file`).
- `layout.rs` — `write_segment_streamed`, through the in-flight window; the
  body is a closure that takes the `&MutationInFlight` witness, so it cannot be
  entered with the window closed.

Exempt by construction, stated rather than assumed:

- `VarveFile`'s *read* surface and every open-time path. Nothing is published,
  so there is no guarded operation.
- `DiskIndexWriteBatch` (`disk_index.rs`) needs no flag at all: every staging
  entry point validates fully and then performs only infallible `BTreeMap`
  inserts and counter adds, and `commit(mut self)` consumes the batch, so a
  half-staged or twice-committed batch is not representable. Its vestigial flag
  was deleted rather than wired up.

### What is mechanically enforced, and what is not

Stated plainly, because round 10 declared this class closed on the strength of
a reading pass and round 12 found the same shape in the file it had rewritten.

**Enforced by the compiler** (a violation does not build):

| Property | Proof |
| --- | --- |
| The poison-check witness cannot be forged anywhere in or out of the crate | `crates/varve/tests/ui/fail_fabricated_mutation_permit.rs` |
| A mutation window cannot be opened without the check | `crates/varve/tests/ui/fail_unpermitted_mutation_window.rs` |
| The post-append mirror install cannot be reached without its reservation | `crates/varve/tests/ui/fail_fabricated_index_reservation.rs` |
| The authoritative record append cannot run *before* the mirror reservation, and the mirror cannot be grown around the token | `RecordFile::append_record_at_end` takes `&ReservedIndexSlot`, and `ResidentIndex`'s `Vec` is unnameable outside `mod resident_index` — round 12 asserted the first of these without enforcing it, and re-verification compiled both bypasses |
| Matrix state cannot be addressed from a block id or a commit category without the fail-closed fatal-recovery check | `crates/varve/tests/ui/fail_fabricated_fatal_access.rs`, plus the signatures of `MatrixLayout::block_index` / `commit_index` |
| The page-index disk writers cannot be called from the rest of `matrix.rs` | module privacy: they are declared in `mod page_index` with no `pub(super)` |
| `file.rs`'s append core and in-place rewrite cannot run without the guard | their signatures take `&MutationPermit` |
| A replacement target cannot be *written through* without the version check | `crates/varve/tests/ui/fail_fabricated_replacement_target.rs` — and note the wording: round 12's row said "addressed", which was false, because reading `self.index` and computing an ordinal is unrestricted and always was |
| An already-indexed record's bytes cannot be rewritten without dropping the block's keyed-tail map | `crates/varve/tests/ui/fail_fabricated_record_overwrite.rs` |
| A CRC-validity bit cannot be read as evidence of absence without the completeness proof | `crates/varve/tests/ui/fail_fabricated_crc_valid_completeness.rs` — and the capability itself is gone, not guarded: outside `mod crc_valid_evidence` no operation on the bitmap returns a `bool` except through the witness |
| A second in-place write pair cannot be introduced against the primary handle | module privacy: `VarveFile::file` is a `RecordFile` whose `File` is unnameable outside `mod record_file`, which implements neither `Write` nor `Seek` and lends out no `&File` |

The `tests/ui` enforcement fixtures are driven by
`crates/varve/tests/compile.rs::mechanical_enforcement_contracts`, under the
`scalable-fault-injection` feature: the enforcement types are crate-private and
are visible to an external crate only through the `#[doc(hidden)]`
`varve_core::enforcement_probe`, which re-exports them rather than copying
them, so the fixtures test the real types. Each was negative-controlled by
relaxing the private field and observing the fixture start compiling.

**Enforced by a source-level gate** (`crates/varve/tests/enforcement_gates.rs`)
— weaker than a type, because it checks the declaration rather than every use,
but it fails CI on the one edit that could reintroduce the defect:

- the two page-index writers stay inside `mod page_index` and stay unexported;
- no module declares its own `poisoned: bool`;
- **no struct in any module of `varve-core` declares a `bool` field that an
  `fn ensure_*` guard refuses on** (round 14). The predecessor of this gate
  matched the literal `poisoned: bool` in six named files, which by
  construction could not find a gate flag with a different name — and two
  existed: `matrix.rs`'s `fatal_access_blocked`, and `crc_valid_complete`,
  which the round that was closing this class *added*. The rule is now stated
  by shape rather than by name, the sweep enumerates the source directory
  rather than a list, and it fires on the exact pre-fix declaration
  (negative-controlled by dropping that shape into the crate as an
  undeclared module and watching the gate name it);
- the resident record index stays a `ResidentIndex` with a private `Vec`, is
  not `Default` and lends out no `&mut Vec`, has exactly one growth in the
  crate, and the append still demands the reservation token (round 14);
- the matrix fatal-recovery flag stays inside `mod fatal_access`, the witness
  comes into existence in exactly one place, and both resolvers keep demanding
  it (round 14);
- there is no `self.index.push(` on the append path;
- `VarveFile` holds its handle as a `RecordFile`, there is no `self.file.write_all(`
  / `write(` / `seek(` / `write_vectored(` anywhere in `file.rs` outside
  `mod record_file`, and `RecordFile` neither implements a general write nor
  returns a `&File` (round 13);
- every use of the one escape hatch, `matrix_region()`, is an argument to a
  `crate::matrix::` call or to a matrix durability sync (round 13);
- `ReplacementTarget::resolve` and `RecordOverwrite::prepare` remain the single
  constructors, the in-place writer still takes the token rather than a
  caller-chosen offset, and the keyed-tail invalidation is still inside
  `prepare` (round 13);
- `MatrixBlockLayout::crc_valid_bits` stays a `CrcValidEvidence` rather than a
  bare `SparseBitmap`, no completeness boolean is declared beside it,
  `CompleteCrcValidEvidence::get` stays the only bit read, the CRC rebuild
  still takes the witness, and the module's two named escapes — the fail-closed
  `require_meaningful` and the page-index maintenance lend
  `page_index_mirror_mut` — keep exactly one call site each, with no `.is_ok()`
  laundering of the fail-closed result (round 14).

**Still enforced by review only**, named so a future round does not mistake
silence for coverage:

- The permit is a plain ZST with no lifetime, so a permit taken and then held
  across an intervening poisoning call would still be accepted. Mitigated by
  consuming it by value at every cross-module entry point and re-taking it
  immediately before each disk-touching step; making staleness unrepresentable
  needs the writer's fields split into a borrow-disjoint inner struct. Open
  item 21.
- `matrix.rs`'s prepared values are reachable only from inside `matrix.rs`, so
  their ordering property has a compile-fail proof only via module privacy, not
  via a `tests/ui` fixture. Deferred deliberately: exposing them to prove them
  would destroy the privacy that is the enforcement. Open item 22.
- `MatrixLayout`'s `blocks` and `commits` vectors can be indexed directly by
  code that already holds a resolved position, because the layout is declared
  in `matrix.rs` itself. The fatal-access witness is enforced at the two
  resolvers, so no path can go from an identifier to matrix state without the
  check; a path that is *handed* a position by the half of an operation that
  did check is not re-checked. Closing that would mean moving `MatrixLayout`
  behind its own module boundary, which touches some seventy call sites without
  adding a property.
- Mirrors outside the four routed files — the sidecar `TailCache`, the stream
  resident tails, the matrix quarantine map — are *installed infallibly today*
  (`store_tail_cache` moves an already-owned map; `set_tail` writes a slot
  bounded by the declared block count) but nothing stops a fallible step being
  added after their disk write. Open item 23.

### What a future round should do with this section

Add to it, and prefer it to the enumeration. When a finding is an instance of a
shape listed here, the fix is to route the offending code through the existing
type, not to add a local check; when it is a new shape, name the shape, add the
type, and say what stops compiling. A row here is worth more than a table of
sites because it does not decay: the enumeration is true of the code that
existed when it was written, and this is true of code nobody has written yet.

## Inventory and audit status

Legend for **Audit**: `deep` = read the implementation and its tests against all
five invariants; `spot` = checked the specific invariant most at risk; `carried`
= accepted on a sibling's evidence this round without independent re-derivation.

### Persisted structures and regions

| Structure | Where | Added | 1 | 2 | 3 | 4 | 5 | Audit |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| Matrix page index (VMAT v4) | `crates/varve-core/src/matrix.rs` | round 4 (PERF-01), redesigned round 5 (F-03/F-06) | ok | **fixed round 9 (F-02)** | **fixed round 9 (F-02)** | ok | ok | deep - the round-5 self-checking occupancy header made a damaged *entry* visible, but said nothing about a damaged *protocol*: the whole-map rebuild zeroed the region and republished entries one at a time, so an interruption left a valid, short count, which is exactly the "damage looks like a clean end" shape invariant 2 names. Round 9 publishes a generation instead of editing one: `PAGE_INDEX_REBUILD_MARKER` (`u64::MAX`) goes into the occupancy slot and is `sync_data`d BEFORE anything is destroyed, only the entry region is cleared and refilled, every entry/digest/page is synced, and the real count is the single last write that publishes. `load_page_index` refuses the marker with a `Fatal` `MatrixCorruptionKind::CommitMap` finding routed through `fatal_access_blocked`, and `recovery_report` now recommends `RebuildCommitMap` for any fatal CommitMap finding, so the damage names its own way out. **No layout bump**: a unit test proves the marker is not a representable header at any capacity, including saturated `PAGE_INDEX_MAX_ENTRIES`, so an older reader rejects it as damage - also fail-closed. Regressions: process-abort tests at all four stages plus injected entry/header write failures, all with allocation maps forced unavailable. **Round 12 (F-03) closed the mirror-ordering half of this structure structurally**: every entry and header write now goes through the private `mod page_index` prepare/commit gate, so the ordering property is a compile-time one rather than a reviewed one - see [Mechanically enforced shapes](#mechanically-enforced-shapes). No wire-format change; VMAT stays at version 4 and the encoding is byte-identical |
| Per-page digests | `crates/varve-core/src/matrix.rs` | round 4 | ok | ok | ok | ok | ok | spot - damage produces a `Fatal` `MatrixCorruptionKind` finding; the trust boundary (redundancy, not authentication) is documented |
| Matrix creation nonce | `crates/varve-core/src/matrix.rs`, `stream.rs` | round 3 (STO-01) | n/a | ok | ok | ok | ok | spot - a mismatch is a typed identity rejection, not a silent accept |
| Sidecar identity/generation versions | `crates/varve-core/src/disk_index.rs`, `indexed.rs` | rounds 3-4 | n/a | ok | ok | ok | ok | spot |
| Checkpoint cadence state | `crates/varve-core/src/file.rs` | round 3 | n/a | ok | ok | ok | ok | spot - growth is guarded by the linear-cost test the round-5 report verified |
| VMAT layout version gate | `crates/varve-core/src/matrix.rs` | rounds 3-5 | n/a | ok | n/a | ok | ok | carried - v1/v2/v3 are refused as stale-regenerable with `FormatVersionMismatch`; v4 is current |

### In-memory caches and registries

| Structure | Where | Added | 1 | 2 | 3 | 4 | 5 | Audit |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| Resident keyed-tail cache | `VarveFile`, `crates/varve-core/src/file.rs` | round 4 (PERF-05) | **fixed round 6, completed round 7** | n/a | ok | ok | ok | deep - was `try_reserve`-only; round 6 charged the incremental growth to `ReadLimits::max_keyed_tail_bytes` before the append (API3-02) but charged the *initial build* only after the whole map existed, so the charge gated retention rather than the peak. Round 7 (API3-05) moved the charge into the build. Ordering was fixed in round 5 (F-01). **Round 9: this is now the only keyed-tail cache.** The generated writers were folded onto it (F-01), so it serves both the generic `push_keyed`/`delete` API and every generated `push_<block>`/`delete_<block>`, and the charge it applies - inline `(Vec<u8>, u64)` storage plus the key payload bytes the map owns - is now the charge on every keyed path |
| Generated keyed writer tail maps | **removed round 9 (F-01)** | pre-existing, unaudited until round 6 | n/a | n/a | n/a | n/a | n/a | deep - this structure no longer exists. Rounds 6, 7 and 8 each fixed one property of it (incremental charge, initial-build charge, delete clone ordering) and round 8 still shipped a post-publication `HashMap::insert` running the caller's `Hash`/`Eq`. Round 9 took the report's strongest correction and deleted the structure instead: the generated writers now route through the **Resident keyed-tail cache** row below, which was already audited for this property, so there is one implementation rather than two that can drift. `VarveWriter::reserve_keyed_tail_slot` went with it - its own rustdoc admitted it could not prevent a later `Hash`/`Eq`. Consequences worth knowing: the generated path now charges retained key payload heap (closing open item 2), key identity is canonical-encoding equality rather than `K: Eq` (the semantics the generic path always had), a format without `keyed_offset_chain` builds no map at all, and interleaving the generated and generic keyed APIs is now coherent. Regressions: `crates/varve/tests/generated_keyed_atomicity.rs` (independently panicking `Hash` and `Eq` on push and delete, inherent and trait routes, asserting 0 invocations after publication) |
| Resident block tails (`BlockTails`) | `crates/varve-core/src/file.rs` | round 3 (PERF3-03) | ok - bounded by the format's declared block count, not by file content; the post-append `Vec::insert` runs at most once per distinct block id | n/a | ok | ok | ok | deep |
| Process-global block-contract cache | `crates/varve-core/src/collections.rs` | round 4 | ok - bounded by the program's own `(spec, block id)` set, and only for hand-built specs; not chosen by file content | n/a | ok | ok | ok | deep |
| Shared sidecar `Database` registry | `crates/varve-core/src/disk_index.rs` | round 3 | ok - one slot per open sidecar path, bounded by handles the caller opens; pruning is amortised | n/a | ok | ok | ok | spot |
| Sparse bitmap page map / resident tails | `crates/varve-core/src/matrix.rs` | rounds 4-5 | ok | n/a | **fixed round 9 (F-03)** | ok | ok | deep - `set_byte` performed fallible page allocation and map reservation, and `apply_commit_bit` called it *after* the bitmap byte was on disk, so an `AllocationFailed` left disk and memory disagreeing on a writer that ordinary matrix mutation handling does not poison (it poisons only for `Error::Io`). Split into `prepare_byte_write` (page allocation, map reservation, `Arc::make_mut`, all checked count arithmetic; produces a detached `PreparedByteWrite`) and `commit_byte_write` (returns a `PageResidencyDelta`, not a `Result`, and allocates nothing). The failure is removed rather than handled, so no writer-poisoning widening was needed. Dropping a prepared write frees the detached page and leaves the bitmap byte-for-byte unchanged. The same shape was found and fixed in `apply_cell_crc_valid` and in the session write-tracking bit of `write_cell`/`write_cell_payload`, neither of which the review named |
| `TailCache` / `SharedSidecar` tail map | `crates/varve-core/src/disk_index.rs` | round 3 (`4e07a3f`) | ok - bounded by `tail_limit_for_spec` and the declared block count, validated at `disk_index.rs` `insert`/load; the bound is stated here rather than assumed | n/a | ok | ok | ok | spot - added round 7; was missing from this inventory entirely |
| `AllocatedExtents` (`query_allocated_extents`) | `crates/varve-core/src/matrix.rs` | round 4 (`0d4b9c6`), touched round 5 (F-07) | ok - a resident `Vec<(u64, u64)>` bounded by `MAX_TRACKED_EXTENTS` (8192, i.e. 128 KiB), which is a compile-time constant and not chosen by file content. The cap is checked *after* the pushes that cross it, so the true bound is `MAX_TRACKED_EXTENTS + 1` entries on Linux (one push per check) and `MAX_TRACKED_EXTENTS + 512` on Windows (one `FSCTL_QUERY_ALLOCATED_RANGES` batch per check), i.e. at most ~139 KiB either way; the growth uses infallible `Vec::push` rather than `try_reserve` | n/a | ok | ok | ok | spot - added round 7; was missing from this inventory entirely. See open item 6 |
| Fault-injection counters | `file.rs`, `matrix.rs`, `stream.rs` | rounds 4-5 | ok - thread-local `Cell`s, feature gated | n/a | ok | ok | ok | spot. See open item 4 below: one injector in `stream.rs` is `#[cfg(test)]` process-global |

### Cost and limit models

| Model | Where | Added | Status | Audit |
| --- | --- | --- | --- | --- |
| Hash-table reservation model | `crates/varve-core/src/codec.rs` | round 4 (SAFE-01), corrected round 5 (F-02) | ok - models the small-table capacity classes; documented as a deliberate over-estimate valid for control-group widths up to 32, not as a measurement of one build | carried |
| `BTreeMap` materialization charge | `crates/varve-core/src/codec.rs` | pre-existing, unaudited until now | **fixed this round** - was `len * size_of::<(K, V)>()`, i.e. entries packed end to end. A std node allocates eleven fixed slots whatever its fill, is only guaranteed to hold five, and adds a header and child pointers (API3-04) | deep |
| `KeyedMergeEstimate` | `crates/varve-core/src/file.rs` | round 3 (PERF-03), corrected round 5 (F-04) | ok - renamed to `peak_resident_structural_bytes()` and documented as a structural estimate, explicitly **not** an upper bound, with its four exclusions named | carried |
| `ReadLimits::UNTRUSTED` | `crates/varve-core/src/format.rs` | round 3 | ok | spot |
| `ReadLimits::max_keyed_tail_bytes` | `crates/varve-core/src/format.rs` | **this round** | new - the dimension the keyed-tail charge needs; `STANDARD` leaves it at `u64::MAX`, `UNTRUSTED` sets 256 MiB | deep |
| Matrix resident bitmap budget (payload + index) | `crates/varve-core/src/matrix.rs` | round 5 (F-03) | **fixed round 8 (F-01 refund, F-02 rollback), round 9 (F-05 pre-allocation charge)** | deep - both round-8 blockers were defects in exactly this round-5 structure, so the previous `carried` was not an audit. Three contracts are now recorded and tested. (a) `clear_category` totals the commit-bitmap, quarantined-copy, and validity-bitmap payloads *and* the matching `resident_index_bytes()` before clearing anything, then `checked_sub`s both counters, so a populate/clear cycle returns to its exact baseline instead of leaking a charge per cycle and eventually returning `LimitExceeded` for released memory. (b) `apply_commit_bit`, `apply_cell_crc_valid`, and the write-tracking bit release their payload precharge on any `Err` from a fallible step, so a failed first-page mutation leaves the counters as they were. (c) Round 9: `rebuild_commit_map_from_crc` computes `materialisation_cost` and charges it *before* `prepare_byte_write` allocates (F-05), and `prepare_current_write_bit` charges before the memory is taken rather than after the commit bit, validity bit and payload are already durable, so the ceiling is a strict pre-allocation limit on every path rather than a report issued once a page is resident. Deliberate narrowings, stated in code comments: a retained page-index entry is *not* refunded (it is real resident memory), and `current_write_bits` is not cleared by a commit-category clear (the statement it records stays true). Regressions: `matrix_integrity_scaling.rs::category_clear_refunds_payload_and_page_index_residency_every_cycle`, `::repeated_populate_clear_cycles_do_not_exhaust_the_bitmap_ceiling`, and the three `matrix::mutation_precharge_rollback_tests` |

### Published contracts and claims

| Claim | Where | Status | Audit |
| --- | --- | --- | --- |
| Merge sizing entry point | `docs/api-reference.md`, `docs/performance.md`, `docs/update-compact-guide.md` | ok - "upper bound" retracted everywhere; only the three count fields are labelled bounds | spot |
| Matrix open complexity | `docs/performance.md`, `docs/matrix-storage-design.md`, `CHANGELOG.md`, `pages_to_visit` / `matrix_open_bitmap_pages_visited` rustdoc | **corrected round 8 (F-07)** - the candidate-page bound is now stated as the worst case in every public copy: `L` indexed pages plus `A` allocation-derived candidates, `O(L+A)` time, `Theta(L+A)` temporary memory, up to `O(4096U)` page-byte I/O for `U` distinct pages. Live-state cost is described as the sparse-allocation *operating case*, not an unconditional bound; the surviving `O(live pages)` uses are the no-allocation-map case, where the index alone drives enumeration, and are correct | deep |
| Matrix open corruption visibility (allocation-map coverage) | `crates/varve-core/src/matrix.rs` (`matrix_open_allocation_map_available`, `pages_to_visit`, `load_paged_bitmap`), `docs/performance.md`, `docs/matrix-storage-design.md`, `CHANGELOG.md` | **corrected round 8 (F-06)** - documentation only, no behaviour change. The guarantee that holds everywhere is now separated from the one that does not: every persisted-index page is verified on every platform, and allocation-map pages are additionally checked only where the platform supplies a usable map. An omitted page is never loaded, so this is a corruption-visibility limit, not unchecked acceptance. Previously the unconditional "detection strength is unchanged" claim was published in four places; the last surviving copy (`CHANGELOG.md`) was corrected in this round's integration pass | deep |
| Whole-category clear cost | `docs/performance.md`, `docs/matrix-storage-design.md` | ok - names the two range-removal mechanisms and states the Theta(cells/8) streaming fallback, with the runtime accessor that proves which ran | carried |
| Durability wording | `docs/durability-model.md` | ok | spot |
| Limits table | `docs/declaration-and-internals.md`, `docs/api-reference.md` | updated this round with `keyed_tail` | deep |
| Immutable-CI / feature-matrix wording | `README.md`, `.github/workflows/ci.yml` | **corrected round 8 (F-08)** - the lint-matrix comment claimed 7 optional features and a 128-configuration power set; the facade exposes 6 (`integrity`, `mmap`, `zero-copy`, `compression-zstd`, `high-cardinality-dev`, `scalable-fault-injection`), so the power set is 64. The nine configured jobs were already correct and are unchanged | deep |
| Dependency-policy exception (`allow-wildcard-paths`) | `deny.toml`, `crates/varve-macros/Cargo.toml` | **added round 8 (F-05)** - the root `cargo deny check` reported `bans FAILED` because `varve-macros` dev-depends on the facade by path only, which carries no version requirement. The review's preferred correction (add `version = "0.4.0"`) was implemented and **rejected with evidence**: a dev-dependency *with* a version survives into the published manifest, so `cargo package -p varve-macros` then fails with "no matching package named `varve` found" - the facade version being released is not on crates.io yet, and this recurs on every version bump. The report's stated alternative was taken instead: `allow-wildcard-paths = true`. **Round 9 (F-09) corrected this row's own scope claim**: it said the option "applies only to path dev-dependencies of published crates", which is narrower than the truth. Re-derived against cargo-deny 0.19.9 by introducing each shape: a path dev-dependency of a published crate is allowed (the edge above); a non-dev path dependency of a *published* crate is denied; a path dependency of ANY kind from a crate with `publish = false` is **allowed**; a registry wildcard is denied, dev or not. The residual surface the option really buys is therefore unversioned path dependencies from never-published crates. `deny.toml` now states that, and the `dependency-policy-fixture` CI job asserts the two denied shapes still fail. Root `cargo deny check` and verified `cargo package` now both pass, which the version route cannot achieve simultaneously; the published `varve-macros` manifest has an empty `[dev-dependencies]`, confirming the edge never reaches crates.io. **Do not "fix" this back to an explicit version.** Residual: `tools/public-api-fixture` and `tools/rename-fixture` declare the same version-less path dependency, but are detached workspaces with no cargo-deny job, so they do not affect the root gate today | deep |
| Post-commit matrix hook outcome | `VarveFile::write_matrix_cell_durable`, `docs/api-reference.md`, `docs/durability-model.md`, `crates/varve/tests/matrix.rs` | **added round 9 (F-04)** - the hook is documented as post-publication and the behaviour was intentional, but a plain `Result<()>` cannot distinguish "failed before publication" from "published, hook failed", so a result-driven retry can duplicate the hook's external work. It now returns the typed published outcome `Error::MatrixCommittedButHookFailed { event, source }`, matching the shape `PublishedButParentSyncPending` / `ReplacePublicationIndeterminate` already established, and carries the `MatrixCommitEvent` so the notification alone can be retried. The event is additionally derived from layout geometry *before* the commit, so `commit_event` can no longer fail for an already-durable cell. The deliberate test at `matrix.rs` was kept and strengthened to assert the typed distinction, not deleted | deep |
| Allocator-failure policy | `docs/durability-model.md` ("Allocator Failure And Published Outcomes"), `docs/api-reference.md`, `VarveFile::write_matrix_cell_durable` rustdoc | **added round 12 (F-06)** - the crate had no single stated policy, and the published-outcome family was documented as an absolute structural claim. A subprocess harness failed the next allocation at each post-commit branch and both children terminated on a 56-byte allocation while the reopened file contained the committed values. The policy now stated once: *content-sized* allocations (size chosen by file bytes or a caller-supplied count) are charged to a `ReadLimits` ceiling and `try_reserve`d into `AllocationFailed`/`LimitExceeded`; *shape-sized* allocations bounded by the program's own compile-time shape - the two boxes a published-outcome variant holds - use ordinary infallible allocation and abort on refusal, as `Box::new` always does. The consequence, published rather than implied: an allocator refusal after publication ends the process instead of substituting a different outcome, so no caller observes a *wrong* outcome, and the on-disk state is the published one. The correction is documentation (option c of three); (a) pre-staging cannot remove `Box::new(source)` because the source only exists once the step has failed and `Error` is recursive, and it would put two allocations on the success path of every durable cell write to serve a path that ends in `abort`; (b) a non-recursive outcome type cannot carry the caller's own hook error. F-10 was resolved compatibly by *removing* the rebuild's `SparseBitmap::clone` rather than making it fallible | deep |
| Matrix CRC rebuild refuses incomplete evidence | `crates/varve-core/src/matrix.rs`, `docs/recovery-model.md`, `docs/api-reference.md` | **added round 12 (F-04)** - `rebuild_matrix_commit_from_crc` now returns `Error::MatrixFatalCorruption` instead of `Ok(count)` when the block's CRC-validity page index could not be enumerated in full, including under `with_matrix_fatal_forensics`. This is a deliberate narrowing of a recovery path that was publishing false negatives. Both documents state the refusal and that forensic mode does not relax it. Residual: the reused variant's message ends with advice to enable forensics, which is unhelpful for a caller already using it - see open item 16 | deep |
| In-place replacement same-key requirement | `VarveFile::replace_fixed_in_place_exclusive` rustdoc, `docs/api-reference.md`, `docs/durability-model.md`, `docs/update-compact-guide.md` | **added round 12 (F-02)** - the unsafe contract required exclusivity and said nothing about the key, while generated keyed writers read predecessors from the resident cache the mutation invalidated nothing in. The requirement is now stated with its reason (records appended earlier already point into the target and there is no new generation in which to rebuild the chain), alongside the guarantee that holds regardless (the tail map is dropped before the first byte). Not enforced - see open item 15 | deep |
| Fuzz artifact gate | `docs/fuzzing-and-fault-injection.md`, `scripts/run-security-fuzz.ps1` | **corrected round 12 (F-09)** - the document said any file under `fuzz/artifacts` fails the gate; the script checked only the corpus-generation and campaign exit codes, and a harness proved it exits 0 with a seeded artifact still in place. The script now enumerates the directory before the run (exit 2, refusing to start) and after every target (exit 3), and never deletes anything, because a reproducer is the only copy of an input that reached a defect. The document states exactly that, including the two exit codes | deep |
| Resident generated-writer cost and routing | `crates/varve-macros/src/lib.rs` generated rustdoc, `docs/api-reference.md` | **added round 12 (P-01)** - construction primes one keyed-tail map per keyed block and each pass walks the resident index, so cost is `Theta(M*N)` and aggregate retention is about `M *` the per-block-id ceiling. Documented behaviour of the resident path, not a contract violation, but it was published only as a limits footnote. The generated writer type and its `from_inner` now state the cost model and route high-cardinality formats to `key_index = disk` and the generated disk-indexed APIs. Documentation only; no generated logic changed | deep |
| docs.rs published surface | `crates/varve/Cargo.toml`, `crates/varve-core/Cargo.toml`, `crates/varve-macros/Cargo.toml`, `.github/workflows/ci.yml` | **corrected round 9 (F-06)** - the CI rustdoc job said `--all-features` "matches the docs.rs default for this project". It did not: all three publishable manifests have `default = []` and none declared `[package.metadata.docs.rs]`, so docs.rs used its default selection (no optional feature) and every feature-gated public item could be missing from the published pages. All three now declare `all-features = true`, which makes the claim true by declaration rather than by assumption, and CI additionally gates the no-default-feature surface with wording that says which gate covers which surface | deep |

### Platform-conditional paths

| Path | Where | Status | Audit |
| --- | --- | --- | --- |
| Self-test artifact cleanup | `crates/varve-core/src/diagnostics.rs` | ok - Windows deletes through the verified non-delete-shared handle; Unix deletes via `openat`/`unlinkat` confined to an exclusively owned directory, or **refuses with a typed `Environment` step failure** | carried |
| Writer-lock marker cleanup | `crates/varve-core/src/diagnostics.rs` | **improved round 6, residual stated** - was `remove_file` by pathname on both platforms, guarded only by an advisory lock; now routed through `remove_path_if_same_object`, with `Refused`/`Failed` reported as a typed `cleanup` step (domain `Environment`) instead of swallowed (API3-03). The identity is captured by opening **the same unverified pathname**, so it closes the capture-to-unlink window but does not prove the marker is ours. On Unix the exclusive-directory confinement carries the real protection; on **Windows there is no directory confinement**, so a marker swapped in before the capture is still deleted. See open item 5 | deep |
| Writer-lock marker mutation (`<target>.lock`) | `crates/varve-core/src/file.rs` | **added round 9 (F-07)** - acquisition truncates and rewrites the object the marker path names, and drop truncates it again, so a pre-placed hard link or a followed symbolic link from that path routed those writes at a foreign object (a zero-length object is treated as an unused marker, so an unrelated empty file was an eligible victim). Done on **both** platforms, not one: Unix opens with `O_NOFOLLOW` and maps `ELOOP`/`EMLINK` to the refusal; Windows opens the reparse point itself with `FILE_FLAG_OPEN_REPARSE_POINT`, so no write can reach a link target whatever the check then decides, and rejects it by attribute. Both then require a regular file with `nlink == 1` and otherwise return `Error::WriterLockMarkerNotDedicated { path, reason }`. Any other platform refuses typed rather than silently offering less. This never affected authoritative single-writer exclusion, which is a native object lock on the target file itself. Residual: the Windows reparse-point branch is compile-verified and asserted by a test that **skips loudly** where the host does not grant symbolic-link creation - it did skip on the round-9 host; the hard-link branch ran and passed there | deep |
| Replacement publication + parent sync | `crates/varve-core/src/file.rs` | ok - typed `PublishedButParentSyncPending` / `ReplacePublicationIndeterminate` on both platforms | spot |
| Sparse range removal / hole punching | `crates/varve-core/src/matrix.rs` | ok - qualified and runtime-detectable rather than claimed uniformly | carried |
| File identity | `crates/varve-core/src/file.rs` | ok | spot |

## The rule

**Any new persisted structure, cache, registry, cost model, or published claim
must be walked against all five invariants, and the walk recorded in this table,
before it merges.** A fix that answers a finding but is not walked is how the
last three rounds produced their blockers.

Two mechanical habits catch most of it:

1. When you add a structure whose size depends on file content or a
   caller-supplied count, ask which `ReadLimitKey` charges it. If the answer is
   "none, but it uses `try_reserve`", the answer is no.
2. When you add a step after an append, publish, or rename, ask what the caller
   sees if it fails. If the answer is a bare `Err`, the answer is no.
3. Round 9's addition: habit 2 applied to *one* site is how four consecutive
   rounds each found the next instance. Apply it to **every** authoritative
   commit in the file you are touching, and record the result below, so the
   next round is confirming an enumeration rather than discovering one.

## The class-3 enumeration

Every authoritative commit walked in round 9, and what runs after it. "Safe"
means the post-commit steps are infallible, allocation-free and free of
user-supplied code; anything else names its typed published outcome.

### `crates/varve-core/src/file.rs`

| Commit | What follows it | Status |
| --- | --- | --- |
| `write_record_with_prev_key` (the append) | `snapshot.with_len` (fallible - routes to `rollback_append`: truncate plus index/cadence/tails restore, `WriteRollbackFailed` and poison if the rollback itself fails), then the index install (pre-reserved), `checkpoint_cadence.note_appended`, `block_tails.note_appended`, snapshot assignment, `publish_sequence` | safe - the one fallible step is rolled back; the rest are infallible and none is user code; `block_tails` growth is bounded by the format's declared block count. (`with_len` added to this row in round 11; classification unchanged.) **Round 12's enforcement pass made the "pre-reserved" clause structural rather than asserted**: the charge and the `try_reserve` now produce a `ReservedIndexSlot`, and `install` — which takes it by value and returns `()` — is the only `push` onto the resident index outside file open, so the reservation cannot be moved below the write. The function also no longer calls the poison guard itself; it demands a `MutationPermit`, so a sibling append helper cannot reach the write without the check |
| `push_keyed_info` / `delete_info` | `commit_keyed_tail` only: an insert of an already-owned `Vec<u8>` into a slot reserved and charged beforehand | safe - round 9's F-01 fix. On the fault-injection-only reservation-loss path it invalidates the block's map rather than keeping a stale predecessor |
| `push_with_prev_key_info` / `delete_with_prev_key_info` | nothing - invalidation happens before the append | safe |
| `replace_block` / `replace_fixed` / `replace_rewrite` | fallible rebind after `replace_path_atomically` | typed - `PublishedButRebindFailed` (and poisons) or `PublishedButParentSyncPending`. **Corrected round 12 (F-07):** the two outcomes are not exclusive, and the rebind used to be called with `?` on the parent-sync-pending arm, so a double fault returned `PublishedButRebindFailed` alone and silently dropped the already-observed sync failure. The variant now carries `parent_sync: Option<Box<Error>>` and renders both clauses; `Error::with_pending_parent_sync` attaches the fact only to that variant, because every other error from a replacement path means publication did not happen. Regression: one double-fault test per replacement API, driven by a single injector arming (`WriteFault::ParentSyncThenRebindAfterPublish`) present on **both** the unix and windows `sync_parent_directory` branches |
| `replace_fixed_in_place_exclusive` | two infallible index field assignments plus `publish_sequence` | safe - a write error poisons. **Round 12 (F-02) added the step *before* the write:** the seek/write pair moved into `VarveFile::overwrite_record_bytes_in_place`, which drops the block's resident keyed-tail map before the first byte reaches disk. **Round 13** moved the invalidation again, into `RecordOverwrite::prepare`, and made that permission the only route to the write, so the property survives a new mutation path written by someone who never reads this row. That is a pre-commit, infallible, allocation-free step, so the classification of this row is unchanged; what changed is that a key-changing in-place replacement can no longer leave a stale predecessor behind. The residual - the physical `prev_same_key_offset` chain still cannot be repaired if a caller changes a key in place - is an explicit `# Safety` requirement rather than an enforced one. See open item 15 |
| `write_matrix_cell_durable_with_barrier` | exactly two steps after `commit_matrix_cell`: (1) `barrier.sync_matrix_commit`, (2) the caller's hook | **both typed.** (2) round 9 - `MatrixCommittedButHookFailed` carrying the event; `commit_event` moved *before* the commit. (1) **fixed round 11** - it was still a bare `Err` (poison only) although the commit bit was already on disk and the cell readable after a clean exit, i.e. the same defect as F-04 one step earlier and the same shape as the round-10 `commit_durable` fix. It now returns `Error::MatrixCommittedButDurabilityUnproven { event, source }`, carrying the same event, and still poisons. The pre-commit `sync_matrix_data` is deliberately *not* typed: it precedes the commit, so nothing is published. Regression: `matrix.rs::a_commit_sync_failure_is_reported_as_published_and_a_data_sync_failure_is_not`, which asserts both classifications, that the hook did not run, that the writer is poisoned, and that a fresh reader sees the cell committed; negative-controlled against the pre-fix code (it reports the bare `InvalidFormatSpec`) |
| `WriterLock::acquire_with_policy` | marker metadata write after the object lock is held | safe - a failure clears the marker and returns; the marker object itself is now proved dedicated before any mutation (F-07) |
| `write_index_checkpoint`, `write_commit_marker`, `write_embedded_manifest_if_needed`, `push_op`, `write_metadata` | nothing - the append is the last step | safe |
| `commit_durable` | `flush`, `sync_all`, `sync_created_pathname_once`, all fallible, after `write_commit_marker` has appended the authoritative record | **fixed round 10** - `flush`/`sync_all` now return `Error::CommittedButDurabilityUnproven { sequence, source }`; `sync_created_pathname_once` already returned `PublishedButParentSyncPending` and is unchanged. Regression: `publication_failure_results.rs::a_durability_failure_after_the_commit_marker_is_reported_as_published`, which also proves the claim the variant makes (the marker is really there, a repeat `commit_durable` returns the same record instead of appending a second marker, and a reader sees the transaction) |
| `write_keyed_values_atomically` (merge/compact publication) | after `replace_path_atomically`: `remove_rewrite_temp_lock_marker`, which returns `()` | safe - typed `PublishedButParentSyncPending` / `ReplacePublicationIndeterminate` on the publication itself, and the only post-publication step cannot fail. Walked round 10; was not in the round-9 table |
| `write_matrix_sidecar_file` | after `replace_path_atomically`: nothing but the `match` that classifies it | safe - same typed publication outcomes. Walked round 10; was not in the round-9 table |
| `truncate_uncommitted_tail_if_needed` (open-time) | nothing - the truncation is the last step | safe, and note this is *contractual* truncation, not damage: a transaction-marker format defines records after the last marker as uncommitted. Invariant 2 does not apply. Walked round 10 |

### `crates/varve-core/src/matrix.rs`

| Commit | What follows it | Status |
| --- | --- | --- |
| `apply_commit_bit` | was `SparseBitmap::set_byte` (fallible allocation) after the bitmap byte reached disk | fixed round 9 (F-03) - prepare/commit split |
| `apply_cell_crc_valid` | identical shape on the validity bitmap | fixed round 9 - not named by the review |
| `write_cell` / `write_cell_payload` | was `charge_current_write_bit` after the commit bit, validity bit and payload were durable | fixed round 9 - `prepare_current_write_bit` / `commit_current_write_bit` / `abandon_current_write_bit`; not named by the review |
| `clear_category` | two `checked_sub` settlements ran after the regions were zeroed | fixed round 9 - both totals derived before the first disk mutation |
| `rebuild_commit_map_from_crc` | `budget.pages.checked_sub(released)` ran after publication | fixed round 9 - derived before it |
| `record_page_index_entry` / `release_page_index_entry` / `republish_page_index_entry` / `compact_page_index` | every persisted page-index entry and header write | **restructured round 12 (F-03), and this replaces four rows with one property.** The round-9 rows claimed the mirror capacity was reserved before the header write; that was false for `compact_page_index`, which wrote the sorted entries to disk and only then `try_reserve`d the mirror, so an `AllocationFailed` left a usable writer whose slot map no longer described the array and whose next removal could overwrite a live page's only entry. All four are now `prepare`/`commit` pairs behind the private `mod page_index` (see [Mechanically enforced shapes](#mechanically-enforced-shapes)): every fallible step — budget charge, mirror reservation, offset and count arithmetic, byte encoding — is a precondition of the first disk byte, and the install afterwards is `clear`/`extend`/`push`/`insert` into reserved capacity. `compact_page_index` no longer exists as a function; compaction is a phase of the append. The invariant that replaces the enumeration: **after a prepared page-index mutation begins writing, the only error it can return is `Error::Io`.** The header-failure unwind and the release-path resynchronisation are preserved verbatim inside the gate. Regression: an injected mirror-reservation failure, negative-controlled against the pre-fix ordering, which fails at the disk-order comparison and then at an end-to-end read after reopen with no allocation map |
| `write_commit_map_pages` | the whole republication | fixed round 9 (F-02) - marker, sync, refill, publish; failure past the marker calls `poison_interrupted_rebuild` |
| `poison_interrupted_rebuild` (runs after the destructive marker is durable) | `format!` plus `Vec::push` to build the fatal finding | safe, and enumerated in round 11 rather than left implicit: this is allocation on a post-destruction path, but the allocator is abort-on-OOM here and the on-disk marker fails closed on its own, so a lost in-memory finding cannot turn into a silent recovery |
| `clear_category` page-index ordering | zeroes the index before the map it describes | safe by direction - an interruption leaves exactly the outcome the caller asked for; documented, needs no marker |
| compaction entry ordering (inside `PreparedAppend::commit`) | the compacted run and the appended entry precede the count that admits them | safe by direction - an interruption leaves the old, larger count naming a superset of the live set. Round 12 removed the intermediate header write the separate `compact_page_index` used to publish, so there is one header write, not two |
| whole-region `zero_range` erasure (`clear_category`, the rebuild's entry-region reset) | an infallible `SparseBitmap::clear` / mirror `clear()` | safe, and this is the **deliberate exception** to the page-index gate: it is not an entry or header write, and it takes no capacity |
| `rebuild_commit_map_from_crc` evidence gate | - | **added round 12 (F-04)** - not a post-commit step but a pre-commit refusal, recorded here because it removes an authoritative publication rather than ordering one. The rebuild used to treat an absent CRC-validity bit as "not committed" even when the reader had already flagged that block's validity page index as unreadable, so a forensic rebuild could publish false negatives over the previous commit view. `load_page_index` now returns whether the index was *complete*, `load_paged_bitmap` returns that separately from "the bytes I loaded were authentic", and the rebuild refuses with `Error::MatrixFatalCorruption` before doing anything. The refusal is **unconditional**: `with_matrix_fatal_forensics` relaxes reading fatal state, not a destructive republication, following `poison_interrupted_rebuild`. The other consumer of the same evidence (`verify_cell_crc`) was checked and already fails closed |
| `write_aux_at_len`, `update_cell_crc`, `clear_cell_crc`, `commit_cell`, `clear_cell` | no memory update after the disk write, or safe-direction ordering (commit bit last on commit, first on clear) | safe |

### `crates/varve-macros/src/lib.rs`

| Commit | What follows it | Status |
| --- | --- | --- |
| generated `push_<block>` / `delete_<block>`, inherent and trait routes | nothing | fixed round 9 (F-01) - the per-writer typed map is gone |
| generated `replace_<block>` | was `__varve_translate_tail_offsets` after `replace_block` published a new generation - fallible (`ResourceArithmeticOverflow`), and on failure returned a bare `Err` with a half-translated map | fixed round 9 by removal; not named by the review |
| generated `from_inner` | priming is fallible but precedes every mutation | safe |

### `crates/varve-core/src/disk_index.rs` (round 10)

| Commit | What follows it | Status |
| --- | --- | --- |
| `DiskIndexWriteBatch::commit` (the redb transaction commit) | `store_tail_cache` | safe - moves an already-owned `BTreeMap` into a `Mutex` slot: infallible, allocation-free, and lock poisoning is absorbed with `unwrap_or_else(into_inner)`. The map is *taken* from the cache when the batch opens, so any failure before the commit leaves the cache empty (a miss) rather than stale - the design note at `take_tail_cache` states this and it holds |
| `begin_generation`, `publish_clean`, `commit_after_native_sync` | nothing - each returns the metadata it already computed | safe |

### `crates/varve-core/src/stream.rs` (round 10)

| Commit | What follows it | Status |
| --- | --- | --- |
| `append_prepared_chunk` (the native write) | `snapshot.with_len` (fallible, rolled back by truncate+seek, and a failed rollback poisons with `WriteRollbackFailed`), sequence/count/`set_tail` (infallible, bounded by the declared block count), `stage_state_records`, `commit_state_chunk` | typed - **corrected round 11.** The round-10 row claimed "both sidecar steps poison and return `PublishedButIndexStale`" and that was **false for `commit_state_chunk`**: only its `batch.commit()` failure was typed, while the primary-generation restamp ahead of it (`primary_generation`, which reads the primary and allocates, and `set_primary_generation`) returned a bare `Err`. The restamp fires whenever the witness window is still filling, i.e. the early life of every stream file, on ordinary chunk boundaries. Now `commit_state_chunk` captures `batch_last_sequence` *before* its body and routes every failure of that body through `published_sidecar_error`, which poisons and returns `PublishedButIndexStale { sequence }` whenever published records are staged, and leaves the error bare when none are. The classification is around the whole body, not at individual steps, so a fallible step added to this path later cannot reopen the defect |
| `append_prepared_chunk_summarized` (round 11) | assignment of the pre-derived `BatchAppendInfo` into the caller's summary | safe - all the summary arithmetic is checked *before* the native write (`next_batch_summary` is a pure function), so the only post-publication step is an infallible assignment. It replaced three `update_batch_summary` call sites that ran fallible checked arithmetic *after* the chunk was published and returned a bare `Err`; the failure would have handed the caller a `BatchAppendError::written` that under-reports what is in the file, which is the value `push_iter` relies on to decide whether to poison |
| every mutating entry point (`append_prepared_chunk`, `append_prepared_chunk_summarized`, ...) | - | **added round 12 (F-05), shape B rather than shape 3.** The poison flag was checked by the public guard, but `append_prepared_chunk` - reachable from `indexed.rs` - performed the guarded mutation without consulting it, so a single-record indexed put or delete could proceed after a failed append rollback had already poisoned the stream, contradicting the published `WriterPoisoned` contract. Fixed with a witness token: `PoisonFlag` owns the boolean and `MutationPermit` can only be produced by `PoisonFlag::issue`, which is private to the module (round 14) and reachable only through `GuardedWriter::writer_permit`, i.e. from the writer whose flag it reads. See [Mechanically enforced shapes](#mechanically-enforced-shapes) |
| `VarveStreamWriter::sync` | `publish_clean` after `sync_all` | safe - a failure poisons and returns a bare `Err`, which is *truthful* here: `sync` never claimed durability, and the sidecar stays dirty so reopen restores the last clean checkpoint. Nothing is claimed that is not true |
| `restore_checkpointed` (truncate + `sync_all` of the native file) | `commit_after_native_sync`, then seek/metadata/snapshot rebind | safe by idempotence - the truncation target is the sidecar's own checkpoint base, so a failure at any of these leaves a state that the identical next attempt reproduces exactly |
| `VarveStreamWriter::create` (`sync_all` of the fresh header) | `create_state_store` | safe by direction - a failure leaves a header-only native file with no sidecar, and the retry path (`create_unmanaged`) truncates it |

### `crates/varve-core/src/indexed.rs` (round 10)

| Commit | What follows it | Status |
| --- | --- | --- |
| `publish_update` / `publish_coverage` | the coverage/update application after the native publication, then `finish_record` | typed - poisons and returns `PublishedButIndexStale { sequence }` |
| `publish_prepared_chunk` (the batch path) | `crate::stream::update_batch_summary` (fallible arithmetic) and `commit_pending_batch` | **corrected round 11.** The round-10 row claimed this whole file was "typed - poisons and returns `PublishedButIndexStale`". It was false on the batch path: both post-publication steps returned a bare `Err`. Demonstrably an oversight rather than a design choice, because the *single-record* path wrapped the identical `commit_pending_batch` call (`finish_record`) and the batch path did not. Now the chunk is published through `append_prepared_chunk_summarized` (summary arithmetic before the write, infallible assignment after) and `commit_pending_batch` types its own failures |
| `commit_pending_batch` | the primary-generation restamp (`primary_generation`, `set_primary_generation`) and then the sidecar batch commit, all after the native publication | typed - **corrected round 11**, same defect and same fix as `commit_state_chunk`: `batch_last_sequence` is captured before the body and every failure of the body goes through `published_index_error`, which poisons and returns `PublishedButIndexStale { sequence }` when published records are staged. The wrapping now lives in the function that owns the post-publication work, so it covers both callers and any future one. It also removed a smaller untruth: the commit-failure arm used `batch_last_sequence.unwrap_or(0)`, reporting sequence 0 as published when nothing was |
| `push_info` / `delete_info` (the single-record path) | - | **added round 12 (F-05).** The indexed writer kept a *second* poison flag which could disagree with the stream's: after an append rollback failure the stream was poisoned and this one stayed clear, so a later put or delete passed the indexed guard and called the stream's prepared append directly. The second flag is deleted - `ensure_not_poisoned` now reads the stream's flag and `poison` sets the stream's - and both call sites take a fresh `MutationPermit` after `ensure_update_capacity`, because that call can itself commit a sidecar chunk and poison the writer. The refusal still names `"indexed"` when it is reached through this API |
| `rebuild_disk_index` publication (`publish_temp_path_atomically`) | primary-identity re-check, sidecar retirement, durability classification | typed - every post-publication step resolves *against* publication (retire the sidecar and report a typed mismatch, or `PublishedButParentSyncPending`); the source comment states the rule and the code follows it |

### `crates/varve-core/src/layout.rs` (round 10)

| Commit | What follows it | Status |
| --- | --- | --- |
| `LayoutWriter::write_segment` | the whole segment body is one fallible closure; on `Ok`, two infallible field assignments | safe - every failure truncates to the pre-segment EOF and restores the in-memory segment count, and a failed rollback is reported as `WriteRollbackFailed`. The back-patches at the end are inside the rolled-back region |

`crates/varve-core/src/native_layout.rs` was walked and holds no commit point:
it is pure header/record encoding into a caller-supplied writer.

## Known open items

These are recorded rather than silently accepted. Each names the file and the
recommended remediation.

1. **`hash_table_reservation_bytes` is now `pub(crate)`**
   (`crates/varve-core/src/codec.rs`), which unblocks adding a
   `state_table_overhead_bytes` field to `KeyedMergeEstimate`. That would make
   the estimate closer, but still not a bound - `Key`/`T` heap is unobservable
   without a caller-supplied-bounds trait - so it must stay under the "structural
   estimate" contract. Not blocking.
2. **CLOSED in round 9** - was "the generated keyed writers' charge is inline
   storage only", predicted to need a `Key`-provided heap-size hook. It was
   closed without that hook: the generated writers no longer store `T::Key` at
   all (F-01), so there is no key heap to observe indirectly. They share the
   resident byte-keyed cache, which charges inline storage plus the key payload
   bytes it owns. Regression:
   `generated_keyed_atomicity.rs::a_generated_keyed_writer_charges_the_key_bytes_it_retains`.
3. **`max_keyed_tail_bytes` is checked per keyed block id, not summed across
   block ids.** A format with many keyed blocks can therefore hold that many
   times the ceiling. Summing would need a per-file running total in
   `KeyedTails`; the per-block-id semantics are documented in
   `docs/api-reference.md` and in the field's own rustdoc.
4. **The sidecar publication interposition hook in `crates/varve-core/src/stream.rs`
   is `#[cfg(test)]` process-global state**, keyed by the canonical primary path.
   It is compiled out of every non-test build. Narrowed in round 8 (F-04): it
   was a single `Mutex<Option<(PathBuf, Hook)>>`, so one slot held one hook and
   a second test arming it silently discarded the first — under a parallel
   `cargo test -p varve-core --all-features --lib` the two sidecar-retirement
   tests raced for that slot. It is now a `Mutex<Vec<(PathBuf, Hook)>>`:
   consumption `swap_remove`s only the entry whose path matches, and
   registration asserts that no hook is already armed for the same path, so a
   duplicate registration is a loud failure rather than an abandoned hook. A
   test that arms it and then fails before the publication point still leaves
   its entry armed for the rest of that binary's run, but that leak is now
   *visible* — the next registration for the same path asserts — rather than
   silently changing another test's behaviour. A thread-local cell would still
   be tidier. Reproduction note for CI: the race is thread-count dependent and
   did not reproduce at `RUST_TEST_THREADS=4` on the round-8 integration host;
   it was proved directly by forcing the interleaving
   (`--test-threads=2 retires_a_sidecar_published_for_a_replaced_primary`),
   which failed 15 of 15 runs pre-fix and passed 20 of 20 post-fix.
5. **Windows writer-lock marker removal has no directory confinement.**
   `remove_unowned_lock_marker` (`crates/varve-core/src/diagnostics.rs`)
   captures the marker's identity by opening the same unverified pathname, so
   the identity check is self-referential: it closes the capture-to-unlink
   window but proves nothing about whose marker it is. On Unix the exclusive
   directory confinement in `remove_path_if_same_object` supplies the real
   protection; Windows has no equivalent, so a marker swapped in before the
   capture is still deleted. Closing it needs the marker's identity to be
   captured at creation time (in `file.rs`) and threaded to the removal, or a
   Windows directory-handle-relative delete.
6. **`AllocatedExtents` grows with infallible `Vec::push` and is capped after
   the fact** (`crates/varve-core/src/matrix.rs`). Bounded and small - see the
   inventory row for the exact bound on each platform - but the cap is a
   compile-time constant rather than a `ReadLimits` charge, and OOM there would
   abort rather than return a typed error.
7. **The rounds-1-2 module set is not walked.** See the scope note at the top of
   this document. Recommended: assign `disk_index.rs`, `stream.rs`,
   `indexed.rs` and `scan_control.rs` an explicit owner and walk each against
   the five invariants, adding rows here, before the next release.
8. **The Unix cleanup branch is compile-verified, not executed on this host.**
   `cargo clippy --target x86_64-unknown-linux-gnu ... -D warnings` passes for
   the lib and its tests, but the unix-gated tests in `diagnostics.rs` and
   `self_check.rs` have never been run. The Linux CI leg must confirm them,
   especially if it runs as root.
9. **CLOSED in round 10** - was "`VarveFile::commit_durable` returns a bare
   `Err` after the commit marker is appended". It now returns
   `Error::CommittedButDurabilityUnproven { sequence, source }`, and
   `docs/durability-model.md` gained the section "Commit Marker Versus Commit
   Durability" stating the three-way distinction (not committed / committed but
   durability unproven / committed but created-pathname durability pending) in
   the same pass, which is what the deferral was waiting for.
10. **The Windows lock-marker reparse-point branch has never executed.** The
   F-07 test `a_reparse_point_lock_marker_is_refused_and_left_untouched`
   announces a loud skip where the host does not grant
   `SeCreateSymbolicLinkPrivilege`, which is the case on the round-9
   development host. The hard-link branch runs on both platforms. A Windows CI
   leg with developer mode enabled, or an administrative local run, would
   close this.
11. **`crates/varve-core/src/diagnostics.rs` is edited by whoever adds an
   `Error` variant.** `classify_error` is an exhaustive match over `Error`, so
   every new variant forces a change there even when the finding being fixed is
   entirely in another file. Round 9 added two variants and therefore two lines
   to a file it did not otherwise own; round 10 added a third for
   `CommittedButDurabilityUnproven`. A catch-all arm would remove the
   compile-time forcing function, which is worth keeping; the cost is that
   `diagnostics.rs` cannot be assigned exclusively to one agent while error
   variants are in flight elsewhere. Round 11 added a fourth for
   `MatrixCommittedButDurabilityUnproven`.
14. **CLOSED in round 11** - was the second residual named by the round-10
   verifier: the round-10 `stream.rs` and `indexed.rs` enumeration rows asserted
   a property the code did not have. Two live instances of class 3 survived on
   the *published batch-append* paths, in modules the sweep listed as fully
   enumerated: the primary-generation restamp inside
   `stream.rs::commit_state_chunk` (reached from `append_prepared_chunk` on
   ordinary chunk boundaries) and inside `indexed.rs::commit_pending_batch`,
   plus `indexed.rs::publish_prepared_chunk`'s post-publication
   `update_batch_summary`. All three are fixed structurally rather than
   pointwise - see the corrected rows in the enumeration - and the two rows are
   rewritten. A third, unnamed instance was found by walking forward from the
   same commit and closed with them: the same `update_batch_summary`-after-the-
   write shape at three call sites in `stream.rs::push_iter_inner`. Regressions:
   `publication_failure_results.rs::a_restamp_failure_after_a_published_stream_chunk_is_reported_as_published`
   and `..._indexed_chunk_...`, both negative-controlled against the pre-fix
   classification (they report the bare injected `Io` error).
13. **CLOSED in round 11** - was the residual named by the round-10 verifier:
   `write_matrix_cell_durable_with_barrier` returned a bare `Err` when
   `sync_matrix_commit` failed after the commit bit was already on disk, and
   `docs/durability-model.md` claimed every error other than the hook variant
   meant the cell was not committed. Both are fixed together: the step returns
   `Error::MatrixCommittedButDurabilityUnproven { event, source }`, and the
   document now enumerates the two post-commit steps and states that the
   pre-commit data sync is the one that stays a plain error. The forward walk
   from that commit point is complete - there is no third step.
15. **A key-changing in-place replacement is a contract, not a check**
   (`crates/varve-core/src/file.rs`). Round 12's F-02 was corrected by
   invalidating the resident keyed-tail map before the write, not by the
   report's preferred blanket rejection, because rejecting keyed blocks breaks
   `crates/varve/tests/native_security_hardening.rs::keyed_tombstone_order_uses_sequence_before_physical_ordinal`,
   an existing test of a legitimate *same-key* in-place replacement. `T:
   VarveBlock` exposes no key and `FieldDescriptor` carries no key flag, so a
   precise "reject only key-changing" check is not expressible at that
   signature. Two routes close it: add
   `replace_keyed_fixed_in_place_exclusive<T: VarveKeyedBlock>` that decodes the
   stored record and refuses unless the keys match, reject `T::IS_KEYED` at the
   `VarveBlock` entry point on a `keyed_offset_chain` format, and dispatch the
   generated `replace_<block>` accordingly (`crates/varve-macros/src/lib.rs`);
   or add a `key: bool` to `FormatDescriptor`/`FieldDescriptor` so the key byte
   range is computable from `T::FIELDS` and the existing signature can reject
   only genuine key changes. The physical `prev_same_key_offset` chain remains
   unrepairable if the contract is violated.
16. **`Error::MatrixFatalCorruption` is reused for the F-04 refusal**
   (`crates/varve-core/src/matrix.rs::ensure_crc_valid_evidence_complete`). It
   is honest as far as "a fatal recovery finding blocks this operation", and it
   matches the existing unconditional refusal from `poison_interrupted_rebuild`,
   but its message ends by recommending `with_matrix_fatal_forensics` - which is
   useless advice for a caller that has already enabled forensics and is being
   refused anyway. Recommended:
   `Error::MatrixRecoveryEvidenceIncomplete { block_id: u32 }` saying the
   validity page index could not be enumerated in full and that a rebuild would
   publish absences as uncommitted. Adding it forces an arm in
   `diagnostics.rs::classify_error` (open item 11) and a one-line change at the
   refusal.
17. **F-04's explicitly named discard operation does not exist.** The review
   asked that an operator who genuinely wants to discard unverifiable
   visibility be able to, through a distinct, explicitly named operation. Only
   the refusal was implemented, because the public entry point belongs in
   `file.rs`/`lib.rs`. Suggested shape:
   `VarveWriter::rebuild_matrix_commit_from_crc_discarding_unverifiable::<T>()`,
   documented as destructive and as *not* a recovery of the commit view. The
   internal parameter for it was deliberately not added, because an unreachable
   variant is dead code.
18. **`finish_matrix_mutation`'s rustdoc does not name the page-index gate**
   (`crates/varve-core/src/file.rs`). Its narrow `Error::Io` poison predicate is
   correct and **must not be widened** (see the round-10 note above), and round
   12 strengthened the reason: after a prepared page-index mutation starts
   writing, `Error::Io` is the only error it can return. The rustdoc should name
   `matrix.rs`'s `mod page_index` as the second structure discharging that
   obligation, alongside the existing `prepare_byte_write`/`commit_byte_write`
   pairing. Documentation, not behaviour.
19. **`SparseBitmap: Clone` still exists** (`crates/varve-core/src/matrix.rs`).
   F-10's only fallible-relevant clone site was removed rather than made
   fallible, but the derive is retained because `MatrixLayout` derives `Clone`
   and holds `SparseBitmap` values. If `MatrixLayout::clone` has no live caller
   (it is `pub` and reachable from `file.rs`), dropping both derives would make
   the infallible-container-allocation class unrepresentable for this type too.
20. **`WriterLock::drop` still discards `clear_writer_lock_info`'s error**
   (`crates/varve-core/src/file.rs`), because `Drop` cannot report. Round 12's
   F-08 made the consequence visible - the self-test now reports a failed
   `cleanup` step when the marker cannot be re-acquired or identified, and the
   next run's re-acquisition fails loudly - but a caller with no self-test still
   gets no signal. An explicit `release()`/`close()` returning the error would
   close it; that is an API addition.
21. **`MutationPermit` binds the writer's *type*, not its instance**
   (`crates/varve-core/src/writer_permit.rs`). A permit obtained and then held
   across an intervening poisoning call would still be accepted. Every
   cross-module entry point consumes it by value and every disk-touching step
   re-takes it, so no live code holds a stale one, but that is a convention
   rather than a type. Round 14 closed the *decoy-flag* half of this (a
   throwaway `PoisonFlag` can no longer mint a witness at all, and a permit for
   one writer type cannot be spent on another), which leaves only the case of a
   second, genuinely healthy writer of the same type: producing one now costs a
   second open file rather than one stack `bool`, and its flag really was
   checked, so the residue is misattribution rather than a bypassed check. Binding the permit's lifetime to the flag conflicts with
   `&mut self` methods unless each writer's fields are split into a
   borrow-disjoint inner struct — a refactor of `stream.rs`, `file.rs` and
   `layout.rs` larger than the enforcement pass covered.
22. **`matrix.rs`'s prepared page-index values have no `tests/ui` proof.** The
   four prepared types and the two disk-writing functions are private to
   `mod page_index`, and that privacy *is* the enforcement, so exposing them
   through `enforcement_probe` to write a compile-fail fixture would destroy
   what the fixture was proving. The substitute is the source-level gate
   `enforcement_gates.rs::the_matrix_page_index_writers_stay_unreachable_outside_their_gate`,
   which fails if either function gains a visibility modifier or leaves the
   module. If the crate ever grows an internal compile-fail harness (a
   `trybuild` suite compiled *as part of* `varve-core`, e.g. via a
   `#[cfg(test)]` sub-crate under `crates/varve-core/tests/`), this should be
   the first thing moved onto it.
23. **Three mirrors are safe today by inspection, not by type.**
   `disk_index.rs::store_tail_cache` (moves an already-owned `BTreeMap` into a
   `Mutex` slot), `stream.rs::set_tail` (a slot bounded by the declared block
   count) and the matrix quarantine map are all installed infallibly after
   their disk write, so shape A does not apply to them *as written*. None of
   them is behind a prepared-value gate, so a fallible step added after the
   write would not be refused by the compiler. Recommended: route them through
   the same prepare/commit shape when one of them next needs to change,
   starting with `store_tail_cache`, which is the one on a published batch
   path.
24. **A cleared category cannot rebuild until reopen.** `clear_category`
   clears the validity bitmap and its on-disk index but deliberately does not
   reset completeness: a cleared map is not a re-enumerated one, and a session
   that opened on a damaged validity index still has no basis for reading
   absence as proof. The effect is fail-closed but operator-visible — a writer
   that clears a category still cannot `rebuild_matrix_commit_from_crc` in that
   session. Closing it properly means re-enumerating the evidence (or admitting
   that a cleared map is complete-by-construction, which needs the clear path to
   prove the index is empty rather than assume it). Asserted as-is by
   `matrix.rs::crc_valid_evidence_tests::clearing_the_map_does_not_restore_completeness`
   so the behaviour cannot change silently.

12. **Invariant 2 spot-check of the remaining "destructive in-place
   republication" shapes, round 10.** F-02 was that shape in the matrix page
   index. The other persisted structures were checked for it and none has it:
   the redb sidecar publishes a new generation through a savepoint and a
   metadata commit and never edits one in place; the stream and indexed sidecar
   rebuilds write a private temp and publish by atomic replacement; matrix
   sidecars do the same; the index checkpoint is *appended* as an ordinary
   record rather than overwritten; `clear_category` and `compact_page_index`
   were re-derived by round 9 as safe-by-direction. The one destructive
   in-place operation that remains is `truncate_uncommitted_tail_if_needed`,
   and it is contractual rather than damage (see the class-3 table). Not
   walked for this shape: `format.rs`'s manifest region, which no round has
   opened.

## Related reports

- `docs/performance-stability-review-2026-07-21-f661f65-final.md` (round 12, the
  round that retired enumeration in favour of the enforcement types above)
- `docs/performance-stability-review-2026-07-21-8732e83-final.md` (rounds 9-11)
- `docs/performance-stability-review-2026-07-20-060851a-final.md` (round 5, the
  six blockers this checklist exists to stop recurring)
- `docs/performance-stability-review-2026-07-20-0d4b9c6-final.md` (round 4)
- `docs/adversarial-performance-security-review-2026-07-19-4e07a3f-final.md`
  (round 3)
- `docs/adversarial-performance-security-review-2026-07-19-opus-final.md`
  (round 2)
- `docs/adversarial-performance-security-review-2026-07-19.md` (round 1)
