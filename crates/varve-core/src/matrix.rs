use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::sync::Arc;

use crate::codec::encode_to_vec_limited;
use crate::format::ReadLimitKey;
use crate::{
    BlockKind, Decoder, Error, FormatSpec, IntegrityPolicy, MatrixCommitKind, ReadLimits, Result,
    VarveMatrixBlock,
};

const VMAT_MAGIC: &[u8; 4] = b"VMAT";
// PERF-01/PERF-02: layout version 2 replaces the whole-bitmap commit checksum
// with per-page digests and stops explicitly initialising the per-cell CRC and
// validity regions at create. Version 1 artifacts describe a different physical
// representation, so they are rejected as stale-regenerable rather than being
// reinterpreted under the new rules.
const VMAT_VERSION: u16 = 2;
const VMAT_HEADER_LEN: u32 = 160;
const MCRC_MAGIC: &[u8; 4] = b"MCRC";
const MCRC_VERSION: u16 = 2;
const MCRC_HEADER_LEN: u64 = 16;
const CRC_LEN: u64 = 4;
// Commit and validity bitmaps are hashed, stored, and made resident one page at
// a time, so a single bit mutation costs O(page) instead of O(whole bitmap) and
// a never-touched page costs nothing at all.
const BITMAP_PAGE_BYTES: u64 = 4096;
const PAGE_DIGEST_LEN: u64 = 8;
const PAGE_STATE_UNINITIALIZED: u32 = 0;
const PAGE_STATE_INITIALIZED: u32 = 1;
const ZERO_PAGE: [u8; BITMAP_PAGE_BYTES as usize] = [0; BITMAP_PAGE_BYTES as usize];
const MATRIX_BYTES_RESOURCE: &str = "matrix bytes";
const MATRIX_SLOT_PAYLOAD_RESOURCE: &str = "matrix slot payload";
const MATRIX_DESCRIPTOR_RESOURCE: &str = "matrix descriptors";
#[allow(dead_code)]
const MATRIX_SIDECAR_RESOURCE: &str = "matrix sidecar";

type CommitPlan = (String, MatrixCommitKind, u64);
type StoredCommitPlan = (String, MatrixCommitKind, u64, u64, u64);

/// Upper bound on the number of filesystem extents tracked for one matrix file.
///
/// A file fragmented beyond this is dense enough that skipping holes would save
/// nothing, so the query gives up and every byte is read exactly as before.
const MAX_TRACKED_EXTENTS: usize = 8192;

/// The byte ranges of a matrix file that the filesystem reports as allocated,
/// i.e. the only ranges that can possibly hold non-zero bytes.
///
/// PERF-02: the commit maps, per-cell checksums, and validity bitmaps are
/// established as a zero extent by `set_len` and are never written at create.
/// Authenticating a `PAGE_STATE_UNINITIALIZED` page still requires proving that
/// it reads as zero, which previously meant streaming every page of every map
/// at open — `Theta(cell_count / 8)` of sequential reads even for a matrix with
/// no committed cells.
///
/// A range the filesystem reports as unallocated has never been written since
/// the file was created, so it reads as zero and cannot hold stray bytes: the
/// proof is obtained from the filesystem instead of from the bytes. Writing a
/// stray byte into an untouched page necessarily allocates that page, so the
/// detection strength of open is unchanged — corruption is still read and still
/// reported. Open therefore costs `O(bytes actually written)` rather than
/// `O(cell_count)`.
///
/// `None` means "unknown": the platform or filesystem cannot prove anything, in
/// which case every page is read exactly as it was before this change.
#[derive(Clone, Debug, Default)]
struct AllocatedExtents {
    /// Half-open `[start, end)` ranges, sorted and disjoint.
    ranges: Vec<(u64, u64)>,
}

impl AllocatedExtents {
    /// Queries the filesystem, restoring the file cursor before returning.
    fn query(file: &mut File) -> Option<Self> {
        let cursor = file.stream_position().ok()?;
        let ranges = query_allocated_extents(file);
        let restored = file.seek(SeekFrom::Start(cursor)).is_ok();
        let ranges = ranges?;
        if !restored {
            return None;
        }
        // The lookup binary-searches, and skipping a range because the platform
        // answered out of order would turn into an unread page. Verify the
        // ordering rather than trusting it; an unexpected shape means "unknown".
        let ordered = ranges
            .iter()
            .try_fold(0u64, |previous_end, (start, end)| {
                (*start >= previous_end && *end >= *start).then_some(*end)
            })
            .is_some();
        if !ordered {
            return None;
        }
        Some(Self { ranges })
    }

    /// True when `[offset, offset + len)` overlaps any allocated range, i.e.
    /// when the range must be read to establish its contents.
    fn may_hold_data(&self, offset: u64, len: u64) -> bool {
        if len == 0 {
            return false;
        }
        let Some(end) = offset.checked_add(len) else {
            return true;
        };
        let index = self
            .ranges
            .partition_point(|(_, range_end)| *range_end <= offset);
        matches!(self.ranges.get(index), Some((start, _)) if *start < end)
    }
}

/// True when the range must be read, either because the filesystem says it may
/// hold data or because no allocation map could be obtained.
fn range_may_hold_data(extents: Option<&AllocatedExtents>, offset: u64, len: u64) -> bool {
    extents.is_none_or(|extents| extents.may_hold_data(offset, len))
}

#[cfg(windows)]
fn query_allocated_extents(file: &mut File) -> Option<Vec<(u64, u64)>> {
    use std::os::windows::io::AsRawHandle;
    use std::ptr;
    use windows_sys::Win32::Foundation::ERROR_MORE_DATA;
    use windows_sys::Win32::System::IO::DeviceIoControl;
    use windows_sys::Win32::System::Ioctl::{
        FILE_ALLOCATED_RANGE_BUFFER, FSCTL_QUERY_ALLOCATED_RANGES,
    };

    const ENTRY_LEN: usize = std::mem::size_of::<FILE_ALLOCATED_RANGE_BUFFER>();

    let file_len = i64::try_from(file.metadata().ok()?.len()).ok()?;
    let mut ranges: Vec<(u64, u64)> = Vec::new();
    if file_len == 0 {
        return Some(ranges);
    }
    let mut output = vec![FILE_ALLOCATED_RANGE_BUFFER::default(); 512];
    let mut scan_from = 0i64;
    loop {
        let input = FILE_ALLOCATED_RANGE_BUFFER {
            FileOffset: scan_from,
            Length: file_len - scan_from,
        };
        let mut returned = 0u32;
        // SAFETY: the handle is borrowed from a live `File` opened for reading,
        // the input and output buffers are valid for the byte lengths passed,
        // and `returned` is a writable out pointer. The FSCTL only reads
        // allocation metadata.
        let ok = unsafe {
            DeviceIoControl(
                file.as_raw_handle() as _,
                FSCTL_QUERY_ALLOCATED_RANGES,
                ptr::from_ref(&input).cast(),
                ENTRY_LEN as u32,
                output.as_mut_ptr().cast(),
                (output.len() * ENTRY_LEN) as u32,
                &mut returned,
                ptr::null_mut(),
            )
        };
        let more = if ok == 0 {
            if std::io::Error::last_os_error().raw_os_error() != Some(ERROR_MORE_DATA as i32) {
                return None;
            }
            true
        } else {
            false
        };
        let count = returned as usize / ENTRY_LEN;
        for entry in &output[..count] {
            let start = u64::try_from(entry.FileOffset).ok()?;
            let end = start.checked_add(u64::try_from(entry.Length).ok()?)?;
            ranges.push((start, end));
        }
        if !more || count == 0 {
            break;
        }
        scan_from = output[count - 1]
            .FileOffset
            .checked_add(output[count - 1].Length)?;
        if scan_from >= file_len || ranges.len() > MAX_TRACKED_EXTENTS {
            break;
        }
    }
    if ranges.len() > MAX_TRACKED_EXTENTS {
        return None;
    }
    Some(ranges)
}

#[cfg(target_os = "linux")]
fn query_allocated_extents(file: &mut File) -> Option<Vec<(u64, u64)>> {
    use std::os::unix::io::AsRawFd;

    let fd = file.as_raw_fd();
    let file_len = file.metadata().ok()?.len();
    let mut ranges: Vec<(u64, u64)> = Vec::new();
    let mut cursor = 0i64;
    while (cursor as u64) < file_len {
        // SAFETY: `fd` is borrowed from a live `File`; `lseek` only moves the
        // descriptor offset, which the caller restores.
        let data = unsafe { libc::lseek(fd, cursor, libc::SEEK_DATA) };
        if data < 0 {
            // `ENXIO` means there is no data at or after `cursor`, which is the
            // documented end of the scan. Anything else means the filesystem
            // cannot answer, so nothing may be skipped.
            return match std::io::Error::last_os_error().raw_os_error() {
                Some(libc::ENXIO) => Some(ranges),
                _ => None,
            };
        }
        // SAFETY: same invariants as the `SEEK_DATA` call above.
        let hole = unsafe { libc::lseek(fd, data, libc::SEEK_HOLE) };
        if hole < 0 {
            return None;
        }
        ranges.push((data as u64, hole as u64));
        cursor = hole;
        if ranges.len() > MAX_TRACKED_EXTENTS {
            return None;
        }
    }
    Some(ranges)
}

#[cfg(not(any(windows, target_os = "linux")))]
fn query_allocated_extents(_file: &mut File) -> Option<Vec<(u64, u64)>> {
    None
}

/// Best-effort request that the filesystem represent unwritten regions of a
/// newly created matrix as holes.
///
/// On POSIX this is already the default. On Windows a file must carry the
/// sparse attribute *before* it is extended, otherwise `set_len` allocates the
/// clusters it reserves and `FSCTL_QUERY_ALLOCATED_RANGES` reports the whole
/// file as data. Failure is ignored: the matrix is then simply dense, which
/// costs performance and nothing else.
#[cfg(windows)]
fn mark_file_sparse(file: &File) {
    use std::os::windows::io::AsRawHandle;
    use std::ptr;
    use windows_sys::Win32::System::IO::DeviceIoControl;
    use windows_sys::Win32::System::Ioctl::FSCTL_SET_SPARSE;

    let mut returned = 0u32;
    // SAFETY: the handle is borrowed from a live writable `File`,
    // `FSCTL_SET_SPARSE` accepts null input/output buffers, and `returned` is a
    // valid out pointer.
    unsafe {
        DeviceIoControl(
            file.as_raw_handle() as _,
            FSCTL_SET_SPARSE,
            ptr::null(),
            0,
            ptr::null_mut(),
            0,
            &mut returned,
            ptr::null_mut(),
        );
    }
}

#[cfg(not(windows))]
fn mark_file_sparse(_file: &File) {}

/// Zeroes `[offset, offset + len)`, deallocating the range where the platform
/// supports it.
///
/// Returns `true` when the range was zeroed by a hole punch, which is `O(1)` in
/// `len`; `false` means the caller must fall back to writing zeros.
fn punch_zero_range(file: &mut File, offset: u64, len: u64) -> bool {
    if len == 0 {
        return true;
    }
    punch_zero_range_native(file, offset, len)
}

#[cfg(windows)]
fn punch_zero_range_native(file: &mut File, offset: u64, len: u64) -> bool {
    use std::os::windows::io::AsRawHandle;
    use std::ptr;
    use windows_sys::Win32::System::IO::DeviceIoControl;
    use windows_sys::Win32::System::Ioctl::{FILE_ZERO_DATA_INFORMATION, FSCTL_SET_ZERO_DATA};

    let (Some(file_offset), Some(end)) = (
        i64::try_from(offset).ok(),
        offset
            .checked_add(len)
            .and_then(|end| i64::try_from(end).ok()),
    ) else {
        return false;
    };
    let input = FILE_ZERO_DATA_INFORMATION {
        FileOffset: file_offset,
        BeyondFinalZero: end,
    };
    let mut returned = 0u32;
    // SAFETY: the handle is borrowed from a live writable `File`, the input
    // buffer is valid for the byte length passed, and `returned` is a writable
    // out pointer.
    let ok = unsafe {
        DeviceIoControl(
            file.as_raw_handle() as _,
            FSCTL_SET_ZERO_DATA,
            ptr::from_ref(&input).cast(),
            std::mem::size_of::<FILE_ZERO_DATA_INFORMATION>() as u32,
            ptr::null_mut(),
            0,
            &mut returned,
            ptr::null_mut(),
        )
    };
    ok != 0
}

#[cfg(target_os = "linux")]
fn punch_zero_range_native(file: &mut File, offset: u64, len: u64) -> bool {
    use std::os::unix::io::AsRawFd;

    let (Ok(offset), Ok(len)) = (libc::off_t::try_from(offset), libc::off_t::try_from(len)) else {
        return false;
    };
    // SAFETY: `fd` is borrowed from a live writable `File`; `fallocate` only
    // affects the requested byte range and never changes the file length with
    // `FALLOC_FL_KEEP_SIZE`.
    let result = unsafe {
        libc::fallocate(
            file.as_raw_fd(),
            libc::FALLOC_FL_PUNCH_HOLE | libc::FALLOC_FL_KEEP_SIZE,
            offset,
            len,
        )
    };
    result == 0
}

#[cfg(not(any(windows, target_os = "linux")))]
fn punch_zero_range_native(_file: &mut File, _offset: u64, _len: u64) -> bool {
    false
}

/// Zeroes `[offset, offset + len)` with a hole punch where possible, falling
/// back to writing zeros.
fn zero_range(file: &mut File, offset: u64, len: u64) -> Result<()> {
    if punch_zero_range(file, offset, len) {
        return Ok(());
    }
    count_category_clear_bytes_written(len);
    file.seek(SeekFrom::Start(offset))?;
    write_zeros(file, len)
}

#[cfg(test)]
std::thread_local! {
    static FAIL_NEXT_SLOT_WRITE: std::cell::Cell<bool> = const {
        std::cell::Cell::new(false)
    };
    static FAIL_NEXT_BITMAP_WRITE: std::cell::Cell<bool> = const {
        std::cell::Cell::new(false)
    };
}

#[cfg(any(test, feature = "scalable-fault-injection"))]
mod scaling_counters {
    use std::cell::Cell;

    std::thread_local! {
        pub(super) static BITMAP_BYTES_HASHED: Cell<u64> = const { Cell::new(0) };
        pub(super) static CREATE_METADATA_BYTES_WRITTEN: Cell<u64> = const { Cell::new(0) };
        pub(super) static OPEN_BITMAP_BYTES_RESIDENT: Cell<u64> = const { Cell::new(0) };
        pub(super) static OPEN_BITMAP_BYTES_READ: Cell<u64> = const { Cell::new(0) };
        pub(super) static CATEGORY_CLEAR_BYTES_WRITTEN: Cell<u64> = const { Cell::new(0) };
        pub(super) static OPEN_ALLOCATION_MAP_AVAILABLE: Cell<u64> = const { Cell::new(0) };
    }

    pub(super) fn add(cell: &'static std::thread::LocalKey<Cell<u64>>, value: u64) {
        cell.with(|counter| counter.set(counter.get().saturating_add(value)));
    }

    pub(super) fn set(cell: &'static std::thread::LocalKey<Cell<u64>>, value: u64) {
        cell.with(|counter| counter.set(value));
    }

    #[cfg(feature = "scalable-fault-injection")]
    pub(super) fn get(cell: &'static std::thread::LocalKey<Cell<u64>>) -> u64 {
        cell.with(Cell::get)
    }
}

#[cfg(any(test, feature = "scalable-fault-injection"))]
fn count_bitmap_bytes_hashed(bytes: u64) {
    scaling_counters::add(&scaling_counters::BITMAP_BYTES_HASHED, bytes);
}

#[cfg(not(any(test, feature = "scalable-fault-injection")))]
fn count_bitmap_bytes_hashed(_bytes: u64) {}

#[cfg(any(test, feature = "scalable-fault-injection"))]
fn count_create_metadata_bytes(bytes: u64) {
    scaling_counters::add(&scaling_counters::CREATE_METADATA_BYTES_WRITTEN, bytes);
}

#[cfg(not(any(test, feature = "scalable-fault-injection")))]
fn count_create_metadata_bytes(_bytes: u64) {}

#[cfg(any(test, feature = "scalable-fault-injection"))]
fn count_open_bitmap_bytes_read(bytes: u64) {
    scaling_counters::add(&scaling_counters::OPEN_BITMAP_BYTES_READ, bytes);
}

#[cfg(not(any(test, feature = "scalable-fault-injection")))]
fn count_open_bitmap_bytes_read(_bytes: u64) {}

#[cfg(any(test, feature = "scalable-fault-injection"))]
fn count_category_clear_bytes_written(bytes: u64) {
    scaling_counters::add(&scaling_counters::CATEGORY_CLEAR_BYTES_WRITTEN, bytes);
}

#[cfg(not(any(test, feature = "scalable-fault-injection")))]
fn count_category_clear_bytes_written(_bytes: u64) {}

#[cfg(any(test, feature = "scalable-fault-injection"))]
fn record_open_allocation_map(extents: Option<&AllocatedExtents>) {
    scaling_counters::set(
        &scaling_counters::OPEN_ALLOCATION_MAP_AVAILABLE,
        u64::from(extents.is_some()),
    );
}

#[cfg(not(any(test, feature = "scalable-fault-injection")))]
fn record_open_allocation_map(_extents: Option<&AllocatedExtents>) {}

#[cfg(any(test, feature = "scalable-fault-injection"))]
fn record_open_resident_bitmap_bytes(layout: &MatrixLayout) {
    let commits: u64 = layout
        .commits
        .iter()
        .map(|commit| {
            commit.bits.resident_bytes()
                + commit
                    .quarantined_raw_bits
                    .as_ref()
                    .map(SparseBitmap::resident_bytes)
                    .unwrap_or(0)
        })
        .sum();
    let blocks: u64 = layout
        .blocks
        .iter()
        .map(|block| {
            block.crc_valid_bits.resident_bytes() + block.current_write_bits.resident_bytes()
        })
        .sum();
    scaling_counters::set(
        &scaling_counters::OPEN_BITMAP_BYTES_RESIDENT,
        commits.saturating_add(blocks),
    );
}

#[cfg(not(any(test, feature = "scalable-fault-injection")))]
fn record_open_resident_bitmap_bytes(_layout: &MatrixLayout) {}

#[cfg(test)]
pub(crate) fn inject_partial_slot_write_failure() {
    FAIL_NEXT_SLOT_WRITE.set(true);
}

#[cfg(test)]
pub(crate) fn inject_bitmap_write_failure() {
    FAIL_NEXT_BITMAP_WRITE.set(true);
}

