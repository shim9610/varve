# Migration Guide

Varve migrations are explicit. Normal typed reads reject block version
mismatches; migration code names both the source and target block types and
performs semantic conversion in Rust.

## Unreleased Pre-1.0 Wire Changes

The current unreleased changes are wire-breaking under the pre-1.0 policy;
there is no automatic migration path for these surfaces:

- The computed schema hash algorithm moved to version 3
  (`FormatSpec::SCHEMA_HASH_ALGORITHM_VERSION`, domain tag
  `varve-schema-v3`). Every computed value changes. Version 3 folds each
  field's resolved codec identity (`VarveEncode::SCHEMA_ID` /
  `VarveDecode::SCHEMA_ID`) into the block fingerprint, so two blocks whose
  field types are spelled identically but whose custom codecs emit different
  bytes no longer share a hash. Files whose header pins a v1 or v2 computed
  hash fail open with `SchemaHashMismatch`; recreate them (or re-derive and
  re-pin literal hashes in the declaration). Files with `schema_hash` omitted
  (stored 0) are unaffected because the comparison is disabled.
- `#[derive(VarveBlock)]` now rejects, at compile time, any field whose codec
  declares no `SCHEMA_ID` (the trait default `0`). Built-in scalars,
  containers and derived blocks always declare one; a hand-written codec used
  as a block field must add `const SCHEMA_ID: u64 = ...;` to both its
  `VarveEncode` and `VarveDecode` impls, choosing a value that changes
  whenever its emitted bytes change. Wrapping such a codec in
  `Option`/`Vec`/array/map/tuple does not satisfy the requirement: the
  container fold propagates the missing identity outward.
- Matrix native files gained a creation-nonce region after the native file
  header. Matrix files created before this change are refused with
  `InvalidMatrixLayout`; recreate them. Non-matrix native files are unchanged.
- The matrix sidecar envelope is version 3. Version-1/2 sidecars are refused
  as `MatrixSidecarMismatch("sidecar version")` and simply regenerate —
  sidecars are regenerable resume state, not data.
- The matrix layout is `VMAT` version 4 with an `MCRC` version 2 integrity
  region. Within this release the layout moved 1 -> 2 -> 3 -> 4; only version 4
  ships. Version 2 introduced per-page commit digests instead of one CRC per
  commit category and stopped explicitly initializing per-cell metadata at
  create; version 3 adds the persisted page-index region between the static aux
  regions and the `MCRC` region, described by two header fields appended after
  `append_log_start` (so every earlier field keeps its index) with the reserved
  tail shrinking from 32 to 16 bytes. Layout version 4 then changed the
  page-index encoding: the region gained a validated occupancy header slot and
  became `(page_count + 1) * 8` bytes, entry count comes from that header rather
  than from a terminator scan, and entries are released when a page empties. A
  version 1, 2, or 3 matrix file is refused with
  `Error::FormatVersionMismatch { expected: 4, actual: <1, 2, or 3> }` and must
  be recreated; region lengths and total matrix file length change, so there is
  no in-place fixup.
- Matrix open no longer walks every logical bitmap page: the pages it visits come
  from the page index unioned with the filesystem allocated-range map, merged in
  one linear pass with no sort. Recorded trade-off — where no allocation map is
  available, a stray byte written out of band into a page the matrix never
  published is no longer detected at open.
- A damaged page-index entry is no longer silent. In layout version 3 a zeroed
  entry terminated enumeration and hid every later committed page; in version 4
  it is a `Fatal` `MatrixCorruptionKind::CommitMap` finding and enumeration
  continues past it. Files that previously opened "clean" with a damaged index
  will now report a fatal finding, which is the correct outcome.
- An interrupted whole-map commit rebuild is now a fail-closed state instead of
  a silent one. `rebuild_matrix_commit_from_crc` publishes `u64::MAX` into the
  page-index occupancy header and syncs it before clearing the entry region,
  and replaces it with the real count only once every entry, digest and page is
  durable. A matrix left holding the marker opens with a `Fatal`
  `MatrixCorruptionKind::CommitMap` finding recommending `RebuildCommitMap`,
  never as a short index that silently hides committed pages. This needs no
  layout-version bump — the marker is never a resting state, and a reader that
  predates it rejects it as a damaged header, which is also fail-closed. Acting
  on the recommendation requires `FormatSpec::with_matrix_fatal_forensics()`,
  since fatal findings are fail-closed by default.
- A matrix whose bitmap page count would exceed `2^48 - 1` (about 1 EiB of
  bitmap) is refused at layout time with `Error::InvalidMatrixLayout`. No
  realistic configuration reaches this.
