use std::cell::Cell;
use std::fs;
use std::io::Write;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use varve::{
    AdapterInputFile, BinaryCursor, Endian, Error, LayoutFieldValue, LayoutReader, LayoutValue,
    ReadLimits, SegmentWrite, SegmentWriteStream, varve_format,
};

varve_format! {
    pub format LayoutHardeningFormat {
        magic: b"HRDN";
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
            segment Data repeat once {
                lead_in Lead {
                    bytes tag = b"DATA";
                    u32 kind;
                    u64 next = finalize(target = segment_end, relative_to = after_lead_in);
                    u64 raw = finalize(target = raw_region_start, relative_to = after_lead_in);
                }

                metadata Metadata;
                raw_region Raw;

                footer Footer {
                    bytes seal = b"DONE";
                    u64 segment_len = finalize(target = segment_end, relative_to = segment_start);
                }
            }
        }
    }
}

#[test]
fn returned_stream_error_rolls_back_and_writer_is_reusable() -> varve::Result<()> {
    let path = temp_path("rollback", "hard");
    cleanup(&path);
    let fields = data_fields(7);
    let mut writer = LayoutHardeningFormat::create_layout_writer(&path)?;
    let original_len = fs::metadata(&path)?.len();

    let error = writer
        .write_segment_streamed(SegmentWriteStream {
            name: "Data",
            fields: &fields,
            footer_fields: &[],
            write_metadata: |out: &mut dyn Write| {
                out.write_all(b"partial metadata")?;
                Ok(())
            },
            write_raw: |out: &mut dyn Write| {
                out.write_all(b"partial raw")?;
                Err(Error::AdapterDiagnostic("injected stream error"))
            },
        })
        .expect_err("injected stream error should be returned");
    assert!(matches!(
        error,
        Error::AdapterDiagnostic("injected stream error")
    ));
    assert_eq!(fs::metadata(&path)?.len(), original_len);

    let info = writer.write_segment(SegmentWrite {
        name: "Data",
        fields: &fields,
        footer_fields: &[],
        metadata: b"metadata",
        raw: b"raw",
    })?;
    assert_eq!(info.segment_start, original_len);
    assert!(matches!(
        writer.write_segment(SegmentWrite {
            name: "Data",
            fields: &fields,
            footer_fields: &[],
            metadata: b"second",
            raw: b"segment",
        }),
        Err(Error::LayoutRepeatedOnceSegment { .. })
    ));
    writer.flush()?;
    drop(writer);

    let reader = LayoutHardeningFormat::open_layout_reader(&path)?;
    assert_eq!(reader.segment_count(), 1);
    assert_eq!(reader.read_metadata(0)?, b"metadata");
    assert_eq!(reader.read_raw(0)?, b"raw");
    drop(reader);
    cleanup(&path);
    Ok(())
}

#[test]
fn caller_field_prevalidation_runs_before_callbacks_or_poison() -> varve::Result<()> {
    let path = temp_path("prevalidation", "hard");
    cleanup(&path);
    let invalid_fields = [LayoutFieldValue {
        name: "kind",
        value: LayoutValue::Bytes(vec![1, 2, 3, 4]),
    }];
    let metadata_called = Cell::new(false);
    let raw_called = Cell::new(false);
    let mut writer = LayoutHardeningFormat::create_layout_writer(&path)?;
    let original_len = fs::metadata(&path)?.len();

    let result = writer.write_segment_streamed(SegmentWriteStream {
        name: "Data",
        fields: &invalid_fields,
        footer_fields: &[],
        write_metadata: |_out: &mut dyn Write| {
            metadata_called.set(true);
            Ok(())
        },
        write_raw: |_out: &mut dyn Write| {
            raw_called.set(true);
            Ok(())
        },
    });
    assert!(matches!(
        result,
        Err(Error::LayoutFieldTypeMismatch("kind"))
    ));
    assert!(!metadata_called.get());
    assert!(!raw_called.get());
    assert_eq!(fs::metadata(&path)?.len(), original_len);

    let fields = data_fields(9);
    writer.write_segment(SegmentWrite {
        name: "Data",
        fields: &fields,
        footer_fields: &[],
        metadata: b"",
        raw: b"",
    })?;
    writer.flush()?;
    drop(writer);
    cleanup(&path);
    Ok(())
}

