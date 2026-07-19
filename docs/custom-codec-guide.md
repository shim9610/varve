# Custom Codec Guide

Most block fields can use built-in codecs: scalars, `bool`, `String`,
`Vec<u8>`, selected typed vectors, fixed arrays, tuples, `Option`, `BTreeMap`,
and `HashMap`. Add a custom codec when a field type has its own stable wire
meaning.

## Implement Encode And Decode

Implement `VarveEncode` and `VarveDecode` for the field type. Use the provided
`Encoder` and `Decoder` methods so bytes stay endian-aware.

```rust
use varve::{Decoder, Encoder, VarveDecode, VarveEncode, WireType};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
struct TimestampMillis(u64);

impl VarveEncode for TimestampMillis {
    const WIRE_TYPE: WireType = WireType::U64;
    const SCHEMA_ID: u64 = 0x54_53_4d_53_00_00_00_01; // "TSMS" v1

    fn encode_varve(&self, encoder: &mut Encoder) -> varve::Result<()> {
        encoder.write_u64(self.0);
        Ok(())
    }
}

impl VarveDecode for TimestampMillis {
    const WIRE_TYPE: WireType = WireType::U64;
    const SCHEMA_ID: u64 = <TimestampMillis as VarveEncode>::SCHEMA_ID;

    fn decode_varve(decoder: &mut Decoder<'_>) -> varve::Result<Self> {
        Ok(Self(decoder.read_u64()?))
    }
}
```

The type can now be used in derived blocks:

```rust
#[derive(Clone, Debug, PartialEq, varve::VarveBlock)]
#[varve(id = 120, version = 1, kind = "variable", key = "id")]
struct Event {
    #[varve(field_id = 1)]
    id: u64,
    #[varve(field_id = 2)]
    created_at: TimestampMillis,
}
```

If the custom type is part of a key, it must also satisfy the key bounds:
`Eq + Hash + Clone + Send + Sync + 'static`.

## Declare A Schema Identity

`SCHEMA_ID` is mandatory in practice: `#[derive(VarveBlock)]` asserts at compile
time that every field type's `VarveEncode` and `VarveDecode` declare a non-zero
`SCHEMA_ID`, and a field whose codec leaves the trait default `0` fails to
compile with a message naming the field. The rule is wire-type agnostic, and
wrapping the codec in `Option`, `Vec`, an array, a map, or a tuple does not
satisfy it: container identities fold their elements, so a missing element
identity propagates outward as "no identity".

The reason is that block fingerprints previously identified a field type by its
source spelling and coarse wire type. Two hand-written codecs spelled the same
and declaring the same wire type could emit different bytes and still compute
the same schema hash. `SCHEMA_ID` is the value that separates them, and derived
blocks fold every field's encode and decode identity into
`SCHEMA_FINGERPRINT`, so identity is transitive through nesting.

Choosing a value:

- Pick something stable and unique to *this* codec — a constant derived from the
  type's fully qualified name plus a revision counter reads well.
- Change it whenever the emitted bytes change in a way readers must notice.
  That is the whole contract: `SCHEMA_ID` changes the computed schema hash, and
  a pinned computed hash then fails closed with `SchemaHashMismatch`.
- Keep the encode and decode values equal unless the two sides genuinely
  describe different wire meanings.
- Never reuse another codec's value to make an old hash keep matching.

Enum-like values should use this same explicit pattern. Pick a stable
integer or string representation, reject unknown discriminants unless they are
part of the format contract, and bump the block or field-level semantic version
when the meaning changes.

## Codec Rules

- Make encoding canonical: the same value should produce the same bytes.
- Declare `SCHEMA_ID` on both impls, and revise it whenever the bytes change.
- Honor `encoder.endian()` and `decoder.endian()` for multibyte numbers.
- Return `UnexpectedEof`, `LengthOverflow`, or another `varve::Error` instead
  of panicking on malformed input.
- Keep decode strict enough that invalid bytes do not silently become valid
  business values.
- Avoid embedding Rust memory layout unless using the explicit zero-copy API.
- For map-like custom types, sort keys before encoding if insertion order is
  not semantically meaningful.

For variable blocks, the derive macro wraps each field payload with field id,
wire type, and length. Unknown field ids with known wire types are skipped.
Unknown wire type values are malformed data.

## Raw Mmap And Zero-Copy

Custom codecs are still owned canonical decoding. Raw zero-copy is a separate
unsafe opt-in behind the `zero-copy` feature, which implies `mmap`.

Use `VarveRawFixedBlock` only for fixed blocks where every payload is a valid
instance of the Rust type in raw memory layout, with matching endian, exact
`size_of::<T>()`, and valid alignment. Variable blocks are not zero-copy
eligible in the first scaffold.

```rust
#[cfg(feature = "zero-copy")]
#[derive(
    Clone,
    Copy,
    Debug,
    PartialEq,
    varve::VarveBlock,
    varve::zerocopy::FromBytes,
    varve::zerocopy::Immutable,
    varve::zerocopy::KnownLayout,
)]
#[repr(C)]
#[varve(id = 121, version = 1, kind = "fixed")]
struct RawPoint {
    x: u32,
    y: u32,
}

#[cfg(feature = "zero-copy")]
unsafe impl varve::VarveRawFixedBlock for RawPoint {
    const RAW_ENDIAN: varve::Endian = varve::Endian::Little;
}
```

Default typed reads do not use this path. Callers opt in through unsafe raw
access:

```rust
// SAFETY: every handle and process keeps the backing file immutable and valid
// until `mmap` is dropped.
let mmap = unsafe { file.mmap_payloads()? };
let point = unsafe { mmap.raw_fixed::<RawPoint>(index) }?;
```

The mapping constructor requires backing-file immutability for the mapping's
entire lifetime. The raw view has an additional representation contract for the
returned reference lifetime.

## Performance Check

Run the ignored smoke tests when a codec affects large field payloads, map
sorting, allocation behavior, mmap payload windows, or zero-copy reads:

```powershell
cargo test -p varve --test perf_smoke -- --ignored --nocapture
cargo test -p varve --features mmap --test perf_smoke -- --ignored --nocapture
cargo test -p varve --features mmap,zero-copy --test perf_smoke -- --ignored --nocapture
```

Also add focused roundtrip and byte-stability tests for custom codecs whose
canonical representation matters.
