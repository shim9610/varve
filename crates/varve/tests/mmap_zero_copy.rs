#![cfg(feature = "mmap")]

use std::fs::remove_file;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use varve::{Endian, Error, RecordIndexEntry, VarveBlock, decode_from_slice, varve_format};

#[cfg(feature = "zero-copy")]
use varve::{BlockKind, Decoder, Encoder, VarveDecode, VarveEncode};

#[derive(Clone, Debug, PartialEq, VarveBlock)]
#[varve(id = 80, version = 1, kind = "fixed")]
struct MmapPoint {
    x: u32,
    y: u32,
}

#[derive(Clone, Debug, PartialEq, VarveBlock)]
#[varve(id = 81, version = 1, kind = "variable")]
struct MmapMessage {
    #[varve(field_id = 1)]
    id: u32,
    #[varve(field_id = 2)]
    text: String,
}

#[cfg(feature = "zero-copy")]
#[derive(
    Clone,
    Copy,
    Debug,
    PartialEq,
    VarveBlock,
    zerocopy::FromBytes,
    zerocopy::Immutable,
    zerocopy::KnownLayout,
)]
#[repr(C)]
#[varve(id = 82, version = 1, kind = "fixed")]
struct RawPoint {
    x: u32,
    y: u32,
}

#[cfg(feature = "zero-copy")]
unsafe impl varve::VarveRawFixedBlock for RawPoint {
    const RAW_ENDIAN: Endian = Endian::Little;
}

#[cfg(feature = "zero-copy")]
#[derive(
    Clone,
    Debug,
    PartialEq,
    VarveBlock,
    zerocopy::FromBytes,
    zerocopy::Immutable,
    zerocopy::KnownLayout,
)]
#[repr(C)]
#[varve(id = 83, version = 1, kind = "variable")]
struct RawVariable {
    #[varve(field_id = 1)]
    id: u32,
}

#[cfg(feature = "zero-copy")]
unsafe impl varve::VarveRawFixedBlock for RawVariable {
    const RAW_ENDIAN: Endian = Endian::Little;
}

#[cfg(feature = "zero-copy")]
#[derive(
    Clone,
    Copy,
    Debug,
    PartialEq,
    VarveBlock,
    zerocopy::FromBytes,
    zerocopy::Immutable,
    zerocopy::KnownLayout,
)]
#[repr(C)]
#[varve(id = 84, version = 1, kind = "fixed")]
struct BigEndianRawPoint {
    x: u32,
    y: u32,
}

#[cfg(feature = "zero-copy")]
unsafe impl varve::VarveRawFixedBlock for BigEndianRawPoint {
    const RAW_ENDIAN: Endian = Endian::Big;
}

#[cfg(feature = "zero-copy")]
#[derive(
    Clone, Copy, Debug, PartialEq, zerocopy::FromBytes, zerocopy::Immutable, zerocopy::KnownLayout,
)]
#[repr(C)]
struct SizeMismatchRawPoint {
    x: u32,
}

#[cfg(feature = "zero-copy")]
impl VarveEncode for SizeMismatchRawPoint {
    const WIRE_TYPE: varve::WireType = varve::WireType::Nested;

    fn encode_varve(&self, encoder: &mut Encoder) -> varve::Result<()> {
        encoder.write_u32(self.x);
        encoder.write_u32(self.x.wrapping_add(1));
        Ok(())
    }
}

#[cfg(feature = "zero-copy")]
impl VarveDecode for SizeMismatchRawPoint {
    const WIRE_TYPE: varve::WireType = varve::WireType::Nested;

    fn decode_varve(decoder: &mut Decoder<'_>) -> varve::Result<Self> {
        let x = decoder.read_u32()?;
        let _extra = decoder.read_u32()?;
        Ok(Self { x })
    }
}

#[cfg(feature = "zero-copy")]
impl VarveBlock for SizeMismatchRawPoint {
    const ID: u32 = 85;
    const VERSION: u16 = 1;
    const KIND: BlockKind = BlockKind::Fixed;
    const ENDIAN: Option<Endian> = None;
}

