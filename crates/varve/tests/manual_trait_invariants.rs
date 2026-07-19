//! Runtime coverage for the manual `VarveBlock` trait invariants driven by
//! review findings API-02 (schema fingerprint) and API-03 (keyedness).
//!
//! The registry is process-local and first-seen per (format, block id), so
//! every test registers the generated type first before probing manual impls.

use std::fs::remove_file;
use std::path::{Path, PathBuf};

use varve::{
    BlockKind, Decoder, Encoder, Endian, Error, MatrixDimensions, MatrixKey, VarveBlock,
    VarveDecode, VarveEncode, VarveKeyedBlock, VarveMatrixBlock, WireType, varve_format,
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

varve_format! {
    pub format InvariantMatrixFormat {
        magic: b"IMTX";
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
        dims {
            scan: u32,
            ch: u32,
        }
        commit: cell_bitmap {
            keyspace = [scan, ch];
            categories = [analysis];
        };
        blocks {
            matrix MatrixGenCell(id = 62, dims = [scan, ch], category = analysis) {
                value: u32,
            }
        }
    }
}

/// DEF-01 probe: same block id/version/kind, same dimensions, category, and
/// slot stride as the generated matrix cell — but different decode semantics
/// (i32 vs u32) and therefore an honest different fingerprint. Without the
/// common registration gate this type passed every shape check.
#[derive(Clone, Debug, PartialEq)]
struct MatrixImpostorCell {
    value: i32,
}

impl VarveEncode for MatrixImpostorCell {
    const WIRE_TYPE: WireType = WireType::Nested;

    fn encode_varve(&self, encoder: &mut Encoder) -> varve::Result<()> {
        self.value.encode_varve(encoder)
    }
}

impl VarveDecode for MatrixImpostorCell {
    const WIRE_TYPE: WireType = WireType::Nested;

    fn decode_varve(decoder: &mut Decoder<'_>) -> varve::Result<Self> {
        Ok(Self {
            value: i32::decode_varve(decoder)?,
        })
    }
}

impl VarveBlock for MatrixImpostorCell {
    const ID: u32 = <MatrixGenCell as VarveBlock>::ID;
    const VERSION: u16 = <MatrixGenCell as VarveBlock>::VERSION;
    const KIND: BlockKind = BlockKind::Matrix;
    const ENDIAN: Option<Endian> = None;
    const IS_KEYED: bool = false;
    const SCHEMA_FINGERPRINT: u64 = 0xF00D_FACE_CAFE_0001;
}

impl VarveMatrixBlock for MatrixImpostorCell {
    const DIMENSIONS: [&'static str; 2] = <MatrixGenCell as VarveMatrixBlock>::DIMENSIONS;
    const CATEGORY: &'static str = <MatrixGenCell as VarveMatrixBlock>::CATEGORY;
    const SLOT_STRIDE: u64 = <MatrixGenCell as VarveMatrixBlock>::SLOT_STRIDE;
}

struct TempPath {
    path: PathBuf,
    _dir: tempfile::TempDir,
}

impl std::ops::Deref for TempPath {
    type Target = PathBuf;

    fn deref(&self) -> &PathBuf {
        &self.path
    }
}

impl AsRef<std::path::Path> for TempPath {
    fn as_ref(&self) -> &std::path::Path {
        &self.path
    }
}

fn temp_path(name: &str) -> TempPath {
    let dir = tempfile::tempdir().expect("create per-test temp directory");
    let path = dir.path().join(format!(
        "varve_manual_invariants_{name}_{}.vrv",
        std::process::id()
    ));
    TempPath { path, _dir: dir }
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

/// Creates a matrix file with one committed generated cell, registering the
/// generated type as the first-seen contract for the matrix block id.
fn create_matrix_fixture(path: &Path) -> varve::Result<()> {
    let dims = MatrixDimensions::from_pairs([("scan", 2), ("ch", 1)]);
    let mut writer = InvariantMatrixFormat::spec().create_writer_with_dims(path, dims)?;
    writer.write_matrix_cell(MatrixKey::new(0, 0), &MatrixGenCell { value: 41 })?;
    writer.commit_matrix_cell::<MatrixGenCell>(MatrixKey::new(0, 0))?;
    writer.flush()?;
    Ok(())
}

fn assert_matrix_impostor_rejected(result: Result<(), Error>) {
    match result {
        Err(Error::BlockSchemaFingerprintMismatch {
            block_id,
            registered,
            declared,
        }) => {
            assert_eq!(block_id, <MatrixGenCell as VarveBlock>::ID);
            assert_eq!(
                registered,
                <MatrixGenCell as VarveBlock>::SCHEMA_FINGERPRINT
            );
            assert_eq!(declared, 0xF00D_FACE_CAFE_0001);
        }
        other => panic!("expected BlockSchemaFingerprintMismatch, got {other:?}"),
    }
}

/// DEF-01: a same-stride manual matrix type with a different fingerprint is
/// rejected by the common registration gate on the cell read path.
#[test]
fn matrix_impostor_same_stride_read_is_rejected() -> varve::Result<()> {
    let path = temp_path("matrix_impostor_read");
    cleanup(&path);
    create_matrix_fixture(&path)?;

    let mut reader = InvariantMatrixFormat::spec().open_reader(&path)?;
    assert_eq!(
        reader.read_matrix_cell::<MatrixGenCell>(MatrixKey::new(0, 0))?,
        MatrixGenCell { value: 41 }
    );
    assert_matrix_impostor_rejected(
        reader
            .read_matrix_cell::<MatrixImpostorCell>(MatrixKey::new(0, 0))
            .map(|_| ()),
    );

    cleanup(&path);
    Ok(())
}

/// DEF-01: the same gate guards the cell write path.
#[test]
fn matrix_impostor_same_stride_write_is_rejected() -> varve::Result<()> {
    let path = temp_path("matrix_impostor_write");
    cleanup(&path);
    create_matrix_fixture(&path)?;

    let mut writer = InvariantMatrixFormat::spec().open_writer(&path)?;
    writer.write_matrix_cell(MatrixKey::new(1, 0), &MatrixGenCell { value: 7 })?;
    assert_matrix_impostor_rejected(
        writer
            .write_matrix_cell(MatrixKey::new(1, 0), &MatrixImpostorCell { value: -7 })
            .map(|_| ()),
    );

    cleanup(&path);
    Ok(())
}

/// DEF-01: the same gate guards the mmap cell window path.
#[cfg(feature = "mmap")]
#[test]
fn matrix_impostor_same_stride_mmap_is_rejected() -> varve::Result<()> {
    let path = temp_path("matrix_impostor_mmap");
    cleanup(&path);
    create_matrix_fixture(&path)?;

    let reader = InvariantMatrixFormat::spec().open_reader(&path)?;
    // SAFETY: the file is not mutated for the mapping's lifetime; no other
    // handle, thread, or process touches this test-owned temp path.
    let mapped = unsafe { reader.mmap_matrix()? };
    assert!(
        mapped
            .cell_payload_window::<MatrixGenCell>(MatrixKey::new(0, 0))
            .is_ok()
    );
    assert_matrix_impostor_rejected(
        mapped
            .cell_payload_window::<MatrixImpostorCell>(MatrixKey::new(0, 0))
            .map(|_| ()),
    );

    cleanup(&path);
    Ok(())
}
