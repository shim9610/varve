#![cfg(all(test, feature = "high-cardinality-dev"))]

use std::{
    alloc::{GlobalAlloc, Layout, System},
    cell::Cell,
    env,
    ffi::OsStr,
    fs::{File, OpenOptions},
    io,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use crate::{
    BlockKind, CommitPolicy, Endian, Error, FormatSpec, IndexPolicy, IntegrityPolicy,
    ManifestPolicy, ReadLimits, RecoveryPolicy, Result, SnapshotFile, VarveBlock, VarveDecode,
    VarveEncode, WireType,
    file::{
        prepare_stream_user_record, read_stream_entry_at, reset_stream_io_counters,
        stream_io_counters,
    },
    scalable_extent::UntrustedRecordPointer,
};

const PIB: u64 = 1 << 50;
const TIB: u64 = 1 << 40;
const MAX_ALLOCATED_BYTES: u64 = 64 * 1024 * 1024;
const SEQUENCE: u64 = 0x1020_3040_5060_7080;
const VALUE: u64 = 0x8877_6655_4433_2211;

thread_local! {
    static COUNTED_ALLOCATIONS: Cell<Option<u64>> = const { Cell::new(None) };
}

struct CountingAllocator;

fn count_allocation() {
    let _ = COUNTED_ALLOCATIONS.try_with(|counter| {
        if let Some(count) = counter.get() {
            counter.set(Some(count.saturating_add(1)));
        }
    });
}

// SAFETY: every operation forwards the original pointer and layout contract
// unchanged to `System`; the thread-local bookkeeping neither dereferences nor
// retains allocation pointers.
unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        count_allocation();
        // SAFETY: the caller supplies the `GlobalAlloc::alloc` layout contract,
        // which is forwarded unchanged to the system allocator.
        unsafe { System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        count_allocation();
        // SAFETY: the caller supplies the `GlobalAlloc::alloc_zeroed` layout
        // contract, which is forwarded unchanged to the system allocator.
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: `ptr` and `layout` came from this allocator, which delegates
        // all allocation operations to `System` without changing either value.
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        count_allocation();
        // SAFETY: the caller's valid reallocation tuple is forwarded unchanged
        // to the same system allocator that produced `ptr`.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static TEST_ALLOCATOR: CountingAllocator = CountingAllocator;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PibProbeBlock(u64);

impl VarveEncode for PibProbeBlock {
    const WIRE_TYPE: WireType = WireType::U64;

    fn encode_varve(&self, encoder: &mut crate::Encoder) -> Result<()> {
        self.0.encode_varve(encoder)
    }
}

impl VarveDecode for PibProbeBlock {
    const WIRE_TYPE: WireType = WireType::U64;

    fn decode_varve(decoder: &mut crate::Decoder<'_>) -> Result<Self> {
        Ok(Self(u64::decode_varve(decoder)?))
    }
}

impl VarveBlock for PibProbeBlock {
    const ID: u32 = 0x5049_4201;
    const VERSION: u16 = 1;
    const KIND: BlockKind = BlockKind::Fixed;
    const ENDIAN: Option<Endian> = Some(Endian::Little);
    const SCHEMA_FINGERPRINT: u64 = 0xBF9F112353AA547B;
    const IS_KEYED: bool = false;
}

#[derive(Debug)]
struct ProbeReport {
    filesystem: String,
    record_offset: u64,
    logical_len: u64,
    allocated_bytes: u64,
}

#[derive(Debug)]
enum ProbeError {
    Unsupported(String),
    Failed(String),
}

struct CleanupGuard {
    path: PathBuf,
}

impl Drop for CleanupGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

static NEXT_FILE_ID: AtomicU64 = AtomicU64::new(0);

#[cfg(feature = "integrity")]
fn require_integrity() {}

#[cfg(not(feature = "integrity"))]
fn require_integrity() {
    panic!(
        "sparse offset probe requires --features high-cardinality-dev,integrity for CRC verification"
    );
}

#[test]
#[ignore = "creates and probes a real sparse file with a logical offset of one PiB"]
fn real_file_positional_io_at_one_pib() {
    require_integrity();

    let required =
        env::var_os("VARVE_REQUIRE_PIB_SPARSE").is_some_and(|value| value == OsStr::new("1"));
    match run_probe() {
        Ok(report) => eprintln!(
            "PiB sparse probe passed: os={} arch={} filesystem={} record_offset={} logical_len={} allocated_bytes={}",
            env::consts::OS,
            env::consts::ARCH,
            report.filesystem,
            report.record_offset,
            report.logical_len,
            report.allocated_bytes
        ),
        Err(ProbeError::Unsupported(reason)) if !required => eprintln!(
            "PiB sparse probe skipped: os={} arch={} reason={reason}",
            env::consts::OS,
            env::consts::ARCH
        ),
        Err(ProbeError::Unsupported(reason)) => {
            panic!("required PiB sparse probe is unsupported: {reason}")
        }
        Err(ProbeError::Failed(reason)) => panic!("PiB sparse probe failed: {reason}"),
    }
}

#[test]
#[ignore = "creates and probes a real sparse file with a logical offset of one TiB"]
fn real_file_positional_io_at_one_tib_smoke() {
    require_integrity();
    match run_probe_at(TIB) {
        Ok(report) => eprintln!(
            "sparse offset smoke passed: os={} arch={} filesystem={} record_offset={} logical_len={} allocated_bytes={}",
            env::consts::OS,
            env::consts::ARCH,
            report.filesystem,
            report.record_offset,
            report.logical_len,
            report.allocated_bytes
        ),
        Err(ProbeError::Unsupported(reason)) => eprintln!(
            "sparse offset smoke skipped: os={} arch={} reason={reason}",
            env::consts::OS,
            env::consts::ARCH
        ),
        Err(ProbeError::Failed(reason)) => panic!("sparse offset smoke failed: {reason}"),
    }
}

fn run_probe() -> std::result::Result<ProbeReport, ProbeError> {
    run_probe_at(PIB)
}

fn run_probe_at(record_offset: u64) -> std::result::Result<ProbeReport, ProbeError> {
    if usize::BITS < 64 {
        return Err(ProbeError::Unsupported(format!(
            "pointer width is {} bits; the sparse-offset probe requires a 64-bit target",
            usize::BITS
        )));
    }

    let directory = env::var_os("VARVE_PIB_TEST_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(env::temp_dir);
    let directory_metadata = directory.metadata().map_err(|error| {
        failed(
            "inspect VARVE_PIB_TEST_DIR (or the process temporary directory)",
            error,
        )
    })?;
    if !directory_metadata.is_dir() {
        return Err(ProbeError::Failed(format!(
            "probe directory is not a directory: {}",
            directory.display()
        )));
    }

    let (guard, file) = create_probe_file(&directory)
        .map_err(|error| failed("create cleanup-guarded probe file", error))?;
    prepare_sparse_file(&file)?;

    let spec = probe_spec();
    let prepared = prepare_stream_user_record(
        spec,
        &PibProbeBlock(VALUE),
        SEQUENCE,
        record_offset,
        None,
        None,
    )
    .map_err(|error| ProbeError::Failed(format!("prepare native stream frame: {error}")))?;
    let physical_len = u64::try_from(prepared.bytes.len())
        .map_err(|_| ProbeError::Failed("native frame length does not fit u64".into()))?;
    let logical_len = record_offset
        .checked_add(physical_len)
        .ok_or_else(|| ProbeError::Failed("sparse native frame extent overflowed u64".into()))?;

    write_all_at(&file, &prepared.bytes, record_offset)
        .map_err(|error| sparse_io_error("write native frame at sparse offset", error))?;
    file.sync_all()
        .map_err(|error| sparse_io_error("sync sparse file", error))?;
    drop(file);

    let reopened =
        File::open(&guard.path).map_err(|error| failed("reopen synced sparse file", error))?;
    let allocation = allocation_info(&guard.path, &reopened)?;
    let actual_len = reopened
        .metadata()
        .map_err(|error| failed("read reopened sparse file metadata", error))?
        .len();
    if actual_len != logical_len {
        return Err(ProbeError::Failed(format!(
            "logical length mismatch after reopen: expected {logical_len}, got {actual_len}"
        )));
    }
    if allocation.allocated_bytes >= MAX_ALLOCATED_BYTES {
        return Err(ProbeError::Unsupported(format!(
            "filesystem {} allocated {} bytes for logical length {}; sparse allocation must be below {} bytes",
            allocation.filesystem, allocation.allocated_bytes, actual_len, MAX_ALLOCATED_BYTES
        )));
    }

    let snapshot = SnapshotFile::new(reopened)
        .map_err(|error| ProbeError::Failed(format!("bind reopened snapshot: {error}")))?;
    let untrusted = UntrustedRecordPointer::new(record_offset, physical_len);
    let validated = snapshot
        .validate(untrusted, true)
        .map_err(|error| ProbeError::Failed(format!("validate sparse record extent: {error}")))?;
    let span = validated.span();
    if span.record_offset().get() != record_offset
        || span.physical_len().get() != physical_len
        || span.end().get() != logical_len
    {
        return Err(ProbeError::Failed(format!(
            "validated framing mismatch: {span:?}"
        )));
    }

    reset_stream_io_counters();
    let point_reads_before = stream_io_counters().2;
    let (past_eof, allocations) = count_allocations_during(|| {
        read_stream_entry_at(spec, &snapshot, record_offset, physical_len + 1)
    });
    let point_reads_after = stream_io_counters().2;
    if !matches!(
        past_eof,
        Err(Error::SnapshotRangeOutOfBounds {
            offset,
            len,
            snapshot_len,
        }) if offset == record_offset && len == physical_len + 1 && snapshot_len == logical_len
    ) {
        return Err(ProbeError::Failed(format!(
            "one-byte-past-EOF pointer returned unexpected result: {past_eof:?}"
        )));
    }
    if point_reads_after != point_reads_before || allocations != 0 {
        return Err(ProbeError::Failed(format!(
            "one-byte-past-EOF pointer reached point I/O/allocation: point reads {point_reads_before}->{point_reads_after}, allocations={allocations}"
        )));
    }

    let entry = read_stream_entry_at(spec, &snapshot, record_offset, physical_len)
        .map_err(|error| ProbeError::Failed(format!("point-read sparse frame: {error}")))?;
    if stream_io_counters().2 != point_reads_before + 1 {
        return Err(ProbeError::Failed(
            "valid sparse point read did not move the point-read counter exactly once".into(),
        ));
    }
    if entry.block_id != PibProbeBlock::ID
        || entry.block_version != PibProbeBlock::VERSION
        || entry.sequence != SEQUENCE
        || entry.record_offset != record_offset
        || entry.payload_offset != prepared.info.payload_offset
        || entry.payload_len != prepared.info.payload_len
        || entry.footer_offset != prepared.info.footer_offset
        || entry.checksum != prepared.checksum
        || entry.checked_physical_end().ok() != Some(logical_len)
    {
        return Err(ProbeError::Failed(format!(
            "native frame metadata mismatch: prepared={prepared:?}, read={entry:?}"
        )));
    }

    let payload = entry
        .read_payload_snapshot(spec, &snapshot)
        .map_err(|error| ProbeError::Failed(format!("verify/read snapshot payload: {error}")))?;
    if payload != VALUE.to_le_bytes() {
        return Err(ProbeError::Failed(format!(
            "payload mismatch: expected {:02x?}, got {:02x?}",
            VALUE.to_le_bytes(),
            payload
        )));
    }
    let decoded = crate::decode_from_slice::<PibProbeBlock>(&payload, Endian::Little)
        .map_err(|error| ProbeError::Failed(format!("typed-decode sparse payload: {error}")))?;
    if decoded != PibProbeBlock(VALUE) {
        return Err(ProbeError::Failed(format!(
            "typed payload mismatch: expected {VALUE:#018x}, got {:#018x}",
            decoded.0
        )));
    }

    let footer_offset = entry
        .footer_offset
        .ok_or_else(|| ProbeError::Failed("bounded native frame has no footer".into()))?;
    let footer_len = crate::native_layout::native_record_footer_len();
    let footer = snapshot
        .read_vec_at(footer_offset, footer_len, footer_len, "sparse probe footer")
        .map_err(|error| ProbeError::Failed(format!("read native frame footer: {error}")))?;
    let expected_footer = &prepared.bytes[prepared.bytes.len() - footer.len()..];
    if footer != expected_footer
        || entry.prev_same_block_offset.is_some()
        || entry.prev_same_key_offset.is_some()
    {
        return Err(ProbeError::Failed("native footer framing mismatch".into()));
    }

    let mut crc = crc32fast::Hasher::new();
    crc.update(&payload);
    crc.update(&footer);
    let actual_crc = crc.finalize();
    if actual_crc != prepared.checksum || actual_crc != entry.checksum {
        return Err(ProbeError::Failed(format!(
            "CRC mismatch: prepared={}, entry={}, recomputed={actual_crc}",
            prepared.checksum, entry.checksum
        )));
    }

    drop(snapshot);
    Ok(ProbeReport {
        filesystem: allocation.filesystem,
        record_offset,
        logical_len: actual_len,
        allocated_bytes: allocation.allocated_bytes,
    })
}

fn probe_spec() -> FormatSpec {
    FormatSpec::new(
        b"VPIB",
        1,
        Endian::Little,
        0,
        IndexPolicy::ScanOnOpen,
        IntegrityPolicy::Crc32,
        RecoveryPolicy::Strict,
        ManifestPolicy::None,
        &[],
    )
    .with_commit_policy(CommitPolicy::RecordFooter)
    .with_read_limits(ReadLimits::finite_all(u64::MAX))
}

pub(crate) fn count_allocations_during<T>(operation: impl FnOnce() -> T) -> (T, u64) {
    COUNTED_ALLOCATIONS.with(|counter| {
        assert!(counter.replace(Some(0)).is_none());
    });
    let output = operation();
    let allocations = COUNTED_ALLOCATIONS.with(|counter| {
        counter
            .replace(None)
            .expect("allocation counter was enabled for the operation")
    });
    (output, allocations)
}

fn create_probe_file(directory: &Path) -> io::Result<(CleanupGuard, File)> {
    for _ in 0..64 {
        let id = NEXT_FILE_ID.fetch_add(1, Ordering::Relaxed);
        let path = directory.join(format!("varve-pib-probe-{}-{id}.bin", std::process::id()));
        match OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(file) => return Ok((CleanupGuard { path }, file)),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "could not reserve a unique PiB probe filename after 64 attempts",
    ))
}

fn failed(stage: &str, error: impl std::fmt::Display) -> ProbeError {
    ProbeError::Failed(format!("{stage}: {error}"))
}

fn sparse_io_error(stage: &str, error: io::Error) -> ProbeError {
    if matches!(
        error.kind(),
        io::ErrorKind::Unsupported
            | io::ErrorKind::InvalidInput
            | io::ErrorKind::FileTooLarge
            | io::ErrorKind::StorageFull
    ) {
        ProbeError::Unsupported(format!("{stage}: {error}"))
    } else {
        failed(stage, error)
    }
}

struct AllocationInfo {
    filesystem: String,
    allocated_bytes: u64,
}

#[cfg(unix)]
fn prepare_sparse_file(_file: &File) -> std::result::Result<(), ProbeError> {
    // POSIX writes beyond EOF create a hole; allocation is verified after sync
    // through `MetadataExt::blocks` rather than assumed from logical length.
    Ok(())
}

#[cfg(windows)]
fn prepare_sparse_file(file: &File) -> std::result::Result<(), ProbeError> {
    use std::{os::windows::io::AsRawHandle, ptr};
    use windows_sys::Win32::System::{IO::DeviceIoControl, Ioctl::FSCTL_SET_SPARSE};

    let mut returned = 0;
    // SAFETY: the handle is borrowed from a live `File`; `FSCTL_SET_SPARSE`
    // accepts null input/output buffers, and `returned` is a valid out pointer.
    let marked = unsafe {
        DeviceIoControl(
            file.as_raw_handle() as _,
            FSCTL_SET_SPARSE,
            ptr::null(),
            0,
            ptr::null_mut(),
            0,
            &mut returned,
            ptr::null_mut(),
        )
    };
    if marked == 0 {
        return Err(ProbeError::Unsupported(format!(
            "FSCTL_SET_SPARSE rejected the probe file: {}",
            io::Error::last_os_error()
        )));
    }
    Ok(())
}

#[cfg(not(any(unix, windows)))]
fn prepare_sparse_file(_file: &File) -> std::result::Result<(), ProbeError> {
    Err(ProbeError::Unsupported(format!(
        "{} has no implemented sparse-file marking/allocation probe",
        env::consts::OS
    )))
}

#[cfg(unix)]
fn write_all_at(file: &File, bytes: &[u8], offset: u64) -> io::Result<()> {
    use std::os::unix::fs::FileExt;

    let mut written = 0usize;
    while written < bytes.len() {
        let position = offset
            .checked_add(written as u64)
            .ok_or_else(|| io::Error::other("PiB positional write overflow"))?;
        match file.write_at(&bytes[written..], position) {
            Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
            Ok(count) => written += count,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

#[cfg(windows)]
fn write_all_at(file: &File, bytes: &[u8], offset: u64) -> io::Result<()> {
    use std::os::windows::fs::FileExt;

    let mut written = 0usize;
    while written < bytes.len() {
        let position = offset
            .checked_add(written as u64)
            .ok_or_else(|| io::Error::other("PiB positional write overflow"))?;
        match file.seek_write(&bytes[written..], position) {
            Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
            Ok(count) => written += count,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

#[cfg(not(any(unix, windows)))]
fn write_all_at(_file: &File, _bytes: &[u8], _offset: u64) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "no native positional file-write implementation",
    ))
}

#[cfg(unix)]
fn allocation_info(_path: &Path, file: &File) -> std::result::Result<AllocationInfo, ProbeError> {
    use std::os::unix::fs::MetadataExt;

    let metadata = file
        .metadata()
        .map_err(|error| failed("read Unix sparse allocation metadata", error))?;
    let allocated_bytes = metadata.blocks().checked_mul(512).ok_or_else(|| {
        ProbeError::Failed("Unix st_blocks allocation byte count overflowed u64".into())
    })?;
    Ok(AllocationInfo {
        filesystem: format!("unix-device-{}", metadata.dev()),
        allocated_bytes,
    })
}

#[cfg(windows)]
fn allocation_info(path: &Path, _file: &File) -> std::result::Result<AllocationInfo, ProbeError> {
    use std::{os::windows::ffi::OsStrExt, ptr};
    use windows_sys::Win32::{
        Foundation::{GetLastError, SetLastError},
        Storage::FileSystem::{
            GetCompressedFileSizeW, GetVolumeInformationW, GetVolumePathNameW, INVALID_FILE_SIZE,
        },
    };

    let wide_path: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let mut high = 0u32;
    // SAFETY: `wide_path` is NUL-terminated and `high` is a valid writable
    // pointer. Clearing last-error is required to disambiguate 0xffff_ffff.
    let low = unsafe {
        SetLastError(0);
        GetCompressedFileSizeW(wide_path.as_ptr(), &mut high)
    };
    // SAFETY: `GetLastError` has no preconditions and is read immediately after
    // `GetCompressedFileSizeW` on this thread.
    let last_error = unsafe { GetLastError() };
    if low == INVALID_FILE_SIZE && last_error != 0 {
        return Err(failed(
            "read Windows sparse allocation metadata",
            io::Error::from_raw_os_error(last_error as i32),
        ));
    }

    let mut volume_path = [0u16; 512];
    // SAFETY: all buffers are writable for their advertised lengths and the
    // input path is NUL-terminated.
    let volume_ok = unsafe {
        GetVolumePathNameW(
            wide_path.as_ptr(),
            volume_path.as_mut_ptr(),
            volume_path.len() as u32,
        )
    };
    if volume_ok == 0 {
        return Err(failed(
            "resolve Windows filesystem volume",
            io::Error::last_os_error(),
        ));
    }

    let mut filesystem_name = [0u16; 128];
    // SAFETY: `volume_path` was populated as a NUL-terminated path by
    // `GetVolumePathNameW`; optional output pointers are null, and the
    // filesystem-name buffer is writable for its advertised length.
    let info_ok = unsafe {
        GetVolumeInformationW(
            volume_path.as_ptr(),
            ptr::null_mut(),
            0,
            ptr::null_mut(),
            ptr::null_mut(),
            ptr::null_mut(),
            filesystem_name.as_mut_ptr(),
            filesystem_name.len() as u32,
        )
    };
    if info_ok == 0 {
        return Err(failed(
            "identify Windows filesystem",
            io::Error::last_os_error(),
        ));
    }
    let name_len = filesystem_name
        .iter()
        .position(|value| *value == 0)
        .unwrap_or(filesystem_name.len());

    Ok(AllocationInfo {
        filesystem: String::from_utf16_lossy(&filesystem_name[..name_len]),
        allocated_bytes: (u64::from(high) << 32) | u64::from(low),
    })
}

#[cfg(not(any(unix, windows)))]
fn allocation_info(_path: &Path, _file: &File) -> std::result::Result<AllocationInfo, ProbeError> {
    Err(ProbeError::Unsupported(format!(
        "{} has no implemented filesystem allocation metadata source",
        env::consts::OS
    )))
}
