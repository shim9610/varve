# Adapter Toolkit Design

This document defines the Varve adapter toolkit for external binary formats. It
is not a TDMS implementation plan. TDMS is one useful stress case, but the
toolkit stays generic enough for other segmented formats such as
instrument logs, media chunks, packet captures, image containers, and columnar
binary files.

The design principle is declaration plus user definition:

- Varve declares and verifies reusable binary mechanics.
- The adapter author defines domain meaning where the format requires it.
- A TDMS adapter is one branch produced by these generic pieces, not a
  hardcoded Varve feature.

## Layer Model

```text
external file bytes
  -> Varve physical layout
  -> adapter toolkit primitives
  -> user-defined domain semantics
  -> public adapter API
```

The existing physical layout layer owns file headers, segment lead-ins,
metadata regions, raw regions, footers, finalized offsets, range reads,
streamed segment writes, and tolerant scan reports.

The adapter toolkit sits above that layer. It helps parse binary metadata,
build logical indexes over raw regions, reduce segment metadata into state, and
run adapter self-checks. It does not know TDMS object paths, NI scaling,
DAQmx, HDF export, or any other domain-specific meaning.

## Declaration And Definition Split

An adapter author should be able to declare stable mechanics and provide Rust
definitions for the parts that are not universal.

The current implementation exposes runtime declarations through Rust structs
and traits. A future macro DSL could wrap this surface, but the stable direction
is already usable directly:

```rust
struct ExamplePropertyCodec;
impl TaggedValueCodec for ExamplePropertyCodec {
    type Value = ExampleProperty;
    type TypeId = u32;
    // user-defined encode/decode
}

let mut chunks = ChunkIndexBuilder::new();
chunks.push(entry, segment_info)?;

let reduced = reduce_segments_by_ref::<ExampleReducer, _>(segments_and_metadata)?;
```

The user declares which generic mechanisms are needed and supplies codecs,
chunk builders, and reducers as ordinary Rust types. That leaves room for each
adapter to define its own domain model.

## Generic Components

### Binary Cursor And Writer

Most external binary formats repeat the same checked endian operations:
primitive reads, primitive writes, bounded slices, arrays, length-prefixed
strings, and length-prefixed blobs.

Current shape:

```rust
let mut cursor = BinaryCursor::new(bytes, Endian::Little);
let count = cursor.u32()?;
let name = cursor.len_prefixed_string::<u32>()?;
let values = cursor.array_f64(count as usize)?;

let mut writer = BinaryWriter::new(Endian::Little);
writer.u32(count)?;
writer.len_prefixed_bytes::<u32>(name.as_bytes())?;
```

Varve provides bounds checks, overflow checks, cursor position reporting,
and consistent error classification. The adapter author still decides what each
field means.

Customization points:

- endian per cursor or per field group;
- length width such as `u16`, `u32`, or `u64`;
- string validation such as UTF-8, raw bytes, or user codec;
- fixed-count arrays, count-prefixed arrays, or user-bounded arrays.

### Length-Prefixed Values

Length-prefixed bytes and strings are common enough to deserve first-class
helpers. They are independent from Varve-native variable fields.

Current helpers:

```rust
cursor.len_prefixed_bytes::<u32>()?;
cursor.len_prefixed_string::<u32>()?;
writer.len_prefixed_bytes::<u32>(bytes)?;
```

These helpers reduce adapter boilerplate without forcing a specific metadata
grammar.

### Tagged Value Codec

Many binary metadata formats encode:

```text
name + type_id + payload
```

Varve provides the dispatch pattern, but the mapping from type id to
meaning must remain user-defined.

Current trait shape:

```rust
trait TaggedValueCodec {
    type Value;
    type TypeId;

    fn decode(type_id: Self::TypeId, cursor: &mut BinaryCursor<'_>) -> Result<Self::Value>;
    fn encode(value: &Self::Value, writer: &mut BinaryWriter) -> Result<Self::TypeId>;
}
```

