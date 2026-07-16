# Runtime Resource Policy And Resized Replacement

Status: implemented for Varve 0.3.0.

## Goals

- Make `varve_format!` resource limits optional rather than a wire-format
  requirement.
- Select effective resource policy when a file handle is created or opened.
- Keep hostile lengths from driving one-shot allocation or cumulative
  materialization.
- Replace native fixed or variable records even when the encoded size changes.
- Preserve existing wire bytes, schema hashes, snapshots, CRC coverage,
  checkpoints, commit visibility, and offset-chain correctness.

## Runtime Limits

`ReadLimits` remains the compatibility type. `ResourceLimits` is a public type
alias and is the preferred documentation term because the policy also applies
to writers, mmap, matrix storage, scans, and materialization.

`ReadLimits::default()` remains `ReadLimits::missing()` for source-compatible
partial-policy construction. `ReadLimits::STANDARD` is a complete fallback
policy used only when an open/create operation resolves missing fields.

Standard policy:

| Resource | Value |
| --- | ---: |
| file length | `u64::MAX` |
| scan bytes | `u64::MAX` |
| records | `u64::MAX` |
| index bytes | `u64::MAX` |
| stored record payload | `64 MiB` |
| logical record payload | `256 MiB` |
| cumulative materialization | `1 GiB` |
| segments | `u64::MAX` |
| matrix dimension | `16_000_000` |
| matrix cells | `16_000_000` |
| matrix bitmap | `64 MiB` |
| matrix CRC storage | `128 MiB` |
| matrix metadata | `256 MiB` |
| matrix slot region | `8 GiB` |
| sidecar | `256 MiB` |
| mmap | `8 GiB` |

The standard policy places no total ceiling on an append log: file length,
scan length, record count, segment count, and cumulative index bytes are all
`u64::MAX`. They use checked arithmetic and fallible incremental growth, but do
not reject a valid log merely because it kept appending. The standard policy is
therefore allocation-spike-safe, not a CPU, I/O, or total-memory quota.
Services processing hostile files should supply finite work limits at open.

Each handle captures one complete effective policy:

1. An explicit runtime override wins when its field is not `Missing`.
2. Otherwise an optional format default wins when not `Missing`.
3. Otherwise `ReadLimits::STANDARD` supplies the field.

Two constructor families have deliberately different semantics:

- Existing `*_with_limits` methods remain fieldwise tightening operations for
  compatibility.
- New `*_with_resource_limits` methods overlay runtime values and may raise or
  lower optional format defaults.

`tighten_read_limits` remains explicit. In a tightening overlay, `Missing`
means no additional constraint. `TrustedUnbounded` is accepted only by the
existing explicitly named trusted APIs; ordinary resolved policies must be
finite, although finite `u64::MAX` is valid.

No per-read or per-write policy overloads are added. Reopen/create a handle
with the desired runtime policy so every operation on that handle is coherent.

## Format DSL

The entire `limits` declaration is optional, and individual entries are
optional. Omitted entries are format defaults of `Missing` and resolve at
open/create time. Unknown and duplicate names remain compile errors.

Legacy `limits: trusted_unbounded;` remains accepted for source compatibility,
but ordinary APIs still resolve to the standard finite allocation policy.
Explicit trusted APIs are required to authorize unbounded operation.

Limits remain absent from native headers, schema hashes, and manifests. Native
record `payload_len` and variable-field length headers remain authoritative.

## Allocation Safety

Before allocation, reserve, decompression, mmap, or materialization driven by
wire data, code must perform checked extent arithmetic, validate against the
captured snapshot, apply the effective policy, and only then convert to
`usize` or grow memory. File copying, scans, checksums, and unchanged-record
rewrites remain chunked. Multi-record APIs retain a cumulative
`MaterializationBudget`.

Container decoders may grow incrementally only after proving bytes are present
inside an already policy-bounded logical record. They must not reserve directly
from an untrusted element count.

## Replacement API

The new API is additive:

```rust
pub struct ReplacementInfo {
    pub sequence: u64,
    pub record_offset: u64,
    pub old_payload_len: u64,
    pub new_payload_len: u64,
    pub old_physical_len: u64,
    pub new_physical_len: u64,
}

impl ReplacementInfo {
    pub fn translate_record_offset(&self, old: u64) -> Result<u64>;
}

pub trait VarveReplaceBlock: VarveBlock {
    fn validate_replacement(old: &Self, new: &Self) -> Result<()>;
}

pub fn replace_block<T: VarveReplaceBlock>(
    &mut self,
    index: usize,
    value: &T,
) -> Result<ReplacementInfo>;
```

`VarveBlock` derive and inline block generation implement
`VarveReplaceBlock`. Unkeyed blocks accept replacement. Keyed blocks require
the same key and return `ReplacementKeyMismatch` before publication otherwise.
Manual `VarveBlock` implementations opt into the new API by implementing the
new trait, so existing downstream implementations remain source-compatible.

The target keeps its original sequence. Replacement therefore changes value
and physical extent without making an older keyed event logically newer than a
later put, operation, or tombstone. Existing legacy replacement APIs retain
their signatures and behavior.

`ReplacementInfo::translate_record_offset` leaves offsets through the target
unchanged and applies the checked physical-length delta to later offsets. This
is valid because checkpoint entry encodings and every non-target record retain
their physical lengths. Generated keyed tail maps use this translation in
place, without allocation, after successful publication.

## Rewrite Invariants

`replace_block` applies only to native non-matrix user records. It rejects
matrix storage and custom physical layouts. It performs a same-directory,
streaming copy-on-write rewrite:

- preserve record order, target sequence, versions, flags, compression hints,
  internal records, and committed visibility;
- encode/compress the replacement under the writer's captured policy;
- stream unchanged payload bytes from the old snapshot;
- recalculate every record and payload offset;
- regenerate every checkpoint from the rebuilt prefix;
- translate every predecessor offset through the old-to-new offset map;
- encode each footer before computing its CRC;
- verify every predecessor points to an earlier compatible rewritten record;
- compute CRC over the rebuilt header/payload/footer contract;
- validate, flush, and sync the temporary generation before atomic publish;
- retain old opened snapshots on the old generation;
- remove temporary files on every pre-publication or publication failure;
- preserve the existing poisoned-writer contract after publication succeeds
  but writer rebinding fails.

Parent-directory durability remains platform-specific and follows the existing
atomic publication contract; it is not silently claimed where unsupported.

## Generated API

Every non-matrix inline block gets an inherent method and matching generated
write-trait method:

```rust
fn replace_<singular>(
    &mut self,
    index: usize,
    value: &Block,
) -> Result<ReplacementInfo>;
```

After success all generated keyed tail-map values are translated in place.
Key sets cannot change because keyed replacement validates equality first.

## Compatibility And Non-Goals

- Existing files remain readable and writable without migration.
- Existing `replace_rewrite`, `replace_fixed`, `ReplaceStrategy`, and unsafe
  exclusive replacement signatures remain available.
- Existing `*_with_limits` remains tightening-only.
- Resized in-place replacement, key-changing replacement, matrix-slot resizing,
  custom-layout replacement, and live-reader mutation are out of scope.

## Acceptance Tests

- DSL compile pass for omitted and partial limits; compile fail for unknown and
  duplicate entries.
- Standard fallback plus runtime resource overrides that raise and lower format
  defaults; legacy tightening remains fieldwise minimum.
- Hostile record/checkpoint/container lengths fail before large allocation.
- Grow, shrink, and equal-size replacement for fixed and variable blocks.
- Same-key keyed success and key-change rejection, including older put followed
  by put/op/tombstone histories.
- Header CRC, footer predecessor chains, multiple checkpoints, compression
  hints, and transaction-marker visibility survive resizing.
- Existing reader sees the old snapshot; new reader sees the replacement.
- Generated keyed append/delete after replacement uses translated tail offsets.
- Pre-publication and publication failures preserve the old generation and do
  not leak rewrite temporary files.
- Matrix/custom layout rejection is explicit.
- Performance smoke records rewrite throughput and confirms memory does not
  scale with whole-file size.