- `PackedBitmap::new`/`get`/`set` and its decode now report
  `Error::InvalidCanonicalEncoding` / `Error::LengthOverflow` and a
  non-matrix allocation resource instead of `Error::InvalidMatrixLayout`, since
  it is an ordinary variable-field codec. Callers matching on
  `InvalidMatrixLayout` for these cases must update.
- A matrix field may now be spelled as a **type alias** of a supported scalar
  (`type Word = u32;`). Eligibility was previously decided by a whitelist of
  literal primitive spellings, which rejected aliases even though the generated
  `SLOT_STRIDE` would have accepted them. Nothing that compiled before stops
  compiling.
- `KeyedMergeEstimate::peak_resident_bytes()` is **renamed** to
  `peak_resident_structural_bytes()` and is documented as a structural estimate
  rather than an upper bound, which is what it always was; its value now also
  includes the output vector reserved while the merge state is alive. The new
  public field `max_output_values_bytes` reports that term. There is no
  compiling alias, deliberately: the old name's contract was false and callers
  must re-read the sizing text rather than silently keep a wrong guarantee.
- `PackedBitmap` is no longer accepted as a fixed-width matrix field (it has no
  encoded width fixed by its type). It remains usable as a variable field, and
  both it and `ChunkedBytes` now declare the stable non-zero codec `SCHEMA_ID`
  that derived fields require — before that they could not be used as derived
  fields at all.
- A `VarveBlock` whose `ENDIAN`, resolved through `FormatSpec::endian`,
  contradicts the format's own declaration for that block id is now rejected at
  typed registration with `Error::EndianMismatch`. Manual mirrors of a generated
  block must copy its `ENDIAN` alongside its `SCHEMA_FINGERPRINT`.
- The first `sync`/`commit_durable` on a handle that *created* its pathname now
  also fsyncs the parent directory, once per created file, and can return
  `Error::PublishedButParentSyncPending` where it previously returned `Ok(())`.
  Handles that opened an existing pathname are unchanged.
- Decoding a very large `HashMap` under a tight explicit materialization limit
  can now fail with `Error::LimitExceeded`: the charge is the hash table actually
  allocated rather than a per-entry model that under-counted it.
- Stream and disk-indexed primaries now begin with an internal creation-nonce
  record. Existing primaries keep working, but record counts, sequence numbers,
  and record offsets of newly created primaries shift by one relative to 0.3.0;
  tests or tools that assert absolute counts on a freshly created stream or
  indexed file must account for it.
- The disk-index sidecar metadata record is version 3 and the plan digest
  domain was bumped. A version 2 sidecar is refused as
  `DiskIndexError::MetadataLength` rather than `MetadataVersion`, because
  `decode_metadata` checks the record length before magic and version and the
  record grew from 260 to 300 bytes; a sidecar published against an older plan
  digest is refused as stale. Both recover with `rebuild_disk_index`. This is
  hypothetical: `disk_index.rs` is new in 0.4.0, so no released version wrote a
  v2 sidecar.
- Decoding now charges the materialization budget 8 bytes for each distinct
  variable field id above 63. A caller that sized a materialization budget to
  the exact payload byte count *and* uses field ids above 63 must add that
  allowance or it will see
  `Error::LimitExceeded { resource: "variable field ids" }`.
- Generic `push`/`push_info` on a `keyed_offset_chain` format now refuses keyed
  blocks (see the Changelog Breaking entry); switch those calls to
  `push_keyed`/`push_keyed_info`.

## Move Limits To Handle Creation

`limits { ... }` is no longer mandatory and may be partial. Resource policy is
resolved when a reader or writer is opened. Ordinary handles use
`ReadLimits::STANDARD`, which leaves total append length, record count, segment
count, scan length, and cumulative index bytes uncapped while bounding
one-shot payload and materialization work.

Existing `*_with_limits` calls remain tightening-only. Use the new
`*_with_resource_limits` family when runtime policy must raise or lower an
optional format default.

Code that constructs `FormatSpec` with a public struct literal must add the new
`read_limits` field. Prefer `FormatSpec::new(...)`, `FormatSpec::builder()`, or
the generated `Format::spec()` method so future policy additions do not require
editing a literal.

Use visibly named `*_trusted_unbounded` methods only when the complete input
provenance is under your control. Omission now selects the standard runtime
policy rather than causing a compile error.

## Migrate To Resized Replacement

Generated writers now expose `replace_<block>(index, &value)`. The encoded
payload may grow or shrink. Native replacement preserves the target sequence,
rebuilds headers, CRCs, checkpoints, and offset-chain footers, and atomically
publishes a new file generation. Keyed replacement must preserve the key.

## Migrate Fixed Replacement