#[test]
fn callback_panic_leaves_layout_writer_poisoned() -> varve::Result<()> {
    let path = temp_path("panic_poison", "hard");
    cleanup(&path);
    let fields = data_fields(11);
    let mut writer = LayoutHardeningFormat::create_layout_writer(&path)?;

    let panic = catch_unwind(AssertUnwindSafe(|| {
        let _ = writer.write_segment_streamed(SegmentWriteStream {
            name: "Data",
            fields: &fields,
            footer_fields: &[],
            write_metadata: |out: &mut dyn Write| -> varve::Result<()> {
                out.write_all(b"partial")?;
                panic!("injected callback panic");
            },
            write_raw: |_out: &mut dyn Write| Ok(()),
        });
    }));
    assert!(panic.is_err());

    assert_layout_poisoned(writer.write_segment(SegmentWrite {
        name: "Data",
        fields: &fields,
        footer_fields: &[],
        metadata: b"metadata",
        raw: b"raw",
    }));
    assert_layout_poisoned(writer.flush());
    assert_layout_poisoned(writer.sync());
    drop(writer);
    cleanup(&path);
    Ok(())
}

#[test]
fn layout_reader_payloads_stay_bound_to_the_open_object() -> varve::Result<()> {
    let path = temp_path("snapshot_original", "hard");
    let replacement = temp_path("snapshot_replacement", "hard");
    cleanup(&path);
    cleanup(&replacement);
    write_layout_data(&path, b"old metadata", b"old raw")?;
    write_layout_data(&replacement, b"new metadata", b"new raw")?;

    let old_reader = LayoutHardeningFormat::open_layout_reader(&path)?;
    fs::remove_file(&path)?;
    fs::rename(&replacement, &path)?;

    assert_eq!(old_reader.read_metadata(0)?, b"old metadata");
    assert_eq!(old_reader.read_raw(0)?, b"old raw");
    let new_reader = LayoutHardeningFormat::open_layout_reader(&path)?;
    assert_eq!(new_reader.read_metadata(0)?, b"new metadata");
    assert_eq!(new_reader.read_raw(0)?, b"new raw");

    drop(old_reader);
    drop(new_reader);
    cleanup(&path);
    cleanup(&replacement);
    Ok(())
}

#[test]
fn layout_open_and_region_reads_enforce_runtime_limits() -> varve::Result<()> {
    let path = temp_path("runtime_limits", "hard");
    cleanup(&path);
    write_layout_data(&path, b"metadata", b"raw")?;
    let file_len = fs::metadata(&path)?.len();
    let spec = LayoutHardeningFormat::spec();

    // No `max_file_len` case here any more: opening no longer refuses a file
    // for being longer than a declared number. The scan is bounded by
    // `max_scan_bytes` below, the index by `max_index_bytes`, and a region read
    // by `max_record_payload_len` — each against the resource it consumes, and
    // each still asserted. A length ceiling on top of those refused to read a
    // file whose bytes were already on disk while bounding nothing extra.
    assert_limit(
        spec.open_layout_reader_with_limits(
            &path,
            ReadLimits::missing().with_max_scan_bytes(file_len - 1),
        ),
        "scan bytes",
    );
    assert_limit(
        spec.open_layout_reader_with_limits(&path, ReadLimits::missing().with_max_segments(0)),
        "segment count",
    );
    assert_limit(
        spec.open_layout_reader_with_limits(&path, ReadLimits::missing().with_max_index_bytes(0)),
        "index bytes",
    );

    let reader = spec.open_layout_reader_with_limits(
        &path,
        ReadLimits::missing().with_max_record_payload_len(4),
    )?;
    assert_limit(reader.read_metadata(0), "record payload length");
    assert_eq!(reader.read_metadata_range(0, 1, 4)?, b"etad");
    assert_limit(reader.read_metadata_range(0, 0, 5), "record payload length");
    assert_eq!(reader.read_raw(0)?, b"raw");

    drop(reader);
    cleanup(&path);
    Ok(())
}

#[test]
fn layout_owned_reads_apply_materialization_limit_after_bounded_open() -> varve::Result<()> {
    let path = temp_path("materialized_read_limit", "hard");
    cleanup(&path);
    let raw = [0x5a; 32];
    write_layout_data(&path, b"metadata", &raw)?;
    let reader = LayoutHardeningFormat::spec().open_layout_reader_with_limits(
        &path,
        ReadLimits::missing().with_max_materialized_bytes(24),
    )?;
    assert_limit(reader.read_raw(0), "materialized bytes");
    assert_eq!(reader.read_raw_range(0, 0, 24)?, vec![0x5a; 24]);
    drop(reader);
    cleanup(&path);
    Ok(())
}

