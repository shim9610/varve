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
use crc_valid_evidence::CrcValidEvidence;
use fatal_access::{FatalAccessAllowed, FatalAccessGate};

const VMAT_MAGIC: &[u8; 4] = b"VMAT";
// PERF-01/PERF-02: layout version 2 replaces the whole-bitmap commit checksum
// with per-page digests and stops explicitly initialising the per-cell CRC and
// validity regions at create. Version 1 artifacts describe a different physical
// representation, so they are rejected as stale-regenerable rather than being
// reinterpreted under the new rules.
// PERF-01: layout version 3 adds the persisted page-index region. Open used to
// visit `0..page_count` of every commit map and validity bitmap, which is
// `Theta(cell_count / 8)` of loop iterations even when the filesystem proved
// every page a hole, and degraded to that much *I/O* whenever the allocation
// map was unavailable or exceeded the tracked-extent cap. Version 2 artifacts
// carry no index, so they are rejected as stale-regenerable rather than being
// reinterpreted under the new rules.
// PERF-01/SAFE: layout version 4 gives every persisted page index a validated
// occupancy header and makes the array the *live* set rather than the history
// of everything ever published. Version 3 arrays are terminator-scanned, so a
// zeroed or garbage entry silently truncated enumeration and hid every later
// committed page; and they never shed an entry, so both the array and its
// in-memory tracking grew with historically touched pages. A version 3 array
// has no header slot and a different entry base, so version 3 artifacts are
// rejected as stale-regenerable rather than being reinterpreted.
const VMAT_VERSION: u16 = 4;
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
/// One persisted page-index slot: a little-endian `u64`.
///
/// Slot `0` is the occupancy header; slots `1..=count` are entries holding the
/// page ordinal plus one, so that a zero entry inside the counted prefix is
/// provably damage rather than an ambiguous "end of array" (VMAT v4).
const PAGE_INDEX_ENTRY_LEN: u64 = 8;
/// Slots reserved ahead of the first entry: the occupancy header.
const PAGE_INDEX_HEADER_SLOTS: u64 = 1;
/// Smallest persisted-page-index request (64 bytes).
const PAGE_INDEX_MIN_SCAN_ENTRIES: u64 = 8;
/// Largest persisted-page-index request (4 KiB).
const PAGE_INDEX_SCAN_ENTRIES: u64 = 512;
/// Largest occupancy count the page-index header can encode.
///
/// The header packs the count into the low 48 bits and a derived check value
/// into the high 16, so a single 8-byte slot carries its own redundancy. A
/// matrix whose page count would exceed this is refused at layout time rather
/// than being written with an unrepresentable header.
const PAGE_INDEX_MAX_ENTRIES: u64 = (1u64 << 48) - 1;
/// Odd 64-bit mixing constant used to derive the page-index header check bits.
///
/// This is a redundancy code, not authentication: it detects a torn or
/// bit-flipped header slot, and deliberately makes no claim against an actor
/// who can rewrite the file, exactly as [`write_page_digest`] documents for the
/// digest array.
const PAGE_INDEX_HEADER_MIX: u64 = 0x9E37_79B9_7F4A_7C15;
/// Modelled resident cost of one page-index *slot* (the `Vec<u64>` element).
///
/// Deliberately double the 8 bytes the element itself occupies, so the charge
/// stays above the amortised spare capacity a growing `Vec` holds. This is a
/// conservative model, not a measurement of the live allocation.
const PAGE_INDEX_SLOT_RESIDENT_BYTES: u64 = 16;
/// Modelled resident cost of one page-index *page-to-slot map* entry.
///
/// A `HashMap<u64, u64>` entry is 16 bytes of key/value plus one control byte,
/// held at a 7/8 load factor: about 20 bytes. Charged as 32 so the model stays
/// above any small-table specialisation on the supported toolchains.
const PAGE_INDEX_MAP_RESIDENT_BYTES: u64 = 32;
/// Modelled resident cost of tracking one newly indexed page.
const PAGE_INDEX_ENTRY_RESIDENT_BYTES: u64 =
    PAGE_INDEX_SLOT_RESIDENT_BYTES + PAGE_INDEX_MAP_RESIDENT_BYTES;
/// Occupancy-header encoding published while a whole-map page-index
/// republication is in flight (F-02).
///
/// It is never a resting state: [`write_commit_map_pages`] writes it, makes it
/// durable, and only replaces it with a real count once every entry, digest and
/// page it describes is durable too. Any interruption in between therefore
/// leaves this value on disk, and [`load_page_index`] refuses it with a fatal
/// finding instead of reading a half-rebuilt index as authoritative.
///
/// It is *not* representable as a valid header. `page_index_header_count`
/// rejects it at every capacity, which
/// `the_rebuild_marker_is_never_a_valid_occupancy_header` proves, so a reader
/// that predates this marker treats it as a damaged header — also fail-closed —
/// and no on-disk layout version bump is needed for a value that only exists
/// between a rebuild's first and last write.
const PAGE_INDEX_REBUILD_MARKER: u64 = u64::MAX;
const PAGE_STATE_UNINITIALIZED: u32 = 0;
const PAGE_STATE_INITIALIZED: u32 = 1;
const ZERO_PAGE: [u8; BITMAP_PAGE_BYTES as usize] = [0; BITMAP_PAGE_BYTES as usize];
const MATRIX_BYTES_RESOURCE: &str = "matrix bytes";
const MATRIX_SLOT_PAYLOAD_RESOURCE: &str = "matrix slot payload";
const MATRIX_DESCRIPTOR_RESOURCE: &str = "matrix descriptors";
/// Allocation resource for the ordinary `PackedBitmap` codec, which is usable
/// in plain variable fields and must not report matrix-shaped failures there.
const PACKED_BITMAP_RESOURCE: &str = "packed bitmap bytes";
#[allow(dead_code)]
const MATRIX_SIDECAR_RESOURCE: &str = "matrix sidecar";

type CommitPlan = (String, MatrixCommitKind, u64);
type StoredCommitPlan = (String, MatrixCommitKind, u64, u64, u64);

/// Upper bound on the number of filesystem extents tracked for one matrix file.
///
/// A file fragmented beyond this is dense enough that skipping holes would save
/// nothing, so the query gives up and reports no allocation map at all. Open
/// then enumerates from the persisted page index alone, which still names every
/// page holding state; it does not fall back to reading every page.
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
/// stray byte into an untouched page necessarily allocates that page, so *while
/// this map is available* such a page is pulled back into the visit set and the
/// corruption is still read and still reported.
///
/// F-06: that last property is conditional on the map. Where it is `None` the
/// visit set is the persisted page index alone, which names only pages the
/// matrix itself published; a stray byte written out of band into a page the
/// matrix never published is then not visited and not reported. The page is not
/// trusted either — it is simply never loaded — so this is a limit on corruption
/// visibility, not a path that accepts unchecked bytes. Every persisted-index
/// page is verified on every platform.
///
/// This map is a *secondary* source. Open enumerates the union of it and the
/// persisted page index in `O(Q)` for `Q` candidate pages (see
/// [`pages_to_visit`]), never `O(cell_count)`.
///
/// `None` means "unknown": the platform or filesystem cannot answer, or the
/// file is fragmented past [`MAX_TRACKED_EXTENTS`]. Enumeration then comes from
/// the persisted page index alone and still costs `O(live pages)`. Since `VMAT`
/// v3 there is no full-logical-scan fallback; unallocated-but-indexed pages are
/// simply read rather than skipped.
#[derive(Clone, Debug, Default)]
struct AllocatedExtents {
    /// Half-open `[start, end)` ranges, sorted and disjoint.
    ranges: Vec<(u64, u64)>,
}

