use varve::{
    BlockKind, Decoder, Encoder, Endian, KeyedBlockVec, Result, VarveBlock, VarveDecode,
    VarveEncode, VarveFile, VarveKeyedBlock, WireType,
};

struct ContradictoryKeyed(u64);

impl VarveEncode for ContradictoryKeyed {
    const WIRE_TYPE: WireType = WireType::U64;

    fn encode_varve(&self, encoder: &mut Encoder) -> Result<()> {
        self.0.encode_varve(encoder)
    }
}

impl VarveDecode for ContradictoryKeyed {
    const WIRE_TYPE: WireType = WireType::U64;

    fn decode_varve(decoder: &mut Decoder<'_>) -> Result<Self> {
        Ok(Self(u64::decode_varve(decoder)?))
    }
}

impl VarveBlock for ContradictoryKeyed {
    const ID: u32 = 1;
    const VERSION: u16 = 1;
    const KIND: BlockKind = BlockKind::Fixed;
    const ENDIAN: Option<Endian> = None;
    // Contradiction: the keyed impl below is denied here.
    const IS_KEYED: bool = false;
    const SCHEMA_FINGERPRINT: u64 = 0x436f_6e74_7261_6431;
}

impl VarveKeyedBlock for ContradictoryKeyed {
    type Key = u64;

    fn key(&self) -> Self::Key {
        self.0
    }
}

fn keyed_use(file: &VarveFile) -> Result<KeyedBlockVec<u64, ContradictoryKeyed>> {
    file.keyed_blocks::<ContradictoryKeyed>()
}

fn main() {
    // Reify the keyed entry point exactly like a real caller would; the
    // KeyedBlockContract const inside the keyed collection then fails to
    // evaluate at monomorphization time.
    let _ = keyed_use as fn(&VarveFile) -> Result<KeyedBlockVec<u64, ContradictoryKeyed>>;
}