#[test]
fn layout_writer_limits_fail_before_unbounded_growth_and_roll_back() -> varve::Result<()> {
    let payload_path = temp_path("writer_payload_limit", "hard");
    let file_path = temp_path("writer_file_limit", "hard");
    let scan_path = temp_path("writer_scan_limit", "hard");
    let index_path = temp_path("writer_index_limit", "hard");
    let segment_path = temp_path("writer_segment_limit", "hard");
    cleanup(&payload_path);
    cleanup(&file_path);
    cleanup(&scan_path);
    cleanup(&index_path);
    cleanup(&segment_path);
    let spec = LayoutHardeningFormat::spec();
    let fields = data_fields(17);

    let mut payload_writer = spec.create_layout_writer_with_limits(
        &payload_path,
        ReadLimits::missing().with_max_record_payload_len(4),
    )?;
    assert_limit(
        payload_writer.write_segment(SegmentWrite {
            name: "Data",
            fields: &fields,
            footer_fields: &[],
            metadata: b"12345",
            raw: b"",
        }),
        "record payload length",
    );
    assert_eq!(fs::metadata(&payload_path)?.len(), 0);
    payload_writer.write_segment(SegmentWrite {
        name: "Data",
        fields: &fields,
        footer_fields: &[],
        metadata: b"1234",
        raw: b"5678",
    })?;
    drop(payload_writer);

    // The `max_file_len(35)` write-refusal case is gone with the ceiling. A
    // writer no longer refuses a segment because the file would get longer;
    // what it still refuses, and what the payload writer above still proves, is
    // a segment whose own payload exceeds `max_record_payload_len` — and the
    // rollback-to-zero assertion that mattered is asserted there.
    let _ = &file_path;

    let mut scan_writer = spec.create_layout_writer_with_limits(
        &scan_path,
        ReadLimits::missing().with_max_scan_bytes(35),
    )?;
    assert_limit(
        scan_writer.write_segment(SegmentWrite {
            name: "Data",
            fields: &fields,
            footer_fields: &[],
            metadata: b"",
            raw: b"",
        }),
        "scan bytes",
    );
    assert_eq!(fs::metadata(&scan_path)?.len(), 0);
    drop(scan_writer);

    let mut index_writer = spec.create_layout_writer_with_limits(
        &index_path,
        ReadLimits::missing().with_max_index_bytes(0),
    )?;
    assert_limit(
        index_writer.write_segment(SegmentWrite {
            name: "Data",
            fields: &fields,
            footer_fields: &[],
            metadata: b"",
            raw: b"",
        }),
        "index bytes",
    );
    assert_eq!(fs::metadata(&index_path)?.len(), 0);
    drop(index_writer);

    let mut segment_writer = spec.create_layout_writer_with_limits(
        &segment_path,
        ReadLimits::missing().with_max_segments(0),
    )?;
    assert_limit(
        segment_writer.write_segment(SegmentWrite {
            name: "Data",
            fields: &fields,
            footer_fields: &[],
            metadata: b"",
            raw: b"",
        }),
        "segment count",
    );
    assert_eq!(fs::metadata(&segment_path)?.len(), 0);
    drop(segment_writer);

    cleanup(&payload_path);
    cleanup(&file_path);
    cleanup(&scan_path);
    cleanup(&index_path);
    cleanup(&segment_path);
    Ok(())
}

#[test]
fn ordinary_layout_open_resolves_missing_and_handle_specs_sanitize_trust() -> varve::Result<()> {
    let path = temp_path("trusted_spec", "hard");
    cleanup(&path);
    write_layout_data(&path, b"metadata", b"raw")?;
    let unbounded = LayoutHardeningFormat::spec().with_read_limits(ReadLimits::trusted_unbounded());

    // The refusal is unchanged; the resource it names is not. `file length`
    // used to be the first required limit, and it is no longer required at all
    // — nothing enforces a file-length ceiling. `scan bytes` is now the first,
    // and it does bound a read.
    assert!(matches!(
        unbounded.open_layout_reader(&path),
        Err(Error::TrustedUnboundedRequiresExplicitApi {
            resource: "scan bytes"
        })
    ));
    let reader = unbounded.open_layout_reader_trusted_unbounded(&path)?;
    assert_eq!(reader.read_metadata(0)?, b"metadata");
    let reader_spec = reader.spec();
    drop(reader);
    assert!(matches!(
        reader_spec.open_layout_reader(&path),
        Err(Error::TrustedUnboundedRequiresExplicitApi {
            resource: "scan bytes"
        })
    ));

    let writer = unbounded.open_layout_writer_trusted_unbounded(&path)?;
    let writer_spec = writer.spec();
    drop(writer);
    assert!(matches!(
        writer_spec.open_layout_writer(&path),
        Err(Error::TrustedUnboundedRequiresExplicitApi {
            resource: "scan bytes"
        })
    ));

    let missing = LayoutHardeningFormat::spec().with_read_limits(ReadLimits::missing());
    let reader = missing.open_layout_reader(&path)?;
    assert_eq!(reader.read_metadata(0)?, b"metadata");
    assert_eq!(
        reader.spec().read_limits.max_records,
        varve::ReadLimit::Finite(u64::MAX)
    );
    let reader = LayoutReader::open(missing, &path)?;
    assert_eq!(reader.read_metadata(0)?, b"metadata");

    cleanup(&path);
    Ok(())
}

