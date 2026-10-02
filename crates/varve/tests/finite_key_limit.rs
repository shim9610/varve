//! Compile the actual upper limit, not just the parser's count calculation.
use varve::{Encoder, Endian, VarveEncode, varve_format};

varve_format! {
    pub format Capacity {
        magic: b"KEYCAP"; version: 1; schema_hash: computed;
        blocks {
            fixed Slot(id = 1, key = [scan, frame],
                key_domain = [scan = 0..64, frame = 0..64]) {
                scan: u16, frame: u16, value: u64,
            }
        }
    }
}

#[test]
fn all_4096_combinations_roundtrip_without_runtime_registration() -> varve::Result<()> {
    assert_eq!(SlotKey::COUNT, 4096);
    assert_eq!(std::mem::size_of::<SlotKey>(), 2);
    for code in 0..4096 {
        let key = SlotKey::from_values((code / 64, code % 64))?;
        assert_eq!(key.code(), code);
        assert_eq!(SlotKey::from_code(code)?, key);
        assert_eq!(key.values(), (code / 64, code % 64));
        let mut encoder = Encoder::new(Endian::Little);
        key.encode_varve(&mut encoder)?;
        assert_eq!(encoder.into_inner(), code.to_le_bytes());
    }
    assert!(SlotKey::from_values((64, 0)).is_err());
    assert!(SlotKey::from_code(4096).is_err());
    for code in 4096..=u16::MAX {
        assert!(SlotKey::from_code(code).is_err());
    }
    Ok(())
}