`ReplaceStrategy::FixedInPlace` was removed. Use
`ReplaceStrategy::FixedCopyOnWrite` or `replace_fixed` for the safe same-size
path. These publish a validated replacement generation atomically, so readers
opened before replacement continue to observe their original snapshot.

The lower-copy path is available only as
`unsafe replace_fixed_in_place_exclusive`. Its caller must prove that no reader
or writer can overlap the operation and must accept that it does not preserve
old snapshots.

## Canonical Map Decoding

Variable-field map decoding now rejects duplicate and non-canonical key order.
Consequently, decoded `HashMap<K, V>` keys require `K: Ord` in addition to the
normal codec bounds. Custom map-like codecs must enforce equivalent duplicate
and ordering rules if they accept hostile input.

## Migrate From 0.1 To 0.2

The 0.2 release intentionally changes three source contracts from 0.1:
`FormatSpec` requires `read_limits`, `ReplaceStrategy::FixedInPlace` is removed,
and `ReplaceStrategy::FixedCopyOnWrite` is added. Facade users of the re-exported
core types are source-affected even when they depend only on `varve`. These
changes avoid preserving a misleading safe in-place contract. Existing valid
native 0.1 files do not require a wire migration.

## Version Blocks Deliberately

Keep the same block id when the new type represents the same logical block, and
bump the block version when existing bytes no longer decode as the new type.

```rust
use varve::{VarveBlock, VarveMigration};

#[derive(Clone, Debug, PartialEq, VarveBlock)]
#[varve(id = 20, version = 1, kind = "fixed")]
struct CounterV1 {
    value: u32,
}

#[derive(Clone, Debug, PartialEq, VarveBlock)]
#[varve(id = 20, version = 2, kind = "fixed")]
struct CounterV2 {
    value: u32,
    doubled: u32,
}

struct CounterMigration;

impl VarveMigration<CounterV1, CounterV2> for CounterMigration {
    fn migrate(from: CounterV1) -> varve::Result<CounterV2> {
        Ok(CounterV2 {
            value: from.value,
            doubled: from.value.saturating_mul(2),
        })
    }
}
```

`From::ID` and `To::ID` must match. `blocks_migrated` reads records matching
`From::VERSION`, decodes `From`, applies the migration, and returns owned target
values.

```rust
let file = OldFormat::open_readonly("old.varve")?;
let values = file.blocks_migrated::<CounterV1, CounterV2, CounterMigration>()?;
```

## Publish Migrated Data

`blocks_migrated` does not rewrite the file. To publish a migrated file, create
an output with the new format and push the migrated values.

```rust
let old = OldFormat::open_readonly("old.varve")?;
let migrated = old.blocks_migrated::<CounterV1, CounterV2, CounterMigration>()?;

let mut out = NewFormat::create("new.varve")?;
for value in &migrated {
    out.push(value)?;
}
out.flush()?;
out.sync()?;
```

For keyed data, first decide the migration boundary:

- Migrate raw historical puts when every old record should be converted.
- Materialize old keyed state first when tombstones and ops define the intended
  final state.
- Compact before or after migration when the output should drop old ops,
  tombstones, and unrelated block ids.

## Matrix Byte Copy Scaffold

For preallocated matrix data, unchanged fixed-stride cell payloads can be copied
without decoding and re-encoding:

```rust
let mut old = OldMatrixFormat::open_reader("old.varve")?;
let mut out = NewMatrixFormat::create_writer_with_dims("new.varve", dims)?;

out.copy_matrix_cell_bytes_from::<OldCell, NewCell>(&mut old, key)?;
```

`OldCell::DIMENSIONS` and `NewCell::DIMENSIONS` must match exactly, and
`SLOT_STRIDE` must be equal. The source cell must be committed; the read path
still verifies enabled source CRC evidence. The target writer writes the bytes
into the target slot and commits the target cell. Semantic conversions,
keyspace iteration, and output publication policy remain caller-owned.

## Field Evolution

For variable blocks, preserve field ids for unchanged fields. Add new fields
with new ids and a valid default when old records may be decoded by the new
type. Use an explicit migration when a field is renamed, split, joined, deleted
without a default, or changes meaning.

Embedded manifests can help inspect what was written, but the caller's
`FormatSpec` remains authoritative for typed reads and migrations.

## Performance Check

Run the ignored smoke tests when a migration changes scan volume, materializes
large keyed state, rewrites output files, or changes codecs used by migrated
blocks:

```powershell
cargo test -p varve --test perf_smoke -- --ignored --nocapture
cargo test -p varve --features mmap --test perf_smoke -- --ignored --nocapture
cargo test -p varve --features mmap,zero-copy --test perf_smoke -- --ignored --nocapture
```

For expensive semantic conversions, add a local migration-sized case and compare
small, medium, and large data before integrating.
