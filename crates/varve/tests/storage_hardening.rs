#[cfg(feature = "mmap")]
use std::fs::OpenOptions;
use std::fs::remove_file;
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