#[test]
fn array_f64_checks_complete_extent_before_allocation_or_cursor_movement() {
    let bytes = [0; 9];
    let mut cursor = BinaryCursor::new(&bytes, Endian::Little);
    assert_eq!(cursor.u8().expect("prefix byte"), 0);
    assert!(matches!(cursor.array_f64(2), Err(Error::UnexpectedEof)));
    assert_eq!(cursor.position(), 1);

    assert!(matches!(
        cursor.array_f64(usize::MAX),
        Err(Error::LengthOverflow { value: u64::MAX })
    ));
    assert_eq!(cursor.position(), 1);
}

#[test]
fn binary_cursor_materialization_ceiling_rejects_hostile_counts_before_allocation() {
    let bytes = [0; 16];
    let mut cursor = BinaryCursor::with_materialization_limit(&bytes, Endian::Little, 8);
    assert_eq!(cursor.materialization_limit(), 8);
    assert_eq!(cursor.materialization_remaining(), 8);

    assert!(matches!(
        cursor.array_f64(2),
        Err(Error::LimitExceeded {
            resource: "binary cursor materialization",
            actual: 16,
            limit: 8,
        })
    ));
    assert_eq!(cursor.position(), 0);

    assert!(matches!(
        cursor.array_f64(usize::MAX),
        Err(Error::LengthOverflow { value: u64::MAX })
    ));
    assert_eq!(cursor.position(), 0);
    assert_eq!(cursor.materialization_remaining(), 8);
}

#[test]
fn len_prefixed_string_checks_extent_and_ceiling_before_allocation() {
    let over_limit = [4, 0, b't', b'e', b's', b't'];
    let mut cursor = BinaryCursor::with_materialization_limit(&over_limit, Endian::Little, 3);
    assert!(matches!(
        cursor.len_prefixed_string::<u16>(),
        Err(Error::LimitExceeded {
            resource: "binary cursor materialization",
            actual: 4,
            limit: 3,
        })
    ));
    assert_eq!(cursor.position(), 0);

    let truncated = [u8::MAX, u8::MAX];
    let mut cursor =
        BinaryCursor::with_materialization_limit(&truncated, Endian::Little, usize::MAX);
    assert!(matches!(
        cursor.len_prefixed_string::<u16>(),
        Err(Error::UnexpectedEof)
    ));
    assert_eq!(cursor.position(), 0);
}

#[test]
fn adapter_temp_suffix_grammar_is_exact() -> varve::Result<()> {
    let max = "a".repeat(32);
    for extension in ["bin", ".bin", "a.b", "a-b", "a_b", max.as_str()] {
        let input = AdapterInputFile::from_bytes(extension, b"payload")?;
        let path = input.path().to_path_buf();
        let normalized = extension.strip_prefix('.').unwrap_or(extension);
        let expected_suffix = format!(".{normalized}");
        assert!(
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.ends_with(&expected_suffix))
        );
        assert_eq!(fs::read(&path)?, b"payload");
        drop(input);
        assert!(!path.exists());
    }

    let too_long = "a".repeat(33);
    let invalid = [
        "",
        ".",
        "..",
        "..bin",
        "a..b",
        "-ab",
        "ab-",
        "_ab",
        "ab_",
        "a.b.",
        "a/b",
        "a\\b",
        "a:b",
        "a b",
        "a\tb",
        "a#b",
        "a\0b",
        "\u{e9}",
        too_long.as_str(),
    ];
    for extension in invalid {
        match AdapterInputFile::from_bytes(extension, b"payload") {
            Err(Error::InvalidAdapterExtension(actual)) => assert_eq!(actual, extension),
            Err(other) => panic!("unexpected error for {extension:?}: {other:?}"),
            Ok(input) => panic!("invalid extension produced {:?}", input.path()),
        }
    }
    Ok(())
}