#[cfg(feature = "zero-copy")]
unsafe impl varve::VarveRawFixedBlock for SizeMismatchRawPoint {
    const RAW_ENDIAN: Endian = Endian::Little;
}

#[cfg(feature = "zero-copy")]
varve_format! {
    pub struct MmapFormat {
        magic: b"MZ";
        version: 1;
        endian: little;
        blocks: [
            MmapPoint,
            MmapMessage,
            RawPoint,
            RawVariable,
            BigEndianRawPoint,
            SizeMismatchRawPoint
        ];
    }
}

#[cfg(not(feature = "zero-copy"))]
varve_format! {
    pub struct MmapFormat {
        magic: b"MZ";
        version: 1;
        endian: little;
        blocks: [MmapPoint, MmapMessage];
    }
}

#[test]
fn mmap_payload_windows_match_indexed_payload_reads() -> varve::Result<()> {
    let path = temp_path("payload_windows");
    cleanup(&path);

    {
        let mut file = MmapFormat::create(&path)?;
        file.push(&MmapPoint { x: 10, y: 20 })?;
        file.push(&MmapMessage {
            id: 7,
            text: "hello mmap".to_string(),
        })?;
        file.push(&MmapPoint { x: 30, y: 40 })?;
        file.flush()?;
    }

    let file = MmapFormat::open_readonly(&path)?;
    let mmap = file.mmap_payloads()?;

    for entry in file.index_entries() {
        assert_eq!(mmap.payload_window(entry)?, entry.read_payload(&path)?);
    }

    let first = mmap
        .block_payload_window::<MmapPoint>(0)?
        .expect("first fixed payload");
    assert_eq!(
        decode_from_slice::<MmapPoint>(first, Endian::Little)?,
        MmapPoint { x: 10, y: 20 }
    );

    let second = mmap
        .block_payload_window::<MmapPoint>(1)?
        .expect("second fixed payload");
    assert_eq!(
        decode_from_slice::<MmapPoint>(second, Endian::Little)?,
        MmapPoint { x: 30, y: 40 }
    );
    assert!(mmap.block_payload_window::<MmapPoint>(2)?.is_none());

    let variable = mmap
        .block_payload_window::<MmapMessage>(0)?
        .expect("variable payload");
    assert_eq!(
        decode_from_slice::<MmapMessage>(variable, Endian::Little)?,
        MmapMessage {
            id: 7,
            text: "hello mmap".to_string()
        }
    );
    assert!(mmap.block_payload_window::<MmapMessage>(1)?.is_none());

    cleanup(&path);
    Ok(())
}

#[test]
fn mmap_rejects_entries_outside_snapshot() -> varve::Result<()> {
    let path = temp_path("forged_entry");
    cleanup(&path);

    {
        let mut file = MmapFormat::create(&path)?;
        file.push(&MmapPoint { x: 1, y: 2 })?;
        file.flush()?;
    }

    let file = MmapFormat::open_readonly(&path)?;
    let mmap = file.mmap_payloads()?;
    let mut forged: RecordIndexEntry = mmap.index_entries()[0].clone();
    forged.sequence = forged.sequence.saturating_add(1);

    assert!(matches!(
        mmap.payload_window(&forged),
        Err(Error::MmapEntryNotInSnapshot)
    ));

    cleanup(&path);
    Ok(())
}

#[test]
fn mmap_snapshot_does_not_expose_later_appends() -> varve::Result<()> {
    let path = temp_path("snapshot_append");
    cleanup(&path);

    {
        let mut file = MmapFormat::create(&path)?;
        file.push(&MmapPoint { x: 1, y: 2 })?;
        file.flush()?;
    }

    let mut file = MmapFormat::open(&path)?;
    let snapshot = file.mmap_payloads()?;
    assert_eq!(snapshot.len(), 1);

    file.push(&MmapPoint { x: 3, y: 4 })?;
    file.flush()?;

    assert_eq!(snapshot.len(), 1);
    assert!(snapshot.block_payload_window::<MmapPoint>(1)?.is_none());

    let fresh = file.mmap_payloads()?;
    assert_eq!(fresh.len(), 2);
    let appended = fresh
        .block_payload_window::<MmapPoint>(1)?
        .expect("appended fixed payload");
    assert_eq!(
        decode_from_slice::<MmapPoint>(appended, Endian::Little)?,
        MmapPoint { x: 3, y: 4 }
    );

    cleanup(&path);
    Ok(())
}