fn write_slot_payload(file: &mut File, payload: &[u8]) -> Result<()> {
    #[cfg(test)]
    if FAIL_NEXT_SLOT_WRITE.replace(false) {
        if let Some(first) = payload.first() {
            file.write_all(std::slice::from_ref(first))?;
        }
        return Err(Error::Io(std::io::Error::other(
            "injected partial matrix slot write failure",
        )));
    }

    file.write_all(payload)?;
    Ok(())
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MatrixDimensionValue {
    pub name: String,
    pub value: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MatrixDimensions {
    values: Vec<MatrixDimensionValue>,
}

impl MatrixDimensions {
    pub fn from_pairs<const N: usize>(pairs: [(&'static str, u64); N]) -> Self {
        let values = pairs
            .into_iter()
            .map(|(name, value)| MatrixDimensionValue {
                name: name.to_string(),
                value,
            })
            .collect();
        Self { values }
    }

    pub fn get(&self, name: &str) -> Option<u64> {
        self.values
            .iter()
            .find(|value| value.name == name)
            .map(|value| value.value)
    }

    pub fn values(&self) -> &[MatrixDimensionValue] {
        &self.values
    }
}

impl<const N: usize> From<[(&'static str, u64); N]> for MatrixDimensions {
    fn from(value: [(&'static str, u64); N]) -> Self {
        Self::from_pairs(value)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct MatrixKey {
    pub scan: u64,
    pub ch: u64,
}

impl MatrixKey {
    pub const fn new(scan: u64, ch: u64) -> Self {
        Self { scan, ch }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MatrixCellStatus {
    Committed,
    NotCommitted,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PackedBitmap {
    bit_len: u64,
    bytes: Vec<u8>,
}

impl PackedBitmap {
    pub fn new(bit_len: u64) -> Result<Self> {
        Ok(Self {
            bit_len,
            bytes: filled_bytes(bit_bytes(bit_len)?, 0)?,
        })
    }

    pub fn bit_len(&self) -> u64 {
        self.bit_len
    }

    pub fn get(&self, ordinal: u64) -> Result<bool> {
        if ordinal >= self.bit_len {
            return Err(Error::InvalidMatrixLayout);
        }
        get_bit(&self.bytes, ordinal)
    }

    pub fn set(&mut self, ordinal: u64, value: bool) -> Result<()> {
        if ordinal >= self.bit_len {
            return Err(Error::InvalidMatrixLayout);
        }
        set_bit(&mut self.bytes, ordinal, value)
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }
}

impl crate::VarveEncode for PackedBitmap {
    const WIRE_TYPE: crate::WireType = crate::WireType::Bytes;

    fn encode_varve(&self, encoder: &mut crate::Encoder) -> Result<()> {
        crate::VarveEncode::encode_varve(&self.bit_len, encoder)?;
        crate::VarveEncode::encode_varve(&self.bytes, encoder)
    }
}

impl crate::VarveDecode for PackedBitmap {
    const WIRE_TYPE: crate::WireType = crate::WireType::Bytes;

    fn decode_varve(decoder: &mut crate::Decoder<'_>) -> Result<Self> {
        let bit_len = <u64 as crate::VarveDecode>::decode_varve(decoder)?;
        let bytes = <Vec<u8> as crate::VarveDecode>::decode_varve(decoder)?;
        if bytes.len() as u64 != bit_bytes(bit_len)? {
            return Err(Error::InvalidMatrixLayout);
        }
        Ok(Self { bit_len, bytes })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MatrixCorruptionSeverity {
    Fatal,
    Recoverable,
    Advisory,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MatrixCorruptionKind {
    Header,
    Layout,
    CommitMap,
    Slot,
    Sidecar,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MatrixRecoveryFinding {
    pub kind: MatrixCorruptionKind,
    pub severity: MatrixCorruptionSeverity,
    pub message: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MatrixRecoveryAction {
    ClearCell { category: String, key: MatrixKey },
    ClearCategory { category: String },
    RebuildCommitMap { category: Option<String> },
    Resume,
    Restart,
    DiscardSidecar,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MatrixRecoveryReport {
    pub findings: Vec<MatrixRecoveryFinding>,
    pub recommended_actions: Vec<MatrixRecoveryAction>,
}

/// Thread-local observability counters for the paged matrix integrity
/// representation (PERF-01/PERF-02 scaling contracts).
///
/// These hang off `MatrixRecoveryReport` because it is the matrix diagnostic
/// type already exported from the crate root; they are not a property of any
/// particular report value.
#[cfg(feature = "scalable-fault-injection")]
impl MatrixRecoveryReport {
    /// Bitmap bytes fed to the checksum while maintaining commit-map integrity.
    pub fn matrix_bitmap_bytes_hashed() -> u64 {
        scaling_counters::get(&scaling_counters::BITMAP_BYTES_HASHED)
    }

    /// Metadata bytes explicitly written by matrix creation, excluding the
    /// sparse extent established with `set_len`.
    pub fn matrix_create_metadata_bytes_written() -> u64 {
        scaling_counters::get(&scaling_counters::CREATE_METADATA_BYTES_WRITTEN)
    }

    /// Resident bitmap bytes held by the most recently created or opened matrix
    /// layout on this thread.
    pub fn matrix_open_resident_bitmap_bytes() -> u64 {
        scaling_counters::get(&scaling_counters::OPEN_BITMAP_BYTES_RESIDENT)
    }

    /// Bitmap and page-digest bytes read from disk while opening matrix
    /// layouts on this thread.
    ///
    /// Pages the filesystem proves were never written are neither read nor
    /// counted, so this tracks the work the matrix has actually accumulated
    /// rather than its total cell count.
    pub fn matrix_open_bitmap_bytes_read() -> u64 {
        scaling_counters::get(&scaling_counters::OPEN_BITMAP_BYTES_READ)
    }

    /// Zero bytes explicitly written by whole-category clears on this thread.
    ///
    /// A clear that could deallocate its regions instead of overwriting them
    /// counts nothing.
    pub fn matrix_category_clear_bytes_written() -> u64 {
        scaling_counters::get(&scaling_counters::CATEGORY_CLEAR_BYTES_WRITTEN)
    }

    /// Whether the most recent matrix open obtained a filesystem allocation map.
    ///
    /// When this is false the platform or filesystem could not prove any range
    /// unwritten, so every page was read exactly as it was before the paged
    /// representation existed: correctness is unaffected, but open cost falls
    /// back to `O(cell_count / 8)`.
    pub fn matrix_open_allocation_map_available() -> bool {
        scaling_counters::get(&scaling_counters::OPEN_ALLOCATION_MAP_AVAILABLE) != 0
    }

    /// Resets every matrix integrity counter for the calling thread.
    pub fn reset_matrix_integrity_counters() {
        scaling_counters::set(&scaling_counters::BITMAP_BYTES_HASHED, 0);
        scaling_counters::set(&scaling_counters::CREATE_METADATA_BYTES_WRITTEN, 0);
        scaling_counters::set(&scaling_counters::OPEN_BITMAP_BYTES_RESIDENT, 0);
        scaling_counters::set(&scaling_counters::OPEN_BITMAP_BYTES_READ, 0);
        scaling_counters::set(&scaling_counters::CATEGORY_CLEAR_BYTES_WRITTEN, 0);
        scaling_counters::set(&scaling_counters::OPEN_ALLOCATION_MAP_AVAILABLE, 0);
    }
}

/// A bitmap whose pages are materialised only once they hold a set bit.
///
/// Pages that were never written are not resident and are not read back: the
/// on-disk page digest (or, where integrity is disabled, the streamed load)
/// establishes that they are zero, and a zero page answers every query without
/// occupying memory. Residency therefore tracks the work actually performed
/// against the matrix instead of its total cell count.
#[derive(Clone, Debug)]
struct SparseBitmap {
    bit_count: u64,
    byte_len: u64,
    page_count: u64,
    pages: HashMap<u64, Arc<Vec<u8>>>,
    ones: u64,
}

impl SparseBitmap {
    fn new(bit_count: u64) -> Result<Self> {
        let byte_len = bit_bytes(bit_count)?;
        Ok(Self {
            bit_count,
            byte_len,
            page_count: page_count_for(byte_len)?,
            pages: HashMap::new(),
            ones: 0,
        })
    }

    fn page_len(&self, page: u64) -> Result<u64> {
        let start = page
            .checked_mul(BITMAP_PAGE_BYTES)
            .ok_or(Error::InvalidMatrixLayout)?;
        if start >= self.byte_len {
            return Err(Error::InvalidMatrixLayout);
        }
        Ok((self.byte_len - start).min(BITMAP_PAGE_BYTES))
    }

    fn page_bytes(&self, page: u64) -> Result<&[u8]> {
        let len = usize::try_from(self.page_len(page)?).map_err(|_| Error::InvalidMatrixLayout)?;
        match self.pages.get(&page) {
            Some(bytes) if bytes.len() == len => Ok(bytes.as_slice()),
            Some(_) => Err(Error::InvalidMatrixLayout),
            None => Ok(&ZERO_PAGE[..len]),
        }
    }

    fn byte(&self, index: u64) -> Result<u8> {
        if index >= self.byte_len {
            return Err(Error::InvalidMatrixLayout);
        }
        let page = index / BITMAP_PAGE_BYTES;
        let within =
            usize::try_from(index % BITMAP_PAGE_BYTES).map_err(|_| Error::InvalidMatrixLayout)?;
        match self.pages.get(&page) {
            Some(bytes) => bytes.get(within).copied().ok_or(Error::InvalidMatrixLayout),
            None => Ok(0),
        }
    }

    fn get(&self, ordinal: u64) -> Result<bool> {
        if ordinal >= self.bit_count {
            return Err(Error::InvalidMatrixLayout);
        }
        Ok(self.byte(ordinal / 8)? & (1u8 << (ordinal % 8)) != 0)
    }

    /// Bytes that writing `value` at `index` would newly make resident, so a
    /// caller can charge the matrix bitmap budget before the memory is taken.
    /// A write that changes nothing materialises nothing and costs nothing.
    fn materialisation_cost(&self, index: u64, value: u8) -> Result<u64> {
        if index >= self.byte_len {
            return Err(Error::InvalidMatrixLayout);
        }
        if self.byte(index)? == value {
            return Ok(0);
        }
        let page = index / BITMAP_PAGE_BYTES;
        if self.pages.contains_key(&page) {
            Ok(0)
        } else {
            self.page_len(page)
        }
    }

    /// Returns the number of bytes newly made resident, which is one page the
    /// first time a page is touched and zero afterwards. Callers charge the
    /// return value to the matrix bitmap budget.
    fn set_byte(&mut self, index: u64, value: u8) -> Result<u64> {
        let current = self.byte(index)?;
        if current == value {
            return Ok(0);
        }
        let page = index / BITMAP_PAGE_BYTES;
        let within =
            usize::try_from(index % BITMAP_PAGE_BYTES).map_err(|_| Error::InvalidMatrixLayout)?;
        let mut materialised = 0;
        if !self.pages.contains_key(&page) {
            let page_len = self.page_len(page)?;
            let bytes = filled_bytes_for(page_len, 0, ReadLimitKey::MatrixBitmapBytes.resource())?;
            try_reserve_map(
                &mut self.pages,
                1,
                ReadLimitKey::MatrixBitmapBytes.resource(),
            )?;
            self.pages.insert(page, Arc::new(bytes));
            materialised = page_len;
        }
        let bytes = self
            .pages
            .get_mut(&page)
            .ok_or(Error::InvalidMatrixLayout)?;
        *Arc::make_mut(bytes)
            .get_mut(within)
            .ok_or(Error::InvalidMatrixLayout)? = value;
        self.ones = self
            .ones
            .checked_add(u64::from(value.count_ones()))
            .and_then(|ones| ones.checked_sub(u64::from(current.count_ones())))
            .ok_or(Error::InvalidMatrixLayout)?;
        Ok(materialised)
    }

    fn set(&mut self, ordinal: u64, value: bool) -> Result<u64> {
        if ordinal >= self.bit_count {
            return Err(Error::InvalidMatrixLayout);
        }
        let index = ordinal / 8;
        let mask = 1u8 << (ordinal % 8);
        let current = self.byte(index)?;
        let next = if value {
            current | mask
        } else {
            current & !mask
        };
        self.set_byte(index, next)
    }

    // Loading never materialises an all-zero page, so residency after open is
    // proportional to the pages that carry state rather than to the cell count.
    fn insert_loaded_page(&mut self, page: u64, bytes: Vec<u8>) -> Result<u64> {
        let page_len = self.page_len(page)?;
        if usize_to_u64(bytes.len())? != page_len {
            return Err(Error::InvalidMatrixLayout);
        }
        let ones = bytes
            .iter()
            .try_fold(0u64, |acc, byte| {
                acc.checked_add(u64::from(byte.count_ones()))
            })
            .ok_or(Error::InvalidMatrixLayout)?;
        if ones == 0 {
            return Ok(0);
        }
        try_reserve_map(
            &mut self.pages,
            1,
            ReadLimitKey::MatrixBitmapBytes.resource(),
        )?;
        self.pages.insert(page, Arc::new(bytes));
        self.ones = self
            .ones
            .checked_add(ones)
            .ok_or(Error::InvalidMatrixLayout)?;
        Ok(page_len)
    }

    fn clear(&mut self) {
        self.pages.clear();
        self.ones = 0;
    }

    fn ones(&self) -> u64 {
        self.ones
    }

    fn resident_bytes(&self) -> u64 {
        self.pages
            .values()
            .map(|page| page.len() as u64)
            .fold(0u64, u64::saturating_add)
    }
}

/// Running total of resident matrix bitmap bytes, checked against
/// `ReadLimitKey::MatrixBitmapBytes` every time the total grows.
///
/// PERF-02: the budget used to be charged once, up front, as
/// `2 * block_bitmap_len + commit_map_len` — the dense worst case of a
/// representation that no longer exists. That refused large matrices on a
/// cell-count-scaled figure even though a freshly created matrix holds no
/// resident bitmap bytes at all. The charge now tracks the pages actually
/// materialised, so the limit means what it says and still fails closed: every
/// growth is checked before the memory is used.
#[derive(Clone, Copy, Debug)]
struct ResidentBitmapBudget {
    limits: ReadLimits,
    used: u64,
}

impl ResidentBitmapBudget {
    fn new(limits: ReadLimits) -> Self {
        Self { limits, used: 0 }
    }

    fn charge(&mut self, bytes: u64) -> Result<()> {
        if bytes == 0 {
            return Ok(());
        }
        self.used = self
            .used
            .checked_add(bytes)
            .ok_or(Error::ResourceArithmeticOverflow {
                resource: ReadLimitKey::MatrixBitmapBytes.resource(),
            })?;
        self.limits
            .check(ReadLimitKey::MatrixBitmapBytes, self.used)
    }
}

fn page_count_for(byte_len: u64) -> Result<u64> {
    byte_len
        .checked_add(BITMAP_PAGE_BYTES - 1)
        .map(|value| value / BITMAP_PAGE_BYTES)
        .ok_or(Error::InvalidMatrixLayout)
}

fn page_digest_len(byte_len: u64) -> Result<u64> {
    page_count_for(byte_len)?
        .checked_mul(PAGE_DIGEST_LEN)
        .ok_or(Error::InvalidMatrixLayout)
}

fn page_digest_offset(base: u64, page: u64) -> Result<u64> {
    page.checked_mul(PAGE_DIGEST_LEN)
        .and_then(|delta| base.checked_add(delta))
        .ok_or(Error::InvalidMatrixLayout)
}

fn read_page_digest(file: &mut File, offset: u64) -> Result<(u32, u32)> {
    file.seek(SeekFrom::Start(offset))?;
    let mut bytes = [0; PAGE_DIGEST_LEN as usize];
    file.read_exact(&mut bytes)?;
    Ok((
        u32::from_le_bytes(bytes[0..4].try_into().expect("slice")),
        u32::from_le_bytes(bytes[4..8].try_into().expect("slice")),
    ))
}

/// Writes one page digest.
///
/// Trust boundary, recorded deliberately rather than left implicit. The digest
/// array and the checksum-validity bitmap are *not* themselves covered by the
/// MCRC metadata checksum, which spans only the dimension, block, and category
/// tables. A CRC cannot authenticate metadata against anyone who can write the
/// file, and the only construction that would detect a fabricated digest — a
/// composition over the whole digest array — must be recomputed on every
/// mutation and on every open, which reintroduces exactly the cell-count-scaled
/// costs PERF-01 and PERF-02 exist to remove.
///
/// The consequence is bounded and unchanged from the single unprotected commit
/// checksum this array replaced: an attacker who writes a commit bit together
/// with a matching digest can make `cell_status` report `Committed` for a cell
/// that was never written. Reading that cell still fails typed with
/// `MatrixChecksumMismatch`, because `verify_cell_crc` consults the persistent
/// validity bit and the recorded per-cell checksum, so no fabricated payload is
/// ever returned. Detecting metadata forgery requires a keyed digest, which is
/// a format decision outside this representation.
fn write_page_digest(file: &mut File, offset: u64, crc: u32, state: u32) -> Result<()> {
    let mut bytes = [0; PAGE_DIGEST_LEN as usize];
    bytes[0..4].copy_from_slice(&crc.to_le_bytes());
    bytes[4..8].copy_from_slice(&state.to_le_bytes());
    file.seek(SeekFrom::Start(offset))?;
    file.write_all(&bytes)?;
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MatrixResumeSignal {
    Clean,
    Partial { committed: u64, total: u64 },
    ResumeAvailable { committed: u64, total: u64 },
    RestartRecommended,
    DiscardRecommended,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MatrixCommitEvent {
    pub block_id: u32,
    pub category: &'static str,
    pub key: MatrixKey,
    pub slot_offset: u64,
    pub slot_len: u64,
}

#[derive(Clone, Debug)]
pub struct MatrixLayout {
    dimensions: Vec<MatrixDimensionValue>,
    commits: Vec<MatrixCommitLayout>,
    blocks: Vec<MatrixBlockLayout>,
    aux: Vec<MatrixAuxLayout>,
    crc_findings: Vec<MatrixRecoveryFinding>,
    append_log_start: u64,
    read_limits: ReadLimits,
    resident_bitmap_bytes: u64,
    // Precomputed at open time so accessors gate on a single flag instead of
    // scanning findings per cell access.
    fatal_access_blocked: bool,
}

#[derive(Clone, Debug)]
struct MatrixCommitLayout {
    name: String,
    kind: MatrixCommitKind,
    bit_count: u64,
    map_offset: u64,
    bits: SparseBitmap,
    quarantined_raw_bits: Option<SparseBitmap>,
    quarantine_finding: Option<MatrixRecoveryFinding>,
    digest_offset: Option<u64>,
}

#[derive(Clone, Debug)]
struct MatrixBlockLayout {
    block_id: u32,
    dimensions: [String; 2],
    slot_stride: u64,
    slot_region_offset: u64,
    cell_count: u64,
    crc_offset: Option<u64>,
    crc_valid_offset: Option<u64>,
    crc_valid_bits: SparseBitmap,
    current_write_bits: SparseBitmap,
}

#[derive(Clone, Debug)]
struct MatrixAuxLayout {
    name: String,
    offset: u64,
    byte_len: u64,
}

#[derive(Clone, Debug)]
struct MatrixCrcLayout {
    region_offset: u64,
    commit_digest_offsets: Vec<u64>,
    block_crc_offsets: Vec<u64>,
    block_valid_offsets: Vec<u64>,
}

#[derive(Default)]
struct MatrixCrcVerification {
    findings: Vec<MatrixRecoveryFinding>,
    commit_findings: HashMap<String, MatrixRecoveryFinding>,
}

impl MatrixLayout {
    pub fn append_log_start(&self) -> u64 {
        self.append_log_start
    }

    /// Charges newly resident bitmap bytes against
    /// `ReadLimitKey::MatrixBitmapBytes` before the memory is taken.
    fn charge_resident_bitmap(&mut self, bytes: u64) -> Result<()> {
        if bytes == 0 {
            return Ok(());
        }
        let next = self.resident_bitmap_bytes.checked_add(bytes).ok_or(
            Error::ResourceArithmeticOverflow {
                resource: ReadLimitKey::MatrixBitmapBytes.resource(),
            },
        )?;
        self.read_limits
            .check(ReadLimitKey::MatrixBitmapBytes, next)?;
        self.resident_bitmap_bytes = next;
        Ok(())
    }

    // Fail-closed gate for `Fatal` recovery findings: safe accessors must not
    // consume fatal-state data unless the spec opted into forensic access.
    fn ensure_fatal_access_allowed(&self) -> Result<()> {
        if self.fatal_access_blocked {
            return Err(Error::MatrixFatalCorruption);
        }
        Ok(())
    }

    pub fn dimension(&self, name: &str) -> Option<u64> {
        self.dimensions
            .iter()
            .find(|value| value.name == name)
            .map(|value| value.value)
    }

    fn block_index(&self, block_id: u32) -> Result<usize> {
        self.blocks
            .iter()
            .position(|block| block.block_id == block_id)
            .ok_or(Error::MatrixBlockMissing(block_id))
    }

    fn commit_index(&self, name: &str) -> Result<usize> {
        self.commits
            .iter()
            .position(|commit| commit.name == name)
            .ok_or_else(|| Error::MatrixCommitMissing(name.to_string()))
    }

    fn ordinal_for_block(&self, block_index: usize, key: MatrixKey) -> Result<u64> {
        let block = &self.blocks[block_index];
        let dim0 = self
            .dimension(&block.dimensions[0])
            .ok_or_else(|| Error::MatrixDimensionMissing(block.dimensions[0].clone()))?;
        let dim1 = self
            .dimension(&block.dimensions[1])
            .ok_or_else(|| Error::MatrixDimensionMissing(block.dimensions[1].clone()))?;
        if key.scan >= dim0 || key.ch >= dim1 {
            return Err(Error::MatrixKeyOutOfBounds {
                scan: key.scan,
                ch: key.ch,
            });
        }
        key.scan
            .checked_mul(dim1)
            .and_then(|base| base.checked_add(key.ch))
            .ok_or(Error::InvalidMatrixLayout)
    }

    fn slot_offset(&self, block_index: usize, ordinal: u64) -> Result<u64> {
        let block = &self.blocks[block_index];
        ordinal
            .checked_mul(block.slot_stride)
            .and_then(|delta| block.slot_region_offset.checked_add(delta))
            .ok_or(Error::InvalidMatrixLayout)
    }

    fn aux(&self, name: &str) -> Result<&MatrixAuxLayout> {
        self.aux
            .iter()
            .find(|aux| aux.name == name)
            .ok_or_else(|| Error::MatrixAuxMissing(name.to_string()))
    }
}

pub(crate) fn create_layout(
    spec: FormatSpec,
    file: &mut File,
    header_len: u64,
    dims: &MatrixDimensions,
) -> Result<MatrixLayout> {
    let crc_enabled = matrix_crc_enabled(spec)?;
    let (dimension_table_len, block_table_len, category_table_len) =
        matrix_descriptor_table_lengths(spec)?;
    check_matrix_metadata_limit(
        spec.read_limits,
        dimension_table_len,
        block_table_len,
        category_table_len,
    )?;
    let dimensions = dimension_values(spec, dims)?;
    check_matrix_dimensions(spec.read_limits, &dimensions)?;
    let cell_counts = matrix_cell_counts(spec, &dimensions)?;
    let commit_plans = matrix_commit_plans(spec, &dimensions, &cell_counts)?;
    let commit_map_len = matrix_commit_map_len(&commit_plans)?;
    let slot_region_len = matrix_slot_region_len(spec, &cell_counts)?;
    spec.read_limits
        .check(ReadLimitKey::MatrixSlotRegionLen, slot_region_len)?;
    let block_bitmap_len = matrix_block_bitmap_len(spec, &cell_counts)?;
    // PERF-02: creation makes no bitmap page resident, so the resident budget
    // starts empty and is charged as pages are actually materialised. The
    // cell-count-scaled admission gates that remain (cells, slot region length,
    // metadata bytes, checksum bytes, file length) all describe on-disk
    // extents, which creation really does reserve.
    let resident_bitmap_bytes = 0;
    let region_crc_len = if crc_enabled {
        crc_table_len(spec, &commit_plans, &cell_counts)?
    } else {
        0
    };
    let accounted_crc_bytes =
        matrix_accounted_crc_len(crc_enabled, region_crc_len, block_bitmap_len)?;
    spec.read_limits
        .check(ReadLimitKey::MatrixCrcBytes, accounted_crc_bytes)?;

    let dimension_table_off = header_len
        .checked_add(u64::from(VMAT_HEADER_LEN))
        .ok_or(Error::InvalidMatrixLayout)?;
    let block_table_off = dimension_table_off
        .checked_add(dimension_table_len)
        .ok_or(Error::InvalidMatrixLayout)?;
    let commit_category_off = block_table_off
        .checked_add(block_table_len)
        .ok_or(Error::InvalidMatrixLayout)?;
    let commit_map_off = commit_category_off
        .checked_add(category_table_len)
        .ok_or(Error::InvalidMatrixLayout)?;
    let slot_region_off = commit_map_off
        .checked_add(commit_map_len)
        .ok_or(Error::InvalidMatrixLayout)?;
    let slot_region_end = slot_region_off
        .checked_add(slot_region_len)
        .ok_or(Error::InvalidMatrixLayout)?;
    let aux_offsets = matrix_aux_offsets(spec, slot_region_end)?;
    let aux_region_len = matrix_aux_region_len(spec)?;
    let aux_region_end = slot_region_end
        .checked_add(aux_region_len)
        .ok_or(Error::InvalidMatrixLayout)?;
    let region_crc_off = if crc_enabled { aux_region_end } else { 0 };
    let append_log_start = if crc_enabled {
        region_crc_off
            .checked_add(region_crc_len)
            .ok_or(Error::InvalidMatrixLayout)?
    } else {
        aux_region_end
    };
    spec.read_limits
        .check(ReadLimitKey::FileLen, append_log_start)?;

    let dimension_table = encode_dimension_table(&dimensions, dimension_table_len)?;
    let mut dimension_index = HashMap::new();
    try_reserve_map(
        &mut dimension_index,
        dimensions.len(),
        MATRIX_DESCRIPTOR_RESOURCE,
    )?;
    for (index, value) in dimensions.iter().enumerate() {
        dimension_index.insert(
            value.name.clone(),
            u16::try_from(index).map_err(|_| Error::InvalidMatrixLayout)?,
        );
    }

    let mut commit_offsets = HashMap::new();
    try_reserve_map(
        &mut commit_offsets,
        commit_plans.len(),
        MATRIX_DESCRIPTOR_RESOURCE,
    )?;
    let mut next_commit_offset = commit_map_off;
    for (name, _, bit_count) in &commit_plans {
        let len = bit_bytes(*bit_count)?;
        commit_offsets.insert(name.clone(), (next_commit_offset, len));
        next_commit_offset = next_commit_offset
            .checked_add(len)
            .ok_or(Error::InvalidMatrixLayout)?;
    }

    let mut block_offsets = HashMap::new();
    try_reserve_map(
        &mut block_offsets,
        spec.matrix_blocks.len(),
        MATRIX_DESCRIPTOR_RESOURCE,
    )?;
    let mut next_slot_offset = slot_region_off;
    for block in spec.matrix_blocks {
        let cell_count = *cell_counts
            .get(block.category)
            .ok_or_else(|| Error::MatrixCommitMissing(block.category.to_string()))?;
        let len = cell_count
            .checked_mul(block.slot_stride)
            .ok_or(Error::InvalidMatrixLayout)?;
        block_offsets.insert(block.block_id, (next_slot_offset, len, cell_count));
        next_slot_offset = next_slot_offset
            .checked_add(len)
            .ok_or(Error::InvalidMatrixLayout)?;
    }

    let block_table = encode_block_table(spec, &dimension_index, &commit_plans, &block_offsets)?;
    let category_table = encode_category_table(&commit_plans, &commit_offsets)?;
    if usize_to_u64(block_table.len())? != block_table_len
        || usize_to_u64(category_table.len())? != category_table_len
    {
        return Err(Error::InvalidMatrixLayout);
    }
    let crc_layout = crc_layout_from_parts(
        region_crc_off,
        region_crc_len,
        spec,
        &commit_plans,
        &block_offsets,
    )?;
    let header = encode_header(MatrixHeaderFields {
        dimension_count: u32::try_from(dimensions.len()).map_err(|_| Error::InvalidMatrixLayout)?,
        matrix_block_count: u32::try_from(spec.matrix_blocks.len())
            .map_err(|_| Error::InvalidMatrixLayout)?,
        commit_category_count: u32::try_from(spec.matrix_commits.len())
            .map_err(|_| Error::InvalidMatrixLayout)?,
        dimension_table_off,
        dimension_table_len,
        block_table_off,
        block_table_len,
        commit_category_off,
        commit_category_len: category_table_len,
        commit_map_off,
        commit_map_len,
        slot_region_off,
        slot_region_len,
        region_crc_off,
        region_crc_len,
        append_log_start,
    });
    let has_crc = crc_layout.is_some();
    let initial_crc_valid_bits = zero_crc_valid_bitmaps(spec, has_crc, &block_offsets)?;
    let layout = layout_from_parts(
        dimensions,
        spec,
        &commit_plans,
        &commit_offsets,
        &block_offsets,
        &aux_offsets,
        zero_commit_bitmaps(&commit_plans)?,
        crc_layout,
        initial_crc_valid_bits,
        HashMap::new(),
        Vec::new(),
        append_log_start,
        resident_bitmap_bytes,
    )?;

    // PERF-02: only the fixed-size descriptor tables and the checksum-region
    // header are written explicitly. The commit maps, per-cell checksums, and
    // validity bitmaps are established as a zero extent by `set_len`, so
    // creation cost does not scale with the cell count. A never-written page is
    // recognised by its `PAGE_STATE_UNINITIALIZED` digest, which is exactly the
    // zero extent, and is therefore distinguishable from a page that was
    // written and happens to hold zeros.
    // Requested before the reserved regions are established so that `set_len`
    // leaves them as holes rather than allocated clusters; see
    // `AllocatedExtents` for why open depends on that distinction.
    mark_file_sparse(file);
    file.seek(SeekFrom::Start(header_len))?;
    file.write_all(&header)?;
    file.write_all(&dimension_table)?;
    file.write_all(&block_table)?;
    file.write_all(&category_table)?;
    count_create_metadata_bytes(
        usize_to_u64(header.len())?
            .saturating_add(usize_to_u64(dimension_table.len())?)
            .saturating_add(usize_to_u64(block_table.len())?)
            .saturating_add(usize_to_u64(category_table.len())?),
    );
    file.set_len(append_log_start)?;
    if has_crc {
        file.seek(SeekFrom::Start(region_crc_off))?;
        write_crc_header(file, &dimension_table, &block_table, &category_table)?;
        count_create_metadata_bytes(MCRC_HEADER_LEN);
    }
    file.seek(SeekFrom::Start(append_log_start))?;
    Ok(layout)
}

pub(crate) fn read_layout_at_len(
    spec: FormatSpec,
    file: &mut File,
    header_len: u64,
    file_len: u64,
) -> Result<MatrixLayout> {
    spec.read_limits.check(ReadLimitKey::FileLen, file_len)?;
    validate_range(header_len, u64::from(VMAT_HEADER_LEN), file_len)?;
    let crc_enabled = matrix_crc_enabled(spec)?;
    let header = read_header(file, header_len)?;
    validate_header_descriptor_shape(spec, &header)?;
    check_matrix_metadata_limit(
        spec.read_limits,
        header.dimension_table_len,
        header.block_table_len,
        header.commit_category_len,
    )?;
    let aux_region_len = matrix_aux_region_len(spec)?;
    validate_layout_ranges(header_len, file_len, &header, aux_region_len)?;
    validate_crc_presence(crc_enabled, &header)?;

    let dimension_table = read_range(
        file,
        header.dimension_table_off,
        header.dimension_table_len,
        MATRIX_DESCRIPTOR_RESOURCE,
    )?;
    let dimensions = decode_dimension_table(&dimension_table, header.dimension_count)?;
    validate_dimension_names(spec, &dimensions)?;
    check_matrix_dimensions(spec.read_limits, &dimensions)?;
    let cell_counts = matrix_cell_counts(spec, &dimensions)?;
    let expected_commit_plans = matrix_commit_plans(spec, &dimensions, &cell_counts)?;
    validate_dimension_derived_lengths(
        spec,
        crc_enabled,
        &header,
        &expected_commit_plans,
        &cell_counts,
    )?;
    spec.read_limits
        .check(ReadLimitKey::MatrixSlotRegionLen, header.slot_region_len)?;
    let block_bitmap_len = matrix_block_bitmap_len(spec, &cell_counts)?;
    let accounted_crc_bytes =
        matrix_accounted_crc_len(crc_enabled, header.region_crc_len, block_bitmap_len)?;
    spec.read_limits
        .check(ReadLimitKey::MatrixCrcBytes, accounted_crc_bytes)?;

    let category_table = read_range(
        file,
        header.commit_category_off,
        header.commit_category_len,
        MATRIX_DESCRIPTOR_RESOURCE,
    )?;
    let commit_plans = decode_category_table(&category_table, header.commit_category_count)?;
    let commit_offsets = validate_commit_table(
        &expected_commit_plans,
        &commit_plans,
        header.commit_map_off,
        header.commit_map_len,
    )?;

    let block_table = read_range(
        file,
        header.block_table_off,
        header.block_table_len,
        MATRIX_DESCRIPTOR_RESOURCE,
    )?;
    let block_offsets = decode_block_table(
        spec,
        &dimensions,
        &commit_plans,
        &cell_counts,
        &block_table,
        header.matrix_block_count,
        (header.slot_region_off, header.slot_region_len),
    )?;
    let slot_region_end = header
        .slot_region_off
        .checked_add(header.slot_region_len)
        .ok_or(Error::InvalidMatrixLayout)?;
    let aux_offsets = matrix_aux_offsets(spec, slot_region_end)?;
    let crc_layout = crc_layout_from_parts(
        header.region_crc_off,
        header.region_crc_len,
        spec,
        &expected_commit_plans,
        &block_offsets,
    )?;
    let mut crc_verification = verify_crc_header(
        file,
        crc_layout.as_ref(),
        &[&dimension_table, &block_table, &category_table],
        commit_plans.len(),
    )?;
    // PERF-02: one allocation-map query replaces streaming every page of every
    // commit map and validity bitmap. Ranges the filesystem reports as holes
    // were never written and therefore read as zero.
    let extents = AllocatedExtents::query(file);
    record_open_allocation_map(extents.as_ref());
    let mut budget = ResidentBitmapBudget::new(spec.read_limits);
    let commit_bits = load_commit_bitmaps(
        file,
        crc_layout.as_ref(),
        &commit_plans,
        extents.as_ref(),
        &mut budget,
        &mut crc_verification,
    )?;
    let crc_valid_bits = load_crc_valid_bits(
        spec,
        file,
        crc_layout.as_ref(),
        &block_offsets,
        extents.as_ref(),
        &mut budget,
    )?;

    layout_from_parts(
        dimensions,
        spec,
        &expected_commit_plans,
        &commit_offsets,
        &block_offsets,
        &aux_offsets,
        commit_bits,
        crc_layout,
        crc_valid_bits,
        crc_verification.commit_findings,
        crc_verification.findings,
        header.append_log_start,
        budget.used,
    )
}

pub(crate) fn write_cell<T: VarveMatrixBlock>(
    spec: FormatSpec,
    layout: &mut MatrixLayout,
    file: &mut File,
    key: MatrixKey,
    value: &T,
) -> Result<()> {
    ensure_matrix_block::<T>(spec)?;
    ensure_commit_publishable(layout, T::CATEGORY)?;
    let block_index = layout.block_index(T::ID)?;
    let ordinal = layout.ordinal_for_block(block_index, key)?;
    let offset = layout.slot_offset(block_index, ordinal)?;
    let slot_stride = layout.blocks[block_index].slot_stride;
    // DEF-02: the slot stride is the hard bound for the encode itself, so a
    // hostile or miswritten `VarveMatrixBlock` encode can never buffer more
    // than one byte past the stride before the typed error fires. The cap is
    // `slot_stride + 1` (not `slot_stride`) so an encode of exactly one extra
    // byte still completes and the exact-size check below reports its true
    // length; anything larger stops buffering at the first over-limit write
    // and the overflow is mapped back to the existing `MatrixSizeMismatch`
    // contract, with `actual` being the encoded length observed at cutoff.
    let payload = match encode_to_vec_limited(
        value,
        T::ENDIAN.unwrap_or(spec.endian),
        slot_stride.saturating_add(1),
        MATRIX_SLOT_PAYLOAD_RESOURCE,
    ) {
        Ok(payload) => payload,
        Err(Error::LimitExceeded {
            resource: MATRIX_SLOT_PAYLOAD_RESOURCE,
            actual,
            ..
        }) => {
            return Err(Error::MatrixSizeMismatch {
                expected: slot_stride,
                actual,
            });
        }
        Err(err) => return Err(err),
    };
    if payload.len() as u64 != slot_stride {
        return Err(Error::MatrixSizeMismatch {
            expected: slot_stride,
            actual: payload.len() as u64,
        });
    }
    let (commit_index, commit_update) = prepare_cell_commit(layout, T::CATEGORY, ordinal, false)?;
    let crc_valid_update = prepare_cell_crc_valid(layout, block_index, ordinal, false)?;

    apply_commit_bit(layout, file, commit_index, commit_update)?;
    if let Some(update) = crc_valid_update {
        apply_cell_crc_valid(layout, file, block_index, update)?;
    }
    file.seek(SeekFrom::Start(offset))?;
    write_slot_payload(file, &payload)?;
    charge_current_write_bit(layout, block_index, ordinal)?;
    Ok(())
}

/// Records that this session wrote `ordinal`, charging the page it makes
/// resident before the memory is taken.
fn charge_current_write_bit(
    layout: &mut MatrixLayout,
    block_index: usize,
    ordinal: u64,
) -> Result<()> {
    let index = ordinal / 8;
    let byte = layout.blocks[block_index].current_write_bits.byte(index)?;
    let cost = layout.blocks[block_index]
        .current_write_bits
        .materialisation_cost(index, byte | (1u8 << (ordinal % 8)))?;
    layout.charge_resident_bitmap(cost)?;
    layout.blocks[block_index]
        .current_write_bits
        .set(ordinal, true)?;
    Ok(())
}

pub(crate) fn write_cell_payload<T: VarveMatrixBlock>(
    spec: FormatSpec,
    layout: &mut MatrixLayout,
    file: &mut File,
    key: MatrixKey,
    payload: &[u8],
) -> Result<()> {
    ensure_matrix_block::<T>(spec)?;
    ensure_commit_publishable(layout, T::CATEGORY)?;
    let block_index = layout.block_index(T::ID)?;
    let ordinal = layout.ordinal_for_block(block_index, key)?;
    let offset = layout.slot_offset(block_index, ordinal)?;
    let slot_stride = layout.blocks[block_index].slot_stride;
    if payload.len() as u64 != slot_stride {
        return Err(Error::MatrixSizeMismatch {
            expected: slot_stride,
            actual: payload.len() as u64,
        });
    }
    let (commit_index, commit_update) = prepare_cell_commit(layout, T::CATEGORY, ordinal, false)?;
    let crc_valid_update = prepare_cell_crc_valid(layout, block_index, ordinal, false)?;

    apply_commit_bit(layout, file, commit_index, commit_update)?;
    if let Some(update) = crc_valid_update {
        apply_cell_crc_valid(layout, file, block_index, update)?;
    }
    file.seek(SeekFrom::Start(offset))?;
    write_slot_payload(file, payload)?;
    charge_current_write_bit(layout, block_index, ordinal)?;
    Ok(())
}

pub(crate) fn read_cell<T: VarveMatrixBlock>(
    spec: FormatSpec,
    layout: &MatrixLayout,
    file: &mut File,
    key: MatrixKey,
) -> Result<T> {
    ensure_matrix_block::<T>(spec)?;
    if cell_status::<T>(spec, layout, key)? == MatrixCellStatus::NotCommitted {
        return Err(Error::MatrixNotCommitted);
    }
    let block_index = layout.block_index(T::ID)?;
    let ordinal = layout.ordinal_for_block(block_index, key)?;
    let offset = layout.slot_offset(block_index, ordinal)?;
    let stride = layout.blocks[block_index].slot_stride;
    layout
        .read_limits
        .check(ReadLimitKey::RecordPayloadLen, stride)?;
    layout
        .read_limits
        .check(ReadLimitKey::MaterializedBytes, stride)?;
    file.seek(SeekFrom::Start(offset))?;
    let mut payload = filled_bytes(stride, 0)?;
    file.read_exact(&mut payload)?;
    verify_cell_crc(layout, file, block_index, ordinal, &payload)?;
    let materialized_limit = layout
        .read_limits
        .require(ReadLimitKey::MaterializedBytes)?
        .unwrap_or(u64::MAX);
    let decode_limit =
        materialized_limit
            .checked_sub(stride)
            .ok_or(Error::ResourceArithmeticOverflow {
                resource: "materialized bytes",
            })?;
    Decoder::decode_from_slice_limited(&payload, T::ENDIAN.unwrap_or(spec.endian), decode_limit)
}

pub(crate) fn cell_status<T: VarveMatrixBlock>(
    spec: FormatSpec,
    layout: &MatrixLayout,
    key: MatrixKey,
) -> Result<MatrixCellStatus> {
    ensure_matrix_block::<T>(spec)?;
    ensure_commit_publishable(layout, T::CATEGORY)?;
    let block_index = layout.block_index(T::ID)?;
    let ordinal = layout.ordinal_for_block(block_index, key)?;
    let commit_index = layout.commit_index(T::CATEGORY)?;
    if layout.commits[commit_index].bits.get(ordinal)? {
        Ok(MatrixCellStatus::Committed)
    } else {
        Ok(MatrixCellStatus::NotCommitted)
    }
}

pub(crate) fn commit_cell<T: VarveMatrixBlock>(
    spec: FormatSpec,
    layout: &mut MatrixLayout,
    file: &mut File,
    key: MatrixKey,
) -> Result<()> {
    ensure_matrix_block::<T>(spec)?;
    ensure_commit_publishable(layout, T::CATEGORY)?;
    let block_index = layout.block_index(T::ID)?;
    let ordinal = layout.ordinal_for_block(block_index, key)?;
    let written_this_session = layout.blocks[block_index].current_write_bits.get(ordinal)?;
    if !written_this_session && slot_is_all_zero(layout, file, block_index, ordinal)? {
        return Err(Error::MatrixCellNotWritten);
    }
    let crc_valid_update = prepare_cell_crc_valid(layout, block_index, ordinal, true)?;
    let (commit_index, commit_update) = prepare_cell_commit(layout, T::CATEGORY, ordinal, true)?;
    update_cell_crc(layout, file, block_index, ordinal)?;
    if let Some(update) = crc_valid_update {
        apply_cell_crc_valid(layout, file, block_index, update)?;
    }
    apply_commit_bit(layout, file, commit_index, commit_update)
}

pub(crate) fn clear_cell<T: VarveMatrixBlock>(
    spec: FormatSpec,
    layout: &mut MatrixLayout,
    file: &mut File,
    key: MatrixKey,
) -> Result<()> {
    ensure_matrix_block::<T>(spec)?;
    ensure_commit_publishable(layout, T::CATEGORY)?;
    let block_index = layout.block_index(T::ID)?;
    let ordinal = layout.ordinal_for_block(block_index, key)?;
    set_cell_commit(layout, file, T::CATEGORY, ordinal, false)?;
    set_cell_crc_valid(layout, file, block_index, ordinal, false)?;
    clear_cell_crc(layout, file, block_index, ordinal)
}

pub(crate) fn clear_cell_by_category(
    spec: FormatSpec,
    layout: &mut MatrixLayout,
    file: &mut File,
    category: &str,
    key: MatrixKey,
) -> Result<()> {
    ensure_commit_publishable(layout, category)?;
    let block_index = block_index_for_category(spec, layout, category)?;
    let ordinal = layout.ordinal_for_block(block_index, key)?;
    set_cell_commit(layout, file, category, ordinal, false)?;
    set_cell_crc_valid(layout, file, block_index, ordinal, false)?;
    clear_cell_crc(layout, file, block_index, ordinal)
}

pub(crate) fn clear_category(
    spec: FormatSpec,
    layout: &mut MatrixLayout,
    file: &mut File,
    category: &str,
) -> Result<u64> {
    layout.ensure_fatal_access_allowed()?;
    let commit_index = layout.commit_index(category)?;
    let cleared = count_committed(&layout.commits[commit_index])?;
    let commit_kind = layout.commits[commit_index].kind;
    let map_offset = layout.commits[commit_index].map_offset;
    let digest_offset = layout.commits[commit_index].digest_offset;
    let map_len = layout.commits[commit_index].bits.byte_len;
    let page_count = layout.commits[commit_index].bits.page_count;
    // Everything the category held becomes non-resident again.
    let released = layout.commits[commit_index]
        .bits
        .resident_bytes()
        .saturating_add(
            layout.commits[commit_index]
                .quarantined_raw_bits
                .as_ref()
                .map(SparseBitmap::resident_bytes)
                .unwrap_or(0),
        );
    let cleared_valid = if commit_kind == MatrixCommitKind::Cell {
        let block_index = block_index_for_category(spec, layout, category)?;
        layout.blocks[block_index]
            .crc_valid_offset
            .map(|valid_offset| (block_index, valid_offset))
    } else {
        None
    };

    // A whole-category clear restores exactly the post-create encoding: an
    // all-zero commit map whose every page digest reads `{0, UNINITIALIZED}`.
    // That is byte-identical to the zero extent `set_len` leaves behind, so the
    // regions are punched back into holes where the platform supports it,
    // making the clear independent of the cell count instead of writing
    // `Theta(cell_count / 8)` zero bytes. Where hole punching is unavailable
    // the fallback writes the same zeros as before.
    if let Some((block_index, valid_offset)) = cleared_valid {
        zero_range(
            file,
            valid_offset,
            layout.blocks[block_index].crc_valid_bits.byte_len,
        )?;
    }
    if let Some(digest_offset) = digest_offset {
        let digest_len = page_count
            .checked_mul(PAGE_DIGEST_LEN)
            .ok_or(Error::InvalidMatrixLayout)?;
        if !punch_zero_range(file, digest_offset, digest_len) {
            count_category_clear_bytes_written(digest_len);
            for page in 0..page_count {
                write_page_digest(
                    file,
                    page_digest_offset(digest_offset, page)?,
                    0,
                    PAGE_STATE_UNINITIALIZED,
                )?;
            }
        }
    }
    zero_range(file, map_offset, map_len)?;

    if let Some((block_index, _)) = cleared_valid {
        layout.blocks[block_index].crc_valid_bits.clear();
    }
    {
        let commit = &mut layout.commits[commit_index];
        commit.bits.clear();
        commit.quarantined_raw_bits = None;
        commit.quarantine_finding = None;
    }
    layout.resident_bitmap_bytes = layout
        .resident_bitmap_bytes
        .checked_sub(released)
        .ok_or(Error::InvalidMatrixLayout)?;
    Ok(cleared)
}

pub(crate) fn commit_event<T: VarveMatrixBlock>(
    spec: FormatSpec,
    layout: &MatrixLayout,
    key: MatrixKey,
) -> Result<MatrixCommitEvent> {
    ensure_matrix_block::<T>(spec)?;
    layout.ensure_fatal_access_allowed()?;
    let block_index = layout.block_index(T::ID)?;
    let ordinal = layout.ordinal_for_block(block_index, key)?;
    let offset = layout.slot_offset(block_index, ordinal)?;
    Ok(MatrixCommitEvent {
        block_id: T::ID,
        category: T::CATEGORY,
        key,
        slot_offset: offset,
        slot_len: layout.blocks[block_index].slot_stride,
    })
}

pub(crate) fn read_cell_payload<T: VarveMatrixBlock>(
    spec: FormatSpec,
    layout: &MatrixLayout,
    file: &mut File,
    key: MatrixKey,
) -> Result<Vec<u8>> {
    ensure_matrix_block::<T>(spec)?;
    if cell_status::<T>(spec, layout, key)? == MatrixCellStatus::NotCommitted {
        return Err(Error::MatrixNotCommitted);
    }
    let block_index = layout.block_index(T::ID)?;
    let ordinal = layout.ordinal_for_block(block_index, key)?;
    let offset = layout.slot_offset(block_index, ordinal)?;
    let stride = layout.blocks[block_index].slot_stride;
    layout
        .read_limits
        .check(ReadLimitKey::RecordPayloadLen, stride)?;
    layout
        .read_limits
        .check(ReadLimitKey::MaterializedBytes, stride)?;
    file.seek(SeekFrom::Start(offset))?;
    let mut payload = filled_bytes(stride, 0)?;
    file.read_exact(&mut payload)?;
    verify_cell_crc(layout, file, block_index, ordinal, &payload)?;
    Ok(payload)
}

pub(crate) fn aux_len(layout: &MatrixLayout, name: &str) -> Result<u64> {
    layout.ensure_fatal_access_allowed()?;
    Ok(layout.aux(name)?.byte_len)
}

pub(crate) fn read_aux_at_len(
    layout: &MatrixLayout,
    file: &mut File,
    logical_file_len: u64,
    name: &str,
    offset: u64,
    len: u64,
) -> Result<Vec<u8>> {
    layout.ensure_fatal_access_allowed()?;
    layout
        .read_limits
        .check(ReadLimitKey::FileLen, logical_file_len)?;
    layout
        .read_limits
        .check(ReadLimitKey::RecordPayloadLen, len)?;
    layout
        .read_limits
        .check(ReadLimitKey::MaterializedBytes, len)?;
    let absolute = aux_absolute_offset(layout.aux(name)?, offset, len)?;
    validate_range(absolute, len, logical_file_len)?;
    file.seek(SeekFrom::Start(absolute))?;
    let mut payload = filled_bytes(len, 0)?;
    file.read_exact(&mut payload)?;
    Ok(payload)
}

pub(crate) fn write_aux_at_len(
    layout: &MatrixLayout,
    file: &mut File,
    logical_file_len: u64,
    name: &str,
    offset: u64,
    payload: &[u8],
) -> Result<()> {
    let len = payload
        .len()
        .try_into()
        .map_err(|_| Error::InvalidMatrixLayout)?;
    layout.ensure_fatal_access_allowed()?;
    layout
        .read_limits
        .check(ReadLimitKey::FileLen, logical_file_len)?;
    layout
        .read_limits
        .check(ReadLimitKey::RecordPayloadLen, len)?;
    let absolute = aux_absolute_offset(layout.aux(name)?, offset, len)?;
    validate_range(absolute, len, logical_file_len)?;
    file.seek(SeekFrom::Start(absolute))?;
    file.write_all(payload)?;
    Ok(())
}

#[cfg(feature = "mmap")]
pub(crate) fn cell_payload_parts<T: VarveMatrixBlock>(
    spec: FormatSpec,
    layout: &MatrixLayout,
    key: MatrixKey,
) -> Result<(u64, u64, Option<u64>)> {
    ensure_matrix_block::<T>(spec)?;
    if cell_status::<T>(spec, layout, key)? == MatrixCellStatus::NotCommitted {
        return Err(Error::MatrixNotCommitted);
    }
    let block_index = layout.block_index(T::ID)?;
    let ordinal = layout.ordinal_for_block(block_index, key)?;
    let offset = layout.slot_offset(block_index, ordinal)?;
    let block = &layout.blocks[block_index];
    let crc_offset = block
        .crc_offset
        .map(|crc_offset| indexed_crc_offset(crc_offset, ordinal))
        .transpose()?;
    Ok((offset, block.slot_stride, crc_offset))
}

#[cfg(feature = "mmap")]
pub(crate) fn verify_payload_crc_bytes(
    payload_offset: u64,
    payload: &[u8],
    expected: u32,
) -> Result<()> {
    let actual = crc32_bytes(payload)?;
    if actual != expected {
        return Err(Error::MatrixChecksumMismatch {
            offset: payload_offset,
            expected,
            actual,
        });
    }
    Ok(())
}

pub(crate) fn resume_signal(layout: &MatrixLayout, category: &str) -> Result<MatrixResumeSignal> {
    layout.ensure_fatal_access_allowed()?;
    let commit = &layout.commits[layout.commit_index(category)?];
    let committed = count_committed(commit)?;
    if committed == 0 || committed == commit.bit_count {
        Ok(MatrixResumeSignal::Clean)
    } else {
        Ok(MatrixResumeSignal::Partial {
            committed,
            total: commit.bit_count,
        })
    }
}

pub(crate) fn sidecar_resume_signal(
    layout: &MatrixLayout,
    category: &str,
    sidecar_exists: bool,
) -> Result<MatrixResumeSignal> {
    layout.ensure_fatal_access_allowed()?;
    layout.read_limits.check(ReadLimitKey::SidecarLen, 0)?;
    let commit = &layout.commits[layout.commit_index(category)?];
    let committed = count_committed(commit)?;
    if committed == 0 || committed == commit.bit_count {
        if sidecar_exists {
            Ok(MatrixResumeSignal::DiscardRecommended)
        } else {
            Ok(MatrixResumeSignal::Clean)
        }
    } else if sidecar_exists {
        Ok(MatrixResumeSignal::ResumeAvailable {
            committed,
            total: commit.bit_count,
        })
    } else {
        Ok(MatrixResumeSignal::RestartRecommended)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[allow(dead_code)]
pub(crate) struct MatrixSidecarReadPlan {
    pub(crate) format_magic_offset: u64,
    pub(crate) category_offset: u64,
    pub(crate) payload_offset: u64,
    pub(crate) payload_len: u64,
    pub(crate) total_len: u64,
}

#[allow(dead_code)]
pub(crate) fn check_matrix_sidecar_file_len(spec: FormatSpec, file_len: u64) -> Result<()> {
    spec.read_limits.check(ReadLimitKey::SidecarLen, file_len)
}

#[allow(dead_code)]
pub(crate) fn matrix_sidecar_write_len(
    spec: FormatSpec,
    fixed_header_len: u64,
    format_magic_len: u64,
    category_len: u64,
    payload_len: u64,
) -> Result<u64> {
    spec.read_limits
        .check(ReadLimitKey::MaterializedBytes, payload_len)?;
    let metadata_len = fixed_header_len
        .checked_add(format_magic_len)
        .and_then(|len| len.checked_add(category_len))
        .ok_or(Error::ResourceArithmeticOverflow {
            resource: MATRIX_SIDECAR_RESOURCE,
        })?;
    let total_len =
        metadata_len
            .checked_add(payload_len)
            .ok_or(Error::ResourceArithmeticOverflow {
                resource: MATRIX_SIDECAR_RESOURCE,
            })?;
    check_matrix_sidecar_file_len(spec, total_len)?;
    Ok(total_len)
}

#[allow(clippy::too_many_arguments)]
#[allow(dead_code)]
pub(crate) fn matrix_sidecar_read_plan(
    spec: FormatSpec,
    file_len: u64,
    fixed_header_len: u64,
    format_magic_len: u64,
    category_len: u64,
    payload_len: u64,
    flags: u16,
    reserved: u16,
    trailing_reserved: u32,
) -> Result<MatrixSidecarReadPlan> {
    check_matrix_sidecar_file_len(spec, file_len)?;
    if flags != 0 || reserved != 0 || trailing_reserved != 0 {
        return Err(Error::InvalidMatrixSidecar);
    }
    let total_len = matrix_sidecar_write_len(
        spec,
        fixed_header_len,
        format_magic_len,
        category_len,
        payload_len,
    )?;
    if total_len != file_len {
        return Err(Error::InvalidMatrixSidecar);
    }
    let category_offset = fixed_header_len.checked_add(format_magic_len).ok_or(
        Error::ResourceArithmeticOverflow {
            resource: MATRIX_SIDECAR_RESOURCE,
        },
    )?;
    let payload_offset =
        category_offset
            .checked_add(category_len)
            .ok_or(Error::ResourceArithmeticOverflow {
                resource: MATRIX_SIDECAR_RESOURCE,
            })?;
    Ok(MatrixSidecarReadPlan {
        format_magic_offset: fixed_header_len,
        category_offset,
        payload_offset,
        payload_len,
        total_len,
    })
}

pub(crate) fn recovery_report(layout: &MatrixLayout) -> MatrixRecoveryReport {
    let mut findings = layout.crc_findings.clone();
    let mut recommended_actions = findings
        .iter()
        .filter_map(|finding| match finding.kind {
            MatrixCorruptionKind::Slot => Some(MatrixRecoveryAction::ClearCategory {
                category: "unknown".to_string(),
            }),
            _ => None,
        })
        .collect::<Vec<_>>();
    for commit in &layout.commits {
        if let Some(finding) = &commit.quarantine_finding {
            findings.push(finding.clone());
            recommended_actions.push(match commit.kind {
                MatrixCommitKind::Cell => MatrixRecoveryAction::RebuildCommitMap {
                    category: Some(commit.name.clone()),
                },
                MatrixCommitKind::Single | MatrixCommitKind::PerChannel => {
                    MatrixRecoveryAction::ClearCategory {
                        category: commit.name.clone(),
                    }
                }
            });
        }
        if let Ok(committed) = count_committed(commit)
            && committed > 0
            && committed < commit.bit_count
        {
            findings.push(MatrixRecoveryFinding {
                kind: MatrixCorruptionKind::Sidecar,
                severity: MatrixCorruptionSeverity::Advisory,
                message: format!(
                    "partial matrix progress in category {}: {committed}/{}",
                    commit.name, commit.bit_count
                ),
            });
            recommended_actions.push(MatrixRecoveryAction::Resume);
        }
    }
    MatrixRecoveryReport {
        findings,
        recommended_actions,
    }
}

pub(crate) fn rebuild_commit_map_from_crc<T: VarveMatrixBlock>(
    spec: FormatSpec,
    layout: &mut MatrixLayout,
    file: &mut File,
) -> Result<u64> {
    ensure_matrix_block::<T>(spec)?;
    layout.ensure_fatal_access_allowed()?;
    let block_index = layout.block_index(T::ID)?;
    let block = &layout.blocks[block_index];
    let Some(crc_offset) = block.crc_offset else {
        return Err(Error::IntegrityFeatureDisabled);
    };
    let commit_index = layout.commit_index(T::CATEGORY)?;
    if layout.commits[commit_index].kind != MatrixCommitKind::Cell
        || layout.commits[commit_index].bit_count != block.cell_count
    {
        return Err(Error::InvalidMatrixLayout);
    }

    // The rebuilt map is materialised page by page and charged the same way,
    // so a rebuild is admitted on the pages it actually needs rather than on
    // the dense worst case of the whole map.
    let mut budget = ResidentBitmapBudget {
        limits: layout.read_limits,
        used: layout.resident_bitmap_bytes,
    };
    let mut rebuilt = SparseBitmap::new(layout.commits[commit_index].bit_count)?;
    let mut committed = 0u64;
    for ordinal in 0..block.cell_count {
        let slot_offset = layout.slot_offset(block_index, ordinal)?;
        let actual = crc32_file_range(file, slot_offset, block.slot_stride)?;
        let stored = read_crc_at(file, indexed_crc_offset(crc_offset, ordinal)?)?;
        let valid = block.crc_valid_bits.get(ordinal)? && actual == stored;
        if valid {
            committed += 1;
        }
        budget.charge(rebuilt.set(ordinal, valid)?)?;
    }

    let commit = &layout.commits[commit_index];
    write_commit_map_pages(file, commit.map_offset, commit.digest_offset, &rebuilt)?;
    let commit = &mut layout.commits[commit_index];
    let released = commit
        .bits
        .resident_bytes()
        .checked_add(
            commit
                .quarantined_raw_bits
                .as_ref()
                .map(SparseBitmap::resident_bytes)
                .unwrap_or(0),
        )
        .ok_or(Error::InvalidMatrixLayout)?;
    commit.bits = rebuilt;
    commit.quarantined_raw_bits = None;
    commit.quarantine_finding = None;
    layout.resident_bitmap_bytes = budget
        .used
        .checked_sub(released)
        .ok_or(Error::InvalidMatrixLayout)?;
    Ok(committed)
}

pub(crate) fn is_single_committed(layout: &MatrixLayout, name: &str) -> Result<bool> {
    let commit = &layout.commits[layout.commit_index(name)?];
    if commit.kind != MatrixCommitKind::Single {
        return Err(Error::MatrixCommitMissing(name.to_string()));
    }
    ensure_commit_publishable(layout, name)?;
    commit.bits.get(0)
}

pub(crate) fn set_single_committed(
    layout: &mut MatrixLayout,
    file: &mut File,
    name: &str,
    value: bool,
) -> Result<()> {
    let index = layout.commit_index(name)?;
    if layout.commits[index].kind != MatrixCommitKind::Single {
        return Err(Error::MatrixCommitMissing(name.to_string()));
    }
    set_commit_bit(layout, file, index, 0, value)
}

pub(crate) fn is_channel_committed(
    layout: &MatrixLayout,
    name: &str,
    channel: u64,
) -> Result<bool> {
    let commit = &layout.commits[layout.commit_index(name)?];
    if commit.kind != MatrixCommitKind::PerChannel || channel >= commit.bit_count {
        return Err(Error::MatrixCommitMissing(name.to_string()));
    }
    ensure_commit_publishable(layout, name)?;
    commit.bits.get(channel)
}

pub(crate) fn set_channel_committed(
    layout: &mut MatrixLayout,
    file: &mut File,
    name: &str,
    channel: u64,
    value: bool,
) -> Result<()> {
    let index = layout.commit_index(name)?;
    if layout.commits[index].kind != MatrixCommitKind::PerChannel
        || channel >= layout.commits[index].bit_count
    {
        return Err(Error::MatrixCommitMissing(name.to_string()));
    }
    set_commit_bit(layout, file, index, channel, value)
}

fn ensure_matrix_block<T: VarveMatrixBlock>(spec: FormatSpec) -> Result<()> {
    // DEF-01: run the common registration gate first, exactly like the
    // fixed/variable block paths. It enforces the process-local first-seen
    // schema-fingerprint and keyedness contract, so a manual matrix type
    // with the same shape/stride but different codec/decode semantics is
    // rejected before any cell read, write, or mmap view.
    crate::collections::ensure_registered_block::<T>(spec)?;
    let descriptor = spec.block(T::ID).ok_or(Error::UnregisteredBlock(T::ID))?;
    if descriptor.kind != BlockKind::Matrix || T::KIND != BlockKind::Matrix {
        return Err(Error::BlockKindMismatch {
            expected: BlockKind::Matrix,
            actual: T::KIND,
        });
    }
    if descriptor.version != T::VERSION {
        return Err(Error::BlockVersionMismatch {
            block_id: T::ID,
            expected: descriptor.version,
            actual: T::VERSION,
        });
    }
    let matrix = spec
        .matrix_blocks
        .iter()
        .find(|block| block.block_id == T::ID)
        .ok_or(Error::MatrixBlockMissing(T::ID))?;
    if matrix.dimensions != T::DIMENSIONS
        || matrix.category != T::CATEGORY
        || matrix.slot_stride != T::SLOT_STRIDE
    {
        return Err(Error::InvalidMatrixLayout);
    }
    Ok(())
}

fn block_index_for_category(
    spec: FormatSpec,
    layout: &MatrixLayout,
    category: &str,
) -> Result<usize> {
    let block = spec
        .matrix_blocks
        .iter()
        .find(|block| block.category == category)
        .ok_or_else(|| Error::MatrixCommitMissing(category.to_string()))?;
    layout.block_index(block.block_id)
}

fn ensure_commit_publishable(layout: &MatrixLayout, category: &str) -> Result<()> {
    layout.ensure_fatal_access_allowed()?;
    let commit = &layout.commits[layout.commit_index(category)?];
    if commit.quarantined_raw_bits.is_some() {
        return Err(Error::MatrixCommitQuarantined(category.to_string()));
    }
    Ok(())
}

fn set_cell_commit(
    layout: &mut MatrixLayout,
    file: &mut File,
    category: &str,
    ordinal: u64,
    value: bool,
) -> Result<()> {
    let (commit_index, update) = prepare_cell_commit(layout, category, ordinal, value)?;
    apply_commit_bit(layout, file, commit_index, update)
}

fn prepare_cell_commit(
    layout: &MatrixLayout,
    category: &str,
    ordinal: u64,
    value: bool,
) -> Result<(usize, CommitBitUpdate)> {
    let commit_index = layout.commit_index(category)?;
    if layout.commits[commit_index].kind != MatrixCommitKind::Cell {
        return Err(Error::MatrixCommitMissing(category.to_string()));
    }
    let update = prepare_commit_bit(layout, commit_index, ordinal, value)?;
    Ok((commit_index, update))
}

fn set_commit_bit(
    layout: &mut MatrixLayout,
    file: &mut File,
    commit_index: usize,
    ordinal: u64,
    value: bool,
) -> Result<()> {
    let update = prepare_commit_bit(layout, commit_index, ordinal, value)?;
    apply_commit_bit(layout, file, commit_index, update)
}

struct BitmapByteUpdate {
    byte_index: u64,
    byte_offset: u64,
    byte_value: u8,
}

struct CommitBitUpdate {
    bitmap: BitmapByteUpdate,
    digest: Option<(u64, u32)>,
}

fn prepare_bitmap_update(
    bits: &SparseBitmap,
    bit_count: u64,
    base_offset: u64,
    ordinal: u64,
    value: bool,
) -> Result<BitmapByteUpdate> {
    if ordinal >= bit_count {
        return Err(Error::InvalidMatrixLayout);
    }
    let byte_index = ordinal / 8;
    let current = bits.byte(byte_index)?;
    let mask = 1u8 << (ordinal % 8);
    let byte_value = if value {
        current | mask
    } else {
        current & !mask
    };
    let byte_offset = base_offset
        .checked_add(byte_index)
        .ok_or(Error::InvalidMatrixLayout)?;
    Ok(BitmapByteUpdate {
        byte_index,
        byte_offset,
        byte_value,
    })
}

fn write_bitmap_byte(file: &mut File, update: &BitmapByteUpdate) -> Result<()> {
    #[cfg(test)]
    if FAIL_NEXT_BITMAP_WRITE.replace(false) {
        return Err(Error::Io(std::io::Error::other(
            "injected matrix bitmap write failure",
        )));
    }

    file.seek(SeekFrom::Start(update.byte_offset))?;
    file.write_all(&[update.byte_value])?;
    Ok(())
}

fn prepare_commit_bit(
    layout: &MatrixLayout,
    commit_index: usize,
    ordinal: u64,
    value: bool,
) -> Result<CommitBitUpdate> {
    layout.ensure_fatal_access_allowed()?;
    let commit = &layout.commits[commit_index];
    if commit.quarantined_raw_bits.is_some() {
        return Err(Error::MatrixCommitQuarantined(commit.name.clone()));
    }
    let bitmap = prepare_bitmap_update(
        &commit.bits,
        commit.bit_count,
        commit.map_offset,
        ordinal,
        value,
    )?;
    // PERF-01: only the page holding the mutated byte is rehashed, so the cost
    // of a commit-bit mutation is bounded by `BITMAP_PAGE_BYTES` regardless of
    // how many bits the category has.
    let digest = match commit.digest_offset {
        Some(base) => {
            let page = bitmap.byte_index / BITMAP_PAGE_BYTES;
            let within = usize::try_from(bitmap.byte_index % BITMAP_PAGE_BYTES)
                .map_err(|_| Error::InvalidMatrixLayout)?;
            let page_bytes = commit.bits.page_bytes(page)?;
            count_bitmap_bytes_hashed(usize_to_u64(page_bytes.len())?);
            let crc = crc32_bytes_with_replacement(page_bytes, within, bitmap.byte_value)?;
            Some((page_digest_offset(base, page)?, crc))
        }
        None => None,
    };
    Ok(CommitBitUpdate { bitmap, digest })
}

fn apply_commit_bit(
    layout: &mut MatrixLayout,
    file: &mut File,
    commit_index: usize,
    update: CommitBitUpdate,
) -> Result<()> {
    let cost = layout.commits[commit_index]
        .bits
        .materialisation_cost(update.bitmap.byte_index, update.bitmap.byte_value)?;
    layout.charge_resident_bitmap(cost)?;
    if let Some((offset, crc)) = update.digest {
        write_page_digest(file, offset, crc, PAGE_STATE_INITIALIZED)?;
    }
    write_bitmap_byte(file, &update.bitmap)?;
    layout.commits[commit_index]
        .bits
        .set_byte(update.bitmap.byte_index, update.bitmap.byte_value)?;
    Ok(())
}

// Whole-map publication (rebuild and recovery) rewrites every page together
// with its digest, keeping full-map verification available without ever making
// a single-bit mutation cost more than one page.
fn write_commit_map_pages(
    file: &mut File,
    map_offset: u64,
    digest_offset: Option<u64>,
    bits: &SparseBitmap,
) -> Result<()> {
    for page in 0..bits.page_count {
        let bytes = bits.page_bytes(page)?;
        if let Some(base) = digest_offset {
            write_page_digest(
                file,
                page_digest_offset(base, page)?,
                crc32_bytes(bytes)?,
                PAGE_STATE_INITIALIZED,
            )?;
        }
        let offset = page
            .checked_mul(BITMAP_PAGE_BYTES)
            .and_then(|delta| map_offset.checked_add(delta))
            .ok_or(Error::InvalidMatrixLayout)?;
        file.seek(SeekFrom::Start(offset))?;
        file.write_all(bytes)?;
    }
    Ok(())
}

fn update_cell_crc(
    layout: &MatrixLayout,
    file: &mut File,
    block_index: usize,
    ordinal: u64,
) -> Result<()> {
    let Some(crc_offset) = layout.blocks[block_index].crc_offset else {
        return Ok(());
    };
    let offset = layout.slot_offset(block_index, ordinal)?;
    let stride = layout.blocks[block_index].slot_stride;
    let crc = crc32_file_range(file, offset, stride)?;
    write_crc_at(file, indexed_crc_offset(crc_offset, ordinal)?, crc)
}

fn clear_cell_crc(
    layout: &MatrixLayout,
    file: &mut File,
    block_index: usize,
    ordinal: u64,
) -> Result<()> {
    let Some(crc_offset) = layout.blocks[block_index].crc_offset else {
        return Ok(());
    };
    let zero_crc = crc32_zeroes(layout.blocks[block_index].slot_stride)?;
    write_crc_at(file, indexed_crc_offset(crc_offset, ordinal)?, zero_crc)
}

fn set_cell_crc_valid(
    layout: &mut MatrixLayout,
    file: &mut File,
    block_index: usize,
    ordinal: u64,
    value: bool,
) -> Result<()> {
    let Some(update) = prepare_cell_crc_valid(layout, block_index, ordinal, value)? else {
        return Ok(());
    };
    apply_cell_crc_valid(layout, file, block_index, update)
}

fn prepare_cell_crc_valid(
    layout: &MatrixLayout,
    block_index: usize,
    ordinal: u64,
    value: bool,
) -> Result<Option<BitmapByteUpdate>> {
    let block = &layout.blocks[block_index];
    let Some(valid_offset) = block.crc_valid_offset else {
        return Ok(None);
    };
    prepare_bitmap_update(
        &block.crc_valid_bits,
        block.cell_count,
        valid_offset,
        ordinal,
        value,
    )
    .map(Some)
}

fn apply_cell_crc_valid(
    layout: &mut MatrixLayout,
    file: &mut File,
    block_index: usize,
    update: BitmapByteUpdate,
) -> Result<()> {
    let cost = layout.blocks[block_index]
        .crc_valid_bits
        .materialisation_cost(update.byte_index, update.byte_value)?;
    layout.charge_resident_bitmap(cost)?;
    write_bitmap_byte(file, &update)?;
    layout.blocks[block_index]
        .crc_valid_bits
        .set_byte(update.byte_index, update.byte_value)?;
    Ok(())
}

fn verify_cell_crc(
    layout: &MatrixLayout,
    file: &mut File,
    block_index: usize,
    ordinal: u64,
    payload: &[u8],
) -> Result<()> {
    let Some(crc_offset) = layout.blocks[block_index].crc_offset else {
        return Ok(());
    };
    // PERF-02: the per-cell checksum region is no longer initialised at create,
    // so the validity bit is the authority on whether a stored checksum exists.
    // Without it a never-written cell would be checked against a zero extent.
    if layout.blocks[block_index].crc_valid_offset.is_some()
        && !layout.blocks[block_index].crc_valid_bits.get(ordinal)?
    {
        return Err(Error::MatrixChecksumMismatch {
            offset: layout.slot_offset(block_index, ordinal)?,
            expected: 0,
            actual: crc32_bytes(payload)?,
        });
    }
    let expected = read_crc_at(file, indexed_crc_offset(crc_offset, ordinal)?)?;
    let actual = crc32_bytes(payload)?;
    if expected != actual {
        return Err(Error::MatrixChecksumMismatch {
            offset: layout.slot_offset(block_index, ordinal)?,
            expected,
            actual,
        });
    }
    Ok(())
}

fn slot_is_all_zero(
    layout: &MatrixLayout,
    file: &mut File,
    block_index: usize,
    ordinal: u64,
) -> Result<bool> {
    let offset = layout.slot_offset(block_index, ordinal)?;
    let stride = layout.blocks[block_index].slot_stride;
    file.seek(SeekFrom::Start(offset))?;
    let mut buffer = [0u8; 64 * 1024];
    let mut remaining = stride;
    while remaining != 0 {
        let chunk_len = usize::try_from(remaining.min(buffer.len() as u64))
            .map_err(|_| Error::LengthOverflow { value: remaining })?;
        file.read_exact(&mut buffer[..chunk_len])?;
        if buffer[..chunk_len].iter().any(|byte| *byte != 0) {
            return Ok(false);
        }
        remaining -= chunk_len as u64;
    }
    Ok(true)
}

#[cfg(feature = "integrity")]
fn crc32_file_range(file: &mut File, offset: u64, len: u64) -> Result<u32> {
    file.seek(SeekFrom::Start(offset))?;
    let mut hasher = crc32fast::Hasher::new();
    let mut buffer = [0u8; 64 * 1024];
    let mut remaining = len;
    while remaining != 0 {
        let chunk_len = usize::try_from(remaining.min(buffer.len() as u64))
            .map_err(|_| Error::LengthOverflow { value: remaining })?;
        file.read_exact(&mut buffer[..chunk_len])?;
        hasher.update(&buffer[..chunk_len]);
        remaining -= chunk_len as u64;
    }
    Ok(hasher.finalize())
}

#[cfg(not(feature = "integrity"))]
fn crc32_file_range(_file: &mut File, _offset: u64, _len: u64) -> Result<u32> {
    Err(Error::IntegrityFeatureDisabled)
}

// The set-bit total is maintained incrementally by `SparseBitmap`, so progress
// and resume reporting no longer scan the whole category per call.
fn count_committed(commit: &MatrixCommitLayout) -> Result<u64> {
    Ok(commit.bits.ones())
}

fn dimension_values(
    spec: FormatSpec,
    dims: &MatrixDimensions,
) -> Result<Vec<MatrixDimensionValue>> {
    let mut values = Vec::new();
    try_reserve_vec(
        &mut values,
        spec.matrix_dimensions.len(),
        MATRIX_DESCRIPTOR_RESOURCE,
    )?;
    for dimension in spec.matrix_dimensions {
        let value = dims
            .get(dimension.name)
            .ok_or_else(|| Error::MatrixDimensionMissing(dimension.name.to_string()))?;
        values.push(MatrixDimensionValue {
            name: dimension.name.to_string(),
            value,
        });
    }
    Ok(values)
}

fn matrix_cell_counts(
    spec: FormatSpec,
    dimensions: &[MatrixDimensionValue],
) -> Result<HashMap<String, u64>> {
    let mut counts = HashMap::new();
    try_reserve_map(
        &mut counts,
        spec.matrix_blocks.len(),
        ReadLimitKey::MatrixCells.resource(),
    )?;
    let mut aggregate = 0u64;
    for block in spec.matrix_blocks {
        let dim0 = dimensions
            .iter()
            .find(|dimension| dimension.name == block.dimensions[0])
            .map(|dimension| dimension.value)
            .ok_or_else(|| Error::MatrixDimensionMissing(block.dimensions[0].to_string()))?;
        let dim1 = dimensions
            .iter()
            .find(|dimension| dimension.name == block.dimensions[1])
            .map(|dimension| dimension.value)
            .ok_or_else(|| Error::MatrixDimensionMissing(block.dimensions[1].to_string()))?;
        let count = dim0
            .checked_mul(dim1)
            .ok_or(Error::ResourceArithmeticOverflow {
                resource: ReadLimitKey::MatrixCells.resource(),
            })?;
        spec.read_limits.check(ReadLimitKey::MatrixCells, count)?;
        aggregate = aggregate
            .checked_add(count)
            .ok_or(Error::ResourceArithmeticOverflow {
                resource: ReadLimitKey::MatrixCells.resource(),
            })?;
        spec.read_limits
            .check(ReadLimitKey::MatrixCells, aggregate)?;
        match counts.insert(block.category.to_string(), count) {
            Some(existing) if existing != count => return Err(Error::InvalidMatrixLayout),
            _ => {}
        }
    }
    Ok(counts)
}

fn matrix_commit_plans(
    spec: FormatSpec,
    dimensions: &[MatrixDimensionValue],
    cell_counts: &HashMap<String, u64>,
) -> Result<Vec<CommitPlan>> {
    let mut plans = Vec::new();
    try_reserve_vec(
        &mut plans,
        spec.matrix_commits.len(),
        MATRIX_DESCRIPTOR_RESOURCE,
    )?;
    for commit in spec.matrix_commits {
        let bit_count = match commit.kind {
            MatrixCommitKind::Cell => *cell_counts
                .get(commit.name)
                .ok_or_else(|| Error::MatrixCommitMissing(commit.name.to_string()))?,
            MatrixCommitKind::Single => 1,
            MatrixCommitKind::PerChannel => per_channel_count(dimensions)?,
        };
        plans.push((commit.name.to_string(), commit.kind, bit_count));
    }
    Ok(plans)
}

fn matrix_descriptor_table_lengths(spec: FormatSpec) -> Result<(u64, u64, u64)> {
    let dimension_table_len = spec
        .matrix_dimensions
        .iter()
        .try_fold(0u64, |len, dimension| {
            let entry_len = usize_to_u64(dimension.name.len())?
                .checked_add(10)
                .ok_or(Error::InvalidMatrixLayout)?;
            len.checked_add(entry_len).ok_or(Error::InvalidMatrixLayout)
        })?;
    let block_table_len = usize_to_u64(spec.matrix_blocks.len())?
        .checked_mul(44)
        .ok_or(Error::InvalidMatrixLayout)?;
    let category_table_len = spec.matrix_commits.iter().try_fold(0u64, |len, commit| {
        let entry_len = usize_to_u64(commit.name.len())?
            .checked_add(30)
            .ok_or(Error::InvalidMatrixLayout)?;
        len.checked_add(entry_len).ok_or(Error::InvalidMatrixLayout)
    })?;
    Ok((dimension_table_len, block_table_len, category_table_len))
}

fn matrix_commit_map_len(commit_plans: &[CommitPlan]) -> Result<u64> {
    commit_plans
        .iter()
        .try_fold(0u64, |len, (_, _, bit_count)| {
            len.checked_add(bit_bytes(*bit_count)?)
                .ok_or(Error::InvalidMatrixLayout)
        })
}

fn check_matrix_dimensions(limits: ReadLimits, dimensions: &[MatrixDimensionValue]) -> Result<()> {
    for dimension in dimensions {
        limits.check(ReadLimitKey::MatrixDimension, dimension.value)?;
    }
    Ok(())
}

fn check_matrix_metadata_limit(
    limits: ReadLimits,
    dimension_table_len: u64,
    block_table_len: u64,
    category_table_len: u64,
) -> Result<()> {
    let metadata_len = u64::from(VMAT_HEADER_LEN)
        .checked_add(dimension_table_len)
        .and_then(|len| len.checked_add(block_table_len))
        .and_then(|len| len.checked_add(category_table_len))
        .ok_or(Error::ResourceArithmeticOverflow {
            resource: ReadLimitKey::MatrixMetadataBytes.resource(),
        })?;
    limits.check(ReadLimitKey::MatrixMetadataBytes, metadata_len)
}

fn matrix_block_bitmap_len(spec: FormatSpec, cell_counts: &HashMap<String, u64>) -> Result<u64> {
    spec.matrix_blocks.iter().try_fold(0u64, |total, block| {
        let cell_count = *cell_counts
            .get(block.category)
            .ok_or_else(|| Error::MatrixCommitMissing(block.category.to_string()))?;
        total
            .checked_add(bit_bytes(cell_count)?)
            .ok_or(Error::ResourceArithmeticOverflow {
                resource: ReadLimitKey::MatrixBitmapBytes.resource(),
            })
    })
}

fn matrix_accounted_crc_len(
    crc_enabled: bool,
    region_crc_len: u64,
    block_bitmap_len: u64,
) -> Result<u64> {
    if crc_enabled {
        region_crc_len
            .checked_add(block_bitmap_len)
            .ok_or(Error::ResourceArithmeticOverflow {
                resource: ReadLimitKey::MatrixCrcBytes.resource(),
            })
    } else {
        Ok(0)
    }
}

fn zero_commit_bitmaps(commit_plans: &[CommitPlan]) -> Result<Vec<SparseBitmap>> {
    let mut bitmaps = Vec::new();
    try_reserve_vec(
        &mut bitmaps,
        commit_plans.len(),
        ReadLimitKey::MatrixBitmapBytes.resource(),
    )?;
    for (_, _, bit_count) in commit_plans {
        bitmaps.push(SparseBitmap::new(*bit_count)?);
    }
    Ok(bitmaps)
}

fn zero_crc_valid_bitmaps(
    spec: FormatSpec,
    crc_enabled: bool,
    block_offsets: &HashMap<u32, (u64, u64, u64)>,
) -> Result<HashMap<u32, SparseBitmap>> {
    let mut bitmaps = HashMap::new();
    if !crc_enabled {
        return Ok(bitmaps);
    }
    try_reserve_map(
        &mut bitmaps,
        spec.matrix_blocks.len(),
        ReadLimitKey::MatrixCrcBytes.resource(),
    )?;
    for block in spec.matrix_blocks {
        let (_, _, cell_count) = *block_offsets
            .get(&block.block_id)
            .ok_or(Error::MatrixBlockMissing(block.block_id))?;
        bitmaps.insert(block.block_id, SparseBitmap::new(cell_count)?);
    }
    Ok(bitmaps)
}

fn matrix_slot_region_len(spec: FormatSpec, cell_counts: &HashMap<String, u64>) -> Result<u64> {
    spec.matrix_blocks.iter().try_fold(0u64, |len, block| {
        let cell_count = *cell_counts
            .get(block.category)
            .ok_or_else(|| Error::MatrixCommitMissing(block.category.to_string()))?;
        let block_len = cell_count
            .checked_mul(block.slot_stride)
            .ok_or(Error::InvalidMatrixLayout)?;
        len.checked_add(block_len).ok_or(Error::InvalidMatrixLayout)
    })
}

fn per_channel_count(dimensions: &[MatrixDimensionValue]) -> Result<u64> {
    dimensions
        .iter()
        .find(|dimension| {
            matches!(
                dimension.name.as_str(),
                "ch" | "channel" | "channels" | "n_channels"
            )
        })
        .or_else(|| dimensions.get(1))
        .map(|dimension| dimension.value)
        .ok_or_else(|| Error::MatrixDimensionMissing("channel".to_string()))
}

fn matrix_aux_region_len(spec: FormatSpec) -> Result<u64> {
    spec.matrix_aux.iter().try_fold(0u64, |acc, aux| {
        acc.checked_add(aux.byte_len)
            .ok_or(Error::InvalidMatrixLayout)
    })
}

fn matrix_aux_offsets(spec: FormatSpec, start: u64) -> Result<HashMap<String, (u64, u64)>> {
    let mut offsets = HashMap::new();
    try_reserve_map(
        &mut offsets,
        spec.matrix_aux.len(),
        MATRIX_DESCRIPTOR_RESOURCE,
    )?;
    let mut next = start;
    for aux in spec.matrix_aux {
        offsets.insert(aux.name.to_string(), (next, aux.byte_len));
        next = next
            .checked_add(aux.byte_len)
            .ok_or(Error::InvalidMatrixLayout)?;
    }
    Ok(offsets)
}

fn aux_absolute_offset(aux: &MatrixAuxLayout, offset: u64, len: u64) -> Result<u64> {
    let end = offset
        .checked_add(len)
        .ok_or_else(|| Error::MatrixAuxOutOfBounds {
            name: aux.name.clone(),
            offset,
            len,
            byte_len: aux.byte_len,
        })?;
    if end > aux.byte_len {
        return Err(Error::MatrixAuxOutOfBounds {
            name: aux.name.clone(),
            offset,
            len,
            byte_len: aux.byte_len,
        });
    }
    aux.offset
        .checked_add(offset)
        .ok_or(Error::InvalidMatrixLayout)
}

#[allow(clippy::too_many_arguments)]
fn layout_from_parts(
    dimensions: Vec<MatrixDimensionValue>,
    spec: FormatSpec,
    commit_plans: &[CommitPlan],
    commit_offsets: &HashMap<String, (u64, u64)>,
    block_offsets: &HashMap<u32, (u64, u64, u64)>,
    aux_offsets: &HashMap<String, (u64, u64)>,
    commit_bits: Vec<SparseBitmap>,
    crc: Option<MatrixCrcLayout>,
    mut crc_valid_bits: HashMap<u32, SparseBitmap>,
    mut commit_findings: HashMap<String, MatrixRecoveryFinding>,
    crc_findings: Vec<MatrixRecoveryFinding>,
    append_log_start: u64,
    resident_bitmap_bytes: u64,
) -> Result<MatrixLayout> {
    if commit_bits.len() != commit_plans.len() {
        return Err(Error::InvalidMatrixLayout);
    }
    let has_fatal_finding = crc_findings
        .iter()
        .chain(commit_findings.values())
        .any(|finding| finding.severity == MatrixCorruptionSeverity::Fatal);
    let fatal_access_blocked = has_fatal_finding && !spec.matrix_fatal_forensics;
    let mut commits = Vec::new();
    try_reserve_vec(&mut commits, commit_plans.len(), MATRIX_DESCRIPTOR_RESOURCE)?;
    for (commit_index, ((name, kind, bit_count), raw_bits)) in
        commit_plans.iter().zip(commit_bits).enumerate()
    {
        let (map_offset, map_len) = *commit_offsets
            .get(name)
            .ok_or_else(|| Error::MatrixCommitMissing(name.clone()))?;
        if raw_bits.byte_len != map_len {
            return Err(Error::InvalidMatrixLayout);
        }
        let quarantine_finding = commit_findings.remove(name);
        // Quarantine retains the raw map that the load already charged and
        // installs an empty replacement beside it, so it takes no further
        // resident bytes. The charge that admits it happened page by page as
        // the map was read.
        let (bits, quarantined_raw_bits) = if quarantine_finding.is_some() {
            spec.read_limits
                .check(ReadLimitKey::MatrixBitmapBytes, resident_bitmap_bytes)?;
            (SparseBitmap::new(*bit_count)?, Some(raw_bits))
        } else {
            (raw_bits, None)
        };
        commits.push(MatrixCommitLayout {
            name: name.clone(),
            kind: *kind,
            bit_count: *bit_count,
            map_offset,
            bits,
            quarantined_raw_bits,
            quarantine_finding,
            digest_offset: crc
                .as_ref()
                .map(|crc| {
                    crc.commit_digest_offsets
                        .get(commit_index)
                        .copied()
                        .ok_or(Error::InvalidMatrixLayout)
                })
                .transpose()?,
        });
    }
    if !commit_findings.is_empty() {
        return Err(Error::InvalidMatrixLayout);
    }
    let mut blocks = Vec::new();
    try_reserve_vec(
        &mut blocks,
        spec.matrix_blocks.len(),
        MATRIX_DESCRIPTOR_RESOURCE,
    )?;
    for (block_index, block) in spec.matrix_blocks.iter().enumerate() {
        let (slot_region_offset, slot_region_len, cell_count) = *block_offsets
            .get(&block.block_id)
            .ok_or(Error::MatrixBlockMissing(block.block_id))?;
        if slot_region_len
            != cell_count
                .checked_mul(block.slot_stride)
                .ok_or(Error::InvalidMatrixLayout)?
        {
            return Err(Error::InvalidMatrixLayout);
        }
        let crc_valid_len = bit_bytes(cell_count)?;
        let crc_valid_bits_for_block = match &crc {
            Some(_) => crc_valid_bits
                .remove(&block.block_id)
                .ok_or(Error::InvalidMatrixLayout)?,
            None => SparseBitmap::new(0)?,
        };
        if crc.is_some() && crc_valid_bits_for_block.byte_len != crc_valid_len {
            return Err(Error::InvalidMatrixLayout);
        }
        let current_write_bits = SparseBitmap::new(cell_count)?;
        blocks.push(MatrixBlockLayout {
            block_id: block.block_id,
            dimensions: [
                block.dimensions[0].to_string(),
                block.dimensions[1].to_string(),
            ],
            slot_stride: block.slot_stride,
            slot_region_offset,
            cell_count,
            crc_offset: crc
                .as_ref()
                .map(|crc| {
                    crc.block_crc_offsets
                        .get(block_index)
                        .copied()
                        .ok_or(Error::InvalidMatrixLayout)
                })
                .transpose()?,
            crc_valid_offset: crc
                .as_ref()
                .map(|crc| {
                    crc.block_valid_offsets
                        .get(block_index)
                        .copied()
                        .ok_or(Error::InvalidMatrixLayout)
                })
                .transpose()?,
            crc_valid_bits: crc_valid_bits_for_block,
            current_write_bits,
        });
    }
    if !crc_valid_bits.is_empty() {
        return Err(Error::InvalidMatrixLayout);
    }
    let mut aux = Vec::new();
    try_reserve_vec(&mut aux, spec.matrix_aux.len(), MATRIX_DESCRIPTOR_RESOURCE)?;
    for descriptor in spec.matrix_aux {
        let (offset, byte_len) = *aux_offsets
            .get(descriptor.name)
            .ok_or_else(|| Error::MatrixAuxMissing(descriptor.name.to_string()))?;
        if byte_len != descriptor.byte_len {
            return Err(Error::InvalidMatrixLayout);
        }
        aux.push(MatrixAuxLayout {
            name: descriptor.name.to_string(),
            offset,
            byte_len,
        });
    }
    let layout = MatrixLayout {
        dimensions,
        commits,
        blocks,
        aux,
        crc_findings,
        append_log_start,
        read_limits: spec.read_limits,
        resident_bitmap_bytes,
        fatal_access_blocked,
    };
    record_open_resident_bitmap_bytes(&layout);
    Ok(layout)
}

fn matrix_crc_enabled(spec: FormatSpec) -> Result<bool> {
    match spec.integrity_policy {
        IntegrityPolicy::None => Ok(false),
        IntegrityPolicy::Crc32 | IntegrityPolicy::Crc32WithHeader => {
            #[cfg(feature = "integrity")]
            {
                Ok(true)
            }
            #[cfg(not(feature = "integrity"))]
            {
                Err(Error::IntegrityFeatureDisabled)
            }
        }
    }
}

fn validate_crc_presence(enabled: bool, header: &MatrixHeaderFields) -> Result<()> {
    match (
        enabled,
        header.region_crc_off == 0 && header.region_crc_len == 0,
    ) {
        (true, true) | (false, false) => Err(Error::InvalidMatrixLayout),
        _ => Ok(()),
    }
}

// Region layout (MCRC v2): header, then one page-digest array per commit
// category, then the per-cell checksum array and validity bitmap of each block.
fn crc_table_len(
    spec: FormatSpec,
    commit_plans: &[CommitPlan],
    cell_counts: &HashMap<String, u64>,
) -> Result<u64> {
    let mut len = MCRC_HEADER_LEN;
    for (_, _, bit_count) in commit_plans {
        len = len
            .checked_add(page_digest_len(bit_bytes(*bit_count)?)?)
            .ok_or(Error::InvalidMatrixLayout)?;
    }
    for block in spec.matrix_blocks {
        let cell_count = *cell_counts
            .get(block.category)
            .ok_or_else(|| Error::MatrixCommitMissing(block.category.to_string()))?;
        len = len
            .checked_add(
                cell_count
                    .checked_mul(CRC_LEN)
                    .ok_or(Error::InvalidMatrixLayout)?,
            )
            .ok_or(Error::InvalidMatrixLayout)?;
        len = len
            .checked_add(bit_bytes(cell_count)?)
            .ok_or(Error::InvalidMatrixLayout)?;
    }
    Ok(len)
}

fn crc_layout_from_parts(
    region_crc_off: u64,
    region_crc_len: u64,
    spec: FormatSpec,
    commit_plans: &[CommitPlan],
    block_offsets: &HashMap<u32, (u64, u64, u64)>,
) -> Result<Option<MatrixCrcLayout>> {
    if region_crc_off == 0 && region_crc_len == 0 {
        return Ok(None);
    }
    if region_crc_off == 0 || region_crc_len < MCRC_HEADER_LEN {
        return Err(Error::InvalidMatrixLayout);
    }
    let mut cursor = region_crc_off
        .checked_add(MCRC_HEADER_LEN)
        .ok_or(Error::InvalidMatrixLayout)?;
    let mut commit_digest_offsets = Vec::new();
    try_reserve_vec(
        &mut commit_digest_offsets,
        commit_plans.len(),
        MATRIX_DESCRIPTOR_RESOURCE,
    )?;
    for (_, _, bit_count) in commit_plans {
        commit_digest_offsets.push(cursor);
        cursor = cursor
            .checked_add(page_digest_len(bit_bytes(*bit_count)?)?)
            .ok_or(Error::InvalidMatrixLayout)?;
    }
    let mut block_crc_offsets = Vec::new();
    try_reserve_vec(
        &mut block_crc_offsets,
        spec.matrix_blocks.len(),
        MATRIX_DESCRIPTOR_RESOURCE,
    )?;
    for block in spec.matrix_blocks {
        let (_, _, cell_count) = *block_offsets
            .get(&block.block_id)
            .ok_or(Error::MatrixBlockMissing(block.block_id))?;
        block_crc_offsets.push(cursor);
        cursor = cursor
            .checked_add(
                cell_count
                    .checked_mul(CRC_LEN)
                    .ok_or(Error::InvalidMatrixLayout)?,
            )
            .ok_or(Error::InvalidMatrixLayout)?;
    }
    let mut block_valid_offsets = Vec::new();
    try_reserve_vec(
        &mut block_valid_offsets,
        spec.matrix_blocks.len(),
        MATRIX_DESCRIPTOR_RESOURCE,
    )?;
    for block in spec.matrix_blocks {
        let (_, _, cell_count) = *block_offsets
            .get(&block.block_id)
            .ok_or(Error::MatrixBlockMissing(block.block_id))?;
        block_valid_offsets.push(cursor);
        cursor = cursor
            .checked_add(bit_bytes(cell_count)?)
            .ok_or(Error::InvalidMatrixLayout)?;
    }
    let expected_len = cursor
        .checked_sub(region_crc_off)
        .ok_or(Error::InvalidMatrixLayout)?;
    if region_crc_len != expected_len {
        return Err(Error::InvalidMatrixLayout);
    }
    Ok(Some(MatrixCrcLayout {
        region_offset: region_crc_off,
        commit_digest_offsets,
        block_crc_offsets,
        block_valid_offsets,
    }))
}

fn write_crc_header(
    file: &mut File,
    dimension_table: &[u8],
    block_table: &[u8],
    category_table: &[u8],
) -> Result<()> {
    let mut header = [0u8; MCRC_HEADER_LEN as usize];
    header[0..4].copy_from_slice(MCRC_MAGIC);
    header[4..6].copy_from_slice(&MCRC_VERSION.to_le_bytes());
    header[8..12].copy_from_slice(
        &crc32_segments(&[dimension_table, block_table, category_table])?.to_le_bytes(),
    );
    file.write_all(&header)?;
    Ok(())
}

fn verify_crc_header(
    file: &mut File,
    crc: Option<&MatrixCrcLayout>,
    metadata_segments: &[&[u8]],
    commit_count: usize,
) -> Result<MatrixCrcVerification> {
    let Some(crc) = crc else {
        return Ok(MatrixCrcVerification::default());
    };
    file.seek(SeekFrom::Start(crc.region_offset))?;
    let mut header = [0u8; MCRC_HEADER_LEN as usize];
    file.read_exact(&mut header)?;
    if &header[0..4] != MCRC_MAGIC || header[6..8] != [0; 2] || header[12..16] != [0; 4] {
        return Err(Error::InvalidMatrixLayout);
    }
    let version = u16::from_le_bytes(header[4..6].try_into().expect("slice"));
    if version != MCRC_VERSION {
        return Err(Error::FormatVersionMismatch {
            expected: MCRC_VERSION,
            actual: version,
        });
    }

    let mut verification = MatrixCrcVerification::default();
    try_reserve_map(
        &mut verification.commit_findings,
        commit_count,
        MATRIX_DESCRIPTOR_RESOURCE,
    )?;
    let stored_metadata = u32::from_le_bytes(header[8..12].try_into().expect("slice"));
    let actual_metadata = crc32_segments(metadata_segments)?;
    if stored_metadata != actual_metadata {
        verification.findings.push(MatrixRecoveryFinding {
            kind: MatrixCorruptionKind::Header,
            severity: MatrixCorruptionSeverity::Fatal,
            message: format!(
                "matrix metadata crc mismatch: expected {stored_metadata:#010x}, got {actual_metadata:#010x}"
            ),
        });
    }
    Ok(verification)
}

/// Streams a bitmap region one page at a time, materialising only the pages
/// that carry a set bit.
///
/// With `digest_base`, each page is authenticated against its stored digest:
/// `PAGE_STATE_UNINITIALIZED` asserts the page was never published and must
/// still read as zero, while `PAGE_STATE_INITIALIZED` asserts the recorded
/// checksum. The two states are distinct on disk, so a page that was written
/// with zeros is never confused with one that was never written. Without a
/// digest base the region is unauthenticated, exactly as before this format
/// version, and the load only decides residency.
///
/// PERF-02: a page is skipped without any I/O when the filesystem proves that
/// neither the page nor its digest slot has ever been written. Such a page is
/// `PAGE_STATE_UNINITIALIZED` holding zeros, which is precisely what reading it
/// would have established, so skipping changes neither residency nor the set of
/// reported findings — a stray byte written into an untouched page allocates it
/// and is therefore still read and still reported.
fn load_paged_bitmap(
    file: &mut File,
    base_offset: u64,
    bit_count: u64,
    digest_base: Option<u64>,
    extents: Option<&AllocatedExtents>,
    budget: &mut ResidentBitmapBudget,
    resource: &'static str,
) -> Result<(SparseBitmap, bool)> {
    let mut bits = SparseBitmap::new(bit_count)?;
    let mut intact = true;
    for page in 0..bits.page_count {
        let len = bits.page_len(page)?;
        let offset = page
            .checked_mul(BITMAP_PAGE_BYTES)
            .and_then(|delta| base_offset.checked_add(delta))
            .ok_or(Error::InvalidMatrixLayout)?;
        let digest_offset = digest_base
            .map(|base| page_digest_offset(base, page))
            .transpose()?;
        if !range_may_hold_data(extents, offset, len) {
            // The page is a hole, so it reads as zero without being read. Its
            // digest still has to agree, because a digest recorded for a page
            // whose bytes never reached the disk is a torn commit and must stay
            // detectable. Where the digest slot is itself a hole the page costs
            // no I/O at all.
            let Some(digest_offset) = digest_offset else {
                continue;
            };
            if !range_may_hold_data(extents, digest_offset, PAGE_DIGEST_LEN) {
                continue;
            }
            let zeros =
                &ZERO_PAGE[..usize::try_from(len).map_err(|_| Error::InvalidMatrixLayout)?];
            let (stored, state) = read_page_digest(file, digest_offset)?;
            count_open_bitmap_bytes_read(PAGE_DIGEST_LEN);
            intact &= match state {
                PAGE_STATE_UNINITIALIZED => stored == 0,
                PAGE_STATE_INITIALIZED => crc32_bytes(zeros)? == stored,
                _ => false,
            };
            continue;
        }
        let bytes = read_range(file, offset, len, resource)?;
        count_open_bitmap_bytes_read(len);
        if let Some(digest_offset) = digest_offset {
            let (stored, state) = read_page_digest(file, digest_offset)?;
            count_open_bitmap_bytes_read(PAGE_DIGEST_LEN);
            let page_ok = match state {
                PAGE_STATE_UNINITIALIZED => stored == 0 && bytes.iter().all(|byte| *byte == 0),
                PAGE_STATE_INITIALIZED => crc32_bytes(&bytes)? == stored,
                _ => false,
            };
            intact &= page_ok;
        }
        if bytes.iter().any(|byte| *byte != 0) {
            budget.charge(len)?;
        }
        bits.insert_loaded_page(page, bytes)?;
    }
    Ok((bits, intact))
}

fn load_commit_bitmaps(
    file: &mut File,
    crc: Option<&MatrixCrcLayout>,
    commits: &[StoredCommitPlan],
    extents: Option<&AllocatedExtents>,
    budget: &mut ResidentBitmapBudget,
    verification: &mut MatrixCrcVerification,
) -> Result<Vec<SparseBitmap>> {
    let mut bitmaps = Vec::new();
    try_reserve_vec(
        &mut bitmaps,
        commits.len(),
        ReadLimitKey::MatrixBitmapBytes.resource(),
    )?;
    for (index, (name, _, bit_count, map_offset, map_len)) in commits.iter().enumerate() {
        if bit_bytes(*bit_count)? != *map_len {
            return Err(Error::InvalidMatrixLayout);
        }
        let digest_base = crc
            .map(|crc| {
                crc.commit_digest_offsets
                    .get(index)
                    .copied()
                    .ok_or(Error::InvalidMatrixLayout)
            })
            .transpose()?;
        let (bits, intact) = load_paged_bitmap(
            file,
            *map_offset,
            *bit_count,
            digest_base,
            extents,
            budget,
            ReadLimitKey::MatrixBitmapBytes.resource(),
        )?;
        if !intact {
            verification.commit_findings.insert(
                name.clone(),
                MatrixRecoveryFinding {
                    kind: MatrixCorruptionKind::CommitMap,
                    severity: MatrixCorruptionSeverity::Recoverable,
                    message: format!("matrix commit map page crc mismatch for {name}"),
                },
            );
        }
        bitmaps.push(bits);
    }
    Ok(bitmaps)
}

fn load_crc_valid_bits(
    spec: FormatSpec,
    file: &mut File,
    crc: Option<&MatrixCrcLayout>,
    block_offsets: &HashMap<u32, (u64, u64, u64)>,
    extents: Option<&AllocatedExtents>,
    budget: &mut ResidentBitmapBudget,
) -> Result<HashMap<u32, SparseBitmap>> {
    let Some(crc) = crc else {
        return Ok(HashMap::new());
    };
    let mut valid_bits = HashMap::new();
    try_reserve_map(
        &mut valid_bits,
        spec.matrix_blocks.len(),
        ReadLimitKey::MatrixCrcBytes.resource(),
    )?;
    for (block_index, block) in spec.matrix_blocks.iter().enumerate() {
        let (_, _, cell_count) = *block_offsets
            .get(&block.block_id)
            .ok_or(Error::MatrixBlockMissing(block.block_id))?;
        let offset = crc
            .block_valid_offsets
            .get(block_index)
            .copied()
            .ok_or(Error::InvalidMatrixLayout)?;
        let (bits, _) = load_paged_bitmap(
            file,
            offset,
            cell_count,
            None,
            extents,
            budget,
            ReadLimitKey::MatrixCrcBytes.resource(),
        )?;
        valid_bits.insert(block.block_id, bits);
    }
    Ok(valid_bits)
}

struct MatrixHeaderFields {
    dimension_count: u32,
    matrix_block_count: u32,
    commit_category_count: u32,
    dimension_table_off: u64,
    dimension_table_len: u64,
    block_table_off: u64,
    block_table_len: u64,
    commit_category_off: u64,
    commit_category_len: u64,
    commit_map_off: u64,
    commit_map_len: u64,
    slot_region_off: u64,
    slot_region_len: u64,
    region_crc_off: u64,
    region_crc_len: u64,
    append_log_start: u64,
}

fn encode_header(fields: MatrixHeaderFields) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(VMAT_HEADER_LEN as usize);
    bytes.extend_from_slice(VMAT_MAGIC);
    bytes.extend_from_slice(&VMAT_VERSION.to_le_bytes());
    bytes.extend_from_slice(&0u16.to_le_bytes());
    bytes.extend_from_slice(&VMAT_HEADER_LEN.to_le_bytes());
    bytes.extend_from_slice(&fields.dimension_count.to_le_bytes());
    bytes.extend_from_slice(&fields.matrix_block_count.to_le_bytes());
    bytes.extend_from_slice(&fields.commit_category_count.to_le_bytes());
    for value in [
        fields.dimension_table_off,
        fields.dimension_table_len,
        fields.block_table_off,
        fields.block_table_len,
        fields.commit_category_off,
        fields.commit_category_len,
        fields.commit_map_off,
        fields.commit_map_len,
        fields.slot_region_off,
        fields.slot_region_len,
        fields.region_crc_off,
        fields.region_crc_len,
        fields.append_log_start,
    ] {
        bytes.extend_from_slice(&value.to_le_bytes());
    }
    bytes.extend_from_slice(&[0; 32]);
    debug_assert_eq!(bytes.len(), VMAT_HEADER_LEN as usize);
    bytes
}

fn read_header(file: &mut File, header_len: u64) -> Result<MatrixHeaderFields> {
    file.seek(SeekFrom::Start(header_len))?;
    let mut bytes = [0; VMAT_HEADER_LEN as usize];
    file.read_exact(&mut bytes)?;
    if &bytes[0..4] != VMAT_MAGIC {
        return Err(Error::InvalidMatrixLayout);
    }
    let version = u16::from_le_bytes(bytes[4..6].try_into().expect("slice"));
    // A version-1 artifact describes the pre-paging physical representation and
    // is stale-regenerable, not readable under version 2.
    if version != VMAT_VERSION {
        return Err(Error::FormatVersionMismatch {
            expected: VMAT_VERSION,
            actual: version,
        });
    }
    let header_len = u32::from_le_bytes(bytes[8..12].try_into().expect("slice"));
    if header_len != VMAT_HEADER_LEN || bytes[6..8] != [0; 2] || bytes[128..160] != [0; 32] {
        return Err(Error::InvalidMatrixLayout);
    }
    let mut pos = 12;
    let read_u32 = |bytes: &[u8], pos: &mut usize| {
        let value = u32::from_le_bytes(bytes[*pos..*pos + 4].try_into().expect("slice"));
        *pos += 4;
        value
    };
    let read_u64 = |bytes: &[u8], pos: &mut usize| {
        let value = u64::from_le_bytes(bytes[*pos..*pos + 8].try_into().expect("slice"));
        *pos += 8;
        value
    };
    let dimension_count = read_u32(&bytes, &mut pos);
    let matrix_block_count = read_u32(&bytes, &mut pos);
    let commit_category_count = read_u32(&bytes, &mut pos);
    Ok(MatrixHeaderFields {
        dimension_count,
        matrix_block_count,
        commit_category_count,
        dimension_table_off: read_u64(&bytes, &mut pos),
        dimension_table_len: read_u64(&bytes, &mut pos),
        block_table_off: read_u64(&bytes, &mut pos),
        block_table_len: read_u64(&bytes, &mut pos),
        commit_category_off: read_u64(&bytes, &mut pos),
        commit_category_len: read_u64(&bytes, &mut pos),
        commit_map_off: read_u64(&bytes, &mut pos),
        commit_map_len: read_u64(&bytes, &mut pos),
        slot_region_off: read_u64(&bytes, &mut pos),
        slot_region_len: read_u64(&bytes, &mut pos),
        region_crc_off: read_u64(&bytes, &mut pos),
        region_crc_len: read_u64(&bytes, &mut pos),
        append_log_start: read_u64(&bytes, &mut pos),
    })
}

fn encode_dimension_table(
    dimensions: &[MatrixDimensionValue],
    expected_len: u64,
) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    try_reserve_bytes(&mut bytes, expected_len, MATRIX_DESCRIPTOR_RESOURCE)?;
    for dimension in dimensions {
        write_name(&mut bytes, &dimension.name)?;
        bytes.extend_from_slice(&dimension.value.to_le_bytes());
    }
    if usize_to_u64(bytes.len())? != expected_len {
        return Err(Error::InvalidMatrixLayout);
    }
    Ok(bytes)
}

fn decode_dimension_table(bytes: &[u8], count: u32) -> Result<Vec<MatrixDimensionValue>> {
    let mut cursor = Cursor::new(bytes);
    let mut dimensions = Vec::new();
    try_reserve_vec(
        &mut dimensions,
        usize::try_from(count).map_err(|_| Error::InvalidMatrixLayout)?,
        MATRIX_DESCRIPTOR_RESOURCE,
    )?;
    for _ in 0..count {
        let name = cursor.read_name()?;
        let value = cursor.read_u64()?;
        dimensions.push(MatrixDimensionValue { name, value });
    }
    if cursor.remaining() != 0 {
        return Err(Error::InvalidMatrixLayout);
    }
    Ok(dimensions)
}

fn encode_block_table(
    spec: FormatSpec,
    dimension_index: &HashMap<String, u16>,
    commit_plans: &[CommitPlan],
    block_offsets: &HashMap<u32, (u64, u64, u64)>,
) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    let expected_len = usize_to_u64(spec.matrix_blocks.len())?
        .checked_mul(44)
        .ok_or(Error::ResourceArithmeticOverflow {
            resource: MATRIX_DESCRIPTOR_RESOURCE,
        })?;
    try_reserve_bytes(&mut bytes, expected_len, MATRIX_DESCRIPTOR_RESOURCE)?;
    for block in spec.matrix_blocks {
        let descriptor = spec
            .block(block.block_id)
            .ok_or(Error::MatrixBlockMissing(block.block_id))?;
        let dim0 = *dimension_index
            .get(block.dimensions[0])
            .ok_or_else(|| Error::MatrixDimensionMissing(block.dimensions[0].to_string()))?;
        let dim1 = *dimension_index
            .get(block.dimensions[1])
            .ok_or_else(|| Error::MatrixDimensionMissing(block.dimensions[1].to_string()))?;
        let category = commit_plans
            .iter()
            .position(|(name, _, _)| name == block.category)
            .ok_or_else(|| Error::MatrixCommitMissing(block.category.to_string()))?;
        let category = u16::try_from(category).map_err(|_| Error::InvalidMatrixLayout)?;
        let (slot_region_off, slot_region_len, cell_count) = *block_offsets
            .get(&block.block_id)
            .ok_or(Error::MatrixBlockMissing(block.block_id))?;
        bytes.extend_from_slice(&block.block_id.to_le_bytes());
        bytes.extend_from_slice(&descriptor.version.to_le_bytes());
        bytes.extend_from_slice(&dim0.to_le_bytes());
        bytes.extend_from_slice(&dim1.to_le_bytes());
        bytes.extend_from_slice(&category.to_le_bytes());
        bytes.extend_from_slice(&block.slot_stride.to_le_bytes());
        bytes.extend_from_slice(&cell_count.to_le_bytes());
        bytes.extend_from_slice(&slot_region_off.to_le_bytes());
        bytes.extend_from_slice(&slot_region_len.to_le_bytes());
    }
    if usize_to_u64(bytes.len())? != expected_len {
        return Err(Error::InvalidMatrixLayout);
    }
    Ok(bytes)
}

fn decode_block_table(
    spec: FormatSpec,
    dimensions: &[MatrixDimensionValue],
    commits: &[StoredCommitPlan],
    cell_counts: &HashMap<String, u64>,
    bytes: &[u8],
    count: u32,
    slot_region: (u64, u64),
) -> Result<HashMap<u32, (u64, u64, u64)>> {
    const ENTRY_LEN: usize = 44;
    let count_usize = usize::try_from(count).map_err(|_| Error::InvalidMatrixLayout)?;
    let expected_len = count_usize
        .checked_mul(ENTRY_LEN)
        .ok_or(Error::InvalidMatrixLayout)?;
    if bytes.len() != expected_len || count_usize != spec.matrix_blocks.len() {
        return Err(Error::InvalidMatrixLayout);
    }
    let mut offsets = HashMap::new();
    try_reserve_map(&mut offsets, count_usize, MATRIX_DESCRIPTOR_RESOURCE)?;
    let mut cursor = Cursor::new(bytes);
    let (slot_region_base, slot_region_len) = slot_region;
    let mut next_slot_offset = slot_region_base;
    for expected in spec.matrix_blocks {
        let block_id = cursor.read_u32()?;
        let block_version = cursor.read_u16()?;
        let dim0 = usize::from(cursor.read_u16()?);
        let dim1 = usize::from(cursor.read_u16()?);
        let category = usize::from(cursor.read_u16()?);
        let slot_stride = cursor.read_u64()?;
        let cell_count = cursor.read_u64()?;
        let slot_region_off = cursor.read_u64()?;
        let slot_region_len = cursor.read_u64()?;
        if block_id != expected.block_id
            || block_version
                != spec
                    .block(expected.block_id)
                    .ok_or(Error::MatrixBlockMissing(expected.block_id))?
                    .version
            || slot_stride != expected.slot_stride
        {
            return Err(Error::InvalidMatrixLayout);
        }
        let dim0_name = dimensions
            .get(dim0)
            .ok_or(Error::InvalidMatrixLayout)?
            .name
            .as_str();
        let dim1_name = dimensions
            .get(dim1)
            .ok_or(Error::InvalidMatrixLayout)?
            .name
            .as_str();
        let category_name = commits
            .get(category)
            .ok_or(Error::InvalidMatrixLayout)?
            .0
            .as_str();
        if expected.dimensions != [dim0_name, dim1_name] || expected.category != category_name {
            return Err(Error::InvalidMatrixLayout);
        }
        let expected_cell_count = *cell_counts
            .get(expected.category)
            .ok_or_else(|| Error::MatrixCommitMissing(expected.category.to_string()))?;
        let expected_slot_len = expected_cell_count
            .checked_mul(expected.slot_stride)
            .ok_or(Error::InvalidMatrixLayout)?;
        if cell_count != expected_cell_count
            || slot_region_off != next_slot_offset
            || slot_region_len != expected_slot_len
        {
            return Err(Error::InvalidMatrixLayout);
        }
        next_slot_offset = next_slot_offset
            .checked_add(slot_region_len)
            .ok_or(Error::InvalidMatrixLayout)?;
        if offsets
            .insert(block_id, (slot_region_off, slot_region_len, cell_count))
            .is_some()
        {
            return Err(Error::InvalidMatrixLayout);
        }
    }
    let slot_region_end = slot_region_base
        .checked_add(slot_region_len)
        .ok_or(Error::InvalidMatrixLayout)?;
    if next_slot_offset != slot_region_end {
        return Err(Error::InvalidMatrixLayout);
    }
    Ok(offsets)
}

fn encode_category_table(
    commits: &[CommitPlan],
    offsets: &HashMap<String, (u64, u64)>,
) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    let expected_len = commits.iter().try_fold(0u64, |len, (name, _, _)| {
        len.checked_add(usize_to_u64(name.len())?.checked_add(30).ok_or(
            Error::ResourceArithmeticOverflow {
                resource: MATRIX_DESCRIPTOR_RESOURCE,
            },
        )?)
        .ok_or(Error::ResourceArithmeticOverflow {
            resource: MATRIX_DESCRIPTOR_RESOURCE,
        })
    })?;
    try_reserve_bytes(&mut bytes, expected_len, MATRIX_DESCRIPTOR_RESOURCE)?;
    for (name, kind, bit_count) in commits {
        let (map_off, map_len) = *offsets
            .get(name)
            .ok_or_else(|| Error::MatrixCommitMissing(name.clone()))?;
        write_name(&mut bytes, name)?;
        bytes.push(commit_kind_byte(*kind));
        bytes.extend_from_slice(&[0; 3]);
        bytes.extend_from_slice(&bit_count.to_le_bytes());
        bytes.extend_from_slice(&map_off.to_le_bytes());
        bytes.extend_from_slice(&map_len.to_le_bytes());
    }
    if usize_to_u64(bytes.len())? != expected_len {
        return Err(Error::InvalidMatrixLayout);
    }
    Ok(bytes)
}

fn decode_category_table(bytes: &[u8], count: u32) -> Result<Vec<StoredCommitPlan>> {
    let mut cursor = Cursor::new(bytes);
    let mut commits = Vec::new();
    try_reserve_vec(
        &mut commits,
        usize::try_from(count).map_err(|_| Error::InvalidMatrixLayout)?,
        MATRIX_DESCRIPTOR_RESOURCE,
    )?;
    for _ in 0..count {
        let name = cursor.read_name()?;
        let kind = commit_kind_from_byte(cursor.read_u8()?)?;
        if cursor.read_exact(3)? != [0; 3] {
            return Err(Error::InvalidMatrixLayout);
        }
        let bit_count = cursor.read_u64()?;
        let map_off = cursor.read_u64()?;
        let map_len = cursor.read_u64()?;
        commits.push((name, kind, bit_count, map_off, map_len));
    }
    if cursor.remaining() != 0 {
        return Err(Error::InvalidMatrixLayout);
    }
    Ok(commits)
}

fn validate_dimension_names(spec: FormatSpec, dimensions: &[MatrixDimensionValue]) -> Result<()> {
    if dimensions.len() != spec.matrix_dimensions.len() {
        return Err(Error::InvalidMatrixLayout);
    }
    for (actual, expected) in dimensions.iter().zip(spec.matrix_dimensions) {
        if actual.name != expected.name {
            return Err(Error::InvalidMatrixLayout);
        }
    }
    Ok(())
}

fn validate_header_descriptor_shape(spec: FormatSpec, header: &MatrixHeaderFields) -> Result<()> {
    let (dimension_table_len, block_table_len, category_table_len) =
        matrix_descriptor_table_lengths(spec)?;
    if header.dimension_count
        != u32::try_from(spec.matrix_dimensions.len()).map_err(|_| Error::InvalidMatrixLayout)?
        || header.matrix_block_count
            != u32::try_from(spec.matrix_blocks.len()).map_err(|_| Error::InvalidMatrixLayout)?
        || header.commit_category_count
            != u32::try_from(spec.matrix_commits.len()).map_err(|_| Error::InvalidMatrixLayout)?
        || header.dimension_table_len != dimension_table_len
        || header.block_table_len != block_table_len
        || header.commit_category_len != category_table_len
    {
        return Err(Error::InvalidMatrixLayout);
    }
    Ok(())
}

fn validate_dimension_derived_lengths(
    spec: FormatSpec,
    crc_enabled: bool,
    header: &MatrixHeaderFields,
    commit_plans: &[CommitPlan],
    cell_counts: &HashMap<String, u64>,
) -> Result<()> {
    let expected_crc_len = if crc_enabled {
        crc_table_len(spec, commit_plans, cell_counts)?
    } else {
        0
    };
    if header.commit_map_len != matrix_commit_map_len(commit_plans)?
        || header.slot_region_len != matrix_slot_region_len(spec, cell_counts)?
        || header.region_crc_len != expected_crc_len
    {
        return Err(Error::InvalidMatrixLayout);
    }
    Ok(())
}

fn validate_commit_table(
    expected: &[CommitPlan],
    stored: &[StoredCommitPlan],
    commit_map_base: u64,
    commit_map_len: u64,
) -> Result<HashMap<String, (u64, u64)>> {
    if stored.len() != expected.len() {
        return Err(Error::InvalidMatrixLayout);
    }
    let mut offsets = HashMap::new();
    try_reserve_map(&mut offsets, expected.len(), MATRIX_DESCRIPTOR_RESOURCE)?;
    let mut next_offset = commit_map_base;
    for ((expected_name, expected_kind, expected_bits), actual) in expected.iter().zip(stored) {
        let expected_map_len = bit_bytes(*expected_bits)?;
        if actual.0 != *expected_name
            || actual.1 != *expected_kind
            || actual.2 != *expected_bits
            || actual.3 != next_offset
            || actual.4 != expected_map_len
        {
            return Err(Error::InvalidMatrixLayout);
        }
        if offsets
            .insert(expected_name.clone(), (actual.3, actual.4))
            .is_some()
        {
            return Err(Error::InvalidMatrixLayout);
        }
        next_offset = next_offset
            .checked_add(expected_map_len)
            .ok_or(Error::InvalidMatrixLayout)?;
    }
    let commit_map_end = commit_map_base
        .checked_add(commit_map_len)
        .ok_or(Error::InvalidMatrixLayout)?;
    if next_offset != commit_map_end {
        return Err(Error::InvalidMatrixLayout);
    }
    Ok(offsets)
}

fn validate_layout_ranges(
    header_len: u64,
    file_len: u64,
    header: &MatrixHeaderFields,
    aux_region_len: u64,
) -> Result<()> {
    let vmat_end = header_len
        .checked_add(u64::from(VMAT_HEADER_LEN))
        .ok_or(Error::InvalidMatrixLayout)?;
    let dimension_end = validate_range(
        header.dimension_table_off,
        header.dimension_table_len,
        file_len,
    )?;
    let block_end = validate_range(header.block_table_off, header.block_table_len, file_len)?;
    let category_end = validate_range(
        header.commit_category_off,
        header.commit_category_len,
        file_len,
    )?;
    let commit_end = validate_range(header.commit_map_off, header.commit_map_len, file_len)?;
    let slot_end = validate_range(header.slot_region_off, header.slot_region_len, file_len)?;
    let aux_end = validate_range(slot_end, aux_region_len, file_len)?;
    let crc_end = if header.region_crc_off == 0 && header.region_crc_len == 0 {
        aux_end
    } else {
        let crc_end = validate_range(header.region_crc_off, header.region_crc_len, file_len)?;
        if header.region_crc_off != aux_end {
            return Err(Error::InvalidMatrixLayout);
        }
        crc_end
    };
    if header.dimension_table_off != vmat_end
        || header.block_table_off != dimension_end
        || header.commit_category_off != block_end
        || header.commit_map_off != category_end
        || header.slot_region_off != commit_end
        || header.append_log_start != crc_end
        || header.append_log_start > file_len
    {
        return Err(Error::InvalidMatrixLayout);
    }
    Ok(())
}

fn validate_range(offset: u64, len: u64, file_len: u64) -> Result<u64> {
    let end = offset.checked_add(len).ok_or(Error::InvalidMatrixLayout)?;
    if end > file_len {
        return Err(Error::InvalidMatrixLayout);
    }
    Ok(end)
}

fn read_range(file: &mut File, offset: u64, len: u64, resource: &'static str) -> Result<Vec<u8>> {
    file.seek(SeekFrom::Start(offset))?;
    let mut bytes = filled_bytes_for(len, 0, resource)?;
    file.read_exact(&mut bytes)?;
    Ok(bytes)
}

fn filled_bytes(len: u64, value: u8) -> Result<Vec<u8>> {
    filled_bytes_for(len, value, MATRIX_BYTES_RESOURCE)
}

fn filled_bytes_for(len: u64, value: u8, resource: &'static str) -> Result<Vec<u8>> {
    let len_usize = usize::try_from(len).map_err(|_| Error::LengthOverflow { value: len })?;
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(len_usize)
        .map_err(|_| Error::AllocationFailed {
            resource,
            requested: len,
        })?;
    bytes.resize(len_usize, value);
    Ok(bytes)
}

fn try_reserve_bytes(bytes: &mut Vec<u8>, requested: u64, resource: &'static str) -> Result<()> {
    let requested_usize =
        usize::try_from(requested).map_err(|_| Error::LengthOverflow { value: requested })?;
    bytes
        .try_reserve_exact(requested_usize)
        .map_err(|_| Error::AllocationFailed {
            resource,
            requested,
        })
}

fn try_reserve_vec<T>(
    values: &mut Vec<T>,
    additional: usize,
    resource: &'static str,
) -> Result<()> {
    let requested = additional
        .checked_mul(std::mem::size_of::<T>().max(1))
        .and_then(|bytes| u64::try_from(bytes).ok())
        .ok_or(Error::ResourceArithmeticOverflow { resource })?;
    values
        .try_reserve_exact(additional)
        .map_err(|_| Error::AllocationFailed {
            resource,
            requested,
        })
}

fn try_reserve_map<K: Eq + std::hash::Hash, V>(
    values: &mut HashMap<K, V>,
    additional: usize,
    resource: &'static str,
) -> Result<()> {
    let requested = additional
        .checked_mul(std::mem::size_of::<(K, V)>().max(1))
        .and_then(|bytes| u64::try_from(bytes).ok())
        .ok_or(Error::ResourceArithmeticOverflow { resource })?;
    values
        .try_reserve(additional)
        .map_err(|_| Error::AllocationFailed {
            resource,
            requested,
        })
}

fn usize_to_u64(value: usize) -> Result<u64> {
    u64::try_from(value).map_err(|_| Error::InvalidMatrixLayout)
}

fn read_crc_at(file: &mut File, offset: u64) -> Result<u32> {
    file.seek(SeekFrom::Start(offset))?;
    let mut bytes = [0; 4];
    file.read_exact(&mut bytes)?;
    Ok(u32::from_le_bytes(bytes))
}

fn write_crc_at(file: &mut File, offset: u64, crc: u32) -> Result<()> {
    file.seek(SeekFrom::Start(offset))?;
    file.write_all(&crc.to_le_bytes())?;
    Ok(())
}

fn indexed_crc_offset(crc_offset: u64, ordinal: u64) -> Result<u64> {
    ordinal
        .checked_mul(CRC_LEN)
        .and_then(|delta| crc_offset.checked_add(delta))
        .ok_or(Error::InvalidMatrixLayout)
}

#[cfg(feature = "integrity")]
fn crc32_bytes(bytes: &[u8]) -> Result<u32> {
    Ok(crc32fast::hash(bytes))
}

#[cfg(not(feature = "integrity"))]
fn crc32_bytes(_bytes: &[u8]) -> Result<u32> {
    Err(Error::IntegrityFeatureDisabled)
}

#[cfg(feature = "integrity")]
fn crc32_bytes_with_replacement(bytes: &[u8], index: usize, value: u8) -> Result<u32> {
    let suffix = index.checked_add(1).ok_or(Error::InvalidMatrixLayout)?;
    if suffix > bytes.len() {
        return Err(Error::InvalidMatrixLayout);
    }
    let mut hasher = crc32fast::Hasher::new();
    hasher.update(&bytes[..index]);
    hasher.update(&[value]);
    hasher.update(&bytes[suffix..]);
    Ok(hasher.finalize())
}

#[cfg(not(feature = "integrity"))]
fn crc32_bytes_with_replacement(_bytes: &[u8], _index: usize, _value: u8) -> Result<u32> {
    Err(Error::IntegrityFeatureDisabled)
}

#[cfg(feature = "integrity")]
fn crc32_segments(segments: &[&[u8]]) -> Result<u32> {
    let mut hasher = crc32fast::Hasher::new();
    for segment in segments {
        hasher.update(segment);
    }
    Ok(hasher.finalize())
}

#[cfg(not(feature = "integrity"))]
fn crc32_segments(_segments: &[&[u8]]) -> Result<u32> {
    Err(Error::IntegrityFeatureDisabled)
}

#[cfg(feature = "integrity")]
fn crc32_zeroes(len: u64) -> Result<u32> {
    const ZERO_CHUNK: [u8; 8192] = [0; 8192];
    let mut hasher = crc32fast::Hasher::new();
    let mut remaining = len;
    while remaining > 0 {
        let chunk = remaining.min(ZERO_CHUNK.len() as u64);
        hasher.update(&ZERO_CHUNK[..chunk as usize]);
        remaining -= chunk;
    }
    Ok(hasher.finalize())
}

#[cfg(not(feature = "integrity"))]
fn crc32_zeroes(_len: u64) -> Result<u32> {
    Err(Error::IntegrityFeatureDisabled)
}

fn write_zeros(file: &mut File, len: u64) -> Result<()> {
    const ZERO_CHUNK: [u8; 8192] = [0; 8192];
    let mut remaining = len;
    while remaining > 0 {
        let chunk = remaining.min(ZERO_CHUNK.len() as u64);
        file.write_all(&ZERO_CHUNK[..chunk as usize])?;
        remaining -= chunk;
    }
    Ok(())
}

fn bit_bytes(bit_count: u64) -> Result<u64> {
    bit_count
        .checked_add(7)
        .map(|value| value / 8)
        .ok_or(Error::InvalidMatrixLayout)
}

fn get_bit(bits: &[u8], ordinal: u64) -> Result<bool> {
    let byte = usize::try_from(ordinal / 8).map_err(|_| Error::InvalidMatrixLayout)?;
    let mask = 1u8 << (ordinal % 8);
    Ok(bits.get(byte).ok_or(Error::InvalidMatrixLayout)? & mask != 0)
}

fn set_bit(bits: &mut [u8], ordinal: u64, value: bool) -> Result<()> {
    let byte = usize::try_from(ordinal / 8).map_err(|_| Error::InvalidMatrixLayout)?;
    let mask = 1u8 << (ordinal % 8);
    let target = bits.get_mut(byte).ok_or(Error::InvalidMatrixLayout)?;
    if value {
        *target |= mask;
    } else {
        *target &= !mask;
    }
    Ok(())
}

fn write_name(bytes: &mut Vec<u8>, name: &str) -> Result<()> {
    let len = u16::try_from(name.len()).map_err(|_| Error::InvalidMatrixLayout)?;
    bytes.extend_from_slice(&len.to_le_bytes());
    bytes.extend_from_slice(name.as_bytes());
    Ok(())
}

fn commit_kind_byte(kind: MatrixCommitKind) -> u8 {
    match kind {
        MatrixCommitKind::Cell => 1,
        MatrixCommitKind::Single => 2,
        MatrixCommitKind::PerChannel => 3,
    }
}

fn commit_kind_from_byte(value: u8) -> Result<MatrixCommitKind> {
    match value {
        1 => Ok(MatrixCommitKind::Cell),
        2 => Ok(MatrixCommitKind::Single),
        3 => Ok(MatrixCommitKind::PerChannel),
        _ => Err(Error::InvalidMatrixLayout),
    }
}

struct Cursor<'a> {
    bytes: &'a [u8],
    offset: usize,
}

impl<'a> Cursor<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, offset: 0 }
    }

    fn remaining(&self) -> usize {
        self.bytes.len().saturating_sub(self.offset)
    }

    fn read_exact(&mut self, len: usize) -> Result<&'a [u8]> {
        let end = self
            .offset
            .checked_add(len)
            .ok_or(Error::InvalidMatrixLayout)?;
        let bytes = self
            .bytes
            .get(self.offset..end)
            .ok_or(Error::InvalidMatrixLayout)?;
        self.offset = end;
        Ok(bytes)
    }

    fn read_u8(&mut self) -> Result<u8> {
        Ok(self.read_exact(1)?[0])
    }

    fn read_u16(&mut self) -> Result<u16> {
        let mut bytes = [0; 2];
        bytes.copy_from_slice(self.read_exact(2)?);
        Ok(u16::from_le_bytes(bytes))
    }

    fn read_u32(&mut self) -> Result<u32> {
        let mut bytes = [0; 4];
        bytes.copy_from_slice(self.read_exact(4)?);
        Ok(u32::from_le_bytes(bytes))
    }

    fn read_u64(&mut self) -> Result<u64> {
        let mut bytes = [0; 8];
        bytes.copy_from_slice(self.read_exact(8)?);
        Ok(u64::from_le_bytes(bytes))
    }

    fn read_name(&mut self) -> Result<String> {
        let len = self.read_u16()? as usize;
        let bytes = self.read_exact(len)?;
        let mut owned = Vec::new();
        try_reserve_vec(&mut owned, len, MATRIX_DESCRIPTOR_RESOURCE)?;
        owned.extend_from_slice(bytes);
        String::from_utf8(owned).map_err(|_| Error::InvalidMatrixLayout)
    }
}

#[cfg(test)]
mod allocated_extents_tests {
    use super::*;

    fn extents(ranges: &[(u64, u64)]) -> AllocatedExtents {
        AllocatedExtents {
            ranges: ranges.to_vec(),
        }
    }

    #[test]
    fn overlap_is_exact_at_every_boundary() {
        let map = extents(&[(4096, 8192), (16384, 20480)]);
        // Fully inside a hole.
        assert!(!map.may_hold_data(0, 4096));
        assert!(!map.may_hold_data(8192, 8192));
        assert!(!map.may_hold_data(20480, 4096));
        // Touching, straddling, and contained by an allocated range.
        assert!(map.may_hold_data(4095, 2));
        assert!(map.may_hold_data(4096, 1));
        assert!(map.may_hold_data(8191, 1));
        assert!(map.may_hold_data(0, 8192));
        assert!(map.may_hold_data(16384, 4096));
        // Beyond every range.
        assert!(!map.may_hold_data(1 << 40, 4096));
        // A zero-length range reads nothing.
        assert!(!map.may_hold_data(4096, 0));
    }

    #[test]
    fn an_empty_map_proves_every_range_is_a_hole() {
        assert!(!extents(&[]).may_hold_data(0, u64::MAX));
    }

    #[test]
    fn a_length_that_overflows_is_treated_as_unproven() {
        assert!(extents(&[]).may_hold_data(u64::MAX, 2));
    }
}

#[cfg(test)]
mod sparse_bitmap_tests {
    use super::*;

    #[test]
    fn pages_materialize_only_when_they_carry_a_set_bit() {
        let bits = 3 * BITMAP_PAGE_BYTES * 8;
        let mut map = SparseBitmap::new(bits).expect("bitmap");
        assert_eq!(map.page_count, 3);
        assert_eq!(map.resident_bytes(), 0);

        // Clearing an already-clear bit must not fault a page in.
        map.set(0, false).expect("clear");
        assert_eq!(map.resident_bytes(), 0);

        map.set(bits - 1, true).expect("set");
        assert_eq!(map.resident_bytes(), BITMAP_PAGE_BYTES);
        assert_eq!(map.ones(), 1);
        assert!(map.get(bits - 1).expect("get"));
        assert!(!map.get(0).expect("get"));
    }

    #[test]
    fn set_bit_totals_track_every_transition() {
        let mut map = SparseBitmap::new(64).expect("bitmap");
        for ordinal in 0..64 {
            map.set(ordinal, true).expect("set");
        }
        assert_eq!(map.ones(), 64);
        for ordinal in 0..64 {
            map.set(ordinal, true).expect("idempotent set");
        }
        assert_eq!(map.ones(), 64);
        for ordinal in 0..32 {
            map.set(ordinal, false).expect("clear");
        }
        assert_eq!(map.ones(), 32);
        map.clear();
        assert_eq!(map.ones(), 0);
        assert_eq!(map.resident_bytes(), 0);
    }

    #[test]
    fn a_short_trailing_page_keeps_its_exact_length() {
        // 8 bits past a page boundary: the second page holds a single byte.
        let mut map = SparseBitmap::new(BITMAP_PAGE_BYTES * 8 + 8).expect("bitmap");
        assert_eq!(map.page_count, 2);
        assert_eq!(map.page_len(1).expect("page len"), 1);
        assert_eq!(map.page_bytes(1).expect("page bytes").len(), 1);
        map.set(BITMAP_PAGE_BYTES * 8, true).expect("set");
        assert_eq!(map.resident_bytes(), 1);
        assert_eq!(map.page_bytes(1).expect("page bytes"), &[1u8]);
        assert!(map.get(BITMAP_PAGE_BYTES * 8 + 8).is_err());
    }

    #[test]
    fn loading_an_all_zero_page_leaves_it_unmaterialized() {
        let mut map = SparseBitmap::new(BITMAP_PAGE_BYTES * 8).expect("bitmap");
        map.insert_loaded_page(0, vec![0; BITMAP_PAGE_BYTES as usize])
            .expect("load zero page");
        assert_eq!(map.resident_bytes(), 0);
        assert_eq!(map.ones(), 0);

        let mut bytes = vec![0; BITMAP_PAGE_BYTES as usize];
        bytes[7] = 0b0000_0101;
        map.insert_loaded_page(0, bytes).expect("load live page");
        assert_eq!(map.resident_bytes(), BITMAP_PAGE_BYTES);
        assert_eq!(map.ones(), 2);
        assert!(map.get(56).expect("get"));
        assert!(map.get(58).expect("get"));
    }
}

#[cfg(test)]
mod limit_tests {
    use super::*;

    fn sidecar_spec(limits: ReadLimits) -> FormatSpec {
        FormatSpec::new(
            b"SIDE",
            1,
            crate::Endian::Little,
            0,
            crate::IndexPolicy::ScanOnOpen,
            IntegrityPolicy::None,
            crate::RecoveryPolicy::Strict,
            crate::ManifestPolicy::None,
            &[],
        )
        .with_read_limits(limits)
    }

    #[test]
    fn sidecar_plan_checks_exact_lengths_limits_and_reserved_fields() {
        let spec = sidecar_spec(ReadLimits::finite_all(100));
        let plan = matrix_sidecar_read_plan(spec, 60, 48, 4, 4, 4, 0, 0, 0).unwrap();
        assert_eq!(
            plan,
            MatrixSidecarReadPlan {
                format_magic_offset: 48,
                category_offset: 52,
                payload_offset: 56,
                payload_len: 4,
                total_len: 60,
            }
        );

        assert!(matches!(
            matrix_sidecar_read_plan(spec, 61, 48, 4, 4, 4, 0, 0, 0),
            Err(Error::InvalidMatrixSidecar)
        ));
        assert!(matches!(
            matrix_sidecar_read_plan(spec, 60, 48, 4, 4, 4, 1, 0, 0),
            Err(Error::InvalidMatrixSidecar)
        ));

        let sidecar_limited = sidecar_spec(ReadLimits::finite_all(100).with_max_sidecar_len(59));
        assert!(matches!(
            matrix_sidecar_read_plan(sidecar_limited, 60, 48, 4, 4, 4, 0, 0, 0),
            Err(Error::LimitExceeded {
                resource: "sidecar length",
                actual: 60,
                limit: 59,
            })
        ));

        let payload_limited =
            sidecar_spec(ReadLimits::finite_all(100).with_max_materialized_bytes(3));
        assert!(matches!(
            matrix_sidecar_read_plan(payload_limited, 60, 48, 4, 4, 4, 0, 0, 0),
            Err(Error::LimitExceeded {
                resource: "materialized bytes",
                actual: 4,
                limit: 3,
            })
        ));

        let unbounded = sidecar_spec(ReadLimits::finite_all(u64::MAX));
        for result in [
            matrix_sidecar_read_plan(unbounded, u64::MAX, 48, u64::MAX, 4, 4, 0, 0, 0),
            matrix_sidecar_read_plan(unbounded, u64::MAX, 48, 4, 4, u64::MAX, 0, 0, 0),
        ] {
            assert!(matches!(
                result,
                Err(Error::ResourceArithmeticOverflow { .. })
            ));
        }
    }
}
