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

Every built-in codec already declares one, so this is a rule about *your* types,
not a gap you have to work around. Scalars declare leaf identities; containers —
`Option`, `Vec`, arrays, tuples, `BTreeMap`, `HashMap` — fold their elements'
identities together with a tag and arity; and the two value-level helpers do the
same: `ChunkedBytes` folds the `chunked_bytes` tag, its container-format version,
and `Vec<u8>`'s identity, and `PackedBitmap` folds the `packed_bitmap` tag with
the `u64` bit length and `Vec<u8>` payload it emits. Both are const expressions
over compile-time constants, so they are stable across builds and platforms, and
both work as ordinary derived variable fields.

Note the version component in `ChunkedBytes`: folding the container-format
version into the identity means bumping that version changes the identity rather
than silently reusing it. That is a good pattern to copy for any codec whose
framing carries its own version.

Enum-like values should use this same explicit pattern. Pick a stable
integer or string representation, reject unknown discriminants unless they are
part of the format contract, and bump the block or field-level semantic version
when the meaning changes.

## Charge Every Owned Allocation (mandatory)

A decoder that allocates *anything whose size comes from the input* must charge
those bytes to the materialization budget with
`Decoder::charge_materialization`, **before** it reserves or allocates them.

This is an obligation on you, not a guarantee the library can enforce.
`VarveDecode` is a plain trait: nothing in it can force a custom implementation
to call `charge_materialization`, so a custom codec that skips the call
allocates outside every configured `ReadLimits` ceiling. Custom codecs are
trusted format-author code — the same trust level as the format declaration
itself — so this is a rule you follow, not a sandbox that contains you. Every
built-in codec already follows it; treat that as the standard your codec is
held to, not as a safety net that also covers yours.

The order matters as much as the call. Charging *after* the allocation makes
the limit a report rather than a ceiling: the memory is already taken by the
time the refusal is returned. The sequence is always

1. read the length or count;
2. reject an extent the remaining input cannot possibly contain;
3. `charge_materialization(bytes, "resource name")?`;
4. `try_reserve_exact` / allocate;
5. fill.

Charge the bytes the *program* will own, not the bytes on the wire. For a
`Vec<f64>` decoded from eight bytes per element those happen to match; for a
`Vec<u32>` decoded from a one-byte-per-element compact encoding they do not,
and the resident cost is what a limit exists to bound.

```rust
use std::mem::size_of;

use varve::{Decoder, Encoder, Error, VarveDecode, VarveEncode, WireType};

#[derive(Clone, Debug, PartialEq)]
struct Samples(Vec<f64>);

impl VarveEncode for Samples {
    const WIRE_TYPE: WireType = WireType::Bytes;
    const SCHEMA_ID: u64 = 0x53_41_4d_50_00_00_00_01; // "SAMP" v1

    fn encode_varve(&self, encoder: &mut Encoder) -> varve::Result<()> {
        encoder.write_u64(self.0.len() as u64);
        for value in &self.0 {
            encoder.write_u64(value.to_bits());
        }
        Ok(())
    }
}

impl VarveDecode for Samples {
    const WIRE_TYPE: WireType = WireType::Bytes;
    const SCHEMA_ID: u64 = <Samples as VarveEncode>::SCHEMA_ID;

    fn decode_varve(decoder: &mut Decoder<'_>) -> varve::Result<Self> {
        // 1. The count is attacker-controlled input, so nothing is sized from
        //    it until it has been validated.
        let count = decoder.read_len()?;
        // 2. A count the input cannot back is malformed, not an allocation
        //    request. This check alone caps `count` at the payload length.
        let wire_bytes = count
            .checked_mul(8)
            .ok_or(Error::LengthOverflow { value: u64::MAX })?;
        if wire_bytes > decoder.remaining() {
            return Err(Error::UnexpectedEof);
        }
        // 3. Charge the RESIDENT cost before taking it. A refusal here returns
        //    `Error::LimitExceeded` with nothing allocated.
        let owned_bytes = (count as u64)
            .checked_mul(size_of::<f64>() as u64)
            .ok_or(Error::ResourceArithmeticOverflow { resource: "samples" })?;
        decoder.charge_materialization(owned_bytes, "samples")?;
        // 4. Only now is the memory taken, and even then fallibly.
        let mut values = Vec::new();
        values
            .try_reserve_exact(count)
            .map_err(|_| Error::AllocationFailed {
                resource: "samples",
                requested: owned_bytes,
            })?;
        // 5. Fill.
        for _ in 0..count {
            values.push(f64::from_bits(decoder.read_u64()?));
        }
        Ok(Self(values))
    }
}
```

### Prove it with a negative self-test

A codec that charges correctly and one that does not are indistinguishable on
well-formed input: both round-trip. The difference is only visible when the
budget is smaller than the value, so write that case explicitly. Give the
decoder a materialization limit below the owned size and require a typed
refusal:

```rust
#[test]
fn samples_refuse_to_materialize_past_the_budget() {
    let bytes = varve::encode_to_vec(&Samples(vec![1.0; 64]), varve::Endian::Little)
        .expect("encode");

    // 512 owned bytes are needed; 256 are offered.
    let refused = Decoder::decode_from_slice_limited::<Samples>(
        &bytes,
        varve::Endian::Little,
        256,
    );
    assert!(
        matches!(
            refused,
            Err(Error::LimitExceeded { resource: "samples", limit: 256, .. })
        ),
        "an uncharged codec returns Ok here: {refused:?}"
    );

    // The control: the same input inside its budget must still decode, so the
    // test cannot pass because the codec is simply broken.
    Decoder::decode_from_slice_limited::<Samples>(&bytes, varve::Endian::Little, 512)
        .expect("the value fits its own budget");
}
```

Delete the `charge_materialization` line and the first assertion fails with
`Ok(..)`. That failure is the whole point of the test: it is the only automated
signal that distinguishes a charged codec from an uncharged one.

## Codec Rules

- Make encoding canonical: the same value should produce the same bytes.
- Declare `SCHEMA_ID` on both impls, and revise it whenever the bytes change.
- Honor `encoder.endian()` and `decoder.endian()` for multibyte numbers.
- Return `UnexpectedEof`, `LengthOverflow`, or another `varve::Error` instead
  of panicking on malformed input.
- Keep decode strict enough that invalid bytes do not silently become valid
  business values.
- Charge every input-sized owned allocation with
  `Decoder::charge_materialization` *before* reserving it, and cover it with a
  negative self-test — see "Charge Every Owned Allocation" above.
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
