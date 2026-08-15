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
        mutable_flags: 0,
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
        "varve_storage_hardening_{label}_{}_{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    TempPath { path, _dir: dir }
}

fn cleanup(path: &PathBuf) {
    let _ = remove_file(path);
    let mut lock = path.as_os_str().to_os_string();
    lock.push(".lock");
    let _ = remove_file(PathBuf::from(lock));
}

// STO3-01 (report finding STO-01): keyed compact/merge opens its rewrite temp
// through `VarveFile::create`, which creates `<temp>.lock`. Dropping that
// writer cleared the marker's contents but left the file, so a *successful*
// compact deposited an empty marker for a pathname whose native file no longer
// exists. Markers for real user paths are deliberately persistent stable
// identities; only this generated, exclusively owned temp pathname is litter.
//
// DUR3-01 (report finding DUR-01): `sync` synced file contents but never the
// parent directory for a pathname the handle created, while atomic replacement
// did. `sync` now makes the created pathname durable too, once.

#[derive(Clone, Debug, PartialEq, varve::VarveBlock)]
#[varve(id = 711, version = 1, kind = "variable", key = "key")]
struct StorageKeyedValue {
    #[varve(field_id = 1)]
    key: u64,
    #[varve(field_id = 2)]
    payload: String,
}

#[derive(Clone, Debug, PartialEq, varve::VarveBlock)]
#[varve(id = 712, version = 1, kind = "variable")]
struct StorageKeyedOp {
    #[varve(field_id = 1)]
    rename_to: String,
}

impl varve::VarveMerge for StorageKeyedValue {
    type Op = StorageKeyedOp;

    fn apply_op(&mut self, op: Self::Op) -> varve::Result<()> {
        self.payload = op.rename_to;
        Ok(())
    }
}

varve_format! {
    pub struct StorageKeyedFormat {
        magic: b"VSTKY";
        version: 1;
        endian: little;
        blocks: [StorageKeyedValue, StorageKeyedOp];
    }
}

fn lock_marker(path: &std::path::Path) -> PathBuf {
    let mut marker = path.as_os_str().to_os_string();
    marker.push(".lock");
    PathBuf::from(marker)
}

/// Names of leftover internal rewrite artifacts in `directory`.
fn rewrite_leftovers(directory: &std::path::Path) -> varve::Result<Vec<String>> {
    let mut leftovers = Vec::new();
    for entry in std::fs::read_dir(directory)? {
        let name = entry?.file_name().to_string_lossy().into_owned();
        if name.contains(".rewrite.") {
            leftovers.push(name);
        }
    }
    leftovers.sort();
    Ok(leftovers)
}

/// STO3-01: the reviewers' fixture. A compact that succeeds must leave nothing
/// behind for the temp it generated, while the markers for the caller's own
/// paths keep behaving exactly as designed.
#[test]
fn successful_keyed_compact_leaves_no_marker_for_its_internal_temp() -> varve::Result<()> {
    let directory = tempfile::tempdir()?;
    let input = directory.path().join("compact-input.varve");
    let output = directory.path().join("compact-output.varve");

    {
        let mut file = StorageKeyedFormat::create(&input)?;
        for key in 0..64u64 {
            file.push(&StorageKeyedValue {
                key,
                payload: format!("v0-{key}"),
            })?;
        }
        // Supersede half the keys so the compact actually drops records.
        for key in 0..32u64 {
            file.push(&StorageKeyedValue {
                key,
                payload: format!("v1-{key}"),
            })?;
        }
        file.flush()?;
    }

    assert!(
        rewrite_leftovers(directory.path())?.is_empty(),
        "the fixture must start clean"
    );

    // STO4-P2: this test publishes through the shared atomic-replacement
    // path, whose indeterminate-failure injection counter is process-wide, so
    // it holds the same gate as the fault-injection tests below rather than
    // consuming a count they armed.
    #[cfg(feature = "scalable-fault-injection")]
    let _gate = fault_gate();

    varve::compact_keyed_file::<StorageKeyedValue, _>(
        StorageKeyedFormat::spec(),
        input.as_path(),
        output.as_path(),
    )?;

    let leftovers = rewrite_leftovers(directory.path())?;
    assert!(
        leftovers.is_empty(),
        "a successful compact must leave no internal rewrite artifact: {leftovers:?}"
    );

    // The publication really happened, so the check above is about hygiene and
    // not about a compact that silently did nothing.
    let compacted = StorageKeyedFormat::open(&output)?;
    assert_eq!(compacted.blocks::<StorageKeyedValue>()?.len(), 64);
    drop(compacted);

    // Markers for the caller's own paths are stable identities and stay put:
    // the temp cleanup must not have generalized into deleting user markers.
    assert!(
        lock_marker(&output).exists(),
        "the output's own writer-lock marker is a stable identity and must remain"
    );
    // And they still work: the output is lockable again, from the marker that
    // survived.
    {
        let mut reopened = StorageKeyedFormat::spec().open_writer(&output)?;
        reopened.push(&StorageKeyedValue {
            key: 1_000,
            payload: "after".to_string(),
        })?;
        reopened.flush()?;
    }
    assert!(lock_marker(&output).exists());
    Ok(())
}

