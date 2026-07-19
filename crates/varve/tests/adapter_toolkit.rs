use std::fs::{OpenOptions, remove_file, write};
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Arc, Barrier};
use std::thread;

use varve::{
    AdapterCheckReport, AdapterCheckStatus, AdapterInputFile, AdapterTailStatus, BinaryCursor,
    BinaryWriter, ChunkEntry, ChunkIndexBuilder, ChunkLayout,
    DEFAULT_SIDECAR_FINGERPRINT_SCAN_LIMIT, Endian, Error, LayoutSegmentInfo, LayoutTailInfo,
    LayoutTailKind, ReadLimits, SegmentReducer, SidecarIdentity, SidecarMode, SidecarPolicy,
    TaggedValueCodec, reduce_segments_by_ref, varve_format,
};

varve_format! {
    pub format AdapterTailFormat {
        magic: b"ATK";
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
        schema_hash: computed;
        preset: none;

        layout {
            segment Data repeat until_eof {
                lead_in Lead {
                    bytes tag = b"ATK!";
                    u32 kind;
                    u64 next = finalize(target = segment_end, relative_to = after_lead_in);
                    u64 raw = finalize(target = raw_region_start, relative_to = after_lead_in);
                }

                metadata Metadata;
                raw_region Raw;
            }
        }
    }
}

#[test]
fn binary_writer_and_cursor_roundtrip_endian_values() -> varve::Result<()> {
    let mut writer = BinaryWriter::new(Endian::Big);
    writer.u8(7)?;
    writer.u16(0x1020)?;
    writer.u32(0x3040_5060)?;
    writer.i64(-77)?;
    writer.f64(3.5)?;
    writer.array_f64(&[1.25, 2.5])?;
    writer.len_prefixed_string::<u16>("hello")?;

    let bytes = writer.into_inner();
    let mut cursor = BinaryCursor::new(&bytes, Endian::Big);
    assert_eq!(cursor.u8()?, 7);
    assert_eq!(cursor.u16()?, 0x1020);
    assert_eq!(cursor.u32()?, 0x3040_5060);
    assert_eq!(cursor.i64()?, -77);
    assert_eq!(cursor.f64()?, 3.5);
    assert_eq!(cursor.array_f64(2)?, vec![1.25, 2.5]);
    assert_eq!(cursor.len_prefixed_string::<u16>()?, "hello");
    cursor.finish()?;
    Ok(())
}

#[test]
fn cursor_reports_eof_and_invalid_utf8() -> varve::Result<()> {
    let mut cursor = BinaryCursor::new(&[1, 2, 3], Endian::Little);
    assert!(matches!(cursor.u32(), Err(Error::UnexpectedEof)));

    let mut writer = BinaryWriter::new(Endian::Little);
    writer.len_prefixed_bytes::<u16>(&[0xff])?;
    let bytes = writer.into_inner();
    let mut cursor = BinaryCursor::new(&bytes, Endian::Little);
    assert!(matches!(
        cursor.len_prefixed_string::<u16>(),
        Err(Error::InvalidUtf8)
    ));
    Ok(())
}

#[test]
fn length_prefix_widths_roundtrip() -> varve::Result<()> {
    let mut writer = BinaryWriter::new(Endian::Little);
    writer.len_prefixed_bytes::<u8>(b"a")?;
    writer.len_prefixed_bytes::<u32>(b"bc")?;
    writer.len_prefixed_bytes::<u64>(b"def")?;
    let bytes = writer.into_inner();

    let mut cursor = BinaryCursor::new(&bytes, Endian::Little);
    assert_eq!(cursor.len_prefixed_bytes::<u8>()?, b"a");
    assert_eq!(cursor.len_prefixed_bytes::<u32>()?, b"bc");
    assert_eq!(cursor.len_prefixed_bytes::<u64>()?, b"def");
    cursor.finish()?;
    Ok(())
}

#[derive(Clone, Debug, PartialEq)]
enum TestValue {
    I64(i64),
    F64(f64),
    String(String),
}

struct TestValueCodec;

impl TaggedValueCodec for TestValueCodec {
    type Value = TestValue;
    type TypeId = u32;

    fn decode(type_id: Self::TypeId, cursor: &mut BinaryCursor<'_>) -> varve::Result<Self::Value> {
        match type_id {
            1 => Ok(TestValue::I64(cursor.i64()?)),
            2 => Ok(TestValue::F64(cursor.f64()?)),
            3 => Ok(TestValue::String(cursor.len_prefixed_string::<u32>()?)),
            other => Err(Error::AdapterUnsupportedType {
                type_id: u64::from(other),
            }),
        }
    }

