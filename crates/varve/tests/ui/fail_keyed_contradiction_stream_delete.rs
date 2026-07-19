use varve::{
    AppendInfo, BlockKind, Decoder, Encoder, Endian, Result, VarveBlock, VarveDecode, VarveEncode,
    VarveKeyedBlock, VarveStreamWriter, WireType,
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
    const SCHEMA_FINGERPRINT: u64 = 0x436f_6e74_7261_6433;
}

impl VarveKeyedBlock for ContradictoryKeyed {
    type Key = u64;

    fn key(&self) -> Self::Key {
        self.0
    }
}

fn low_level_delete(writer: &mut VarveStreamWriter) -> Result<AppendInfo> {
    writer.delete_with_prev_key_info::<ContradictoryKeyed>(&0u64, None)
}

fn main() {
    // Reify the low-level stream delete path exactly like a real caller
    // would; the KeyedBlockContract const inside delete_with_prev_key_info
    // then fails to evaluate at monomorphization time (API2-03).
    let _ = low_level_delete as fn(&mut VarveStreamWriter) -> Result<AppendInfo>;
}