/// STO3-01: a compact that fails before publication must not leave the temp's
/// marker either - the temp itself is deleted, so its marker would name
/// nothing.
#[test]
fn refused_keyed_compact_leaves_no_marker_for_its_internal_temp() -> varve::Result<()> {
    let directory = tempfile::tempdir()?;
    let input = directory.path().join("refused-input.varve");
    let output = directory.path().join("refused-output.varve");

    {
        let mut file = StorageKeyedFormat::create(&input)?;
        for key in 0..16u64 {
            file.push(&StorageKeyedValue {
                key,
                payload: format!("v-{key}"),
            })?;
        }
        file.flush()?;
    }

    let refused = varve::compact_keyed_file_with_key_limit::<StorageKeyedValue, _>(
        StorageKeyedFormat::spec(),
        input.as_path(),
        output.as_path(),
        8,
    )
    .expect_err("a key ceiling below the input cardinality must be refused");
    assert!(
        matches!(refused, Error::LimitExceeded { .. }),
        "expected a typed cardinality refusal, got {refused:?}"
    );

    assert!(!output.exists(), "a refused compact must publish nothing");
    let leftovers = rewrite_leftovers(directory.path())?;
    assert!(
        leftovers.is_empty(),
        "a refused compact must leave no internal rewrite artifact: {leftovers:?}"
    );
    Ok(())
}

/// DUR3-01: `sync` on a file this handle created reports durability for the
/// pathname as well as the contents, and repeating it stays cheap and typed.
#[test]
fn create_and_sync_reports_pathname_durability() -> varve::Result<()> {
    let path = temp_path("create-sync-durability");
    let mut file = StorageFormat::create(&*path)?;
    file.push(&StorageValue { value: 11 })?;
    file.flush()?;
    // A failure here is reported as a typed pending-parent-sync error rather
    // than a silent success; on a working filesystem it must succeed.
    file.sync()?;
    file.push(&StorageValue { value: 12 })?;
    file.flush()?;
    file.sync()?;
    drop(file);

    let reopened = StorageFormat::open(&*path)?;
    assert_eq!(reopened.blocks::<StorageValue>()?.len(), 2);
    drop(reopened);

    // An existing pathname is not re-established by opening it, and syncing a
    // reopened writer still succeeds.
    let mut writer = StorageFormat::spec().open_writer(&*path)?;
    writer.push(&StorageValue { value: 13 })?;
    writer.flush()?;
    writer.sync()?;
    drop(writer);

    cleanup(&path);
    Ok(())
}

// STO4-P2 (report finding, P2 rewrite-temp): `write_keyed_values_atomically`
// created its rewrite temp and only afterwards entered the scope responsible
// for deleting it, so a failure in between - the permission copy - returned
// `Err` with an empty temp left in the output's directory. The temp is now
// owned by an RAII guard from the moment it exists, and the guard is *disarmed*
// only where retention is the deliberate contract: after a successful rename,
// and after an indeterminate publication whose temp may be the only surviving
// copy of the new generation.

/// Serialises tests that depend on the process-wide replacement fault counter.
#[cfg(feature = "scalable-fault-injection")]
fn fault_gate() -> std::sync::MutexGuard<'static, ()> {
    static GATE: std::sync::Mutex<()> = std::sync::Mutex::new(());
    GATE.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(feature = "scalable-fault-injection")]
