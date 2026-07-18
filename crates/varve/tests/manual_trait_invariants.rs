//! Runtime coverage for the manual `VarveBlock` trait invariants driven by
//! review findings API-02 (schema fingerprint) and API-03 (keyedness).
//!
//! The registry is process-local and first-seen per (format, block id), so
//! every test registers the generated type first before probing manual impls.

use std::fs::remove_file;
use std::path::{Path, PathBuf};

use varve::{
    BlockKind, Decoder, Encoder, Endian, Error, VarveBlock, VarveDecode, VarveEncode,
    VarveKeyedBlock, WireType, varve_format,
};

#[derive(Clone, Debug, PartialEq, VarveBlock)]
#[varve(id = 60, version = 1, kind = "fixed")]
struct Plain {
    value: u32,
}

#[derive(Clone, Debug, PartialEq, VarveBlock)]
#[varve(id = 61, version = 1, kind = "variable", key = "id")]
struct KeyedGen {
    #[varve(field_id = 1)]
    id: u64,
    #[varve(field_id = 2)]
    name: String,
}

varve_format! {
    pub struct InvariantFormat {
        magic: b"INVAR";
        version: 1;
        limits {
            file_len: 8_589_934_592;
            records: 4_000_000;
            index_bytes: 536_870_912;
            scan_bytes: 8_589_934_592;
            record_payload: 67_108_864;
            logical_payload: 268_435_456;
            materialized_bytes: 1_073_741_824;
            segments: 4_000_000;
            matrix_dimension: 16_000_000;
            matrix_cells: 16_000_000;
            matrix_bitmap: 64_000_000;
            matrix_crc: 128_000_000;
            matrix_metadata: 268_435_456;
            matrix_slot_region: 8_589_934_592;
            sidecar: 268_435_456;
            mmap: 8_589_934_592;
        }
        endian: little;
        manifest: none;
        blocks: [Plain, KeyedGen];
    }
}

/// Supported escape hatch: a manual impl that mirrors `Plain` exactly and
/// reuses its generated fingerprint instead of inventing one.
#[derive(Clone, Debug, PartialEq)]
struct PlainMirror {
    value: u32,
}

impl VarveEncode for PlainMirror {
    const WIRE_TYPE: WireType = WireType::Nested;

    fn encode_varve(&self, encoder: &mut Encoder) -> varve::Result<()> {
        self.value.encode_varve(encoder)
    }
}

impl VarveDecode for PlainMirror {
    const WIRE_TYPE: WireType = WireType::Nested;

    fn decode_varve(decoder: &mut Decoder<'_>) -> varve::Result<Self> {
        Ok(Self {
            value: u32::decode_varve(decoder)?,
        })
    }
}

impl VarveBlock for PlainMirror {
    const ID: u32 = 60;
    const VERSION: u16 = 1;
    const KIND: BlockKind = BlockKind::Fixed;
    const ENDIAN: Option<Endian> = None;
    const IS_KEYED: bool = false;
    const SCHEMA_FINGERPRINT: u64 = <Plain as VarveBlock>::SCHEMA_FINGERPRINT;
}

/// Impersonation attempt: matches `Plain`'s id/version/kind but declares a
/// different schema (u64 field), so its honest fingerprint differs.
#[derive(Clone, Debug, PartialEq)]
struct PlainImpostor {
    value: u64,
}

impl VarveEncode for PlainImpostor {
    const WIRE_TYPE: WireType = WireType::Nested;

    fn encode_varve(&self, encoder: &mut Encoder) -> varve::Result<()> {
        self.value.encode_varve(encoder)
    }
}

impl VarveDecode for PlainImpostor {
    const WIRE_TYPE: WireType = WireType::Nested;

    fn decode_varve(decoder: &mut Decoder<'_>) -> varve::Result<Self> {
        Ok(Self {
            value: u64::decode_varve(decoder)?,
        })
    }
}

impl VarveBlock for PlainImpostor {
    const ID: u32 = 60;
    const VERSION: u16 = 1;
    const KIND: BlockKind = BlockKind::Fixed;
    const ENDIAN: Option<Endian> = None;
    const IS_KEYED: bool = false;
    const SCHEMA_FINGERPRINT: u64 = 0xDEAD_BEEF_0BAD_F00D;
}

/// Keyedness backstop probe: forges the generated fingerprint but denies
/// keyedness for a block the format registered as keyed.
#[derive(Clone, Debug, PartialEq)]
struct KeyedDenier {
    id: u64,
    name: String,
}

impl VarveEncode for KeyedDenier {
    const WIRE_TYPE: WireType = WireType::Nested;

    fn encode_varve(&self, _encoder: &mut Encoder) -> varve::Result<()> {
        unreachable!("registration must fail before any encode")
    }
}

impl VarveDecode for KeyedDenier {
    const WIRE_TYPE: WireType = WireType::Nested;

    fn decode_varve(_decoder: &mut Decoder<'_>) -> varve::Result<Self> {
        unreachable!("registration must fail before any decode")
    }
}

impl VarveBlock for KeyedDenier {
    const ID: u32 = 61;
    const VERSION: u16 = 1;
    const KIND: BlockKind = BlockKind::Variable;
    const ENDIAN: Option<Endian> = None;
    const IS_KEYED: bool = false;
    const SCHEMA_FINGERPRINT: u64 = <KeyedGen as VarveBlock>::SCHEMA_FINGERPRINT;
}