Varve may offer a small generic `TaggedValue` enum for common primitive cases,
but adapters should be able to replace it entirely.

Customization points:

- type id integer width;
- value enum shape;
- unknown type policy: reject, skip as bytes, or preserve opaque payload;
- timestamp or decimal semantics owned by the adapter.

### Logical Chunk Index

External formats often store raw bytes in one physical region while metadata
describes several logical streams inside that region. TDMS channels are one
example, but the pattern also applies to sensors, frames, columns, and packet
streams.

Current shape:

```rust
let mut chunks = ChunkIndexBuilder::new();
chunks.push(ChunkEntry {
    key: ChannelKey { group, channel },
    segment_index,
    byte_offset,
    byte_len,
    value_count,
    layout: ChunkLayout::Contiguous,
}, segment_info)?;
let index = chunks.finish()?;
```

The builder validates byte bounds against Varve's physical segment
information. It does not infer domain metadata by itself.

Customization points:

- key type;
- contiguous, interleaved, strided, or user-defined raw layout;
- value count and byte stride rules;
- duplicate stream policy;
- chunk ordering and conflict policy.

### Segment Reducer

Many segmented files carry incremental metadata. Later segments may extend,
replace, or reuse earlier state. TDMS `same-as-previous` is one example, but the
general mechanism is a stateful segment reducer.

Current trait shape:

```rust
trait SegmentReducer {
    type Metadata;
    type State;

    fn initial() -> Self::State;
    fn apply_segment(
        state: &mut Self::State,
        segment: &LayoutSegmentInfo,
        metadata: Self::Metadata,
    ) -> Result<()>;
}
```

Varve can provide iteration, diagnostics, and state snapshots. The adapter
defines what reuse, replacement, deletion, or inheritance means.

Customization points:

- complete-file state versus per-stream state;
- strict versus best-effort reduction after a damaged tail;
- unknown metadata policy;
- segment version compatibility policy.

### Sidecar Policy

Some formats pair a main file with an index or cache sidecar. TDMS `.tdms_index`
is one example, but the pattern is generic.

Current shape:

```rust
SidecarPolicy {
    extension: "idx",
    mode: SidecarMode::Optional,
    verify_main_len: true,
    verify_main_fingerprint: true,
}
```

Varve provides path derivation, basic identity checks, and diagnostics.
The adapter owns sidecar payload grammar and invalidation policy.

Customization points:

- extension or naming function;
- required, optional, or generated-on-flush sidecar;
- main-file identity fields;
- rebuild policy.

### Tail Status And Adapter Diagnostics

`inspect_layout_file_report` separates a complete physical prefix from a
terminal truncated or invalid tail. The adapter toolkit builds on that instead
of hiding it.

Adapter diagnostics combine:

- static declaration diagnostics;
- physical layout scan status;
- binary cursor parse status;
- chunk bounds status;
- reducer status;
- sidecar status.

This lets applications decide whether a failure belongs to Varve physical
mechanics, adapter declarations, user domain logic, or damaged file bytes.

## TDMS As One Instantiation

A TDMS adapter could use the toolkit like this:

- physical layout: `TDSm` lead-in, ToC mask, version, next segment offset, raw
  data offset, metadata region, raw region;
- cursor: little-endian primitives and `u32` length-prefixed strings;
- tagged value codec: TDMS property type ids mapped by a user TDMS codec;
- chunk index: channel paths mapped to raw byte ranges;
- reducer: object metadata and same-as-previous raw indexes applied to TDMS
  state;
- sidecar: optional index grammar owned by the TDMS adapter; a full TDMS
  adapter may choose `.tdms_index`, while the Varve example uses `.vtidx` to
  avoid claiming TDMS index compatibility;
- diagnostics: incomplete final segment reported from physical tail status plus
  adapter-computed channel expected/read lengths.

