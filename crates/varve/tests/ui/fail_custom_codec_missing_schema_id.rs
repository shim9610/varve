use varve::{Decoder, Encoder, Result, VarveBlock, VarveDecode, VarveEncode, WireType};

// A hand-written codec that is NOT WireType::Nested. Two crates can spell this
// type identically, declare the same wire type, and pack their u64 differently;
// without a declared SCHEMA_ID the derived block below would fingerprint them
// the same while their bytes differ (API-04).
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
    packed: Packed,
}

fn main() {}