fn write_keyed_merge_input(path: &std::path::Path, keys: u64) -> varve::Result<()> {
    let mut file = StorageKeyedFormat::create(path)?;
    for key in 0..keys {
        file.push(&StorageKeyedValue {
            key,
            payload: format!("v-{key}"),
        })?;
    }
    file.flush()?;
    Ok(())
}

/// STO4-P2: a failure after the temp exists but before the publication scope
/// must leave no rewrite artifact behind.
#[cfg(feature = "scalable-fault-injection")]
#[test]
fn rewrite_temp_is_removed_when_preparation_fails_after_creation() -> varve::Result<()> {
    let _gate = fault_gate();
    let directory = tempfile::tempdir()?;
    let input = directory.path().join("raii-input.varve");
    let output = directory.path().join("raii-output.varve");
    write_keyed_merge_input(&input, 8)?;

    assert!(
        rewrite_leftovers(directory.path())?.is_empty(),
        "the fixture must start clean"
    );

    varve::VarveFile::inject_rewrite_temp_preparation_failures(1);
    let error = varve::compact_keyed_file::<StorageKeyedValue, _>(
        StorageKeyedFormat::spec(),
        input.as_path(),
        output.as_path(),
    )
    .expect_err("the injected preparation failure must fail the compact");
    varve::VarveFile::inject_rewrite_temp_preparation_failures(0);
    assert!(
        matches!(error, Error::Io(_)),
        "expected the injected io failure, got {error:?}"
    );

    assert!(!output.exists(), "a failed compact must publish nothing");
    let leftovers = rewrite_leftovers(directory.path())?;
    assert!(
        leftovers.is_empty(),
        "a failure between temp creation and publication must leave no \
         rewrite artifact: {leftovers:?}"
    );

    // The guard did not generalise into deleting a temp it should keep, and
    // the next compact still works.
    varve::compact_keyed_file::<StorageKeyedValue, _>(
        StorageKeyedFormat::spec(),
        input.as_path(),
        output.as_path(),
    )?;
    assert!(
        rewrite_leftovers(directory.path())?.is_empty(),
        "a successful compact must still leave nothing behind"
    );
    let compacted = StorageKeyedFormat::open(&output)?;
    assert_eq!(compacted.blocks::<StorageKeyedValue>()?.len(), 8);
    Ok(())
}

/// STO4-P2: the guard must not touch the one temp whose retention is the
/// documented contract (DUR2-01).
#[cfg(feature = "scalable-fault-injection")]
#[test]
fn indeterminate_merge_publication_still_retains_its_rewrite_temp() -> varve::Result<()> {
    let _gate = fault_gate();
    let directory = tempfile::tempdir()?;
    let input = directory.path().join("indeterminate-input.varve");
    let output = directory.path().join("indeterminate-output.varve");
    write_keyed_merge_input(&input, 8)?;

    varve::VarveFile::inject_replace_indeterminate_failures(1);
    let error = varve::compact_keyed_file::<StorageKeyedValue, _>(
        StorageKeyedFormat::spec(),
        input.as_path(),
        output.as_path(),
    )
    .expect_err("the injected indeterminate publication must fail the compact");
    varve::VarveFile::inject_replace_indeterminate_failures(0);
    assert!(
        matches!(error, Error::ReplacePublicationIndeterminate { .. }),
        "expected a typed indeterminate publication, got {error:?}"
    );

    let leftovers = rewrite_leftovers(directory.path())?;
    assert!(
        leftovers.iter().any(|name| !name.ends_with(".lock")),
        "the temp may be the only surviving copy of the new generation and \
         must be preserved for reconciliation: {leftovers:?}"
    );
    Ok(())
}

// F-07: the diagnostic `<target>.lock` marker is truncated and rewritten on
// acquisition and truncated again on drop. Those writes must never reach an
// object the marker path merely aliases. Both aliasing shapes are refused
// with `Error::WriterLockMarkerNotDedicated`, on both platforms, and the
// foreign object must be observably untouched afterwards.

/// The unaliased case still works: this is the negative control that proves
/// the two tests below fail for the aliasing and not for the check itself.
#[test]
fn an_ordinary_lock_marker_is_still_accepted() -> varve::Result<()> {
    let path = temp_path("marker_ordinary");
    cleanup(&path);
    {
        let mut writer = StorageFormat::create(&path)?;
        writer.push(&StorageValue { value: 1 })?;
        writer.flush()?;
    }
    assert!(lock_marker(&path).is_file());
    cleanup(&path);
    Ok(())
}

