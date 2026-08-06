use std::fs::{OpenOptions, metadata, remove_file};
#[cfg(feature = "integrity")]
use std::io::Write;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

#[cfg(feature = "integrity")]
use varve::Error;
use varve::{COMMIT_BLOCK_ID, CommitPolicy, IndexPolicy, VarveBlock, varve_format};
// Named only by the verification contracts, which are `integrity`-gated.
#[cfg(feature = "integrity")]
use varve::{IntegrityVerification, ReadLimits, VarveFile};

/// Opens read-only with verification forced on at open.
///
/// These three contracts were written when open-time verification was the only
/// behaviour there was. `IntegrityVerification::DEFAULT` is now `OnDemand`,
/// which moves the refusal from the open to the read that returns the damaged
/// record -- so each contract below now asserts *both*: that declaring `AtOpen`
/// still refuses at open, and that the default still refuses the record.
#[cfg(feature = "integrity")]
fn open_verified_at_open(
    spec: varve::FormatSpec,
    path: &std::path::Path,
) -> varve::Result<VarveFile> {
    VarveFile::open_readonly(
        spec.with_read_limits(
            ReadLimits::MISSING.with_integrity_verification(IntegrityVerification::AtOpen),
        ),
        path,
    )
}

varve_format! {
    pub format DslFormat {
        magic: b"DSLF";
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
        extension: "vdsl";
        index: [scan_on_open, checkpoint_on_flush, block_offset_chain, keyed_offset_chain];
        commit: transaction_marker(on_flush);
        manifest: embedded;
        blocks {
            fixed Point(id = 1) {
                x: u32,
                y: u32,
            }

            variable User(id = 2, key = [id]) {
                id: u64,
                name: String,
                tags: Vec<String> = default,
            }
        }
    }
}

varve_format! {
    pub format ExplicitCommitFormat {
        magic: b"EXPL";
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
        index: [scan_on_open, block_offset_chain];
        commit: transaction_marker(explicit);
        blocks {
            fixed ExplicitPoint(id = 11) {
                value: u32,
            }
        }
    }
}

varve_format! {
    pub format ReplacementDslFormat {
        magic: b"RDSL";
        version: 1;
        limits {
            record_payload: 1_048_576;
        }
        index: [scan_on_open, block_offset_chain, keyed_offset_chain];
        blocks {
            fixed ReplacementPoint(id = 51) {
                value: u32,
            }

            variable ReplacementUser(id = 52, key = [id]) {
                id: u64,
                name: String,
            }
        }
    }
}

varve_format! {
    pub format CrcFooterFormat {
        magic: b"CRCF";
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
        index: [scan_on_open, block_offset_chain];
        commit: record_footer;
        integrity: crc32;
        blocks {
            fixed CrcPoint(id = 21) {
                value: u32,
            }
        }
    }
}

#[cfg(feature = "integrity")]
varve_format! {
    pub format CrcTxnFormat {
        magic: b"CRCT";
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
        index: [scan_on_open, block_offset_chain];
        commit: transaction_marker(on_flush);
        integrity: crc32;
        blocks {
            fixed CrcTxnPoint(id = 31) {
                value: u32,
            }
        }
    }
}

#[cfg(feature = "integrity")]
varve_format! {
    pub format CrcHeaderFormat {
        magic: b"CRCH";
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
        integrity: crc32_with_header;
        blocks {
            fixed CrcHeaderPoint(id = 41) {
                value: u32,
            }
        }
    }
}