The TDMS-specific code remains:

- object path grammar;
- root/group/channel model;
- TDMS property value semantics;
- timestamp epoch and fractions;
- scaling and DAQmx rules;
- public API compatibility with npTDMS;
- DataFrame, HDF, or CLI export features.

This split keeps Varve reusable. A different format should be able to reuse the
same cursor, tagged value, chunk index, reducer, sidecar, and diagnostic
mechanisms with completely different domain types.

## Implemented Surface

- `BinaryCursor` and `BinaryWriter`;
- checked primitive and byte reads/writes;
- length-prefixed bytes/string helpers for `u8`, `u16`, `u32`, and `u64`;
- `TaggedValueCodec` and a small generic `TaggedValue` enum;
- generic `ChunkIndex`, `ChunkIndexEntry`, `ChunkEntry`, and
  `ChunkIndexBuilder`;
- `SegmentReducer`, `reduce_segments`, and `reduce_segments_by_ref`;
- `SidecarPolicy`, `SidecarMode`, `SidecarIdentity`, and `SidecarReport`;
- `AdapterCheckReport`, `AdapterDiagnostic`, and `AdapterTailStatus`;
- `AdapterInputFile` for path-owned or temporary byte-backed adapter inputs.

The macro DSL sketched in earlier drafts is not implemented. The current API is
the runtime layer that such a DSL would generate or call.

## Verification

- `crates/varve/tests/adapter_toolkit.rs` covers cursor/writer roundtrips,
  length prefixes, tagged values, chunk bounds, stateful reducers, sidecar
  identity, and physical tail report composition.
- `crates/varve/examples/tdms_physical/common.rs` is a thin façade over the
  split TDMS example modules. The physical adapter implementation lives in
  `tdms_physical/common/adapter.rs`, example fixture data lives in
  `tdms_physical/common/example_data.rs`, and self-verification lives in
  `tdms_physical/common/verify.rs`. The adapter implementation uses
  `BinaryCursor`, `BinaryWriter`, `TaggedValueCodec`, `ChunkIndexBuilder`, and
  `SegmentReducer`. The example writes a TDMS scalar type matrix across three
  segments, exercises a changed raw-data-index segment,
  TDMS-style `same-as-previous` raw indexes, mixed raw objects, bool/string/
  integer/float tagged properties, an adapter-owned `.vtidx` sidecar, and both
  path-backed and byte-backed adapter inputs. It also reopens existing
  TDMS-style files with `open_layout_writer` and appends another segment.
- `crates/varve/examples/bmp_physical.rs` uses `BinaryWriter`, `BinaryCursor`,
  and row-level `ChunkIndexBuilder` entries for the BMP proof.
- Python harnesses verify Varve-authored TDMS create+append with `npTDMS`,
  npTDMS-authored TDMS files after Varve append, and BMP against Pillow.
- `crates/varve/tests/perf_smoke.rs` includes an ignored adapter-toolkit smoke
  path using `BinaryWriter`, `BinaryCursor`, `ChunkIndexBuilder`, and
  `SegmentReducer`; run it with
  `cargo test -p varve --test perf_smoke -- --ignored --nocapture`.

## Expected Code Reduction

For a complex external format, the toolkit is expected to reduce mechanical
adapter code, not domain logic.

| Area | Without toolkit | With toolkit |
| --- | ---: | ---: |
| checked cursor and endian code | 800-1500 LOC | 100-300 LOC |
| length-prefixed values | 200-500 LOC | 50-150 LOC |
| tagged metadata values | 1000-2000 LOC | 600-1200 LOC |
| logical chunk index | 1000-2000 LOC | 400-900 LOC |
| tail/status diagnostics | 500-1000 LOC | 100-300 LOC |

The biggest win is not only fewer lines. It is fewer duplicated offset,
bounds, endian, and partial-tail mistakes across every external binary adapter.