#[cfg(unix)]
#[test]
fn a_hard_linked_lock_marker_is_refused_and_left_untouched() -> varve::Result<()> {
    let path = temp_path("marker_hardlink");
    cleanup(&path);
    let directory = path.parent().expect("temp parent").to_path_buf();
    let victim = directory.join("unrelated-empty-file");
    std::fs::write(&victim, b"")?;
    std::fs::hard_link(&victim, lock_marker(&path))?;

    let error = StorageFormat::create(&path).expect_err("an aliased marker must be refused");
    assert!(
        matches!(
            error,
            Error::WriterLockMarkerNotDedicated {
                reason: "the marker object has more than one hard link",
                ..
            }
        ),
        "expected a typed marker refusal, got {error:?}"
    );
    assert_eq!(
        std::fs::metadata(&victim)?.len(),
        0,
        "the aliased object must not have been written through"
    );
    let _ = remove_file(lock_marker(&path));
    cleanup(&path);
    Ok(())
}

#[cfg(unix)]
#[test]
fn a_symlinked_lock_marker_is_refused_and_left_untouched() -> varve::Result<()> {
    let path = temp_path("marker_symlink");
    cleanup(&path);
    let directory = path.parent().expect("temp parent").to_path_buf();
    let victim = directory.join("unrelated-symlink-target");
    std::fs::write(&victim, b"")?;
    std::os::unix::fs::symlink(&victim, lock_marker(&path))?;

    let error = StorageFormat::create(&path).expect_err("a symlinked marker must be refused");
    assert!(
        matches!(
            error,
            Error::WriterLockMarkerNotDedicated {
                reason: "the final path component is a symbolic link",
                ..
            }
        ),
        "expected a typed marker refusal, got {error:?}"
    );
    assert_eq!(
        std::fs::metadata(&victim)?.len(),
        0,
        "the symlink target must not have been written through"
    );
    let _ = remove_file(lock_marker(&path));
    cleanup(&path);
    Ok(())
}

/// Windows hard links need no privilege; symbolic links do, so the reparse
/// case is covered by `a_reparse_point_lock_marker_is_refused` only when the
/// host grants the privilege, and is skipped rather than failing otherwise.
#[cfg(windows)]
#[test]
fn a_hard_linked_lock_marker_is_refused_and_left_untouched() -> varve::Result<()> {
    let path = temp_path("marker_hardlink");
    cleanup(&path);
    let directory = path.parent().expect("temp parent").to_path_buf();
    let victim = directory.join("unrelated-empty-file");
    std::fs::write(&victim, b"")?;
    std::fs::hard_link(&victim, lock_marker(&path))?;

    let error = StorageFormat::create(&path).expect_err("an aliased marker must be refused");
    assert!(
        matches!(
            error,
            Error::WriterLockMarkerNotDedicated {
                reason: "the marker object has more than one hard link",
                ..
            }
        ),
        "expected a typed marker refusal, got {error:?}"
    );
    assert_eq!(
        std::fs::metadata(&victim)?.len(),
        0,
        "the aliased object must not have been written through"
    );
    let _ = remove_file(lock_marker(&path));
    cleanup(&path);
    Ok(())
}

#[cfg(windows)]
#[test]
fn a_reparse_point_lock_marker_is_refused_and_left_untouched() -> varve::Result<()> {
    let path = temp_path("marker_reparse");
    cleanup(&path);
    let directory = path.parent().expect("temp parent").to_path_buf();
    let victim = directory.join("unrelated-symlink-target");
    std::fs::write(&victim, b"")?;
    if std::os::windows::fs::symlink_file(&victim, lock_marker(&path)).is_err() {
        // Creating a symbolic link needs SeCreateSymbolicLinkPrivilege or
        // developer mode. Without it there is nothing to prove here; the hard
        // link test above still covers the aliasing class on this platform.
        // Announced rather than silently skipped, so a passing run is never
        // mistaken for evidence the reparse branch was exercised.
        eprintln!(
            "SKIPPED a_reparse_point_lock_marker_is_refused_and_left_untouched: \
             this host does not grant symbolic-link creation"
        );
        cleanup(&path);
        return Ok(());
    }

    let error = StorageFormat::create(&path).expect_err("a reparse marker must be refused");
    assert!(
        matches!(
            error,
            Error::WriterLockMarkerNotDedicated {
                reason: "the final path component is a reparse point",
                ..
            }
        ),
        "expected a typed marker refusal, got {error:?}"
    );
    assert_eq!(
        std::fs::metadata(&victim)?.len(),
        0,
        "the reparse target must not have been written through"
    );
    let _ = remove_file(lock_marker(&path));
    cleanup(&path);
    Ok(())
}