#[cfg(feature = "zero-copy")]
#[test]
fn zero_copy_raw_fixed_successfully_views_canonical_little_endian_payload() -> varve::Result<()> {
    let path = temp_path("raw_fixed");
    cleanup(&path);

    {
        let mut file = MmapFormat::create(&path)?;
        file.push(&RawPoint {
            x: 0x1122_3344,
            y: 0x5566_7788,
        })?;
        file.flush()?;
    }

    let file = MmapFormat::open_readonly(&path)?;
    let mmap = file.mmap_payloads()?;
    assert_eq!(
        mmap.raw_fixed::<RawPoint>(0)?,
        Some(&RawPoint {
            x: 0x1122_3344,
            y: 0x5566_7788
        })
    );
    assert!(mmap.raw_fixed::<RawPoint>(1)?.is_none());

    cleanup(&path);
    Ok(())
}

#[cfg(feature = "zero-copy")]
#[test]
fn zero_copy_rejects_variable_raw_block() -> varve::Result<()> {
    let path = temp_path("raw_variable");
    cleanup(&path);

    {
        let mut file = MmapFormat::create(&path)?;
        file.push(&RawVariable { id: 9 })?;
        file.flush()?;
    }

    let file = MmapFormat::open_readonly(&path)?;
    let mmap = file.mmap_payloads()?;

    assert!(matches!(
        mmap.raw_fixed::<RawVariable>(0),
        Err(Error::ZeroCopyBlockKindMismatch {
            actual: BlockKind::Variable
        })
    ));

    cleanup(&path);
    Ok(())
}

#[cfg(feature = "zero-copy")]
#[test]
fn zero_copy_rejects_raw_endian_mismatch() -> varve::Result<()> {
    let path = temp_path("raw_endian");
    cleanup(&path);

    {
        let mut file = MmapFormat::create(&path)?;
        file.push(&BigEndianRawPoint { x: 1, y: 2 })?;
        file.flush()?;
    }

    let file = MmapFormat::open_readonly(&path)?;
    let mmap = file.mmap_payloads()?;

    assert!(matches!(
        mmap.raw_fixed::<BigEndianRawPoint>(0),
        Err(Error::ZeroCopyEndianMismatch {
            expected: Endian::Little,
            actual: Endian::Big
        })
    ));

    cleanup(&path);
    Ok(())
}

#[cfg(feature = "zero-copy")]
#[test]
fn zero_copy_rejects_raw_payload_size_mismatch() -> varve::Result<()> {
    let path = temp_path("raw_size");
    cleanup(&path);

    {
        let mut file = MmapFormat::create(&path)?;
        file.push(&SizeMismatchRawPoint { x: 5 })?;
        file.flush()?;
    }

    let file = MmapFormat::open_readonly(&path)?;
    let mmap = file.mmap_payloads()?;

    assert!(matches!(
        mmap.raw_fixed::<SizeMismatchRawPoint>(0),
        Err(Error::ZeroCopyPayloadSizeMismatch {
            expected: 4,
            actual: 8
        })
    ));

    cleanup(&path);
    Ok(())
}

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

fn temp_path(name: &str) -> PathBuf {
    let mut path = std::env::temp_dir();
    path.push(format!(
        "varve_mmap_zero_copy_{name}_{}_{}_{}.vrv",
        std::process::id(),
        std::thread::current().name().unwrap_or("anon"),
        TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    path
}

fn cleanup(path: &PathBuf) {
    let _ = remove_file(path);
    let mut lock = path.as_os_str().to_os_string();
    lock.push(".lock");
    let _ = remove_file(PathBuf::from(lock));
}