    fn encode(value: &Self::Value, writer: &mut BinaryWriter) -> varve::Result<Self::TypeId> {
        match value {
            TestValue::I64(value) => {
                writer.i64(*value)?;
                Ok(1)
            }
            TestValue::F64(value) => {
                writer.f64(*value)?;
                Ok(2)
            }
            TestValue::String(value) => {
                writer.len_prefixed_string::<u32>(value)?;
                Ok(3)
            }
        }
    }
}

#[test]
fn tagged_value_codec_keeps_type_id_policy_user_owned() -> varve::Result<()> {
    let mut writer = BinaryWriter::new(Endian::Little);
    let type_id = TestValueCodec::encode(&TestValue::String("ada".to_string()), &mut writer)?;
    writer.u32(type_id)?;
    let bytes = writer.into_inner();

    let mut cursor = BinaryCursor::new(&bytes, Endian::Little);
    let value = TestValueCodec::decode(3, &mut cursor)?;
    assert_eq!(value, TestValue::String("ada".to_string()));
    assert_eq!(cursor.u32()?, 3);
    assert!(matches!(
        TestValueCodec::decode(99, &mut cursor),
        Err(Error::AdapterUnsupportedType { type_id: 99 })
    ));
    Ok(())
}

#[test]
fn chunk_index_validates_raw_bounds_and_keeps_duplicate_keys() -> varve::Result<()> {
    let segment = segment_info(10, 32);
    let mut builder = ChunkIndexBuilder::new();
    builder
        .push(chunk("voltage", 0, 0, 16, 2), &segment)?
        .push(chunk("voltage", 0, 16, 16, 2), &segment)?;
    let index = builder.finish()?;
    assert_eq!(index.len(), 2);
    let entries: Vec<_> = index.entries_for(&"voltage").collect();
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0].absolute_offset()?, 10);
    assert_eq!(entries[1].absolute_offset()?, 26);

    let mut builder = ChunkIndexBuilder::new();
    let result = builder.push(chunk("voltage", 0, 24, 16, 2), &segment);
    assert!(matches!(result, Err(Error::AdapterBounds { .. })));
    Ok(())
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ReducerMetadata {
    value: Option<u32>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct ReducerState {
    values: Vec<u32>,
    previous: u32,
}

struct TestReducer;

impl SegmentReducer for TestReducer {
    type Metadata = ReducerMetadata;
    type State = ReducerState;

    fn initial() -> Self::State {
        ReducerState::default()
    }

    fn apply_segment(
        state: &mut Self::State,
        _segment: &LayoutSegmentInfo,
        metadata: Self::Metadata,
    ) -> varve::Result<()> {
        let value = metadata.value.unwrap_or(state.previous);
        state.previous = value;
        state.values.push(value);
        Ok(())
    }
}

#[test]
fn reducer_applies_stateful_segment_metadata() -> varve::Result<()> {
    let first = segment_info(0, 4);
    let second = segment_info(4, 4);
    let report = reduce_segments_by_ref::<TestReducer, _>([
        (&first, ReducerMetadata { value: Some(7) }),
        (&second, ReducerMetadata { value: None }),
    ])?;
    assert_eq!(report.segments_applied, 2);
    assert_eq!(report.state.values, vec![7, 7]);
    Ok(())
}

#[test]
fn sidecar_policy_checks_presence_and_identity() -> varve::Result<()> {
    let main = temp_path("adapter_toolkit_sidecar", "bin");
    let sidecar = main.with_extension("idx");
    cleanup(&main);
    cleanup(&sidecar);
    write(&main, b"main")?;

    let policy = SidecarPolicy {
        extension: "idx",
        mode: SidecarMode::Optional,
        verify_main_len: true,
        verify_main_fingerprint: true,
    };
    assert_eq!(policy.sidecar_path(&main), sidecar);
    assert_eq!(policy.check_presence(&main)?, AdapterCheckStatus::Warning);

    write(policy.sidecar_path(&main), b"index")?;
    let identity = SidecarIdentity::from_main_file(&main)?;
    let report = policy.inspect(&main, Some(&identity))?;
    assert_eq!(report.status, AdapterCheckStatus::Passed);

    cleanup(&main);
    cleanup(&policy.sidecar_path(&main));
    Ok(())
}

#[test]
fn sidecar_identity_scan_limit_is_explicit() -> varve::Result<()> {
    let main = temp_path("adapter_toolkit_sidecar_limit", "bin");
    cleanup(&main);
    write(&main, b"main")?;

    assert!(matches!(
        SidecarIdentity::from_main_file_with_scan_limit(&main, 3),
        Err(Error::LimitExceeded {
            resource: "sidecar fingerprint scan bytes",
            actual: 4,
            limit: 3,
        })
    ));
    assert_eq!(SidecarIdentity::from_main_file(&main)?.main_len, 4);

    cleanup(&main);
    Ok(())
}

#[test]
fn sidecar_identity_default_rejects_large_extent_before_scanning() -> varve::Result<()> {
    let main = temp_path("adapter_toolkit_sidecar_default_limit", "bin");
    cleanup(&main);
    let file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&main)?;
    file.set_len(DEFAULT_SIDECAR_FINGERPRINT_SCAN_LIMIT + 1)?;
    drop(file);

    assert!(matches!(
        SidecarIdentity::from_main_file(&main),
        Err(Error::LimitExceeded {
            resource: "sidecar fingerprint scan bytes",
            actual,
            limit: DEFAULT_SIDECAR_FINGERPRINT_SCAN_LIMIT,
        }) if actual == DEFAULT_SIDECAR_FINGERPRINT_SCAN_LIMIT + 1
    ));

    cleanup(&main);
    Ok(())
}