impl AllocatedExtents {
    /// Queries the filesystem, restoring the file cursor before returning.
    fn query(file: &mut File) -> Option<Self> {
        if allocation_map_forced_unavailable() {
            return None;
        }
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

    /// Appends every `unit`-sized slot of `[base, base + len)` that overlaps an
    /// allocated range, in ascending order.
    ///
    /// PERF-01: the walk is over ranges, not over slots, so its cost is
    /// proportional to the *allocated* part of the region and never to the
    /// region's logical size. This is what lets open enumerate the pages that
    /// may hold bytes without a `0..page_count` loop.
    fn allocated_units(
        &self,
        base: u64,
        len: u64,
        unit: u64,
        unit_count: u64,
        out: &mut Vec<u64>,
        resource: &'static str,
    ) -> Result<()> {
        if len == 0 || unit == 0 || unit_count == 0 {
            return Ok(());
        }
        let end = base.checked_add(len).ok_or(Error::InvalidMatrixLayout)?;
        let first_range = self
            .ranges
            .partition_point(|(_, range_end)| *range_end <= base);
        for (range_start, range_end) in &self.ranges[first_range..] {
            if *range_start >= end {
                break;
            }
            let from = (*range_start).max(base) - base;
            let to = (*range_end).min(end) - base;
            if to == 0 {
                continue;
            }
            let first = from / unit;
            let last = ((to - 1) / unit).min(unit_count - 1);
            if first > last {
                continue;
            }
            let count = last - first + 1;
            try_reserve_vec(
                out,
                usize::try_from(count).map_err(|_| Error::InvalidMatrixLayout)?,
                resource,
            )?;
            for unit_index in first..=last {
                out.push(unit_index);
            }
        }
        Ok(())
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

/// Zeroes `[offset, offset + len)` by removing the range where the platform
/// and filesystem support it, and by streaming zero bytes where they do not.
///
/// F-08, stated exactly rather than qualitatively: range removal is attempted
/// on Windows (`FSCTL_SET_ZERO_DATA`) and Linux (`FALLOC_FL_PUNCH_HOLE`) only,
/// and even there it fails on filesystems without sparse support. Every other
/// target, and every failed attempt, streams `len` zero bytes, which is
/// `Theta(len)` — for a whole-category clear that is `Theta(cells / 8)`, not
/// cell-count independent. The outcome is recorded unconditionally so a caller
/// can *detect* the slow path with
/// [`MatrixRecoveryReport::matrix_last_zero_range_streamed_bytes`] rather than
/// having to infer it from the target triple.
fn zero_range(file: &mut File, offset: u64, len: u64) -> Result<()> {
    if punch_zero_range(file, offset, len) {
        sparse_zeroing::record(0);
        return Ok(());
    }
    sparse_zeroing::record(len);
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
    /// F-02: the page-index publication boundary, which is the first fallible
    /// step after a commit-bit mutation charges its payload page.
    static FAIL_NEXT_PAGE_INDEX_WRITE: std::cell::Cell<bool> = const {
        std::cell::Cell::new(false)
    };
    /// F-02: the page-digest boundary, which sits between page-index
    /// publication and the bitmap-byte write.
    static FAIL_NEXT_PAGE_DIGEST_WRITE: std::cell::Cell<bool> = const {
        std::cell::Cell::new(false)
    };
}

/// Always-compiled observation of the sparse-zeroing fallback (PERF-03/F-08).
///
/// Whole-category clear and whole-map rebuild ask the filesystem to *remove* a
/// byte range. Where that is unavailable the range has to be streamed as zero
/// bytes instead, which is `Theta(bitmap bytes)` rather than cell-count
/// independent. A caller must be able to detect that it is on the slow path
/// without enabling a test-only feature, so this counter is not feature gated.
mod sparse_zeroing {
    use std::cell::Cell;

    std::thread_local! {
        /// Bytes the most recent [`super::zero_range`] call had to stream
        /// because the range could not be removed. Zero after a successful
        /// removal.
        pub(super) static LAST_STREAMED_BYTES: Cell<u64> = const { Cell::new(0) };
        /// Bytes streamed by every [`super::zero_range`] call on this thread.
        pub(super) static TOTAL_STREAMED_BYTES: Cell<u64> = const { Cell::new(0) };
    }

    pub(super) fn record(streamed: u64) {
        LAST_STREAMED_BYTES.with(|cell| cell.set(streamed));
        if streamed != 0 {
            TOTAL_STREAMED_BYTES.with(|cell| cell.set(cell.get().saturating_add(streamed)));
        }
    }

    pub(super) fn last() -> u64 {
        LAST_STREAMED_BYTES.with(Cell::get)
    }

    pub(super) fn total() -> u64 {
        TOTAL_STREAMED_BYTES.with(Cell::get)
    }
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
        pub(super) static OPEN_BITMAP_PAGES_VISITED: Cell<u64> = const { Cell::new(0) };
        pub(super) static FORCE_NO_ALLOCATION_MAP: Cell<u64> = const { Cell::new(0) };
        pub(super) static PAGE_INDEX_BYTES_RESIDENT: Cell<u64> = const { Cell::new(0) };
        /// F-02: countdown to an injected page-index entry-write failure.
        /// Zero is inert; `n` fails the `n`-th write from now.
        pub(super) static PAGE_INDEX_ENTRY_WRITE_FAILURE: Cell<u64> = const { Cell::new(0) };
        /// F-02: countdown to an injected page-index header-write failure,
        /// which covers both the rebuild marker and the final publication.
        pub(super) static PAGE_INDEX_HEADER_WRITE_FAILURE: Cell<u64> = const { Cell::new(0) };
        /// F-03: countdown to an injected first-touch bitmap page allocation
        /// failure, which is the step that used to run *after* the bitmap byte
        /// was already durable.
        pub(super) static BITMAP_PAGE_ALLOCATION_FAILURE: Cell<u64> = const { Cell::new(0) };
        /// F-03: countdown to an injected page-index *mirror* reservation
        /// failure — the `try_reserve` that `compact_page_index` performed after
        /// it had already rewritten the entry array on disk. Zero is inert.
        pub(super) static PAGE_INDEX_RESERVATION_FAILURE: Cell<u64> = const { Cell::new(0) };
        /// F-02: whole-map republication stage at which the process aborts, so
        /// a test can prove what a *process-level* interruption leaves on disk.
        /// Zero is inert.
        pub(super) static REBUILD_ABORT_STAGE: Cell<u64> = const { Cell::new(0) };
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

    pub(super) fn get_always(cell: &'static std::thread::LocalKey<Cell<u64>>) -> u64 {
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
fn count_open_bitmap_pages_visited(pages: u64) {
    scaling_counters::add(&scaling_counters::OPEN_BITMAP_PAGES_VISITED, pages);
}

#[cfg(not(any(test, feature = "scalable-fault-injection")))]
fn count_open_bitmap_pages_visited(_pages: u64) {}

/// True when a test has asked open to behave as if the platform reported no
/// allocation map at all, which is also what a file fragmented past
/// [`MAX_TRACKED_EXTENTS`] produces.
#[cfg(any(test, feature = "scalable-fault-injection"))]
fn allocation_map_forced_unavailable() -> bool {
    scaling_counters::get_always(&scaling_counters::FORCE_NO_ALLOCATION_MAP) != 0
}

#[cfg(not(any(test, feature = "scalable-fault-injection")))]
fn allocation_map_forced_unavailable() -> bool {
    false
}

/// Consumes one step of an injected-failure countdown, reporting whether this
/// call is the selected one (F-02).
#[cfg(any(test, feature = "scalable-fault-injection"))]
fn take_injected_failure(cell: &'static std::thread::LocalKey<std::cell::Cell<u64>>) -> bool {
    let remaining = scaling_counters::get_always(cell);
    if remaining == 0 {
        return false;
    }
    scaling_counters::set(cell, remaining - 1);
    remaining == 1
}

/// Aborts the process at a named boundary inside a whole-map page-index
/// republication (F-02).
///
/// Interruption between emptying the persisted index and republishing it is the
/// only way to observe the state [`PAGE_INDEX_REBUILD_MARKER`] exists to make
/// visible, and it cannot be produced by returning an error, because a returned
/// error also unwinds in-memory state. Inert unless a test arms a stage
/// explicitly, and compiled out of every build without the test-only
/// fault-injection feature.
#[cfg(any(test, feature = "scalable-fault-injection"))]
fn rebuild_abort_stage(stage: u64) {
    if scaling_counters::get_always(&scaling_counters::REBUILD_ABORT_STAGE) == stage {
        std::process::abort();
    }
}

#[cfg(not(any(test, feature = "scalable-fault-injection")))]
fn rebuild_abort_stage(_stage: u64) {}

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
    scaling_counters::set(
        &scaling_counters::PAGE_INDEX_BYTES_RESIDENT,
        layout.resident_page_index_bytes,
    );
}

#[cfg(not(any(test, feature = "scalable-fault-injection")))]
fn record_open_resident_bitmap_bytes(_layout: &MatrixLayout) {}

/// Tracks the live resident total, so PERF-02 eviction is observable and not
/// only visible at open.
#[cfg(any(test, feature = "scalable-fault-injection"))]
fn record_resident_bitmap_bytes(bytes: u64) {
    scaling_counters::set(&scaling_counters::OPEN_BITMAP_BYTES_RESIDENT, bytes);
}

#[cfg(not(any(test, feature = "scalable-fault-injection")))]
fn record_resident_bitmap_bytes(_bytes: u64) {}

/// Tracks the modelled resident cost of persisted page-index tracking, so
/// F-03's "resident cost follows live state" contract is observable rather than
/// asserted.
#[cfg(any(test, feature = "scalable-fault-injection"))]
fn record_resident_page_index_bytes(bytes: u64) {
    scaling_counters::set(&scaling_counters::PAGE_INDEX_BYTES_RESIDENT, bytes);
}

#[cfg(not(any(test, feature = "scalable-fault-injection")))]
fn record_resident_page_index_bytes(_bytes: u64) {}

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

/// Byte length of a `PackedBitmap` carrying `bit_len` bits.
///
/// Deliberately separate from [`bit_bytes`]: `PackedBitmap` is an ordinary
/// variable-field codec that happens to live in this module, so its failures
/// must be codec failures. Reporting `InvalidMatrixLayout` for a malformed
/// bitmap in a non-matrix field told the caller to look at a matrix layout that
/// may not even exist.
fn packed_bitmap_bytes(bit_len: u64) -> Result<u64> {
    bit_len
        .checked_add(7)
        .map(|value| value / 8)
        .ok_or(Error::LengthOverflow { value: bit_len })
}

impl PackedBitmap {
    pub fn new(bit_len: u64) -> Result<Self> {
        Ok(Self {
            bit_len,
            bytes: filled_bytes_for(packed_bitmap_bytes(bit_len)?, 0, PACKED_BITMAP_RESOURCE)?,
        })
    }

    pub fn bit_len(&self) -> u64 {
        self.bit_len
    }

    pub fn get(&self, ordinal: u64) -> Result<bool> {
        if ordinal >= self.bit_len {
            return Err(Error::InvalidCanonicalEncoding(
                "packed bitmap ordinal is past its bit length",
            ));
        }
        get_bit(&self.bytes, ordinal)
    }

    pub fn set(&mut self, ordinal: u64, value: bool) -> Result<()> {
        if ordinal >= self.bit_len {
            return Err(Error::InvalidCanonicalEncoding(
                "packed bitmap ordinal is past its bit length",
            ));
        }
        set_bit(&mut self.bytes, ordinal, value)
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }
}

/// Structural codec identity of [`PackedBitmap`] (API-04).
///
/// Derived the way the built-in container codecs derive theirs: a tag naming
/// this codec folded with the identities of the codecs whose bytes it actually
/// emits, in emission order (`u64` bit length, then `Vec<u8>` payload). The
/// value is a const expression over compile-time constants only, so it is
/// stable across builds, distinct from any other built-in identity, and
/// non-zero — which is what `#[derive(VarveBlock)]` requires of every field
/// codec, and therefore what makes a `PackedBitmap` *variable* field compile.
///
/// It deliberately does **not** make `PackedBitmap` usable as a fixed-width
/// matrix field: the encoding above has no width that is fixed by the type, so
/// there is no compile-time slot stride for it. See
/// `varve-macros::matrix_field_is_fixed_width`.
const PACKED_BITMAP_SCHEMA_ID: u64 = crate::codec::container_schema_id_with_arity(
    b"packed_bitmap",
    1,
    &[
        <u64 as crate::VarveEncode>::SCHEMA_ID,
        <Vec<u8> as crate::VarveEncode>::SCHEMA_ID,
    ],
);

impl crate::VarveEncode for PackedBitmap {
    const WIRE_TYPE: crate::WireType = crate::WireType::Bytes;
    const SCHEMA_ID: u64 = PACKED_BITMAP_SCHEMA_ID;

    fn encode_varve(&self, encoder: &mut crate::Encoder) -> Result<()> {
        crate::VarveEncode::encode_varve(&self.bit_len, encoder)?;
        crate::VarveEncode::encode_varve(&self.bytes, encoder)
    }
}

impl crate::VarveDecode for PackedBitmap {
    const WIRE_TYPE: crate::WireType = crate::WireType::Bytes;
    // Symmetric codec: `decode_varve` accepts exactly what `encode_varve`
    // emits, so it declares the same identity, as the built-in codecs do.
    const SCHEMA_ID: u64 = PACKED_BITMAP_SCHEMA_ID;

    fn decode_varve(decoder: &mut crate::Decoder<'_>) -> Result<Self> {
        let bit_len = <u64 as crate::VarveDecode>::decode_varve(decoder)?;
        let bytes = <Vec<u8> as crate::VarveDecode>::decode_varve(decoder)?;
        // Ordinary malformed codec input, so an ordinary decode error: this
        // type is accepted in plain variable fields and must not report a
        // matrix-layout fault there.
        if bytes.len() as u64 != packed_bitmap_bytes(bit_len)? {
            return Err(Error::InvalidCanonicalEncoding(
                "packed bitmap payload length does not match its bit length",
            ));
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

/// Always-available cost qualification for whole-range zeroing (F-08).
///
/// Whole-category clear, whole-map rebuild, and page-index rebuild all ask the
/// filesystem to remove a byte range. Where removal is unavailable the range is
/// streamed as zero bytes, which costs `Theta(bitmap bytes)`. These accessors
/// are deliberately *not* behind a test-only feature: a caller that depends on
/// the cheap path must be able to detect at runtime that it did not get it.
impl MatrixRecoveryReport {
    /// Whether this build targets a platform with a range-removal call at all.
    ///
    /// Necessary but **not** sufficient: the call still fails on filesystems
    /// without sparse-file support, in which case the streaming fallback runs
    /// and [`Self::matrix_last_zero_range_streamed_bytes`] reports it.
    pub const fn matrix_sparse_zeroing_supported() -> bool {
        cfg!(any(windows, target_os = "linux"))
    }

    /// Bytes the most recent *single* range-zeroing request on this thread had
    /// to stream because the range could not be removed.
    ///
    /// `0` means that one range was removed without writing it, and any nonzero
    /// value is proof that the caller is on the `Theta(bytes)` fallback path —
    /// but the scope is deliberately narrow in two ways a caller must respect:
    ///
    /// * **One request, not one operation.** A whole-category clear issues
    ///   several range requests (validity bitmap, page indexes, page digests,
    ///   commit map). This reports only the last of them, so a `0` here does
    ///   not prove the whole clear avoided streaming. To qualify an operation,
    ///   read [`Self::matrix_total_zero_range_streamed_bytes`] before and after
    ///   it: the delta is nonzero exactly when some request streamed.
    /// * **This thread only.** Both counters are thread-local. A clear
    ///   performed on a worker thread is invisible to a reader on another
    ///   thread, which would see `0` and wrongly conclude it took the cheap
    ///   path. Sample the counters on the thread that ran the operation.
    pub fn matrix_last_zero_range_streamed_bytes() -> u64 {
        sparse_zeroing::last()
    }

    /// Bytes streamed by every range-zeroing request **on this thread** since
    /// it started, across all matrices.
    ///
    /// This is the counter to qualify a whole operation with: take it before
    /// and after, on the thread performing the operation, and a nonzero delta
    /// is exact proof that at least one range had to be streamed.
    pub fn matrix_total_zero_range_streamed_bytes() -> u64 {
        sparse_zeroing::total()
    }
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

    /// Modelled resident bytes held by persisted page-index tracking on this
    /// thread (F-03).
    ///
    /// This is the second term of the `ReadLimitKey::MatrixBitmapBytes` charge.
    /// It follows the *live* page set, so set/clear churn returns it to its
    /// starting value instead of growing with historically touched pages.
    pub fn matrix_resident_page_index_bytes() -> u64 {
        scaling_counters::get(&scaling_counters::PAGE_INDEX_BYTES_RESIDENT)
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
    /// unwritten, so no page could be skipped without reading it. Enumeration of
    /// the pages this matrix published is unaffected: it comes from the
    /// persisted page index, which names every page holding state, so open still
    /// costs `O(live pages)` rather than `O(cell_count / 8)`, and **every
    /// persisted-index page is verified on every platform**.
    ///
    /// F-06: two things are lost, not one. The first is the ability to skip the
    /// read of an indexed page whose bytes the filesystem would have proved
    /// zero. The second is coverage of pages the matrix never indexed: those are
    /// additionally visited **only** when the platform supplies a usable
    /// allocation map, so with this false a stray byte written out of band into
    /// a never-published page is not detected at open. Such a page is not
    /// trusted — it is never loaded — but it is also not reported.
    pub fn matrix_open_allocation_map_available() -> bool {
        scaling_counters::get(&scaling_counters::OPEN_ALLOCATION_MAP_AVAILABLE) != 0
    }

    /// Bitmap pages that matrix opens on this thread actually visited.
    ///
    /// PERF-01: this is the loop bound that must not follow the logical page
    /// count. Open visits the union of the persisted page index and, where the
    /// filesystem can answer, the pages its allocation map reports as written.
    /// F-07: that union is the *candidate* page count — `L` indexed pages plus
    /// `A` allocation-derived candidates — and only the first term follows live
    /// state. A densely allocated bitmap region makes `A` proportional to the
    /// region's page count even when few bits are live, so this counter is
    /// bounded by candidate pages, not by committed cells. Bytes read are a
    /// weaker witness, because a loop that skips every page still runs.
    pub fn matrix_open_bitmap_pages_visited() -> u64 {
        scaling_counters::get(&scaling_counters::OPEN_BITMAP_PAGES_VISITED)
    }

    /// Resident bitmap page bytes currently held by the most recently created,
    /// opened, or mutated matrix layout on this thread.
    ///
    /// PERF-02: this falls again when a page loses its final set bit, so a
    /// set/clear cycle returns to its starting value instead of accumulating
    /// residency for every page it has ever touched.
    pub fn matrix_resident_bitmap_bytes() -> u64 {
        scaling_counters::get(&scaling_counters::OPEN_BITMAP_BYTES_RESIDENT)
    }

    /// Makes matrix open behave as if the platform reported no allocation map,
    /// which is also what a file fragmented past the tracked-extent cap
    /// produces. Open must stay bounded by the persisted page index in that
    /// case rather than falling back to visiting every logical page.
    pub fn force_matrix_allocation_map_unavailable(force: bool) {
        scaling_counters::set(&scaling_counters::FORCE_NO_ALLOCATION_MAP, u64::from(force));
    }

    /// Fails the `after`-th persisted page-index *entry* write from now (F-02).
    ///
    /// Zero disarms. This is the boundary a whole-map republication crosses once
    /// per materialised page, so it is how a test reaches the window between
    /// emptying the persisted index and republishing it in full.
    pub fn inject_matrix_page_index_entry_write_failure(after: u64) {
        scaling_counters::set(&scaling_counters::PAGE_INDEX_ENTRY_WRITE_FAILURE, after);
    }

    /// Fails the `after`-th persisted page-index *header* write from now (F-02).
    ///
    /// Zero disarms. During a whole-map republication the header is written
    /// exactly twice: the rebuild marker first and the published occupancy count
    /// last, so `1` and `2` select those two boundaries.
    pub fn inject_matrix_page_index_header_write_failure(after: u64) {
        scaling_counters::set(&scaling_counters::PAGE_INDEX_HEADER_WRITE_FAILURE, after);
    }

    /// Fails the `after`-th first-touch bitmap page allocation from now (F-03).
    ///
    /// Zero disarms. This is the allocation that used to run *after* the bitmap
    /// byte was durable, so a typed `AllocationFailed` there left disk holding a
    /// commit bit that memory did not have. It is now the last fallible step
    /// before persistence begins, which is what this hook exists to prove.
    pub fn inject_matrix_bitmap_page_allocation_failure(after: u64) {
        scaling_counters::set(&scaling_counters::BITMAP_PAGE_ALLOCATION_FAILURE, after);
    }

    /// Fails the `after`-th persisted page-index *mirror* reservation from now
    /// (F-03). Zero disarms.
    ///
    /// This is the allocation that page-index compaction used to perform after
    /// it had already rewritten the entry array on disk, so a typed
    /// `AllocationFailed` there left the persisted order and the in-memory slot
    /// map describing different arrays on a writer that stayed usable. It is now
    /// a precondition of the rewrite, which is what this hook exists to prove:
    /// arming it must leave the array byte-for-byte unchanged.
    pub fn inject_matrix_page_index_mirror_reservation_failure(after: u64) {
        scaling_counters::set(&scaling_counters::PAGE_INDEX_RESERVATION_FAILURE, after);
    }

    /// Aborts the calling process at one stage of a whole-map page-index
    /// republication (F-02). Zero disarms.
    ///
    /// The stages are `1` once the rebuild marker is durable and before the
    /// entry region is cleared, `2` once it is cleared and before any entry is
    /// republished, `3` after the first republished entry, and `4` once every
    /// entry, digest and page is durable and before the new occupancy count is
    /// published. Only a dedicated child process should arm this.
    pub fn abort_process_at_matrix_rebuild_stage(stage: u64) {
        scaling_counters::set(&scaling_counters::REBUILD_ABORT_STAGE, stage);
    }

    /// Resets every matrix integrity counter for the calling thread.
    ///
    /// The forced-unavailable allocation map is a mode, not a counter, so it is
    /// left alone: a test turns it off explicitly.
    pub fn reset_matrix_integrity_counters() {
        scaling_counters::set(&scaling_counters::BITMAP_BYTES_HASHED, 0);
        scaling_counters::set(&scaling_counters::CREATE_METADATA_BYTES_WRITTEN, 0);
        scaling_counters::set(&scaling_counters::OPEN_BITMAP_BYTES_RESIDENT, 0);
        scaling_counters::set(&scaling_counters::OPEN_BITMAP_BYTES_READ, 0);
        scaling_counters::set(&scaling_counters::CATEGORY_CLEAR_BYTES_WRITTEN, 0);
        scaling_counters::set(&scaling_counters::OPEN_ALLOCATION_MAP_AVAILABLE, 0);
        scaling_counters::set(&scaling_counters::OPEN_BITMAP_PAGES_VISITED, 0);
        scaling_counters::set(&scaling_counters::PAGE_INDEX_BYTES_RESIDENT, 0);
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
struct BitmapPage {
    bytes: Arc<Vec<u8>>,
    /// Set bits held by this page, maintained on every mutation so that
    /// "this page is now all zero" is an `O(1)` question (PERF-02).
    ones: u64,
}

/// One bitmap byte write whose fallible work is already done (F-03).
///
/// Produced by [`SparseBitmap::prepare_byte_write`] and consumed by
/// [`SparseBitmap::commit_byte_write`]. Holding one of these is the statement
/// "installing this write cannot fail and cannot allocate", which is what lets a
/// mutation put every fallible step, including the page allocation, strictly
/// before the disk write it accompanies.
///
/// Dropping one instead of committing it is the failure path: the detached page
/// is freed and the bitmap is exactly as it was.
struct PreparedByteWrite {
    page: u64,
    within: usize,
    value: u8,
    /// Page allocated for this write but not yet installed. `None` when the
    /// page is already resident, or when the write changes nothing.
    fresh: Option<BitmapPage>,
    /// `false` when the byte already holds the value being written.
    changes: bool,
    materialised: u64,
    page_ones_after: u64,
    ones_after: u64,
}

/// Residency change produced by one bitmap mutation.
///
/// PERF-02: a page that loses its final set bit is released immediately, so
/// long-running set/clear churn cannot hold residency proportional to the pages
/// it has historically touched. Both terms are reported so the caller can charge
/// the growth *before* the memory is taken and refund the shrink afterwards.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct PageResidencyDelta {
    materialised: u64,
    released: u64,
}

impl PageResidencyDelta {
    const NONE: Self = Self {
        materialised: 0,
        released: 0,
    };

    const fn materialised(bytes: u64) -> Self {
        Self {
            materialised: bytes,
            released: 0,
        }
    }
}

#[derive(Clone, Debug)]
struct SparseBitmap {
    bit_count: u64,
    byte_len: u64,
    page_count: u64,
    pages: HashMap<u64, BitmapPage>,
    ones: u64,
    /// Page named by each occupied slot of the persisted page index, in slot
    /// order, including any duplicate a crash-interrupted removal left behind.
    ///
    /// This mirrors the persisted array exactly: `index_slots.len()` is the
    /// occupancy count written into the array's header slot, and entry `k`
    /// lives at slot `k + 1` on disk.
    index_slots: Vec<u64>,
    /// First slot naming each distinct page, so a page can be found, skipped,
    /// and *removed* in `O(1)` (F-03).
    ///
    /// A page joins the index the first time it is published holding a set bit
    /// and leaves it again when its final set bit clears, so both this map and
    /// the persisted array track **live** pages rather than every page the
    /// matrix has ever published. That is what keeps the resident cost of the
    /// index proportional to live state and not to historical churn.
    ///
    /// Both terms are charged against `ReadLimitKey::MatrixBitmapBytes` through
    /// [`ResidentBitmapBudget::charge_index`] before the memory is taken, using
    /// the [`PAGE_INDEX_ENTRY_RESIDENT_BYTES`] model, so the limit bounds the
    /// whole resident matrix bitmap footprint and not just its payload pages.
    indexed_pages: HashMap<u64, u64>,
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
            index_slots: Vec::new(),
            indexed_pages: HashMap::new(),
        })
    }

    /// Entries the persisted array currently holds.
    fn index_len(&self) -> Result<u64> {
        usize_to_u64(self.index_slots.len())
    }

    /// Records that `page` occupies the next persisted index slot.
    ///
    /// The residency the tracking will take is charged against the resident
    /// bitmap budget *before* it is taken, and refunded if the reservation then
    /// fails, so no allocation escapes the limit (F-03). A repeat of a page
    /// already named by an earlier slot — which only a crash-interrupted
    /// removal can produce — still occupies its own slot, so the mirror never
    /// drifts from the array.
    fn note_indexed_page(&mut self, page: u64, budget: &mut ResidentBitmapBudget) -> Result<()> {
        let slot = self.index_len()?;
        if slot >= self.page_count || slot >= PAGE_INDEX_MAX_ENTRIES {
            return Err(Error::InvalidMatrixLayout);
        }
        let known = self.indexed_pages.contains_key(&page);
        let charge = if known {
            PAGE_INDEX_SLOT_RESIDENT_BYTES
        } else {
            PAGE_INDEX_ENTRY_RESIDENT_BYTES
        };
        budget.charge_index(charge)?;
        let reserved = (|| -> Result<()> {
            try_reserve_vec(
                &mut self.index_slots,
                1,
                ReadLimitKey::MatrixBitmapBytes.resource(),
            )?;
            if !known {
                try_reserve_map(
                    &mut self.indexed_pages,
                    1,
                    ReadLimitKey::MatrixBitmapBytes.resource(),
                )?;
            }
            Ok(())
        })();
        if let Err(err) = reserved {
            budget.release_index(charge);
            return Err(err);
        }
        self.index_slots.push(page);
        if !known {
            self.indexed_pages.insert(page, slot);
        }
        Ok(())
    }

    /// Mirrors the on-disk swap that overwrote `slot` with the array's last
    /// entry, `moved`.
    ///
    /// Allocation-free by construction: the only map mutations are a removal
    /// and an in-place value update, so this can be used to resynchronise the
    /// mirror after a partially applied removal without any chance of failing
    /// a second time.
    fn mirror_index_swap(&mut self, page: u64, slot: u64, moved: u64, last: u64) {
        self.indexed_pages.remove(&page);
        if slot == last {
            return;
        }
        if let Ok(slot_index) = usize::try_from(slot)
            && slot_index < self.index_slots.len()
        {
            self.index_slots[slot_index] = moved;
        }
        if let Some(held) = self.indexed_pages.get_mut(&moved)
            && *held == last
        {
            *held = slot;
        }
    }

    /// Mirrors the occupancy header shrinking by one.
    fn mirror_index_pop(&mut self) {
        self.index_slots.pop();
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
            Some(page) if page.bytes.len() == len => Ok(page.bytes.as_slice()),
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
            Some(page) => page
                .bytes
                .get(within)
                .copied()
                .ok_or(Error::InvalidMatrixLayout),
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

    /// Set bits the page holding `index` would carry if `value` were written.
    ///
    /// `O(1)`: the page keeps a running count, so neither the page nor the map
    /// is scanned. A result of zero means the page would still be
    /// indistinguishable from the zero page, which is what lets a mutation skip
    /// both residency and a persisted page-index entry.
    fn page_ones_after(&self, index: u64, value: u8) -> Result<u64> {
        if index >= self.byte_len {
            return Err(Error::InvalidMatrixLayout);
        }
        let current = self.byte(index)?;
        let page_ones = self
            .pages
            .get(&(index / BITMAP_PAGE_BYTES))
            .map(|page| page.ones)
            .unwrap_or(0);
        page_ones
            .checked_add(u64::from(value.count_ones()))
            .and_then(|ones| ones.checked_sub(u64::from(current.count_ones())))
            .ok_or(Error::InvalidMatrixLayout)
    }

    /// Resolves everything one byte write needs that can fail, without making
    /// the write visible (F-03, invariant 3).
    ///
    /// The first write to a page has to allocate that page and reserve a map
    /// slot for it, and both of those can fail. Doing them here, before the
    /// caller's disk mutation, is what lets [`Self::commit_byte_write`] be
    /// infallible and allocation-free: a durable bitmap byte can then never be
    /// left disagreeing with an in-memory map that refused to grow.
    ///
    /// The prepared page is *detached* — allocated but not installed — so an
    /// error between preparation and commitment simply drops it. The only
    /// residue is spare capacity in `pages`, which the next successful write
    /// consumes.
    fn prepare_byte_write(&mut self, index: u64, value: u8) -> Result<PreparedByteWrite> {
        let current = self.byte(index)?;
        let page_index = index / BITMAP_PAGE_BYTES;
        let within =
            usize::try_from(index % BITMAP_PAGE_BYTES).map_err(|_| Error::InvalidMatrixLayout)?;
        if current == value {
            return Ok(PreparedByteWrite {
                page: page_index,
                within,
                value,
                fresh: None,
                changes: false,
                materialised: 0,
                page_ones_after: 0,
                ones_after: self.ones,
            });
        }
        let mut materialised = 0;
        let mut fresh = None;
        let mut page_ones = 0;
        match self.pages.get_mut(&page_index) {
            Some(existing) => {
                if within >= existing.bytes.len() {
                    return Err(Error::InvalidMatrixLayout);
                }
                // Take unique ownership now. `Arc::make_mut` clones a shared
                // payload, and a clone is an allocation; doing it here means the
                // `make_mut` after persistence can only ever be a no-op.
                let _ = Arc::make_mut(&mut existing.bytes);
                page_ones = existing.ones;
            }
            None => {
                let page_len = self.page_len(page_index)?;
                if usize_to_u64(within)? >= page_len {
                    return Err(Error::InvalidMatrixLayout);
                }
                #[cfg(any(test, feature = "scalable-fault-injection"))]
                if take_injected_failure(&scaling_counters::BITMAP_PAGE_ALLOCATION_FAILURE) {
                    return Err(Error::AllocationFailed {
                        resource: ReadLimitKey::MatrixBitmapBytes.resource(),
                        requested: page_len,
                    });
                }
                let bytes =
                    filled_bytes_for(page_len, 0, ReadLimitKey::MatrixBitmapBytes.resource())?;
                try_reserve_map(
                    &mut self.pages,
                    1,
                    ReadLimitKey::MatrixBitmapBytes.resource(),
                )?;
                fresh = Some(BitmapPage {
                    bytes: Arc::new(bytes),
                    ones: 0,
                });
                materialised = page_len;
            }
        }
        let page_ones_after = page_ones
            .checked_add(u64::from(value.count_ones()))
            .and_then(|ones| ones.checked_sub(u64::from(current.count_ones())))
            .ok_or(Error::InvalidMatrixLayout)?;
        let ones_after = self
            .ones
            .checked_add(u64::from(value.count_ones()))
            .and_then(|ones| ones.checked_sub(u64::from(current.count_ones())))
            .ok_or(Error::InvalidMatrixLayout)?;
        Ok(PreparedByteWrite {
            page: page_index,
            within,
            value,
            fresh,
            changes: true,
            materialised,
            page_ones_after,
            ones_after,
        })
    }

    /// Installs a prepared byte write and reports the residency it took and gave
    /// back.
    ///
    /// Infallible and allocation-free by construction: the page was allocated
    /// and the map slot reserved by [`Self::prepare_byte_write`], the payload is
    /// uniquely owned, and every count was computed there. This is the step a
    /// caller runs *after* its disk mutation is durable, which is what keeps
    /// disk and memory in agreement whatever the allocator does (F-03).
    ///
    /// The first write to a page materialises it; the write that clears the
    /// page's final set bit releases it again (PERF-02). The zero check is the
    /// page's maintained set-bit count, so neither the page nor the map is ever
    /// scanned on the hot path.
    fn commit_byte_write(&mut self, prepared: PreparedByteWrite) -> PageResidencyDelta {
        if !prepared.changes {
            return PageResidencyDelta::NONE;
        }
        if let Some(page) = prepared.fresh {
            self.pages.insert(prepared.page, page);
        }
        let Some(page) = self.pages.get_mut(&prepared.page) else {
            // Unreachable: preparation either found the page or built one.
            return PageResidencyDelta::NONE;
        };
        if let Some(slot) = Arc::make_mut(&mut page.bytes).get_mut(prepared.within) {
            *slot = prepared.value;
        }
        page.ones = prepared.page_ones_after;
        self.ones = prepared.ones_after;
        if prepared.page_ones_after == 0 {
            // The page is now byte-for-byte the zero page that a non-resident
            // page already answers with, so holding it would be pure overhead.
            let released = self
                .pages
                .remove(&prepared.page)
                .map(|page| usize_to_u64(page.bytes.len()).unwrap_or(0))
                .unwrap_or(0);
            return PageResidencyDelta {
                materialised: prepared.materialised,
                released,
            };
        }
        PageResidencyDelta::materialised(prepared.materialised)
    }

    /// Prepare-then-commit in one step.
    ///
    /// `#[cfg(test)]` deliberately: every production mutation has a disk write
    /// to order against, so it must hold the two halves apart (F-03). Only the
    /// bitmap's own unit tests, which have no file at all, use the shorthand.
    #[cfg(test)]
    fn set_byte(&mut self, index: u64, value: u8) -> Result<PageResidencyDelta> {
        let prepared = self.prepare_byte_write(index, value)?;
        Ok(self.commit_byte_write(prepared))
    }

    /// The byte value writing `value` at `ordinal` would produce.
    fn byte_after_set(&self, ordinal: u64, value: bool) -> Result<(u64, u8)> {
        if ordinal >= self.bit_count {
            return Err(Error::InvalidMatrixLayout);
        }
        let index = ordinal / 8;
        let mask = 1u8 << (ordinal % 8);
        let current = self.byte(index)?;
        Ok((
            index,
            if value {
                current | mask
            } else {
                current & !mask
            },
        ))
    }

    fn prepare_set(&mut self, ordinal: u64, value: bool) -> Result<PreparedByteWrite> {
        let (index, next) = self.byte_after_set(ordinal, value)?;
        self.prepare_byte_write(index, next)
    }

    #[cfg(test)]
    fn set(&mut self, ordinal: u64, value: bool) -> Result<PageResidencyDelta> {
        let (index, next) = self.byte_after_set(ordinal, value)?;
        self.set_byte(index, next)
    }

    // Loading never materialises an all-zero page, so residency after open is
    // proportional to the pages that carry state rather than to the cell count.
    fn insert_loaded_page(&mut self, page: u64, bytes: Vec<u8>) -> Result<u64> {
        let page_len = self.page_len(page)?;
        if usize_to_u64(bytes.len())? != page_len {
            return Err(Error::InvalidMatrixLayout);
        }
        // A page index recovered from a crash-interrupted append may name the
        // same page twice; loading it twice must not double-count its bits.
        if self.pages.contains_key(&page) {
            return Ok(0);
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
        self.pages.insert(
            page,
            BitmapPage {
                bytes: Arc::new(bytes),
                ones,
            },
        );
        self.ones = self
            .ones
            .checked_add(ones)
            .ok_or(Error::InvalidMatrixLayout)?;
        Ok(page_len)
    }

    /// Drops every page *and* the persisted-index tracking, which is what a
    /// whole-map clear or rebuild does on disk as well.
    fn clear(&mut self) {
        self.pages.clear();
        self.ones = 0;
        self.indexed_pages.clear();
        self.index_slots.clear();
    }

    fn ones(&self) -> u64 {
        self.ones
    }

    fn resident_bytes(&self) -> u64 {
        self.pages
            .values()
            .map(|page| page.bytes.len() as u64)
            .fold(0u64, u64::saturating_add)
    }

    /// Modelled resident cost of this bitmap's page-index tracking (F-03).
    fn resident_index_bytes(&self) -> u64 {
        let slots = usize_to_u64(self.index_slots.len()).unwrap_or(u64::MAX);
        let distinct = usize_to_u64(self.indexed_pages.len()).unwrap_or(u64::MAX);
        slots
            .saturating_mul(PAGE_INDEX_SLOT_RESIDENT_BYTES)
            .saturating_add(distinct.saturating_mul(PAGE_INDEX_MAP_RESIDENT_BYTES))
    }
}

/// F-04, SHAPE B, mechanical enforcement: the CRC-validity bitmap and the proof
/// that it is *complete* evidence, held in one type whose bitmap is unnameable
/// from the rest of this file.
///
/// The validity bitmap answers "is the stored CRC for this cell meaningful". A
/// **clear** bit therefore has two possible meanings, and the file cannot tell
/// them apart on its own:
///
/// * nothing has ever established a CRC for that cell — the honest reading, and
/// * the page carrying that bit was never loaded, because the block's validity
///   page index was damaged and the load could not enumerate it in full.
///
/// A recovery path that reads a clear bit as *proof of absence* and republishes
/// the result — `rebuild_commit_map_from_crc` — therefore erases committed cells
/// using evidence the file never supplied. That was F-04.
///
/// Round 12 fixed it with a `crc_valid_complete: bool` beside the bitmap and one
/// added call to a checking function. Re-verification found that fix *partial*,
/// and rightly: the flag bound nothing. A new consumer that wrote
/// `block.crc_valid_bits.get(ordinal)?` compiled cleanly, which is exactly the
/// "the next function will simply fail to repeat the check" shape this round
/// exists to eliminate.
///
/// So the capability is gone, not merely guarded. Outside this module there is
/// **no** operation on the validity bitmap that yields a `bool`:
///
/// * [`CrcValidEvidence::complete`] is the only producer of a
///   [`CompleteCrcValidEvidence`], and it is the only witness whose
///   [`CompleteCrcValidEvidence::get`] returns a bit. Producing it *is* the
///   completeness refusal, so a rebuild written next month cannot address the
///   evidence without having made the check — the compiler refuses, not review.
/// * [`CrcValidEvidence::require_meaningful`] is the fail-closed read, for
///   callers that must refuse on an absent bit rather than publish from it
///   (`verify_cell_crc`). It returns `Result<()>`: it hands back no value to
///   branch on, so it cannot be used to build a published commit view. The one
///   remaining laundering route — `.is_ok()` on its result — is not expressible
///   without a deliberate act, and
///   `crates/varve/tests/enforcement_gates.rs::the_crc_validity_bitmap_is_only_read_through_its_evidence_type`
///   fails if a second call site appears at all.
/// * every other method is accounting (`resident_bytes`), geometry
///   (`byte_len`, `page_count`) or mutation (`prepare_update`,
///   `commit_byte_write`, `clear`) — none of them reports the state of a bit.
///
/// The single escape is [`CrcValidEvidence::page_index_mirror_mut`], which lends
/// the bitmap to the shared page-index maintenance helpers (they take
/// `&mut SparseBitmap` because commit maps use them too). It is named, it has
/// one call site, and the same source gate holds it to one.
/// Mechanical enforcement for **shape B** on the matrix's fatal-recovery gate.
///
/// A `Fatal` recovery finding blocks safe access to matrix state unless the
/// spec opted into forensic access. Until now that was a `fatal_access_blocked:
/// bool` field consulted by an `ensure_fatal_access_allowed` helper at nine
/// call sites plus two more inside `ensure_commit_publishable` and
/// `prepare_commit_bit` — exactly the configuration F-05 was found in, one file
/// over: a guard that protects the operations that remember to call it, and
/// nothing else. Re-verification of round 12 named it as an unclosed instance
/// of the class the round exists for, and noted that the sweep which produced
/// the shape-B inventory could not have found it, because it grepped for the
/// word `poisoned` and this flag has another name.
///
/// The boolean now lives in [`FatalAccessGate`], whose field is unnameable
/// outside this module, and the only thing that can be obtained from it is
/// [`FatalAccessAllowed`] — a zero-sized witness with a private field and one
/// constructor, [`FatalAccessGate::allow`], which *is* the refusal.
///
/// The witness is then demanded by `MatrixLayout::block_index` and
/// `MatrixLayout::commit_index`, the two functions that turn a block id or a
/// commit category into a position in the layout's state. Every operation in
/// this file that reads or publishes a cell, a commit bit or a CRC-validity bit
/// resolves its target through one of them, so a matrix accessor written next
/// month cannot address the state it wants to touch without having made the
/// check.
///
/// **What this does not do, stated rather than implied.** `MatrixLayout` is
/// declared in this file, so its `blocks` and `commits` vectors can still be
/// indexed directly by code that already holds a position — that is how the
/// existing prepare/commit pairs pass a resolved index between halves of one
/// operation, and putting the layout itself behind a module boundary would
/// touch some seventy call sites without adding a property, since a position
/// can only be *obtained* through the two gated resolvers. The enforced claim
/// is therefore precise: no new code path can go from an identifier to matrix
/// state without the refusal running.
pub(crate) mod fatal_access {
    use super::{Error, Result};

    /// Witness that no `Fatal` recovery finding blocks this access.
    ///
    /// Zero-sized, private field, single constructor. Held by reference by the
    /// resolvers, because one check legitimately covers the whole operation
    /// that follows it.
    #[derive(Debug)]
    pub struct FatalAccessAllowed(());

    /// Whether safe access to this layout's state is blocked by a `Fatal`
    /// recovery finding, and the sole source of [`FatalAccessAllowed`].
    #[derive(Clone, Debug)]
    pub struct FatalAccessGate {
        blocked: bool,
    }

    impl FatalAccessGate {
        /// Precomputed at open time: `true` when the layout carries a `Fatal`
        /// finding and the spec did not opt into forensic access.
        pub(crate) fn new(blocked: bool) -> Self {
            Self { blocked }
        }

        /// The fail-closed refusal, and the only way to obtain the witness.
        pub(crate) fn allow(&self) -> Result<FatalAccessAllowed> {
            if self.blocked {
                return Err(Error::MatrixFatalCorruption);
            }
            Ok(FatalAccessAllowed(()))
        }

        /// Blocks safe access from here on. Not reversible: a rebuild that was
        /// interrupted after marking the persisted page index leaves state no
        /// safe accessor may consume, and reopening with forensic access is the
        /// deliberate, explicitly named route to reading it anyway.
        pub(crate) fn block(&mut self) {
            self.blocked = true;
        }
    }
}

pub(crate) mod crc_valid_evidence {
    use super::{
        BitmapByteUpdate, Error, PageResidencyDelta, PreparedByteWrite, Result, SparseBitmap,
        prepare_bitmap_update,
    };

    /// A block's CRC-validity bitmap together with whether it is complete.
    #[derive(Clone, Debug)]
    pub struct CrcValidEvidence {
        bits: SparseBitmap,
        /// False when this block's validity page index could not be enumerated
        /// in full, so the bitmap is a *subset* of the published evidence and a
        /// clear bit outside the loaded pages means nothing.
        complete: bool,
    }

    /// Proof that the validity bitmap it borrows was enumerated in full.
    ///
    /// The only way to obtain one is `CrcValidEvidence::complete`, which is
    /// the refusal. There is no other constructor, and the borrowed field is
    /// private, so this witness cannot be fabricated — see
    /// `crates/varve/tests/ui/fail_fabricated_crc_valid_completeness.rs`.
    #[must_use = "the witness is the completeness proof; obtaining and dropping \
                  it discards the only reason a clear bit may be read as absence"]
    pub struct CompleteCrcValidEvidence<'a> {
        bits: &'a SparseBitmap,
    }

    impl CrcValidEvidence {
        pub(super) fn new(bits: SparseBitmap, complete: bool) -> Self {
            Self { bits, complete }
        }

        /// Proves the evidence complete, or refuses.
        ///
        /// This is the F-04 refusal, and it is deliberately unconditional —
        /// exactly like `poison_interrupted_rebuild`. `matrix_fatal_forensics`
        /// relaxes *reading* fatal state; consuming absent evidence as proof is
        /// a destructive republication, not a read. Discarding unverifiable
        /// visibility on purpose is a different operation from recovering it and
        /// has to be asked for by name rather than obtained by default.
        ///
        /// The refusal reuses [`Error::MatrixFatalCorruption`] because that is
        /// what this is — a fatal recovery finding blocking an operation. Its
        /// message's pointer at forensic access is unhelpful for a caller that
        /// already enabled it; a dedicated variant belongs in `error.rs`
        /// (checklist open item 16).
        pub(super) fn complete(&self) -> Result<CompleteCrcValidEvidence<'_>> {
            if !self.complete {
                return Err(Error::MatrixFatalCorruption);
            }
            Ok(CompleteCrcValidEvidence { bits: &self.bits })
        }

        /// Fail-closed read: `Ok(())` when the cell's CRC is known meaningful,
        /// otherwise the caller's refusal.
        ///
        /// Safe on incomplete evidence *because it yields nothing*: an omitted
        /// page reads as "not established", which for this shape of caller means
        /// refuse, never publish. Callers that need the bit as data must go
        /// through [`Self::complete`].
        pub(super) fn require_meaningful(
            &self,
            ordinal: u64,
            absent: impl FnOnce() -> Error,
        ) -> Result<()> {
            if self.bits.get(ordinal)? {
                Ok(())
            } else {
                Err(absent())
            }
        }

        pub(super) fn byte_len(&self) -> u64 {
            self.bits.byte_len
        }

        pub(super) fn page_count(&self) -> u64 {
            self.bits.page_count
        }

        pub(super) fn resident_bytes(&self) -> u64 {
            self.bits.resident_bytes()
        }

        pub(super) fn resident_index_bytes(&self) -> u64 {
            self.bits.resident_index_bytes()
        }

        /// Drops every page and the persisted-index tracking, mirroring what a
        /// whole-map clear does on disk.
        ///
        /// Completeness is deliberately *not* reset: a cleared map is not
        /// evidence that was re-enumerated, and a session that opened on a
        /// damaged validity index still has no basis for reading absence as
        /// proof. The state is fail-closed until the next open (checklist open
        /// item 24).
        pub(super) fn clear(&mut self) {
            self.bits.clear();
        }

        pub(super) fn prepare_update(
            &self,
            bit_count: u64,
            base_offset: u64,
            ordinal: u64,
            value: bool,
        ) -> Result<BitmapByteUpdate> {
            prepare_bitmap_update(&self.bits, bit_count, base_offset, ordinal, value)
        }

        pub(super) fn materialisation_cost(&self, index: u64, value: u8) -> Result<u64> {
            self.bits.materialisation_cost(index, value)
        }

        pub(super) fn prepare_byte_write(
            &mut self,
            index: u64,
            value: u8,
        ) -> Result<PreparedByteWrite> {
            self.bits.prepare_byte_write(index, value)
        }

        pub(super) fn commit_byte_write(
            &mut self,
            prepared: PreparedByteWrite,
        ) -> PageResidencyDelta {
            self.bits.commit_byte_write(prepared)
        }

        /// The one named escape: page-index maintenance takes
        /// `&mut SparseBitmap` because commit maps share the same helpers.
        ///
        /// It must keep exactly one call site (`page_index_target`), which the
        /// source gate asserts. Every additional one is a fresh route to
        /// `SparseBitmap::get`, i.e. to F-04.
        pub(super) fn page_index_mirror_mut(&mut self) -> &mut SparseBitmap {
            &mut self.bits
        }
    }

    impl CompleteCrcValidEvidence<'_> {
        /// Reads a validity bit as evidence, which the witness makes sound.
        pub(super) fn get(&self, ordinal: u64) -> Result<bool> {
            self.bits.get(ordinal)
        }
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
/// F-03: page-index tracking is charged here too, so
/// `ReadLimitKey::MatrixBitmapBytes` bounds the whole resident matrix bitmap
/// footprint — payload pages *and* the structure that names them — rather than
/// leaving the index outside every runtime limit. The two terms are kept apart
/// only so the existing payload-residency contracts stay observable; every
/// admission decision is made on their sum.
#[derive(Clone, Copy, Debug)]
struct ResidentBitmapBudget {
    limits: ReadLimits,
    pages: u64,
    index: u64,
}

impl ResidentBitmapBudget {
    fn new(limits: ReadLimits) -> Self {
        Self {
            limits,
            pages: 0,
            index: 0,
        }
    }

    fn resume(limits: ReadLimits, pages: u64, index: u64) -> Self {
        Self {
            limits,
            pages,
            index,
        }
    }

    fn charge(&mut self, bytes: u64) -> Result<()> {
        if bytes == 0 {
            return Ok(());
        }
        let next = self
            .pages
            .checked_add(bytes)
            .ok_or(Error::ResourceArithmeticOverflow {
                resource: ReadLimitKey::MatrixBitmapBytes.resource(),
            })?;
        let total = next
            .checked_add(self.index)
            .ok_or(Error::ResourceArithmeticOverflow {
                resource: ReadLimitKey::MatrixBitmapBytes.resource(),
            })?;
        self.limits.check(ReadLimitKey::MatrixBitmapBytes, total)?;
        self.pages = next;
        Ok(())
    }

    fn release(&mut self, bytes: u64) {
        self.pages = self.pages.saturating_sub(bytes);
    }

    /// Charges page-index tracking before the memory is taken.
    fn charge_index(&mut self, bytes: u64) -> Result<()> {
        if bytes == 0 {
            return Ok(());
        }
        let next = self
            .index
            .checked_add(bytes)
            .ok_or(Error::ResourceArithmeticOverflow {
                resource: ReadLimitKey::MatrixBitmapBytes.resource(),
            })?;
        let total = next
            .checked_add(self.pages)
            .ok_or(Error::ResourceArithmeticOverflow {
                resource: ReadLimitKey::MatrixBitmapBytes.resource(),
            })?;
        self.limits.check(ReadLimitKey::MatrixBitmapBytes, total)?;
        self.index = next;
        Ok(())
    }

    fn release_index(&mut self, bytes: u64) {
        self.index = self.index.saturating_sub(bytes);
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

/// On-disk size of one bitmap's persisted page index: an occupancy header slot
/// plus one entry per page.
///
/// That is `8` bytes per `4096` bytes of bitmap plus a fixed `8`, i.e. about
/// `1/512` of the map it describes and the same order as the page-digest array
/// beside it. Like both of those regions it is established as a zero extent —
/// which decodes as "no entries" — and is only ever written where a page is
/// actually published.
fn index_page_array_len(page_count: u64) -> Result<u64> {
    if page_count > PAGE_INDEX_MAX_ENTRIES {
        // The occupancy header cannot represent this many entries, so the
        // matrix is refused at layout time rather than written with a header
        // that could not be validated on the way back in.
        return Err(Error::InvalidMatrixLayout);
    }
    page_count
        .checked_add(PAGE_INDEX_HEADER_SLOTS)
        .and_then(|slots| slots.checked_mul(PAGE_INDEX_ENTRY_LEN))
        .ok_or(Error::InvalidMatrixLayout)
}

/// Check bits folded into the page-index occupancy header.
///
/// `check(0) == 0`, so a never-written (all-zero) header slot decodes as an
/// empty index and a freshly created matrix needs no explicit write. Every
/// nonzero count produces a nonzero header, so "zero header" and "count zero"
/// are the same statement rather than two indistinguishable ones.
const fn page_index_header_check(count: u64) -> u64 {
    (count.wrapping_mul(PAGE_INDEX_HEADER_MIX) >> 48) << 48
}

/// Encodes an occupancy count into the page index's header slot.
fn page_index_header_value(count: u64) -> Result<u64> {
    if count > PAGE_INDEX_MAX_ENTRIES {
        return Err(Error::InvalidMatrixLayout);
    }
    Ok(count | page_index_header_check(count))
}

/// Decodes the page index's header slot, or `None` when it is damaged.
///
/// This is a redundancy code over a single 8-byte slot, so it catches a torn
/// write or a bit flip; it is explicitly not authentication against an actor
/// who can rewrite the file, exactly as [`write_page_digest`] records for the
/// digest array.
fn page_index_header_count(value: u64, capacity: u64) -> Option<u64> {
    let count = value & PAGE_INDEX_MAX_ENTRIES;
    if value != count | page_index_header_check(count) || count > capacity {
        return None;
    }
    Some(count)
}

fn page_index_len(byte_len: u64) -> Result<u64> {
    index_page_array_len(page_count_for(byte_len)?)
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
    #[cfg(test)]
    if FAIL_NEXT_PAGE_DIGEST_WRITE.replace(false) {
        return Err(Error::Io(std::io::Error::other(
            "injected matrix page digest write failure",
        )));
    }

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
    /// Modelled resident cost of every persisted page index this layout tracks
    /// (F-03). Charged against `ReadLimitKey::MatrixBitmapBytes` together with
    /// `resident_bitmap_bytes`, and released as pages leave the live set.
    resident_page_index_bytes: u64,
    // Precomputed at open time so accessors gate on a single flag instead of
    // scanning findings per cell access. Shape B, mechanical enforcement: the
    // boolean itself is unnameable outside `mod fatal_access`, and the only
    // thing that can be done with it is to ask for the witness that addressing
    // matrix state demands. See that module for what that forbids.
    fatal_access: FatalAccessGate,
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
    /// Base offset of this category's persisted page index (PERF-01).
    index_offset: u64,
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
    /// Base offset of the validity bitmap's persisted page index (PERF-01).
    crc_valid_index_offset: Option<u64>,
    /// The CRC-validity bitmap *and* whether it is complete evidence (F-04).
    ///
    /// This is not a `SparseBitmap`: the bitmap is unnameable outside
    /// [`mod crc_valid_evidence`](crc_valid_evidence), so no path can read a
    /// clear bit as proof of absence without first obtaining the completeness
    /// witness. Round 12 tracked the same fact in a `bool` beside the bitmap,
    /// which bound nothing.
    crc_valid_bits: CrcValidEvidence,
    current_write_bits: SparseBitmap,
}

#[derive(Clone, Debug)]
struct MatrixAuxLayout {
    name: String,
    offset: u64,
    byte_len: u64,
}

/// Where each paged bitmap's persisted page index lives (PERF-01).
///
/// Region layout: one entry array per commit category, in category order, then
/// — when checksums are enabled, because that is when the validity bitmaps
/// exist — one per matrix block, in block order.
#[derive(Clone, Debug)]
struct MatrixPageIndexLayout {
    commit_offsets: Vec<u64>,
    block_valid_offsets: Vec<u64>,
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

    /// The resident bitmap budget this layout currently holds.
    ///
    /// `ResidentBitmapBudget` is `Copy`, so a mutation path can take it out,
    /// charge against it while holding a mutable borrow of one bitmap, and
    /// write it back with [`Self::adopt_budget`].
    fn budget(&self) -> ResidentBitmapBudget {
        ResidentBitmapBudget::resume(
            self.read_limits,
            self.resident_bitmap_bytes,
            self.resident_page_index_bytes,
        )
    }

    fn adopt_budget(&mut self, budget: ResidentBitmapBudget) {
        self.resident_bitmap_bytes = budget.pages;
        self.resident_page_index_bytes = budget.index;
        record_resident_bitmap_bytes(budget.pages);
        record_resident_page_index_bytes(budget.index);
    }

    /// Charges newly resident bitmap bytes against
    /// `ReadLimitKey::MatrixBitmapBytes` before the memory is taken.
    ///
    /// F-03: the limit is applied to the payload pages *and* the page-index
    /// tracking that names them, so no part of the resident matrix bitmap
    /// footprint sits outside a runtime resource limit.
    fn charge_resident_bitmap(&mut self, bytes: u64) -> Result<()> {
        if bytes == 0 {
            return Ok(());
        }
        let mut budget = self.budget();
        budget.charge(bytes)?;
        self.adopt_budget(budget);
        Ok(())
    }

    /// Gives back residency a mutation released, so a set/clear cycle returns
    /// to the budget it started from (PERF-02).
    fn release_resident_bitmap(&mut self, bytes: u64) {
        if bytes == 0 {
            return;
        }
        self.resident_bitmap_bytes = self.resident_bitmap_bytes.saturating_sub(bytes);
        record_resident_bitmap_bytes(self.resident_bitmap_bytes);
    }

    /// Applies one bitmap mutation's residency delta. The growth was charged
    /// before the memory was taken; only the refund is left.
    fn settle_page_delta(&mut self, delta: PageResidencyDelta) {
        self.release_resident_bitmap(delta.released);
    }

    // Fail-closed gate for `Fatal` recovery findings: safe accessors must not
    // consume fatal-state data unless the spec opted into forensic access.
    //
    // Returns the witness rather than `()`, because every route from a block id
    // or a commit category to the layout's state — `block_index` and
    // `commit_index` — demands one. See `mod fatal_access`.
    fn ensure_fatal_access_allowed(&self) -> Result<FatalAccessAllowed> {
        self.fatal_access.allow()
    }

    pub fn dimension(&self, name: &str) -> Option<u64> {
        self.dimensions
            .iter()
            .find(|value| value.name == name)
            .map(|value| value.value)
    }

    /// Resolves a block id against this layout. Takes the fatal-access witness
    /// by reference: this is one of the two ways to turn an identifier into a
    /// position in the layout's state, so demanding the witness here is what
    /// makes the fail-closed refusal a precondition of *addressing* matrix
    /// state rather than a line each accessor has to remember.
    fn block_index(&self, _allowed: &FatalAccessAllowed, block_id: u32) -> Result<usize> {
        self.blocks
            .iter()
            .position(|block| block.block_id == block_id)
            .ok_or(Error::MatrixBlockMissing(block_id))
    }

    /// Resolves a commit category against this layout. See [`Self::block_index`]
    /// for why it takes the witness.
    fn commit_index(&self, _allowed: &FatalAccessAllowed, name: &str) -> Result<usize> {
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
    let page_index_off = aux_region_end;
    let page_index_region_len =
        page_index_table_len(spec, crc_enabled, &commit_plans, &cell_counts)?;
    let page_index_end = page_index_off
        .checked_add(page_index_region_len)
        .ok_or(Error::InvalidMatrixLayout)?;
    let region_crc_off = if crc_enabled { page_index_end } else { 0 };
    let append_log_start = if crc_enabled {
        region_crc_off
            .checked_add(region_crc_len)
            .ok_or(Error::InvalidMatrixLayout)?
    } else {
        page_index_end
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
    let page_index_layout = page_index_layout_from_parts(
        page_index_off,
        page_index_region_len,
        spec,
        crc_enabled,
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
        page_index_off,
        page_index_len: page_index_region_len,
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
        page_index_layout,
        initial_crc_valid_bits,
        HashMap::new(),
        Vec::new(),
        append_log_start,
        resident_bitmap_bytes,
        // Creation publishes no page, so no page-index entry is tracked yet.
        0,
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
    let page_index_layout = page_index_layout_from_parts(
        header.page_index_off,
        header.page_index_len,
        spec,
        crc_enabled,
        &expected_commit_plans,
        &block_offsets,
    )?;
    let mut crc_verification = verify_crc_header(
        file,
        crc_layout.as_ref(),
        &[&dimension_table, &block_table, &category_table],
        commit_plans.len(),
    )?;
    // PERF-01/PERF-02: the persisted page index names the pages this matrix has
    // ever published, and one allocation-map query names the pages the
    // filesystem says hold bytes. Open visits their union, so it never loops
    // over logical pages and never degrades to full logical bitmap I/O when the
    // platform cannot answer.
    let extents = AllocatedExtents::query(file);
    record_open_allocation_map(extents.as_ref());
    let mut budget = ResidentBitmapBudget::new(spec.read_limits);
    let commit_bits = load_commit_bitmaps(
        file,
        crc_layout.as_ref(),
        &page_index_layout,
        &commit_plans,
        extents.as_ref(),
        &mut budget,
        &mut crc_verification,
    )?;
    let crc_valid_bits = load_crc_valid_bits(
        spec,
        file,
        crc_layout.as_ref(),
        &page_index_layout,
        &block_offsets,
        extents.as_ref(),
        &mut PagedBitmapSink {
            budget: &mut budget,
            findings: &mut crc_verification.findings,
        },
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
        page_index_layout,
        crc_valid_bits,
        crc_verification.commit_findings,
        crc_verification.findings,
        header.append_log_start,
        budget.pages,
        budget.index,
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
    let allowed = ensure_commit_publishable(layout, T::CATEGORY)?;
    let block_index = layout.block_index(&allowed, T::ID)?;
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
    let write_bit = prepare_current_write_bit(layout, block_index, ordinal)?;

    let persisted = (|| -> Result<()> {
        apply_commit_bit(layout, file, commit_index, commit_update)?;
        if let Some(update) = crc_valid_update {
            apply_cell_crc_valid(layout, file, block_index, update)?;
        }
        file.seek(SeekFrom::Start(offset))?;
        write_slot_payload(file, &payload)
    })();
    match persisted {
        Ok(()) => {
            commit_current_write_bit(layout, block_index, write_bit);
            Ok(())
        }
        Err(err) => {
            abandon_current_write_bit(layout, write_bit);
            Err(err)
        }
    }
}

/// The session write-tracking bit for `ordinal`: charged and allocated, but not
/// yet installed.
struct PreparedWriteBit {
    cost: u64,
    write: PreparedByteWrite,
}

/// Charges and allocates the session write-tracking bit for `ordinal` *before*
/// the slot payload it records is written (invariant 3).
///
/// This used to run after the payload write, so a matrix write could return
/// `LimitExceeded` — which entitles the caller to believe nothing happened —
/// with the commit bit, the validity bit and the payload already durable. The
/// bit is now the last, infallible step of the whole cell write, and the limit
/// is refused before anything reaches the file.
fn prepare_current_write_bit(
    layout: &mut MatrixLayout,
    block_index: usize,
    ordinal: u64,
) -> Result<PreparedWriteBit> {
    let index = ordinal / 8;
    let byte = layout.blocks[block_index].current_write_bits.byte(index)?;
    let cost = layout.blocks[block_index]
        .current_write_bits
        .materialisation_cost(index, byte | (1u8 << (ordinal % 8)))?;
    layout.charge_resident_bitmap(cost)?;
    match layout.blocks[block_index]
        .current_write_bits
        .prepare_set(ordinal, true)
    {
        Ok(write) => Ok(PreparedWriteBit { cost, write }),
        Err(err) => {
            layout.release_resident_bitmap(cost);
            Err(err)
        }
    }
}

/// Installs a prepared session write-tracking bit. Infallible.
fn commit_current_write_bit(
    layout: &mut MatrixLayout,
    block_index: usize,
    prepared: PreparedWriteBit,
) {
    let delta = layout.blocks[block_index]
        .current_write_bits
        .commit_byte_write(prepared.write);
    layout.settle_page_delta(delta);
}

/// Gives back a prepared bit's charge when the write it accompanied failed.
fn abandon_current_write_bit(layout: &mut MatrixLayout, prepared: PreparedWriteBit) {
    layout.release_resident_bitmap(prepared.cost);
}

pub(crate) fn write_cell_payload<T: VarveMatrixBlock>(
    spec: FormatSpec,
    layout: &mut MatrixLayout,
    file: &mut File,
    key: MatrixKey,
    payload: &[u8],
) -> Result<()> {
    ensure_matrix_block::<T>(spec)?;
    let allowed = ensure_commit_publishable(layout, T::CATEGORY)?;
    let block_index = layout.block_index(&allowed, T::ID)?;
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
    let write_bit = prepare_current_write_bit(layout, block_index, ordinal)?;

    let persisted = (|| -> Result<()> {
        apply_commit_bit(layout, file, commit_index, commit_update)?;
        if let Some(update) = crc_valid_update {
            apply_cell_crc_valid(layout, file, block_index, update)?;
        }
        file.seek(SeekFrom::Start(offset))?;
        write_slot_payload(file, payload)
    })();
    match persisted {
        Ok(()) => {
            commit_current_write_bit(layout, block_index, write_bit);
            Ok(())
        }
        Err(err) => {
            abandon_current_write_bit(layout, write_bit);
            Err(err)
        }
    }
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
    // `cell_status` above has already made the fail-closed refusal; taking the
    // witness here is what lets this path address the block at all.
    let allowed = layout.ensure_fatal_access_allowed()?;
    let block_index = layout.block_index(&allowed, T::ID)?;
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
    let allowed = ensure_commit_publishable(layout, T::CATEGORY)?;
    let block_index = layout.block_index(&allowed, T::ID)?;
    let ordinal = layout.ordinal_for_block(block_index, key)?;
    let commit_index = layout.commit_index(&allowed, T::CATEGORY)?;
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
    let allowed = ensure_commit_publishable(layout, T::CATEGORY)?;
    let block_index = layout.block_index(&allowed, T::ID)?;
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
    let allowed = ensure_commit_publishable(layout, T::CATEGORY)?;
    let block_index = layout.block_index(&allowed, T::ID)?;
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
    let allowed = ensure_commit_publishable(layout, category)?;
    let block_index = block_index_for_category(spec, layout, &allowed, category)?;
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
    let allowed = layout.ensure_fatal_access_allowed()?;
    let commit_index = layout.commit_index(&allowed, category)?;
    let cleared = count_committed(&layout.commits[commit_index])?;
    let commit_kind = layout.commits[commit_index].kind;
    let map_offset = layout.commits[commit_index].map_offset;
    let digest_offset = layout.commits[commit_index].digest_offset;
    let map_len = layout.commits[commit_index].bits.byte_len;
    let page_count = layout.commits[commit_index].bits.page_count;
    let index_offset = layout.commits[commit_index].index_offset;
    let cleared_valid = if commit_kind == MatrixCommitKind::Cell {
        let block_index = block_index_for_category(spec, layout, &allowed, category)?;
        layout.blocks[block_index]
            .crc_valid_offset
            .map(|valid_offset| (block_index, valid_offset))
    } else {
        None
    };

    // Everything the category held becomes non-resident again: the commit
    // bitmap's payload pages, the quarantined copy of that map, the validity
    // bitmap this clear drops with them, and the page-index tracking of all
    // three. F-01: both budget terms are totalled *before* anything is cleared,
    // because `SparseBitmap::clear` drops the pages and the index mirror the
    // totals are derived from, and refunding only the payload term left the
    // page-index charge — and the validity bitmap's payload charge — standing
    // for memory that had already been released. Repeated populate/clear cycles
    // then accumulated phantom residency until the budget refused a matrix that
    // held nothing.
    let mut released_pages = layout.commits[commit_index]
        .bits
        .resident_bytes()
        .checked_add(
            layout.commits[commit_index]
                .quarantined_raw_bits
                .as_ref()
                .map(SparseBitmap::resident_bytes)
                .unwrap_or(0),
        )
        .ok_or(Error::InvalidMatrixLayout)?;
    let mut released_index = layout.commits[commit_index]
        .bits
        .resident_index_bytes()
        .checked_add(
            layout.commits[commit_index]
                .quarantined_raw_bits
                .as_ref()
                .map(SparseBitmap::resident_index_bytes)
                .unwrap_or(0),
        )
        .ok_or(Error::InvalidMatrixLayout)?;
    if let Some((block_index, _)) = cleared_valid {
        released_pages = released_pages
            .checked_add(layout.blocks[block_index].crc_valid_bits.resident_bytes())
            .ok_or(Error::InvalidMatrixLayout)?;
        released_index = released_index
            .checked_add(
                layout.blocks[block_index]
                    .crc_valid_bits
                    .resident_index_bytes(),
            )
            .ok_or(Error::InvalidMatrixLayout)?;
    }

    // Invariant 3, RULE B: the subtraction that settles the two counters used to
    // run after the regions were zeroed and the maps dropped, so an accounting
    // underflow would have returned `Err` from a clear that had already
    // happened. Both new totals are derived here, before the first disk
    // mutation; the post-clear step is a pair of assignments.
    let next_resident_pages = layout
        .resident_bitmap_bytes
        .checked_sub(released_pages)
        .ok_or(Error::InvalidMatrixLayout)?;
    let next_resident_index = layout
        .resident_page_index_bytes
        .checked_sub(released_index)
        .ok_or(Error::InvalidMatrixLayout)?;

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
            layout.blocks[block_index].crc_valid_bits.byte_len(),
        )?;
        if let Some(valid_index_offset) = layout.blocks[block_index].crc_valid_index_offset {
            zero_range(
                file,
                valid_index_offset,
                index_page_array_len(layout.blocks[block_index].crc_valid_bits.page_count())?,
            )?;
        }
    }
    // The page index describes exactly the published pages, so restoring the
    // post-create zero extent means emptying it too — otherwise every later
    // open would revisit pages the clear has just proven empty.
    //
    // RULE B, and the reason this needs no F-02 rebuild marker: emptying the
    // index before the map it describes is the *safe* direction for a clear.
    // An interruption between the two leaves an index that names nothing and a
    // map whose bytes are unreachable, which is byte-for-byte the outcome the
    // caller asked for. The destructive-republication window F-02 closes exists
    // only where the index has to come back afterwards.
    zero_range(file, index_offset, index_page_array_len(page_count)?)?;
    if let Some(digest_offset) = digest_offset {
        let digest_len = page_count
            .checked_mul(PAGE_DIGEST_LEN)
            .ok_or(Error::InvalidMatrixLayout)?;
        if punch_zero_range(file, digest_offset, digest_len) {
            sparse_zeroing::record(0);
        } else {
            // F-08: this range is zeroed by an explicit per-page digest loop
            // rather than by `zero_range`, so it has to record its own outcome.
            // Without this, a clear whose digest array streamed but whose final
            // map range was removed would report zero streamed bytes.
            sparse_zeroing::record(digest_len);
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
    layout.resident_bitmap_bytes = next_resident_pages;
    layout.resident_page_index_bytes = next_resident_index;
    record_resident_bitmap_bytes(layout.resident_bitmap_bytes);
    record_resident_page_index_bytes(layout.resident_page_index_bytes);
    Ok(cleared)
}

pub(crate) fn commit_event<T: VarveMatrixBlock>(
    spec: FormatSpec,
    layout: &MatrixLayout,
    key: MatrixKey,
) -> Result<MatrixCommitEvent> {
    ensure_matrix_block::<T>(spec)?;
    let allowed = layout.ensure_fatal_access_allowed()?;
    let block_index = layout.block_index(&allowed, T::ID)?;
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
    // `cell_status` above has already made the fail-closed refusal; taking the
    // witness here is what lets this path address the block at all.
    let allowed = layout.ensure_fatal_access_allowed()?;
    let block_index = layout.block_index(&allowed, T::ID)?;
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
    // `cell_status` above has already made the fail-closed refusal; taking the
    // witness here is what lets this path address the block at all.
    let allowed = layout.ensure_fatal_access_allowed()?;
    let block_index = layout.block_index(&allowed, T::ID)?;
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
    let allowed = layout.ensure_fatal_access_allowed()?;
    let commit = &layout.commits[layout.commit_index(&allowed, category)?];
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
    let allowed = layout.ensure_fatal_access_allowed()?;
    layout.read_limits.check(ReadLimitKey::SidecarLen, 0)?;
    let commit = &layout.commits[layout.commit_index(&allowed, category)?];
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
    // F-02: a fatal commit-map finding — an interrupted rebuild marker, a torn
    // occupancy header, or an unusable counted entry — means the persisted page
    // index cannot be trusted to name the published set. The recovery that
    // rewrites it is a commit-map rebuild, so the report has to say so instead
    // of reporting damage with no route out of it.
    if findings.iter().any(|finding| {
        finding.kind == MatrixCorruptionKind::CommitMap
            && finding.severity == MatrixCorruptionSeverity::Fatal
    }) {
        recommended_actions.push(MatrixRecoveryAction::RebuildCommitMap { category: None });
    }
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

/// Fails this layout closed after a whole-map page-index republication was
/// interrupted past the point where it published the rebuild marker (F-02).
///
/// The persisted index no longer describes the published set, and the in-memory
/// mirror still describes the *previous* one, so any later mutation would write
/// the old occupancy count over the new entry region. Both halves are refused
/// until the file is reopened and the rebuild is run again, and the reason is
/// reported rather than left as an unexplained refusal.
///
/// This is deliberately unconditional. A caller that opted into
/// `matrix_fatal_forensics` gets the same refusal here, because forensics
/// relaxes *reading* fatal state, not writing into a half-published index; the
/// route back is to reopen with forensics and rebuild.
fn poison_interrupted_rebuild(layout: &mut MatrixLayout, commit_index: usize) {
    let category = layout.commits[commit_index].name.clone();
    layout.crc_findings.push(MatrixRecoveryFinding {
        kind: MatrixCorruptionKind::CommitMap,
        severity: MatrixCorruptionSeverity::Fatal,
        message: format!(
            "matrix commit-map rebuild for {category} was interrupted after the persisted page \
             index was marked; the index on disk does not name the published set and the rebuild \
             must be run again"
        ),
    });
    layout.fatal_access.block();
}

pub(crate) fn rebuild_commit_map_from_crc<T: VarveMatrixBlock>(
    spec: FormatSpec,
    layout: &mut MatrixLayout,
    file: &mut File,
) -> Result<u64> {
    ensure_matrix_block::<T>(spec)?;
    let allowed = layout.ensure_fatal_access_allowed()?;
    let block_index = layout.block_index(&allowed, T::ID)?;
    let block = &layout.blocks[block_index];
    // F-04: the completeness refusal, and simultaneously the only way to obtain
    // permission to read a validity bit as evidence. It runs before every
    // fallible step of the rebuild and before the first disk mutation, so a
    // refused rebuild leaves the previous commit view exactly as it was.
    //
    // Deliberately after `ensure_fatal_access_allowed`, so a non-forensic caller
    // still sees the ordinary fatal refusal first; this one is reached only in
    // forensic mode, which does not relax it.
    let evidence = block.crc_valid_bits.complete()?;
    let Some(crc_offset) = block.crc_offset else {
        return Err(Error::IntegrityFeatureDisabled);
    };
    let commit_index = layout.commit_index(&allowed, T::CATEGORY)?;
    if layout.commits[commit_index].kind != MatrixCommitKind::Cell
        || layout.commits[commit_index].bit_count != block.cell_count
    {
        return Err(Error::InvalidMatrixLayout);
    }

    // The rebuilt map is materialised page by page and charged the same way,
    // so a rebuild is admitted on the pages it actually needs rather than on
    // the dense worst case of the whole map.
    let mut budget = layout.budget();
    let mut rebuilt = SparseBitmap::new(layout.commits[commit_index].bit_count)?;
    let mut committed = 0u64;
    for ordinal in 0..block.cell_count {
        let slot_offset = layout.slot_offset(block_index, ordinal)?;
        let actual = crc32_file_range(file, slot_offset, block.slot_stride)?;
        let stored = read_crc_at(file, indexed_crc_offset(crc_offset, ordinal)?)?;
        let valid = evidence.get(ordinal)? && actual == stored;
        if valid {
            committed += 1;
        }
        // F-05, invariant 1: the page is charged *before* it is allocated, so
        // the ceiling is a strict pre-allocation limit rather than one checked a
        // page too late. `materialisation_cost` inspects without allocating, and
        // `commit_byte_write` allocates nothing, so the only allocating step
        // sits between the charge and the install. An early return drops
        // `rebuilt` whole and never adopts `budget`, so no charge escapes.
        let (index, next) = rebuilt.byte_after_set(ordinal, valid)?;
        let cost = rebuilt.materialisation_cost(index, next)?;
        budget.charge(cost)?;
        let prepared = rebuilt.prepare_byte_write(index, next)?;
        let delta = rebuilt.commit_byte_write(prepared);
        debug_assert_eq!(delta.materialised, cost);
        budget.release(delta.released);
    }

    let commit = &layout.commits[commit_index];
    let regions = CommitMapRegions {
        map_offset: commit.map_offset,
        digest_offset: commit.digest_offset,
        index_offset: commit.index_offset,
    };
    // Invariant 3, RULE B: this subtraction is the accounting for a publication
    // that has not happened yet, so it is derived — and allowed to fail — before
    // the first destructive write rather than after it. `write_commit_map_pages`
    // moves only the index term of the budget, so the payload total computed
    // here stays correct across it.
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
    let next_resident_pages = budget
        .pages
        .checked_sub(released)
        .ok_or(Error::InvalidMatrixLayout)?;
    let mut destructive = false;
    // F-10: the map being replaced is *borrowed*, not cloned. The clone that
    // used to stand here was an infallible container allocation on a recovery
    // path where every other allocation returns a typed `AllocationFailed`, and
    // `write_commit_map_pages` only ever reads the previous map's indexed-page
    // keys and page count. `ResidentBitmapBudget` is `Copy` and `regions` is
    // already extracted, so nothing else needs `layout` while the borrow is
    // live, and the allocation is removed rather than made fallible.
    if let Err(err) = write_commit_map_pages(
        file,
        regions,
        &layout.commits[commit_index].bits,
        &mut rebuilt,
        &mut budget,
        &mut destructive,
    ) {
        if destructive {
            // F-02: the rebuild marker is on disk and the entry region is an
            // unknown subset of the published pages. The next open refuses it,
            // and this session has to refuse it too — the in-memory mirror still
            // describes the *old* index, so an ordinary mutation would republish
            // that count over a region that no longer matches it.
            poison_interrupted_rebuild(layout, commit_index);
        }
        return Err(err);
    }
    let commit = &mut layout.commits[commit_index];
    // The replaced map's own page-index tracking goes with it.
    let released_index = commit.bits.resident_index_bytes().saturating_add(
        commit
            .quarantined_raw_bits
            .as_ref()
            .map(SparseBitmap::resident_index_bytes)
            .unwrap_or(0),
    );
    commit.bits = rebuilt;
    commit.quarantined_raw_bits = None;
    commit.quarantine_finding = None;
    budget.pages = next_resident_pages;
    budget.release_index(released_index);
    layout.adopt_budget(budget);
    Ok(committed)
}

pub(crate) fn is_single_committed(layout: &MatrixLayout, name: &str) -> Result<bool> {
    let allowed = layout.ensure_fatal_access_allowed()?;
    let commit = &layout.commits[layout.commit_index(&allowed, name)?];
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
    let allowed = layout.ensure_fatal_access_allowed()?;
    let index = layout.commit_index(&allowed, name)?;
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
    let allowed = layout.ensure_fatal_access_allowed()?;
    let commit = &layout.commits[layout.commit_index(&allowed, name)?];
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
    let allowed = layout.ensure_fatal_access_allowed()?;
    let index = layout.commit_index(&allowed, name)?;
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
    allowed: &FatalAccessAllowed,
    category: &str,
) -> Result<usize> {
    let block = spec
        .matrix_blocks
        .iter()
        .find(|block| block.category == category)
        .ok_or_else(|| Error::MatrixCommitMissing(category.to_string()))?;
    layout.block_index(allowed, block.block_id)
}

/// Yields the fatal-access witness as well as making the quarantine refusal,
/// so a caller that has established publishability does not re-read the gate to
/// address the category it just checked.
fn ensure_commit_publishable(layout: &MatrixLayout, category: &str) -> Result<FatalAccessAllowed> {
    let allowed = layout.ensure_fatal_access_allowed()?;
    let commit = &layout.commits[layout.commit_index(&allowed, category)?];
    if commit.quarantined_raw_bits.is_some() {
        return Err(Error::MatrixCommitQuarantined(category.to_string()));
    }
    Ok(allowed)
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
    let allowed = layout.ensure_fatal_access_allowed()?;
    let commit_index = layout.commit_index(&allowed, category)?;
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

/// Byte offset of entry `slot`, which lives past the occupancy header slot.
fn page_index_entry_offset(base: u64, slot: u64) -> Result<u64> {
    slot.checked_add(PAGE_INDEX_HEADER_SLOTS)
        .and_then(|slot| slot.checked_mul(PAGE_INDEX_ENTRY_LEN))
        .and_then(|delta| base.checked_add(delta))
        .ok_or(Error::InvalidMatrixLayout)
}

/// The **only** route by which persisted page-index bytes can be written.
///
/// F-03, invariant 3, SHAPE A. The persisted entry array has an in-memory
/// mirror ([`SparseBitmap::index_slots`] and [`SparseBitmap::indexed_pages`]),
/// and every round of this review that touched the matrix has produced the same
/// defect in a different function: the disk bytes are written first and the
/// mirror is brought into agreement afterwards by a step that can fail or
/// allocate. When that step returns `AllocationFailed` the writer is *not*
/// poisoned — `finish_matrix_mutation` poisons only `Error::Io`, deliberately —
/// so the session continues with a slot map that no longer describes the array,
/// and the next removal overwrites a live page's only entry.
///
/// Reading for that shape does not work. Round 10 ran an exhaustive sweep whose
/// stated purpose was to enumerate it, rewrote this file by 736 lines, and
/// walked straight past `compact_page_index`, which had exactly the shape and
/// had been introduced by an earlier round of the same exercise. So the shape is
/// made unrepresentable here instead of merely absent:
///
/// * The functions that put page-index bytes on disk — [`write_entry_run`],
///   [`write_header_bytes`] — are private to this module. Nothing in the rest of
///   `matrix.rs` can call them, and the compiler, not a comment, is what says so.
/// * The only way to reach them is to hold one of the prepared values below.
///   Producing one performs **every** fallible step of the mutation: the budget
///   charge, the mirror reservations, all offset and count arithmetic, and the
///   encoding of every byte that will be written.
/// * `commit` takes the prepared value **by value**, writes the already-encoded
///   bytes, and then installs the mirror with `clear`/`extend`/`push`/`insert`
///   calls into capacity that is already reserved — infallible and
///   allocation-free by construction.
///
/// A caller that tries to skip the preparation has nothing to pass to `commit`
/// and does not compile. The residual property this buys, and the one the rest
/// of the matrix mutation machinery depends on, is stated once here: **after a
/// prepared page-index mutation begins writing, the only error it can return is
/// `Error::Io`** — which `finish_matrix_mutation` does poison on. There is no
/// remaining path on which a typed non-I/O refusal is raised between a durable
/// page-index byte and its mirror.
///
/// Invariant 1 is satisfied inside the same boundary: the residency each
/// mutation will take is charged against `ReadLimitKey::MatrixBitmapBytes`
/// during preparation, before the memory is reserved, and released again if the
/// reservation or the disk write then fails.
///
/// The one page-index disk mutation that deliberately does **not** go through
/// here is the whole-region [`zero_range`] performed by `clear_category` and by
/// the entry-region reset inside [`write_commit_map_pages`]. Neither is an entry
/// or header write: both erase the array wholesale and are paired with
/// [`SparseBitmap::clear`] or with the two `clear()` calls on the mirror, which
/// are infallible and take no capacity. They are named here so that "the only
/// route" is a statement about entry and header bytes rather than a claim the
/// file does not support.
mod page_index {
    use super::*;

    /// Writes an already-encoded run of consecutive entries.
    ///
    /// Private to this module: this is the disk mutation that SHAPE A puts
    /// behind a prepared value. It takes bytes rather than pages precisely so
    /// that no arithmetic — and therefore no non-`Io` failure — remains at this
    /// point. A compaction rewrite is one seek and one write rather than one per
    /// entry.
    fn write_entry_run(file: &mut File, offset: u64, bytes: &[u8]) -> Result<()> {
        if bytes.is_empty() {
            return Ok(());
        }
        #[cfg(test)]
        if FAIL_NEXT_PAGE_INDEX_WRITE.replace(false) {
            return Err(Error::Io(std::io::Error::other(
                "injected matrix page index write failure",
            )));
        }
        #[cfg(any(test, feature = "scalable-fault-injection"))]
        if take_injected_failure(&scaling_counters::PAGE_INDEX_ENTRY_WRITE_FAILURE) {
            return Err(Error::Io(std::io::Error::other(
                "injected matrix page index entry write failure",
            )));
        }

        file.seek(SeekFrom::Start(offset))?;
        file.write_all(bytes)?;
        Ok(())
    }

    /// Writes one raw occupancy-header slot.
    ///
    /// Private for the same reason. The value is always encoded and validated
    /// during preparation, which is what keeps [`PAGE_INDEX_REBUILD_MARKER`] —
    /// deliberately not a representable count (F-02) — expressible without
    /// exposing an unvalidated header write to the rest of the file.
    fn write_header_bytes(file: &mut File, base: u64, bytes: [u8; 8]) -> Result<()> {
        #[cfg(any(test, feature = "scalable-fault-injection"))]
        if take_injected_failure(&scaling_counters::PAGE_INDEX_HEADER_WRITE_FAILURE) {
            return Err(Error::Io(std::io::Error::other(
                "injected matrix page index header write failure",
            )));
        }

        file.seek(SeekFrom::Start(base))?;
        file.write_all(&bytes)?;
        Ok(())
    }

    /// Fails a page-index *mirror* reservation, which is the allocation F-03
    /// requires to happen before the first disk byte rather than after the last
    /// one.
    #[cfg(any(test, feature = "scalable-fault-injection"))]
    fn injected_mirror_reservation_failure(requested: u64) -> Result<()> {
        if take_injected_failure(&scaling_counters::PAGE_INDEX_RESERVATION_FAILURE) {
            return Err(Error::AllocationFailed {
                resource: ReadLimitKey::MatrixBitmapBytes.resource(),
                requested,
            });
        }
        Ok(())
    }

    #[cfg(not(any(test, feature = "scalable-fault-injection")))]
    fn injected_mirror_reservation_failure(_requested: u64) -> Result<()> {
        Ok(())
    }

    fn resource() -> &'static str {
        ReadLimitKey::MatrixBitmapBytes.resource()
    }

    /// Encodes one entry. `page + 1`, so a never-written slot (zero) is not a
    /// valid page reference.
    fn entry_bytes(page: u64) -> Result<[u8; 8]> {
        Ok(page
            .checked_add(1)
            .ok_or(Error::InvalidMatrixLayout)?
            .to_le_bytes())
    }

    /// The compaction half of a prepared append: the deduplicated, sorted live
    /// set, already encoded for the disk and already reserved for the mirror.
    struct PreparedCompaction {
        /// Live pages in slot order — the mirror's new `index_slots`.
        pages: Vec<u64>,
        /// The same set encoded as consecutive entries, ready to write.
        encoded: Vec<u8>,
        /// Offset of slot 0. Every slot this run covers is at a monotonically
        /// larger offset, and the largest was validated during preparation.
        first_entry_offset: u64,
        /// Slot residency the shorter array gives back once it is installed.
        refund: u64,
    }

    /// A page-index append whose every fallible step is already done (F-03).
    ///
    /// Holding one of these is the statement "publishing this entry cannot fail
    /// for any reason other than I/O, and installing its mirror cannot fail at
    /// all".
    #[must_use = "a prepared page-index append must be committed; dropping it \
                  leaks its resident-index charge"]
    pub(super) struct PreparedAppend {
        base: u64,
        compaction: Option<PreparedCompaction>,
        page: u64,
        slot: u64,
        entry_offset: u64,
        entry: [u8; 8],
        header: [u8; 8],
        charge: u64,
    }

    /// Prepares the publication of `page` in the persisted index at `base`.
    ///
    /// Returns `Ok(None)` when the page is already named, which is the common
    /// case on the hot path: a page joins the index once and costs nothing
    /// afterwards.
    ///
    /// Where the array is full this also prepares the compaction that makes room
    /// — the deduplicated live set replacing an array a crash-interrupted
    /// removal left holding duplicates. Compaction and append are prepared and
    /// committed as one unit so that the array is never left mid-rewrite by a
    /// step that could have been done earlier.
    pub(super) fn prepare_append(
        base: u64,
        bits: &mut SparseBitmap,
        page: u64,
        budget: &mut ResidentBitmapBudget,
    ) -> Result<Option<PreparedAppend>> {
        if bits.indexed_pages.contains_key(&page) {
            return Ok(None);
        }
        let mut slot = bits.index_len()?;
        let compaction = if slot >= bits.page_count {
            let plan = prepare_compaction(base, bits)?;
            slot = usize_to_u64(plan.pages.len())?;
            Some(plan)
        } else {
            None
        };
        if slot >= bits.page_count || slot >= PAGE_INDEX_MAX_ENTRIES {
            return Err(Error::InvalidMatrixLayout);
        }
        if page >= bits.page_count {
            return Err(Error::InvalidMatrixLayout);
        }
        let entry_offset = page_index_entry_offset(base, slot)?;
        let entry = entry_bytes(page)?;
        let count = slot.checked_add(1).ok_or(Error::InvalidMatrixLayout)?;
        let header = page_index_header_value(count)?.to_le_bytes();

        // Invariant 1: charged before the memory is taken, refunded if taking it
        // then fails.
        let charge = PAGE_INDEX_ENTRY_RESIDENT_BYTES;
        budget.charge_index(charge)?;
        let extra = compaction
            .as_ref()
            .map(|plan| plan.pages.len())
            .unwrap_or(0)
            .checked_add(1)
            .ok_or(Error::InvalidMatrixLayout)?;
        let reserved = (|| -> Result<()> {
            injected_mirror_reservation_failure(usize_to_u64(extra)?.saturating_mul(8))?;
            try_reserve_vec(&mut bits.index_slots, extra, resource())?;
            try_reserve_map(&mut bits.indexed_pages, 1, resource())
        })();
        if let Err(err) = reserved {
            budget.release_index(charge);
            return Err(err);
        }
        Ok(Some(PreparedAppend {
            base,
            compaction,
            page,
            slot,
            entry_offset,
            entry,
            header,
            charge,
        }))
    }

    /// Builds the replacement array for a full index: the sorted, deduplicated
    /// set of live pages, encoded and validated.
    ///
    /// Every fallible thing a compaction needs happens here — the temporary
    /// vectors, the offset arithmetic and the entry encoding — so the rewrite
    /// itself is a single write of already-encoded bytes.
    fn prepare_compaction(base: u64, bits: &SparseBitmap) -> Result<PreparedCompaction> {
        let mut pages = Vec::new();
        try_reserve_vec(&mut pages, bits.indexed_pages.len(), resource())?;
        pages.extend(bits.indexed_pages.keys().copied());
        pages.sort_unstable();
        let first_entry_offset = page_index_entry_offset(base, 0)?;
        if let Some(last_slot) = pages.len().checked_sub(1) {
            // The far end of the run. Every intermediate offset is smaller, so
            // validating this one validates the whole run and the write loop is
            // left with no arithmetic that can fail.
            page_index_entry_offset(base, usize_to_u64(last_slot)?)?;
        }
        let mut encoded = Vec::new();
        try_reserve_vec(
            &mut encoded,
            pages
                .len()
                .checked_mul(PAGE_INDEX_ENTRY_LEN as usize)
                .ok_or(Error::InvalidMatrixLayout)?,
            resource(),
        )?;
        for page in &pages {
            if *page >= bits.page_count {
                return Err(Error::InvalidMatrixLayout);
            }
            encoded.extend_from_slice(&entry_bytes(*page)?);
        }
        let before = bits.index_len()?;
        let refund = before
            .saturating_sub(usize_to_u64(pages.len())?)
            .saturating_mul(PAGE_INDEX_SLOT_RESIDENT_BYTES);
        Ok(PreparedCompaction {
            pages,
            encoded,
            first_entry_offset,
            refund,
        })
    }

    impl PreparedAppend {
        /// Publishes the prepared entry and installs its mirror.
        ///
        /// Ordering, and why an interruption at any point is safe: the entries
        /// are written before the occupancy count that admits them, so a crash
        /// or write failure between the two leaves the **old**, larger count
        /// naming a superset of the live set — a compaction rewrites slots
        /// `0..n` with the same pages the old array already named, and the new
        /// entry lands at slot `n`, which the old count either excludes or
        /// covers with a duplicate of the prefix. Enumeration only ever needs a
        /// superset, and the index never shrinks before the shorter count is
        /// durable.
        ///
        /// The mirror install afterwards is infallible: capacity for both the
        /// compacted run and the appended slot was reserved during preparation,
        /// and the map only receives one genuinely new key, whose slot was
        /// reserved with it. The compaction's own map writes are overwrites of
        /// keys that are already present.
        pub(super) fn commit(
            self,
            file: &mut File,
            bits: &mut SparseBitmap,
            budget: &mut ResidentBitmapBudget,
        ) -> Result<()> {
            if let Err(err) = self.persist(file) {
                // Nothing was installed, so nothing has to be unwound beyond the
                // charge. The array on disk still names a superset of the live
                // set and the mirror is untouched.
                budget.release_index(self.charge);
                return Err(err);
            }
            self.install(bits, budget);
            Ok(())
        }

        fn persist(&self, file: &mut File) -> Result<()> {
            if let Some(plan) = &self.compaction {
                write_entry_run(file, plan.first_entry_offset, &plan.encoded)?;
            }
            write_entry_run(file, self.entry_offset, &self.entry)?;
            write_header_bytes(file, self.base, self.header)
        }

        fn install(self, bits: &mut SparseBitmap, budget: &mut ResidentBitmapBudget) {
            if let Some(plan) = self.compaction {
                bits.index_slots.clear();
                bits.index_slots.extend(plan.pages.iter().copied());
                for (slot, page) in plan.pages.iter().enumerate() {
                    bits.indexed_pages.insert(*page, slot as u64);
                }
                budget.release_index(plan.refund);
            }
            bits.index_slots.push(self.page);
            bits.indexed_pages.insert(self.page, self.slot);
        }
    }

    /// A page-index removal whose every fallible step is already done.
    ///
    /// Removal never allocates, but it still goes through the gate: its disk
    /// writes are the same two slots, and letting one caller reach them by
    /// another route is how the guarded operation stops being guarded.
    #[must_use = "a prepared page-index release must be committed"]
    pub(super) struct PreparedRelease {
        base: u64,
        page: u64,
        slot: u64,
        moved: u64,
        last: u64,
        /// `None` when the vacated slot *is* the last one, so shortening the
        /// count is the whole removal.
        swap: Option<(u64, [u8; 8])>,
        header: [u8; 8],
    }

    /// Prepares the removal of `page` from the persisted index.
    ///
    /// `None` means there is nothing to remove, or that the mirror cannot
    /// describe the removal — in which case the array is left alone, which is
    /// the safe direction (an entry naming an all-zero page costs one page read
    /// at open and nothing else).
    pub(super) fn prepare_release(
        base: u64,
        bits: &SparseBitmap,
        page: u64,
    ) -> Option<PreparedRelease> {
        let slot = bits.indexed_pages.get(&page).copied()?;
        let last = bits.index_len().ok()?.checked_sub(1)?;
        let moved = bits.index_slots.get(usize::try_from(last).ok()?).copied()?;
        let swap = if slot == last {
            None
        } else {
            Some((
                page_index_entry_offset(base, slot).ok()?,
                entry_bytes(moved).ok()?,
            ))
        };
        let header = page_index_header_value(last).ok()?.to_le_bytes();
        Some(PreparedRelease {
            base,
            page,
            slot,
            moved,
            last,
            swap,
            header,
        })
    }

    impl PreparedRelease {
        /// Applies the removal, reporting nothing.
        ///
        /// F-03: this runs strictly *after* the bitmap byte that emptied the page
        /// is durable, because the safe direction is a superset. For the same
        /// reason it cannot fail the caller: turning an already-published
        /// mutation into an `Err` because an optimisation failed is the defect
        /// this review records elsewhere. Every failure path resynchronises the
        /// mirror with whatever the disk actually holds, and each of those
        /// resynchronisations is a removal plus an in-place value update, so
        /// none of them can fail a second time.
        pub(super) fn commit(
            self,
            file: &mut File,
            bits: &mut SparseBitmap,
            budget: &mut ResidentBitmapBudget,
        ) {
            if let Some((offset, bytes)) = self.swap
                && write_entry_run(file, offset, &bytes).is_err()
            {
                // Nothing was changed on disk, so nothing changes in the mirror.
                return;
            }
            if write_header_bytes(file, self.base, self.header).is_err() {
                // The swap landed but the shorter count did not: the array still
                // holds `last + 1` entries, with `moved` now named twice and
                // `page` gone. Mirror exactly that and refund only the map entry
                // that was dropped.
                bits.mirror_index_swap(self.page, self.slot, self.moved, self.last);
                budget.release_index(PAGE_INDEX_MAP_RESIDENT_BYTES);
                return;
            }
            bits.mirror_index_swap(self.page, self.slot, self.moved, self.last);
            bits.mirror_index_pop();
            budget.release_index(PAGE_INDEX_ENTRY_RESIDENT_BYTES);
        }
    }

    /// One entry of a whole-map republication, prepared (F-02).
    #[must_use = "a prepared page-index republication entry must be committed"]
    pub(super) struct PreparedRepublish {
        page: u64,
        slot: u64,
        entry_offset: u64,
        entry: [u8; 8],
        charge: u64,
    }

    /// Prepares one entry of a page index that is being republished wholesale.
    ///
    /// The occupancy header stays at [`PAGE_INDEX_REBUILD_MARKER`] for the whole
    /// of a republication, so this deliberately publishes no count: the caller
    /// commits [`PreparedPublication`] once, at the end. The caller supplies
    /// distinct pages and appends them to an index it has just emptied, so
    /// neither the already-indexed check nor the compaction path applies.
    pub(super) fn prepare_republish(
        base: u64,
        bits: &mut SparseBitmap,
        page: u64,
        budget: &mut ResidentBitmapBudget,
    ) -> Result<PreparedRepublish> {
        let slot = bits.index_len()?;
        if slot >= bits.page_count || slot >= PAGE_INDEX_MAX_ENTRIES || page >= bits.page_count {
            return Err(Error::InvalidMatrixLayout);
        }
        let entry_offset = page_index_entry_offset(base, slot)?;
        let entry = entry_bytes(page)?;
        let charge = PAGE_INDEX_ENTRY_RESIDENT_BYTES;
        budget.charge_index(charge)?;
        let reserved = (|| -> Result<()> {
            injected_mirror_reservation_failure(8)?;
            try_reserve_vec(&mut bits.index_slots, 1, resource())?;
            try_reserve_map(&mut bits.indexed_pages, 1, resource())
        })();
        if let Err(err) = reserved {
            budget.release_index(charge);
            return Err(err);
        }
        Ok(PreparedRepublish {
            page,
            slot,
            entry_offset,
            entry,
            charge,
        })
    }

    impl PreparedRepublish {
        pub(super) fn commit(
            self,
            file: &mut File,
            bits: &mut SparseBitmap,
            budget: &mut ResidentBitmapBudget,
        ) -> Result<()> {
            if let Err(err) = write_entry_run(file, self.entry_offset, &self.entry) {
                budget.release_index(self.charge);
                return Err(err);
            }
            bits.index_slots.push(self.page);
            bits.indexed_pages.insert(self.page, self.slot);
            Ok(())
        }
    }

    /// The occupancy count that publishes a republished generation (F-02).
    #[must_use = "a prepared page-index publication must be committed"]
    pub(super) struct PreparedPublication {
        header: [u8; 8],
    }

    /// Encodes the count a republication will publish.
    ///
    /// The mirror it publishes is already installed — each entry installed its
    /// own — so committing this is a pure disk write with nothing to follow it.
    pub(super) fn prepare_publication(bits: &SparseBitmap) -> Result<PreparedPublication> {
        Ok(PreparedPublication {
            header: page_index_header_value(bits.index_len()?)?.to_le_bytes(),
        })
    }

    impl PreparedPublication {
        pub(super) fn commit(self, file: &mut File, base: u64) -> Result<()> {
            write_header_bytes(file, base, self.header)
        }
    }

    /// Publishes [`PAGE_INDEX_REBUILD_MARKER`] into the occupancy slot (F-02).
    ///
    /// This is not a mirror mutation and takes no prepared value: it destroys
    /// nothing on its own and is the step that makes everything after it
    /// fail-closed, so there is no in-memory state that could disagree with it.
    pub(super) fn write_rebuild_marker(file: &mut File, base: u64) -> Result<()> {
        write_header_bytes(file, base, PAGE_INDEX_REBUILD_MARKER.to_le_bytes())
    }
}

fn read_page_index_header(file: &mut File, base: u64) -> Result<u64> {
    file.seek(SeekFrom::Start(base))?;
    let mut bytes = [0; PAGE_INDEX_ENTRY_LEN as usize];
    file.read_exact(&mut bytes)?;
    Ok(u64::from_le_bytes(bytes))
}

/// Records `page` in the persisted page index before its bytes are written.
///
/// PERF-01: this is what lets open enumerate the written pages without a
/// `0..page_count` loop, including where the platform reports no allocation map
/// at all. The order matters: an index entry whose page never reached the disk
/// names a page that still reads as uninitialised zeros, which open already
/// accepts, whereas a written page with no entry would be invisible to an open
/// that has no allocation map to fall back on.
///
/// Cost is one 8-byte write the first time a page is touched — one per 32,768
/// commit bits — and nothing at all afterwards.
///
/// F-03: the whole mutation, including the compaction an already-full array
/// needs, is prepared before the first byte reaches the disk and installed
/// infallibly afterwards. There is no local ordering to get right here, because
/// [`page_index`] does not expose an ordering to get wrong.
fn record_page_index_entry(
    file: &mut File,
    base: u64,
    bits: &mut SparseBitmap,
    page: u64,
    budget: &mut ResidentBitmapBudget,
) -> Result<()> {
    let Some(prepared) = page_index::prepare_append(base, bits, page, budget)? else {
        return Ok(());
    };
    prepared.commit(file, bits, budget)
}

/// Removes `page` from the persisted page index once its final set bit clears.
///
/// F-03: this is what makes both the array and its in-memory mirror track
/// *live* pages instead of every page ever published. It runs strictly after
/// the bitmap byte that emptied the page is durable, because the safe direction
/// is a superset: an entry naming an all-zero page costs one extra page read at
/// open and nothing else, whereas dropping an entry for a page that still holds
/// committed bits would hide them.
///
/// `O(1)`: the vacated slot is overwritten with the array's last entry and the
/// occupancy count is decremented — two 8-byte writes, no scan.
fn release_page_index_entry(
    file: &mut File,
    base: u64,
    bits: &mut SparseBitmap,
    page: u64,
    budget: &mut ResidentBitmapBudget,
) {
    if let Some(prepared) = page_index::prepare_release(base, bits, page) {
        prepared.commit(file, bits, budget);
    }
}

/// Records the persisted page index entry for the page holding `byte_index`,
/// but only where the mutation leaves that page holding a set bit.
///
/// A page that stays all zero needs no entry: its bytes are the zero page an
/// unvisited page already answers with, and its digest authenticates exactly
/// that. Skipping those keeps both the array and its in-memory tracking set
/// proportional to the pages that carry state rather than to the pages a writer
/// has merely touched.
fn record_mutated_page_index(
    layout: &mut MatrixLayout,
    file: &mut File,
    target: PageIndexTarget,
    byte_index: u64,
    byte_value: u8,
) -> Result<()> {
    let page = byte_index / BITMAP_PAGE_BYTES;
    let mut budget = layout.budget();
    let outcome = (|| -> Result<()> {
        let (base, bits) = match page_index_target(layout, target) {
            Some(parts) => parts,
            None => return Ok(()),
        };
        if bits.indexed_pages.contains_key(&page)
            || bits.page_ones_after(byte_index, byte_value)? == 0
        {
            return Ok(());
        }
        record_page_index_entry(file, base, bits, page, &mut budget)
    })();
    layout.adopt_budget(budget);
    outcome
}

/// Drops the persisted page index entry for the page holding `byte_index` now
/// that the page is empty (F-03).
fn release_mutated_page_index(
    layout: &mut MatrixLayout,
    file: &mut File,
    target: PageIndexTarget,
    byte_index: u64,
) {
    let page = byte_index / BITMAP_PAGE_BYTES;
    let mut budget = layout.budget();
    if let Some((base, bits)) = page_index_target(layout, target) {
        release_page_index_entry(file, base, bits, page, &mut budget);
    }
    layout.adopt_budget(budget);
}

/// The persisted index base and bitmap a target names, or `None` where the
/// target keeps no persisted index (a session write-tracking map, or a validity
/// bitmap in a format without checksums).
fn page_index_target(
    layout: &mut MatrixLayout,
    target: PageIndexTarget,
) -> Option<(u64, &mut SparseBitmap)> {
    match target {
        PageIndexTarget::Commit(index) => Some((
            layout.commits[index].index_offset,
            &mut layout.commits[index].bits,
        )),
        PageIndexTarget::CrcValid(index) => {
            let base = layout.blocks[index].crc_valid_index_offset?;
            Some((
                base,
                layout.blocks[index].crc_valid_bits.page_index_mirror_mut(),
            ))
        }
    }
}

#[derive(Clone, Copy)]
enum PageIndexTarget {
    Commit(usize),
    CrcValid(usize),
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
    // Invariant 3 (F-03). Every step that can fail — the budget charge, the page
    // allocation and map reservation, the page-index publication, the digest
    // write and the bitmap-byte write — happens *before* the memory update that
    // makes the mutation visible, and that update is
    // `SparseBitmap::commit_byte_write`, which returns nothing and allocates
    // nothing. The previous ordering called `set_byte` after the durable bitmap
    // byte, so a first-touch page allocation that returned `AllocationFailed`
    // left disk holding the new bit while memory held the old byte, with the
    // writer still usable and its next mutation derived from stale memory.
    //
    // The precharge is still rolled back on every escape, because none of those
    // failures leaves anything materialised. A retained page-index entry is real
    // resident memory and is deliberately *not* refunded here.
    let prepared = match layout.commits[commit_index]
        .bits
        .prepare_byte_write(update.bitmap.byte_index, update.bitmap.byte_value)
    {
        Ok(prepared) => prepared,
        Err(err) => {
            layout.release_resident_bitmap(cost);
            return Err(err);
        }
    };
    let persisted = (|| -> Result<()> {
        record_mutated_page_index(
            layout,
            file,
            PageIndexTarget::Commit(commit_index),
            update.bitmap.byte_index,
            update.bitmap.byte_value,
        )?;
        if let Some((offset, crc)) = update.digest {
            write_page_digest(file, offset, crc, PAGE_STATE_INITIALIZED)?;
        }
        write_bitmap_byte(file, &update.bitmap)
    })();
    if let Err(err) = persisted {
        // `prepared` is dropped here: the detached page is freed and the bitmap
        // is byte-for-byte what it was before the call.
        layout.release_resident_bitmap(cost);
        return Err(err);
    }
    let delta = layout.commits[commit_index]
        .bits
        .commit_byte_write(prepared);
    layout.settle_page_delta(delta);
    if delta.released != 0 {
        // The page is now byte-for-byte the zero page, and the bitmap byte
        // that made it so is already durable, so its index entry can go
        // (F-03).
        release_mutated_page_index(
            layout,
            file,
            PageIndexTarget::Commit(commit_index),
            update.bitmap.byte_index,
        );
    }
    Ok(())
}

/// The three file regions one commit bitmap occupies: its bytes, its per-page
/// digests where integrity is enabled, and its persisted page index.
#[derive(Clone, Copy)]
struct CommitMapRegions {
    map_offset: u64,
    digest_offset: Option<u64>,
    index_offset: u64,
}

/// Appends `page` to a page index that is being republished wholesale, without
/// touching the occupancy header.
///
/// The header is the rebuild marker for the whole of a republication (F-02), so
/// the per-entry header write that [`record_page_index_entry`] performs would
/// publish a half-built generation. The caller supplies pages that are distinct
/// and appends them to an index it has just emptied, so neither the
/// already-indexed check nor the compaction path of the incremental helper
/// applies.
///
/// F-03: this used to reserve the mirror slot *after* the entry byte reached the
/// disk. It goes through [`page_index`] like every other page-index mutation, so
/// the reservation is a precondition of the write rather than a consequence of
/// it.
fn republish_page_index_entry(
    file: &mut File,
    base: u64,
    bits: &mut SparseBitmap,
    page: u64,
    budget: &mut ResidentBitmapBudget,
) -> Result<()> {
    page_index::prepare_republish(base, bits, page, budget)?.commit(file, bits, budget)
}

/// Whole-map publication (rebuild and recovery) rewrites every page this map has
/// ever published, together with its digest and its page-index entry, keeping
/// full-map verification available without ever making a single-bit mutation
/// cost more than one page — and without a `0..page_count` loop.
///
/// F-02, invariant 2. The persisted page index is the *only* way a reopen with
/// no filesystem allocation map can find a published page, so emptying it and
/// refilling it in place opened a window in which an interruption left a valid,
/// short occupancy count. That state is indistinguishable from "those pages were
/// never published": pages the interrupted rebuild had not reached were never
/// visited, their cells reopened as `NotCommitted`, and no finding was produced
/// at all. Poisoning the writer did not help, because the damage was on disk.
///
/// The window is closed by publishing a *generation* rather than mutating one:
///
/// 1. the occupancy header is replaced by [`PAGE_INDEX_REBUILD_MARKER`], and
///    that single slot is made durable **before** anything is destroyed;
/// 2. only the entry region is cleared and refilled — the header goes on naming
///    a rebuild in progress for the whole of it;
/// 3. every entry, digest and page byte is made durable;
/// 4. the real occupancy count is written last, and *is* the atomic publication
///    of the new generation.
///
/// A crash or a write failure anywhere from 1 to 4 therefore leaves the marker
/// on disk, and [`load_page_index`] refuses it with a fatal
/// [`MatrixCorruptionKind::CommitMap`] finding: reopen fails closed and asks for
/// the rebuild to be run again, instead of reading a short index as
/// authoritative. `destructive` reports back whether the marker was published,
/// so the caller knows whether an error left durable state needing that
/// treatment.
///
/// This follows the same shape as the native replacement publication and the
/// sidecar generation: stage everything, make it durable, then publish with a
/// single write, and reject a half-published artifact with a typed error rather
/// than reading it. It costs two `sync_data` calls on a recovery path that
/// already rewrites every published page, and nothing at all on the hot path.
fn write_commit_map_pages(
    file: &mut File,
    regions: CommitMapRegions,
    previous: &SparseBitmap,
    bits: &mut SparseBitmap,
    budget: &mut ResidentBitmapBudget,
    destructive: &mut bool,
) -> Result<()> {
    let CommitMapRegions {
        map_offset,
        digest_offset,
        index_offset,
    } = regions;
    let mut pages = Vec::new();
    try_reserve_vec(
        &mut pages,
        previous
            .indexed_pages
            .len()
            .saturating_add(bits.pages.len()),
        ReadLimitKey::MatrixBitmapBytes.resource(),
    )?;
    pages.extend(previous.indexed_pages.keys().copied());
    pages.extend(bits.pages.keys().copied());
    pages.sort_unstable();
    pages.dedup();

    let page_count = previous.page_count.max(bits.page_count);
    let entries_offset = index_offset
        .checked_add(PAGE_INDEX_ENTRY_LEN)
        .ok_or(Error::InvalidMatrixLayout)?;
    let entries_len = index_page_array_len(page_count)?
        .checked_sub(PAGE_INDEX_ENTRY_LEN)
        .ok_or(Error::InvalidMatrixLayout)?;

    // (1) Publish the rebuild marker and make it durable before anything a
    // reopen depends on is destroyed.
    page_index::write_rebuild_marker(file, index_offset)?;
    file.sync_data()?;
    *destructive = true;
    rebuild_abort_stage(1);

    // (2) The index is rebuilt from scratch: a page that the rebuild leaves all
    // zero is written back as the uninitialised encoding and drops out of the
    // index entirely, so it costs nothing at the next open. Only the entries are
    // cleared; the header keeps naming the rebuild.
    zero_range(file, entries_offset, entries_len)?;
    budget.release_index(bits.resident_index_bytes());
    bits.indexed_pages.clear();
    bits.index_slots.clear();
    rebuild_abort_stage(2);

    for page in pages {
        if page >= bits.page_count {
            return Err(Error::InvalidMatrixLayout);
        }
        let materialised = bits.pages.contains_key(&page);
        if materialised {
            republish_page_index_entry(file, index_offset, bits, page, budget)?;
            rebuild_abort_stage(3);
        }
        let bytes = bits.page_bytes(page)?;
        if let Some(base) = digest_offset {
            let (crc, state) = if materialised {
                (crc32_bytes(bytes)?, PAGE_STATE_INITIALIZED)
            } else {
                (0, PAGE_STATE_UNINITIALIZED)
            };
            write_page_digest(file, page_digest_offset(base, page)?, crc, state)?;
        }
        let offset = page
            .checked_mul(BITMAP_PAGE_BYTES)
            .and_then(|delta| map_offset.checked_add(delta))
            .ok_or(Error::InvalidMatrixLayout)?;
        file.seek(SeekFrom::Start(offset))?;
        file.write_all(bytes)?;
    }

    // (3) Everything the new count will admit is durable, and only then (4) is
    // the count itself written. That single 8-byte write is the publication.
    //
    // The count is encoded before the sync, so the publication itself is a write
    // of already-validated bytes: the mirror it publishes was installed entry by
    // entry, and nothing follows it.
    let publication = page_index::prepare_publication(bits)?;
    file.sync_data()?;
    rebuild_abort_stage(4);
    publication.commit(file, index_offset)?;
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
    block
        .crc_valid_bits
        .prepare_update(block.cell_count, valid_offset, ordinal, value)
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
    // The validity bitmap's twin of the commit-bit path, with the same
    // prepare-persist-install ordering and the same rollback, for the same
    // reasons and with the same exclusion of a retained page-index entry
    // (F-03, invariant 3).
    let prepared = match layout.blocks[block_index]
        .crc_valid_bits
        .prepare_byte_write(update.byte_index, update.byte_value)
    {
        Ok(prepared) => prepared,
        Err(err) => {
            layout.release_resident_bitmap(cost);
            return Err(err);
        }
    };
    let persisted = (|| -> Result<()> {
        record_mutated_page_index(
            layout,
            file,
            PageIndexTarget::CrcValid(block_index),
            update.byte_index,
            update.byte_value,
        )?;
        write_bitmap_byte(file, &update)
    })();
    if let Err(err) = persisted {
        layout.release_resident_bitmap(cost);
        return Err(err);
    }
    let delta = layout.blocks[block_index]
        .crc_valid_bits
        .commit_byte_write(prepared);
    layout.settle_page_delta(delta);
    if delta.released != 0 {
        release_mutated_page_index(
            layout,
            file,
            PageIndexTarget::CrcValid(block_index),
            update.byte_index,
        );
    }
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
    //
    // F-04: this is the fail-closed reader. It is the one caller allowed to
    // consult the validity bitmap without the completeness witness, precisely
    // because `require_meaningful` hands back no bit — an absent one can only
    // become this refusal, never a published commit bit. `actual` is computed
    // once and reused by both refusals, so the fail-closed path costs no extra
    // checksum pass.
    let actual = crc32_bytes(payload)?;
    if layout.blocks[block_index].crc_valid_offset.is_some() {
        let offset = layout.slot_offset(block_index, ordinal)?;
        layout.blocks[block_index]
            .crc_valid_bits
            .require_meaningful(ordinal, || Error::MatrixChecksumMismatch {
                offset,
                expected: 0,
                actual,
            })?;
    }
    let expected = read_crc_at(file, indexed_crc_offset(crc_offset, ordinal)?)?;
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
) -> Result<HashMap<u32, (SparseBitmap, bool)>> {
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
        // A freshly created matrix has published nothing, so its empty
        // validity bitmap is complete evidence by construction (F-04).
        bitmaps.insert(block.block_id, (SparseBitmap::new(cell_count)?, true));
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
    page_index: MatrixPageIndexLayout,
    mut crc_valid_bits: HashMap<u32, (SparseBitmap, bool)>,
    mut commit_findings: HashMap<String, MatrixRecoveryFinding>,
    crc_findings: Vec<MatrixRecoveryFinding>,
    append_log_start: u64,
    resident_bitmap_bytes: u64,
    resident_page_index_bytes: u64,
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
            spec.read_limits.check(
                ReadLimitKey::MatrixBitmapBytes,
                resident_bitmap_bytes
                    .checked_add(resident_page_index_bytes)
                    .ok_or(Error::ResourceArithmeticOverflow {
                        resource: ReadLimitKey::MatrixBitmapBytes.resource(),
                    })?,
            )?;
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
            index_offset: page_index
                .commit_offsets
                .get(commit_index)
                .copied()
                .ok_or(Error::InvalidMatrixLayout)?,
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
        let (loaded_valid_bits, index_complete) = match &crc {
            Some(_) => crc_valid_bits
                .remove(&block.block_id)
                .ok_or(Error::InvalidMatrixLayout)?,
            // Without integrity there is no validity bitmap at all, and no
            // recovery path reads one; "complete" is vacuously true and the
            // refusal below is unreachable for such a format.
            None => (SparseBitmap::new(0)?, true),
        };
        // This is the one place a `SparseBitmap` becomes CRC-validity evidence.
        // From here on the bitmap is unnameable and the completeness fact
        // travels with it (F-04).
        let crc_valid_bits_for_block = CrcValidEvidence::new(loaded_valid_bits, index_complete);
        if crc.is_some() && crc_valid_bits_for_block.byte_len() != crc_valid_len {
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
            crc_valid_index_offset: crc
                .as_ref()
                .map(|_| {
                    page_index
                        .block_valid_offsets
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
        resident_page_index_bytes,
        fatal_access: FatalAccessGate::new(fatal_access_blocked),
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

fn page_index_table_len(
    spec: FormatSpec,
    crc_enabled: bool,
    commit_plans: &[CommitPlan],
    cell_counts: &HashMap<String, u64>,
) -> Result<u64> {
    let mut len = 0u64;
    for (_, _, bit_count) in commit_plans {
        len = len
            .checked_add(page_index_len(bit_bytes(*bit_count)?)?)
            .ok_or(Error::InvalidMatrixLayout)?;
    }
    if crc_enabled {
        for block in spec.matrix_blocks {
            let cell_count = *cell_counts
                .get(block.category)
                .ok_or_else(|| Error::MatrixCommitMissing(block.category.to_string()))?;
            len = len
                .checked_add(page_index_len(bit_bytes(cell_count)?)?)
                .ok_or(Error::InvalidMatrixLayout)?;
        }
    }
    Ok(len)
}

fn page_index_layout_from_parts(
    page_index_off: u64,
    page_index_len_total: u64,
    spec: FormatSpec,
    crc_enabled: bool,
    commit_plans: &[CommitPlan],
    block_offsets: &HashMap<u32, (u64, u64, u64)>,
) -> Result<MatrixPageIndexLayout> {
    let mut cursor = page_index_off;
    let mut commit_offsets = Vec::new();
    try_reserve_vec(
        &mut commit_offsets,
        commit_plans.len(),
        MATRIX_DESCRIPTOR_RESOURCE,
    )?;
    for (_, _, bit_count) in commit_plans {
        commit_offsets.push(cursor);
        cursor = cursor
            .checked_add(page_index_len(bit_bytes(*bit_count)?)?)
            .ok_or(Error::InvalidMatrixLayout)?;
    }
    let mut block_valid_offsets = Vec::new();
    if crc_enabled {
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
                .checked_add(page_index_len(bit_bytes(cell_count)?)?)
                .ok_or(Error::InvalidMatrixLayout)?;
        }
    }
    let expected_len = cursor
        .checked_sub(page_index_off)
        .ok_or(Error::InvalidMatrixLayout)?;
    if page_index_len_total != expected_len {
        return Err(Error::InvalidMatrixLayout);
    }
    Ok(MatrixPageIndexLayout {
        commit_offsets,
        block_valid_offsets,
    })
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

/// Where one paged bitmap and its authentication metadata live on disk.
#[derive(Clone, Copy)]
struct PagedBitmapSource<'a> {
    base_offset: u64,
    digest_base: Option<u64>,
    index_base: u64,
    extents: Option<&'a AllocatedExtents>,
}

/// Mutable state every paged-bitmap load contributes to: the resident budget
/// that admits its memory and the findings list that records its damage.
struct PagedBitmapSink<'a> {
    budget: &'a mut ResidentBitmapBudget,
    findings: &'a mut Vec<MatrixRecoveryFinding>,
}

/// Loads the persisted page index into `bits`.
///
/// F-06, the whole point of the v4 header: enumeration length comes from a
/// *validated occupancy count*, never from scanning for a terminator. The old
/// representation treated a zero or out-of-range entry as the successful end of
/// the array, so a single damaged entry silently truncated enumeration — every
/// later committed page went unvisited, its cells answered `NotCommitted`, and
/// no finding was produced at all. Damage now has exactly one outcome: the
/// entry is reported as a fatal finding and enumeration continues, so a damaged
/// index can cost visibility of one page but can never hide the rest.
///
/// The header slot carries its own redundancy, so a torn or flipped header is
/// detected too. There the true count is unknowable, so the index contributes
/// nothing and a fatal finding is raised; enumeration falls back to whatever
/// the allocation map can prove. Silence is never an outcome.
///
/// Cost is `O(count)` entries read — the live pages — and never `O(page_count)`.
///
/// F-04: the return value is whether the index was **complete** — whether every
/// page it named could be enumerated. `false` means the caller holds an
/// in-memory bitmap that is a *subset* of the published state, with the missing
/// pages reading as clear. That is safe for reading, because default open is
/// already fail-closed on the fatal finding this records, but it is not safe as
/// *evidence*: a recovery step that treats a clear bit as proof of absence would
/// erase state it never looked at. Callers that consume a bitmap as evidence
/// must propagate this.
fn load_page_index(
    file: &mut File,
    source: &PagedBitmapSource<'_>,
    bits: &mut SparseBitmap,
    sink: &mut PagedBitmapSink<'_>,
    resource: &'static str,
    label: &str,
) -> Result<bool> {
    let capacity = bits.page_count.min(PAGE_INDEX_MAX_ENTRIES);
    if capacity == 0 {
        return Ok(true);
    }
    // A region the filesystem proves to be a hole holds a zero header, which is
    // the encoding for "no entries". Reading it would answer the same thing.
    if !range_may_hold_data(source.extents, source.index_base, PAGE_INDEX_ENTRY_LEN) {
        return Ok(true);
    }
    let header = read_page_index_header(file, source.index_base)?;
    count_open_bitmap_bytes_read(PAGE_INDEX_ENTRY_LEN);
    // F-02: a whole-map republication marks the header before it empties the
    // entry region and only replaces the marker once the new generation is
    // durable, so seeing it means the index on disk names an unknown subset of
    // the published pages. Reading that subset as authoritative is precisely the
    // silent state loss the marker exists to prevent, so this is fatal and the
    // index contributes nothing.
    if header == PAGE_INDEX_REBUILD_MARKER {
        sink.findings.push(MatrixRecoveryFinding {
            kind: MatrixCorruptionKind::CommitMap,
            severity: MatrixCorruptionSeverity::Fatal,
            message: format!(
                "matrix page index for {label} is marked as an interrupted rebuild; the pages it \
                 names are not the published set and the commit map must be rebuilt again"
            ),
        });
        return Ok(false);
    }
    let Some(count) = page_index_header_count(header, capacity) else {
        sink.findings.push(MatrixRecoveryFinding {
            kind: MatrixCorruptionKind::CommitMap,
            severity: MatrixCorruptionSeverity::Fatal,
            message: format!(
                "matrix page index header for {label} is damaged ({header:#018x}); the set of \
                 published pages cannot be enumerated from it"
            ),
        });
        return Ok(false);
    };

    let mut scanned = 0u64;
    // The request grows geometrically from a single cache line, so a nearly
    // empty index costs one small read whatever the map's width.
    let mut window = PAGE_INDEX_MIN_SCAN_ENTRIES;
    let mut damaged = 0u64;
    while scanned < count {
        let entries = window.min(count - scanned);
        window = window.saturating_mul(2).min(PAGE_INDEX_SCAN_ENTRIES);
        let offset = page_index_entry_offset(source.index_base, scanned)?;
        let len = entries
            .checked_mul(PAGE_INDEX_ENTRY_LEN)
            .ok_or(Error::InvalidMatrixLayout)?;
        let bytes = read_range(file, offset, len, resource)?;
        count_open_bitmap_bytes_read(len);
        for entry in bytes.chunks_exact(PAGE_INDEX_ENTRY_LEN as usize) {
            let raw = u64::from_le_bytes(entry.try_into().expect("chunk"));
            // `raw` is `page + 1`. Inside the counted prefix a zero or
            // out-of-range value is damage, not an end marker, so it is
            // reported and skipped rather than ending the scan.
            if raw == 0 || raw > bits.page_count {
                damaged = damaged.saturating_add(1);
                continue;
            }
            bits.note_indexed_page(raw - 1, sink.budget)?;
        }
        scanned += entries;
    }
    if damaged != 0 {
        sink.findings.push(MatrixRecoveryFinding {
            kind: MatrixCorruptionKind::CommitMap,
            severity: MatrixCorruptionSeverity::Fatal,
            message: format!(
                "matrix page index for {label} has {damaged} unusable entries out of {count}; \
                 the pages they named cannot be located"
            ),
        });
    }
    Ok(damaged == 0)
}

/// The deduplicated set of pages open has to look at.
///
/// PERF-01: it is the union of the persisted page index — the pages this matrix
/// currently holds state in — and, where the platform can answer, the pages the
/// allocation map reports as holding bytes. The first term keeps open bounded
/// when no allocation map exists; the second is what makes a stray byte written
/// into a page the matrix never published detectable, and it exists only where
/// the platform supplies a usable map (F-06). Neither term is derived from the
/// logical page count.
///
/// F-07: the result is a *candidate* set, not a live-state set. For `L` indexed
/// pages and `A` allocation-derived candidates the union costs `O(L + A)` time
/// and `Theta(L + A)` temporary memory, and the caller reads up to `4096` bytes
/// per distinct candidate. `A` follows how densely the file is allocated, so a
/// dense bitmap extent yields candidates proportional to the region even when
/// few bits are live. Sparse allocation — the operating case this design targets
/// — is what makes the union track live state.
///
/// F-07: building the union is `O(Q)` in the candidate entries, not
/// `O(Q log Q)`. The index term is already distinct, because it is materialised
/// through a page-to-slot map, and the allocation term arrives in ascending
/// order, so cross-duplicates are removed with a hash probe and neighbour
/// comparison in a single linear pass instead of a sort. The result is not
/// sorted: the loader visits pages independently, and `insert_loaded_page` is
/// idempotent, so ordering buys nothing that would justify the extra `log Q`.
///
/// The page-digest array is deliberately *not* mapped back into this set. A
/// digest written without its page is a torn commit, but the index entry for
/// that page is written before either, so the index already covers it — while
/// an allocation granule spans thousands of 8-byte digest slots, so deriving
/// pages from that array would reintroduce a visit count proportional to the
/// map width.
fn pages_to_visit(
    source: &PagedBitmapSource<'_>,
    bits: &SparseBitmap,
    resource: &'static str,
) -> Result<Vec<u64>> {
    let mut pages = Vec::new();
    try_reserve_vec(&mut pages, bits.indexed_pages.len(), resource)?;
    pages.extend(bits.indexed_pages.keys().copied());
    let Some(extents) = source.extents else {
        return Ok(pages);
    };
    let boundary = pages.len();
    extents.allocated_units(
        source.base_offset,
        bits.byte_len,
        BITMAP_PAGE_BYTES,
        bits.page_count,
        &mut pages,
        resource,
    )?;
    // The allocation term is non-decreasing across extents, so one pass drops
    // both its own repeats at extent boundaries and anything the index already
    // named.
    let mut write = boundary;
    let mut previous: Option<u64> = None;
    for read in boundary..pages.len() {
        let page = pages[read];
        if previous == Some(page) || bits.indexed_pages.contains_key(&page) {
            continue;
        }
        previous = Some(page);
        pages[write] = page;
        write += 1;
    }
    pages.truncate(write);
    Ok(pages)
}

/// Loads the pages of a bitmap region that can hold state, materialising only
/// the ones that carry a set bit.
///
/// With `digest_base`, each visited page is authenticated against its stored
/// digest: `PAGE_STATE_UNINITIALIZED` asserts the page was never published and
/// must still read as zero, while `PAGE_STATE_INITIALIZED` asserts the recorded
/// checksum. The two states are distinct on disk, so a page that was written
/// with zeros is never confused with one that was never written. Without a
/// digest base the region is unauthenticated, exactly as before this format
/// version, and the load only decides residency.
///
/// PERF-01: the loop runs over [`pages_to_visit`], not over `0..page_count`, so
/// its length is the pages this matrix has published or the filesystem reports
/// as written — never the logical page count, and never the whole logical
/// bitmap merely because no allocation map could be obtained.
///
/// PERF-02: a visited page is still skipped without any I/O when the filesystem
/// proves that neither the page nor its digest slot has ever been written. Such
/// a page is `PAGE_STATE_UNINITIALIZED` holding zeros, which is precisely what
/// reading it would have established, so skipping changes neither residency nor
/// the set of reported findings — a stray byte written into an untouched page
/// allocates it and is therefore still visited, still read, and still reported.
/// That last sentence is about the skip, and it presupposes the allocation map
/// that produced the skip; where no map is available there is nothing to skip
/// and never-indexed pages are not visited at all (F-06).
fn load_paged_bitmap(
    file: &mut File,
    source: PagedBitmapSource<'_>,
    bit_count: u64,
    sink: &mut PagedBitmapSink<'_>,
    resource: &'static str,
    label: &str,
) -> Result<LoadedPagedBitmap> {
    let base_offset = source.base_offset;
    let digest_base = source.digest_base;
    let extents = source.extents;
    let mut bits = SparseBitmap::new(bit_count)?;
    let index_complete = load_page_index(file, &source, &mut bits, sink, resource, label)?;
    let pages = pages_to_visit(&source, &bits, resource)?;
    count_open_bitmap_pages_visited(usize_to_u64(pages.len())?);
    let mut intact = true;
    for page in pages {
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
            sink.budget.charge(len)?;
        }
        bits.insert_loaded_page(page, bytes)?;
    }
    Ok(LoadedPagedBitmap {
        bits,
        intact,
        index_complete,
    })
}

/// One loaded paged bitmap and the two independent statements a caller needs
/// about it.
///
/// `intact` is about the bytes that *were* loaded: every visited page agreed
/// with its stored digest. `index_complete` (F-04) is about the pages that were
/// not: it is false when the persisted page index could not be enumerated in
/// full, so the loaded bitmap is a subset of the published state with the
/// missing pages reading as clear. The two are separate because a map can be
/// byte-perfect and still incomplete, and only the second one decides whether
/// the map may be used as *evidence* that a bit is not set.
struct LoadedPagedBitmap {
    bits: SparseBitmap,
    intact: bool,
    index_complete: bool,
}

fn load_commit_bitmaps(
    file: &mut File,
    crc: Option<&MatrixCrcLayout>,
    page_index: &MatrixPageIndexLayout,
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
        let index_base = page_index
            .commit_offsets
            .get(index)
            .copied()
            .ok_or(Error::InvalidMatrixLayout)?;
        let LoadedPagedBitmap { bits, intact, .. } = load_paged_bitmap(
            file,
            PagedBitmapSource {
                base_offset: *map_offset,
                digest_base,
                index_base,
                extents,
            },
            *bit_count,
            &mut PagedBitmapSink {
                budget,
                findings: &mut verification.findings,
            },
            ReadLimitKey::MatrixBitmapBytes.resource(),
            &format!("commit category {name}"),
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

/// Loads every block's CRC-validity bitmap, together with whether each one is
/// complete evidence (F-04).
///
/// A validity bitmap is loaded with `digest_base: None`, so its *pages* are
/// unauthenticated by design, but its persisted page index is checked exactly
/// like any other and a damaged one is already recorded as a fatal finding.
/// Before this returned per-block completeness, the pages that index omitted
/// simply stayed absent, and every cell in them read as "CRC not meaningful"
/// with nothing distinguishing that from "checked, and it is not". Forensic mode
/// removes the fail-closed gate on the fatal finding, and CRC commit-map rebuild
/// then treated those clear bits as authoritative. The completeness of each map
/// is therefore tracked separately from the map itself and follows it into the
/// layout.
fn load_crc_valid_bits(
    spec: FormatSpec,
    file: &mut File,
    crc: Option<&MatrixCrcLayout>,
    page_index: &MatrixPageIndexLayout,
    block_offsets: &HashMap<u32, (u64, u64, u64)>,
    extents: Option<&AllocatedExtents>,
    sink: &mut PagedBitmapSink<'_>,
) -> Result<HashMap<u32, (SparseBitmap, bool)>> {
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
        let index_base = page_index
            .block_valid_offsets
            .get(block_index)
            .copied()
            .ok_or(Error::InvalidMatrixLayout)?;
        let loaded = load_paged_bitmap(
            file,
            PagedBitmapSource {
                base_offset: offset,
                digest_base: None,
                index_base,
                extents,
            },
            cell_count,
            sink,
            ReadLimitKey::MatrixCrcBytes.resource(),
            &format!("block {} validity bitmap", block.block_id),
        )?;
        valid_bits.insert(block.block_id, (loaded.bits, loaded.index_complete));
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
    page_index_off: u64,
    page_index_len: u64,
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
        // Appended after `append_log_start` so every previously defined field
        // keeps its index; the reserved tail shrinks by the same 16 bytes.
        fields.page_index_off,
        fields.page_index_len,
    ] {
        bytes.extend_from_slice(&value.to_le_bytes());
    }
    bytes.extend_from_slice(&[0; 16]);
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
    // Version 1 describes the pre-paging physical representation and version 2
    // carries no page-index region; both are stale-regenerable, not readable
    // under version 3.
    if version != VMAT_VERSION {
        return Err(Error::FormatVersionMismatch {
            expected: VMAT_VERSION,
            actual: version,
        });
    }
    let header_len = u32::from_le_bytes(bytes[8..12].try_into().expect("slice"));
    if header_len != VMAT_HEADER_LEN || bytes[6..8] != [0; 2] || bytes[144..160] != [0; 16] {
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
        page_index_off: read_u64(&bytes, &mut pos),
        page_index_len: read_u64(&bytes, &mut pos),
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
        || header.page_index_len
            != page_index_table_len(spec, crc_enabled, commit_plans, cell_counts)?
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
    let page_index_end = validate_range(header.page_index_off, header.page_index_len, file_len)?;
    if header.page_index_off != aux_end {
        return Err(Error::InvalidMatrixLayout);
    }
    let crc_end = if header.region_crc_off == 0 && header.region_crc_len == 0 {
        page_index_end
    } else {
        let crc_end = validate_range(header.region_crc_off, header.region_crc_len, file_len)?;
        if header.region_crc_off != page_index_end {
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

    /// F-02: the rebuild marker has to be unmistakable, so it must not be a
    /// representable occupancy header at *any* capacity — including the largest
    /// the header can encode, where the count field is saturated.
    #[test]
    fn the_rebuild_marker_is_never_a_valid_occupancy_header() {
        assert!(page_index_header_count(PAGE_INDEX_REBUILD_MARKER, u64::MAX).is_none());
        assert!(
            page_index_header_count(PAGE_INDEX_REBUILD_MARKER, PAGE_INDEX_MAX_ENTRIES).is_none()
        );
        assert!(page_index_header_count(PAGE_INDEX_REBUILD_MARKER, 1).is_none());
        // A zero header is still an empty index, not the marker.
        assert_eq!(page_index_header_count(0, 8), Some(0));
    }

    /// F-03: preparation does every allocating and fallible step, so the
    /// installation that follows a durable disk write is a pure state update.
    #[test]
    fn a_prepared_byte_write_installs_without_allocating_or_failing() {
        let mut map = SparseBitmap::new(BITMAP_PAGE_BYTES * 8).expect("bitmap");
        let prepared = map.prepare_byte_write(0, 0b0000_0011).expect("prepare");
        // The page is allocated but not installed: the bitmap is unchanged and
        // still holds no resident bytes.
        assert_eq!(map.resident_bytes(), 0);
        assert_eq!(map.byte(0).expect("byte"), 0);
        let delta = map.commit_byte_write(prepared);
        assert_eq!(delta.materialised, BITMAP_PAGE_BYTES);
        assert_eq!(delta.released, 0);
        assert_eq!(map.byte(0).expect("byte"), 0b0000_0011);
        assert_eq!(map.ones(), 2);

        // Dropping a prepared write instead of committing it leaves nothing
        // behind.
        let abandoned = map.prepare_byte_write(1, 0xFF).expect("prepare");
        drop(abandoned);
        assert_eq!(map.byte(1).expect("byte"), 0);
        assert_eq!(map.ones(), 2);

        // Clearing the last set bits releases the page again.
        let prepared = map.prepare_byte_write(0, 0).expect("prepare clear");
        let delta = map.commit_byte_write(prepared);
        assert_eq!(delta.materialised, 0);
        assert_eq!(delta.released, BITMAP_PAGE_BYTES);
        assert_eq!(map.resident_bytes(), 0);
        assert_eq!(map.ones(), 0);
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

/// Test-only view of a committed cell bit, so the rollback tests can prove a
/// retried mutation actually landed.
#[cfg(all(test, feature = "integrity"))]
fn is_cell_committed_for_test(layout: &MatrixLayout, category: &str, ordinal: u64) -> bool {
    let allowed = layout
        .ensure_fatal_access_allowed()
        .expect("fatal access allowed");
    let index = layout
        .commit_index(&allowed, category)
        .expect("commit category");
    layout.commits[index].bits.get(ordinal).expect("commit bit")
}

/// F-02: a failed bitmap mutation must not leave a payload precharge standing.
///
/// `apply_commit_bit` and `apply_cell_crc_valid` charge the page a mutation
/// would materialise *before* three fallible steps — page-index publication, the
/// page-digest write, and the bitmap-byte write — because the charge has to
/// happen before the memory is taken. The page itself does not become resident
/// until `set_byte`, so an escape in between used to leave the payload counter
/// charged for a page that does not exist. Enough failed mutations then refused
/// admissions for memory the matrix never held.
///
/// These tests drive the matrix layout directly rather than through
/// `VarveFile`, because an injected `Io` failure poisons that writer while the
/// contract under test includes *a retry after the failure still succeeds*.
///
/// The fixture declares `IntegrityPolicy::Crc32` because one of the three
/// fallible boundaries under test *is* the page-digest write, which only exists
/// when digests do. Without the `integrity` feature `create_layout` refuses the
/// spec with `IntegrityFeatureDisabled`, so the module is gated on the feature
/// rather than on `test` alone; the workspace's `--all-features` gate still runs
/// every one of these tests.
#[cfg(all(test, feature = "integrity"))]
mod mutation_precharge_rollback_tests {
    use super::*;
    use crate::{
        BlockDescriptor, Endian, IndexPolicy, ManifestPolicy, MatrixBlockDescriptor,
        MatrixCommitDescriptor, MatrixDimensionDescriptor, MatrixDimensions, RecoveryPolicy,
    };

    const CATEGORY: &str = "cells";
    const BLOCK_ID: u32 = 909;
    /// 65_536 cells: two 4 KiB commit-map pages, so a mutation can be aimed at
    /// a page other than the first.
    const SCANS: u64 = 512;
    const CHANNELS: u64 = 128;

    fn precharge_spec() -> FormatSpec {
        static BLOCKS: &[BlockDescriptor] = &[BlockDescriptor {
            id: BLOCK_ID,
            name: "PrechargeCell",
            version: 1,
            kind: BlockKind::Matrix,
            fields: &[],
        }];
        static DIMENSIONS: &[MatrixDimensionDescriptor] = &[
            MatrixDimensionDescriptor { name: "scan" },
            MatrixDimensionDescriptor { name: "ch" },
        ];
        static COMMITS: &[MatrixCommitDescriptor] = &[MatrixCommitDescriptor {
            name: CATEGORY,
            kind: MatrixCommitKind::Cell,
        }];
        static MATRIX_BLOCKS: &[MatrixBlockDescriptor] = &[MatrixBlockDescriptor {
            block_id: BLOCK_ID,
            dimensions: ["scan", "ch"],
            category: CATEGORY,
            slot_stride: 4,
        }];

        FormatSpec::new(
            b"MPRC",
            1,
            Endian::Little,
            0,
            IndexPolicy::ScanOnOpen,
            IntegrityPolicy::Crc32,
            RecoveryPolicy::Strict,
            ManifestPolicy::None,
            BLOCKS,
        )
        .with_matrix_spec(DIMENSIONS, COMMITS, MATRIX_BLOCKS)
        .with_read_limits(ReadLimits::finite_all(u64::MAX))
    }

    fn fixture() -> (tempfile::TempDir, File, MatrixLayout) {
        let directory = tempfile::tempdir().expect("temp dir");
        let path = directory.path().join("precharge.varve");
        let mut file = File::options()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&path)
            .expect("create matrix file");
        let dims = MatrixDimensions::from_pairs([("scan", SCANS), ("ch", CHANNELS)]);
        let layout =
            create_layout(precharge_spec(), &mut file, 0, &dims).expect("create matrix layout");
        (directory, file, layout)
    }

    fn commit(layout: &mut MatrixLayout, file: &mut File, ordinal: u64) -> Result<()> {
        let (commit_index, update) =
            prepare_cell_commit(layout, CATEGORY, ordinal, true).expect("prepare commit bit");
        apply_commit_bit(layout, file, commit_index, update)
    }

    fn counters(layout: &MatrixLayout) -> (u64, u64) {
        (
            layout.resident_bitmap_bytes,
            layout.resident_page_index_bytes,
        )
    }

    /// The commit-bit path, at all three fallible boundaries.
    ///
    /// The payload counter must return to exactly its pre-mutation value after
    /// every failure, and the mutation must still be retryable. The page-index
    /// counter is deliberately *not* asserted equal after the digest and
    /// bitmap-write failures: the index entry those leave behind is a real
    /// resident entry, and refunding it would understate held memory.
    #[test]
    fn a_failed_commit_bit_refunds_its_payload_precharge_at_every_boundary() {
        for (name, arm) in [
            ("page index", &FAIL_NEXT_PAGE_INDEX_WRITE),
            ("page digest", &FAIL_NEXT_PAGE_DIGEST_WRITE),
            ("bitmap byte", &FAIL_NEXT_BITMAP_WRITE),
        ] {
            let (_dir, mut file, mut layout) = fixture();
            let (payload_before, index_before) = counters(&layout);
            assert_eq!(
                payload_before, 0,
                "{name}: a fresh matrix already holds resident pages"
            );

            arm.set(true);
            let failed = commit(&mut layout, &mut file, 0);
            assert!(
                matches!(failed, Err(Error::Io(_))),
                "{name}: injected failure did not surface as an Io error: {failed:?}"
            );
            let (payload_after, index_after) = counters(&layout);
            assert_eq!(
                payload_after, payload_before,
                "{name}: a failed mutation left a payload page charged with no \
                 resident page"
            );
            assert!(
                index_after >= index_before,
                "{name}: page-index residency fell below its starting value"
            );

            // The failure must not have made the matrix unusable: retrying the
            // same mutation succeeds and charges the page exactly once.
            commit(&mut layout, &mut file, 0).expect("retry after injected failure");
            let (payload_retried, _) = counters(&layout);
            assert_eq!(
                payload_retried,
                payload_before + BITMAP_PAGE_BYTES,
                "{name}: the retry did not charge exactly one page"
            );
            assert!(
                is_cell_committed_for_test(&layout, CATEGORY, 0),
                "{name}: the retried commit bit is not set"
            );
        }
    }

    /// The same contract for the validity bitmap, whose apply path repeats the
    /// charge and two of the same fallible steps.
    #[test]
    fn a_failed_validity_bit_refunds_its_payload_precharge() {
        for (name, arm) in [
            ("page index", &FAIL_NEXT_PAGE_INDEX_WRITE),
            ("bitmap byte", &FAIL_NEXT_BITMAP_WRITE),
        ] {
            let (_dir, mut file, mut layout) = fixture();
            let (payload_before, _) = counters(&layout);

            let update = prepare_cell_crc_valid(&layout, 0, 0, true)
                .expect("prepare validity bit")
                .expect("integrity is enabled, so a validity bitmap exists");
            arm.set(true);
            let failed = apply_cell_crc_valid(&mut layout, &mut file, 0, update);
            assert!(
                matches!(failed, Err(Error::Io(_))),
                "{name}: injected failure did not surface as an Io error: {failed:?}"
            );
            let (payload_after, _) = counters(&layout);
            assert_eq!(
                payload_after, payload_before,
                "{name}: a failed validity mutation left a payload page charged"
            );

            let update = prepare_cell_crc_valid(&layout, 0, 0, true)
                .expect("prepare validity bit")
                .expect("validity bitmap exists");
            apply_cell_crc_valid(&mut layout, &mut file, 0, update)
                .expect("retry after injected failure");
            let (payload_retried, _) = counters(&layout);
            assert_eq!(
                payload_retried,
                payload_before + BITMAP_PAGE_BYTES,
                "{name}: the retry did not charge exactly one page"
            );
        }
    }

    /// Repeated failures must not accumulate: this is the shape that turns the
    /// leak into an `Error::LimitExceeded` for memory that was never taken.
    #[test]
    fn repeated_failed_mutations_do_not_accumulate_payload_charges() {
        let (_dir, mut file, mut layout) = fixture();
        let (payload_before, _) = counters(&layout);
        for round in 0..16 {
            FAIL_NEXT_BITMAP_WRITE.set(true);
            let ordinal = (round % 2) * 32_768;
            assert!(matches!(
                commit(&mut layout, &mut file, ordinal),
                Err(Error::Io(_))
            ));
            let (payload_after, _) = counters(&layout);
            assert_eq!(
                payload_after, payload_before,
                "round {round} accumulated a payload charge for a page that was \
                 never materialised"
            );
        }
        commit(&mut layout, &mut file, 0).expect("commit after sixteen failures");
    }
}

/// F-04: the evidence type's own contract, independent of any file.
///
/// The integration proof lives in
/// `crates/varve/tests/matrix_integrity_scaling.rs::crc_rebuild_refuses_an_incompletely_loaded_validity_index`
/// and the proof that the witness cannot be forged from outside the crate lives
/// in `crates/varve/tests/ui/fail_fabricated_crc_valid_completeness.rs`. These
/// are the unit-level statements the two of them rest on.
#[cfg(test)]
mod crc_valid_evidence_tests {
    use super::crc_valid_evidence::CrcValidEvidence;
    use super::{Error, SparseBitmap};

    fn evidence(complete: bool) -> CrcValidEvidence {
        let mut bits = SparseBitmap::new(16).expect("bitmap");
        let prepared = bits.prepare_byte_write(0, 0b0000_0001).expect("prepare");
        bits.commit_byte_write(prepared);
        CrcValidEvidence::new(bits, complete)
    }

    #[test]
    fn incomplete_evidence_issues_no_witness() {
        let incomplete = evidence(false);
        assert!(matches!(
            incomplete.complete(),
            Err(Error::MatrixFatalCorruption)
        ));
    }

    #[test]
    fn complete_evidence_reads_bits() {
        let complete = evidence(true);
        let witness = complete.complete().expect("witness");
        assert!(witness.get(0).expect("set bit"));
        assert!(!witness.get(1).expect("clear bit"));
    }

    /// The fail-closed reader is the one operation allowed on incomplete
    /// evidence, and this is why: it returns no bit. A set bit passes, an absent
    /// one becomes the caller's refusal — there is no third outcome for a caller
    /// to publish from.
    #[test]
    fn the_fail_closed_reader_yields_a_refusal_not_a_bit() {
        let incomplete = evidence(false);
        incomplete
            .require_meaningful(0, || Error::InvalidMatrixLayout)
            .expect("a set validity bit is meaningful whether or not the map is complete");
        assert!(matches!(
            incomplete.require_meaningful(1, || Error::InvalidMatrixLayout),
            Err(Error::InvalidMatrixLayout)
        ));
    }

    /// Clearing the map is not re-enumeration. A session that opened on a
    /// damaged validity index has no basis for reading absence as proof, and
    /// clearing a category does not give it one, so the refusal survives until
    /// the next open (checklist open item 24).
    #[test]
    fn clearing_the_map_does_not_restore_completeness() {
        let mut incomplete = evidence(false);
        incomplete.clear();
        assert!(matches!(
            incomplete.complete(),
            Err(Error::MatrixFatalCorruption)
        ));
    }
}