#[test]
fn adapter_temp_paths_are_unique_and_hold_independent_bytes() -> varve::Result<()> {
    let first = AdapterInputFile::from_bytes("bin", b"first")?;
    let second = AdapterInputFile::from_bytes("bin", b"second")?;
    let first_path = first.path().to_path_buf();
    let second_path = second.path().to_path_buf();

    assert_ne!(first_path, second_path);
    assert_eq!(fs::read(&first_path)?, b"first");
    assert_eq!(fs::read(&second_path)?, b"second");
    drop(first);
    assert!(!first_path.exists());
    assert!(second_path.exists());
    drop(second);
    assert!(!second_path.exists());
    Ok(())
}

#[test]
fn adapter_temp_path_lives_until_final_clone_is_dropped() -> varve::Result<()> {
    let input = AdapterInputFile::from_bytes("dat", b"shared")?;
    let clone = input.clone();
    let path = input.path().to_path_buf();

    drop(input);
    assert!(path.exists());
    assert_eq!(fs::read(&path)?, b"shared");
    drop(clone);
    assert!(!path.exists());
    Ok(())
}

#[test]
fn adapter_caller_path_is_never_removed() -> varve::Result<()> {
    let path = temp_path("caller_path", "dat");
    cleanup(&path);
    fs::write(&path, b"caller-owned")?;

    let input = AdapterInputFile::from_path(&path);
    let clone = input.clone();
    assert!(!input.is_temporary());
    assert_eq!(input.path(), path.as_path());
    drop(input);
    drop(clone);

    assert_eq!(fs::read(&path)?, b"caller-owned");
    cleanup(&path);
    Ok(())
}

fn data_fields(kind: u32) -> [LayoutFieldValue; 1] {
    [LayoutFieldValue {
        name: "kind",
        value: LayoutValue::U32(kind),
    }]
}

fn write_layout_data(path: &Path, metadata: &[u8], raw: &[u8]) -> varve::Result<()> {
    let fields = data_fields(7);
    let mut writer = LayoutHardeningFormat::create_layout_writer(path)?;
    writer.write_segment(SegmentWrite {
        name: "Data",
        fields: &fields,
        footer_fields: &[],
        metadata,
        raw,
    })?;
    writer.flush()?;
    Ok(())
}

fn assert_limit<T>(result: varve::Result<T>, resource: &'static str) {
    assert!(matches!(
        result,
        Err(Error::LimitExceeded {
            resource: actual,
            ..
        }) if actual == resource
    ));
}

#[test]
fn binary_cursor_materialization_budget_is_cumulative() -> varve::Result<()> {
    let bytes = [1, 0, b'a', 1, 0, b'b'];
    let mut cursor = BinaryCursor::with_materialization_limit(&bytes, Endian::Little, 1);
    assert_eq!(cursor.len_prefixed_string::<u16>()?, "a");
    assert_eq!(cursor.materialization_remaining(), 0);
    assert!(matches!(
        cursor.len_prefixed_string::<u16>(),
        Err(Error::LimitExceeded {
            resource: "binary cursor materialization",
            actual: 2,
            limit: 1,
        })
    ));
    Ok(())
}

fn assert_layout_poisoned<T>(result: varve::Result<T>) {
    assert!(matches!(result, Err(Error::WriterPoisoned("layout"))));
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

impl AsRef<Path> for TempPath {
    fn as_ref(&self) -> &Path {
        &self.path
    }
}

fn temp_path(name: &str, extension: &str) -> TempPath {
    static NEXT_PATH: AtomicU64 = AtomicU64::new(0);

    let unique = NEXT_PATH.fetch_add(1, Ordering::Relaxed);
    let dir = tempfile::tempdir().expect("create per-test temp directory");
    let path = dir.path().join(format!(
        "varve_layout_adapter_hardening_{}_{}_{}.{}",
        std::process::id(),
        name,
        unique,
        extension
    ));
    TempPath { path, _dir: dir }
}

fn cleanup(path: &Path) {
    let _ = fs::remove_file(path);
    let mut lock = path.as_os_str().to_os_string();
    lock.push(".lock");
    let _ = fs::remove_file(PathBuf::from(lock));
}