#[test]
fn sidecar_identity_hashes_exactly_the_captured_extent_during_growth() -> varve::Result<()> {
    let main = temp_path("adapter_toolkit_sidecar_growth", "bin");
    cleanup(&main);
    write(&main, vec![0x5a; 32 * 1024 * 1024])?;

    let barrier = Arc::new(Barrier::new(2));
    let writer_barrier = Arc::clone(&barrier);
    let writer_path = main.clone();
    let writer = thread::spawn(move || -> std::io::Result<()> {
        let mut file = OpenOptions::new().append(true).open(writer_path)?;
        writer_barrier.wait();
        let chunk = [0xa5; 64 * 1024];
        for _ in 0..256 {
            file.write_all(&chunk)?;
            thread::yield_now();
        }
        file.flush()
    });

    barrier.wait();
    let identity = SidecarIdentity::from_main_file(&main)?;
    writer.join().expect("append thread panicked")?;

    let final_bytes = std::fs::read(&main)?;
    let captured_len = usize::try_from(identity.main_len).expect("test file fits in usize");
    assert!(captured_len <= final_bytes.len());
    assert_eq!(
        identity.main_fingerprint,
        stable_fingerprint(&final_bytes[..captured_len])
    );

    cleanup(&main);
    Ok(())
}

#[test]
fn adapter_report_preserves_layout_tail_status() -> varve::Result<()> {
    let path = temp_path("adapter_toolkit_tail", "atk");
    cleanup(&path);

    {
        let mut writer = AdapterTailFormat::create_layout_writer(&path)?;
        writer.write_data(AdapterTailFormatDataLayoutWrite {
            fields: AdapterTailFormatDataLayoutFields { kind: 1 },
            footer_fields: AdapterTailFormatDataLayoutFooterFields,
            metadata: b"meta",
            raw: b"raw",
        })?;
        writer.write_data(AdapterTailFormatDataLayoutWrite {
            fields: AdapterTailFormatDataLayoutFields { kind: 2 },
            footer_fields: AdapterTailFormatDataLayoutFooterFields,
            metadata: b"broken",
            raw: b"tail",
        })?;
        writer.flush()?;
    }
    let len = std::fs::metadata(&path)?.len();
    let file = std::fs::OpenOptions::new().write(true).open(&path)?;
    file.set_len(len - 2)?;

    let layout_report = AdapterTailFormat::inspect_layout_file_report(&path)?;
    let adapter_report = AdapterCheckReport::from_layout_report(&layout_report);
    assert_eq!(layout_report.segments.len(), 1);
    assert_eq!(
        adapter_report.physical_tail.as_ref().unwrap().kind,
        LayoutTailKind::InvalidSegmentBounds
    );
    assert_eq!(adapter_report.status(), AdapterCheckStatus::Warning);

    cleanup(&path);
    Ok(())
}

#[test]
fn layout_segment_and_index_limits_stop_scans_before_growth() -> varve::Result<()> {
    let path = temp_path("adapter_toolkit_limits", "atk");
    cleanup(&path);

    {
        let mut writer = AdapterTailFormat::create_layout_writer(&path)?;
        for kind in [1, 2] {
            writer.write_data(AdapterTailFormatDataLayoutWrite {
                fields: AdapterTailFormatDataLayoutFields { kind },
                footer_fields: AdapterTailFormatDataLayoutFooterFields,
                metadata: b"meta",
                raw: b"raw",
            })?;
        }
        writer.flush()?;
    }
    let spec = AdapterTailFormat::spec();

    assert!(matches!(
        spec.open_layout_reader_with_limits(&path, ReadLimits::missing().with_max_segments(1),),
        Err(Error::LimitExceeded {
            resource: "segment count",
            actual: 2,
            limit: 1,
        })
    ));
    assert!(matches!(
        spec.open_layout_reader_with_limits(&path, ReadLimits::missing().with_max_index_bytes(0),),
        Err(Error::LimitExceeded {
            resource: "index bytes",
            ..
        })
    ));
    assert!(matches!(
        spec.inspect_layout_file_report_with_limits(
            &path,
            ReadLimits::missing().with_max_segments(1),
        ),
        Err(Error::LimitExceeded {
            resource: "segment count",
            ..
        })
    ));
    assert!(matches!(
        spec.open_layout_writer_with_limits(&path, ReadLimits::missing().with_max_segments(1),),
        Err(Error::LimitExceeded {
            resource: "segment count",
            ..
        })
    ));

    cleanup(&path);
    Ok(())
}

