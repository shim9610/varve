# Changelog

All notable repository releases are documented here. Varve follows semantic
versioning; while the crates remain below 1.0, incompatible Rust API changes
increment the minor version.

## Unreleased

### `follow()` — a reader that advances without reopening

A handle fixes its snapshot length when it opens and reads positionally against
it. That is what lets one handle serve concurrent readers through `&self` while
a writer appends — a reader can never observe a record the writer has not
finished — but it also meant the handle never observed a finished one either.
Measured on a five-record file grown to fifteen, the open handle went on
reporting 6 records while a fresh open of the same path reported 17. Reopening
was the only way forward, and on a scanning open that is `O(records)`.

`VarveFile::follow` and `VarveReader::follow` frame only the bytes past the end
the handle already holds and adopt them, returning the bytes gained. Following a
growing file costs `O(appended)` per call, and the `ScanBytes` charge is the
tail rather than the file, so a long-lived stream reader does not walk into that
ceiling for reading nothing new.

Derived state the handle caches is dropped with it — keyed tails, and the
matrix chunk directory. That directory is built on the first chunked read and
kept in a `OnceLock` that nothing invalidates, which was sound only while a
handle's snapshot could not grow. Measured while verifying this: without the
drop, a followed handle answered `MatrixNotCommitted` for a cell that a fresh
open of the same file read back.

It stops where an open stops. Under a `transaction_marker` policy the same
`committed_prefix_len` boundary decides both, so an appended-but-uncommitted
tail answers `0` and the same call adopts the whole run once the marker lands.

Three things it deliberately does not do. It does not cross a generation: the
replacement paths publish by renaming a new file over the pathname, and this
follows the *object* the handle opened, so a followed handle stays a complete,
self-consistent view of its own generation and answers `0` forever after a
republish. It follows the append log only — a matrix cell region sits ahead of
the log and was never bounded by the snapshot. And a read-write handle answers
`0`, because varve admits one writer per object and that writer's own appends
already extend its snapshot.

`VarveReader::follow` is the only `&mut self` method on that type. Every read
still takes `&self`.

### `is_current()` and `reopen_readonly()` — moving to a republished generation

`follow()` deliberately does not cross a generation, which leaves a handle bound
to a replaced object answering `0` forever with no way to say why. These are the
two halves that close that, and **both take `&self`**.

`is_current` compares the operating system's object identity — device and inode
on Unix, volume and file index on Windows — against what the pathname resolves
to now. One `open` and two metadata calls; nothing is read and nothing is
framed. It is `false` for a removed pathname too, which is exactly the case
where "still works" and "still current" come apart: the descriptor pins the
object, so the handle keeps reading perfectly. Identity reuse cannot fool it,
because a live descriptor is what stops the object from being freed and its
identity handed to something else.

`reopen_readonly` returns a fresh handle on the current generation, by the route
this handle was using — a directoryless handle reopens directoryless, through
the lazy route, so a format carrying `index: header_tails` reopens at a cost
that does not grow with the file. `VarveReader::reopen` is the same on that
type.

`&self` on both is the design rather than a detail. A handle shared as
`Arc<VarveFile>` cannot be mutated, so moving those readers forward means
*replacing* the handle: the owner reopens and stores the new `Arc` while every
reader keeps reading, and the superseded object is released when the last reader
drops it. The alternative — interior mutability so a `refresh(&self)` could swap
a shared handle in place — is deliberately not offered, because it would put an
atomic load on every read, forever, in every format including the ones that
never republish anything.

### `index: header_tails` — a commit boundary a later append cannot hide

**New capability, opt-in, and off by default.** The open digest already writes
down the three facts an open needs, but it writes them as a *record*, and a
record is usable only while it is the file's last one. Anything appended after
it — a partial write, records from a run that then crashed — hides it and the
open falls back to reading everything. The moment a cheap resume is most needed
is the moment that is most likely.

`index: header_tails` puts the same table in a fixed region of the **file
header**, written at create and rewritten at the end of every commit point.
`open_readonly_lazy_with_report` and `VarveWriter::open_lazy_with_report` report
`LazyOpenSource::HeaderTails` when they take it.

The region is sized by the declaration and by nothing else — `8 + 2 x (28 + 12 x
(blocks + 10) + 4)` bytes, **360 for a two-block format**, the same at any
record count, where the `+ 10` is one slot per internal block id. The update rides the durability request that already ends a commit, so
there is **no extra `fsync`**. Measured on a two-block fixture: an open takes
**4 record framings** — the commit marker plus one per distinct block id — and
the same 4 on a file ten times larger, against a scanning open that frames every
record.

**A table at a fixed offset has to earn belief**, because unlike the digest its
position proves nothing: it is always present and always "last". Each slot
records the offset of the commit marker it was written for, and an open frames
that record, requires it to be a commit marker, then walks forward — at most a
segment and a digest may follow one — and requires the walk to land exactly on
the end of the file. A file appended to since that commit, a table left over
from an earlier one, a tail naming another block's record, or a torn slot all
fall back to the scan and answer identically.

Requires `block_offset_chain` (turned on for you) and `integrity: crc32` — the
region is overwritten in place at a constant length, so a torn write leaves
something that frames perfectly, and the per-slot checksum is the only thing
that separates it from a good write. Refused together with a matrix declaration
and with `open_digest_on_flush`.

**Turning it on changes the schema hash**, because the region moves every record
offset in the file. Existing files do not open with it and cannot be given the
region in place. A format that does not declare it is byte-identical to before.

**Two source-breaking changes for callers**, both additive on disk. `IndexPolicy`
gained a public field, so struct-literal construction of one needs updating;
`new` and the `with_*` builders do not. `LazyOpenSource` gained a
`HeaderTails` variant, so an exhaustive `match` on it needs an arm — a handler
that treats it like `Digest` is correct, since both mean "the open read a table
instead of the file".

New on disk: file-header extension block `b"VBTT"`, present only when the option
is declared. See [Format Author Guide](docs/format-author-guide.md) for the
layout and what a reader checks before adopting it.

### A memory-mapped payload window no longer covers the file header

**Soundness, no API change for existing formats.** `mmap_payloads` mapped from
byte 0, so the file header was inside every mapping — harmless while the header
was written exactly once, and not harmless once `header_tails` rewrites part of
it at every commit. Every accessor slices through a `&[u8]` over the *whole*
mapping, so a window held across a commit meant a live shared reference over
bytes the writer was storing into.

The mapping now starts at the append log. Every payload is at or after that
offset by construction, so nothing is lost, and the header is outside every
reference the type constructs. `mmap_matrix` still maps from byte 0 — the matrix
region it serves sits between the header and the append log — which is sound
because `header_tails` and matrix blocks are refused together.

### A lazily opened handle can build its keyed tails

**New capability.** `key_tail_offsets` went through the resident record
directory, which a lazy handle deliberately has none of — so it answered
`NoResidentDirectory`. A *generated* writer primes one keyed-tail map per keyed
block at construction, so a format with keyed blocks could not be opened lazily
at all: the refusal arrived before the caller had done anything.

It now falls back to the offset chains, which already hold the answer.
`prev_same_block_offset` links each record to the previous record of its block,
and `index: keyed_offset_chain` turns that chain on as well as the keyed one, so
files written by earlier versions already carry it. The build collects the
block's chain and the tombstone chain, orders them by record offset, and hands
the result to the same builder the resident path uses — the two agree by
construction rather than by two implementations matching.

Bounded by one keyed block's records plus the tombstones, not by the file.
Measured on a file grown from 50 to 1,000 non-keyed records: **the same number
of entries read at both sizes.** Nothing changes for a handle that has a
resident directory; that path is untouched.

Requires `block_offset_chain`. A format without it still gets
`NoResidentDirectory`, because without the chain there is no second route to
those records.

### Reading one block no longer reads every record in the file

**Performance, no API change.** When the resident index became one slot per
record instead of one entry per record, producing an entry became a positional
read of that record's header — and every walk that wanted a single block was
still written the way it had been when entries were free:

```rust
let entry = entry?;                       // now: read this record's header
if entry.block_id != T::ID { continue; }  // now: and throw it away
```

Ten walks do this; eight of them want one block. The slot now carries the block
id, so the filter runs *before* the read. It costs no memory: `u64` forces
eight-byte alignment, so the `bool` already sat in padding and the `u32` takes
four bytes of it — a const assertion at the one site that grows the index keeps
the slot at sixteen bytes.

Measured on a 1,020-record file holding five records of the block being read:
**1020 entry rebuilds before, 5 after.** Priming four keyed-tail maps over a
70-record file: **280 before, 20 after.** `verify_all` and `index_entries_into`
genuinely want every record and are unchanged.

`VarveFile::take_record_entry_faults()` (behind `scalable-fault-injection`)
counts the rebuilds, which is the unit this cost is charged in. Nothing could
see it before: the open-scan byte counter charges the scan, and these walks run
after it.

## 0.7.0 - 2026-08-10

### A written matrix chunk is editable

**Breaking, behavioural.** A write to a row of an already-written chunk used to
fail with `Error::MatrixChunkClosed`; it now succeeds. See
[API Changes §B.-2](docs/api-changes.md).

The matrix *region* has always been rewritten in place, and a chunk is an
ordinary record, which the writer has been able to rewrite in place since 0.5.0
— so the refusal made a growing matrix the one thing in the file that was
append-only, for no reason in the format. It also caught rows of a chunk that
was merely *skipped*: nothing committed means no record was ever written, and
those rows were unwritable for the life of the file.

The open chunk is written out, the addressed chunk's record is read back into
the buffer, and the next transition rewrites that record where it already sits.
The file does not grow, and no second record for a chunk index is ever created.
Memory is bounded by the same `rows_per_chunk` ceiling as before.

Two costs are real and are written down in the API note. A workload alternating
between two far-apart chunks pays a reload plus a rewrite each time;
append-only streaming never takes the path at all. And **a rewrite is not
crash-atomic**: it overwrites the only copy, so power loss before the next
`sync`, a `write_all` that fails part-way, or a process crash can leave a chunk
holding a mix of its old and new cells. Under `IntegrityPolicy::Crc32` or
`Crc32WithHeader` that is detected — the per-cell checksum sits beside the cell
and a read fails with `ChecksumMismatch`. Under `IntegrityPolicy::None` it is
**not** detected: the record checksum is a constant zero and no per-cell table
exists, so mixed cells are served as values. The damage cannot spread past that
one chunk's cells — the rewrite is byte-length-identical and in place, so
framing, offset chains, the index and every other record are untouched.

`Error::MatrixChunkNotReopenable { chunk, reason }` is new, and reports the two
cases where the payload length would change and so the record cannot be
rewritten at its stored size: `chunk_compression` and `segment_on_flush`. Both
are off by default. `clear_matrix_cell::<T>` and `clear_matrix_cell_by_category`
both reach a written chunk; they are one operation differing only in whether the
block is named by type or by category string, and a test pins that they accept
and refuse the same things. `clear_matrix_category` — the bulk one, no key,
returns a count — reaches written chunks too, and counts them: one reload and
one rewrite per chunk holding the category, memory still one chunk at a time.
It is not atomic and does not claim to be; clearing an already-clear category
counts nothing, so a failure part-way is answered by calling again.

An uncommitted write into a chunk is no longer discarded when the writer moves
on. A chunk counts as needing a write-out once anything is written to it, not
only once something is committed — a widening that was previously unsafe,
because a chunk that went out could never come back and a `write` → `flush`
sequence therefore locked the rest of its rows permanently.

### A **writer** open that reads no record

`VarveFile::open_lazy` and `VarveWriter::open_lazy`, with
`open_lazy_with_report` beside each. Additive; see
[API Changes §B.-3](docs/api-changes.md).

The digest open above landed on the read side only, and that left the standing
requirement — TB-scale files work, memory bounded by the working set — true for
readers and false for the workload this project names as primary. Every writer
open called `load_index` with `ScanIntent::Writer` and framed every record in
the file, at any size, and a continuous appender reopens its writer on every
restart. Measured at 200 records: the scanning open frames **202**, the lazy one
frames **1**. The difference is the file, not the constant.

It inherits both of the read-side open's absences. No resident directory —
`blocks::<T>()` answers `NoResidentDirectory`, and `record_map` is the walk;
appending is unaffected, because the append path maintains the block tails and
the sequence itself and the digest supplied both. And no checkpoint or segment:
a spec declaring `checkpoint_on_flush` or `segment_on_flush` is refused at open
with the new `Error::LazyWriterIndexPolicy`, because both records serialize the
resident index this handle does not keep. Refusing beats opening a handle that
writes nothing and leaves a file slower to open than its spec claims.

Falls back to the full scan for any file whose digest is not usable, exactly as
the read-side open does, and `open_lazy_with_report` returns which route it took.

### One `lseek` per appended record, gone

`AppendSnapshot` took its rollback cursor with `stream_position` — a syscall per
record, on the path whose policy admits none. The same round removed two
`fstat`s from this window and left this one, because the test guarding it counts
`metadata` calls and could not see a seek. Measured at 1,000 records: **2,000
seeks before, 1,000 after**, and the survivor is `append_record_at_end`'s own
`seek(SeekFrom::End(0))` — the check that refuses an append at any offset but
the end, whose removal is the positional-write redesign and not this.

Nothing takes the number now, because nothing needed it: its only consumer was
the value `rollback_append` seeks back to, and a rollback truncates to
`AppendSnapshot::eof` — an offset the append already holds. Restoring that
instead costs no syscall and no bookkeeping. `take_record_file_seeks` (behind
`scalable-fault-injection`) is what makes the count assertable.

The first attempt *tracked* the cursor in `RecordFile` instead, and that version
was unsound. The writer's `snapshot` is a `try_clone` of the same handle, and
`try_clone` shares the open file description — so it shares the offset, and it
seeks it: `SnapshotFile::cursor_at` (the layout scan), every `replace_*` through
`validate_generation_index`, and on Windows every positional read, since
`read_exact_at` is `seek_read` there. That last one moves the offset from another
thread, under `&self`, while the writer holds `&mut` — so no
invalidate-on-handout scheme could have covered it. The cache is gone and
`enforcement_gates.rs` pins its absence.

One cold-path behaviour changed with it. A rollback used to put the handle back
exactly where the append found it; it now leaves it at the restored end of file.
The two differ only for the first append after a reopen — the open scan leaves
the handle where it stopped reading (measured 5493) while `eof` is the committed
end (9143) — and only when that append fails. Nothing reads this handle
sequentially: appends seek `SEEK_END`, rewrites seek their own offset, reads are
positional.

## 0.6.0 - 2026-08-07

### The reader stops keeping a copy of the file

**Breaking, source-level.** Two signatures changed; both are listed in
[API Changes §B](docs/api-changes.md).

An open handle used to keep one `RecordIndexEntry` per record — 104 bytes, for
the life of the handle, growing with the file. It now keeps a 16-byte directory
slot (the record's offset and its committed bit, the two facts not in the
record) and rebuilds the entry from the record's own header and footer when a
read asks. Measured across 2,000 → 20,000 records: **177.5 bytes per record
retained before, 16.00 after.**

`LayoutReader` had the same shape and got the same treatment: a
`Vec<LayoutSegmentInfo>` — a struct plus two `Vec`s of decoded field values per
segment — became a 16-byte slot. Measured 1,000 → 50,000 segments: **343.05
bytes per segment before, 21.07 after.**

The peak of a single open is a separate figure and is unchanged: the scan still
materialises the entries before the directory is taken from them.

### The caller owns the buffers

Every read that returned a collection has an `_into` twin that fills a buffer
you supply — `index_entries_into`, `all_metadata_into`, `block_entries_into`,
`decode_blocks_into`, `blocks_migrated_into`, `keyed_blocks_into`,
`materialized_keyed_blocks_into`, `key_tail_offsets_into`, `segments_into`, and
the generated per-block twins on both the inherent and the trait route. The
allocating forms remain as thin wrappers.

`open_readonly_with_scratch` and `open_with_scratch` take the scan's entry
buffer. Measured, eight opens of a 20,000-record file: **29,824,680 bytes
against 5,969,576** through one buffer — the difference is exactly seven further
copies of the entry array.

Reading one record through an entry you already hold, into a buffer you own:
`read_payload_into`, `read_logical_payload_into`, `decode_block_into`. Measured:
**0 allocations for 20,000 reads** through a cached index.

Internally the per-record read loops now share one payload buffer per walk
rather than allocating one per record. Measured: **1.00 allocations per record
before, 0.00 after** (one for the whole walk).

### A handle can keep no directory at all

`open_readonly_without_directory(spec, path, &mut index)` keeps nothing that
scales with the file and hands you the directory instead. Measured: opening a
20,000-record file allocates **320,218 bytes with a directory and 218 without** —
the slot array is not built, not built-and-freed.

No read is withdrawn in that mode. `with_directory(&index)` answers all of them
against the directory you hold; `RecordDirectory` is implemented for
`ResidentIndex` and for `[RecordIndexEntry]`. Calling a read on the handle
itself returns the new `Error::NoResidentDirectory { operation }`, naming both
the read and the way to answer it — deliberately an error rather than an empty
answer, which would be indistinguishable from an empty file.

### An index you build only as far as the question

`file.record_map(&mut buffer)` returns a `RecordMap<'_>` over a
`Vec<RecordIndexEntry>` you own. It reads nothing when created and walks the
record chain forward only when asked: `find(predicate)` stops at the first
match, `fill_to(k)` stops at `k` entries, `fill()` goes to the end. A second
question resumes where the first stopped, `clear()` empties and rewinds,
`release()` empties and keeps the position.

It implements `RecordDirectory`, so a map *is* a directory —
`with_directory(&map)` answers `blocks`, `scan`, `keyed_blocks` and the rest
against the prefix walked so far. It borrows the buffer, not the handle, so
reads through the handle stay available while the map is alive.

Measured on a 50,000-record, 31 MB file, wanting ten records at position 25,000:
**100,238 read syscalls** through `index_entries_into`, **50,058** through
`record_map` `fill_to`, and **13** to find the first record of a block. A
bounded-memory pass (`fill_to(64)` / `release()` / repeat) covers every record
with 64 entries live instead of 50,100 — 6.6 KB rather than 5.2 MB — and reads
no record twice.

A map walked to completion is the open scan's index, entry for entry. What a
partial one does not do is validate sequence uniqueness past its stopping point.

### An open that reads no record

