use std::fs::{OpenOptions, remove_file};
use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

#[cfg(feature = "integrity")]
use varve::ReadLimits;
use varve::{Error, RecoveryPolicy, VarveBlock, VarveFile, VarveMerge, varve_format};

#[derive(Clone, Debug, PartialEq, Eq, VarveBlock)]
#[varve(id = 901, version = 1, kind = "fixed")]
struct NativeValue {
    value: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, VarveBlock)]
#[varve(id = 902, version = 1, kind = "fixed", key = "id")]
struct KeyedValue {
    id: u64,
    value: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, VarveBlock)]
#[varve(id = 903, version = 1, kind = "fixed")]
struct NoopOp {
    marker: u8,
}

impl VarveMerge for KeyedValue {
    type Op = NoopOp;

    fn apply_op(&mut self, _op: Self::Op) -> varve::Result<()> {
        Ok(())
    }
}

varve_format! {
    pub struct NativeSecurityFormat {
        magic: b"NSEC";
        version: 1;
        limits {
            file_len: 8_388_608;
            records: 1024;
            index_bytes: 1_048_576;
            scan_bytes: 8_388_608;
            record_payload: 1_048_576;
            logical_payload: 1_048_576;
            materialized_bytes: 2_097_152;
            segments: 1024;
            matrix_dimension: 1024;
            matrix_cells: 1_048_576;
            matrix_bitmap: 1_048_576;
            matrix_crc: 4_194_304;
            matrix_metadata: 1_048_576;
            matrix_slot_region: 8_388_608;
            sidecar: 1_048_576;
            mmap: 8_388_608;
        }
        endian: little;
        index: [scan_on_open, block_offset_chain, keyed_offset_chain];
        commit: record_footer;
        blocks: [NativeValue, KeyedValue, NoopOp];
    }
}

varve_format! {
    pub struct TrustedSecurityFormat {
        magic: b"NTRUST";
        version: 1;
        limits: trusted_unbounded;
        endian: little;
        blocks: [NativeValue];
    }
}

#[cfg(feature = "integrity")]
varve_format! {
    pub format SidecarSecurityFormat {
        magic: b"NSID";
        version: 1;
        limits {
            file_len: 8_388_608;
            records: 1024;
            index_bytes: 1_048_576;
            scan_bytes: 8_388_608;
            record_payload: 1_048_576;
            logical_payload: 1_048_576;
            materialized_bytes: 1_048_576;
            segments: 1024;
            matrix_dimension: 1024;
            matrix_cells: 1_048_576;
            matrix_bitmap: 1_048_576;
            matrix_crc: 4_194_304;
            matrix_metadata: 1_048_576;
            matrix_slot_region: 8_388_608;
            sidecar: 1_048_576;
            mmap: 8_388_608;
        }
        endian: little;
        schema_hash: computed;
        dims {
            row: u32,
            column: u32,
        }
        commit: cell_bitmap {
            keyspace = [row, column];
            categories = [analysis];
        };
        blocks {
            matrix SidecarCell(id = 904, dims = [row, column], category = analysis) {
                value: u32,
            }
        }
    }
}

#[test]
fn native_reader_and_lazy_collection_stay_on_original_path_generation() -> varve::Result<()> {
    let path = temp_path("path_generation");
    let replacement = temp_path("path_generation_replacement");
    cleanup(&path);
    cleanup(&replacement);

    write_native_value(&path, 10)?;
    let reader = NativeSecurityFormat::open_readonly(&path)?;
    let lazy = reader.blocks::<NativeValue>()?;
    write_native_value(&replacement, 20)?;

    remove_file(&path)?;
    std::fs::rename(&replacement, &path)?;

    assert_eq!(
        reader.blocks::<NativeValue>()?.get(0)?,
        Some(NativeValue { value: 10 })
    );
    assert_eq!(lazy.get(0)?, Some(NativeValue { value: 10 }));
    assert_eq!(
        NativeSecurityFormat::open_readonly(&path)?
            .blocks::<NativeValue>()?
            .get(0)?,
        Some(NativeValue { value: 20 })
    );

    drop(reader);
    cleanup(&path);
    cleanup(&replacement);
    Ok(())
}

