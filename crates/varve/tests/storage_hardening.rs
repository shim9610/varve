use std::fs::{File, OpenOptions, remove_file};
use std::io::{Seek, SeekFrom, Write};
use std::panic::catch_unwind;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use varve::{Endian, Error, VarveBlock, encode_to_vec, varve_format};

#[derive(Clone, Debug, PartialEq, Eq, VarveBlock)]
#[varve(id = 701, version = 1, kind = "fixed")]
struct StorageValue {
    value: u64,
}

varve_format! {
    pub struct StorageFormat {
        magic: b"VSTOR";
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
        blocks: [StorageValue];
    }
}

#[cfg(feature = "compression-zstd")]
#[derive(Clone, Debug, PartialEq, Eq, VarveBlock)]
#[varve(id = 702, version = 1, kind = "variable")]
struct CompressedValue {
    #[varve(field_id = 1)]
    payload: Vec<u8>,
}

#[cfg(feature = "compression-zstd")]
varve_format! {
    pub struct StorageCompressionFormat {
        magic: b"VSTZ";
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
        compression: variable_blocks(
            zstd,
            level = fast,
            header = record_explicit,
            min_len = 0,
            only_if_smaller = false,
            max_len = 1048576,
        );
        blocks: [CompressedValue];
    }
}

#[test]
fn empty_reopen_starts_sequence_at_zero() -> varve::Result<()> {
    let path = temp_path("empty_sequence");
    cleanup(&path);
    drop(StorageFormat::create(&path)?);

    let mut file = StorageFormat::open(&path)?;
    assert_eq!(file.push(&StorageValue { value: 7 })?, 0);

    drop(file);
    cleanup(&path);
    Ok(())
}

#[test]
fn payload_reads_check_extent_limit_and_physical_end() -> varve::Result<()> {
    let path = temp_path("payload_extent");
    cleanup(&path);
    let value = StorageValue { value: 11 };
    let logical = encode_to_vec(&value, Endian::Little)?;

    let mut file = StorageFormat::create(&path)?;
    file.push(&value)?;
    let entry = file.index_entries()[0].clone();
    drop(file);

    assert_eq!(
        entry.read_payload_limited(&path, logical.len() as u64)?,
        logical
    );
    assert!(matches!(
        entry.read_payload_limited(&path, logical.len() as u64 - 1),
        Err(Error::LimitExceeded {
            resource: "payload",
            ..
        })
    ));
    assert_eq!(
        entry.read_logical_payload_limited(
            StorageFormat::spec(),
            &path,
            logical.len() as u64,
            logical.len() as u64
        )?,
        logical
    );

    let mut beyond_eof = entry.clone();
    beyond_eof.payload_offset = std::fs::metadata(&path)?.len();
    beyond_eof.payload_len = 1;
    assert!(matches!(
        beyond_eof.read_payload(&path),
        Err(Error::UnexpectedEof)
    ));

    let payload_end = entry.payload_offset + entry.payload_len;
    let mut missing_footer = entry.clone();
    missing_footer.footer_offset = Some(payload_end);
    assert!(matches!(
        missing_footer.read_payload_limited(&path, logical.len() as u64),
        Err(Error::UnexpectedEof)
    ));

    let mut misplaced_footer = entry.clone();
    misplaced_footer.footer_offset = Some(payload_end - 1);
    assert!(matches!(
        misplaced_footer.read_payload(&path),
        Err(Error::CorruptTail { .. })
    ));

    let mut overflow = entry;
    overflow.payload_offset = u64::MAX - 1;
    overflow.payload_len = 4;
    overflow.footer_offset = None;
    assert!(matches!(
        overflow.checked_physical_end(),
        Err(Error::LengthOverflow { .. })
    ));
    assert_eq!(overflow.physical_end(), u64::MAX);

    cleanup(&path);
    Ok(())
}

#[test]
fn sparse_payload_rejects_standard_limit_before_reading() -> varve::Result<()> {
    let path = temp_path("sparse_payload_limit");
    cleanup(&path);
    let payload_len = 64 * 1024 * 1024 + 1;
    File::create(&path)?.set_len(payload_len)?;
    let entry = varve::RecordIndexEntry {
        block_id: StorageValue::ID,
        block_version: StorageValue::VERSION,
        flags: 0,
        sequence: 0,
        record_offset: 0,
        payload_offset: 0,
        payload_len,
        checksum: 0,
        uncompressed_len_hint: 0,
        footer_offset: None,
        prev_same_block_offset: None,
        prev_same_key_offset: None,
        committed: true,
    };

    assert!(matches!(
        entry.read_payload(&path),
        Err(Error::LimitExceeded {
            resource: "record payload length",
            ..
        })
    ));

    cleanup(&path);
    Ok(())
}