`IndexPolicy::open_digest_on_flush` makes each commit point append an internal
**open digest** record carrying the three facts an open needs that live in no
single record: where the committed file ends, what sequence the next append
takes, and where each block's newest record sits. `VarveFile::open_readonly_lazy`
reads it and frames **one record**.

Measured, 50,000 records / 31 MB:

| open | read syscalls | bytes read | file overhead |
| --- | --- | --- | --- |
| `open_readonly` (scan) | 100,316 | 3,207,102 | — |
| `open_readonly` (segment chain) | 516 | 7,332,522 | +3.67 MB |
| `open_readonly_lazy` (digest) | **20** | **516** | **+12.8 KB** |

The last column is how a format chooses between the two tail records. A segment
carries an index entry per record it covers, so its overhead tracks the record
count; a digest carries twelve bytes per *distinct block id*, so it is constant —
measured directly, the same format at 200 and at 2,000 records writes exactly
the same 128-byte digest. What a segment buys for its size is an open that
*builds the index*; a digest deliberately does not, and pairs with `record_map`.

The handle keeps no directory. `blocks`, `scan` and `keyed_blocks` return
`Error::NoResidentDirectory` and are answered through `with_directory`;
`block_chain`, `block_tail_offset`, `read_block_at` and every entry-taking
`_into` read need no directory and work directly, which is what the block tails
are in the digest for.

**It falls back to the full scan** for a file with no usable digest — written
before the option, appended past its last commit, truncated, rotted. Not an
error and the answer is identical, but four orders of magnitude, so
`open_readonly_lazy_with_report` returns `LazyOpenSource::{Digest, FullScan}`.
Every field is validated against the file: flipping one bit in the trailer, a
tail offset, a block id or the sequence each falls back and answers what the
file actually holds.

Requires `block_offset_chain`. Composes with `segment_on_flush` and
`checkpoint_on_flush` alike — the three answer different questions. Off by
default; a file written without it is byte-identical to one written before it
existed, and `computed_schema_hash()` is unchanged. `IndexPolicy` gained a
public field, so struct-literal construction of one needs updating; `new` and
the `with_*` builders do not.

New on disk: internal block id `OPEN_DIGEST_BLOCK_ID` (`0xFFFF_FFF5`), payload
magic `b"VDIG"`. See [Spec](docs/spec.md) for the layout and the acceptance
rules. A reader whose spec does not declare it indexes it as an internal record
exactly as it indexes a segment record; measured, the entry lists are identical.

### `max_file_len` no longer does anything

All twenty-two file-length enforcement sites were removed, and `file_len` came
off both required-declaration lists — a format that omits it now opens. A
ceiling on how large a *file* may be bounded nothing the reader allocates; what
it allocates is bounded by `max_records`, `max_index_bytes`, `max_scan_bytes`,
`max_record_payload_len` and `max_logical_payload_len`, each of which has a check
that consults it.

The field and the DSL's `limits { file_len: .. }` remain so existing format
definitions keep parsing. The value is inert. A format that relied on it to
refuse a large file must state one of the limits above instead.

### Fixed

- `diagnose_file` and `inspect_layout_file` read the record index through
  `index_entries()`, whose infallible surface swallows a refused `IndexBytes`
  charge and returns an empty snapshot. Both reported a refused file as a file
  with **no records** / **no segments**; both now surface the refusal.


### Internal segments: open can stop reading every record

`IndexPolicy::segment_on_flush` makes every commit point append an internal
*segment* record covering exactly the records that commit point added. Its
record footer's `prev_same_block_offset` names the previous segment — a value
known when it is written, so nothing is back-patched and nothing is rewritten in
place. Open confirms a record footer at the end of the file, walks that chain
backwards, and reads **no data record**.

Measured on one Linux host, `rustc 1.95` release build, 4 KiB payloads, a commit
point every 256 records, page cache dropped between the write and the open:

| records | file | open, scanning | open, chained | records the open framed |
| --- | --- | --- | --- | --- |
| 50,000 | 213 MB | 1,272 ms | **60 ms** | 196 |
| 200,000 | 852 MB | 5,867 ms | **370 ms** | 782 |

The framed-record count is the load-bearing figure — it is exactly the number of
segment records. The wall-clock ratio is 16x-29x across the range measured, on a
host whose scan timings vary by about 2x run to run; read it as an order of
magnitude. Not measured on Windows, and not measured past 200,000 records.

`segment_on_flush` **supersedes `checkpoint_on_flush`**, and declaring both is
refused. The chain answers the same question incrementally rather than
serialising the whole index from scratch, has no `(max_record_payload_len - 22)
/ 73` entry ceiling, and is the thing open actually reads — a checkpoint is
validated on the way past and its entries discarded. Refused rather than
silently cleared, so a format that declared the checkpoint finds out.

**A writing session must end with `flush` or `commit`.** Open starts the chain
from the record at the end of the file, so a writer that stops on a data record
leaves nothing to start from and the open scans — correctly, silently, and with
none of the benefit. The same 50,000-record file above opens in 60 ms with a
final flush and 1,272 ms without one.

A segment is varve's internal lookup unit, not something a declaration names, so
it has no `varve_format!` clause. Enable it with
`IndexPolicy::with_segment_on_flush`.

**No on-disk format change and no new file.** The record footer already carried
a chain and a magic; open simply never used either.

**What it does not change:** the resident index. It is still one
`RecordIndexEntry` per record, still about 104 bytes each. What segments remove
is the open-time walk, not the residency.

The option is off by default, and a spec that leaves it off writes byte-identical
files and hashes to the same `computed_schema_hash()` as before. Turning it on
requires `block_offset_chain` (which it enables — the chain is that footer field)
and a `crc32` integrity policy, and it refuses in-place fixed replacement
(`replace_fixed`, `replace_fixed_in_place_exclusive`,
`ReplaceStrategy::FixedCopyOnWrite`), which would restamp a record an already
written segment describes. `replace_block` re-encodes every segment payload
against the new generation and still works.

The chain is derived. A file it cannot account for — written before the option,
appended past the last commit point, truncated, corrupt, or broken mid-chain —
falls back to the full scan and produces the identical index, never an error.
`open_recover` and any open under `IntegrityVerification::AtOpen` always scan.

### H1: a block can be kept out of the resident index

`BlockResidencyDescriptor` on `FormatSpec`, via `with_block_residency`. A block
declared `resident: false` has its records written, sequenced, chained and
recovered exactly as before, and **not** mirrored in memory — at write and at
open. Resident cost stops tracking that block's record count, which is the law
the index work exists to break: 104 bytes per record for the life of the handle.

Off by default, not in the computed schema hash, and byte-identical output when
unused — residency changes no byte of the file, so enabling it must not become a
migration for files that did not change.

It requires `block_offset_chain`, because the footer chain is the only way back
to a non-resident record, and gives up, loudly:

- `blocks::<T>()` returns `Error::BlockNotResident` rather than an empty
  collection, which would be indistinguishable from "nothing was written";
- replacement, which resolves its target by position in the resident index;
- whole-generation rewrite, refused for the format, because both rewrite paths
  rebuild the file by iterating that index and would silently drop what it does
  not hold.

Three pieces of writer state were derived from the resident index and could not
stay that way, each a silent corruption if left: the **next sequence** (a reopen
re-issued numbers already on disk), the **block tails** (a non-resident block's
tail vanished), and **"is there anything to commit"** (a flush after only
non-resident appends skipped the marker, leaving those records permanently
uncommitted and invisible). All three now come from the scan or the append site.

### H2: reading a block through its footer chain

`VarveFile::block_chain(block_id)` walks a block's records newest-first, and
`read_block_at::<T>(record_offset)` turns one into a value. Both take `&self`
and read positionally through the open snapshot, materialising one entry at a
time — the resident index is never consulted and never grown, so the walk costs
the working set rather than the record count. `block_tail_offset(block_id)` is
the entry point on its own.

This is what a non-resident block is read through; without it H1 would have
shipped a capability with no way to use it. `block_chain` refuses a format
without `block_offset_chain`, where a walk that stopped after one record would
look like an answer.

### Fixed

- **`flush` was fatal past a checkpoint ceiling.** A full index checkpoint
  serializes the whole index into one record, so past
  `(max_record_payload_len - 22) / 73` entries — 919,299 on the 64 MiB default —
  no record can hold it, and `write_index_checkpoint` answered `LimitExceeded`.
  `flush` propagated it, so a file stopped being flushable at that record count.
  The ceiling is now part of `needs_index_checkpoint`: an oversized checkpoint is
  skipped, which costs the next open the scan it already falls back to.
- **An index checkpoint could claim coverage it did not describe.**
  `validate_index_checkpoint` compared record offsets with `<`, which accepts a
  *gap*: a one-entry checkpoint could name a covered offset far past the record
  it listed, and every record in between was on disk, absent from the checkpoint,
  and therefore absent from `index_entries()` and from every typed read. Entries
  must now tile their coverage exactly, and the walk must end at the covered
  offset.

## 0.5.0 - 2026-07-25

**This was scoped as a `0.4.1` hotfix and is released as `0.5.0`, because the
version number has to describe what changed rather than how urgently it shipped.**
It removes a public enum variant, changes the value every `ReadLimits` constructor
puts in a public field, adds a second public policy enum, and changes how two
composition functions behave. Under the
policy stated three lines above this section, an incompatible Rust API change
increments the minor version below 1.0. Nothing here is a wire-format change: no
encoder, decoder, header field or version constant is touched, and a 0.4.0 file
reads unchanged.

The release fixes **both** defects it was opened for: `*_with_resource_limits`
no longer discards a declared residency policy, and an undeclared matrix open is
no longer eager. The second one took a design change rather than a constant flip —
residency and verification were one option and are now two — and the section
"Residency and verification are now separate policies" below is the whole of it.

### Breaking changes at a glance

