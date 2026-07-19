use varve::{Decoder, Encoder, Result, VarveBlock, VarveDecode, VarveEncode, WireType};

// Wrapping an identity-less codec in a built-in container must not launder it
// into an identity: the container fold propagates the missing SCHEMA_ID
// outward, so the derive still rejects the field (API-04).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Packed(u64);

impl VarveEncode for Packed {
    const WIRE_TYPE: WireType = WireType::U64;

    fn encode_varve(&self, encoder: &mut Encoder) -> Result<()> {
        self.0.encode_varve(encoder)
    }
}

impl VarveDecode for Packed {
    const WIRE_TYPE: WireType = WireType::U64;

    fn decode_varve(decoder: &mut Decoder<'_>) -> Result<Self> {
        Ok(Self(u64::decode_varve(decoder)?))
    }
}

#[derive(Clone, Debug, PartialEq, VarveBlock)]
#[varve(id = 1, version = 1, kind = "fixed")]
struct Outer {
    packed: [Packed; 2],
}

fn main() {}