#[test]
fn hostile_on_disk_payload_lengths_fail_without_panicking_or_allocating() -> varve::Result<()> {
    let path = temp_path("hostile_disk_payload_len");
    cleanup(&path);
    let mut writer = StorageFormat::create(&path)?;
    writer.push(&StorageValue { value: 17 })?;
    let entry = writer.index_entries()[0].clone();
    drop(writer);

    let payload_len_offset = entry.record_offset + 16;
    let file_len = std::fs::metadata(&path)?.len();
    for hostile_len in [file_len, u64::MAX] {
        let mut file = OpenOptions::new().write(true).open(&path)?;
        file.seek(SeekFrom::Start(payload_len_offset))?;
        file.write_all(&hostile_len.to_le_bytes())?;
        drop(file);

        let opened = catch_unwind(|| StorageFormat::open_readonly(&path));
        assert!(opened.is_ok(), "hostile payload length caused a panic");
        assert!(matches!(
            opened.expect("checked above"),
            Err(Error::CorruptTail { .. }) | Err(Error::LimitExceeded { .. })
        ));
    }

    cleanup(&path);
    Ok(())
}

#[cfg(feature = "compression-zstd")]
#[test]
fn logical_limit_does_not_cap_compressed_storage_overhead() -> varve::Result<()> {
    let path = temp_path("logical_compressed_limit");
    cleanup(&path);
    let value = CompressedValue { payload: vec![] };
    let logical = encode_to_vec(&value, Endian::Little)?;

    let mut file = StorageCompressionFormat::create(&path)?;
    file.push(&value)?;
    let entry = file.index_entries()[0].clone();
    assert!(entry.payload_len > logical.len() as u64);
    drop(file);

    assert_eq!(
        entry.read_logical_payload_limited(
            StorageCompressionFormat::spec(),
            &path,
            entry.payload_len,
            logical.len() as u64
        )?,
        logical
    );
    assert!(matches!(
        entry.read_logical_payload_limited(
            StorageCompressionFormat::spec(),
            &path,
            entry.payload_len - 1,
            logical.len() as u64
        ),
        Err(Error::LimitExceeded {
            resource: "payload",
            ..
        })
    ));

    let runtime =
        varve::ReadLimits::missing().with_max_logical_payload_len(logical.len() as u64 - 1);
    let limited_spec = StorageCompressionFormat::spec().with_resource_limits(runtime);
    assert!(matches!(
        entry.read_logical_payload_limited(limited_spec, &path, u64::MAX, u64::MAX,),
        Err(Error::LimitExceeded {
            resource: "logical payload length",
            ..
        })
    ));

    cleanup(&path);
    Ok(())
}

#[cfg(feature = "mmap")]
#[test]
fn mmap_constructor_rejects_a_stale_index_extent() -> varve::Result<()> {
    let path = temp_path("mmap_stale_extent");
    cleanup(&path);
    let mut file = StorageFormat::create(&path)?;
    file.push(&StorageValue { value: 19 })?;
    drop(file);

    let reader = StorageFormat::open_readonly(&path)?;
    let truncated_len = reader.index_entries()[0].payload_offset;
    OpenOptions::new()
        .write(true)
        .open(&path)?
        .set_len(truncated_len)?;

    // SAFETY: Mutation is complete before this call and no mapping is returned.
    assert!(matches!(
        unsafe { reader.mmap_payloads() },
        Err(Error::MmapPayloadOutOfBounds { .. })
    ));

    drop(reader);
    cleanup(&path);
    Ok(())
}

#[cfg(feature = "mmap")]
#[test]
fn mmap_constructor_rejects_truncated_empty_snapshot() -> varve::Result<()> {
    let path = temp_path("mmap_truncated_empty_snapshot");
    cleanup(&path);
    drop(StorageFormat::create(&path)?);

    let reader = StorageFormat::open_readonly(&path)?;
    assert!(reader.index_entries().is_empty());
    OpenOptions::new().write(true).open(&path)?.set_len(1)?;

    let mapped = catch_unwind(|| {
        // SAFETY: Mutation is complete before this call and no mapping is returned.
        unsafe { reader.mmap_payloads() }
    });
    assert!(mapped.is_ok(), "truncated empty snapshot caused a panic");
    assert!(matches!(
        mapped.expect("checked above"),
        Err(Error::MmapPayloadOutOfBounds { .. })
    ));

    drop(reader);
    cleanup(&path);
    Ok(())
}

fn temp_path(label: &str) -> PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    std::env::temp_dir().join(format!(
        "varve_storage_hardening_{label}_{}_{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ))
}

fn cleanup(path: &PathBuf) {
    let _ = remove_file(path);
    let mut lock = path.as_os_str().to_os_string();
    lock.push(".lock");
    let _ = remove_file(PathBuf::from(lock));
}
