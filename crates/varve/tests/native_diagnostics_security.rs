use std::ffi::OsString;
use std::fs::{OpenOptions, read, remove_file, write};
use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use varve::{
    DiagnosticDomain, DiagnosticSeverity, Error, OP_BLOCK_ID, ReadLimits, TOMBSTONE_BLOCK_ID,
    VarveBlock, VarveMerge, diagnose_file, varve_format,
};

#[derive(Clone, Debug, PartialEq, VarveBlock)]
#[varve(id = 71, version = 1, kind = "fixed")]
struct GoldenPoint {
    x: u32,
    y: u32,
}

#[derive(Clone, Debug, PartialEq, VarveBlock)]
#[varve(id = 72, version = 1, kind = "variable", key = "id")]
struct SecurityUser {
    #[varve(field_id = 1)]
    id: u64,
    #[varve(field_id = 2)]
    name: String,
}

#[derive(Clone, Debug, PartialEq, VarveBlock)]
#[varve(id = 73, version = 1, kind = "variable")]
struct SecurityUserOp {
    #[varve(field_id = 1)]
    name: String,
}

impl VarveMerge for SecurityUser {
    type Op = SecurityUserOp;

    fn apply_op(&mut self, op: Self::Op) -> varve::Result<()> {
        self.name = op.name;
        Ok(())
    }
}

varve_format! {
    pub struct NativeHeaderSecurityFormat {
        magic: b"NSEC";
        version: 1;
        limits {
            file_len: 4096;
            records: 32;
            index_bytes: 4096;
            scan_bytes: 4096;
            record_payload: 256;
            logical_payload: 256;
            materialized_bytes: 4096;
        }
        endian: little;
        schema_hash: computed;
        commit: record_footer;
        blocks: [GoldenPoint];
    }
}

varve_format! {
    pub struct NativeEnvelopeSecurityFormat {
        magic: b"NENV";
        version: 1;
        limits {
            file_len: 4096;
            records: 32;
            index_bytes: 4096;
            scan_bytes: 4096;
            record_payload: 256;
            logical_payload: 256;
            materialized_bytes: 4096;
        }
        endian: little;
        blocks: [SecurityUser, SecurityUserOp];
    }
}

#[test]
fn native_header_and_footer_bytes_remain_canonical() -> varve::Result<()> {
    let path = temp_path("golden");
    cleanup(&path);

    {
        let mut file = NativeHeaderSecurityFormat::create(&path)?;
        file.push(&GoldenPoint { x: 9, y: 10 })?;
        file.flush()?;
    }

    let mut expected = Vec::new();
    expected.extend_from_slice(b"NSEC");
    expected.extend_from_slice(b"VARVE3");
    expected.extend_from_slice(&1u16.to_le_bytes());
    expected.push(1);
    expected.push(0);
    expected.extend_from_slice(&NativeHeaderSecurityFormat::spec().schema_hash.to_le_bytes());
    expected.extend_from_slice(&0u32.to_le_bytes());

    expected.extend_from_slice(&GoldenPoint::ID.to_le_bytes());
    expected.extend_from_slice(&GoldenPoint::VERSION.to_le_bytes());
    expected.extend_from_slice(&0u16.to_le_bytes());
    expected.extend_from_slice(&0u64.to_le_bytes());
    expected.extend_from_slice(&8u64.to_le_bytes());
    expected.extend_from_slice(&0u32.to_le_bytes());
    expected.extend_from_slice(&0u32.to_le_bytes());
    expected.extend_from_slice(&9u32.to_le_bytes());
    expected.extend_from_slice(&10u32.to_le_bytes());

    expected.extend_from_slice(b"VRF1");
    expected.extend_from_slice(&1u16.to_le_bytes());
    expected.extend_from_slice(&0u16.to_le_bytes());
    expected.extend_from_slice(&0u64.to_le_bytes());
    expected.extend_from_slice(&0u64.to_le_bytes());
    expected.extend_from_slice(&0u32.to_le_bytes());
    expected.extend_from_slice(&0u32.to_le_bytes());

    assert_eq!(read(&path)?, expected);
    assert_eq!(
        NativeHeaderSecurityFormat::open_readonly(&path)?
            .blocks::<GoldenPoint>()?
            .get(0)?,
        Some(GoldenPoint { x: 9, y: 10 })
    );

    cleanup(&path);
    Ok(())
}