#[test]
fn format_first_dsl_generates_typed_api_and_offset_chains() -> varve::Result<()> {
    let path = temp_path("dsl_generated_api");
    cleanup(&path);

    let spec = DslFormat::spec();
    assert_ne!(spec.schema_hash, 0);
    assert_eq!(spec.index_policy, IndexPolicy::new(true, true, true, true));
    assert_eq!(
        spec.commit_policy,
        CommitPolicy::TransactionMarker(varve::TransactionMarkerMode::OnFlush)
    );

    {
        let mut writer = DslFormat::create_writer(&path)?;
        let first_point = writer.push_point(&Point { x: 1, y: 2 })?;
        let second_point = writer.push_point(&Point { x: 3, y: 4 })?;
        assert_eq!(
            second_point.prev_same_block_offset,
            Some(first_point.record_offset)
        );

        let first_user = writer.push_user(&User {
            id: 7,
            name: "Ada".to_string(),
            tags: vec!["compiler".to_string()],
        })?;
        let second_user = writer.push_user(&User {
            id: 7,
            name: "Grace".to_string(),
            tags: Vec::new(),
        })?;
        assert_eq!(
            second_user.prev_same_block_offset,
            Some(first_user.record_offset)
        );
        assert_eq!(
            second_user.prev_same_key_offset,
            Some(first_user.record_offset)
        );
        writer.flush()?;
    }

    assert_eq!(
        &read_marker(&path, DslFormat::spec().magic.len())?,
        b"VARVE3"
    );

    let reader = DslFormat::open_reader(&path)?;
    let points = reader.points()?;
    assert_eq!(points.len(), 2);
    assert_eq!(points.get(1)?.unwrap(), Point { x: 3, y: 4 });

    let users = reader.users()?;
    assert_eq!(users.len(), 1);
    assert_eq!(
        users.get(&7)?.unwrap(),
        User {
            id: 7,
            name: "Grace".to_string(),
            tags: Vec::new(),
        }
    );

    let raw = DslFormat::open_readonly(&path)?;
    assert!(
        raw.index_entries()
            .iter()
            .any(|entry| entry.block_id == COMMIT_BLOCK_ID)
    );
    assert!(
        raw.index_entries()
            .iter()
            .filter(|entry| entry.block_id == Point::ID)
            .all(|entry| entry.footer_offset.is_some())
    );
    let raw_entries = raw.index_entries();
    let user_entries = raw_entries
        .iter()
        .filter(|entry| entry.block_id == User::ID)
        .collect::<Vec<_>>();
    assert_eq!(user_entries.len(), 2);
    assert_eq!(
        user_entries[1].prev_same_block_offset,
        Some(user_entries[0].record_offset)
    );
    assert_eq!(
        user_entries[1].prev_same_key_offset,
        Some(user_entries[0].record_offset)
    );

    cleanup(&path);
    Ok(())
}

#[test]
fn explicit_transaction_marker_controls_reader_visibility() -> varve::Result<()> {
    let path = temp_path("dsl_explicit_commit");
    cleanup(&path);

    let mut writer = ExplicitCommitFormat::create_writer(&path)?;
    writer.push_explicit_point(&ExplicitPoint { value: 10 })?;
    writer.flush()?;

    let reader = ExplicitCommitFormat::open_reader(&path)?;
    assert!(reader.explicit_points()?.is_empty());

    writer.commit()?;
    writer.flush()?;
    drop(writer);

    let reader = ExplicitCommitFormat::open_reader(&path)?;
    let points = reader.explicit_points()?;
    assert_eq!(points.len(), 1);
    assert_eq!(points.get(0)?.unwrap(), ExplicitPoint { value: 10 });

    cleanup(&path);
    Ok(())
}

#[test]
fn durable_commit_marks_visible_records() -> varve::Result<()> {
    let path = temp_path("dsl_durable_commit");
    cleanup(&path);

    let mut writer = ExplicitCommitFormat::create_writer(&path)?;
    writer.push_explicit_point(&ExplicitPoint { value: 42 })?;
    let info = writer.commit_durable()?;
    assert!(info.committed);
    drop(writer);

    let reader = ExplicitCommitFormat::open_reader(&path)?;
    let points = reader.explicit_points()?;
    assert_eq!(points.len(), 1);
    assert_eq!(points.get(0)?.unwrap(), ExplicitPoint { value: 42 });

    cleanup(&path);
    Ok(())
}

