# High-Cardinality Streaming And Disk Index Specification

Status: historical first-slice specification, superseded by
`docs/petabyte-io-final-spec.md` and `docs/scalable-io.md`. It remains useful
for the original disk-index decision record, but its scan-on-open and
post-append native revalidation protocol is no longer implemented. All APIs
remain gated by `high-cardinality-dev` and are not a v0.3 public compatibility
commitment.

## Objective

Add opt-in native handles whose resident bookkeeping is independent of total
record count and unique-key count. Provide latest put/tombstone equality lookup
through a bounded-cache on-disk index.

The primary Varve file remains authoritative. Existing APIs, native bytes,
schema hashes, manifests, and default generated behavior remain unchanged.

## Architecture decision

Release 1 uses `redb 4.1` as a derived COW B-tree sidecar. `redb` is pure Rust,
MIT OR Apache-2.0, ACID, MVCC, crash-safe, and exposes an explicit cache-size
builder. This avoids an unverified mutable hash-table wire format and preserves
reader snapshots within the sidecar.

Lookup is O(log K) sidecar page work plus one validated native-record read. The
resident bound, not a misleading strict O(1) latency claim, is the release-1
contract. A disk hash may be added only after its snapshot and crash protocol
has an independent proof and fault suite.

| Profile | Record bookkeeping | Key bookkeeping | Resident complexity |
| --- | --- | --- | --- |
| resident | existing `Vec<RecordIndexEntry>` | existing maps | O(R + K) |
| stream | cursor only | none | O(B + P) |
| indexed | cursor only | redb cache | O(B + C + P) |

`B` is declared block count, `C` is one global sidecar cache ceiling, and `P`
is the larger of the active physical/logical payload and canonical key
workspace. Cache metadata must be included in `C`; no cardinality-sized cache
directory is permitted outside the storage engine.

## DSL

Only inline keyed native fixed/variable blocks accept `key_index`:

```rust
variable Frame(
    id = 10,
    version = 1,
    key = [scan, frame],
    key_index = disk,
) {
    scan: u32,
    frame: u32,
    payload: Vec<u8>,
}
```

Values are `memory` (default) and `disk`. `key_index` without `key`, on a
matrix block, outside `high-cardinality-dev`, or on an external registry-only
block is a compile error. It is generated operational metadata and is excluded
from the native schema hash.

`VarveDiskKey` is a marker over `VarveEncode + VarveDecode + Eq + Hash + Clone`
whose safety contract requires deterministic canonical encoding and identical
encoding for `Eq` values. Macro-generated scalar/tuple keys implement it.

## Core signatures

```rust
pub struct StreamOptions {
    pub limits: ResourceLimits,
}

pub struct DiskIndexOptions {
    pub cache_bytes: usize, // global per sidecar, default 8 MiB, minimum 1 MiB
    pub limits: ResourceLimits,
}

pub struct StreamResidentState {
    pub declared_block_tails: usize,
    pub retained_record_entries: usize, // always zero
    pub retained_key_entries: usize,    // always zero
    pub active_payload_bytes: u64,
}

pub enum DiskIndexEntry {
    Missing,
    Put { record_offset: u64, sequence: u64 },
    Tombstone { record_offset: u64, sequence: u64 },
}

pub struct VarveStreamReader;
pub struct VarveStreamWriter;
pub struct StreamEvents;
pub struct StreamingBlocks<T>;
pub struct VarveIndexedReader;
pub struct VarveIndexedWriter;

impl VarveStreamReader {
    pub fn open(spec: FormatSpec, path: impl AsRef<Path>, options: StreamOptions)
        -> Result<Self>;
    pub fn events(&self) -> Result<StreamEvents>;
    pub fn blocks<T: VarveBlock>(&self) -> Result<StreamingBlocks<T>>;
    pub fn resident_state(&self) -> StreamResidentState;
}

impl VarveStreamWriter {
    pub fn create(spec: FormatSpec, path: impl AsRef<Path>, options: StreamOptions)
        -> Result<Self>;
    pub fn open(spec: FormatSpec, path: impl AsRef<Path>, options: StreamOptions)
        -> Result<Self>;
    pub fn push_info<T: VarveBlock>(&mut self, value: &T) -> Result<AppendInfo>;
    pub fn delete_with_prev_key_info<T: VarveKeyedBlock>(
        &mut self, key: &T::Key, previous: Option<u64>) -> Result<AppendInfo>;
    pub fn flush(&mut self) -> Result<()>;
    pub fn sync(&mut self) -> Result<()>;
    pub fn resident_state(&self) -> StreamResidentState;
}

impl VarveIndexedReader {
    pub fn open(spec: FormatSpec, path: impl AsRef<Path>, options: DiskIndexOptions)
        -> Result<Self>;
    pub fn lookup<T: VarveKeyedBlock>(&self, key: &T::Key)
        -> Result<DiskIndexEntry>;
    pub fn get<T: VarveKeyedBlock>(&self, key: &T::Key) -> Result<Option<T>>;
}
```

Sidecar mutation is private to `VarveIndexedWriter`; public `AppendInfo` is not
an index-update capability. Generated indexed methods perform native append and
derived-index update as one state machine.

Generated formats expose stream constructors and typed iterators. Formats with
`key_index = disk` additionally expose indexed constructors and `get_<block>`.
Existing generated resident types remain unchanged.

Iterator items are `Result<BlockEvent>` and `Result<T>`. Iterators own a cloned
`SnapshotFile` and cursor, are `Send` when their decoded `T` is `Send`, and do
not borrow the reader.