#[test]
fn fixed_cow_preserves_old_reader_and_supports_record_footers() -> varve::Result<()> {
    let path = temp_path("cow_fixed");
    cleanup(&path);
    write_native_value(&path, 1)?;

    let old_reader = NativeSecurityFormat::open_readonly(&path)?;
    let old_lazy = old_reader.blocks::<NativeValue>()?;
    {
        let mut writer = NativeSecurityFormat::open(&path)?;
        writer.replace_fixed(0, &NativeValue { value: 2 })?;
        writer.flush()?;
    }

    assert_eq!(old_lazy.get(0)?, Some(NativeValue { value: 1 }));
    assert_eq!(
        old_reader.blocks::<NativeValue>()?.get(0)?,
        Some(NativeValue { value: 1 })
    );
    assert_eq!(
        NativeSecurityFormat::open_readonly(&path)?
            .blocks::<NativeValue>()?
            .get(0)?,
        Some(NativeValue { value: 2 })
    );

    drop(old_reader);
    cleanup(&path);
    Ok(())
}

#[test]
fn exclusive_in_place_replace_is_explicitly_not_snapshot_safe() -> varve::Result<()> {
    let path = temp_path("exclusive_fixed");
    cleanup(&path);
    write_native_value(&path, 1)?;

    let old_reader = NativeSecurityFormat::open_readonly(&path)?;
    let mut writer = NativeSecurityFormat::open(&path)?;
    // SAFETY: This test intentionally demonstrates the consequence of violating
    // the API's exclusion requirement with an already-open reader.
    unsafe {
        writer.replace_fixed_in_place_exclusive(0, &NativeValue { value: 3 })?;
    }
    writer.flush()?;

    assert_eq!(
        old_reader.blocks::<NativeValue>()?.get(0)?,
        Some(NativeValue { value: 3 })
    );

    drop(writer);
    drop(old_reader);
    cleanup(&path);
    Ok(())
}

#[test]
fn duplicate_native_sequences_are_rejected() -> varve::Result<()> {
    let path = temp_path("duplicate_sequence");
    cleanup(&path);
    let second_offset = {
        let mut file = NativeSecurityFormat::create(&path)?;
        file.push(&NativeValue { value: 1 })?;
        file.push(&NativeValue { value: 2 })?;
        file.flush()?;
        file.index_entries()[1].record_offset
    };

    let mut file = OpenOptions::new().read(true).write(true).open(&path)?;
    file.seek(SeekFrom::Start(second_offset + 8))?;
    file.write_all(&0u64.to_le_bytes())?;
    file.flush()?;
    drop(file);

    let error = NativeSecurityFormat::open_readonly(&path).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("duplicate native record sequence")
    );

    cleanup(&path);
    Ok(())
}

#[test]
fn record_limit_failure_is_fatal_to_recovery_and_preserves_file() -> varve::Result<()> {
    let path = temp_path("limit_recovery");
    cleanup(&path);
    let record_offset = {
        let mut file = NativeSecurityFormat::create(&path)?;
        file.push(&NativeValue { value: 1 })?;
        file.flush()?;
        file.index_entries()[0].record_offset
    };

    let before = std::fs::metadata(&path)?.len();
    let mut file = OpenOptions::new().read(true).write(true).open(&path)?;
    file.seek(SeekFrom::Start(record_offset + 16))?;
    file.write_all(&2_000_000u64.to_le_bytes())?;
    file.flush()?;
    drop(file);

    let recovery = NativeSecurityFormat::spec().with_recovery_policy(RecoveryPolicy::TruncateTail);
    assert!(matches!(
        recovery.open_recover_with_report(&path),
        Err(Error::LimitExceeded {
            resource: "record payload length",
            ..
        })
    ));
    assert_eq!(std::fs::metadata(&path)?.len(), before);

    cleanup(&path);
    Ok(())
}

#[test]
fn default_lock_refusal_does_not_parse_and_inspection_is_bounded() -> varve::Result<()> {
    let path = temp_path("bounded_lock");
    cleanup(&path);
    write_native_value(&path, 1)?;
    let lock = lock_path(&path);
    std::fs::write(&lock, vec![b'x'; 32 * 1024])?;

    assert!(matches!(
        NativeSecurityFormat::open(&path),
        Err(Error::WriterLockHeld(_))
    ));
    assert!(matches!(
        NativeSecurityFormat::spec().inspect_writer_lock(&path),
        Err(Error::WriterLockMalformed(_))
    ));
    assert_eq!(std::fs::metadata(&lock)?.len(), 32 * 1024);

    cleanup(&path);
    Ok(())
}