/// Well-formed keyed escape hatch: mirrors `KeyedGen` and agrees on keyedness,
/// so the compile-time keyed contract and the registry both accept it.
#[derive(Clone, Debug, PartialEq)]
struct KeyedMirror {
    id: u64,
    name: String,
}

impl VarveEncode for KeyedMirror {
    const WIRE_TYPE: WireType = WireType::Nested;

    fn encode_varve(&self, encoder: &mut Encoder) -> varve::Result<()> {
        KeyedGen {
            id: self.id,
            name: self.name.clone(),
        }
        .encode_varve(encoder)
    }
}

impl VarveDecode for KeyedMirror {
    const WIRE_TYPE: WireType = WireType::Nested;

    fn decode_varve(decoder: &mut Decoder<'_>) -> varve::Result<Self> {
        let inner = KeyedGen::decode_varve(decoder)?;
        Ok(Self {
            id: inner.id,
            name: inner.name,
        })
    }
}

impl VarveBlock for KeyedMirror {
    const ID: u32 = 61;
    const VERSION: u16 = 1;
    const KIND: BlockKind = BlockKind::Variable;
    const ENDIAN: Option<Endian> = None;
    const IS_KEYED: bool = true;
    const SCHEMA_FINGERPRINT: u64 = <KeyedGen as VarveBlock>::SCHEMA_FINGERPRINT;
}

impl VarveKeyedBlock for KeyedMirror {
    type Key = u64;

    fn key(&self) -> Self::Key {
        self.id
    }
}

fn temp_path(name: &str) -> PathBuf {
    let mut path = std::env::temp_dir();
    path.push(format!(
        "varve_manual_invariants_{name}_{}.vrv",
        std::process::id()
    ));
    path
}

fn cleanup(path: &Path) {
    let _ = remove_file(path);
    let mut lock = path.as_os_str().to_os_string();
    lock.push(".lock");
    let _ = remove_file(PathBuf::from(lock));
}

/// Registers the generated types first so the process-local registry always
/// holds the generated schema as the first-seen contract.
fn register_generated(file: &varve::VarveFile) -> varve::Result<()> {
    file.blocks::<Plain>()?;
    file.blocks::<KeyedGen>()?;
    Ok(())
}

#[test]
fn correct_manual_mirror_still_registers() -> varve::Result<()> {
    let path = temp_path("mirror");
    cleanup(&path);

    {
        let mut file = InvariantFormat::create(&path)?;
        file.push(&Plain { value: 7 })?;
        file.flush()?;
    }

    let file = InvariantFormat::open_readonly(&path)?;
    register_generated(&file)?;

    let mirrored = file.blocks::<PlainMirror>()?;
    assert_eq!(mirrored.len(), 1);
    assert_eq!(mirrored.get(0)?, Some(PlainMirror { value: 7 }));

    cleanup(&path);
    Ok(())
}

#[test]
fn impostor_with_different_schema_is_rejected() -> varve::Result<()> {
    let path = temp_path("impostor");
    cleanup(&path);

    {
        let mut file = InvariantFormat::create(&path)?;
        file.push(&Plain { value: 9 })?;
        file.flush()?;
    }

    let file = InvariantFormat::open_readonly(&path)?;
    register_generated(&file)?;

    let expected = <Plain as VarveBlock>::SCHEMA_FINGERPRINT;
    match file.blocks::<PlainImpostor>() {
        Err(Error::BlockSchemaFingerprintMismatch {
            block_id,
            registered,
            declared,
        }) => {
            assert_eq!(block_id, 60);
            assert_eq!(registered, expected);
            assert_eq!(declared, 0xDEAD_BEEF_0BAD_F00D);
        }
        other => panic!("expected BlockSchemaFingerprintMismatch, got {other:?}"),
    }

    cleanup(&path);
    Ok(())
}

#[test]
fn keyedness_denial_is_rejected_even_with_forged_fingerprint() -> varve::Result<()> {
    let path = temp_path("keyed_denier");
    cleanup(&path);

    {
        let mut file = InvariantFormat::create(&path)?;
        file.push(&KeyedGen {
            id: 1,
            name: "alpha".to_string(),
        })?;
        file.flush()?;
    }

    let file = InvariantFormat::open_readonly(&path)?;
    register_generated(&file)?;

    match file.blocks::<KeyedDenier>() {
        Err(Error::BlockKeyednessMismatch {
            block_id,
            registered,
            declared,
        }) => {
            assert_eq!(block_id, 61);
            assert!(registered);
            assert!(!declared);
        }
        other => panic!("expected BlockKeyednessMismatch, got {other:?}"),
    }

    cleanup(&path);
    Ok(())
}

#[test]
fn keyed_mirror_with_agreeing_keyedness_registers_and_reads() -> varve::Result<()> {
    let path = temp_path("keyed_mirror");
    cleanup(&path);

    {
        let mut file = InvariantFormat::create(&path)?;
        file.push(&KeyedGen {
            id: 5,
            name: "beta".to_string(),
        })?;
        file.flush()?;
    }

    let file = InvariantFormat::open_readonly(&path)?;
    register_generated(&file)?;

    let keyed = file.keyed_blocks::<KeyedMirror>()?;
    assert_eq!(keyed.len(), 1);
    assert_eq!(
        keyed.get(&5)?,
        Some(KeyedMirror {
            id: 5,
            name: "beta".to_string(),
        })
    );

    cleanup(&path);
    Ok(())
}
