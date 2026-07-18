use varve::{BlockKind, Decoder, Encoder, Endian, Result, VarveBlock, VarveDecode, VarveEncode, WireType};

struct ManualNoFingerprint(u64);

impl VarveEncode for ManualNoFingerprint {
    const WIRE_TYPE: WireType = WireType::U64;

    fn encode_varve(&self, encoder: &mut Encoder) -> Result<()> {
        self.0.encode_varve(encoder)
    }
}

impl VarveDecode for ManualNoFingerprint {
    const WIRE_TYPE: WireType = WireType::U64;

    fn decode_varve(decoder: &mut Decoder<'_>) -> Result<Self> {
        Ok(Self(u64::decode_varve(decoder)?))
    }
}

// A manual impl may no longer omit the schema fingerprint: without it a safe
// manual block could impersonate a registered generated type by matching only
// id/version/kind.
impl VarveBlock for ManualNoFingerprint {
    const ID: u32 = 1;
    const VERSION: u16 = 1;
    const KIND: BlockKind = BlockKind::Fixed;
    const ENDIAN: Option<Endian> = None;
    const IS_KEYED: bool = false;
}

fn main() {}