| Change | Migration |
| --- | --- |
| `MatrixMetadataResidency` gains a `Missing` variant and becomes `#[non_exhaustive]` — exhaustive `match`es break | [§A.1](docs/api-changes.md#a1-matrixmetadataresidency-gains-a-missing-state-and-becomes-non_exhaustive) |
| Every `ReadLimits` constructor now puts `Missing` in `matrix_metadata_residency` where it put `EagerVerified` — code comparing that field to a variant now sees `Missing` | [§A.1](docs/api-changes.md#a1-matrixmetadataresidency-gains-a-missing-state-and-becomes-non_exhaustive) |
| **`MatrixMetadataResidency::EagerVerified` is removed**, and `DEFAULT` is now `Lazy { DEFAULT_CACHE_BYTES }`. Code naming the variant stops compiling; an undeclared open retains no commit-map payload and can no longer be refused because its live set exceeds `max_matrix_bitmap_bytes` | [§A.3](docs/api-changes.md#a3-matrixmetadataresidencyeagerverified-is-removed-and-default-is-now-lazy) |
| A reader no longer owns a whole-map commit snapshot as of its open — that was a property of eager residency and has no replacement | [§A.3](docs/api-changes.md#a3-matrixmetadataresidencyeagerverified-is-removed-and-default-is-now-lazy) |
| `*_with_resource_limits` no longer discards a spec-declared residency policy — the behaviour §5.12 of the 0.4.0 migration document told you to work around is **gone**, and the workaround is now a no-op rather than a requirement | [§A.2](docs/api-changes.md#a2-_with_resource_limits-composes-a-declared-residency-policy-instead-of-replacing-it) |
| `*_with_limits` / `tighten` now honours a runtime-declared residency policy where the format declared none (it previously dropped it unconditionally) | [§A.2](docs/api-changes.md#a2-_with_resource_limits-composes-a-declared-residency-policy-instead-of-replacing-it) |
| **At an unchanged signature:** `matrix_resume_signal` and `matrix_sidecar_resume_signal` on a **quarantined** category now return `Err(Error::MatrixCommitQuarantined)` where they returned `Ok(MatrixResumeSignal::Clean)`. The old answer came from the empty replacement map quarantine used to install; nothing is retained now, so there is no map to answer from | [§A.5.1](docs/api-changes.md#a51-matrix_resume_signal--matrix_sidecar_resume_signal-refuse-a-quarantined-category) |

### Residency and verification are now separate policies

**The defect.** An undeclared matrix open was `EagerVerified`: it materialised
every published commit-map page for the whole session, which made open cost scale
with the file rather than the working set and made `max_matrix_bitmap_bytes` an
*admission* limit — a matrix whose live set exceeded it could not be opened at all.
That violates the project's standing policy that open reads a header and pages
fault in on demand.

**Why flipping one constant was not the fix.** The complete
`MatrixRecoveryReport` was a **side effect** of the eager page load. A bare flip
would have silently removed the `Recoverable` `MatrixCorruptionKind::CommitMap`
finding, the whole-category quarantine behind `Error::MatrixCommitQuarantined`, the
`RebuildCommitMap` / `ClearCategory` recommendations, and the
`RecoveryPolicy::Strict` writer gate — and, worse than deferring them, would have
stopped examining damage in pages the persisted index does not name at all, because
the eager visit set was the index *unioned with* the platform's allocation map.
Eight behavioural contracts across `matrix.rs`, `matrix_hardening.rs` and
`matrix_integrity_scaling.rs` depended on it.

**The fix.** Residency (how much stays in memory) and verification (whether pages
are checked) were conflated and are now two options:

- **Residency is always demand-filled and bounded.** `EagerVerified` is removed;
  `MatrixMetadataResidency::DEFAULT` is `Lazy { DEFAULT_CACHE_BYTES }`, clamped to
  whatever `max_matrix_bitmap_bytes` is in force. There is one page-loading path in
  the crate, not two.
- **Verification is `MatrixMetadataVerification`, and it defaults to `AtOpen`.**
  It streams the same candidate set the eager load visited — index unioned with
  allocation map — authenticates each page against its stored digest through one
  reusable 4096-byte buffer, and produces the whole report. It retains nothing
  proportional to the candidate set, so detection at open no longer costs memory.
  `OnDemand` moves the pass off the open; `verify_matrix_metadata()` runs it later.

All eight contracts pass unchanged, because verification still runs by default and
still produces every finding.

**Measured on this host** (`matrix_lazy_residency.rs`, `matrix_integrity_scaling.rs`):

| | before (`EagerVerified`) | after, undeclared (`Lazy` + `AtOpen`) | after, `Lazy` + `OnDemand` |
| --- | --- | --- | --- |
| Open bytes read, 1 live page | 131,240 | 69,800 | **32** |
| Commit-map pages *retained* at open, 1 live page | 1 (4,096 B) | **0** | **0** |
| Open bytes read, 64 live pages | 591,384 | 267,800 | **1,040** |
| Commit-map pages *retained* at open, 64 live pages | 64 (262,144 B) | **0** | **0** |
| Resident after touching 4 distinct live pages | unchanged | 16,384 | 16,384 |
| Peak buffer held by verification | n/a (whole live set retained) | **4,096 B** | **4,096 B** |
| A live set over `max_matrix_bitmap_bytes` | **refused** | opens | opens |

### Changed behaviour at an unchanged signature

The dangerous class: code that still compiles and now behaves differently. All of
it follows from quarantine no longer retaining the damaged map — it used to install
an empty replacement bitmap over it, and some answers came from that replacement.

- **`matrix_resume_signal` / `matrix_sidecar_resume_signal` refuse a quarantined
  category** with `Error::MatrixCommitQuarantined` where they answered
  `Ok(MatrixResumeSignal::Clean)`. Reporting "nothing in progress" for a category
  whose commit map is known damaged was an artifact of the replacement map, not a
  verdict. An unaffected category is unchanged. All three receivers
  (`VarveReader`, `VarveWriter`, `VarveFile`).
- **`clear_matrix_category` reports `Ok(0)` cleared on a quarantined category**,
  and now says so in the code rather than emerging from the replacement map. It
  still does not refuse — it is the recovery path the report recommends — but it
  cannot count bits it is discarding, because every count authenticates what it
  reads.
- **An undeclared matrix open** retains no commit-map payload and can no longer be
  refused because a live set is wider than `max_matrix_bitmap_bytes`. See
  "Residency and verification are now separate policies" above.

Full before/after snippets in
[§A.5](docs/api-changes.md#a5-changed-behaviour-at-an-unchanged-signature-in-050).

### Fixed

- **`*_with_resource_limits` silently discarded a declared
  `matrix_metadata_residency`.** `MatrixMetadataResidency` had no unset state, so
  `ReadLimits::STANDARD` carried a *real* eager declaration and
  `ReadLimits::overlay` — which takes the runtime value outright — could not tell
  "the caller wants eager" from "the caller never mentioned residency". Raising an
  unrelated ceiling therefore reverted a format's declared
  `Lazy { cache_bytes }` to the eager policy, silently discarding the only bound on
  matrix metadata memory, and could then fail the open on the very admission limit
  the caller was raising:

  ```text
  Error::LimitExceeded { resource: "matrix bitmap bytes", actual: 33536, limit: 32768 }
  ```

  That error is the measured pre-fix result of an
  `open_reader_with_resource_limits` call whose only content was a *larger*
  bitmap ceiling. `matrix_metadata_residency` now has a `Missing` state and
  composes like every ceiling above it: a runtime `ReadLimits` that never called
  `with_matrix_metadata_residency` leaves a format-declared policy alone, one
  that did wins. `matrix_metadata_verification` composes identically.

- **`tighten` had the same bug class in the other direction** and is fixed with
  it. A format's declaration still wins, but a runtime declaration is now taken
  where the format made none, instead of being dropped unconditionally.

- **An undeclared matrix open was eager.** See "Residency and verification are now
  separate policies" above.

- **A whole-map aggregate could be counted from unauthenticated bytes.**
  `matrix_resume_signal` and the recovery report's partial-progress advisory read
  the pages a demand cache does not hold, and did so without checking their
  digests. They now use the same authentication every other read uses, so a
  damaged page refuses instead of contributing a number.

### Added

- `MatrixMetadataVerification` — `Missing` | `AtOpen` | `OnDemand`,
  `#[non_exhaustive]`, with `DEFAULT = AtOpen`. The `ReadLimits` field
  `matrix_metadata_verification`, the `const fn with_matrix_metadata_verification`
  builder, and `effective_matrix_metadata_verification()` follow the residency
  policy's idiom exactly: one unset state, one resolution point, one admission
  point.
- `VarveReader::verify_matrix_metadata()`, and the same method on `VarveWriter`
  and `VarveFile` — runs the verification pass on demand and returns a
  `MatrixRecoveryReport`. It reports; it does not arm the quarantine, because the
  fail-closed gate is derived once at open from the findings the layout is
  assembled with.
- `MatrixMetadataResidency::Missing` — the unset state, following the
  `ReadLimit::Missing` idiom exactly. It is what `all()`, `STANDARD`,
  `UNTRUSTED`, `MISSING`, `TRUSTED_UNBOUNDED`, `finite_all()`, `default()` and
  `FormatSpec::new` now carry, and `ReadLimits::resolve` is idempotent over it.
- `MatrixMetadataResidency::DEFAULT` — the single place the default policy lives
  and the single line that changes it. It is `Lazy { DEFAULT_CACHE_BYTES }`.
- `MatrixMetadataResidency::DEFAULT_CACHE_BYTES` — 2 MiB, *derived* rather than
  chosen: `ReadLimits::STANDARD.max_matrix_bitmap_bytes / 32`, computed in a
  `const` expression. Two `const` assertions fail the build if `DEFAULT` stops
  naming a real policy, or if the cache stops covering the whole commit map of
  the largest matrix `STANDARD` will admit (16,000,000 cells = 1,953,125 bytes =
  477 pages, against 512 cached pages).
- `ReadLimits::effective_matrix_metadata_residency()` — resolves `Missing` to
  `DEFAULT`. **Read this instead of the `matrix_metadata_residency` field**; it
  never yields `Missing`. This is the only place the default is resolved.
- `ReadLimits::default_matrix_metadata_cache_bytes()` — `DEFAULT_CACHE_BYTES`
  clamped down to `max_matrix_bitmap_bytes`. A cache the caller *declared* above
  the ceiling is still refused, because that contradiction is theirs to resolve;
  a cache varve *derived* is clamped instead, because tightening a memory ceiling
  must not mean "no matrix opens".

### Removed

- `MatrixMetadataResidency::EagerVerified`. Nothing replaced it as a residency
  mode and no alias was left behind: what callers wanted from it is
  `MatrixMetadataVerification::AtOpen`, which is the default, and a name promising
  eager session-long residency for a bounded demand cache would be worse than a
  compile error. See [§A.3](docs/api-changes.md#a3-matrixmetadataresidencyeagerverified-is-removed-and-default-is-now-lazy)
  for the three things it was used for and what to do about each.

### Still not `O(1)`

An open under the default still reads `O(live pages + allocated pages)` bytes,
because it verifies: 69,800 bytes for a matrix with one live page on NTFS, where
the allocation map reports the bitmap region in ~128 KiB runs. That is now a
declared cost with an off switch (`OnDemand`, 32 bytes) rather than a property of
how much is kept resident. A non-verifying open still reads the persisted page
index in full — 8 bytes per live page — and holds ~96 bytes per live page of index
mirror, which is what makes "this page was never published" answerable without
I/O. Both are documented with numbers in
[Known Limitations §1.1](docs/known-limitations.md#11-open-cost-is-proportional-to-candidate-pages-not-to-the-working-set)
and [§1.2](docs/known-limitations.md#12-what-residency-still-costs-the-page-index-mirror-and-a-per-bitmap-cache-bound).

## 0.4.0 - 2026-07-22

Pre-1.0 minor release. Versions 0.1 through 0.3 existed as source releases only;
no Varve crate has previously been published to crates.io, so a consumer
installing from the registry starts here. The Rust
API and several persisted layouts change incompatibly from 0.3.0. Every affected
persisted artifact is stale-regenerable and is refused with a typed error rather
than migrated in place — nothing is silently misread.

Two documents accompany this release and should be read before upgrading:

- **[docs/api-changes.md](docs/api-changes.md)** — the migration document: new,
  changed, removed, and changed-behaviour-at-an-unchanged-signature, with
  before/after snippets and the exact typed error for every stale artifact class.
- **[docs/known-limitations.md](docs/known-limitations.md)** — what this release
  is and is not ready for, in terms of what a user hits, with measured numbers.

### Breaking changes at a glance

Each links to the section of the migration document that tells you what to edit.

| Change | Migration |
| --- | --- |
| **Matrix files, matrix sidecars, disk-index sidecars, and files pinned with a computed schema hash must be regenerated.** Plain append-log files remain readable | [Read this first](docs/api-changes.md#read-this-first-files-written-by-an-older-version) |
| The computed schema hash algorithm is now **version 3** (v1 → v2 → v3); every computed value changed twice | [§1.1](docs/api-changes.md#11-the-computed-schema-hash-changed-twice) |
| `VarveBlock` gains required `IS_KEYED` and `SCHEMA_FINGERPRINT` — every **manual** `impl VarveBlock` stops compiling | [§3.1](docs/api-changes.md#31-varveblock-gains-two-required-associated-constants--every-manual-impl-stops-compiling) |
| `FormatSpec` gains `block_identities` and `matrix_fatal_forensics` — exhaustive struct literals break | [§3.2](docs/api-changes.md#32-formatspec-gains-two-public-fields--exhaustive-struct-literals-break) |
| `Error::PublishedButRebindFailed` gains `parent_sync` — destructuring breaks | [§3.3](docs/api-changes.md#33-errorpublishedbutrebindfailed-gains-a-third-field) |
| `KeyedMergeEstimate::peak_resident_structural_bytes()` — added and renamed inside this cycle (no released version exposed `peak_resident_bytes()`); it is a structural estimate, not an upper bound | [§2.1](docs/api-changes.md#21-keyedmergeestimatepeak_resident_bytes--peak_resident_structural_bytes) |
| `VarveWriter::reserve_keyed_tail_slot` and `DiskIndexError::BatchPoisoned` removed — **neither ever shipped**; both were added and removed inside this cycle, so neither is a migration step | [§2.2](docs/api-changes.md#22-varvewriterreserve_keyed_tail_slot--never-shipped), [§2.3](docs/api-changes.md#23-diskindexerrorbatchpoisoned--never-shipped-and-never-reachable) |
| Generic `push` now refuses keyed blocks on `keyed_offset_chain` formats with `KeyedChainRequiresKeyedApi` | [§5.2](docs/api-changes.md#52-generic-push-refuses-keyed-blocks-on-keyed_offset_chain-formats) |
| Replacement refuses a cross-version target with `BlockVersionMismatch` | [§5.1](docs/api-changes.md#51-replacement-refuses-a-cross-version-target) |
| Five new post-publication durability outcomes replace a bare `Err`; the work is already in the file, so do not retry the transaction | [§5.3](docs/api-changes.md#53-new-durability-outcomes-where-a-bare-err-used-to-be-returned) |
| Matrix access is fail-closed on a `Fatal` recovery finding | [§5.4](docs/api-changes.md#54-matrix-access-is-fail-closed-on-a-fatal-recovery-finding) |
| `PackedBitmap` is no longer accepted as a fixed-width matrix field | [§3.6](docs/api-changes.md#36-packedbitmap-is-no-longer-accepted-as-a-fixed-width-matrix-field) |
| Tighter limit charges (`HashMap` materialization, keyed-tail build peak) can refuse work that previously succeeded | [§5.6](docs/api-changes.md#56-tighter-and-more-accurate-limit-charges) |
| `FormatSelfTest::run` is non-destructive; `cleanup(true)` can report a failed step where it reported a clean pass | [§5.8](docs/api-changes.md#58-formatselftestrun-is-non-destructive) |
| `scripts/run-security-fuzz.ps1` can exit 2 or 3 where it exited 0 | [§5.10](docs/api-changes.md#510-scriptsrun-security-fuzzps1-exit-codes) |
| `delete` now maintains the keyed offset chain where it previously truncated it — same signature, different on-disk chain shape | [§5.11](docs/api-changes.md#511-delete-now-maintains-the-keyed-offset-chain-instead-of-truncating-it) |
| `*_with_resource_limits` **replaces** a spec-declared `matrix_metadata_residency` rather than composing it, silently. **Fixed in 0.5.0 — do not write the workaround this row describes** | [§5.12](docs/api-changes.md#512-_with_resource_limits-replaces-a-spec-declared-matrix-residency-policy) |

### Added (rounds 15-16)

- **`MatrixMetadataResidency`** (`format::MatrixMetadataResidency`, exported from
  the facade), the `ReadLimits::matrix_metadata_residency` public field, and the
  `const fn ReadLimits::with_matrix_metadata_residency` builder. It declares how
  much of a matrix's persisted commit metadata an open makes resident, and
  therefore when that metadata's integrity is checked.

  `EagerVerified` is the default and is **inert** — byte-for-byte the behaviour
  varve had before the option existed. (**0.5.0 removed this variant and made
  `Lazy` the default; everything this entry says about `EagerVerified` describes
  0.4.0 only.** The verification half of it survives as
  `MatrixMetadataVerification::AtOpen`, which is the default and retains nothing.
  Read the policy through `ReadLimits::effective_matrix_metadata_residency()`.)
  Open **reads and authenticates** every
  commit-map page the matrix has published plus every page the platform's
  allocation map reports as written, and makes resident only those holding a set
  bit; corruption anywhere in the read set is reported by `open`. Under this policy
  `max_matrix_bitmap_bytes` is an **admission** limit, not a cache bound: a matrix
  whose live page set exceeds it cannot be opened at all, and there is **no
  read-driven eviction** — residency falls only when a mutation clears a page's
  last set bit (PERF-02).

  `Lazy { cache_bytes }` reads the persisted page index at open and nothing else —
  no page payload, no page digest, no allocation-map query. A page is read,
  authenticated and cached the first time a bit inside it is addressed, and the
  least recently used cached page is dropped when admitting another would exceed
  `cache_bytes`. A live set larger than `max_matrix_bitmap_bytes` becomes
  *openable* rather than refused; a `cache_bytes` above that ceiling is refused at
  open, so the option cannot raise a declared limit.

  Three consequences are part of the declaration, not accidents: corruption
  detection moves from `open` to first touch and untouched pages are never
  checked; a page absent from the persisted index still reads as clear (the index
  is loaded in full under both policies, so "not cached" and "not published" stay
  distinct); and a page's contents are as of the first touch that faulted it in,
  so a lazy reader does not see one consistent instant.

  Measured on the fixtures in `crates/varve/tests/matrix_lazy_residency.rs`
  (Windows x86_64, 2026-07-22): for a 69.2 MB / 8,388,608-cell matrix with one
  live page, eager open reads 131,240 bytes over 32 pages and leaves 8,192
  resident; lazy open reads 32 bytes, visits 0 pages and leaves 0 resident. At 64
  live pages, eager reads 591,384 bytes and leaves 524,288 resident; lazy reads
  1,040 bytes. **Open is independent of file size under both policies but is not
  `O(1)` under either** — see
  [docs/known-limitations.md §1](docs/known-limitations.md#1-matrix-opening-a-matrix-is-not-o1-because-opening-it-verifies-it).

- `MatrixRecoveryReport` counters, gated on `scalable-fault-injection`:
  `matrix_lazy_cached_bitmap_bytes`, `matrix_lazy_fault_bytes_read`, and
  `reset_matrix_lazy_counters`.

- Two new integration suites: `crates/varve/tests/matrix_lazy_residency.rs` (10
  tests, every assertion an absolute number rather than a ratio between two runs)
  and `crates/varve/tests/matrix_concurrent_reads.rs` (4 tests, including a
  compile-time assertion that the reader handle is `Send + Sync`).

- Two `tests/ui` compile-fail fixtures, `fail_minted_crc_valid_completeness` and
  `fail_minted_fatal_access_gate`, making the disk-before-memory and guard-bypass
  shapes compile-time errors rather than review findings.

### Changed (rounds 15-16)

- **Matrix reads take `&self` instead of `&mut self`.** `read_matrix_cell::<T>`,
  `matrix_cell_payload::<T>` and `read_matrix_aux` relax their receiver on
  `VarveReader`, `VarveWriter` and `VarveFile`. The other three read entry points
  — `matrix_cell_status::<T>`, `matrix_aux_len` and `matrix_resume_signal` — were
  already `&self` in 0.3.0 and are unchanged. So three methods changed, and the
  result is that the complete six-entry-point matrix read surface takes a shared
  borrow on all three handle types (18 signatures). This is source-compatible —
  an existing call through a `&mut` binding still compiles — and what it enables
  is one handle serving concurrent readers. Full table in
  [docs/api-changes.md §3.4](docs/api-changes.md#34-matrix-reads-relax-from-mut-self-to-self--source-compatible).

  Reads go through positional I/O (`pread` on Unix, `seek_read` on Windows). On
  **Windows** each reading thread additionally gets a private file object derived
  with `ReOpenFile` (`MatrixReadPool`), because `ReadFile` serialises on the
  kernel file object: without it, four threads measured **1.54x slower** than one.
  With it, 24,000 reads through one handle measured 0.138s on 1 thread and 0.046s
  on 4 (**0.33x**), and 40,960 lazy fault-in reads measured 3.814s versus 1.667s
  (**0.44x**); both are contract-asserted at `<= 1.0x`. The private-handle pool is
  `#[cfg(windows)]`, and **the concurrent-read scaling contracts have never been
  executed on Unix** — see
  [docs/known-limitations.md §6.1](docs/known-limitations.md#61-the-unix-code-paths-and-what-the-first-linux-run-found).

- One lock now exists in the matrix read path: `SparseBitmap` holds a
  `Mutex<PageStore>` so a demand fault-in can happen under `&self`. It is taken
  only for `O(1)` map operations, is released across the fault-in read, and is
  never taken on the write path. Under `EagerVerified` nothing is ever faulted in.
  (**0.5.0**: that variant is gone, so every persisted map is demand-filled and
  this lock is on every category read.)

- `PoisonFlag::healthy()` is no longer a `const fn`, so the `static DECOY`
  spelling of the guard bypass no longer compiles (E0015).

### Documentation (rounds 15-16)

- New: `docs/known-limitations.md` and `docs/api-changes.md`.
- `docs/api-reference.md`, `docs/scalable-io.md`, `docs/durability-model.md`,
  `README.md` and the internal performance notes corrected against the measured
  behaviour of this tree.
- The changelog's earlier claim that the computed schema hash algorithm is
  version 2 is corrected: the shipping value is **3**
  (`FormatSpec::SCHEMA_HASH_ALGORITHM_VERSION`), and both bumps are described in
  [docs/api-changes.md §1.1](docs/api-changes.md#11-the-computed-schema-hash-changed-twice).

### Assurance limits of this release

Stated here rather than left to be discovered:

- **The numbers in these documents are Windows numbers.** The suite is executed
  on both operating systems — CI runs `ubuntu-latest` and `windows-latest`, and
  the Unix `openat`/`unlinkat` and `O_NOFOLLOW` paths run there rather than being
  compile-verified — but no measurement has been taken on Linux, and a
  filesystem with different extent behaviour moves the constants. The two gates
  that were red when this release was assembled, the Linux `dead_code` lint on
  the Windows-only `MatrixReadPool::handles` field and the default-feature
  failure in `matrix_concurrent_reads`, are both fixed and green.

  This entry originally read "CI has never run on this code", which was true
  when 0.5.0 was assembled and stopped being true on 2026-07-27. The first Linux
  run found three defects every Windows run had passed; they are described in
  [docs/known-limitations.md §6.1](docs/known-limitations.md#61-the-unix-code-paths-and-what-the-first-linux-run-found).
- **No fuzz campaign, Miri run or ASan run has been performed against this
  release's code.** The recorded fuzz evidence predates rounds 12-16, and an
  unpromoted libFuzzer OOM reproducer for `codec_arbitrary` exists in the working
  tree. This release makes no fuzz-pass claim.
- The 1 PiB and 1 TiB positional-I/O probes are `#[ignore]`d and have never run.
  The performance smoke suite and the one-million-key stress probe run in no job.
- The `high-cardinality-dev` scalable module set has not been walked against the
  project's internal invariants.

Full detail in
[docs/known-limitations.md §6](docs/known-limitations.md#6-not-verified).

### Rounds 12-14

Round-12 adversarial review (`f661f65`, 2026-07-21). Five
correctness blockers, two medium post-publication result issues, one low
diagnostic issue, one release-assurance gap, and one documentation-routing
improvement. No wire-format change: `VMAT` stays at layout version 4, the
page-index encoding is byte-identical, and no fixture needed regenerating.

The theme is not the individual fixes. Rounds 5, 7 and 12 each found the *same*
shape - a persisted structure mutated on disk before its in-memory mirror was
updated by a step that can fail - and round 10's exhaustive enumeration of that
class walked straight past the instance round 12 reported, in the file it had
just rewritten. Reading does not close the class, so this round made the shape
impossible to express instead: each recurring shape now maps to a type that
forbids it, and those types are listed under Added below.

### Breaking (rounds 12-14)

- **`Error::PublishedButRebindFailed` gains a third field**,
  `parent_sync: Option<Box<Error>>` (F-07). A replacement publication learns two
  independent durability facts in a fixed order - whether the parent-directory
  sync succeeded, and whether the writer could rebind to the published
  generation - and only the second one ended the call, so a double fault
  returned the rebind variant alone and silently discarded the sync failure.
  `Some(_)` now means: the new generation is visible at the pathname, the writer
  is unusable, **and** the rename is not yet proved durable against power loss.
  `None` means only the first two. `Display` gains a trailing clause when the
  field is present. `Error` is `#[non_exhaustive]` at the enum level but this
  variant is not, so external code destructuring it with `{ sequence, source }`
  must add `parent_sync` or `..`.
- **`replace_rewrite` and `unsafe replace_fixed_in_place_exclusive` now refuse a
  target whose stored `block_version` differs from `T::VERSION`** with
  `Error::BlockVersionMismatch` (F-01). They selected their target by block id
  alone and then wrote `T`'s payload under the stored record's older version
  header - a persistent type/version disagreement on disk, reachable whenever
  `schema_hash = 0` opts out of header-level schema locking. `replace_fixed` and
  `replace_block` already made this refusal, but later in their sequence: the
  check now precedes the size, limit and generation checks, so a call that is
  wrong in two ways reports `BlockVersionMismatch` where it previously reported
  `ReplaceSizeMismatch`. The check is no longer a step any path performs - it is
  a precondition of resolving the target, enforced by a private-field
  `ReplacementTarget` whose sole constructor performs it, so a fifth replacement
  path cannot be written without it.
- **`VarveWriter::rebuild_matrix_commit_from_crc` returns
  `Err(Error::MatrixFatalCorruption)` instead of `Ok(count)` when the block's
  CRC-validity page index could not be enumerated in full** (F-04), *including*
  under `FormatSpec::with_matrix_fatal_forensics`. A rebuild sets a bit only
  where slot-valid evidence says the CRC is meaningful, so pages missing from a
  damaged validity index were published as uncommitted - erasing the previous
  commit view using evidence that was never read. Forensic mode relaxes
  *reading* fatal state, not a destructive republication. Repair or regenerate
  the validity evidence first; there is deliberately no implicit way to discard
  unverifiable visibility.
- **The indexed writer no longer keeps its own poison flag** (F-05). It reads
  and sets the stream writer's, so an indexed writer now refuses after *any*
  stream poison, including one raised by work it did not itself perform. The
  defect this closes: after a native append failed and its truncate-or-seek
  rollback also failed, the stream poisoned itself while the indexed flag stayed
  clear, so a later single-record put or delete passed the indexed guard and
  called the stream's internal prepared append directly - a method that did not
  consult the stream's flag either. The published contract said that mutation
  was refused with `WriterPoisoned`; it was performed. The refusal still reports
  `WriterPoisoned("indexed")` when reached through the indexed API.
- **The unsafe contract of `replace_fixed_in_place_exclusive` is tightened**
  (F-02): for a keyed block on a `keyed_offset_chain` format the caller must not
  change the record's key. Records appended after the target already carry
  `prev_same_key_offset` pointers into it, and this path publishes no new
  generation in which the chain could be rebuilt. Behaviour for conforming
  callers is unchanged; this is a documentation-level tightening of an existing
  `unsafe` API.
- **`scripts/run-security-fuzz.ps1` now enforces the artifact gate** and can
  exit non-zero where it previously exited 0 (F-09): `2` when
  `fuzz/artifacts` already holds a reproducer before the run starts, `3` when
  any file is present after a target. It never deletes anything.
- **A self-test with `cleanup(true)` can now report a failed step where it
  previously reported a clean pass** (F-08): failure to re-acquire or to
  identify the run's own `.lock` marker is reported as a `cleanup` step failure
  in the `Environment` domain instead of returning silently.
- **`DiskIndexError::BatchPoisoned` is removed** (round 12 enforcement pass).
  `DiskIndexWriteBatch` carried a `poisoned` flag that no code path ever set,
  so its guard could not fire and the variant was unreachable. The batch needs
  no flag: every staging entry point validates in full before performing only
  infallible in-memory inserts, and `commit` consumes the batch, so a
  half-staged or twice-committed batch is not representable. `DiskIndexError`
  is `#[non_exhaustive]`, so only code that named this variant is affected.

### Fixed (invariant re-verification, round 12)

- Matrix page-index mutation is prepare-then-commit behind a private module
  (F-03, invariant 3). `compact_page_index` wrote its sorted entries to disk and
  only then performed a fallible `try_reserve` for the in-memory mirror; on
  `AllocationFailed` the writer was **not** poisoned - matrix mutation
  deliberately poisons only on `Error::Io` - so the session continued with a
  slot map that no longer described the array, and a later
  `release_page_index_entry` could overwrite a live page's only entry and shorten
  the array so that page was no longer in the counted prefix. All four
  page-index mutation sites now route through prepared values whose construction
  performs every fallible step - budget charge, mirror reservations, offset and
  count arithmetic, and the encoding of every byte - while the commit writes
  already-encoded bytes and installs the mirror into reserved capacity,
  infallibly. The functions that write page-index bytes are private to that
  module, so nothing else in `matrix.rs` can call them. `compact_page_index` no
  longer exists; compaction is a phase of the append, which also made it one
  sequential write instead of one per entry and removed an intermediate header
  publication. Residual property, relied on by `finish_matrix_mutation`: after a
  prepared page-index mutation begins writing, the only error it can return is
  `Error::Io`.
- Stream poison is enforced by a witness token rather than by a guard callers
  must remember to call (F-05). `MutationPermit` has a private field and no
  constructor other than the poison check, and every mutating entry point takes
  one - by value where another module can reach it, so each such mutation gets
  its own fresh check rather than one amortised over a loop. Batch publication
  re-checks per chunk, because a chunk earlier in the same batch can poison the
  writer.
- The witness now names the writer it speaks for, and can only be minted from
  that writer's own flag. The first cut let any holder of a `PoisonFlag` mint a
  permit, so a throwaway `PoisonFlag::healthy()` could produce one and drive a
  genuinely poisoned writer through its own guard - the token proved that *a*
  flag had been checked, not that *this writer's* had. The constructor is now
  private to `writer_permit`, reachable only through `GuardedWriter::writer_permit`
  (which reads `&self`'s flag), and `MutationPermit` is generic in the writer
  type, so a permit taken for one writer is not the same type as a permit for
  another. Internal only; no public API or wire format changes.
- Exclusive in-place replacement invalidates the affected block's resident
  keyed-tail map **before** the first byte reaches disk (F-02). The seek/write
  pair moved into a single `overwrite_record_bytes_in_place`, the only in-place
  rewrite of an already-indexed record in the crate. The invalidation is
  infallible and allocation-free, so it adds no fallible step in either
  direction, and running it first means the cache cannot survive a half-completed
  write, a poisoned writer, or a caller that violates the contract above.
- The CRC rebuild no longer clones the previous commit bitmap through infallible
  `Clone` (F-10). The clone is *removed* rather than made fallible: the rebuild
  only reads the previous map's indexed-page keys and page count, so the map is
  borrowed. `SparseBitmap: Clone` remains because `MatrixLayout: Clone` requires
  it.
- Post-publication allocator behaviour is stated instead of implied (F-06). Both
  matrix post-commit outcome variants box their event and their source, so
  constructing either allocates twice after the cell is authoritative; a
  subprocess harness failed the very next allocation at each branch and both
  children terminated on a 56-byte allocation while the reopened file contained
  the committed values. The crate now publishes one policy: content-sized
  allocations are charged to a `ReadLimits` ceiling and reserved fallibly into
  `AllocationFailed`/`LimitExceeded`, while shape-sized allocations bounded by
  the program's own compile-time shape use ordinary infallible allocation and
  abort on refusal, as `Box::new` always does. The consequence for every
  published-outcome contract, now written down: an allocator refusal after
  publication terminates the process rather than substituting a different
  outcome, so no caller observes a *wrong* outcome, and the on-disk state is the
  published one. Pre-staging the boxes was rejected - it cannot remove
  `Box::new(source)`, and it would put two allocations on the success path of
  every durable cell write to serve a path that ends in `abort`. See "Allocator
  Failure And Published Outcomes" in `docs/durability-model.md`.

### Added (round 12)

- `MatrixRecoveryReport::inject_matrix_page_index_mirror_reservation_failure`,
  compiled only under `feature = "scalable-fault-injection"`, alongside the
  existing matrix injectors. The crate-private write-fault injector also gained
  a combined parent-sync-then-rebind fault, present on **both** the unix and
  windows `sync_parent_directory` branches so the F-07 double-fault regression
  runs on either platform.
- The contributor invariant checklist (an internal working document, not part of
  the published `docs/` set) gained "Mechanically enforced shapes", the map from
  each recurring defect shape to the type that now forbids it, and a standing
  instruction to prefer that section to the site enumeration. The
  enforcement pass added "What is mechanically enforced, and what is not",
  which separates the properties the compiler refuses from the ones a source
  gate checks from the ones that still rely on review.
- `crates/varve-core/src/writer_permit.rs`: the crate's single implementation of
  the shape-B witness token, promoted out of `stream.rs` so `file.rs` and
  `layout.rs` hold the same type instead of three independent conventions. It
  also models `layout.rs`'s *reversible* in-flight window (`MutationInFlight`),
  which a one-way poison cannot express, without letting anyone assign the
  boolean.
- `ReservedIndexSlot` (`crates/varve-core/src/file.rs`): the shape-A token for
  the resident record index, the mirror on the append path. The limit charge and
  the `try_reserve` produce it; the post-append install consumes it and returns
  `()`. Zero-sized, no hot-path cost.
- `varve_core::enforcement_probe`, `#[doc(hidden)]` and gated on
  `scalable-fault-injection`: re-exports (not copies) of the enforcement types
  so that compile-fail fixtures in another crate can prove they bind.
- Three `trybuild` compile-fail fixtures
  (`crates/varve/tests/ui/fail_fabricated_mutation_permit.rs`,
  `fail_unpermitted_mutation_window.rs`, `fail_fabricated_index_reservation.rs`)
  driven by `compile.rs::mechanical_enforcement_contracts`, and three
  source-level gates in `crates/varve/tests/enforcement_gates.rs`: the matrix
  page-index writers stay unexported inside their module, no module declares its
  own poison boolean, and the resident record index grows only through its
  reservation.

### Documentation (round 12)

- `docs/durability-model.md`: the crate-wide allocator-failure policy; the
  combined parent-sync/rebind outcome; the keyed same-key requirement on
  exclusive in-place replacement and the cache invalidation that holds
  regardless.
- `docs/recovery-model.md`: the CRC rebuild's refusal on incomplete validity
  evidence, and that forensic mode does not relax it.
- The internal matrix storage design notes: the prepared page-index mutation gate,
  its one deliberate exception, and the compaction ordering that replaced the
  separate function.
- `docs/api-reference.md`: the version refusal on every replacement path, the
  three-field rebind variant, the single indexed/stream poison flag, the rebuild
  refusal, the narrowed post-commit outcome family, and the resident-writer cost
  model with its routing to the disk-indexed APIs.
- `docs/self-check-guide.md`: cleanup failures are reported rather than silent,
  and the marker is preserved when it cannot be proved to be ours.
- The internal fuzzing and fault-injection record: exactly what the artifact gate
  checks, when, and with which exit codes.
- Generated typed writers now carry rustdoc stating the `Theta(M*N)` priming
  cost, the per-block-id retention bound, and the `key_index = disk` route a
  high-cardinality format should take instead (P-01). Documentation only; no
  generated logic changed.

### Fixed (round 13, re-verification of round 12's F-01 and F-02)

Re-verification judged both fixes partial. The refusals themselves held under an
independent harness — every replacement entry point, at every ordinal, refused a
cross-version target and left the file byte-identical; the keyed-tail cache was
truthful in-session and after reopen — but the *enforcement* did not bind. Two
probes were compiled against round 12's tree: a new replacement path that
resolved its own ordinal over `self.index` and wrote through the in-place
writer (reproducing F-01 end to end: a v2 payload under a retained v1 header,
silently decoded by a v1 reader), and a new raw `seek`/`write_all` pair that
skipped the writer and left the keyed-tail cache resident (F-02).

- **The primary file handle is no longer a `File`.** `VarveFile::file` is a
  `RecordFile`, declared in a private `mod record_file` that keeps the `File`
  unnameable, implements neither `Write` nor `Seek`, and lends out no `&File`
  (`&File` implements `Write`, so lending one is equivalent to lending a mutable
  handle). Exactly two operations put record bytes on disk: `append_record_at_end`,
  which seeks to the end itself and refuses any offset other than the one the
  caller budgeted, so it cannot land inside an indexed record; and
  `overwrite_indexed_record`, which consumes a `RecordOverwrite`. The second
  probe no longer compiles (E0599: no `seek`, no `write_all`).
- **The keyed-tail invalidation moved into the permission.**
  `RecordOverwrite::prepare` consumes a version-checked `ReplacementTarget`,
  drops the target block's resident keyed-tail map, and yields the only value
  `overwrite_indexed_record` accepts. F-01's refusal and F-02's invalidation are
  now preconditions of addressing the bytes rather than steps inside a function
  a sibling path can bypass. The first probe no longer compiles either (E0451:
  `ReplacementTarget`'s field is private even to the rest of `file.rs`).
- **Two false published claims corrected** (invariant 4): `file.rs`'s statement
  that a later path "cannot reach `self.index[..]` for a caller-chosen ordinal",
  and the invariant checklist's "Enforced by the compiler" row "a
  replacement target cannot be addressed without the version check". Reading the
  index was never restricted and is not now; what is enforced is that an ordinal
  cannot be *written through*. The checklist rows now say which round's claim was
  false and what replaced it.
- **Proofs added**: `crates/varve/tests/ui/fail_fabricated_replacement_target.rs`
  and `fail_fabricated_record_overwrite.rs` (compile-fail, driven by
  `compile.rs::mechanical_enforcement_contracts`, against the real types via
  `varve_core::enforcement_probe`); three source gates in
  `crates/varve/tests/enforcement_gates.rs` covering the wrapped handle, the one
  named escape hatch (`matrix_region`, for `crate::matrix` and the public
  `MatrixDurabilityBarrier`), and the single constructors; and
  `replace_publication_state.rs::every_replacement_entry_point_refuses_a_cross_version_target_at_every_ordinal`,
  which is the re-verification harness itself — all six entry points including
  both `replace` strategy wrappers, both ordinals of a two-record file, the file
  asserted byte-identical after each refusal, and an out-of-range ordinal still
  failing as `UnexpectedEof` so the version refusal cannot mask resolution.

No wire-format change, no public API change, no error-variant change.

### Fixed (round 14, re-verification of round 12's F-04)

- **A CRC-validity bit can no longer be read as evidence of absence without the
  completeness proof.** Round 12 fixed F-04 — a commit-map rebuild that read
  cells it never loaded as "CRC not meaningful" and republished them as
  uncommitted — by tracking completeness in a `crc_valid_complete: bool` beside
  a plain `SparseBitmap` and calling one checking function before the rebuild.
  The behaviour was correct and re-verification confirmed it, but the check bound
  nothing: a new consumer that wrote `block.crc_valid_bits.get(ordinal)?`
  compiled cleanly, which is exactly the "the next function will simply fail to
  repeat it" shape this series exists to eliminate, so the fix was judged
  partial.

  The capability is now gone rather than guarded. The bitmap and its
  completeness are a single `CrcValidEvidence` inside the private
  `mod crc_valid_evidence` (`crates/varve-core/src/matrix.rs`), and outside that
  module **no operation on it returns a validity bit** except
  `CompleteCrcValidEvidence::get`, whose witness has a private field and exactly
  one constructor — `CrcValidEvidence::complete`, which *is* the
  `Error::MatrixFatalCorruption` refusal, still unconditional and still not
  relaxed by `with_matrix_fatal_forensics`. The one reader permitted on
  incomplete evidence, `require_meaningful` (used by `verify_cell_crc`), returns
  `Result<()>`: it hands back no bit, so an absent one can become a typed
  checksum refusal but never published state.

  No behaviour change for any caller: the same rebuilds are refused, with the
  same error, and the fail-closed read still reports
  `Error::MatrixChecksumMismatch`. Proofs:
  `crates/varve/tests/ui/fail_fabricated_crc_valid_completeness.rs` (the witness
  cannot be forged), the source gate
  `enforcement_gates.rs::the_crc_validity_bitmap_is_only_read_through_its_evidence_type`
  (the field keeps the evidence type and the module's two named escapes keep one
  call site each), four unit tests in
  `matrix.rs::crc_valid_evidence_tests`, and three added assertions in
  `matrix_integrity_scaling.rs::crc_rebuild_refuses_an_incompletely_loaded_validity_index`:
  a retried rebuild is refused again, a cell write does not launder incomplete
  evidence into a licence to republish, and a cell whose validity page was never
  loaded still fails closed on read. Negative controls: the round-12 bypass
  consumer no longer compiles (`E0599`, no method `get`), deleting the refusal
  makes the rebuild publish `Ok(1)` for a two-cell fixture, and stubbing the
  fail-closed reader makes the unreadable cell read as valid data.

### Fixed (round 14, re-verification of round 12's enforcement pass)

- **The record append can no longer outrun the mirror reservation, and the
  resident record index can no longer be grown around its token.** Round 12
  added `ReservedIndexSlot` and recorded that "the fallible half cannot be moved
  below the write". Re-verification disproved that by compilation: because
  `reserve` was an ordinary `pub(crate)` function and `VarveFile::index` was an
  ordinary `Vec` field, both of these built inside `file.rs` —

  ```text
  self.file.write_all(b"payload")?;                     // authoritative write
  let slot = ReservedIndexSlot::reserve(&mut self.index, || Ok(64))?;  // AFTER it
  slot.install(&mut self.index, entry);

  let mut mirror = core::mem::take(&mut self.index);    // around the token
  mirror.insert(0, entry);
  self.index = mirror;
  ```

  The first is F-03's exact shape on the append path; the second bypasses the
  token entirely. The mirror is now a `ResidentIndex` whose `Vec` is a private
  field of `mod resident_index`, so the only growth in the crate is
  `ResidentIndex::install`, which consumes the token; and
  `RecordFile::append_record_at_end` **takes `&ReservedIndexSlot`**, so the
  authoritative write cannot be reached until the `ReadLimitKey::IndexBytes`
  charge and the `try_reserve` have succeeded. Reading, `entry_mut`, `truncate`
  and `adopt_generation` remain available, and none of them can add an entry the
  disk does not have. No behaviour change and no hot-path cost: the tokens are
  zero-sized and the work per append is identical. Both bypasses were
  re-checked against the new code and both now fail to compile (`E0061`, missing
  `&ReservedIndexSlot`; `E0277`, `ResidentIndex: Default` not satisfied;
  `E0599`, no method `insert`). Gated by
  `enforcement_gates.rs::the_resident_record_index_cannot_be_grown_or_outrun`.

- **Matrix state can no longer be addressed without the fail-closed
  fatal-recovery check.** `MatrixLayout::fatal_access_blocked` was a `bool`
  consulted by `ensure_fatal_access_allowed` at eleven call sites — a guard
  every accessor had to remember, which is precisely the shape-B configuration
  round 12's F-05 fix existed to eliminate. It was missing from that round's
  shape-B inventory because the inventory was built by grepping the crate for
  `poisoned`, and this flag has another name. The boolean now lives in
  `mod fatal_access` as a private field of `FatalAccessGate`, whose only output
  is the zero-sized `FatalAccessAllowed` witness produced by
  `FatalAccessGate::allow` — which *is* the `Error::MatrixFatalCorruption`
  refusal — and `MatrixLayout::block_index` / `commit_index`, the only two
  routes from a block id or a commit category to a position in the layout's
  state, demand it. A matrix accessor written later cannot address what it wants
  to touch without the check. Behaviour is unchanged for every existing path
  (the same operations refuse, with the same error); the refusal is now made
  slightly earlier than the "wrong commit kind" check in
  `is_single_committed` / `is_channel_committed` / `set_single_committed` /
  `set_channel_committed`, which is only observable on a layout that is already
  fatally blocked. Proofs:
  `crates/varve/tests/ui/fail_fabricated_fatal_access.rs` and
  `enforcement_gates.rs::matrix_state_is_only_addressed_through_the_fatal_access_witness`.

- **The shape-B inventory is now taken by shape, not by name.**
  `enforcement_gates.rs::no_guard_consults_a_bare_boolean_field` enumerates
  every `.rs` file in `varve-core` — not a list of six — and fails when any
  struct declares a `bool` field that an `fn ensure_*` guard refuses on. That is
  the rule the previous gate approximated by matching the literal string
  `poisoned: bool`, which by construction could not find `fatal_access_blocked`
  or `crc_valid_complete`, the latter added by the very round that was closing
  this class. The scan is bounded by each guard's own closing brace and requires
  a refusal rather than a mere read, so bookkeeping flags
  (`indexed.rs::ensure_batch` and its `dirty`) are not reported. Negative
  control: dropping the pre-fix declaration into the crate as an undeclared
  module makes the gate fail and name it.

### Earlier rounds in this release (rounds 1-11, logged 2026-07-20)

The first eleven review rounds of 0.4.0, kept as a development log. 0.4.0 was
never released, so these entries are part of the single release above rather
than a separate one. The Rust API and several persisted layouts change
incompatibly; every affected artifact is stale-regenerable and refused with a
typed error rather than migrated in place.

#### Added

- `ReadLimits::max_keyed_tail_bytes` (DSL key `keyed_tail`, builder
  `with_max_keyed_tail_bytes`) and the `ReadLimitKey` resource
  `"keyed tail bytes"` (API3-02). The resident keyed-tail cache - the
  per-block-id map that lets a keyed append resolve its predecessor in O(1) -
  was guarded by `try_reserve` alone on `VarveFile`/`VarveWriter` and by a bare
  infallible `HashMap::insert` in every generated keyed writer, so a
  large-but-satisfiable cache was refusable by no configured policy and, in the
  generated writers, an allocation the allocator refused aborted the process
  instead of returning a typed error. Both paths now reserve *and charge* the
  slot before the append: growth is refused with
  `Error::LimitExceeded { resource: "keyed tail bytes", .. }` before the record
  becomes authoritative, and the post-append step stays infallible. `STANDARD`
  leaves the ceiling at `u64::MAX`; `UNTRUSTED` sets it to 256 MiB. Repeating an
  existing key is charged nothing.
  Note the shape of that charge changed again in round 9 below: the generated
  writers no longer keep their own typed map, so both routes charge inline
  storage *plus* the key payload bytes retained. `reserve_keyed_tail_slot`,
  added earlier in this release cycle as the generated writers' entry point for
  the charge, does not ship — it was removed in round 9 with the typed map it
  served.
- A contributor invariant checklist, kept as an internal working document rather
  than published: the five invariants every structure this project adds must
  satisfy, the inventory of everything added in the 0.3.0 and 0.4.0
  stabilization rounds, and each entry's audited status.
- Stable codec identities for the two built-in public value types (API-03,
  API-04). `ChunkedBytes` and `PackedBitmap` now declare a non-zero, structurally
  derived `SCHEMA_ID` on both `VarveEncode` and `VarveDecode`
  (`ChunkedBytes` = `0x0b01_19b7_650d_1366`, folded from the `chunked_bytes` tag,
  `CHUNKED_VERSION`, and `Vec<u8>`'s identity; `PackedBitmap` folded the same way
  from the `packed_bitmap` tag and the `u64` + `Vec<u8>` it emits). Both are const
  expressions over compile-time constants, so they are build- and
  platform-stable. **This restores documented public API:** the mandatory
  non-zero field-codec identity contract had made `blob: ChunkedBytes` and
  `mask: PackedBitmap` fail to compile as derived fields. Encoded bytes and
  `WIRE_TYPE` are unchanged.
- `tools/public-api-fixture`: a downstream crate, outside the workspace like
  `tools/rename-fixture`, that derives fixed, variable, and matrix blocks over
  **every** public codec and field type the documentation lists — the scalars,
  `bool`, `String`, `Vec<u8>`, the typed vectors, fixed and nested arrays, 2/3/4
  tuples, `Option`, `BTreeMap`, `HashMap`, `()`, `ChunkedBytes`, and
  `PackedBitmap` — and round-trips each one, plus a real file round-trip through
  a `varve_format!` declaration. It runs as its own CI job (`public-api
  fixture`). This is the gate whose absence let the identity contract silently
  reject public API: nothing in the workspace derived a block over the public
  surface the way a consumer does.
- `KeyedMergeEstimate::peak_resident_structural_bytes()` and the new public
  fields `largest_input_open_transient_bytes` (PERF-05) and
  `max_output_values_bytes` (F-04). The `8N` sequence-uniqueness temporary an
  input's open can hold alongside its resident index, and the output `Vec`
  reserved while the merge state is still alive, were both omitted from the
  estimate. `KeyedMergeEstimate` is `#[non_exhaustive]`, so the added fields are
  not breaking; the method rename is (see Breaking).
- `VarveFile::block_tail_entries_moved()`, a `#[doc(hidden)]` fault-injection
  counter gated on `scalable-fault-injection`, mirroring
  `block_tail_index_touches`. It counts tuples *displaced* in the tail vector
  rather than index visits, which is the cost the old counter could not see.
- A persisted per-page index region in the matrix layout (PERF-01, F-03, F-06).
  It sits between the static aux regions and the `MCRC` region, and open builds
  its visit set from it rather than from the logical page count. The region is
  `(page_count + 1) * 8` bytes: slot `0` is a self-checking occupancy header
  (count in the low 48 bits, a derived check in the high 16) and slots
  `1..=count` hold `page + 1`. Enumeration length comes from the header, never
  from a terminator scan, so a zeroed or out-of-range counted entry is provable
  damage and is reported as a `Fatal` `MatrixCorruptionKind::CommitMap` finding
  while the scan continues past it. The array is the **live** set: a page's
  entry is released in `O(1)` when its final set bit clears. Its resident cost
  is charged to `ReadLimitKey::MatrixBitmapBytes` alongside the payload pages.
  See Breaking for the layout version.
- `MatrixRecoveryReport::matrix_sparse_zeroing_supported()`,
  `::matrix_last_zero_range_streamed_bytes()`, and
  `::matrix_total_zero_range_streamed_bytes()` (F-08). Whole-category clear
  removes the byte range only where the platform supports it (Windows
  `FSCTL_SET_ZERO_DATA`, Linux `FALLOC_FL_PUNCH_HOLE`); everywhere else, and on
  any failed attempt, it streams `Theta(cells / 8)` zero bytes. The capability
  accessor reports the compile-time platform capability and is **not** sufficient
  on its own; the streamed-byte counters are the runtime proof of which path
  ran. All three are always available, not feature-gated.
- CI jobs: `public-api fixture` (above); `test (compression-without-integrity)`
  and `test (integrity-without-compression)`, which run the **test** suite for
  the two singleton feature configurations that own `cfg`-exclusive behaviour
  and previously ran in no test job at all (CI-01); `msrv (1.95.0)`, which pins
  the declared `rust-version` floor and both checks and tests against it, and
  fails if the pin and the manifests disagree (CI-02); and `package contents`,
  which asserts that all three published `.crate` file lists contain
  `README.md`, `LICENSE-MIT`, and `LICENSE-APACHE` (DOC-01).
- Packaged documentation and licenses (DOC-01). All three crates set
  `readme = "../../README.md"` and carry `LICENSE-MIT`/`LICENSE-APACHE` in their
  own directory, so `cargo package --list` now includes them. The `varve` facade
  gained crate-level rustdoc (entry points, a runnable getting-started example,
  the complete Cargo feature table, and the guide index), and `VarveBlock` and
  `varve_format!` gained entry-point rustdoc covering their attributes, the
  generated items, the schema-fingerprint derivation, and the mandatory field
  codec identity rule.
- The API feature table documents the two previously omitted exported features
  and labels them: `high-cardinality-dev` is **experimental** (surface and
  sidecar layout may change without a major version) and
  `scalable-fault-injection` is **test infrastructure**, not a production
  feature.
- Documented resident scale contract and caller-side guards for the keyed
  merge/compact family (PERF-03). `merge_keyed_files`, `compact_keyed_files`,
  and `compact_keyed_file` are resident operations and are explicitly **not**
  PB-scale: time is `Theta(records + decoded bytes) + O(K-live log K-live)` and
  memory is `O(K-ever + largest resident input index + retained live values)`,
  where `K-ever` counts every distinct key ever seen including tombstoned keys.
  Nothing spills to disk and Varve exports no bounded-memory external
  merge/compact. New entry points let a caller predict or fail typed instead of
  exhausting memory: `estimate_keyed_merge::<T, P>(spec, base, deltas) ->
  KeyedMergeEstimate` (decode-free pre-flight; pass an empty delta slice to size
  a single-input compact) and `merge_keyed_files_with_key_limit`,
  `compact_keyed_files_with_key_limit`, `compact_keyed_file_with_key_limit`,
  which refuse a run exceeding `max_distinct_keys` with
  `Error::LimitExceeded { resource: "merge distinct keys", .. }` at the key
  boundary and publish no output. The unbounded entry points delegate with
  `u64::MAX`, so their behaviour is unchanged. `estimate_keyed_merge` itself
  opens each input as a resident file, so it costs `O(largest input index)`.
- `VarveFile::push_keyed` / `push_keyed_info` and `VarveWriter::push_keyed` /
  `push_keyed_info`: maintaining generic keyed append paths that resolve and
  link the keyed offset-chain predecessor. See the corresponding Breaking entry
  for `push` / `push_info`.

- Per-create nonce for stream and disk-indexed primaries (STO-01). Every
  `VarveStreamWriter::create` / `VarveIndexedWriter::create` stamps a 128-bit
  nonce as the primary's first record, under the new reserved
  `CREATION_NONCE_BLOCK_ID`, and folds it into the sidecar's primary-generation
  witness. A sidecar is therefore refused with
  `DiskIndexError::PrimaryGenerationMismatch` when its primary is replaced in
  place by another equal-length primary of the same format, regardless of how
  much leading content the two generations share; `rebuild_disk_index` is the
  recovery. The nonce costs one record at create and nothing per append.
  **Breaking, stale-regenerable:** stream/indexed primaries now carry one extra
  internal leading record, so record counts and sequence numbers shift by one
  relative to 0.3.0, and every existing sidecar's recorded generation witness is
  refused at open until rebuilt. Primaries bootstrapped from a resident
  `VarveFile` carry no nonce and keep the weaker content-window witness only.

- `DiskIndexDescriptor` (exported as `DiskIndexedBlock`) carries
  `schema_fingerprint`, the block schema fingerprint of the concrete type whose
  decode and key-extraction pointers it holds (API-03).
- CI runs a blocking locked fuzz-workspace job: `cargo metadata --locked` before
  any caching or tooling step, then `cargo check --locked --all-targets`,
  `cargo audit --file fuzz/Cargo.lock`, `cargo deny`, and a final
  `git diff --exit-code -- fuzz/Cargo.lock`. The ordering is deliberate — both
  rust-cache and cargo-deny run an unlocked `cargo metadata` that would
  regenerate a stale lockfile and hide the defect the job exists to catch — and
  the trailing diff makes the job fail closed. The clean-archive job now also
  runs `cargo metadata --locked` and `cargo check --locked --all-targets`
  against the archived `fuzz/` workspace (REL-01).
- Windows crash-fault test children suppress interactive error reporting
  (`SetErrorMode`, `SetThreadErrorMode`, and process-level `WerSetFlags`) before
  inducing a fault, so the release gate cannot raise a WerFault dialog on a
  developer machine with default WER settings (TEST-01). On the measured host,
  where interactive WER was already disabled system-wide, suite wall time was
  unchanged within noise; the durable benefit is unattended behaviour, not
  speed.

- Experimental `high-cardinality-dev` stream and disk-index handle family with
  required `.vks`/`.vki` checkpoints, generated typed batch APIs, explicit
  restore/bootstrap/rebuild operations, and indexed point lookup plus lazy
  sequential iteration on one reader.
- Checked file-offset, length, snapshot-bound, and record-pointer types for
  validating sidecar-derived extents before native I/O or allocation.
- Synchronous progress and cooperative cancellation for explicit scalable
  verification, stream bootstrap, and disk-index rebuild scans.
- O(1) explicit stale-writer-lock clearing that does not open or scan the data
  file, with process-aware policies on Windows and Unix.
- Reproducible scalable persistence-boundary, sparse-offset, and redb sidecar
  robustness harnesses.
- Workspace `varve-test-runner` with atomic fresh-session creation, failure
  retention, success cleanup verification, and collision/refusal tests.
- `ReadLimits::UNTRUSTED` / `ReadLimits::untrusted()`, a finite companion to
  `STANDARD` (16 GiB file, 16M records, 16 GiB scan, 1 GiB index, 65,536
  segments) for resident opens of input from untrusted sources.
- Exclusive-create constructors `VarveFile::create_new` and
  `VarveWriter::create_new` that never truncate or reuse an existing path.
- `Error::MatrixFatalCorruption` and `FormatSpec::with_matrix_fatal_forensics()`
  for the matrix fatal-finding fail-closed default and explicit forensic opt-in.
- `historical_distinct_keys()` (`K-ever`) sidecar capacity metric on
  `DiskIndexStore`, `DiskIndexSnapshot`, `VarveIndexedReader`, and
  `VarveIndexedWriter`.
- Required `VarveBlock::SCHEMA_FINGERPRINT` with a `#[derive(VarveBlock)]`-computed
  value, `KeyedBlockContract`, and `Error::BlockSchemaFingerprintMismatch` /
  `Error::BlockKeyednessMismatch` for typed registration.
- `Error::PublishedButParentSyncPending` typed post-publication replacement state
  distinct from a rollback.
- `ReplacePublicationFailure` and the pure, platform-independent
  `classify_replace_publication_error(raw_os_error)` classifier for Windows
  `ReplaceFileW` failures, plus `Error::ReplacePublicationIndeterminate` for OS
  errors 1176/1177, where the target pathname state is unknown. On that error
  the replacement temp file is preserved for reconciliation and a writer bound
  to the target is poisoned; blind retry is forbidden.
- Exclusive-create matrix constructors `VarveFile::create_new_with_dims` and
  `VarveWriter::create_new_with_dims` that hold the single exclusively created,
  lock-bound handle from claim through matrix initialization (no pathname
  re-open window). The matrix self-test uses this path.
- `FormatSpec::block_identities` (with `with_block_identities` and the builder
  setter): a per-block identity table `(block_id, endian override, keyedness,
  generated codec fingerprint)` that `varve_format!` emits and
  `computed_schema_hash()` folds into the hash.
  `FormatSpec::SCHEMA_HASH_ALGORITHM_VERSION` names the hash algorithm
  revision (now 3). `validate()` rejects duplicate or unregistered identities.
- `VarveEncode::SCHEMA_ID` / `VarveDecode::SCHEMA_ID`: the structural identity
  of a codec's bytes. Built-in scalars declare leaf identities and built-in
  containers fold their elements transitively (an element that declares no
  identity propagates outward as "no identity", so a container cannot launder
  one). Derived blocks set it to their `SCHEMA_FINGERPRINT`, which now folds
  every field's encode and decode identity, so nesting propagates identity.
  Hash algorithm version 2 -> 3 (domain tag `varve-schema-v3`): every computed
  value changes and pre-v3 pinned files fail closed with `SchemaHashMismatch`.
  **Breaking for hand-written codecs:** the derive rejects at compile time any
  field whose codec leaves `SCHEMA_ID` at the default `0`, regardless of wire
  type.
- `MatrixSidecarManifest::matrix_creation_nonce`: the 16-byte per-create nonce
  that binds a sidecar to one logical matrix creation, not just one OS file
  object.
- `Encoder::encode_nested_to_vec`, which caps a child encoder at the parent's
  remaining logical-payload budget; generated variable-block field encoding
  uses it so nested fields cannot stage bytes past the writer's limit.
- GitHub Actions CI (Ubuntu + Windows): rustfmt; a per-feature Clippy matrix
  (`-D warnings`, `--locked`) covering no-default-features, default, each
  optional feature alone (`integrity`, `mmap`, `zero-copy`,
  `compression-zstd`, `high-cardinality-dev`, `scalable-fault-injection`), and
  the all-feature workspace union; default and all-feature test runs through
  `varve-test-runner` so leaked test artifacts fail the build; a **blocking**
  `cargo deny`/`cargo audit` supply-chain job; a renamed-dependency fixture
  build (`vv = { package = "varve", ... }`); and a clean-archive job that
  unpacks `git archive HEAD` and runs `cargo metadata`/`cargo check --locked`
  so an uncommitted workspace member can never pass CI again.

#### Changed

- `HashMap` decoding charges the hash table it actually allocates (SAFE-01).
  Decoding used the shared per-entry map model, whose floor of one byte for a
  zero-sized entry let a declared count near `2^30` pass the standard 1 GiB
  materialization budget and then reserve a multi-hundred-megabyte table; a
  bounded randomized run ended at its RSS ceiling on exactly this path.
  `HashMap` now uses its own `preflight_hash_map_count`, which keeps the
  wire-length screen and the "a collection count cannot make bounded input
  progress" rejection and then charges a deliberate over-estimate of hashbrown's
  allocation: `next_power_of_two(ceil(len * 8 / 7)) * (size_of::<T>() + 1)`, plus
  a 16-byte control group and the entry alignment. The `+ 1` per bucket is the
  control byte, which is why a zero-sized entry can no longer be charged one
  byte. All of it is checked arithmetic; an overflowing model returns
  `Error::LimitExceeded` instead of attempting the reservation. Independently,
  `try_reserve` is now given `len.min(1024)`, so a hostile count pre-allocates
  nothing before an entry is proven decodable, while maps of at most 1024
  entries — the common case — reserve exactly as before.
- Typed block registration validates the block's byte order (API-01).
  `BlockContract` carries the `Option<Endian>` override from the authoritative
  `FormatSpec::block_identities` entry, and every typed entry point (push,
  `blocks`, `keyed_blocks`, merge, replace, matrix cell read/write, mmap windows,
  indexed and stream) compares the *resolved* byte order —
  `declared.unwrap_or(spec.endian)` on both sides — before any I/O. A manual type
  that mirrors a generated block's id and fingerprint but declares the opposite
  endian used to register successfully and byte-swap every value it read; it is
  now rejected with `Error::EndianMismatch`. Two declarations that resolve to the
  same byte order are still accepted, because they genuinely produce identical
  bytes.
- The first-use block-contract cache can no longer alias two formats (API-02).
  For any block id the format declares an identity for — which is every block of
  every `varve_format!`-generated spec — the contract is now validated directly
  from the spec's immutable `&'static` data and the process-global cache is
  neither read nor written. That is also strictly cheaper on the per-record
  append path: it removes a global `RwLock` shared-lock acquire and a binary
  search that grew with every format in the process. The cache survives only for
  hand-built specs that declare no identity, and its key gained both slice
  lengths (`blocks` pointer + length, `block_identities` pointer + length, block
  id), so an empty or prefix view of a static array can no longer share an entry
  with the full view. Registration order is now provably irrelevant for
  identity-bearing formats.
- Matrix open is index- and extent-driven, not page-count-driven (PERF-01).
  `load_paged_bitmap` no longer loops `0..page_count`. Its visit set is the union
  of the persisted page index and, where the platform can answer, the pages the
  filesystem allocation map reports as written — enumerated by walking allocated
  *ranges*, not pages. Neither term derives from the logical page count, and an
  unavailable or over-cap allocation map now falls back to the index instead of
  treating every logical page as readable data. The index scan grows
  geometrically from a 64-byte request, so an empty index costs one small read
  regardless of map width. Index entries are written before the page and digest
  they describe, so a torn append can only name a page that still reads as
  uninitialised zeros — a state open already accepts. Recorded trade-off: with no
  allocation map available, a stray byte written out of band into a page the
  matrix never published is no longer detected at open. With an allocation map
  present — the normal case — detection is unchanged.
- Sparse bitmap pages are evicted when their last bit clears (PERF-02). A page is
  now `BitmapPage { bytes, ones }` with a maintained set-bit count, so "is this
  page all zero" is `O(1)` on the mutated page; a page that loses its final set
  bit is dropped immediately and its bytes refunded to the resident bitmap
  budget. Nothing scans the page or the map on the hot path. Long-running sparse
  set/clear activity no longer holds residency proportional to historically
  touched pages.
- `BlockTails::from_index` no longer has a schema-width quadratic term
  (PERF-03). Tail construction records the newest offset per block id in a map
  and sorts the distinct ids exactly once: `O(N + B log B)` time and `O(B)`
  memory, replacing the incremental form's `O(N log B + B^2)` tuple movement.
  The append path deliberately keeps sorted-vector insertion — at most one
  insertion per distinct id for the whole life of a file, `O(log B)` lookups, and
  no hashing on the hot path.
- Keyed merge/compact publish honest cost formulas (PERF-05). The rustdoc for
  `merge_keyed_files`, `compact_keyed_files`, and `merge::compact_keyed_file` now
  states the per-input open's `O(N log N)` sequence-uniqueness sort and the `8N`
  temporary it can hold alongside the resident index. `validate_unique_sequences`
  also gained an allocation-free fast path: the writer hands out sequences
  monotonically, so a file scanned in offset order is proven unique in one pass
  with no `Vec` and no sort, and only a reordered or hostile input pays the copy
  and sort. The estimate still charges the transient unconditionally, because the
  sort is the guaranteed bound.

  **Retraction (F-04):** an earlier draft of this entry called
  `peak_resident_bytes()` "a true upper bound". It never was one. It counts
  `count * size_of::<...>()` inline storage and cannot see the heap owned by
  individual `Key`/`T` values, `HashMap` load-factor slack and control bytes,
  per-record decode scratch, or allocator metadata, so for a heap-owning key or
  value it could understate by an arbitrarily large margin. The method is
  renamed to `peak_resident_structural_bytes()` and documented as a structural
  estimate. Callers who need a hard ceiling must bound the run with a
  `*_with_key_limit` entry point.
- Keyed compact/merge removes its own internal rewrite-temp lock marker
  (STO-01). `write_keyed_values_atomically` unlinks `<temp>.lock` on every exit,
  after the temp's `VarveFile` (and therefore its `WriterLock`) is dropped and
  while the caller still holds the output's writer lock. This is safe precisely
  because the pathname is generated with `create_new` and the output's writer
  lock excludes any competing rewrite that could regenerate it — none of which is
  true of a user path, whose markers remain persistent stable identities and are
  untouched. If the temp itself is preserved (the `ReplacePublicationIndeterminate`
  case) its marker is preserved with it.
- `sync()` and `commit_durable()` make a *created* pathname durable (DUR-01,
  decision recorded). The atomic replacement path already synced the parent
  directory and reported a pending parent sync, so a create path that did not was
  internally inconsistent with a public contract that promises durable
  persistence. A handle that created its pathname now also fsyncs the parent
  directory on its first successful `sync`/`commit_durable`: one directory fsync
  per created file, never repeated, never on the append path, and never for a
  handle that merely opened an existing pathname. See Breaking for the error a
  refused directory sync produces.
- The test runner defaults to locked dependency resolution (REL-01). The
  documented no-argument invocation and every forwarded command now run Cargo
  with `--locked`, inserted after the subcommand and never after a literal `--`,
  so the local completion gate resolves exactly as CI does and a stale
  `Cargo.lock` fails the command instead of being silently refreshed by it. An
  explicit `--locked`, `--frozen`, or `--offline` from the caller is preserved.
- The benchmark reports each operation against its own denominator (BENCH-01).
  Merge and compact consume the base file plus every delta record (updates,
  deletes, inserts) and emit the surviving live values; all three previously
  reported "records/sec" against the original base record count, which was
  neither. Each line now prints input events per second plus the live values
  emitted, and the emitted count is asserted against the file the run actually
  produced. Codec, append, and open/scan lines are unchanged — `records` was
  already their true denominator.
- CI gates are pinned to immutable identities (CI-03). Every action is
  referenced by commit id with its release in a trailing comment
  (`actions/checkout` v4.2.2, `Swatinem/rust-cache` v2.9.1,
  `EmbarkStudios/cargo-deny-action` v2.1.1, `dtolnay/rust-toolchain` master with
  an explicit `toolchain` input), and `cargo-audit` is installed with an exact
  `--version`, so a future run of the workflow evaluates the same code it
  evaluates today.
- Matrix commit-map integrity is paged (PERF-01). The single per-category commit
  CRC is replaced by an array of 8-byte per-page digests (`crc32` plus a state
  word, one per 4 KiB of bitmap). A commit-bit mutation rehashes only the page
  holding the mutated byte, so writing and committing `M` cells hashes
  `Theta(M)` bitmap bytes instead of `Theta(M^2)`. There is deliberately no
  composition checksum over the digest array: maintaining one per mutation would
  reintroduce a size-dependent hot-path cost. Whole-map verification remains
  "verify every page against its digest", which is what open does.
- Matrix creation and open no longer scale with cell count (PERF-02). Creation
  writes only the descriptor tables plus a 16-byte `MCRC` header; the per-cell
  checksum array, the per-cell validity bitmaps, and the commit bitmaps are
  established as a sparse zero extent. Uninitialized pages are encoded
  explicitly (`state = 0` asserts "never published, must still read as zero"),
  so a page written with zeros is distinguishable from a page never written, and
  a per-cell checksum is trusted only when its persistent validity bit is set.
  Commit, CRC-valid, and current-write bitmaps are held sparsely after open: a
  page is materialized only when it carries a set bit, an absent page is
  provably zero, and the maintained set-bit totals make committed-cell counting
  `O(1)` instead of a full scan. The provably-constant `written` bitmap was
  removed outright. Open is bounded the same way: instead of reading every page
  to prove never-published pages still read as zero, it takes that proof from
  the filesystem's allocated-range map
  (`FSCTL_QUERY_ALLOCATED_RANGES` on Windows, `SEEK_DATA`/`SEEK_HOLE`
  elsewhere) and skips reported holes. Where the allocation map is available,
  detection strength is unchanged, because writing a stray byte into an
  untouched page allocates that page and brings it back into the read set, and a
  skipped page's digest is still read when the digest slot itself is allocated.
  Where the platform supplies no usable map, every persisted-index page is still
  verified, but a never-indexed page is not visited, so a stray byte written out
  of band into a page the matrix never published is not detected at open; that
  page is never loaded, so the loss is corruption visibility, not unchecked
  acceptance. As shipped in this release, open enumerates
  the union of the persisted page index and that map in `O(Q)` for `Q` candidate
  pages; where the platform or filesystem cannot answer, or the file is
  fragmented past the tracked extent ceiling, the page index alone drives
  enumeration and open still costs `O(live pages)`. (The intermediate wording of
  this entry stated open's cost in terms of the bytes the matrix had written, and
  described the no-allocation-map case as reading every page; both were retracted
  before release — see *Fixed* below.) Clearing a whole
  commit category removes the byte ranges of the map and its digests instead of
  writing zeros where the platform supports removal, restoring the uninitialized
  encoding in `O(1)` writes, and streams `Theta(cells / 8)` zero bytes where it
  does not.
- The `matrix_bitmap` resource limit now charges the bitmap pages actually
  materialized, checked before each growth, instead of a dense cell-count-scaled
  worst case. It still fails closed, but a large matrix with few committed cells
  is no longer refused on a figure describing a representation that no longer
  exists.
- Generic block registration validates against the format, not against whichever
  type arrived first (API-01). When the `FormatSpec` declares an identity for a
  block id, that immutable `(keyedness, fingerprint)` pair is the authority and
  a disagreeing `T` is rejected with `BlockSchemaFingerprintMismatch` /
  `BlockKeyednessMismatch` before anything is cached or written, so call order
  can no longer decide which wire type a process accepts. The process registry
  is now only a cache of an already-validated result, and its key includes the
  spec's identity table so two specs sharing a descriptor table but declaring
  different identities cannot alias one cached contract. Block ids that a
  hand-built spec declares no identity for keep the documented first-use
  behaviour.
- Disk-index plans validate descriptor schema identity before any decode
  (API-03). Plan construction and plan validation run the registration gate once
  per descriptor, so a descriptor that matches a declared block's id and version
  while being a different type is refused with
  `Error::BlockSchemaFingerprintMismatch` with zero decoder calls. The
  fingerprint is folded into the plan digest, so a sidecar published for one
  block schema is refused as stale for a plan that decodes another.
- Shared sidecar registry access is amortized `O(1)` (PERF-04). A lookup or
  invalidation is one map probe; dead slots are swept only after the map grows
  past a doubling threshold, so opening `S` live identities in sequence costs
  `O(S)` slot checks instead of `Theta(S^2)`. The liveness rule is unchanged.
- Resident block-offset chaining uses maintained sorted block tails (PERF-05).
  Per-append predecessor lookup is `O(log B)` in the number of distinct block
  ids, with no resident-index reads and no allocation once a block id has
  appeared, replacing a reverse scan of the resident index that cost
  `Theta(distance to the previous record of that block)`. The table is rebuilt
  with one forward pass wherever the resident index is loaded or replaced
  wholesale.
- Decoding charges the materialization budget for variable field-id bookkeeping
  (RES-01). Each distinct field id above 63 charges 8 bytes before the tracking
  set reserves, surfacing as
  `Error::LimitExceeded { resource: "variable field ids" }`. Duplicate detection
  still runs first, so a repeated id cannot drain the budget.
- The authoritative single-writer guard is now an OS lock on the open native file
  object (Windows `LockFileEx` on a reserved non-data byte, Unix advisory lock),
  bound at create and re-bound to each published generation. A hard-link or
  reparse alias can no longer open a second concurrent writer; the `.lock` marker
  is diagnostic/break-policy metadata only.
- Atomic replacement reports post-publication durability explicitly: a
  parent-sync failure after a successful rename rebinds the writer and returns
  `PublishedButParentSyncPending`; a failed rebind poisons the writer with
  `PublishedButRebindFailed`. Any other replacement error means publication did
  not happen — with one Windows exception: `ReplacePublicationIndeterminate`
  (`ReplaceFileW` errors 1176/1177) means the pathname state is unknown. The
  Windows path first captures the replacement's OS object identity and, on
  1176/1177, reconciles: if the target already resolves to the replacement
  object the publication is treated as complete; otherwise the typed
  indeterminate error is returned with the temp preserved and the writer
  poisoned.
- Windows parent-directory sync is honest: `FlushFileBuffers` requires
  `GENERIC_WRITE`, so the parent directory is now opened with write access, and
  open/flush refusals (`PermissionDenied`, `InvalidInput`, `Unsupported`) are
  no longer promoted to a `Durable` result. A read-only directory handle fails
  the flush with `ERROR_ACCESS_DENIED` on NTFS (verified live on an NTFS
  host), so the former code silently misreported `Durable` there; publications
  on filesystems that refuse a directory write-open/flush now surface
  `PublishedButParentSyncPending` instead.
- The redb sidecar publication sites (stream/indexed sidecar create, stream
  bootstrap, disk-index rebuild) no longer discard the replacement durability
  state: a parent-sync failure surfaces as `PublishedButParentSyncPending`
  while the already-published sidecar is preserved and usable.
- Every matrix create stamps a fresh 24-byte creation-nonce region (`VMNC`
  magic, version, 128-bit nonce) between the native file header and the matrix
  layout header, cached in the handle at open/create (zero per-operation
  cost). The nonce is folded into the sidecar identity, so recreating a matrix
  into the same pathname/file object with the same dimensions can no longer
  adopt the previous generation's sidecar: stale sidecars are refused as
  `MatrixSidecarMismatch("creation nonce")`, and a caller-supplied generation
  cannot substitute for the native creation identity.
- File creation binds the single-writer object lock before destructive
  initialization: create paths open without truncate, bind the native object
  lock, then `set_len(0)` and write the header, closing the window where a
  losing concurrent creator could truncate the winner's freshly initialized
  file.
- Matrix sidecar reads validate all fixed-header identity fields and the small
  payload magic/category prefix before any payload allocation, read, or hash,
  so an obviously foreign sidecar is rejected without doing
  configured-limit-sized work.
- `needs_index_checkpoint` is O(1): the writer keeps a counter of eligible
  records since the last checkpoint and a precomputed geometric threshold,
  maintained incrementally at the append site, restored on rollback, recovered
  once at open, and recomputed at generation rebind. The former per-flush reverse
  index scan made flush-per-record workloads O(N²) in CPU even after the
  checkpoint byte growth was linearized. The adjacent per-flush
  uncommitted-tail scan is also O(1) now.
- Matrix cell read, write, and mmap access enforce the common typed
  registration gate (`ensure_registered_block`) first, so a manual matrix
  block with the same shape and stride but a different schema fingerprint or
  keyedness is rejected (`BlockSchemaFingerprintMismatch` /
  `BlockKeyednessMismatch`) instead of decoding foreign cells.
- The compile-time keyedness contract (`KeyedBlockContract::<T>::OK`) is now
  evaluated at every public keyed generic entry point — stream delete, indexed
  lookup/get/push/delete, merge/compact, low-level `VarveFile`/reader/writer
  delete, `keyed_blocks`, `key_tail_offsets`, disk-index descriptors, and the
  self-test keyed case — with first-seen runtime registration retained as the
  backstop. A `VarveKeyedBlock` impl declaring `IS_KEYED = false` fails
  compilation at each of these sites.
- All resident writer entry points (push, metadata, replacements, keyed op
  envelopes) encode through the limit-bounded encoder: an oversized value
  fails with the typed `LimitExceeded { resource: "logical payload length" }`
  error and the encoder stops buffering at the limit instead of materializing
  the full encoding first. Matrix cell writes bound the encode by the slot
  stride and keep the exact-size `MatrixSizeMismatch` contract.
- Matrix self-test cleanup is identity-checked: the native target is deleted
  only while the pathname still resolves to the file object this run created
  (race-free delete-by-handle on Windows; check-then-unlink with a documented
  one-syscall residual window on Unix), and the `.lock` marker is removed only
  after re-acquiring it through the standard writer-lock protocol, so
  foreign-owned or populated markers survive.
- `varve_format!`-generated code works when the `varve` dependency is renamed
  in `Cargo.toml` (resolved via `proc-macro-crate`); the
  `extern crate vv as varve` workaround is no longer needed.
- Integration tests own per-test temporary directories (`tempfile::tempdir()`
  guards), so native files, sidecars, and `.lock` markers are collected on
  drop even under plain `cargo test`, on panic, or early return; nothing is
  left in the system temp root. The project's test-artifact rule is that a test
  command is not successful merely because its assertions pass: its temporary
  session must also be removed and verified absent, with the root preserved on
  failure and cargo build caches exempt.
- Matrix `Fatal` recovery findings fail-close every default read/write/aux/
  resume/rebuild accessor with `Error::MatrixFatalCorruption` via an `O(1)` flag
  precomputed at open; `matrix_recovery_report()` stays readable.
- The matrix sidecar manifest is version 3 (104-byte fixed header) binding a
  native object fingerprint, matrix layout generation, and the matrix creation
  nonce, published atomically through a same-directory temp with
  native-then-sidecar ordering and parent sync.
- `CheckpointOnFlush` spaces full index checkpoints geometrically, bounding
  cumulative checkpoint bytes to `O(N)` instead of the former `O(N²)`.
- CRC typed point lookups and streaming scans read each covered payload once and
  no longer checksum or read skipped foreign-block payloads; `verify_all()`
  remains the whole-file integrity pass.
- Tombstone rebuild resolves the descriptor once per record by block id, making
  rebuild cost independent of the plan descriptor count.
- Stream scalar `push_*` calls share one bounded sidecar transaction committed at
  the chunk bound or on `flush()`/`sync()` instead of committing per record;
  `keyed_offset_chain` tail maintenance uses binary search with per-chunk tail
  collapse.
- Indexed handles for one file share a process-local backing database, so
  independent readers (and a reader beside a writer) coexist; contention surfaces
  as a typed `Error::IndexBusy` rather than blocking. Cross-process exclusivity is
  unchanged.

- Clean scalable open no longer scans native records, and append/`sync()` no
  longer rereads newly written native chunks. Bounded batches coalesce native
  writes while disk-key cardinality remains in redb instead of an in-memory
  Varve key map.
- Generated keyedness metadata permits unkeyed batch blocks inside a
  `keyed_offset_chain` format while rejecting chain-unsafe keyed stream calls.
- Scalable restore now validates format tails before native truncation;
  bootstrap/rebuild refuse dirty sidecars, rebuild honors arbitrarily small
  configured transaction bounds, and paged `.vks`/`.vki` length is no longer a
  lifetime append ceiling.
- Disk-index errors retain their typed source, database contention maps to
  `IndexBusy`, bounded encoders stop before over-limit payload/key allocation,
  and atomic replacement syncs the parent directory where supported.
- Under `high-cardinality-dev`, manual `VarveBlock` implementations must declare
  `IS_KEYED`; this closes the low-level keyed-chain bypass that a default
  `false` value would permit.
- Indexed point reads decode and validate the already-read native record, and
  indexed mutations canonicalize each sidecar key once before reusing it for
  previous-key lookup and insertion.
- Writer-lock stale recovery is serialized by an OS-level exclusive guard;
  concurrent recovery cannot remove or supersede a live writer's ownership.
  Windows recovery distinguishes a signaled terminated process object from a
  live process even while another handle temporarily keeps that object open.

#### Breaking

- **Wire-breaking, stale-regenerable:** the persisted page-index region was
  introduced here, at what was then `VMAT` version 3. The layout shipped in this
  release is **version 4** (see the version-4 entry below, which supersedes the
  version numbers and the file length stated in this entry). Two header fields, `page_index_off` and `page_index_len`, are appended as
  fields 13 and 14 — after `append_log_start`, so every previously defined field
  keeps its index — and the reserved tail shrinks from 32 to 16 bytes. The region
  order becomes `… | slot region | static aux | page index | MCRC | append log`,
  so `append_log_start` and the total matrix file length change. A version 2
  artifact older than the shipped layout is refused at open with a typed
  `Error::FormatVersionMismatch`, the same stale-regenerable contract version 1
  already had; recreate it. The shipped expectation is `expected: 4`.
- **Breaking (compile):** `PackedBitmap` is no longer accepted as a fixed-width
  matrix field. It owns a `Vec<u8>` and encodes a `bit_len` plus a variable byte
  string, so it has no encoded width fixed by its type and therefore no
  compile-time slot stride; the previous acceptance matched on the source
  *spelling* and took a stride from `size_of::<PackedBitmap>()`, which is the
  size of a `Vec` plus a `u64`, not the width of the bytes it emits. It remains
  fully usable as an ordinary variable field. Relatedly, inline matrix
  `SLOT_STRIDE` is now generated from each element's `VarveEncode::WIRE_TYPE`
  rather than `size_of`, so a *user* type merely spelled `u32` or `PackedBitmap`
  can no longer be laundered through the syntactic pre-filter: anything without a
  fixed encoded width fails const evaluation.
- **Breaking (runtime, intentional tightening):** a `VarveBlock` implementation
  whose `ENDIAN`, resolved through `FormatSpec::endian`, disagrees with the
  format's own declaration for that block id is now rejected at typed
  registration with `Error::EndianMismatch` (API-01). This only affects code that
  declares a byte order contradicting the format's — that is, exactly the silent
  byte swap this catches. Manual mirrors that copy the generated block's `ENDIAN`
  alongside its `SCHEMA_FINGERPRINT` are unaffected, and generated code always
  agrees with itself.
- **Breaking (runtime):** the first `VarveFile::sync()` or `commit_durable()` on
  a handle that *created* its pathname can now return
  `Error::PublishedButParentSyncPending` where it previously returned `Ok(())`,
  on a filesystem that refuses the directory sync (DUR-01). The file's contents
  are durable and the pathname is visible; only its directory entry's durability
  is unconfirmed, and the request stays pending so a later `sync` retries it.
  Subsequent syncs, and handles that merely opened an existing pathname, are
  unchanged.
- **Breaking (runtime):** decoding a very large `HashMap` under a tight explicit
  materialization limit can now fail with `Error::LimitExceeded` where it
  previously succeeded (SAFE-01). That is the fix: the old charge under-counted
  the allocation actually performed. The error variant and its
  `resource: "HashMap entries"` string are unchanged.
- Wire-breaking, stale-regenerable: the matrix layout is `VMAT` **version 4**
  with an `MCRC` version 2 integrity region. Within this release the layout
  moved 1 -> 2 (integrity representation), 2 -> 3 (page-index region added), and
  3 -> 4 (page-index occupancy header and live-set semantics); only the final
  state ships. Region lengths, `append_log_start`, and total matrix file length
  all change. A version 1, 2, or 3 artifact is refused at open with the typed
  `Error::FormatVersionMismatch { expected: 4, actual: <1, 2, or 3> }`
  (previously the generic `InvalidMatrixLayout` for v1); recreate it.
- Wire-breaking, stale-regenerable: the disk-index sidecar metadata record is
  version 3 (length 260 -> 300, carrying the primary-generation witness) and the
  plan-digest domain was bumped. A version 2 sidecar is refused with
  `DiskIndexError::MetadataVersion`, and a sidecar published against an older
  plan digest is refused as stale. `rebuild_disk_index` is the recovery for
  both; neither is migrated in place.
- New public error variant `DiskIndexError::PrimaryGenerationMismatch` (the enum
  is `#[non_exhaustive]`, so this is additive) and new public
  `Error::KeyedChainRequiresKeyedApi { block_id }`.
- Callers that size a materialization budget to the exact payload byte count and
  use variable field ids above 63 must add 8 bytes per such distinct id.
- Source-breaking (no wire change): on a format whose `index_policy` enables
  `keyed_offset_chain`, the generic `VarveFile::push` / `push_info` and
  `VarveWriter::push` / `push_info` now refuse a keyed block with the new
  `Error::KeyedChainRequiresKeyedApi { block_id }` instead of silently writing
  `prev_same_key_offset = None` and truncating the keyed chain (API-02). The
  rejection is unconditional — it fires on the first push, not only on a push
  that would have had a predecessor — so the contract does not depend on call
  order. **Migration:** call the new maintaining `push_keyed` / `push_keyed_info`
  instead; they resolve the predecessor from the persisted keyed tail offsets
  and rebuild that cache after reopen. Unkeyed blocks and formats without
  `keyed_offset_chain` are unaffected, and the generated typed writers already
  take the maintaining path. `VarveFile::delete` / `VarveWriter::delete` had the
  same truncation defect and now maintain the chain; they gained a
  `T::Key: Eq + Hash` bound, already implied by `VarveKey`.
- Manual `VarveBlock` implementations must now provide
  `const SCHEMA_FINGERPRINT: u64` (no default). Derive-generated blocks are
  unaffected; a manual block mirroring a generated one must reuse its fingerprint
  constant.
- `FormatSelfTest::run` is non-destructive: it refuses a pre-existing target path
  as a `CallerUsage` failed step instead of truncating it, and `cleanup(true)`
  removes only files the run created.
- Matrix access is fail-closed when recovery records a `Fatal` finding; readers
  that relied on reading through fatal-state files must opt in with
  `FormatSpec::with_matrix_fatal_forensics()`.
- The computed schema hash algorithm moved to version 2 at this point
  (`FormatSpec::SCHEMA_HASH_ALGORITHM_VERSION`; it was later moved again to
  version 3 — see the `VarveEncode::SCHEMA_ID` entry above, which is the value
  0.4.0 ships): fields are hashed in
  declaration order with their encoding ordinal and a field-count frame, and
  per-block endian overrides, keyedness, and generated codec fingerprints are
  folded in via `FormatSpec::block_identities`. This closes the hole where two
  blocks with the same field id/name/type set in a different declaration order
  — and therefore different canonical bytes — hashed identically. **Every
  computed hash value changes.** A file created with a pinned v1 computed hash
  fails open with `SchemaHashMismatch` until recreated (pre-1.0 policy: no
  migration path). `schema_hash: computed` declarations recompute
  automatically at build time; release-pinned literal hashes must be
  re-derived. Omitting `schema_hash` still stores 0 and disables the open-time
  comparison, unchanged.
- Matrix native files gain the 24-byte creation-nonce region between the
  native file header and the matrix layout header. Matrix files created before
  this change are refused with `InvalidMatrixLayout` at the nonce region
  (pre-1.0 policy: recreate them). Non-matrix native files are unchanged.
- The matrix sidecar format is version 3 (fixed header grew from 88 to 104
  bytes for the creation nonce). Version-1 and version-2 sidecars are refused
  as `MatrixSidecarMismatch("sidecar version")`; regenerate them (sidecars are
  regenerable resume state, so no native file migration is required beyond the
  matrix-native recreation above).
- `FormatSpec` gained the public field `block_identities`; code constructing
  `FormatSpec` with an exhaustive struct literal must add it. Construction
  through `FormatSpec::new(...)`, the builder, or generated `spec()` is
  unaffected (defaults to empty).
- Under CRC policies, `rebuild_disk_index` no longer reads the payloads of
  records outside the index plan; a corrupt unindexed payload no longer fails
  a rebuild. `verify_all()` remains the whole-file integrity scan.
- `KeyedMergeEstimate::peak_resident_bytes()` is **removed** and replaced by
  `peak_resident_structural_bytes()` (F-04). There is deliberately no compiling
  alias: the old name's documented contract was false, and a deprecation would
  let callers keep reading the value as a guarantee. The new method's value also
  includes the overlapping output-vector term. `max_state_bytes` keeps its name
  and value; only its documentation changed.
- `PackedBitmap::new`, `get`, `set`, and its `decode_varve` now return
  `Error::InvalidCanonicalEncoding` / `Error::LengthOverflow` and a non-matrix
  allocation resource instead of `Error::InvalidMatrixLayout`. It is an ordinary
  variable-field codec and must not report matrix-shaped failures. Callers
  matching on `InvalidMatrixLayout` for these cases must update.
- A matrix whose bitmap page count would exceed `2^48 - 1` (about 1 EiB of
  bitmap, ~10^18 cells) is refused at layout time with
  `Error::InvalidMatrixLayout`, because the page-index occupancy header cannot
  represent the count. Far above any realistic configuration.
- A damaged matrix page-index entry is no longer silent (F-06). A file that
  previously opened "clean" with a zeroed index entry — while hiding every page
  after it — now reports a `Fatal` finding, and default matrix access is
  fail-closed. This is the correct outcome, but it is a behaviour change for
  already-damaged files.
- Destructive `FormatSelfTest` cleanup is now **refused** on Unix in a directory
  that is not exclusively owned (F-05), with a reported `Environment` step
  failure naming the reason, rather than performed through an unverifiable
  pathname. A self-test run with `.cleanup(true)` in a world- or group-writable
  directory (`/tmp` without the sticky bit reserving entries to their owner)
  therefore now leaves its artifact behind and says so. Use a directory only the
  running user can write, or `.cleanup(false)` and remove the artifact yourself.

#### Fixed (invariant re-verification, round 9)

Round 9 answered the round-8 review (`8732e83`, 2026-07-21). All three of
its release blockers were the same defect class — invariant 3, a fallible or
user-code-invoking step placed after the authoritative commit it belongs to —
so each was closed structurally rather than patched at the named line.

- Generated keyed writers no longer own a `HashMap<T::Key, u64>` tail map
  (F-01). Push and delete inserted into it *after* the record or tombstone was
  authoritative, so the caller's `Hash`/`Eq` ran after publication and a
  panicking implementation left a durable record with a stale tail map — the
  next same-key mutation could then link *around* the committed event. The
  generated writers now route through the same byte-keyed resident cache the
  generic keyed API uses, so no user-defined trait executes after publication
  on any keyed path, and the two routes can no longer drift apart. Side effects:
  the generated writers now charge the key payload bytes they retain, not just
  inline storage (closing the round-7 open item); key identity on the generated
  path is canonical-encoding equality rather than the key type's `Eq`, which is
  the semantics the generic path always had; a generated writer for a format
  without `keyed_offset_chain` builds no tail map at all; and the delete path
  encodes the key once instead of twice.
  `VarveWriter::reserve_keyed_tail_slot` is **removed** — its own rustdoc
  admitted it could not prevent a later `Hash`/`Eq`, which is exactly the
  defect.
- The matrix commit-map rebuild publishes a page-index *generation* instead of
  editing one (F-02). It used to zero the persisted page index and refill it
  entry by entry, so an interruption left a valid-looking short occupancy count
  that is indistinguishable from "those pages were never published": committed
  cells reopened as `NotCommitted` with no finding at all. The rebuild now
  writes `u64::MAX` into the occupancy header and syncs that slot **before**
  destroying anything, clears and refills only the entry region, makes every
  entry, digest and page durable, and writes the real count last as the single
  publishing write. A matrix left holding the marker opens with a `Fatal`
  `MatrixCorruptionKind::CommitMap` finding whose report now recommends
  `RebuildCommitMap`. **No wire-format or layout-version bump**: the marker is
  never a resting state and is provably not a representable occupancy count at
  any capacity, so a reader that predates it rejects it as a damaged header —
  also fail-closed. `VMAT` stays at version 4 and no fixture changed.
- Sparse-bitmap page allocation happens before the disk write, not after it
  (F-03). `apply_commit_bit` wrote the bitmap byte to disk and only then called
  `SparseBitmap::set_byte`, which for the first nonzero byte on a page still
  performed fallible page allocation; an `AllocationFailed` there left disk
  holding the new bit while memory held the old byte, on a writer that ordinary
  matrix mutation handling does not poison (it poisons only for `Error::Io`).
  `set_byte` is split into `prepare_byte_write` (every fallible and allocating
  step, producing a detached page) and an infallible, allocation-free
  `commit_byte_write`. The failure is removed rather than handled. The same
  shape was found and fixed in `apply_cell_crc_valid` and in the session
  write-tracking bit of `write_cell`/`write_cell_payload`, neither of which the
  review named: a resident-bitmap budget refusal could previously return
  `LimitExceeded` with the commit bit, validity bit and slot payload already
  durable.
- `write_matrix_cell_durable` reports a post-commit hook failure as the typed
  published outcome `Error::MatrixCommittedButHookFailed { event, source }`
  (F-04). The hook is documented as post-publication and the behaviour was
  intentional, but a plain `Result<()>` could not distinguish "failed before
  publication" from "published, hook failed", so a result-driven retry could
  duplicate the hook's external work. The carried `MatrixCommitEvent` lets a
  caller retry the notification alone. The event is also now derived from
  layout geometry *before* the commit rather than after it, so `commit_event`
  can no longer fail for a cell that is already durable.
- `commit_durable` reports a durability failure that happens *after* the commit
  marker is appended as the typed published outcome
  `Error::CommittedButDurabilityUnproven { sequence, source }` (round 10,
  invariant 3). `write_commit_marker` is the authoritative commit — once the
  marker bytes are in the file a reader that opens it after a clean process
  exit sees the transaction as committed — but the `flush`/`sync_all` that
  follows returned a bare `Err`, which entitles a caller to believe nothing
  happened and re-run the transaction, appending a second marker for work that
  is already recorded. The correct response to the new variant is to retry
  `sync` alone. The created-pathname sync after it already reported
  `PublishedButParentSyncPending` and is unchanged. This was open item 9 in the
  invariant checklist, closed here.
- `write_matrix_cell_durable` reports a failure of the durability request that
  follows the commit as the typed published outcome
  `Error::MatrixCommittedButDurabilityUnproven { event, source }` (round 11,
  invariant 3). Round 9 typed the hook, which is the *last* step after the
  commit; the step immediately before it —
  `MatrixDurabilityBarrier::sync_matrix_commit` — still returned a bare `Err`
  even though `commit_matrix_cell` had already put the commit bit in the file
  and the cell was readable by a fresh reader after a clean exit. Those two are
  now the call's only published outcomes and the forward walk from the commit
  is complete; the pre-commit `sync_matrix_data` deliberately stays a plain
  error because nothing is published when it runs. The variant carries the same
  `MatrixCommitEvent`, the hook does not run, and the writer is poisoned, so
  recovery is to reopen and `sync` rather than to rewrite the cell.
  `docs/durability-model.md` previously claimed every error other than the hook
  variant meant the cell was not committed; that claim was false for this step
  and is corrected in the same pass.
- Every fallible step of a stream/indexed sidecar chunk commit that runs after
  the chunk's records are already in the native file now reports the typed
  published outcome `Error::PublishedButIndexStale { sequence, source }` and
  poisons the writer (round 11, invariant 3). Only the sidecar transaction's
  own `commit()` did so before; the primary-generation restamp that precedes it
  (`VarveStreamWriter::commit_state_chunk` and
  `VarveIndexedWriter::commit_pending_batch`) returned a bare `Err`, and it runs
  on ordinary chunk boundaries for the whole early life of a file — while the
  generation witness window is still filling. A caller entitled to read `Err` as
  "nothing happened" would retry and append the same records twice. The
  classification now wraps the whole body of both functions rather than
  individual steps, so a fallible step added to either path later is covered
  without anyone remembering to wrap it. **Behavioural change:** a sidecar
  failure on these paths that previously surfaced as `Error::Io` (or another
  plain variant) is now the wrapper, with the original in `source`.
- The batch summary a partially published `push_iter` hands back through
  `BatchAppendError::written` is derived before the native write and assigned
  after it (round 11, invariant 3). Its checked arithmetic used to run *after*
  the chunk was published, at four call sites in `VarveStreamWriter` and
  `VarveIndexedWriter`, and a failure there returned a bare `Err` with a summary
  that under-reports what reached the file — the same value `push_iter` uses to
  decide whether the writer must be poisoned.
- The CRC rebuild charges its bitmap page before allocating it (F-05), so the
  ceiling is a strict pre-allocation limit rather than a report issued once one
  page is already resident.
- All three publishable manifests declare
  `[package.metadata.docs.rs] all-features = true` (F-06). They have
  `default = []` and no docs.rs metadata, so docs.rs built the published
  documentation with **no** optional feature enabled while the CI rustdoc job
  claimed `--all-features` matched it; feature-gated public items could be
  absent from the published pages. CI now also gates the no-default-feature
  surface, and its comment states which gate covers which surface.
- The diagnostic `<target>.lock` marker is verified to be a dedicated,
  unaliased regular file before it is truncated or written (F-07). Acquisition
  truncates and rewrites the object the marker path names, so a pre-placed hard
  link — or a followed symbolic link — from that path to an unrelated empty
  file caused transient writes and truncation of that foreign object. Unix
  opens with `O_NOFOLLOW`, Windows opens the reparse point itself with
  `FILE_FLAG_OPEN_REPARSE_POINT` and rejects it by attribute, and both then
  refuse a multi-link or non-regular object with the new
  `Error::WriterLockMarkerNotDedicated { path, reason }`. This never affected
  authoritative single-writer exclusion, which is a native lock on the target
  file object itself.
- `deny.toml` describes the actual scope of `allow-wildcard-paths` (F-09). The
  previous comment claimed it permitted only the one `varve-macros` path
  dev-dependency edge; re-derived against cargo-deny 0.19.9, it also permits
  path dependencies of *any* kind from crates with `publish = false`. Registry
  wildcards and non-dev path wildcards from published crates remain denied, and
  a new `dependency-policy-fixture` CI job asserts both refusals by introducing
  each shape and requiring the gate to fail.
- The test runner no longer retains an empty session directory (F-10).
  Retention preserves evidence, and a failed or interrupted run that produced
  no files has none; an empty `varve-test-session-*` directory is now removed,
  on the failure path and on drop, so the documented "a run that produced
  nothing leaves no session path" policy is literally true. A session holding
  artifacts is never removed.

#### Documentation (round 9)

- `docs/custom-codec-guide.md` states the allocation-charging obligation
  (F-08): every owned allocation whose size comes from input must be charged
  with `Decoder::charge_materialization` *before* it is reserved. `VarveDecode`
  cannot enforce this, and custom codecs are trusted format-author code, so the
  guide now carries a minimal compliant example and a negative self-test that
  distinguishes a charged codec from an uncharged one — the two are
  indistinguishable on well-formed input. The example is compiled and its
  self-test run by `crates/varve/tests/storage_hardening.rs`.
- `docs/api-reference.md` no longer says the generated keyed writers charge
  inline storage only; both routes now charge inline storage plus the key
  payload bytes the map owns.

#### Fixed (invariant re-verification, round 7)

- `max_keyed_tail_bytes` did not bound what it said it bounded (API3-05). Round 6
  charged only the *incremental* growth of the keyed-tail maps. The dominant
  allocation is the map built from file content by
  `VarveFile::key_tail_offsets` — at generated-writer construction
  (`writer_tail_inits`) and at first resident keyed use — and that build had no
  charge at all. A file with `N` distinct keys therefore forced an `N`-entry
  resident map whatever the configured ceiling said, including `UNTRUSTED`'s
  256 MiB; the ceiling only refused *further* growth within the session. The
  build is now charged as it proceeds, so an attacker-chosen key count is
  refused with `Error::LimitExceeded { resource: "keyed tail bytes", .. }`
  before the memory is taken. The resident path additionally charged its map
  only *after* building it, so the charge gated retention rather than the peak;
  that check now runs during the build.
- Because the charge now models the build's structural **peak**, it is larger
  than the map it produces: a transient per-key ordering entry is alive while
  the returned map is reserved, and the resident cache transcodes into a
  canonical-payload map while the returned map is still alive. Opening a file
  charges more than the steady-state map costs. A ceiling sized from the
  steady-state map alone can therefore now refuse an open that previously
  succeeded. This is deliberate and is documented in `docs/api-reference.md`.
- Two false documentation claims shipped by round 6 are retracted. The rustdoc
  on `VarveWriter::reserve_keyed_tail_slot` said the growth is charged against
  `ReadLimits::max_index_bytes`; the code checks `ReadLimitKey::KeyedTailBytes`,
  i.e. `max_keyed_tail_bytes`. And `docs/api-reference.md`, together with the
  internal notes on the declaration and its internals, promised a ceiling on the
  resident keyed-tail cache that the generated writers did not have. Both are corrected
  and both are now gated by `crates/varve/tests/doc_claims.rs`.
- The invariant checklist claimed more coverage than it had. Its header
  advertised an inventory built from rounds 1-5 while covering essentially
  rounds 3-5: `TailCache`/`SharedSidecar` (round 3), `AllocatedExtents`
  (round 4) and the whole `28a1b68` module set were absent. The scope is now
  stated at the top of the document, the two missing structures have rows with
  their bounds stated, and the unwalked module set is a named open item. The
  writer-lock marker row no longer claims an unqualified "identity-checked"
  removal: the identity is captured from the same unverified pathname, which
  closes the capture-to-unlink window but proves nothing about ownership, and
  Windows has no directory confinement to fall back on.

#### Fixed (invariant audit, round 6)

- `tools/rename-fixture` did not run at all. It declares
  `index: [... keyed_offset_chain]` and a keyed `Item` block, then called the
  generic `file.push(&Item { .. })`, which `push_info` refuses with
  `Error::KeyedChainRequiresKeyedApi` (API2-05). The `renamed-dependency` CI job
  the README lists as a release gate therefore failed on every commit since that
  guard landed. It now calls `push_keyed`, which is also the API the fixture is
  meant to exercise through the renamed facade, and asserts the keyed record
  round-trips.
- Self-test cleanup deleted the writer-lock marker `<path>.lock` by pathname,
  guarded only by re-acquiring an *advisory* writer lock (API3-03). On Unix that
  is the same shape F-05 fixed for the artifact itself, so one destructive path
  in the module was still outside the hardened protocol. The marker's identity
  is now captured from an open handle and the removal goes through
  `remove_path_if_same_object`, which confines the deletion to a
  directory-handle-relative `unlinkat` in an exclusively owned directory on Unix
  and to the verified non-delete-shared handle on Windows. A refusal or failure
  is reported as a failed `cleanup` step instead of being swallowed.
- `BTreeMap` decode was charged `len * size_of::<(K, V)>()`, as if entries were
  packed end to end (API3-04). A std B-tree node allocates a fixed-capacity
  array of eleven entry slots whatever its fill, is only guaranteed to hold
  five, and carries a header - and, for internal nodes, twelve child pointers -
  on top. The materialization charge is now a documented over-estimate of that
  real node cost rather than an under-estimate of it.

#### Fixed (release re-verification, round 5)

Every item in this section is a defect in code this project added earlier in the
same unreleased cycle while fixing an earlier finding, not a pre-existing Varve
defect.

- Generic keyed append and delete can no longer return `Err` after the record is
  authoritative (F-01). `push_keyed_info` and `delete` appended first and then
  grew the resident keyed-tail cache, whose byte arithmetic and `try_reserve`
  are fallible, so an allocation failure returned an error for a record that
  existed — and left the superseded predecessor cached, allowing a later keyed
  mutation on the same writer to link around it. The cache slot is now reserved
  *before* the append and the post-append commit step is infallible (it returns
  `()`). A repeat-key append reserves nothing at all and is strictly cheaper
  than before.
- The `HashMap` reservation model no longer undercharges small tables
  (F-02/SAFE2-01). The model charged one entry as two buckets, but hashbrown
  applies small-table capacity classes with a hard floor: on Rust 1.95 x86-64,
  `try_reserve(1)` on a `HashMap<(), ()>` yields capacity 14 — a 16-bucket
  table — so 19 bytes were charged where control storage alone costs 32. The
  source comment claiming small tables "only ever allocate less" was false and
  has been corrected. The model now floors the bucket count at the assumed
  control-group width, charges the control-array alignment as
  `max(group, align_of::<T>())`, and assumes a 32-byte group — wider than the 8-
  or 16-byte group any supported target uses — so it stays an over-estimate on
  every capacity class and on a hypothetical future wider group.
- Matrix page-index storage is charged to a runtime resource limit and tracks
  live pages rather than publication history (F-03), and matrix open no longer
  sorts (F-07): the index and allocation-map terms are merged in one linear pass
  with a hash probe, `O(Q)` time and `Theta(Q)` temporary memory for `Q`
  candidate pages, with no sort. The resulting list is deliberately unsorted;
  page visits are independent and page loading is idempotent.
- Sidecar publication re-verifies the primary after publishing (F-09). The
  verified primary handle is now held open across publication so its identity
  cannot be recycled, and the pathname is re-resolved once more afterwards. If a
  non-cooperating writer replaced the primary in that interval, the just-published
  sidecar is retired by an identity-checked, directory-confined removal and a
  typed mismatch is returned, instead of leaving a stale sidecar that would
  displace the replacement's own. This does not make publication atomic against
  a writer that ignores `WriterLock` — nothing in userspace can — it bounds the
  damage to a rebuild the caller repeats.
- Matrix field eligibility is decided by the resolved type, not the source
  spelling (F-10). The macro rejected a field when a whitelist of literal
  primitive path names did not match, so `type Word = u32;` was refused before
  the generated `SLOT_STRIDE` — which resolves `VarveEncode::WIRE_TYPE` — could
  decide the real width, contradicting `docs/api-reference.md`. The syntactic
  check is now permissive: it rejects only shapes that can never denote a
  fixed-stride type (references, raw pointers, slices, tuples, trait objects,
  `impl Trait`, function pointers) and defers everything else to the const
  check. An ineligible named type still fails, with
  `matrix fields must have a width fixed by their type; this codec does not` or
  an unsatisfied `VarveEncode` bound. New trybuild coverage: an alias, an
  alias-of-an-alias, a module-qualified alias, and an array of an alias all
  compile; a tuple field still fails.
- `write_keyed_values_atomically` owns its rewrite temp through an RAII guard,
  so a permission-copy failure after temp creation can no longer leave an empty
  temp behind. The guard closes the handle before unlinking (Windows cannot
  rename or delete through a live handle), and the publication outcomes that
  must preserve the temp — `ReplacePublicationIndeterminate`, `Durable`, and
  `ParentSyncPending` — explicitly retain it.

#### Fixed (CI and packaging)

- CI verifies publishable archives (F-11). `package contents` runs
  `cargo package --no-verify --list`, which proves file *names* and nothing
  else: it creates no `.crate` archive and compiles nothing from one, and every
  other job built the checkout by path, so the bytes a crates.io user downloads
  were never compiled anywhere. The new `package archives + staged consumer` job
  runs `cargo package --locked` for all three crates **with verification
  enabled**, then extracts the three archives and rebuilds the public-API
  fixture's source against the extracted trees through `[patch.crates-io]`,
  resolving `varve` by version as a downstream crate does, and runs it.
- CI gates rustdoc warnings (F-12). The new `rustdoc (-D warnings)` job runs
  `cargo doc --locked --workspace --all-features --no-deps` with
  `RUSTDOCFLAGS: -D warnings`, so a broken intra-doc link fails the build
  instead of silently rendering as plain text on docs.rs. The two proc-macro
  entry-point examples (`VarveBlock`, `varve_format!`) were ```ignore, so they
  compiled in no job at all; they are now `no_run` and are compiled by the
  existing test jobs. Compiling them requires a path-only dev-dependency from
  `varve-macros` on `varve` — a dev-dependency cycle, which cargo supports and
  strips from the published manifest (proven by the archive verification above).

#### Fixed (documentation accuracy)

- The benchmark example now asserts the emitted live-value count for **all
  three** merge/compact lines against the file each one produced.
  The internal performance notes claimed that already while `perf_bench.rs`
  checked only the direct base+delta output.
- README and the workflow comments no longer overstate CI reproducibility.
  Actions and cargo tools are pinned; `ubuntu-latest`, `windows-latest`, and the
  `stable` toolchain are rolling by design, so a CI run is not reproducible and
  a red run on an unchanged commit is an expected outcome. `msrv (1.95.0)` is
  the only job on a fixed toolchain.
- The Clippy feature matrix is documented as the fixed list it is — no-default,
  default, each optional feature alone, and all-features — not as every
  combinatorial subset. A defect needing a specific pair or triple of features
  is not covered, and the workflow now says so.
- The stale intermediate statement in this changelog that named `VMAT` layout
  version 2 as the shipped matrix layout is corrected to version 4, with the
  within-release progression recorded.
- The internal performance and matrix storage design notes no longer describe
  the pre-v3 full logical scan as the allocation-map fallback. When the
  filesystem cannot answer an allocated-range query, enumeration falls back to
  the persisted page index and still costs `O(live pages)`.

#### Fixed (documentation accuracy, re-verification pass)

- The retracted bytes-written cost claim for matrix open, and the retracted
  read-every-page description of the no-allocation-map case, are gone from the
  two places the previous pass missed: the internal matrix storage design notes
  and the `crates/varve-core/src/matrix.rs`
  rustdoc that `cargo doc` publishes. Both now state the shipped contract —
  `O(Q)` over the union of the page index and the allocation map, `O(live
  pages)` with no allocation map, and no full logical scan since layout
  version 3.
- `VMAT` version drift in current-state text is corrected. The internal matrix
  storage design notes' header section (heading, endianness sentence,
  and the `layout_version u16` field in the diagram), `docs/spec.md`, and
  `docs/migration-guide.md` said version 3 while the code writes and enforces
  version 4; `docs/spec.md` also listed only versions 1 and 2 as refused. The
  two contradictory statements inside this file's own 0.4.0 section — a
  `VMAT` version 3 breaking entry, whose `FormatVersionMismatch` expectation was
  the superseded version, alongside the correct version 4 entry — are retracted
  in place rather than left to be read as current.
- `crates/varve/tests/doc_claims.rs` now enforces all of the above as a test:
  every retracted phrase is forbidden across the docs, the changelog, the README
  and the crate sources, and the layout version stated in the documentation is
  compared against `VMAT_VERSION` in the source. None of this was previously
  caught by any test or CI gate, which is why the same false claims survived a
  correction pass. The version check compares the *shape* of each current-state
  claim rather than a list of superseded numerals, so documentation that runs
  ahead of the code fails the same way documentation left behind by a bump does,
  and a second uncorrected copy of a claim in an already-corrected file is
  reported by path and line. A companion test forbids more than one published
  `FormatVersionMismatch` contract for `VMAT`: in any prose paragraph that
  discusses `VMAT`, every `expected: N` must be the version the code enforces,
  and the rendered refusal list is generated from `VMAT_VERSION` rather than
  written out by hand.
- The security-hardening spec and its companion validation record state their
  scope. Both are records of one completed pass at a pinned baseline commit and
  describe `VMAT` v1 as current; each now says so at the top and points at
  `docs/spec.md` for the shipped layout.

#### Fixed (matrix zeroing accounting)

- A whole-category clear zeroes its page-digest array with an explicit per-page
  loop rather than through `zero_range`, and that loop did not record the
  streaming outcome. A clear whose digest array streamed but whose final range
  was removed therefore reported zero streamed bytes. The digest path now
  records both outcomes, so
  `MatrixRecoveryReport::matrix_total_zero_range_streamed_bytes()` accounts for
  every range a clear zeroes.
- The zero-range accessors now document their exact scope: `..._last_...`
  reports one range *request*, not one operation, and both counters are
  thread-local. The internal performance notes carried the "nonzero exactly when
  the streaming fallback ran" wording without either qualification; they now direct
  callers to the before/after delta of the cumulative counter, sampled on the
  thread that performed the operation.

## 0.3.0 - 2026-07-17

### Added

- Runtime `ResourceLimits` policy APIs that may raise or lower optional format
  defaults for a specific open or create operation.
- Sequence-preserving `replace_block` and generated typed replacement methods
  for native records whose encoded size grows or shrinks.
- Replacement validation for keyed identity, snapshots, offset chains,
  checkpoints, CRC/footer records, transaction visibility, and publication
  failures.

### Changed

- `limits { ... }` is optional and may be partial. It supplies operational
  defaults only and is not a wire-format or schema ceiling.
- Standard append-log totals are uncapped: file length, scan bytes, record
  count, segment count, and index bytes default to `u64::MAX`.
- Finite standard limits remain on one-shot payload decoding, decompression,
  materialization, mmap, sidecar, and matrix allocation surfaces.
- Ordinary native, custom-layout, merge, and compact entrypoints resolve the
  same runtime policy, including low-level reader and writer constructors.
- All owned read results now validate complete extents and finite one-shot
  limits before allocation. Nested codec decoding shares one materialization
  budget, adapter cursors account owned values cumulatively, custom-layout
  headers and payload reads honor it, and matrix CRC/zero scans stream through a
  fixed buffer.
- The TDMS byte-backed example captures and bounds an exact file extent instead
  of reading to a moving EOF.

### Compatibility

- Existing complete `limits` declarations remain accepted, and legacy
  `*_with_limits` methods retain tightening-only behavior.
- Runtime limits are not persisted and do not change existing valid wire bytes
  or schema hashes. No file migration is required from 0.2 solely for this
  release.

## 0.2.0 - 2026-07-11

### Added

- Required declaration-time `ReadLimits` with generated bounded open/read APIs
  and visibly named trusted-unbounded escape hatches.
- Open-object snapshot reads for native and custom layouts, preserving file
  identity and validated logical EOF across pathname replacement.
- Matrix, sidecar, mmap, index, scan, payload, and cumulative materialization
  limits enforced before claim-sized allocation.
- ASan-backed arbitrary-byte fuzz targets for native, codec, custom-layout, and
  matrix-plus-sidecar paths, plus strict Miri and Windows `ReplaceFileW` fault
  tests.
- `PublishedButRebindFailed` for the explicit state where copy-on-write
  publication succeeded but the writer could not bind to the new generation.

### Changed

- `replace_fixed` now uses copy-on-write publication. The former safe
  `ReplaceStrategy::FixedInPlace` variant was removed; the lower-copy operation
  is available only through `unsafe replace_fixed_in_place_exclusive`.
- Canonical decoding now rejects duplicate variable fields, nonzero field
  flags, noncanonical map order, duplicate map keys, and trailing internal
  envelope bytes.
- `HashMap<K, V>` decoding requires `K: Ord` so canonical key order can be
  validated.
- Native recovery distinguishes incomplete tails from fatal corruption and
  resource-limit failures.
- Merge and compact use bounded snapshot reads and shared atomic publication.

### Compatibility

- Existing valid 0.1 native wire bytes and schema hashes are preserved; no file
  migration is required solely for this release.
- Source users must add a complete `limits` declaration or `ReadLimits`, migrate
  fixed replacement calls, and satisfy the canonical map bound where relevant.
  See [Migration Guide](docs/migration-guide.md).