#[test]
fn native_file_header_rejects_nonzero_reserved_flags() -> varve::Result<()> {
    let path = temp_path("reserved_flags");
    cleanup(&path);

    {
        let mut file = NativeHeaderSecurityFormat::create(&path)?;
        file.push(&GoldenPoint { x: 1, y: 2 })?;
        file.flush()?;
    }

    let mut bytes = read(&path)?;
    let flags_offset = b"NSEC".len() + b"VARVE3".len() + 2 + 1;
    assert_eq!(bytes[flags_offset], 0);
    bytes[flags_offset] = 1;
    write(&path, bytes)?;

    assert!(matches!(
        NativeHeaderSecurityFormat::open_readonly(&path),
        Err(Error::InvalidCanonicalEncoding(
            "native file-header reserved flags must be zero"
        ))
    ));

    let report = diagnose_file(NativeHeaderSecurityFormat::spec(), &path);
    assert!(report.items.iter().any(|item| {
        item.severity == DiagnosticSeverity::Error
            && item.domain == DiagnosticDomain::FileData
            && item.code == "file.open_readonly.failed"
    }));

    cleanup(&path);
    Ok(())
}

#[test]
fn native_record_footer_rejects_reserved_and_noncanonical_fields() -> varve::Result<()> {
    let base = temp_path("footer_base");
    let unknown_flags = temp_path("footer_unknown_flags");
    let reserved = temp_path("footer_reserved");
    let zero_link = temp_path("footer_zero_link");
    for path in [&base, &unknown_flags, &reserved, &zero_link] {
        cleanup(path);
    }

    let footer_offset = {
        let mut file = NativeHeaderSecurityFormat::create(&base)?;
        file.push(&GoldenPoint { x: 1, y: 2 })?;
        file.flush()?;
        usize::try_from(file.index_entries()[0].footer_offset.unwrap())
            .expect("test footer offset must fit usize")
    };
    let canonical = read(&base)?;

    let mut bytes = canonical.clone();
    bytes[footer_offset + 6..footer_offset + 8].copy_from_slice(&0x8000u16.to_le_bytes());
    write(&unknown_flags, bytes)?;
    assert!(matches!(
        NativeHeaderSecurityFormat::open_readonly(&unknown_flags),
        Err(Error::InvalidRecordFooter { .. })
    ));

    let mut bytes = canonical.clone();
    bytes[footer_offset + 28..footer_offset + 32].copy_from_slice(&1u32.to_le_bytes());
    write(&reserved, bytes)?;
    assert!(matches!(
        NativeHeaderSecurityFormat::open_readonly(&reserved),
        Err(Error::InvalidRecordFooter { .. })
    ));

    let mut bytes = canonical;
    bytes[footer_offset + 6..footer_offset + 8].copy_from_slice(&1u16.to_le_bytes());
    write(&zero_link, bytes)?;
    assert!(matches!(
        NativeHeaderSecurityFormat::open_readonly(&zero_link),
        Err(Error::InvalidRecordFooter { .. })
    ));

    for path in [&base, &unknown_flags, &reserved, &zero_link] {
        cleanup(path);
    }
    Ok(())
}