#[test]
fn typed_replacement_translates_keyed_tails_after_resize() -> varve::Result<()> {
    let path = temp_path("typed_replacement_tails");
    cleanup(&path);

    let mut writer = ReplacementDslFormat::create_writer(&path)?;
    writer.push_replacement_point(&ReplacementPoint { value: 1 })?;
    let point_replacement = ReplacementDslFormatWrite::replace_replacement_point(
        &mut writer,
        0,
        &ReplacementPoint { value: 2 },
    )?;
    assert_eq!(
        point_replacement.old_physical_len,
        point_replacement.new_physical_len
    );

    writer.push_replacement_user(&ReplacementUser {
        id: 1,
        name: "a".to_string(),
    })?;
    let later = writer.push_replacement_user(&ReplacementUser {
        id: 2,
        name: "later".to_string(),
    })?;
    assert!(matches!(
        writer.replace_replacement_user(
            0,
            &ReplacementUser {
                id: 9,
                name: "different key".to_string(),
            },
        ),
        Err(varve::Error::ReplacementKeyMismatch),
    ));
    let replacement = writer.replace_replacement_user(
        0,
        &ReplacementUser {
            id: 1,
            name: "a much longer replacement value".to_string(),
        },
    )?;
    assert!(replacement.new_physical_len > replacement.old_physical_len);

    let appended = writer.push_replacement_user(&ReplacementUser {
        id: 2,
        name: "newest".to_string(),
    })?;
    assert_eq!(
        appended.prev_same_key_offset,
        Some(replacement.translate_record_offset(later.record_offset)?),
    );
    writer.flush()?;
    drop(writer);

    let reader = ReplacementDslFormat::open_reader(&path)?;
    assert_eq!(
        reader.replacement_points()?.get(0)?,
        Some(ReplacementPoint { value: 2 }),
    );
    assert_eq!(
        reader.replacement_users()?.get(&1)?,
        Some(ReplacementUser {
            id: 1,
            name: "a much longer replacement value".to_string(),
        }),
    );

    cleanup(&path);
    Ok(())
}

#[test]
fn transaction_writer_open_truncates_uncommitted_tail() -> varve::Result<()> {
    let path = temp_path("dsl_writer_open_truncate");
    cleanup(&path);

    {
        let mut writer = ExplicitCommitFormat::create_writer(&path)?;
        writer.push_explicit_point(&ExplicitPoint { value: 10 })?;
        writer.flush()?;
    }
    let len_with_uncommitted_tail = metadata(&path)?.len();

    {
        let mut writer = ExplicitCommitFormat::open_writer(&path)?;
        assert!(metadata(&path)?.len() < len_with_uncommitted_tail);
        writer.push_explicit_point(&ExplicitPoint { value: 20 })?;
        writer.commit()?;
        writer.flush()?;
    }

    let reader = ExplicitCommitFormat::open_reader(&path)?;
    let points = reader.explicit_points()?;
    assert_eq!(points.len(), 1);
    assert_eq!(points.get(0)?.unwrap(), ExplicitPoint { value: 20 });

    cleanup(&path);
    Ok(())
}

#[cfg(feature = "integrity")]
#[test]
fn crc32_covers_varve3_record_footer() -> varve::Result<()> {
    let path = temp_path("crc_footer");
    cleanup(&path);

    let mut writer = CrcFooterFormat::create_writer(&path)?;
    writer.push_crc_point(&CrcPoint { value: 1 })?;
    let second = writer.push_crc_point(&CrcPoint { value: 2 })?;
    writer.flush()?;
    drop(writer);

    let footer_offset = second.footer_offset.expect("VARVE3 footer");
    tamper_byte(&path, footer_offset + 8)?;
    assert!(matches!(
        open_verified_at_open(CrcFooterFormat::spec(), &path),
        Err(Error::ChecksumMismatch { .. })
    ));
    // And under the default the damage is still caught -- on the read.
    let reader = CrcFooterFormat::open_reader(&path)?;
    assert!(matches!(
        reader.crc_points()?.get(1),
        Err(Error::ChecksumMismatch { .. })
    ));
    drop(reader);

    cleanup(&path);
    Ok(())
}

#[cfg(feature = "integrity")]
#[test]
fn transaction_marker_crc_tail_after_latest_marker_is_ignored() -> varve::Result<()> {
    let path = temp_path("crc_transaction_tail");
    cleanup(&path);

    let mut writer = CrcTxnFormat::create_writer(&path)?;
    writer.push_crc_txn_point(&CrcTxnPoint { value: 1 })?;
    writer.flush()?;
    let tail = writer.push_crc_txn_point(&CrcTxnPoint { value: 2 })?;
    drop(writer);

    tamper_byte(&path, tail.payload_offset)?;
    let reader = CrcTxnFormat::open_reader(&path)?;
    let points = reader.crc_txn_points()?;
    assert_eq!(points.len(), 1);
    assert_eq!(points.get(0)?.unwrap(), CrcTxnPoint { value: 1 });

    cleanup(&path);
    Ok(())
}

