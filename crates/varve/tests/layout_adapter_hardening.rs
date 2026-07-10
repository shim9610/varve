use std::cell::Cell;
use std::fs;
use std::io::Write;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use varve::{
    AdapterInputFile, BinaryCursor, Endian, Error, LayoutFieldValue, LayoutValue, SegmentWrite,
    SegmentWriteStream, varve_format,
};

varve_format! {
    pub format LayoutHardeningFormat {
        magic: b"HRDN";
        version: 1;
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
    assert_eq!(reader.segments().len(), 1);
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
    assert_eq!(input.path(), path);
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

fn assert_layout_poisoned<T>(result: varve::Result<T>) {
    assert!(matches!(result, Err(Error::WriterPoisoned("layout"))));
}

fn temp_path(name: &str, extension: &str) -> PathBuf {
    static NEXT_PATH: AtomicU64 = AtomicU64::new(0);

    let unique = NEXT_PATH.fetch_add(1, Ordering::Relaxed);
    let mut path = std::env::temp_dir();
    path.push(format!(
        "varve_layout_adapter_hardening_{}_{}_{}.{}",
        std::process::id(),
        name,
        unique,
        extension
    ));
    path
}

fn cleanup(path: &Path) {
    let _ = fs::remove_file(path);
    let mut lock = path.as_os_str().to_os_string();
    lock.push(".lock");
    let _ = fs::remove_file(PathBuf::from(lock));
}