#[test]
fn internal_key_and_op_envelopes_reject_trailing_bytes() -> varve::Result<()> {
    let tombstone_path = temp_path("trailing_tombstone");
    let op_path = temp_path("trailing_op");
    cleanup(&tombstone_path);
    cleanup(&op_path);

    let tombstone = {
        let mut file = NativeEnvelopeSecurityFormat::create(&tombstone_path)?;
        file.push(&SecurityUser {
            id: 7,
            name: "before-delete".to_string(),
        })?;
        file.delete::<SecurityUser>(&7)?;
        file.flush()?;
        file.index_entries().last().cloned().unwrap()
    };
    assert_eq!(tombstone.block_id, TOMBSTONE_BLOCK_ID);
    append_declared_trailing_byte(&tombstone_path, &tombstone)?;

    let tombstone_error = match NativeEnvelopeSecurityFormat::open_readonly(&tombstone_path) {
        Ok(file) => file.keyed_blocks::<SecurityUser>().unwrap_err(),
        Err(error) => error,
    };
    assert!(matches!(
        tombstone_error,
        Error::TrailingBytes { remaining: 1 }
    ));

    let op = {
        let mut file = NativeEnvelopeSecurityFormat::create(&op_path)?;
        file.push(&SecurityUser {
            id: 8,
            name: "before-op".to_string(),
        })?;
        file.push_op::<SecurityUser>(
            &8,
            &SecurityUserOp {
                name: "after-op".to_string(),
            },
        )?;
        file.flush()?;
        file.index_entries().last().cloned().unwrap()
    };
    assert_eq!(op.block_id, OP_BLOCK_ID);
    append_declared_trailing_byte(&op_path, &op)?;

    let op_error = match NativeEnvelopeSecurityFormat::open_readonly(&op_path) {
        Ok(file) => file
            .materialized_keyed_blocks::<SecurityUser>()
            .unwrap_err(),
        Err(error) => error,
    };
    assert!(matches!(op_error, Error::TrailingBytes { remaining: 1 }));

    cleanup(&tombstone_path);
    cleanup(&op_path);
    Ok(())
}

#[test]
fn diagnostics_enforce_cumulative_materialization_limit() -> varve::Result<()> {
    let path = temp_path("diagnostic_limit");
    cleanup(&path);

    {
        let mut file = NativeHeaderSecurityFormat::create(&path)?;
        file.push(&GoldenPoint { x: 1, y: 2 })?;
        file.push(&GoldenPoint { x: 3, y: 4 })?;
        file.flush()?;
    }

    let runtime_limits = ReadLimits::trusted_unbounded().with_max_materialized_bytes(8);
    let spec = NativeHeaderSecurityFormat::spec().tighten_read_limits(runtime_limits);
    let report = diagnose_file(spec, &path);
    let limit_error = report
        .items
        .iter()
        .find(|item| item.code == "file.record.payload_invalid")
        .expect("the second diagnostic payload must exceed the cumulative limit");
    assert_eq!(limit_error.severity, DiagnosticSeverity::Error);
    assert_eq!(limit_error.domain, DiagnosticDomain::FileData);
    assert!(limit_error.message.contains("materialized bytes"));

    cleanup(&path);
    Ok(())
}

fn append_declared_trailing_byte(
    path: &Path,
    entry: &varve::RecordIndexEntry,
) -> varve::Result<()> {
    assert_eq!(
        entry.checked_physical_end()?,
        std::fs::metadata(path)?.len()
    );
    assert!(entry.footer_offset.is_none());

    let payload_len = entry
        .payload_len
        .checked_add(1)
        .expect("test payload length must fit u64");
    let payload_len_offset = entry
        .record_offset
        .checked_add(16)
        .expect("test header offset must fit u64");
    let mut file = OpenOptions::new().read(true).write(true).open(path)?;
    file.seek(SeekFrom::Start(payload_len_offset))?;
    file.write_all(&payload_len.to_le_bytes())?;
    file.seek(SeekFrom::End(0))?;
    file.write_all(&[0xa5])?;
    file.sync_all()?;
    Ok(())
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
        "varve-native-diagnostics-security-{}-{}-{label}.v",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    TempPath { path, _dir: dir }
}

fn cleanup(path: &Path) {
    let _ = remove_file(path);
    let mut lock = OsString::from(path.as_os_str());
    lock.push(".lock");
    let _ = remove_file(PathBuf::from(lock));
}