#[cfg(feature = "integrity")]
#[test]
fn crc_transaction_fixed_cow_preserves_old_snapshot_and_footer_chain() -> varve::Result<()> {
    let path = temp_path("crc_transaction_cow");
    cleanup(&path);

    {
        let mut writer = CrcTxnFormat::create_writer(&path)?;
        writer.push_crc_txn_point(&CrcTxnPoint { value: 1 })?;
        writer.flush()?;
    }
    let old_reader = CrcTxnFormat::open_reader(&path)?;
    {
        let mut writer = CrcTxnFormat::spec().open(&path)?;
        writer.replace_fixed(0, &CrcTxnPoint { value: 2 })?;
        writer.flush()?;
    }

    assert_eq!(
        old_reader.crc_txn_points()?.get(0)?,
        Some(CrcTxnPoint { value: 1 })
    );
    assert_eq!(
        CrcTxnFormat::open_reader(&path)?.crc_txn_points()?.get(0)?,
        Some(CrcTxnPoint { value: 2 })
    );

    drop(old_reader);
    cleanup(&path);
    Ok(())
}

#[cfg(feature = "integrity")]
#[test]
fn transaction_marker_crc_corruption_before_marker_is_fatal() -> varve::Result<()> {
    let path = temp_path("crc_transaction_committed");
    cleanup(&path);

    let mut writer = CrcTxnFormat::create_writer(&path)?;
    let committed = writer.push_crc_txn_point(&CrcTxnPoint { value: 1 })?;
    writer.flush()?;
    drop(writer);

    tamper_byte(&path, committed.payload_offset)?;
    assert!(matches!(
        open_verified_at_open(CrcTxnFormat::spec(), &path),
        Err(Error::ChecksumMismatch { .. })
    ));
    let reader = CrcTxnFormat::open_reader(&path)?;
    assert!(matches!(
        reader.crc_txn_points()?.get(0),
        Err(Error::ChecksumMismatch { .. })
    ));
    drop(reader);

    cleanup(&path);
    Ok(())
}

#[cfg(feature = "integrity")]
#[test]
fn crc32_with_header_detects_header_tampering() -> varve::Result<()> {
    let path = temp_path("crc_header");
    cleanup(&path);

    let mut writer = CrcHeaderFormat::create_writer(&path)?;
    let info = writer.push_crc_header_point(&CrcHeaderPoint { value: 5 })?;
    writer.flush()?;
    drop(writer);

    tamper_byte(&path, info.record_offset)?;
    assert!(matches!(
        open_verified_at_open(CrcHeaderFormat::spec(), &path),
        Err(Error::ChecksumMismatch { .. })
    ));
    // Under the default this one degrades rather than refusing, and the
    // difference is worth stating: the tampered byte is inside the record
    // header, so the scan indexes the record under a *different* block id and
    // the typed collection simply does not contain it. `AtOpen` above catches
    // it; `OnDemand` turns it into an absence. What must still hold, and is
    // what this asserts, is that no tampered record is ever handed back as
    // valid.
    let reader = CrcHeaderFormat::open_reader(&path)?;
    let observed = reader.crc_header_points()?.get(0);
    assert!(
        !matches!(observed, Ok(Some(_))),
        "a tampered header must never decode as a valid record, got {observed:?}"
    );
    drop(reader);

    cleanup(&path);
    Ok(())
}

fn read_marker(path: &Path, magic_len: usize) -> varve::Result<[u8; 6]> {
    let mut file = OpenOptions::new().read(true).open(path)?;
    file.seek(SeekFrom::Start(magic_len as u64))?;
    let mut marker = [0; 6];
    file.read_exact(&mut marker)?;
    Ok(marker)
}

#[cfg(feature = "integrity")]
fn tamper_byte(path: &Path, offset: u64) -> varve::Result<()> {
    let mut file = OpenOptions::new().read(true).write(true).open(path)?;
    file.seek(SeekFrom::Start(offset))?;
    let mut byte = [0; 1];
    file.read_exact(&mut byte)?;
    file.seek(SeekFrom::Start(offset))?;
    file.write_all(&[byte[0].wrapping_add(1)])?;
    file.flush()?;
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

fn temp_path(name: &str) -> TempPath {
    let dir = tempfile::tempdir().expect("create per-test temp directory");
    let path = dir.path().join(format!(
        "varve_format_dsl_{name}_{}.vrv",
        std::process::id()
    ));
    TempPath { path, _dir: dir }
}

fn cleanup(path: &Path) {
    let _ = remove_file(path);
    let mut lock = path.to_path_buf();
    lock.push(".lock");
    let _ = remove_file(lock);
}