## Streaming validation

The scanner retains one `SnapshotFile`, captured logical EOF, current offset,
strict sequence state, one block tail per declared block when required, fixed
I/O scratch, and at most the current payload. It applies the same native
header/footer/checksum/compression/read-limit checks as resident scanning.

Release 1 requires strictly increasing physical sequences. Non-monotonic but
resident-valid files return `StreamingUnsupported` rather than corruption.

The measurable memory bound is:

```text
O(declared blocks)
+ global cache_bytes
+ max physical payload
+ max logical payload
+ fixed scanner/storage-engine scratch
```

Always-on tests use resident counters and a counting allocator high-water
comparison at small/medium cardinalities. The one-million-key test is ignored
by default and records RSS only as supplemental evidence.

## Sidecar data model

One `<native path>.vki` redb database covers all `disk` blocks. It contains:

- `meta`: version, schema hash, primary object fingerprint, covered logical
  EOF, last sequence, clean/dirty state, and generation;
- `latest`: canonical composite key
  `block_id_be || key_len_be || canonical_key_bytes` to fixed metadata
  `(kind, record_offset, sequence, target_block_id, crc)`.

The metadata/value codecs are byte-exact little-endian internally, validate
reserved bytes, checked lengths and CRC before use, and are unstable dev-only
formats. Sidecar structural CRC is unconditional and independent of native
`integrity`.

Put candidates must match the declared block id/version and decoded full key.
Tombstones must be the internal tombstone block/version and their envelope
target block and decoded key must match. Every offset, extent, sequence,
checksum, compression envelope, and snapshot boundary is revalidated before a
value is returned.

A clean sidecar is trusted local derived metadata for latestness. Deliberate
semantic rewriting with recomputed valid metadata/CRCs is outside the threat
model; native hostile bytes and structurally hostile sidecars remain in scope.

## Snapshot and concurrency model

redb MVCC transactions provide immutable sidecar generations. The historical
first slice proposed retry/rebuild during indexed open. The implemented
petabyte path instead validates one clean sidecar snapshot and returns a typed
error for ahead/behind/dirty state. Ordinary open never retries by scanning or
rebuilding.

Release 1 permits one indexed writer and multiple indexed readers only through
the same opened database coordinator. Cross-process or independently opened
indexed readers while a writer owns the database may return `IndexBusy`.
Ordinary native stream/resident readers remain available.

Paths are canonicalized before native and sidecar lock derivation. File
fingerprints detect stale aliases, but hardlink-alias writer exclusion is not
claimed until an OS file-identity lock is implemented.

## Recovery and durability

An indexed writer receives and retains the native writer-lock capability; its
rebuild path reuses that capability and never reacquires its own lock. A reader
may publish a shared rebuild only after acquiring the native writer lock before
snapshot capture; otherwise it returns `IndexBusy`.

The historical first slice proposed scanning each newly published batch before
sidecar commit. The implemented path derives native framing and sidecar rows
once from the same checked prepared record, writes each native chunk once, and
never rereads it during append or `sync()`. Previous-key lookup observes the
same bounded transaction, avoiding an O(K) pending map.

`flush` does not mark clean. `sync` orders native `sync_all`, durable redb data
commit, then a durable clean metadata transaction. Generation increment uses
checked arithmetic; exhaustion is a typed error. `Drop` never reports or
attempts clean publication.

In the implemented path, missing/stale sidecars require explicit bootstrap or
rebuild. Dirty state requires explicit savepoint restore, which truncates only
the uncommitted native tail to the validated durable frontier. Ordinary reader
and writer open never mutate, truncate, repair, or rebuild.

Per-handle durability and filesystem traits provide deterministic fault
injection without global test hooks.

## Supported release-1 matrix

Supported: native append log, fixed/variable puts, tombstones, composite keys,
commit none/record footer, block chains, integrity, compression, manifests and
metadata, strict/truncate-tail recovery.

Rejected before mutation with `StreamingUnsupported`: transaction markers,
checkpoint-on-flush, matrix/custom layouts, merge ops, replacement/rewrite,
compaction/migration, mmap/zero-copy, ordinal lookup, and complete index
exposure. If format-wide `keyed_offset_chain` is enabled, every keyed append
block must declare `key_index = disk`; disk lookup supplies the previous key
offset while block tails remain O(declared blocks).

## Acceptance

1. Resident and bounded writers produce byte-identical native records for
   supported policies.
2. Existing default/all-feature suites remain green with the feature disabled.
3. Small/medium counting-allocator tests and an ignored one-million-composite-
   key stress test show no cardinality-correlated retained state.
4. Latest put, overwrite, tombstone, reopen, sidecar snapshot, rebuild, and
   stale-generation behavior are correct.
5. Hostile sidecar lengths/offsets/CRCs and native truncation never panic or
   trigger claim-sized allocation.
6. Before stabilization, per-handle fault tests must cover native append,
   sidecar transaction, native sync, clean publication, atomic replacement,
   and directory sync. The current slice covers functional stale-sidecar
   rebuild but not atomic replacement or directory-sync fault injection.
7. Streaming iteration matches resident visibility and corruption errors for
   supported policies.
8. Benchmarks report resident/stream append and scan, indexed lookup latency,
   rebuild throughput, redb cache statistics, and resident counters.
9. `cargo audit` and `cargo deny` accept the optional dependency graph and
   commercial-use licenses before the feature leaves dev.
