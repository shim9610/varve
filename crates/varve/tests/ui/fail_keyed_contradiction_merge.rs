use std::path::Path;

use varve::{
    BlockKind, Decoder, Encoder, Endian, FormatSpec, Result, VarveBlock, VarveDecode, VarveEncode,
    VarveKeyedBlock, VarveMerge, WireType, compact_keyed_file,
};

#[derive(Clone)]
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
    const SCHEMA_FINGERPRINT: u64 = 0x436f_6e74_7261_6432;
}

impl VarveKeyedBlock for ContradictoryKeyed {
    type Key = u64;

    fn key(&self) -> Self::Key {
        self.0
    }
}

impl VarveMerge for ContradictoryKeyed {
    type Op = u64;

    fn apply_op(&mut self, op: u64) -> Result<()> {
        self.0 = op;
        Ok(())
    }
}

fn compact_probe(spec: FormatSpec, input: &Path, output: &Path) -> Result<()> {
    compact_keyed_file::<ContradictoryKeyed, &Path>(spec, input, output)
}

fn main() {
    // Reify the merge/compaction keyed entry point exactly like a real
    // caller would; the KeyedBlockContract const inside compact_keyed_file
    // then fails to evaluate at monomorphization time (API2-03).
    let _ = compact_probe as fn(FormatSpec, &Path, &Path) -> Result<()>;
}