#[test]
fn layout_report_rejects_complete_malformed_segments_as_fatal() -> varve::Result<()> {
    let bad_tag = temp_path("adapter_toolkit_bad_tag", "atk");
    let bad_bounds = temp_path("adapter_toolkit_bad_bounds", "atk");
    cleanup(&bad_tag);
    cleanup(&bad_bounds);

    for path in [&bad_tag, &bad_bounds] {
        let mut writer = AdapterTailFormat::create_layout_writer(path)?;
        writer.write_data(AdapterTailFormatDataLayoutWrite {
            fields: AdapterTailFormatDataLayoutFields { kind: 7 },
            footer_fields: AdapterTailFormatDataLayoutFooterFields,
            metadata: b"meta",
            raw: b"raw",
        })?;
        writer.flush()?;
    }

    let mut bytes = std::fs::read(&bad_tag)?;
    bytes[0] = b'X';
    write(&bad_tag, bytes)?;
    assert!(matches!(
        AdapterTailFormat::inspect_layout_file_report(&bad_tag),
        Err(Error::LayoutLiteralMismatch { .. })
    ));

    let mut bytes = std::fs::read(&bad_bounds)?;
    bytes[8..16].copy_from_slice(&u64::MAX.to_le_bytes());
    write(&bad_bounds, bytes)?;
    assert!(matches!(
        AdapterTailFormat::inspect_layout_file_report(&bad_bounds),
        Err(Error::LayoutInvalidSegmentBounds { .. })
    ));

    cleanup(&bad_tag);
    cleanup(&bad_bounds);
    Ok(())
}

#[test]
fn adapter_input_file_materializes_bytes_and_cleans_up_temp_path() -> varve::Result<()> {
    let input = AdapterInputFile::from_bytes("bin", b"abc")?;
    let path = input.path().to_path_buf();

    assert!(input.is_temporary());
    assert_eq!(std::fs::read(&path)?, b"abc");

    drop(input);
    assert!(!path.exists());
    Ok(())
}

#[test]
fn adapter_tail_status_summarizes_remaining_physical_tail() {
    let tail = LayoutTailInfo {
        offset: 40,
        file_len: 64,
        kind: LayoutTailKind::InvalidSegmentBounds,
    };
    let status = AdapterTailStatus::new(tail, Some(56), Some("footer mismatch".to_string()));

    assert_eq!(status.available_len, 24);
    assert_eq!(status.expected_end, Some(56));
    assert_eq!(status.evidence.as_deref(), Some("footer mismatch"));
}

fn chunk(
    key: &'static str,
    segment_index: usize,
    byte_offset: u64,
    byte_len: u64,
    value_count: u64,
) -> ChunkEntry<&'static str> {
    ChunkEntry {
        key,
        segment_index,
        byte_offset,
        byte_len,
        value_count,
        layout: ChunkLayout::Contiguous,
    }
}

fn segment_info(raw_offset: u64, raw_len: u64) -> LayoutSegmentInfo {
    LayoutSegmentInfo {
        name: "Data",
        segment_start: raw_offset,
        lead_in_len: 0,
        metadata_offset: raw_offset,
        metadata_len: 0,
        raw_offset,
        raw_len,
        footer_offset: raw_offset + raw_len,
        footer_len: 0,
        segment_end: raw_offset + raw_len,
        fields: Vec::new(),
        footer_fields: Vec::new(),
    }
}

fn stable_fingerprint(bytes: &[u8]) -> u64 {
    const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
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

fn temp_path(name: &str, extension: &str) -> TempPath {
    let dir = tempfile::tempdir().expect("create per-test temp directory");
    let path = dir
        .path()
        .join(format!("varve_{name}_{}.{}", std::process::id(), extension));
    TempPath { path, _dir: dir }
}

fn cleanup(path: &PathBuf) {
    let _ = remove_file(path);
    let mut lock = path.as_os_str().to_os_string();
    lock.push(".lock");
    let _ = remove_file(PathBuf::from(lock));
}