#[test]
fn keyed_tombstone_order_uses_sequence_before_physical_ordinal() -> varve::Result<()> {
    let path = temp_path("keyed_order");
    cleanup(&path);
    {
        let mut writer = NativeSecurityFormat::create(&path)?;
        writer.push(&KeyedValue { id: 7, value: 1 })?;
        writer.delete::<KeyedValue>(&7)?;
        // SAFETY: No other handle exists during this exclusive mutation.
        unsafe {
            writer.replace_fixed_in_place_exclusive(0, &KeyedValue { id: 7, value: 3 })?;
        }
        writer.flush()?;
    }

    let reader = NativeSecurityFormat::open_readonly(&path)?;
    assert_eq!(
        reader.keyed_blocks::<KeyedValue>()?.get(&7)?,
        Some(KeyedValue { id: 7, value: 3 })
    );
    assert_eq!(
        reader.materialized_keyed_blocks::<KeyedValue>()?.get(&7),
        Some(&KeyedValue { id: 7, value: 3 })
    );

    cleanup(&path);
    Ok(())
}

#[test]
fn trusted_handle_spec_is_sanitized_before_it_can_escape() -> varve::Result<()> {
    let path = temp_path("trusted_spec");
    cleanup(&path);
    let mut created = TrustedSecurityFormat::spec().create_trusted_unbounded(&path)?;
    created.push(&NativeValue { value: 9 })?;
    created.flush()?;
    let escaped_create_spec = created.spec();
    drop(created);
    assert!(matches!(
        VarveFile::open_readonly(escaped_create_spec, &path),
        Err(Error::TrustedUnboundedRequiresExplicitApi {
            resource: "file length"
        })
    ));

    let reader = TrustedSecurityFormat::open_reader_trusted_unbounded(&path)?;
    let escaped = reader.spec();
    assert!(matches!(
        VarveFile::open_readonly(escaped, &path),
        Err(Error::TrustedUnboundedRequiresExplicitApi {
            resource: "file length"
        })
    ));

    drop(reader);
    cleanup(&path);
    Ok(())
}

#[cfg(feature = "integrity")]
#[test]
fn sidecar_metadata_limit_is_checked_before_body_materialization() -> varve::Result<()> {
    use varve::MatrixDimensions;

    let path = temp_path("sidecar_limit");
    let sidecar = path.with_extension("sidecar");
    cleanup(&path);
    let _ = remove_file(&sidecar);
    {
        let dims = MatrixDimensions::from_pairs([("row", 1), ("column", 1)]);
        let mut file = SidecarSecurityFormat::spec().create_with_dims(&path, dims)?;
        file.write_matrix_sidecar("analysis", &sidecar, 1, &[7; 128])?;
        file.flush()?;
    }

    let sidecar_len = std::fs::metadata(&sidecar)?.len();
    let runtime = ReadLimits::finite_all(u64::MAX).with_max_sidecar_len(sidecar_len - 1);
    let reader = SidecarSecurityFormat::spec().open_reader_with_limits(&path, runtime)?;
    assert!(matches!(
        reader.read_matrix_sidecar("analysis", &sidecar),
        Err(Error::LimitExceeded {
            resource: "sidecar length",
            ..
        })
    ));

    drop(reader);
    remove_file(&sidecar)?;
    cleanup(&path);
    Ok(())
}

fn write_native_value(path: &Path, value: u64) -> varve::Result<()> {
    let mut file = NativeSecurityFormat::create(path)?;
    file.push(&NativeValue { value })?;
    file.flush()
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

fn temp_path(label: &str) -> TempPath {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let dir = tempfile::tempdir().expect("create per-test temp directory");
    let path = dir.path().join(format!(
        "varve_native_security_{label}_{}_{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    TempPath { path, _dir: dir }
}

fn lock_path(path: &Path) -> PathBuf {
    let mut lock = path.as_os_str().to_os_string();
    lock.push(".lock");
    PathBuf::from(lock)
}

fn cleanup(path: &Path) {
    let _ = remove_file(path);
    let _ = remove_file(lock_path(path));
}
