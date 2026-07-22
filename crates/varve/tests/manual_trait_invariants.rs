//! Runtime coverage for the manual `VarveBlock` trait invariants driven by
//! review findings API-02 (schema fingerprint), API-03 (keyedness), and the
//! 2026-07-20 review's API-01 (authoritative block endian) and API-02
//! (contract-cache identity).
//!
//! Where a format declares block identities — every format `varve_format!`
//! emits — that declaration is authoritative and call order is irrelevant, so
//! those tests probe manual impls before *and* after the generated type
//! registers. Only formats that declare no identity for a block id keep the
//! process-local first-use contract, and the last two tests pin exactly which
//! `(descriptor table, identity table, block id)` triples that cache treats as
//! the same format.

use std::fs::remove_file;
use std::path::{Path, PathBuf};

use varve::{
    BlockDescriptor, BlockKind, Decoder, Encoder, Endian, Error, FormatSpec, IndexPolicy,
    IntegrityPolicy, ManifestPolicy, MatrixDimensions, MatrixKey, ReadLimits, RecoveryPolicy,
    VarveBlock, VarveDecode, VarveEncode, VarveFile, VarveKeyedBlock, VarveMatrixBlock, WireType,
    varve_format,
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

    let reader = InvariantMatrixFormat::spec().open_reader(&path)?;
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

#[derive(Clone, Debug, PartialEq, VarveBlock)]
#[varve(id = 63, version = 1, kind = "fixed")]
struct Ordered {
    value: u32,
}

varve_format! {
    pub struct OrderInvariantFormat {
        magic: b"ORDIN";
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
        blocks: [Ordered];
    }
}

/// API-01 impostor: same id/version/kind as `Ordered`, different schema, and
/// used before the generated type ever registers block 63.
#[derive(Clone, Debug, PartialEq)]
struct OrderedImpostor {
    value: u64,
}

impl VarveEncode for OrderedImpostor {
    const WIRE_TYPE: WireType = WireType::Nested;
    const SCHEMA_ID: u64 = 0x0BAD_0DE0_0000_0001;

    fn encode_varve(&self, encoder: &mut Encoder) -> varve::Result<()> {
        self.value.encode_varve(encoder)
    }
}

impl VarveDecode for OrderedImpostor {
    const WIRE_TYPE: WireType = WireType::Nested;
    const SCHEMA_ID: u64 = 0x0BAD_0DE0_0000_0001;

    fn decode_varve(decoder: &mut Decoder<'_>) -> varve::Result<Self> {
        Ok(Self {
            value: u64::decode_varve(decoder)?,
        })
    }
}

impl VarveBlock for OrderedImpostor {
    const ID: u32 = 63;
    const VERSION: u16 = 1;
    const KIND: BlockKind = BlockKind::Fixed;
    const ENDIAN: Option<Endian> = None;
    const IS_KEYED: bool = false;
    const SCHEMA_FINGERPRINT: u64 = 0x0BAD_0001_0BAD_0001;
}

/// API-01: the format's immutable identity decides which type owns a block
/// id, not whichever generic `T` happened to reach the registry first. Block
/// 63 is touched by this test alone, so the impostor genuinely registers
/// first — and is still rejected on both the write and the read path, after
/// which the generated type works normally.
#[test]
fn impostor_registered_first_is_rejected_against_the_format_identity() -> varve::Result<()> {
    let path = temp_path("order_impostor_first");
    cleanup(&path);

    let mut file = OrderInvariantFormat::create(&path)?;

    let expected = <Ordered as VarveBlock>::SCHEMA_FINGERPRINT;
    let assert_rejected = |result: varve::Result<()>| match result {
        Err(Error::BlockSchemaFingerprintMismatch {
            block_id,
            registered,
            declared,
        }) => {
            assert_eq!(block_id, 63);
            assert_eq!(registered, expected);
            assert_eq!(declared, 0x0BAD_0001_0BAD_0001);
        }
        other => panic!("expected BlockSchemaFingerprintMismatch, got {other:?}"),
    };

    assert_rejected(file.push(&OrderedImpostor { value: 1 }).map(|_| ()));
    assert_rejected(file.blocks::<OrderedImpostor>().map(|_| ()));

    // The legitimate generated type is unaffected by the failed first use.
    file.push(&Ordered { value: 7 })?;
    file.flush()?;
    assert_eq!(
        file.blocks::<Ordered>()?.get(0)?,
        Some(Ordered { value: 7 })
    );
    drop(file);

    // Reopening keeps the same verdict in both directions.
    let reopened = OrderInvariantFormat::open_readonly(&path)?;
    assert_eq!(
        reopened.blocks::<Ordered>()?.get(0)?,
        Some(Ordered { value: 7 })
    );
    assert_rejected(reopened.blocks::<OrderedImpostor>().map(|_| ()));

    cleanup(&path);
    Ok(())
}

// ---------------------------------------------------------------------------
// API-01: the authoritative block endian is part of the registration contract
// ---------------------------------------------------------------------------

/// Generated block that overrides the format endian. The format below is
/// little-endian, so `Some(Big)` here is a real, observable override: the
/// block's payload bytes are byte-swapped relative to every other block.
#[derive(Clone, Debug, PartialEq, VarveBlock)]
#[varve(id = 64, version = 1, kind = "fixed", endian = "big")]
struct BigCell {
    value: u32,
}

varve_format! {
    pub struct EndianInvariantFormat {
        magic: b"ENDIN";
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
        blocks: [BigCell];
    }
}

/// The byte-swap probe. Same id, version, kind, and -- copied verbatim, the
/// supported escape hatch -- the same generated schema fingerprint as
/// [`BigCell`]. It differs only in declaring no endian override, which
/// resolves to the format's little endian.
///
/// Before API-01 this registered successfully and decoded `BigCell` payloads
/// with the wrong byte order.
#[derive(Clone, Debug, PartialEq)]
struct BigCellInheritedEndian {
    value: u32,
}

impl VarveEncode for BigCellInheritedEndian {
    const WIRE_TYPE: WireType = WireType::Nested;

    fn encode_varve(&self, encoder: &mut Encoder) -> varve::Result<()> {
        self.value.encode_varve(encoder)
    }
}

impl VarveDecode for BigCellInheritedEndian {
    const WIRE_TYPE: WireType = WireType::Nested;

    fn decode_varve(decoder: &mut Decoder<'_>) -> varve::Result<Self> {
        Ok(Self {
            value: u32::decode_varve(decoder)?,
        })
    }
}

impl VarveBlock for BigCellInheritedEndian {
    const ID: u32 = 64;
    const VERSION: u16 = 1;
    const KIND: BlockKind = BlockKind::Fixed;
    const ENDIAN: Option<Endian> = None;
    const IS_KEYED: bool = false;
    const SCHEMA_FINGERPRINT: u64 = <BigCell as VarveBlock>::SCHEMA_FINGERPRINT;
}

/// Same probe, but naming the wrong byte order explicitly instead of
/// inheriting it. Both spellings must be rejected identically.
#[derive(Clone, Debug, PartialEq)]
struct BigCellExplicitLittle {
    value: u32,
}

impl VarveEncode for BigCellExplicitLittle {
    const WIRE_TYPE: WireType = WireType::Nested;

    fn encode_varve(&self, encoder: &mut Encoder) -> varve::Result<()> {
        self.value.encode_varve(encoder)
    }
}

impl VarveDecode for BigCellExplicitLittle {
    const WIRE_TYPE: WireType = WireType::Nested;

    fn decode_varve(decoder: &mut Decoder<'_>) -> varve::Result<Self> {
        Ok(Self {
            value: u32::decode_varve(decoder)?,
        })
    }
}

impl VarveBlock for BigCellExplicitLittle {
    const ID: u32 = 64;
    const VERSION: u16 = 1;
    const KIND: BlockKind = BlockKind::Fixed;
    const ENDIAN: Option<Endian> = Some(Endian::Little);
    const IS_KEYED: bool = false;
    const SCHEMA_FINGERPRINT: u64 = <BigCell as VarveBlock>::SCHEMA_FINGERPRINT;
}

/// The legitimate mirror: agrees with the authoritative override, so it keeps
/// working. This is the control that proves API-01 rejects byte-order
/// disagreement rather than manual mirrors in general.
#[derive(Clone, Debug, PartialEq)]
struct BigCellAgreeingMirror {
    value: u32,
}

impl VarveEncode for BigCellAgreeingMirror {
    const WIRE_TYPE: WireType = WireType::Nested;

    fn encode_varve(&self, encoder: &mut Encoder) -> varve::Result<()> {
        self.value.encode_varve(encoder)
    }
}

impl VarveDecode for BigCellAgreeingMirror {
    const WIRE_TYPE: WireType = WireType::Nested;

    fn decode_varve(decoder: &mut Decoder<'_>) -> varve::Result<Self> {
        Ok(Self {
            value: u32::decode_varve(decoder)?,
        })
    }
}

impl VarveBlock for BigCellAgreeingMirror {
    const ID: u32 = 64;
    const VERSION: u16 = 1;
    const KIND: BlockKind = BlockKind::Fixed;
    const ENDIAN: Option<Endian> = Some(Endian::Big);
    const IS_KEYED: bool = false;
    const SCHEMA_FINGERPRINT: u64 = <BigCell as VarveBlock>::SCHEMA_FINGERPRINT;
}

fn assert_endian_rejected(result: varve::Result<()>) {
    match result {
        Err(Error::EndianMismatch { expected, actual }) => {
            assert_eq!(expected, Endian::Big, "authoritative block endian");
            assert_eq!(actual, Endian::Little, "resolved manual block endian");
        }
        other => panic!("expected EndianMismatch, got {other:?}"),
    }
}

/// API-01: a manual type whose resolved byte order disagrees with the
/// format's authoritative block identity is rejected at registration, on both
/// the read and the write path, and never byte-swaps a value.
#[test]
fn manual_endian_disagreement_is_rejected_and_never_byte_swaps() -> varve::Result<()> {
    // Deliberately asymmetric so a byte swap is unmistakable.
    const VALUE: u32 = 0x0102_0304;
    const SWAPPED: u32 = 0x0403_0201;
    assert_eq!(VALUE.swap_bytes(), SWAPPED);

    let path = temp_path("endian_disagreement");
    cleanup(&path);

    {
        let mut file = EndianInvariantFormat::create(&path)?;
        // The probes are rejected before the generated type ever registers
        // block 64, so this cannot be explained by first-use ordering.
        assert_endian_rejected(
            file.push(&BigCellInheritedEndian { value: VALUE })
                .map(|_| ()),
        );
        assert_endian_rejected(file.blocks::<BigCellInheritedEndian>().map(|_| ()));
        assert_endian_rejected(
            file.push(&BigCellExplicitLittle { value: VALUE })
                .map(|_| ()),
        );
        assert_endian_rejected(file.blocks::<BigCellExplicitLittle>().map(|_| ()));

        file.push(&BigCell { value: VALUE })?;
        file.flush()?;
    }

    let file = EndianInvariantFormat::open_readonly(&path)?;
    assert_eq!(
        file.blocks::<BigCell>()?.get(0)?,
        Some(BigCell { value: VALUE })
    );

    // The rejection holds after the generated type has registered, and the
    // swapped value is never produced.
    assert_endian_rejected(file.blocks::<BigCellInheritedEndian>().map(|_| ()));
    assert_endian_rejected(file.blocks::<BigCellExplicitLittle>().map(|_| ()));

    // Control: a mirror that agrees on the override reads the true value.
    let mirrored = file.blocks::<BigCellAgreeingMirror>()?;
    assert_eq!(mirrored.len(), 1);
    assert_eq!(
        mirrored.get(0)?,
        Some(BigCellAgreeingMirror { value: VALUE })
    );
    assert_ne!(
        mirrored.get(0)?,
        Some(BigCellAgreeingMirror { value: SWAPPED }),
        "a byte-swapped read must be impossible on this path"
    );

    cleanup(&path);
    Ok(())
}

// ---------------------------------------------------------------------------
// API-02: the first-use contract cache must not alias two logical formats
// ---------------------------------------------------------------------------

const CACHE_BLOCK_ID: u32 = 65;
const AUTHORITATIVE_CACHE_FINGERPRINT: u64 = 0x00AA_00AA_00AA_00AA;
const PROBE_CACHE_FINGERPRINT: u64 = 0x00BB_00BB_00BB_00BB;

static CACHE_BLOCKS: &[BlockDescriptor] = &[BlockDescriptor {
    id: CACHE_BLOCK_ID,
    name: "CacheProbe",
    version: 1,
    kind: BlockKind::Variable,
    fields: &[],
}];

/// One static identity array. The reviewer fixture takes an empty view and a
/// full view of *this* array: both start at the same address, so an
/// address-only cache key cannot tell them apart.
static CACHE_IDENTITIES: [(u32, Option<Endian>, bool, u64); 1] =
    [(CACHE_BLOCK_ID, None, false, AUTHORITATIVE_CACHE_FINGERPRINT)];

/// Manual type whose fingerprint disagrees with `CACHE_IDENTITIES`.
#[derive(Clone, Debug, PartialEq)]
struct CacheProbe {
    value: u64,
}

impl VarveEncode for CacheProbe {
    const WIRE_TYPE: WireType = WireType::Nested;

    fn encode_varve(&self, encoder: &mut Encoder) -> varve::Result<()> {
        self.value.encode_varve(encoder)
    }
}

impl VarveDecode for CacheProbe {
    const WIRE_TYPE: WireType = WireType::Nested;

    fn decode_varve(decoder: &mut Decoder<'_>) -> varve::Result<Self> {
        Ok(Self {
            value: u64::decode_varve(decoder)?,
        })
    }
}

impl VarveBlock for CacheProbe {
    const ID: u32 = CACHE_BLOCK_ID;
    const VERSION: u16 = 1;
    const KIND: BlockKind = BlockKind::Variable;
    const ENDIAN: Option<Endian> = None;
    const IS_KEYED: bool = false;
    const SCHEMA_FINGERPRINT: u64 = PROBE_CACHE_FINGERPRINT;
}

fn hand_built_spec(
    magic: &'static [u8],
    blocks: &'static [BlockDescriptor],
    identities: &'static [(u32, Option<Endian>, bool, u64)],
) -> FormatSpec {
    FormatSpec::new(
        magic,
        1,
        Endian::Little,
        0,
        IndexPolicy::ScanOnOpen,
        IntegrityPolicy::None,
        RecoveryPolicy::Strict,
        ManifestPolicy::None,
        blocks,
    )
    .with_read_limits(ReadLimits::STANDARD)
    .with_block_identities(identities)
}

/// API-02: an empty view and the full view of one static identity array are
/// different logical formats and must never share a cached contract.
#[test]
fn empty_and_full_identity_views_do_not_share_a_cache_entry() -> varve::Result<()> {
    let empty: &'static [(u32, Option<Endian>, bool, u64)] = &CACHE_IDENTITIES[..0];
    let full: &'static [(u32, Option<Endian>, bool, u64)] = &CACHE_IDENTITIES[..];
    // The precondition the old address-only key could not survive.
    assert_eq!(empty.as_ptr(), full.as_ptr());
    assert_ne!(empty.len(), full.len());

    let without_identities = hand_built_spec(b"CACH1", CACHE_BLOCKS, empty);
    let with_identities = hand_built_spec(b"CACH1", CACHE_BLOCKS, full);
    without_identities.validate()?;
    with_identities.validate()?;

    // First: register the probe through the identity-less view. No identity
    // covers block 65 there, so the documented first-use escape hatch records
    // the probe's own fingerprint.
    let first = temp_path("cache_empty_view");
    cleanup(&first);
    {
        let file = VarveFile::create(without_identities, &first)?;
        file.blocks::<CacheProbe>()?;
    }
    cleanup(&first);

    // Then: the same block id through the full view, whose authoritative
    // identity disagrees. Reusing the first entry here was the finding.
    let second = temp_path("cache_full_view");
    cleanup(&second);
    {
        let file = VarveFile::create(with_identities, &second)?;
        match file.blocks::<CacheProbe>() {
            Err(Error::BlockSchemaFingerprintMismatch {
                block_id,
                registered,
                declared,
            }) => {
                assert_eq!(block_id, CACHE_BLOCK_ID);
                assert_eq!(registered, AUTHORITATIVE_CACHE_FINGERPRINT);
                assert_eq!(declared, PROBE_CACHE_FINGERPRINT);
            }
            other => panic!("expected BlockSchemaFingerprintMismatch, got {other:?}"),
        }
    }
    cleanup(&second);

    Ok(())
}

const PREFIX_BLOCK_ID: u32 = 66;

static PREFIX_BLOCKS: &[BlockDescriptor] = &[
    BlockDescriptor {
        id: PREFIX_BLOCK_ID,
        name: "PrefixProbe",
        version: 1,
        kind: BlockKind::Variable,
        fields: &[],
    },
    BlockDescriptor {
        id: 67,
        name: "PrefixTail",
        version: 1,
        kind: BlockKind::Variable,
        fields: &[],
    },
];

/// Two manual types for the same block id, each legitimately owning that id in
/// its own identity-less format.
macro_rules! prefix_probe {
    ($name:ident, $fingerprint:expr) => {
        #[derive(Clone, Debug, PartialEq)]
        struct $name {
            value: u64,
        }

        impl VarveEncode for $name {
            const WIRE_TYPE: WireType = WireType::Nested;

            fn encode_varve(&self, encoder: &mut Encoder) -> varve::Result<()> {
                self.value.encode_varve(encoder)
            }
        }

        impl VarveDecode for $name {
            const WIRE_TYPE: WireType = WireType::Nested;

            fn decode_varve(decoder: &mut Decoder<'_>) -> varve::Result<Self> {
                Ok(Self {
                    value: u64::decode_varve(decoder)?,
                })
            }
        }

        impl VarveBlock for $name {
            const ID: u32 = PREFIX_BLOCK_ID;
            const VERSION: u16 = 1;
            const KIND: BlockKind = BlockKind::Variable;
            const ENDIAN: Option<Endian> = None;
            const IS_KEYED: bool = false;
            const SCHEMA_FINGERPRINT: u64 = $fingerprint;
        }
    };
}

prefix_probe!(PrefixProbeShort, 0x0011_0011_0011_0011);
prefix_probe!(PrefixProbeLong, 0x0022_0022_0022_0022);
prefix_probe!(PrefixProbeIntruder, 0x0033_0033_0033_0033);

/// API-02, descriptor-table half: a one-block prefix view and the full
/// two-block view of one static descriptor array share a start address but are
/// different formats. Each keeps its own first-use contract -- and each still
/// enforces it against a third type.
#[test]
fn descriptor_prefix_views_keep_independent_cache_entries() -> varve::Result<()> {
    let short: &'static [BlockDescriptor] = &PREFIX_BLOCKS[..1];
    let long: &'static [BlockDescriptor] = &PREFIX_BLOCKS[..2];
    assert_eq!(short.as_ptr(), long.as_ptr());
    assert_ne!(short.len(), long.len());

    let short_spec = hand_built_spec(b"PFX1", short, &[]);
    let long_spec = hand_built_spec(b"PFX2", long, &[]);
    short_spec.validate()?;
    long_spec.validate()?;

    let short_path = temp_path("prefix_short");
    cleanup(&short_path);
    let long_path = temp_path("prefix_long");
    cleanup(&long_path);

    {
        let short_file = VarveFile::create(short_spec, &short_path)?;
        let long_file = VarveFile::create(long_spec, &long_path)?;

        // Distinct formats, distinct first-use contracts: both succeed. Under
        // an address-only key the second collided with the first.
        short_file.blocks::<PrefixProbeShort>()?;
        long_file.blocks::<PrefixProbeLong>()?;

        // Repeating a registration is the cache-hit path and stays consistent.
        short_file.blocks::<PrefixProbeShort>()?;
        short_file.blocks::<PrefixProbeShort>()?;
        long_file.blocks::<PrefixProbeLong>()?;

        // Each format still refuses a type that disagrees with the contract it
        // actually recorded, so splitting the entries did not disable the gate.
        for (file, registered) in [
            (&short_file, 0x0011_0011_0011_0011_u64),
            (&long_file, 0x0022_0022_0022_0022_u64),
        ] {
            match file.blocks::<PrefixProbeIntruder>() {
                Err(Error::BlockSchemaFingerprintMismatch {
                    block_id,
                    registered: actual_registered,
                    declared,
                }) => {
                    assert_eq!(block_id, PREFIX_BLOCK_ID);
                    assert_eq!(actual_registered, registered);
                    assert_eq!(declared, 0x0033_0033_0033_0033);
                }
                other => panic!("expected BlockSchemaFingerprintMismatch, got {other:?}"),
            }
        }

        // A rejected type must not be cached as the contract owner.
        short_file.blocks::<PrefixProbeShort>()?;
        long_file.blocks::<PrefixProbeLong>()?;
    }

    cleanup(&short_path);
    cleanup(&long_path);
    Ok(())
}