/// F-08: `docs/custom-codec-guide.md` gained a mandatory
/// "Charge Every Owned Allocation" section whose compliant example and
/// negative self-test are the guide's whole answer to "how do I know my codec
/// charges?". Markdown code blocks compile in no job, so a copy of that
/// example lives here: if the guide's example stops compiling or its self-test
/// stops proving what it claims, this fails. Keep the two in sync - the code
/// below is the two `rust` blocks of that section, unchanged apart from the
/// module indentation rustfmt requires here.
mod custom_codec_guide_charging_example {
    use std::mem::size_of;

    use varve::{Decoder, Encoder, Error, VarveDecode, VarveEncode, WireType};

    #[derive(Clone, Debug, PartialEq)]
    struct Samples(Vec<f64>);

    impl VarveEncode for Samples {
        const WIRE_TYPE: WireType = WireType::Bytes;
        const SCHEMA_ID: u64 = 0x53_41_4d_50_00_00_00_01; // "SAMP" v1

        fn encode_varve(&self, encoder: &mut Encoder) -> varve::Result<()> {
            encoder.write_u64(self.0.len() as u64);
            for value in &self.0 {
                encoder.write_u64(value.to_bits());
            }
            Ok(())
        }
    }

    impl VarveDecode for Samples {
        const WIRE_TYPE: WireType = WireType::Bytes;
        const SCHEMA_ID: u64 = <Samples as VarveEncode>::SCHEMA_ID;

        fn decode_varve(decoder: &mut Decoder<'_>) -> varve::Result<Self> {
            // 1. The count is attacker-controlled input, so nothing is sized from
            //    it until it has been validated.
            let count = decoder.read_len()?;
            // 2. A count the input cannot back is malformed, not an allocation
            //    request. This check alone caps `count` at the payload length.
            let wire_bytes = count
                .checked_mul(8)
                .ok_or(Error::LengthOverflow { value: u64::MAX })?;
            if wire_bytes > decoder.remaining() {
                return Err(Error::UnexpectedEof);
            }
            // 3. Charge the RESIDENT cost before taking it. A refusal here returns
            //    `Error::LimitExceeded` with nothing allocated.
            let owned_bytes = (count as u64).checked_mul(size_of::<f64>() as u64).ok_or(
                Error::ResourceArithmeticOverflow {
                    resource: "samples",
                },
            )?;
            decoder.charge_materialization(owned_bytes, "samples")?;
            // 4. Only now is the memory taken, and even then fallibly.
            let mut values = Vec::new();
            values
                .try_reserve_exact(count)
                .map_err(|_| Error::AllocationFailed {
                    resource: "samples",
                    requested: owned_bytes,
                })?;
            // 5. Fill.
            for _ in 0..count {
                values.push(f64::from_bits(decoder.read_u64()?));
            }
            Ok(Self(values))
        }
    }

    #[test]
    fn samples_refuse_to_materialize_past_the_budget() {
        let bytes =
            varve::encode_to_vec(&Samples(vec![1.0; 64]), varve::Endian::Little).expect("encode");

        // 512 owned bytes are needed; 256 are offered.
        let refused =
            Decoder::decode_from_slice_limited::<Samples>(&bytes, varve::Endian::Little, 256);
        assert!(
            matches!(
                refused,
                Err(Error::LimitExceeded {
                    resource: "samples",
                    limit: 256,
                    ..
                })
            ),
            "an uncharged codec returns Ok here: {refused:?}"
        );

        // The control: the same input inside its budget must still decode, so the
        // test cannot pass because the codec is simply broken.
        Decoder::decode_from_slice_limited::<Samples>(&bytes, varve::Endian::Little, 512)
            .expect("the value fits its own budget");
    }
}
