use std::fs::{OpenOptions, metadata, remove_file};
#[cfg(feature = "integrity")]
use std::io::Write;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

#[cfg(feature = "integrity")]
use varve::Error;
use varve::{COMMIT_BLOCK_ID, CommitPolicy, IndexPolicy, VarveBlock, varve_format};

varve_format! {
    pub format DslFormat {
        magic: b"DSLF";
        version: 1;
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
    pub format CrcFooterFormat {
        magic: b"CRCF";
        version: 1;
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
    let user_entries = raw
        .index_entries()
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
        CrcFooterFormat::open_reader(&path),
        Err(Error::ChecksumMismatch { .. })
    ));

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
fn transaction_marker_crc_corruption_before_marker_is_fatal() -> varve::Result<()> {
    let path = temp_path("crc_transaction_committed");
    cleanup(&path);

    let mut writer = CrcTxnFormat::create_writer(&path)?;
    let committed = writer.push_crc_txn_point(&CrcTxnPoint { value: 1 })?;
    writer.flush()?;
    drop(writer);

    tamper_byte(&path, committed.payload_offset)?;
    assert!(matches!(
        CrcTxnFormat::open_reader(&path),
        Err(Error::ChecksumMismatch { .. })
    ));

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
        CrcHeaderFormat::open_reader(&path),
        Err(Error::ChecksumMismatch { .. })
    ));

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

fn temp_path(name: &str) -> PathBuf {
    let mut path = std::env::temp_dir();
    path.push(format!(
        "varve_format_dsl_{name}_{}.vrv",
        std::process::id()
    ));
    path
}

fn cleanup(path: &Path) {
    let _ = remove_file(path);
    let mut lock = path.to_path_buf();
    lock.push(".lock");
    let _ = remove_file(lock);
}
