use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::sync::{Arc, Mutex, MutexGuard};

use crate::codec::encode_to_vec_limited;
use crate::format::ReadLimitKey;
use crate::{
    BlockKind, Decoder, Error, FormatSpec, IntegrityPolicy, MatrixCommitKind, ReadLimits, Result,
    VarveMatrixBlock,
};
use crc_valid_evidence::CrcValidEvidence;
use fatal_access::{FatalAccessAllowed, FatalAccessGate};
use page_index_enumeration::PageIndexEnumeration;

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
const MATRIX_SIDECAR_RESOURCE: &str = "matrix sidecar";

pub(crate) use region_reader::{MatrixReadPool, MatrixRegionReader};

/// The read half of the matrix region, addressed positionally.
///
/// # Why this module exists
///
/// Every matrix read used to take `file: &mut File` and do `seek` + `read_exact`.
/// That is what forced `VarveFile::read_matrix_cell` and friends to take
/// `&mut self`, which in turn made it impossible for one handle to serve two
/// concurrent readers: the borrow checker refuses the second borrow. The cursor
/// was the only reason for the `&mut`; nothing about a read needs to move it.
///
/// `read_exact_at` here is `pread` on Unix and `seek_read` on Windows — the same
/// primitive `SnapshotFile::read_exact_at` already uses for the record region —
/// so it moves no cursor, needs no exclusive borrow, and two threads issuing it
/// against the same handle do not interfere.
///
/// # Why it is a module and not a bare struct
///
/// The field is a `&File`, and `impl Write for &File` exists, so a value of this
/// type is one field access away from being a *write* handle to the matrix
/// region. Declaring it at file scope would let any of `matrix.rs`'s ~7900 lines
/// write through a value that reads as read-only at the call site. Inside a
/// module with a private field, the borrow cannot be recovered anywhere: the
/// type hands out bytes, never the handle.
mod region_reader {
    use super::{Error, Result};
    use std::fs::File;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Arc, Mutex, Weak};

    /// Ids handed to [`MatrixReadPool`]s. Monotonic and never reused, so a
    /// thread-local entry left behind by a dropped pool can never be mistaken
    /// for a live one.
    static NEXT_POOL_ID: AtomicU64 = AtomicU64::new(1);

    /// How many distinct matrix files one thread keeps a private read handle
    /// for. Small on purpose: the handles are a per-thread resource and the
    /// realistic working set is one or two open matrices.
    const PER_THREAD_HANDLE_SLOTS: usize = 8;

    thread_local! {
        /// This thread's private read handles, keyed by pool id.
        ///
        /// A `thread_local!` static, deliberately **not** a field of any varve
        /// type: a `RefCell` field would make `VarveReader` `!Sync` and destroy
        /// the property this whole module exists for. Nothing here is shared
        /// between threads, so it needs no lock and cannot convoy.
        static PRIVATE_HANDLES: std::cell::RefCell<Vec<(u64, PrivateHandle)>> =
            const { std::cell::RefCell::new(Vec::new()) };
    }

    #[derive(Clone, Debug)]
    enum PrivateHandle {
        /// A live private handle owned by the pool; `Weak` so that dropping the
        /// `RecordFile` closes it even though this cache lives in another
        /// thread's storage.
        Open(Weak<File>),
        /// This platform, or this file, refused a private handle. Remembered so
        /// the cold path is attempted once per thread per file rather than once
        /// per read.
        Unavailable,
    }

    /// The owner of the private per-thread read handles for one matrix file.
    ///
    /// # Why this exists (the measured reason, not a plausible one)
    ///
    /// `read_exact_at` is `pread` on Unix and `seek_read` on Windows. `pread`
    /// takes no per-file lock, so on Unix N threads reading through one handle
    /// scale. `seek_read` is `ReadFile` with an `OVERLAPPED` offset against a
    /// **synchronous** handle, and Windows serialises those on the file object
    /// (it still maintains the shared file pointer). Measured on this repo's
    /// own read path, 240,000 cell reads with `IntegrityPolicy::Crc32`:
    ///
    /// ```text
    /// one shared handle:  1 thread 343,285 reads/s | 4 threads  91,137 reads/s (0.27x)
    /// one handle/thread:  1 thread 327,037 reads/s | 4 threads 593,337 reads/s (1.81x)
    /// ```
    ///
    /// So `&self` alone bought the *right* to share a handle and none of the
    /// throughput. The fix is the one Windows documents: give each reading
    /// thread its own file object. `ReOpenFile` derives a new file object from
    /// the open handle — no path, so no re-resolution and no window in which a
    /// different file could be opened under the same name — and each has its
    /// own file pointer and its own lock.
    ///
    /// # What it costs
    ///
    /// One thread-local lookup over at most [`PER_THREAD_HANDLE_SLOTS`] entries
    /// per read, and one `ReOpenFile` plus one `Mutex` acquisition the first
    /// time a given thread reads a given file. The `Mutex` is *only* on that
    /// cold path; no read touches it. Nothing is allocated per read.
    ///
    /// # Why it cannot go stale
    ///
    /// The pool owns the `Arc<File>`s and the thread-local cache holds `Weak`s,
    /// so every private handle is closed when the `RecordFile` is dropped, even
    /// though the cache entries live in other threads. A dead entry is refreshed
    /// or evicted on next use; ids are never reused.
    #[derive(Debug)]
    pub struct MatrixReadPool {
        id: u64,
        /// Only Windows hands out private handles: `pread` on Unix does not
        /// serialise on the file object, so `reopen` is `#[cfg(windows)]` and
        /// this stays empty everywhere else. The field is still declared
        /// unconditionally so the type is one type on every target; without
        /// the allow, a non-Windows build with the test accessor compiled out
        /// reports it as never read.
        #[cfg_attr(not(any(windows, test)), allow(dead_code))]
        handles: Mutex<Vec<Arc<File>>>,
    }

    impl MatrixReadPool {
        pub(crate) fn new() -> Self {
            Self {
                id: NEXT_POOL_ID.fetch_add(1, Ordering::Relaxed),
                handles: Mutex::new(Vec::new()),
            }
        }

        /// Number of private handles this pool has handed out. Test-facing
        /// accessor; it takes the cold-path lock and is not called by reads.
        #[cfg(test)]
        pub(crate) fn private_handle_count(&self) -> usize {
            self.handles.lock().map(|held| held.len()).unwrap_or(0)
        }

        /// A `Weak` to the first private handle, for the test that proves the
        /// handles die with the pool.
        #[cfg(test)]
        pub(crate) fn first_private_handle_weak(&self) -> Option<Weak<File>> {
            self.handles
                .lock()
                .ok()
                .and_then(|held| held.first().map(Arc::downgrade))
        }

        /// Derives a fresh read-only file object from `file`.
        ///
        /// Returns `None` on any failure; the caller then reads through the
        /// shared handle, which is always correct and merely slower.
        #[cfg(windows)]
        fn reopen(&self, file: &File) -> Option<Arc<File>> {
            use std::os::windows::io::{AsRawHandle, FromRawHandle, RawHandle};
            use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
            use windows_sys::Win32::Storage::FileSystem::{
                FILE_GENERIC_READ, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, ReOpenFile,
            };

            // FILE_SHARE_DELETE matters: std opens with it, and without it here
            // a cached private handle would block deletion of a file the owner
            // has closed.
            let raw = unsafe {
                ReOpenFile(
                    file.as_raw_handle() as _,
                    FILE_GENERIC_READ,
                    FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                    0,
                )
            };
            if raw.is_null() || raw == INVALID_HANDLE_VALUE {
                return None;
            }
            // SAFETY: `ReOpenFile` returned a handle owned by this call and not
            // aliased anywhere else; `File` takes ownership and closes it.
            let handle = Arc::new(unsafe { File::from_raw_handle(raw as RawHandle) });
            let mut held = self.handles.lock().ok()?;
            held.push(Arc::clone(&handle));
            drop(held);
            Some(handle)
        }

        /// Unix `pread` does not serialise on the file object, so a private
        /// handle would buy nothing and cost a descriptor per thread.
        #[cfg(not(windows))]
        fn reopen(&self, _file: &File) -> Option<Arc<File>> {
            None
        }

        /// This thread's private handle for this file, opening one on first use.
        fn private_handle(&self, file: &File) -> Option<Arc<File>> {
            PRIVATE_HANDLES.with(|cache| {
                let mut cache = cache.borrow_mut();
                if let Some(slot) = cache.iter().position(|(id, _)| *id == self.id) {
                    match &cache[slot].1 {
                        PrivateHandle::Unavailable => return None,
                        PrivateHandle::Open(weak) => {
                            if let Some(handle) = weak.upgrade() {
                                return Some(handle);
                            }
                        }
                    }
                    cache.swap_remove(slot);
                }
                let opened = self.reopen(file);
                if cache.len() >= PER_THREAD_HANDLE_SLOTS {
                    cache.remove(0);
                }
                match opened {
                    Some(handle) => {
                        cache.push((self.id, PrivateHandle::Open(Arc::downgrade(&handle))));
                        Some(handle)
                    }
                    None => {
                        cache.push((self.id, PrivateHandle::Unavailable));
                        None
                    }
                }
            })
        }
    }

    /// A positional reader over one open matrix file.
    ///
    /// `Copy`, because it is a shared borrow and callers pass it down several
    /// levels; `Sync` and `Send` follow from `&File` being both.
    #[derive(Clone, Copy, Debug)]
    pub struct MatrixRegionReader<'a> {
        file: &'a File,
        pool: Option<&'a MatrixReadPool>,
    }

    impl<'a> MatrixRegionReader<'a> {
        pub(crate) fn new(file: &'a File) -> Self {
            Self { file, pool: None }
        }

        /// The same reader, plus the private-handle pool of the file it reads.
        ///
        /// Reads issued through this are correct byte-for-byte whether or not
        /// the pool yields a handle; the pool only decides *which* file object
        /// carries them, and therefore whether concurrent readers convoy.
        pub(crate) fn with_pool(file: &'a File, pool: &'a MatrixReadPool) -> Self {
            Self {
                file,
                pool: Some(pool),
            }
        }

        /// Fills `buffer` from `offset`.
        ///
        /// Short reads are retried and `Interrupted` is retried, matching
        /// `SnapshotFile::read_exact_at`; a zero-length read at end of data is
        /// `UnexpectedEof`, which is what the previous `read_exact` reported.
        ///
        /// # The offset is per call, and on Windows the cursor still moves
        ///
        /// Every byte this returns is addressed by the `offset` argument, never
        /// by the handle's file pointer, so two threads reading different
        /// offsets through one handle each get their own bytes. That is the
        /// property criterion (C) needs and it holds on both platforms.
        ///
        /// It is *not* true that nothing observable changes: `pread` leaves the
        /// Unix file pointer alone, but `seek_read` is `ReadFile` with an
        /// `OVERLAPPED` offset, and Windows updates a synchronous handle's file
        /// pointer to the end of the transfer as a side effect. So on Windows
        /// concurrent readers do scribble on each other's cursor — they simply
        /// never consult it.
        ///
        /// Nothing else consults it either, and that is a borrow-checker fact
        /// rather than a convention: the only writers to this region go through
        /// `RecordFile::matrix_region`, which takes `&mut self`, so no write and
        /// no cursor-relative read can be in flight while any
        /// `MatrixRegionReader` derived from the same handle exists. Every
        /// matrix write seeks to its own absolute offset first in any case.
        /// `SnapshotFile::read_exact_at` has relied on the same reasoning for
        /// the record region since before this round.
        ///
        /// # Which file object carries the read
        ///
        /// If this reader was built with a [`MatrixReadPool`], the bytes are
        /// fetched through *this thread's* private file object rather than the
        /// shared handle. That changes no byte and no offset — both are the same
        /// file and the read is positional either way — but on Windows it is the
        /// difference between four threads reading 91,137 cells/s through one
        /// convoyed file object and 593,337 through four. See `MatrixReadPool`.
        pub(crate) fn read_exact_at(&self, offset: u64, buffer: &mut [u8]) -> Result<()> {
            // One count per logical read, not per retry: the subject is whether
            // a read was *issued* while a bitmap page-store lock was held.
            super::count_matrix_region_read(self.pool.is_some());
            let private = self.pool.and_then(|pool| pool.private_handle(self.file));
            let file: &File = private.as_deref().unwrap_or(self.file);
            let mut consumed = 0usize;
            while consumed < buffer.len() {
                let delta = u64::try_from(consumed)
                    .map_err(|_| Error::LengthOverflow { value: u64::MAX })?;
                let current =
                    offset
                        .checked_add(delta)
                        .ok_or(Error::ResourceArithmeticOverflow {
                            resource: "matrix read offset",
                        })?;
                match crate::snapshot::read_at(file, &mut buffer[consumed..], current) {
                    Ok(0) => return Err(Error::UnexpectedEof),
                    Ok(read) => consumed += read,
                    Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                    Err(error) => return Err(error.into()),
                }
            }
            Ok(())
        }
    }

    #[cfg(test)]
    mod tests {
        use super::{MatrixReadPool, MatrixRegionReader};
        use std::io::Write;

        /// The bytes must not depend on which file object carried the read, and
        /// a thread must reuse its private handle rather than opening one per
        /// read.
        #[test]
        fn a_pooled_read_returns_the_same_bytes_as_a_direct_one() {
            let directory = tempfile::tempdir().expect("temp directory");
            let path = directory.path().join("region");
            let contents: Vec<u8> = (0..4096u32).map(|byte| byte as u8).collect();
            {
                let mut file = std::fs::File::create(&path).expect("create");
                file.write_all(&contents).expect("write");
            }
            let file = std::fs::File::open(&path).expect("open");
            let pool = MatrixReadPool::new();

            let mut direct = [0u8; 64];
            MatrixRegionReader::new(&file)
                .read_exact_at(1000, &mut direct)
                .expect("direct read");
            let mut pooled = [0u8; 64];
            let reader = MatrixRegionReader::with_pool(&file, &pool);
            for _ in 0..16 {
                reader
                    .read_exact_at(1000, &mut pooled)
                    .expect("pooled read");
                assert_eq!(direct, pooled);
            }
            assert_eq!(&direct[..], &contents[1000..1064]);

            // One private handle for this thread and its sixteen reads — the
            // cold path runs once per thread per file, never per read. On
            // platforms where `pread` needs no private handle the pool
            // deliberately holds none and the reads above went through the
            // shared handle, which is why this is not asserted as `> 0`.
            let handles = pool.private_handle_count();
            assert!(
                handles <= 1,
                "expected at most one private handle for one thread, found {handles}"
            );
            #[cfg(windows)]
            assert_eq!(handles, 1, "windows must hand this thread its own handle");
        }

        /// Two threads reading through one pooled reader get their own file
        /// objects and their own correct bytes.
        #[test]
        fn two_threads_get_two_private_handles_and_the_right_bytes() {
            let directory = tempfile::tempdir().expect("temp directory");
            let path = directory.path().join("region");
            let contents: Vec<u8> = (0..8192u32).map(|byte| (byte / 7) as u8).collect();
            {
                let mut file = std::fs::File::create(&path).expect("create");
                file.write_all(&contents).expect("write");
            }
            let file = std::fs::File::open(&path).expect("open");
            let pool = MatrixReadPool::new();
            let reader = MatrixRegionReader::with_pool(&file, &pool);
            let contents = &contents;

            std::thread::scope(|scope| {
                for thread in 0..2u64 {
                    scope.spawn(move || {
                        for step in 0..64u64 {
                            let offset = (thread * 512 + step * 13) % 4096;
                            let mut bytes = [0u8; 32];
                            reader
                                .read_exact_at(offset, &mut bytes)
                                .expect("threaded read");
                            let start = offset as usize;
                            assert_eq!(&bytes[..], &contents[start..start + 32]);
                        }
                    });
                }
            });

            #[cfg(windows)]
            assert_eq!(
                pool.private_handle_count(),
                2,
                "each reading thread gets its own file object"
            );
            #[cfg(not(windows))]
            assert_eq!(pool.private_handle_count(), 0);
        }

        /// Dropping the pool closes the private handles even though the
        /// thread-local cache that pointed at them outlives it: the cache holds
        /// `Weak`s. If it held `Arc`s this would leak one handle per thread per
        /// file for the life of the process.
        #[test]
        fn dropping_the_pool_releases_the_private_handles() {
            let directory = tempfile::tempdir().expect("temp directory");
            let path = directory.path().join("region");
            std::fs::write(&path, vec![9u8; 512]).expect("write");
            let file = std::fs::File::open(&path).expect("open");

            let weak = {
                let pool = MatrixReadPool::new();
                let mut bytes = [0u8; 16];
                MatrixRegionReader::with_pool(&file, &pool)
                    .read_exact_at(0, &mut bytes)
                    .expect("pooled read");
                assert_eq!(bytes, [9u8; 16]);
                pool.first_private_handle_weak()
            };
            assert!(
                weak.is_none_or(|handle| handle.upgrade().is_none()),
                "a private handle outlived the pool that owns it"
            );
        }
    }
}

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
/// This map is a *secondary* source. A verifying open *streams* the union of it
/// and the persisted page index in `O(Q)` for `Q` candidate pages (see
/// [`for_each_candidate_page`]), never `O(cell_count)`, and materialises no list
/// of them.
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

    /// Visits every `unit`-sized slot of `[base, base + len)` that overlaps an
    /// allocated range, in ascending order.
    ///
    /// PERF-01: the walk is over ranges, not over slots, so its cost is
    /// proportional to the *allocated* part of the region and never to the
    /// region's logical size. This is what lets verification enumerate the pages
    /// that may hold bytes without a `0..page_count` loop.
    ///
    /// A callback rather than an `out: &mut Vec<u64>` (0.5.0): the collected form
    /// was `Theta(A)` retained memory in a pass whose whole contract is that it
    /// retains one page buffer, and nothing needs the units twice.
    fn for_each_allocated_unit(
        &self,
        base: u64,
        len: u64,
        unit: u64,
        unit_count: u64,
        visit: &mut dyn FnMut(u64) -> Result<()>,
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
            for unit_index in first..=last {
                visit(unit_index)?;
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
    #[cfg(test)]
    if FAIL_NEXT_ZERO_RANGE.replace(false) {
        return Err(Error::Io(std::io::Error::other(
            "injected matrix zero-range failure",
        )));
    }

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
    /// A-3: the first disk mutation a whole-category clear performs.
    ///
    /// `clear_category` writes through `zero_range` and never through
    /// `write_bitmap_byte`, so neither existing injector can make it fail —
    /// which is why the ordering it establishes went unpinned. The clear's
    /// region half must run before the chunk half touches anything, and the
    /// only way to observe that is to fail the region half.
    static FAIL_NEXT_ZERO_RANGE: std::cell::Cell<bool> = const {
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
        /// Bytes read by demand fault-ins under
        /// `MatrixMetadataResidency::Lazy`, so the working-set cost of reading
        /// K pages is measurable and not merely argued.
        pub(super) static LAZY_FAULT_BYTES_READ: Cell<u64> = const { Cell::new(0) };
        /// Payload bytes the demand caches currently hold.
        pub(super) static LAZY_CACHED_BYTES_RESIDENT: Cell<u64> = const { Cell::new(0) };
        /// Hash-map probes spent maintaining the demand cache's recency order
        /// ([`super::PageStore::note_used`] and its two link helpers).
        ///
        /// The unit is deliberately "one map probe", because that is what the
        /// linear predecessor spent per *comparison*: it makes "constant per
        /// touch" and "proportional to the cached page count" the same
        /// measurement in the same units.
        pub(super) static LRU_TOUCH_STEPS: Cell<u64> = const { Cell::new(0) };
        /// Bitmap bytes actually written to the file by
        /// [`super::write_bitmap_byte`] — one per `seek` + one-byte `write_all`
        /// pair that was issued rather than skipped.
        pub(super) static BITMAP_BYTE_WRITES: Cell<u64> = const { Cell::new(0) };
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
        /// Page-store locks this thread holds *right now*
        /// ([`super::PageStoreGuard`]). Not a total: it rises and falls.
        pub(super) static BITMAP_STORE_GUARDS_HELD: Cell<u64> = const { Cell::new(0) };
        /// Slot bytes the explicit CRC commit-map rebuild fed to `crc32` on
        /// this thread. The rebuild's I/O follows the CRC-validity evidence, so
        /// this measures the sweep's cost against the *set* bits rather than
        /// against the whole keyspace it still visits.
        pub(super) static REBUILD_SLOT_BYTES_READ: Cell<u64> = const { Cell::new(0) };
        /// Slot bytes streamed back off the disk on this thread by the two
        /// slot readers — the zero probe and the checksum pass. `commit_cell`
        /// runs both, and the explicit rebuild's sweep runs the second, so this
        /// is every read-back and not only the ones a mutation caused.
        pub(super) static SLOT_BYTES_READ_BACK: Cell<u64> = const { Cell::new(0) };
        /// Positional matrix-region reads issued on this thread
        /// ([`super::MatrixRegionReader::read_exact_at`]).
        pub(super) static MATRIX_REGION_READS: Cell<u64> = const { Cell::new(0) };
        /// Of those, the ones issued while a page-store lock was held. The
        /// read-path contract is that this stays zero; see
        /// [`super::MatrixRecoveryReport::matrix_region_reads_under_bitmap_lock`].
        pub(super) static MATRIX_REGION_READS_UNDER_LOCK: Cell<u64> = const { Cell::new(0) };
        /// Of those, the ones issued by a reader built without the file's
        /// [`super::MatrixReadPool`]. See
        /// [`super::MatrixRecoveryReport::matrix_region_reads_without_pool`].
        pub(super) static MATRIX_REGION_READS_WITHOUT_POOL: Cell<u64> = const { Cell::new(0) };
    }

    pub(super) fn add(cell: &'static std::thread::LocalKey<Cell<u64>>, value: u64) {
        cell.with(|counter| counter.set(counter.get().saturating_add(value)));
    }

    pub(super) fn sub(cell: &'static std::thread::LocalKey<Cell<u64>>, value: u64) {
        cell.with(|counter| counter.set(counter.get().saturating_sub(value)));
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
fn count_slot_bytes_read_back(bytes: u64) {
    scaling_counters::add(&scaling_counters::SLOT_BYTES_READ_BACK, bytes);
}

#[cfg(not(any(test, feature = "scalable-fault-injection")))]
fn count_slot_bytes_read_back(_bytes: u64) {}

#[cfg(any(test, feature = "scalable-fault-injection"))]
fn count_lru_touch_steps(steps: u64) {
    scaling_counters::add(&scaling_counters::LRU_TOUCH_STEPS, steps);
}

#[cfg(not(any(test, feature = "scalable-fault-injection")))]
fn count_lru_touch_steps(_steps: u64) {}

#[cfg(any(test, feature = "scalable-fault-injection"))]
fn count_bitmap_byte_write() {
    scaling_counters::add(&scaling_counters::BITMAP_BYTE_WRITES, 1);
}

#[cfg(not(any(test, feature = "scalable-fault-injection")))]
fn count_bitmap_byte_write() {}

#[cfg(any(test, feature = "scalable-fault-injection"))]
fn count_open_bitmap_pages_visited(pages: u64) {
    scaling_counters::add(&scaling_counters::OPEN_BITMAP_PAGES_VISITED, pages);
}

#[cfg(not(any(test, feature = "scalable-fault-injection")))]
fn count_open_bitmap_pages_visited(_pages: u64) {}

#[cfg(any(test, feature = "scalable-fault-injection"))]
fn count_rebuild_slot_bytes_read(bytes: u64) {
    scaling_counters::add(&scaling_counters::REBUILD_SLOT_BYTES_READ, bytes);
}

#[cfg(not(any(test, feature = "scalable-fault-injection")))]
fn count_rebuild_slot_bytes_read(_bytes: u64) {}

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
fn count_lazy_fault_bytes_read(bytes: u64) {
    scaling_counters::add(&scaling_counters::LAZY_FAULT_BYTES_READ, bytes);
}

#[cfg(not(any(test, feature = "scalable-fault-injection")))]
fn count_lazy_fault_bytes_read(_bytes: u64) {}

/// Records that this thread has acquired one more page-store lock.
///
/// Called by [`PageStoreGuard::new`] and paired with
/// [`leave_page_store_guard`] by its `Drop`, so the depth is exact rather than
/// approximate: a `MutexGuard` is `!Send`, so acquisition and release always
/// happen on one thread and a thread-local count needs no synchronisation and
/// cannot be attributed to the wrong reader.
///
/// This exists so that the matrix read path's central concurrency invariant —
/// **no read is issued while a page-store lock is held** — is a counted fact.
/// It replaces a wall-clock ratio that could not distinguish a convoy from a
/// busy machine.
#[cfg(any(test, feature = "scalable-fault-injection"))]
fn enter_page_store_guard() {
    scaling_counters::add(&scaling_counters::BITMAP_STORE_GUARDS_HELD, 1);
}

#[cfg(not(any(test, feature = "scalable-fault-injection")))]
fn enter_page_store_guard() {}

#[cfg(any(test, feature = "scalable-fault-injection"))]
fn leave_page_store_guard() {
    scaling_counters::sub(&scaling_counters::BITMAP_STORE_GUARDS_HELD, 1);
}

#[cfg(not(any(test, feature = "scalable-fault-injection")))]
fn leave_page_store_guard() {}

/// Counts one positional matrix-region read, and separately counts it as a
/// violation when a page-store lock is held while it is issued.
///
/// The whole matrix region — cell payloads, per-cell checksums, aux ranges,
/// bitmap pages and page digests — is read through
/// [`MatrixRegionReader::read_exact_at`], so this sees every read the matrix
/// performs. What it does not see is I/O issued through any other handle:
/// `SnapshotFile` reads of the *record* region, and the writer's
/// cursor-relative `&mut File` writes.
///
/// `pooled` is whether the reader that issued it carries the file's
/// [`MatrixReadPool`]. A read issued without one is not wrong — it returns the
/// same bytes — but on Windows it goes through the shared file object, which is
/// what convoys concurrent readers, so a site that *has* a pool and drops it is
/// a defect this counts rather than argues about.
#[cfg(any(test, feature = "scalable-fault-injection"))]
fn count_matrix_region_read(pooled: bool) {
    scaling_counters::add(&scaling_counters::MATRIX_REGION_READS, 1);
    if !pooled {
        scaling_counters::add(&scaling_counters::MATRIX_REGION_READS_WITHOUT_POOL, 1);
    }
    if scaling_counters::get_always(&scaling_counters::BITMAP_STORE_GUARDS_HELD) != 0 {
        scaling_counters::add(&scaling_counters::MATRIX_REGION_READS_UNDER_LOCK, 1);
    }
}

#[cfg(not(any(test, feature = "scalable-fault-injection")))]
fn count_matrix_region_read(_pooled: bool) {}

/// Tracks the live demand-cache total, so criterion (B)'s "residency tracks the
/// working set, in both directions" is a measured number rather than a claim.
///
/// A single thread-local total across every lazily backed bitmap: the caches
/// are per-bitmap but the bound that matters to a caller is what the process
/// holds. Each cache reports its own new total, so this is the last reporter's
/// value plus nothing — accurate for the single-matrix case the counters exist
/// to measure, and documented as such.
#[cfg(any(test, feature = "scalable-fault-injection"))]
fn record_lazy_cached_bytes(bytes: u64) {
    scaling_counters::set(&scaling_counters::LAZY_CACHED_BYTES_RESIDENT, bytes);
}

#[cfg(not(any(test, feature = "scalable-fault-injection")))]
fn record_lazy_cached_bytes(_bytes: u64) {}

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
        .map(|commit| commit.bits.resident_bytes())
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
    // Re-baselined per open, so a lazy open reports the residency *it* holds
    // rather than what a previous session's cache left behind.
    let cached: u64 = layout
        .commits
        .iter()
        .map(|commit| commit.bits.cached_bytes())
        .fold(0u64, u64::saturating_add);
    scaling_counters::set(&scaling_counters::LAZY_CACHED_BYTES_RESIDENT, cached);
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

/// Fails the next range-zeroing write, which is how a whole-category clear
/// mutates the file. See [`FAIL_NEXT_ZERO_RANGE`].
#[cfg(test)]
pub(crate) fn inject_zero_range_failure() {
    FAIL_NEXT_ZERO_RANGE.set(true);
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
        scaling_counters::get(&scaling_counters::OPEN_BITMAP_BYTES_RESIDENT).saturating_add(
            scaling_counters::get(&scaling_counters::LAZY_CACHED_BYTES_RESIDENT),
        )
    }

    /// Payload bytes held by matrix commit-map *demand caches* on this thread
    /// (`MatrixMetadataResidency::Lazy`).
    ///
    /// Criterion (B): this is the term that tracks the working set. It is zero
    /// immediately after a lazy open however large the file or its live page
    /// set, rises by one page per distinct page addressed, and never exceeds
    /// the declared `cache_bytes` rounded up to a whole page — the least
    /// recently used page is dropped instead. It is *not* counted by
    /// [`MatrixRecoveryReport::matrix_open_resident_bitmap_bytes`], which
    /// reports the budget-charged term alone;
    /// [`MatrixRecoveryReport::matrix_resident_bitmap_bytes`] reports the sum.
    pub fn matrix_lazy_cached_bitmap_bytes() -> u64 {
        scaling_counters::get(&scaling_counters::LAZY_CACHED_BYTES_RESIDENT)
    }

    /// Bytes read by matrix commit-map demand fault-ins on this thread.
    ///
    /// The cost a lazy open defers. Reading `K` distinct pages costs
    /// `K * (4096 + 8)` here — the page and its digest — and re-reading a page
    /// still cached costs nothing.
    pub fn matrix_lazy_fault_bytes_read() -> u64 {
        scaling_counters::get(&scaling_counters::LAZY_FAULT_BYTES_READ)
    }

    /// Hash-map probes spent keeping the demand cache's recency order, on this
    /// thread.
    ///
    /// The recency update runs on **every** matrix cell read and write that
    /// touches a demand-cached page — `cell_status` reaches it through
    /// `SparseBitmap::byte`, and so does the first statement of
    /// `prepare_byte_write` — so its per-touch cost is the one term in the
    /// matrix hot path that used to follow the *cache size* rather than the
    /// working set. It is now bounded by a small constant: the recency order is
    /// an intrusive doubly-linked list threaded through the cached pages
    /// themselves, so a touch is at most six map probes and a re-read of the
    /// most recently used page is none at all.
    ///
    /// The unit is one probe, which is also what the linear predecessor spent
    /// per comparison of its `VecDeque` scan, so the two are directly
    /// comparable: 512 cached pages cost ~256 there and at most 6 here.
    pub fn matrix_lru_touch_steps() -> u64 {
        scaling_counters::get(&scaling_counters::LRU_TOUCH_STEPS)
    }

    /// Bitmap bytes this thread actually wrote to a matrix file.
    ///
    /// One per `seek` + one-byte `write_all` pair that `write_bitmap_byte`
    /// issued. A mutation whose byte already holds the value being written
    /// stores nothing and counts nothing, so the difference between this and
    /// the number of bit mutations attempted is exactly the redundant I/O.
    pub fn matrix_bitmap_byte_writes() -> u64 {
        scaling_counters::get(&scaling_counters::BITMAP_BYTE_WRITES)
    }

    /// Slot bytes read and hashed by the explicit CRC commit-map rebuild
    /// ([`crate::VarveFile::rebuild_matrix_commit_from_crc`]) on this thread.
    ///
    /// The rebuild visits the whole keyspace — the published recovery model
    /// requires it — but a cell whose CRC-validity bit is clear cannot be
    /// rebuilt as committed whatever its slot holds, so its slot is not read.
    /// This counter is therefore `set bits * slot_stride`, not
    /// `cell count * slot_stride`, and where every bit is set the two coincide.
    pub fn matrix_rebuild_slot_bytes_read() -> u64 {
        scaling_counters::get(&scaling_counters::REBUILD_SLOT_BYTES_READ)
    }

    /// Slot bytes streamed back off the disk on this thread.
    ///
    /// `commit_cell` has two reasons to re-read the slot it is committing: the
    /// zero probe that distinguishes "never written" from "written zeros", and
    /// the checksum pass that records the cell's CRC. The first is already
    /// skipped for a cell this session wrote; the second is not, which is what
    /// this counter exists to keep visible. The explicit rebuild's checksum
    /// sweep goes through the same reader and is counted here too, so a
    /// measurement about mutation should reset immediately before it.
    pub fn matrix_slot_bytes_read_back() -> u64 {
        scaling_counters::get(&scaling_counters::SLOT_BYTES_READ_BACK)
    }

    /// Positional matrix-region reads issued on this thread.
    ///
    /// Every byte the matrix reads — cell payloads, per-cell checksums, aux
    /// ranges, commit-map pages and page digests — is fetched by one
    /// `read_exact_at`, and each of those counts one here. A test uses this to
    /// prove it actually exercised the read path before believing
    /// [`MatrixRecoveryReport::matrix_region_reads_under_bitmap_lock`]: zero
    /// violations out of zero reads is not evidence of anything.
    pub fn matrix_region_reads() -> u64 {
        scaling_counters::get(&scaling_counters::MATRIX_REGION_READS)
    }

    /// Of those reads, the number issued while this thread held a commit-map
    /// page-store lock. **The contract is that this is always zero.**
    ///
    /// This is the deterministic form of criterion (C)'s convoy check. Demand
    /// loading gave each bitmap a `Mutex` over its page map so a fault-in can
    /// happen under `&self`; the property that keeps concurrent readers from
    /// serialising is not that the lock is absent but that it is never held
    /// across I/O. A nonzero value here is precisely a reader parked on a
    /// `pread` while holding a lock every other reader of the same category
    /// needs.
    ///
    /// What it catches: any read — fault-in, aggregate, cell payload, checksum,
    /// aux — issued inside a page-store critical section, on any platform, at
    /// any machine load, from one thread. It needs no second thread to observe
    /// a violation, and no quiet machine.
    ///
    /// What it does *not* catch: contention that is genuine but lock-free (the
    /// Windows file-object convoy `MatrixReadPool` exists to break leaves this
    /// at zero), a lock held across a long *computation* rather than I/O, I/O
    /// issued through a handle other than the matrix region reader, and any
    /// question of throughput. Scaling is still worth measuring; it is just not
    /// assertable on a shared runner.
    pub fn matrix_region_reads_under_bitmap_lock() -> u64 {
        scaling_counters::get(&scaling_counters::MATRIX_REGION_READS_UNDER_LOCK)
    }

    /// Of those reads, the number issued by a reader that carried no
    /// `MatrixReadPool`.
    ///
    /// Not every such read is a defect: the open, verify and rebuild paths hold
    /// the file as `&mut File`, so no other reader can be in flight and no pool
    /// exists to hand them. What this catches is a `&self` read path that *is*
    /// holding a pool — every backed commit map holds one in its `LazyBacking`
    /// — and issues its reads through the shared file object anyway, which is
    /// the Windows convoy the pool exists to break.
    ///
    /// It is a count of issued reads, not of contention: on Unix
    /// `MatrixReadPool::reopen` returns `None` by design (`pread` does not
    /// serialise on the file object), so a pooled read there is byte-identical
    /// and costs one thread-local lookup for a handle it will never get. This
    /// counter is about which reader was used, and is meaningful on both
    /// platforms for that reason; the throughput it protects is Windows-only.
    pub fn matrix_region_reads_without_pool() -> u64 {
        scaling_counters::get(&scaling_counters::MATRIX_REGION_READS_WITHOUT_POOL)
    }

    /// Commit-map page-store locks this thread holds at this instant.
    ///
    /// Zero at every point outside `varve-core`'s own matrix code, so a test can
    /// assert it after a run and know the depth it audited was balanced rather
    /// than leaked by an early return.
    pub fn matrix_bitmap_store_guards_held() -> u64 {
        scaling_counters::get(&scaling_counters::BITMAP_STORE_GUARDS_HELD)
    }

    /// Resets the two lock-audit totals for the calling thread.
    ///
    /// Deliberately leaves `matrix_bitmap_store_guards_held` alone: that is a
    /// live depth, not a total, and zeroing it mid-flight would make the audit
    /// lie in the one direction that matters.
    pub fn reset_matrix_lock_audit_counters() {
        scaling_counters::set(&scaling_counters::MATRIX_REGION_READS, 0);
        scaling_counters::set(&scaling_counters::MATRIX_REGION_READS_UNDER_LOCK, 0);
        scaling_counters::set(&scaling_counters::MATRIX_REGION_READS_WITHOUT_POOL, 0);
    }

    /// Resets the two demand-loading counters, so a test can measure one phase
    /// of a session rather than its total.
    pub fn reset_matrix_lazy_counters() {
        scaling_counters::set(&scaling_counters::LAZY_FAULT_BYTES_READ, 0);
        scaling_counters::set(&scaling_counters::LAZY_CACHED_BYTES_RESIDENT, 0);
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
        scaling_counters::set(&scaling_counters::LAZY_FAULT_BYTES_READ, 0);
        scaling_counters::set(&scaling_counters::LAZY_CACHED_BYTES_RESIDENT, 0);
        scaling_counters::set(&scaling_counters::LRU_TOUCH_STEPS, 0);
        scaling_counters::set(&scaling_counters::BITMAP_BYTE_WRITES, 0);
        scaling_counters::set(&scaling_counters::REBUILD_SLOT_BYTES_READ, 0);
        scaling_counters::set(&scaling_counters::SLOT_BYTES_READ_BACK, 0);
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
    /// True when this page is held by the demand cache
    /// ([`MatrixMetadataResidency::Lazy`]) rather than admitted through
    /// `ReadLimitKey::MatrixBitmapBytes`.
    ///
    /// The distinction is an accounting one and it matters in exactly one
    /// place: `MatrixLayout::resident_bitmap_bytes` is a running total of the
    /// bytes the *budget* granted, and a whole-category clear subtracts
    /// `resident_bytes()` from it. A demand-cached page was never charged to
    /// that total, so counting it there would underflow the subtraction and
    /// turn a legal clear into `InvalidMatrixLayout`.
    cached: bool,
    /// Neighbours in the demand cache's recency chain, or [`NO_PAGE`].
    ///
    /// The chain is intrusive: it lives in the pages themselves rather than in
    /// a side container, which is what makes "this page was just used" and
    /// "this page is gone" both `O(1)` and both impossible to get out of step
    /// with `PageStore::pages` — the entry *is* the page.
    ///
    /// Meaningful only while `cached` is true. A page the writer materialised
    /// carries `cached: false` and is never linked, so [`PageStore::evict_one`]
    /// cannot reach it.
    lru_prev: u64,
    lru_next: u64,
}

/// Sentinel end of the recency chain in [`BitmapPage::lru_prev`] /
/// [`BitmapPage::lru_next`] and [`PageStore::lru_head`] / [`PageStore::lru_tail`].
///
/// Legal page numbers are bounded by `SparseBitmap::page_count` and by
/// [`PAGE_INDEX_MAX_ENTRIES`], so `u64::MAX` is never one of them.
const NO_PAGE: u64 = u64::MAX;

/// Where a lazily loaded bitmap's pages come from (PERF-01 / criterion B).
///
/// Present only under [`MatrixMetadataResidency::Lazy`]. `file` is a *separate*
/// descriptor for the same file, duplicated once at open, so a fault-in is a
/// positional read that needs no mutable borrow of the writer's handle and no
/// reader threaded through twenty call sites. Session-only maps
/// (`current_write_bits`, rebuilt maps) never get one, which is what makes
/// "not cached" and "not published" mechanically distinct: a bitmap with no
/// backing has nothing to fault in and answers from memory alone, exactly as
/// before this option existed.
#[derive(Clone, Debug)]
struct LazyBacking {
    file: Arc<File>,
    /// See [`LazyResidency::pool`]: without it, concurrent fault-ins convoy on
    /// the shared file object on Windows.
    pool: Arc<MatrixReadPool>,
    base_offset: u64,
    digest_base: Option<u64>,
    /// Ceiling on `PageStore::cached_bytes`, always a whole number of pages
    /// and never zero.
    cache_limit: u64,
}

/// The pages of one bitmap, plus every total derived from them.
///
/// Behind a `Mutex` inside [`SparseBitmap`] so a fault-in can happen under
/// `&self` — matrix reads take `&self` (criterion C) and demand loading has to
/// live inside that borrow. Every `&mut self` mutator reaches it through
/// [`SparseBitmap::store_mut`], which is `Mutex::get_mut` and takes no lock at
/// all, so the mutation engine's F-03/F-05 prepare-then-commit discipline is
/// untouched and no lock is taken on the write path.
///
/// # What the lock does and does not serialise
///
/// It is taken only for `O(1)` map operations and is **never held across
/// I/O** — see [`SparseBitmap::faulted_store`], which drops it for the whole
/// of the fault-in read, and [`SparseBitmap::ones_total`], which takes it once
/// per page rather than once per aggregate. So concurrent readers serialise on
/// a hash lookup, not on a `pread`. Stated because "there is no lock in the
/// matrix read path" was true of round 16 and is no longer true of this one:
/// there is one, and it is per-bitmap.
///
/// The `O(1)` in that first sentence is a claim about the *recency update*, and
/// it was false until the demand cache's least-recently-used order stopped
/// being a `VecDeque` beside the map. A touch scanned the deque for the page
/// and then memmoved it, under this lock, on every cell read and every cell
/// write — `O(cached pages)`, up to the 512-page default cache and up to 16,384
/// at the ceiling a 64 MiB `max_matrix_bitmap_bytes` admits. The order is now an
/// intrusive doubly-linked list threaded through [`BitmapPage`] itself
/// ([`PageStore::note_used`]), so the critical section really is a bounded
/// number of map probes; [`MatrixRecoveryReport::matrix_lru_touch_steps`]
/// counts them so the sentence is measured rather than asserted.
///
/// That "never" is *counted*, not argued: acquiring the lock goes through
/// [`PageStoreGuard`], every matrix read goes through
/// [`MatrixRegionReader::read_exact_at`], and the second increments a violation
/// counter if the first is outstanding. `page_store_lock_audit_tests` below
/// asserts the counter is zero for the fault-in and the aggregate, and asserts
/// separately that it is *not* zero when a read is deliberately issued under
/// the lock, so a passing run means the detector was live. The wall-clock ratio
/// that used to stand in for this is now a printed measurement in
/// `crates/varve/tests/matrix_concurrent_reads.rs`, because two runs of it on
/// identical code differ by 1.4x versus 2.1x on a shared CI runner and a
/// threshold that survives that noise would also pass a real convoy.
#[derive(Debug)]
struct PageStore {
    pages: HashMap<u64, BitmapPage>,
    ones: u64,
    /// Payload bytes admitted through the resident bitmap budget.
    charged_bytes: u64,
    /// Payload bytes held by the demand cache.
    cached_bytes: u64,
    /// Least-recently-used end of the intrusive recency chain, or [`NO_PAGE`].
    ///
    /// This and [`Self::lru_tail`] replace the `VecDeque<u64>` this store used
    /// to keep beside `pages`. Two things were wrong with that, and they were
    /// the same thing: the order lived in a container of its own. Finding the
    /// entry for a page was `O(cached pages)` — a scan plus a memmove on
    /// *every* cell read and write, since `SparseBitmap::byte` is the first
    /// statement of both paths — and releasing a page from `pages` did not
    /// release its entry, so the deque grew without bound over set/clear churn
    /// and was charged against no limit. Threading the links through the pages
    /// themselves removes both: a touch is a fixed number of map probes, and
    /// there is no second container left to fall out of step.
    lru_head: u64,
    lru_tail: u64,
}

impl Default for PageStore {
    fn default() -> Self {
        Self {
            pages: HashMap::new(),
            ones: 0,
            charged_bytes: 0,
            cached_bytes: 0,
            lru_head: NO_PAGE,
            lru_tail: NO_PAGE,
        }
    }
}

impl PageStore {
    /// Removes `page` from the recency chain, if it is on it.
    ///
    /// Three map probes at most: the page, its predecessor and its successor.
    /// Idempotent — an unlinked page has both neighbours set to [`NO_PAGE`] and
    /// is not the head, which is the state this leaves behind.
    fn lru_unlink(&mut self, page: u64) {
        count_lru_touch_steps(1);
        let (prev, next) = {
            let Some(held) = self.pages.get_mut(&page) else {
                return;
            };
            if held.lru_prev == NO_PAGE && held.lru_next == NO_PAGE && self.lru_head != page {
                return;
            }
            let prev = std::mem::replace(&mut held.lru_prev, NO_PAGE);
            let next = std::mem::replace(&mut held.lru_next, NO_PAGE);
            (prev, next)
        };
        if prev == NO_PAGE {
            self.lru_head = next;
        } else {
            count_lru_touch_steps(1);
            if let Some(before) = self.pages.get_mut(&prev) {
                before.lru_next = next;
            }
        }
        if next == NO_PAGE {
            self.lru_tail = prev;
        } else {
            count_lru_touch_steps(1);
            if let Some(after) = self.pages.get_mut(&next) {
                after.lru_prev = prev;
            }
        }
    }

    /// Links `page` at the most-recently-used end. Two map probes at most.
    ///
    /// The caller must have unlinked it first; only `cached` pages are ever
    /// passed here, which is what keeps a writer-materialised page out of
    /// [`Self::evict_one`]'s reach.
    fn lru_push_back(&mut self, page: u64) {
        let tail = self.lru_tail;
        count_lru_touch_steps(1);
        let Some(held) = self.pages.get_mut(&page) else {
            return;
        };
        held.lru_prev = tail;
        held.lru_next = NO_PAGE;
        if tail == NO_PAGE {
            self.lru_head = page;
        } else {
            count_lru_touch_steps(1);
            if let Some(before) = self.pages.get_mut(&tail) {
                before.lru_next = page;
            }
        }
        self.lru_tail = page;
    }

    /// Moves `page` to the most-recently-used end.
    ///
    /// On the hot path by construction: every matrix cell read reaches it
    /// through `SparseBitmap::byte`, and so does every cell write, because
    /// `prepare_byte_write` reads the current byte first. The tail test makes
    /// the commonest case — re-reading the page just read — cost nothing at
    /// all, and the rest is bounded by six map probes whatever the cache
    /// holds.
    fn note_used(&mut self, page: u64) {
        if self.lru_tail == page {
            return;
        }
        count_lru_touch_steps(1);
        if self.pages.get(&page).is_some_and(|held| held.cached) {
            self.lru_unlink(page);
            self.lru_push_back(page);
        }
    }

    /// Drops one demand-cached page. Returns false when the cache is empty.
    ///
    /// Safe because demand-cached pages are write-through: every mutation
    /// writes the bitmap byte to disk before `commit_byte_write` updates
    /// memory, so a cached page never holds state the file does not.
    ///
    /// No skip loop: the chain holds exactly the demand-cached pages `pages`
    /// currently holds, so its head is always evictable.
    fn evict_one(&mut self) -> bool {
        let page = self.lru_head;
        if page == NO_PAGE {
            return false;
        }
        self.lru_unlink(page);
        let Some(held) = self.pages.remove(&page) else {
            // Unreachable: the chain names only pages `pages` holds.
            return false;
        };
        self.cached_bytes = self
            .cached_bytes
            .saturating_sub(usize_to_u64(held.bytes.len()).unwrap_or(0));
        self.ones = self.ones.saturating_sub(held.ones);
        true
    }

    /// Pages currently on the recency chain, walked forwards.
    ///
    /// Test-only, and deliberately bounded by `pages.len()`: a chain longer
    /// than the map it is threaded through is the corruption this
    /// representation has to be proved free of, and an unbounded walk would
    /// hang instead of failing.
    #[cfg(test)]
    fn lru_len(&self) -> usize {
        let mut len = 0;
        let mut page = self.lru_head;
        while page != NO_PAGE {
            len += 1;
            assert!(
                len <= self.pages.len(),
                "recency chain is longer than the page map it is threaded through"
            );
            page = self.pages.get(&page).map_or(NO_PAGE, |held| held.lru_next)
        }
        len
    }

    /// Asserts the chain is a well-formed doubly-linked list over exactly the
    /// demand-cached pages of `pages`.
    #[cfg(test)]
    fn assert_lru_consistent(&self) {
        let forwards = self.lru_len();
        let cached = self.pages.values().filter(|held| held.cached).count();
        assert_eq!(
            forwards, cached,
            "the recency chain and the cached page set disagree"
        );
        let mut backwards = 0;
        let mut page = self.lru_tail;
        let mut seen_next = NO_PAGE;
        while page != NO_PAGE {
            backwards += 1;
            let held = self.pages.get(&page).expect("chain names a resident page");
            assert!(held.cached, "the recency chain names an uncached page");
            assert_eq!(held.lru_next, seen_next, "forward link disagrees");
            seen_next = page;
            page = held.lru_prev;
            assert!(backwards <= cached, "the recency chain loops");
        }
        assert_eq!(
            forwards, backwards,
            "the chain has different lengths by end"
        );
        assert_eq!(
            self.lru_head, seen_next,
            "head is not the chain's first page"
        );
    }
}

/// A held page-store lock.
///
/// A newtype over `MutexGuard` for exactly one reason: it makes "this thread is
/// holding a page-store lock right now" observable, which turns the read path's
/// concurrency contract into something a test can *count* rather than time.
/// The contract is
///
/// > no matrix-region read is issued while a page-store lock is held,
///
/// and it is what makes concurrent readers scale: they contend for `O(1)` hash
/// lookups, never for each other's `pread`. `matrix.rs`'s own
/// `page_store_lock_audit_tests` assert it directly, on every platform and
/// under any machine load, which a wall-clock ratio on a shared CI runner
/// cannot do.
///
/// The cost, stated exactly: one field, no extra state, and two calls whose
/// bodies are empty without `cfg(test)` or the `scalable-fault-injection`
/// feature. What remains in an ordinary build is a newtype with an empty `Drop`
/// — not zero source, but zero work, and no lock is held for one instruction
/// longer than the `MutexGuard` alone would be.
struct PageStoreGuard<'a> {
    inner: MutexGuard<'a, PageStore>,
}

impl<'a> PageStoreGuard<'a> {
    fn new(inner: MutexGuard<'a, PageStore>) -> Self {
        enter_page_store_guard();
        Self { inner }
    }
}

impl std::ops::Deref for PageStoreGuard<'_> {
    type Target = PageStore;

    fn deref(&self) -> &PageStore {
        &self.inner
    }
}

impl std::ops::DerefMut for PageStoreGuard<'_> {
    fn deref_mut(&mut self) -> &mut PageStore {
        &mut self.inner
    }
}

impl Drop for PageStoreGuard<'_> {
    fn drop(&mut self) {
        leave_page_store_guard();
    }
}

/// One bitmap page's bytes, however they are held.
///
/// Replaces the `&[u8]` `page_bytes` used to return: the pages now live behind
/// a lock, so a borrow cannot outlive the guard. `Arc` makes the resident case
/// a refcount bump rather than a copy.
enum PagePayload {
    Resident(Arc<Vec<u8>>),
    Zero(usize),
}

impl std::ops::Deref for PagePayload {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        match self {
            Self::Resident(bytes) => bytes.as_slice(),
            Self::Zero(len) => &ZERO_PAGE[..*len],
        }
    }
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
///
/// `emptied` and `released` are **different questions**, and the callers ask
/// each of them. `released` is a budget quantity: how many bytes went back to
/// `ReadLimitKey::MatrixBitmapBytes`. `emptied` is a fact about the file: this
/// page is now byte-for-byte the zero page, so its persisted page-index entry
/// describes nothing and can go.
///
/// They came apart on the demand-cached page. A page faulted in by a read was
/// never charged to the layout's resident total, so emptying it refunds the
/// lazy cache and reports `released == 0` — correctly. Both apply sites keyed
/// the *page-index* release on `released != 0`, so a cached page that cleared to
/// zero kept its entry, permanently, while an identical charged page released
/// its own. Every open thereafter reads the entry and materialises a zero page
/// for it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct PageResidencyDelta {
    materialised: u64,
    released: u64,
    emptied: bool,
}

impl PageResidencyDelta {
    const NONE: Self = Self {
        materialised: 0,
        released: 0,
        emptied: false,
    };

    const fn materialised(bytes: u64) -> Self {
        Self {
            materialised: bytes,
            released: 0,
            emptied: false,
        }
    }
}

#[derive(Debug)]
struct SparseBitmap {
    bit_count: u64,
    byte_len: u64,
    page_count: u64,
    store: Mutex<PageStore>,
    /// Set only under [`MatrixMetadataResidency::Lazy`]; see [`LazyBacking`].
    backing: Option<LazyBacking>,
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

/// Hand-written because [`Mutex`] is not `Clone`.
///
/// A clone takes a snapshot of the pages the original currently holds,
/// including its demand-cached ones, and keeps the same backing — cloning a
/// layout must not turn a lazily backed map into one that answers absent pages
/// as zero.
impl Clone for SparseBitmap {
    fn clone(&self) -> Self {
        let source = self.store();
        Self {
            bit_count: self.bit_count,
            byte_len: self.byte_len,
            page_count: self.page_count,
            store: Mutex::new(PageStore {
                pages: source.pages.clone(),
                ones: source.ones,
                charged_bytes: source.charged_bytes,
                cached_bytes: source.cached_bytes,
                // The recency chain rides along inside `pages`: its links are
                // page numbers, which are stable across the clone, so copying
                // the two ends is the whole of it.
                lru_head: source.lru_head,
                lru_tail: source.lru_tail,
            }),
            backing: self.backing.clone(),
            index_slots: self.index_slots.clone(),
            indexed_pages: self.indexed_pages.clone(),
        }
    }
}

impl SparseBitmap {
    fn new(bit_count: u64) -> Result<Self> {
        let byte_len = bit_bytes(bit_count)?;
        Ok(Self {
            bit_count,
            byte_len,
            page_count: page_count_for(byte_len)?,
            store: Mutex::new(PageStore::default()),
            backing: None,
            index_slots: Vec::new(),
            indexed_pages: HashMap::new(),
        })
    }

    /// Locks the page store.
    ///
    /// Poison-tolerant on purpose: a panic while a page was being installed
    /// leaves the store consistent — every mutation is a completed
    /// prepare-then-commit pair — so refusing every later read would convert a
    /// panic elsewhere into permanent unavailability of the matrix.
    fn store(&self) -> PageStoreGuard<'_> {
        PageStoreGuard::new(self.store.lock().unwrap_or_else(|err| err.into_inner()))
    }

    /// The page store under an exclusive borrow, which takes no lock.
    fn store_mut(&mut self) -> &mut PageStore {
        self.store.get_mut().unwrap_or_else(|err| err.into_inner())
    }

    /// Faults `page` in from the backing file if it is published, not resident,
    /// and this bitmap is lazily backed. A no-op in every other case.
    ///
    /// F-06, and the reason this consults `indexed_pages` rather than the
    /// filesystem: the persisted page index is loaded *in full* at open under
    /// both residency policies, and it names exactly the pages that hold state.
    /// A page it does not name has never been published, so answering it as
    /// clear is a fact the file supplied, not an assumption that "absent means
    /// zero". An index that could not be enumerated in full is a fatal finding
    /// at open under both policies, so this is never reached with a partial
    /// one that a caller could mistake for complete.
    /// The page store, with `page` faulted in if it is published, not cached,
    /// and this bitmap is lazily backed.
    ///
    /// # The lock is never held across the read
    ///
    /// Round 16 removed the read convoy from the matrix; this must not put one
    /// back under the new option. The store's `Mutex` is taken twice — once for
    /// the `O(1)` "is it already here?" test and once to install — and dropped
    /// for the whole of [`Self::load_page`], which is where the `pread` and the
    /// digest check happen. Two threads faulting the same page therefore both
    /// read it and one discards its copy, which costs one duplicated 4 KiB read
    /// and never blocks either thread behind the other's I/O.
    ///
    /// The duplicate is harmless because a faulted page is derived state: it is
    /// authenticated against its own stored digest before installation, and
    /// cached pages are write-through, so two loads of one page either agree or
    /// both fail.
    fn faulted_store(&self, page: u64) -> Result<PageStoreGuard<'_>> {
        let Some(backing) = self.backing.as_ref() else {
            return Ok(self.store());
        };
        {
            let mut store = self.store();
            if store.pages.contains_key(&page) {
                store.note_used(page);
                return Ok(store);
            }
            if !self.indexed_pages.contains_key(&page) {
                return Ok(store);
            }
        }
        let loaded = self.load_page(backing, page)?;
        let mut store = self.store();
        if let Some((bytes, ones)) = loaded {
            self.install_faulted_page(&mut store, backing, page, bytes, ones)?;
        }
        Ok(store)
    }

    /// Reads and authenticates one commit-map page. Holds no lock.
    ///
    /// `Ok(None)` means the page is indistinguishable from the zero page, so
    /// caching it would spend the budget on nothing. Not an error: a
    /// crash-interrupted removal can leave an index entry for a page whose bits
    /// have all cleared.
    fn load_page(&self, backing: &LazyBacking, page: u64) -> Result<Option<(Vec<u8>, u64)>> {
        let len = self.page_len(page)?;
        let offset = page
            .checked_mul(BITMAP_PAGE_BYTES)
            .and_then(|delta| backing.base_offset.checked_add(delta))
            .ok_or(Error::InvalidMatrixLayout)?;
        let reader = MatrixRegionReader::with_pool(backing.file.as_ref(), backing.pool.as_ref());
        let mut bytes = filled_bytes_for(len, 0, ReadLimitKey::MatrixBitmapBytes.resource())?;
        reader.read_exact_at(offset, &mut bytes)?;
        count_lazy_fault_bytes_read(len);
        // The page about to be believed is authenticated here, through the same
        // decision the streaming verification pass uses
        // ([`page_bytes_are_authentic`]). A verification pass is a verdict on the
        // whole map; this is the guarantee that no *read* ever answers from
        // unverified bytes, and it holds whether or not that pass ran.
        if let Some(base) = backing.digest_base {
            let digest_offset = page_digest_offset(base, page)?;
            let (stored, state) = read_page_digest_at(reader, digest_offset)?;
            count_lazy_fault_bytes_read(PAGE_DIGEST_LEN);
            if !page_bytes_are_authentic(&bytes, stored, state)? {
                return Err(Error::MatrixFatalCorruption);
            }
        }
        let ones = bytes
            .iter()
            .try_fold(0u64, |acc, byte| {
                acc.checked_add(u64::from(byte.count_ones()))
            })
            .ok_or(Error::InvalidMatrixLayout)?;
        if ones == 0 {
            return Ok(None);
        }
        Ok(Some((bytes, ones)))
    }

    /// Installs a page [`Self::load_page`] read, under the store's lock.
    ///
    /// Re-checks presence first: the lock was released for the read, so another
    /// thread may have installed the same page meanwhile. Its copy wins and
    /// this one is dropped — they are byte-identical, both having been checked
    /// against the same stored digest.
    fn install_faulted_page(
        &self,
        store: &mut PageStore,
        backing: &LazyBacking,
        page: u64,
        bytes: Vec<u8>,
        ones: u64,
    ) -> Result<()> {
        if store.pages.contains_key(&page) {
            store.note_used(page);
            return Ok(());
        }
        let len = usize_to_u64(bytes.len())?;
        while store
            .cached_bytes
            .checked_add(len)
            .is_none_or(|total| total > backing.cache_limit)
        {
            if !store.evict_one() {
                break;
            }
        }
        try_reserve_map(
            &mut store.pages,
            1,
            ReadLimitKey::MatrixBitmapBytes.resource(),
        )?;
        store.pages.insert(
            page,
            BitmapPage {
                bytes: Arc::new(bytes),
                ones,
                cached: true,
                lru_prev: NO_PAGE,
                lru_next: NO_PAGE,
            },
        );
        // After the insert, not before: the recency chain is threaded through
        // the pages themselves, so the page has to exist to be linked.
        store.lru_push_back(page);
        store.cached_bytes = store.cached_bytes.saturating_add(len);
        store.ones = store.ones.saturating_add(ones);
        record_lazy_cached_bytes(store.cached_bytes);
        Ok(())
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

    fn page_bytes(&self, page: u64) -> Result<PagePayload> {
        let len = usize::try_from(self.page_len(page)?).map_err(|_| Error::InvalidMatrixLayout)?;
        let store = self.faulted_store(page)?;
        match store.pages.get(&page) {
            Some(held) if held.bytes.len() == len => Ok(PagePayload::Resident(held.bytes.clone())),
            Some(_) => Err(Error::InvalidMatrixLayout),
            None => Ok(PagePayload::Zero(len)),
        }
    }

    /// Whether `page` currently holds state, faulting it in if it is published
    /// but not yet cached.
    fn page_is_materialised(&self, page: u64) -> Result<bool> {
        Ok(self.faulted_store(page)?.pages.contains_key(&page))
    }

    fn byte(&self, index: u64) -> Result<u8> {
        if index >= self.byte_len {
            return Err(Error::InvalidMatrixLayout);
        }
        let page = index / BITMAP_PAGE_BYTES;
        let within =
            usize::try_from(index % BITMAP_PAGE_BYTES).map_err(|_| Error::InvalidMatrixLayout)?;
        let store = self.faulted_store(page)?;
        match store.pages.get(&page) {
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
        if self.store().pages.contains_key(&page) {
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
            .store()
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
        // Faults the page in first where the map is lazily backed, so every
        // count below is derived from the published bytes rather than from an
        // absence the cache happens to be showing.
        let current = self.byte(index)?;
        let page_index = index / BITMAP_PAGE_BYTES;
        let within =
            usize::try_from(index % BITMAP_PAGE_BYTES).map_err(|_| Error::InvalidMatrixLayout)?;
        if current == value {
            let ones_after = self.store().ones;
            return Ok(PreparedByteWrite {
                page: page_index,
                within,
                value,
                fresh: None,
                changes: false,
                materialised: 0,
                page_ones_after: 0,
                ones_after,
            });
        }
        let mut materialised = 0;
        let mut fresh = None;
        let mut page_ones = 0;
        let page_len_for_fresh = self.page_len(page_index);
        let store = self.store_mut();
        match store.pages.get_mut(&page_index) {
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
                let page_len = page_len_for_fresh?;
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
                    &mut store.pages,
                    1,
                    ReadLimitKey::MatrixBitmapBytes.resource(),
                )?;
                fresh = Some(BitmapPage {
                    bytes: Arc::new(bytes),
                    ones: 0,
                    cached: false,
                    // Never linked: `cached: false` keeps it out of the
                    // recency chain and therefore out of `evict_one`'s reach.
                    lru_prev: NO_PAGE,
                    lru_next: NO_PAGE,
                });
                materialised = page_len;
            }
        }
        let page_ones_after = page_ones
            .checked_add(u64::from(value.count_ones()))
            .and_then(|ones| ones.checked_sub(u64::from(current.count_ones())))
            .ok_or(Error::InvalidMatrixLayout)?;
        let ones_after = store
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
        let store = self.store_mut();
        if let Some(page) = prepared.fresh {
            store.pages.insert(prepared.page, page);
            store.charged_bytes = store.charged_bytes.saturating_add(prepared.materialised);
        }
        let Some(page) = store.pages.get_mut(&prepared.page) else {
            // Unreachable: preparation either found the page or built one.
            return PageResidencyDelta::NONE;
        };
        if let Some(slot) = Arc::make_mut(&mut page.bytes).get_mut(prepared.within) {
            *slot = prepared.value;
        }
        page.ones = prepared.page_ones_after;
        store.ones = prepared.ones_after;
        if prepared.page_ones_after == 0 {
            // The page is now byte-for-byte the zero page that a non-resident
            // page already answers with, so holding it would be pure overhead.
            //
            // Unlink first, and unconditionally. The recency chain is threaded
            // through the pages, so dropping a page without taking it off the
            // chain would leave its neighbours pointing at nothing — which is
            // also the reason the old side-container form leaked: it released
            // the page and kept the entry, and a later re-fault of the same
            // page pushed a *second* one, so the deque grew with clear-to-zero
            // events without bound and against no limit.
            store.lru_unlink(prepared.page);
            let dropped = store.pages.remove(&prepared.page);
            let released = match dropped {
                // A demand-cached page was never charged to the layout's
                // resident total, so it must be refunded to the cache instead
                // of reported as a budget release.
                Some(page) if page.cached => {
                    let len = usize_to_u64(page.bytes.len()).unwrap_or(0);
                    store.cached_bytes = store.cached_bytes.saturating_sub(len);
                    record_lazy_cached_bytes(store.cached_bytes);
                    0
                }
                Some(page) => {
                    let len = usize_to_u64(page.bytes.len()).unwrap_or(0);
                    store.charged_bytes = store.charged_bytes.saturating_sub(len);
                    len
                }
                None => 0,
            };
            return PageResidencyDelta {
                materialised: prepared.materialised,
                released,
                // The page reached zero. That is true of the cached arm above,
                // where `released` is 0 because the bytes belonged to the lazy
                // cache and not to the layout's budget.
                emptied: true,
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

    /// Drops every page *and* the persisted-index tracking, which is what a
    /// whole-map clear or rebuild does on disk as well.
    fn clear(&mut self) {
        let store = self.store_mut();
        store.pages.clear();
        store.lru_head = NO_PAGE;
        store.lru_tail = NO_PAGE;
        store.ones = 0;
        store.charged_bytes = 0;
        store.cached_bytes = 0;
        record_lazy_cached_bytes(0);
        self.indexed_pages.clear();
        self.index_slots.clear();
    }

    /// Set bits currently *resident*.
    ///
    /// Under a lazily backed map this counts only the cached pages, so it is
    /// not the map's total. Callers that need the total must use
    /// [`Self::ones_total`], which reads the pages the cache does not hold.
    fn ones(&self) -> u64 {
        self.store().ones
    }

    /// Set bits held by the whole map, whatever the cache currently holds.
    ///
    /// A session-only map (no backing) answers from the maintained counter. A
    /// backed map reads each published page it is not holding and counts it
    /// *without* caching it, so an aggregate query costs `O(live pages)` of I/O,
    /// retains one page buffer, and leaves residency where it found it rather
    /// than pulling the whole live set into memory.
    ///
    /// Every page it reads is authenticated, through the same
    /// [`page_bytes_are_authentic`] decision the fault-in and the verification
    /// pass use. Without that this was the one place a *count* could be derived
    /// from bytes nothing had checked — an aggregate is an answer like any other,
    /// and a damaged page must refuse rather than contribute a number.
    ///
    /// # The lock is taken per page, not per aggregate
    ///
    /// This used to hold one [`PageStoreGuard`] for the whole loop, which meant
    /// `O(live pages)` of `pread` under a lock every concurrent reader of the
    /// same category needs for its own `O(1)` lookups — a convoy of exactly the
    /// kind [`PageStore`] documents it does not create, reachable from a `&self`
    /// entry point (`resume_signal`). It now takes the lock once per page and
    /// releases it before that page's read.
    ///
    /// Re-acquiring per page observes the same map a single acquisition would
    /// have: every mutator takes `&mut self`, so no mutation can be in flight
    /// while this `&self` borrow exists. A concurrent *reader* may fault a page
    /// in mid-loop, but each page is still counted exactly once, and from
    /// memory or from disk it is the same authenticated value — demand-cached
    /// pages are write-through.
    fn ones_total(&self) -> Result<u64> {
        let Some(backing) = self.backing.as_ref() else {
            return Ok(self.ones());
        };
        // Through the backing's own pool, exactly as the sibling fault-in at
        // `load_page` does. This aggregate is reachable from the `&self` entry
        // point `resume_signal`, so two readers really can be in this loop at
        // once; the pool is what keeps them off one shared file object.
        let reader = MatrixRegionReader::with_pool(backing.file.as_ref(), backing.pool.as_ref());
        let mut total = 0u64;
        let mut buffer: Option<PageVerifyBuffer> = None;
        for page in self.indexed_pages.keys().copied() {
            // Bound to a `let` on purpose: the guard is a temporary of *this*
            // statement and is released at the semicolon, so it cannot still be
            // held at the read below whatever the edition's temporary-scope
            // rules are.
            let resident_ones = self.store().pages.get(&page).map(|held| held.ones);
            if let Some(ones) = resident_ones {
                total = total.saturating_add(ones);
                continue;
            }
            let len = self.page_len(page)?;
            let offset = page
                .checked_mul(BITMAP_PAGE_BYTES)
                .and_then(|delta| backing.base_offset.checked_add(delta))
                .ok_or(Error::InvalidMatrixLayout)?;
            let buffer = match buffer.as_mut() {
                Some(buffer) => buffer,
                None => buffer.insert(PageVerifyBuffer::new()?),
            };
            let bytes = buffer.read(reader, offset, len)?;
            count_lazy_fault_bytes_read(len);
            if let Some(base) = backing.digest_base {
                let (stored, state) = read_page_digest_at(reader, page_digest_offset(base, page)?)?;
                count_lazy_fault_bytes_read(PAGE_DIGEST_LEN);
                if !page_bytes_are_authentic(bytes, stored, state)? {
                    return Err(Error::MatrixFatalCorruption);
                }
            }
            for byte in bytes {
                total = total.saturating_add(u64::from(byte.count_ones()));
            }
        }
        Ok(total)
    }

    /// Payload bytes this map holds that were admitted through the resident
    /// bitmap budget.
    ///
    /// Deliberately excludes demand-cached pages: `resident_bitmap_bytes` is a
    /// running total of what the budget granted, and a caller subtracting this
    /// from it must never subtract memory the budget never gave.
    fn resident_bytes(&self) -> u64 {
        self.store().charged_bytes
    }

    /// Payload bytes this map's demand cache currently holds.
    ///
    /// Read only by the residency counters, which are compiled out of builds
    /// without the test-only fault-injection feature.
    #[cfg_attr(not(any(test, feature = "scalable-fault-injection")), allow(dead_code))]
    fn cached_bytes(&self) -> u64 {
        self.store().cached_bytes
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
/// the bitmap to the shared page-index maintenance helpers. **Round 16 narrowed
/// it from a source gate to a compiler refusal.** It used to return
/// `&mut SparseBitmap` and be held to one call site by a count in
/// `enforcement_gates.rs`; re-verification showed that a count over call sites
/// says nothing about what one call site may do, and executed
/// `*evidence.page_index_mirror_mut() = attacker_bits;` — a whole-map
/// replacement that leaves `complete` true — reading a laundered bit back
/// through `CompleteCrcValidEvidence::get`. The escape now hands back a
/// [`page_index::PageIndexMirror`], whose borrow is private to `mod
/// page_index`, so outside that module the only things reachable through it are
/// the two questions page-index maintenance asks. The call-site count is kept
/// as a second line of defence.
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
/// constructor, [`allow_for`], which *is* the refusal.
///
/// **Round 15 closed the minting hole.** Round 14 made the witness unforgeable
/// and stopped there, which binds an outside crate and binds nothing inside
/// `varve-core` — where every historical defect in this class lived. Two
/// `pub(crate)` affordances survived and composed into a one-liner:
///
/// ```text
/// FatalAccessGate::new(false).allow()?   // compiled, inside matrix.rs
/// ```
///
/// A caller that did not like the answer its own layout gave could mint a gate
/// that says `false` and spend the resulting witness on `block_index` for a
/// layout carrying a `Fatal` finding. The witness proved that *a* gate had been
/// consulted, not that *this layout's* had — precisely the laundering shape
/// round 14 closed for `MutationPermit` one file over, left open here.
///
/// Both halves are now gone:
///
/// * there is no constructor that takes the decision. [`FatalAccessGate::new`]
///   is deleted; the only constructor is [`FatalAccessGate::evaluate`], which
///   *performs* the classification from the recovery findings and the spec, so
///   no `bool` crosses the module boundary in the direction that matters.
/// * there is no method that turns a gate into a witness. `allow` is a free
///   function [`allow_for`] taking `&MatrixLayout`, so the gate consulted is
///   the gate of the layout the caller is about to address. A stack-allocated
///   decoy has nowhere to be spent.
///
/// **Round 16 closed the assignment half.** Round 15's residual was reported as
/// "a whole `MatrixLayout` literal, twelve fields including three `Vec`s".
/// Re-verification showed the real cost was one line: `MatrixLayout`'s fields
/// are private to the *file*, so with any `&mut MatrixLayout` in scope —
/// `matrix.rs` has many — `layout.fatal_access = FatalAccessGate::evaluate(
/// clean.iter(), spec);` installed an unblocked gate and **undid
/// [`FatalAccessGate::block`]**, the irreversible poison an interrupted rebuild
/// leaves behind. `layout.fatal_access = donor.fatal_access.clone();` did the
/// same off a healthy layout, and appeared in no report at all.
///
/// The property that closes both is structural rather than a call-site count:
/// **no expression of type `FatalAccessGate` exists outside this module.**
/// Three things enforce it, and each was compiled and observed to refuse:
///
/// * `evaluate` is private to this module (E0624 from file scope), and its only
///   caller is [`assemble_layout`], which returns the whole layout it derived
///   the gate for.
/// * `FatalAccessGate` is not `Clone` (E0599 on `.clone()`), so no gate can be
///   copied out of a layout; `MatrixLayout::clone` is hand-written through
///   [`clone_layout`], which copies a layout, never a decision.
/// * `MatrixLayout` implements `Drop`, so a gate cannot be *moved* out of any
///   expression that yields a layout either (E0509 on
///   `donor.clone().fatal_access` and on `assemble_layout(..).fatal_access`).
///
/// The residual that remains, stated rather than implied: a caller inside
/// `matrix.rs` can still call `assemble_layout` and get back a layout whose
/// gate is unblocked. It gains nothing by it — it must supply the blocks and
/// commits it wants to address, i.e. the state itself — but it is why the
/// source gate on the single construction site is kept. What no longer exists
/// is a route from a gate of the caller's choosing to a layout it did not
/// build. Full field-level unnameability still wants `MatrixLayout` behind its
/// own module boundary: checklist open item 25, **not done here**.
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
    use super::{
        Error, FormatSpec, MatrixAuxLayout, MatrixBlockLayout, MatrixCommitLayout,
        MatrixCorruptionSeverity, MatrixDimensionValue, MatrixLayout, MatrixRecoveryFinding,
        ReadLimits, Result,
    };

    /// Witness that no `Fatal` recovery finding blocks this access.
    ///
    /// Zero-sized, private field, single constructor. Held by reference by the
    /// resolvers, because one check legitimately covers the whole operation
    /// that follows it.
    #[derive(Debug)]
    pub struct FatalAccessAllowed(());

    /// Whether safe access to this layout's state is blocked by a `Fatal`
    /// recovery finding, and the sole source of [`FatalAccessAllowed`].
    ///
    /// **Deliberately not `Clone` (round 16).** Re-verification found that
    /// `layout.fatal_access = donor.fatal_access.clone();` — one line, with any
    /// `&mut MatrixLayout` in scope — copied an unblocked gate from a healthy
    /// layout onto a blocked one, undoing `FatalAccessGate::block`. A gate
    /// that can be copied out of one layout is a gate that can be installed
    /// into another, so the derive is the bypass. `MatrixLayout` is therefore
    /// not `Clone` either; nothing in the crate cloned it.
    #[derive(Debug)]
    pub struct FatalAccessGate {
        blocked: bool,
    }

    impl FatalAccessGate {
        /// The only constructor, and it **is** the classification.
        ///
        /// Round 14 had `new(blocked: bool)`, which let any line in `matrix.rs`
        /// choose the answer. The decision is now made here, from the recovery
        /// findings the open path actually collected and the spec's declared
        /// forensic opt-in, so there is no signature anywhere that accepts
        /// "this gate permits" as an argument.
        ///
        /// `findings` must be every finding the layout will carry — the CRC
        /// findings and the per-commit quarantine findings.
        ///
        /// **Private to this module (round 16).** `pub(super)` made it
        /// file-visible, and a file-visible function returning a
        /// `FatalAccessGate` is a file-visible way to *write* one:
        /// `layout.fatal_access = FatalAccessGate::evaluate(clean.iter(), spec);`
        /// is a single field assignment — `MatrixLayout`'s fields are
        /// file-private — and it undoes [`FatalAccessGate::block`]. The rule
        /// that closes it is structural rather than a call-site count: **no
        /// expression of type `FatalAccessGate` exists outside this module**.
        /// The only caller is [`assemble_layout`], which builds the whole
        /// layout around the gate it derives, so a gate can be created only
        /// together with the state it speaks for.
        fn evaluate<'a>(
            findings: impl IntoIterator<Item = &'a MatrixRecoveryFinding>,
            spec: FormatSpec,
        ) -> Self {
            let has_fatal_finding = findings
                .into_iter()
                .any(|finding| finding.severity == MatrixCorruptionSeverity::Fatal);
            Self {
                blocked: has_fatal_finding && !spec.matrix_fatal_forensics,
            }
        }

        /// Blocks safe access from here on. Not reversible: a rebuild that was
        /// interrupted after marking the persisted page index leaves state no
        /// safe accessor may consume, and reopening with forensic access is the
        /// deliberate, explicitly named route to reading it anyway.
        pub(crate) fn block(&mut self) {
            self.blocked = true;
        }

        /// Reads the decision without addressing a layout. Test-only, named to
        /// say so, and compiled out of every shipped build: the production
        /// route to the answer is [`allow_for`], which needs the layout.
        #[cfg(test)]
        pub(super) fn is_blocked_for_tests(&self) -> bool {
            self.blocked
        }
    }

    /// The fail-closed refusal, and the only way to obtain the witness.
    ///
    /// Takes the layout rather than `&self` on the gate on purpose: the witness
    /// then speaks for the state the caller is about to address, and a gate
    /// built anywhere else has nothing to say. This mirrors
    /// `GuardedWriter::writer_permit` in `writer_permit.rs`, which was given
    /// the same shape in round 14 for the same reason.
    pub(super) fn allow_for(layout: &MatrixLayout) -> Result<FatalAccessAllowed> {
        if layout.fatal_access.blocked {
            return Err(Error::MatrixFatalCorruption);
        }
        Ok(FatalAccessAllowed(()))
    }

    /// The one place a [`MatrixLayout`] — and therefore the one place a
    /// [`FatalAccessGate`] — comes into existence.
    ///
    /// Round 15 left the classification inside this module but the *literal*
    /// outside it, which meant a gate value crossed the module boundary and
    /// could be assigned to any layout's field. The literal moves in here
    /// instead: `evaluate` is private, this function returns a whole layout
    /// rather than a gate, and the gate it installs is derived from the
    /// findings that same layout is being built with. There is no signature in
    /// the crate that yields a `FatalAccessGate` on its own, so
    /// `layout.fatal_access = ...` has nothing to assign.
    ///
    /// The findings consulted are every finding the layout will carry: the CRC
    /// findings vector it is given, and the quarantine finding of each commit
    /// category, which by this point holds every entry the caller's
    /// `commit_findings` map had (`layout_from_parts` refuses an undrained
    /// map).
    ///
    /// The residual, stated rather than implied: a caller inside `matrix.rs`
    /// can still *call this function*, and gets back a layout whose gate is
    /// unblocked when it passes no findings. That buys nothing it did not
    /// already have — it must supply the twelve fields of a whole layout,
    /// including the blocks and commits whose state it wants to address, which
    /// is to say it must supply the state itself. What it cannot do is take the
    /// gate off a layout it built and put it on one it did not.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn assemble_layout(
        dimensions: Vec<MatrixDimensionValue>,
        commits: Vec<MatrixCommitLayout>,
        blocks: Vec<MatrixBlockLayout>,
        aux: Vec<MatrixAuxLayout>,
        crc_findings: Vec<MatrixRecoveryFinding>,
        append_log_start: u64,
        read_limits: ReadLimits,
        resident_bitmap_bytes: u64,
        resident_page_index_bytes: u64,
        spec: FormatSpec,
    ) -> MatrixLayout {
        let fatal_access = FatalAccessGate::evaluate(
            crc_findings.iter().chain(
                commits
                    .iter()
                    .filter_map(|commit| commit.quarantine_finding.as_ref()),
            ),
            spec,
        );
        let layout = MatrixLayout {
            dimensions,
            commits,
            blocks,
            aux,
            crc_findings,
            append_log_start,
            read_limits,
            resident_bitmap_bytes,
            resident_page_index_bytes,
            fatal_access,
        };
        super::record_open_resident_bitmap_bytes(&layout);
        layout
    }

    /// Copies a whole layout, decision included — the mmap snapshot path
    /// ([`RecordFile::mmap_matrix`](crate::file::RecordFile)) needs a layout it
    /// owns.
    ///
    /// This is what `#[derive(Clone)]` used to do, minus the part that made it
    /// a bypass. The derive required `FatalAccessGate: Clone`, and a clonable
    /// gate can be lifted off a healthy layout and dropped onto a blocked one.
    /// Copying a *layout* cannot launder anything: the result carries the
    /// blocks, commits and findings it was copied from, so its gate still
    /// speaks for the state beside it. The only thing a caller can do with this
    /// is address the copy, which is addressing the original.
    ///
    /// Partial moves are what would break that, and they are refused by the
    /// `Drop` impl on `MatrixLayout`: `donor.clone().fatal_access` — moving the
    /// gate out of the temporary — is E0509, not a one-liner.
    pub(super) fn clone_layout(layout: &MatrixLayout) -> MatrixLayout {
        MatrixLayout {
            dimensions: layout.dimensions.clone(),
            commits: layout.commits.clone(),
            blocks: layout.blocks.clone(),
            aux: layout.aux.clone(),
            crc_findings: layout.crc_findings.clone(),
            append_log_start: layout.append_log_start,
            read_limits: layout.read_limits,
            resident_bitmap_bytes: layout.resident_bitmap_bytes,
            resident_page_index_bytes: layout.resident_page_index_bytes,
            fatal_access: FatalAccessGate {
                blocked: layout.fatal_access.blocked,
            },
        }
    }

    #[cfg(test)]
    mod tests {
        use super::{FatalAccessGate, MatrixRecoveryFinding};
        use crate::matrix::{MatrixCorruptionKind, MatrixCorruptionSeverity, bypass_catalogue};

        /// The gate the layout carries is the one `evaluate` derived from that
        /// layout's findings — not a value any caller chose.
        ///
        /// Lives inside the module because `evaluate` is private to it, which
        /// is the property under test: a test that could call it from file
        /// scope would be a test that the bypass is still open.
        #[test]
        fn the_gate_is_derived_from_the_findings_not_supplied() {
            let spec = bypass_catalogue::probe_spec();
            let clean: [MatrixRecoveryFinding; 0] = [];
            assert!(!FatalAccessGate::evaluate(clean.iter(), spec).is_blocked_for_tests());
            let fatal = [MatrixRecoveryFinding {
                kind: MatrixCorruptionKind::CommitMap,
                severity: MatrixCorruptionSeverity::Fatal,
                message: String::new(),
            }];
            assert!(FatalAccessGate::evaluate(fatal.iter(), spec).is_blocked_for_tests());
            assert!(
                !FatalAccessGate::evaluate(fatal.iter(), spec.with_matrix_fatal_forensics())
                    .is_blocked_for_tests(),
                "forensic access is the declared opt-in and must still relax the gate"
            );
        }
    }
}

pub(crate) mod crc_valid_evidence {
    use super::{
        BitmapByteUpdate, Error, PageResidencyDelta, PreparedByteWrite, Result, SparseBitmap,
        page_index::PageIndexMirror, page_index_enumeration::PageIndexEnumeration,
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
        /// Binds a loaded validity bitmap to the enumeration that produced it.
        ///
        /// Round 14 spelled this `new(bits, complete: bool)` with `pub(super)`
        /// visibility, so any line in `matrix.rs` could declare a bitmap
        /// complete and immediately mint the F-04 witness from it. The
        /// completeness argument is now a [`PageIndexEnumeration`], whose
        /// constructors are private to `mod page_index_enumeration` and are
        /// reached only by the enumerator itself. There is no signature in the
        /// crate that accepts completeness as a `bool`.
        pub(super) fn from_page_index_enumeration(
            bits: SparseBitmap,
            enumeration: PageIndexEnumeration,
        ) -> Self {
            Self {
                complete: enumeration.is_complete(),
                bits,
            }
        }

        /// Evidence for a bitmap that has published nothing yet.
        ///
        /// Used at creation, and — with `bit_count` zero — for a format without
        /// integrity, where no validity bitmap exists and no recovery path reads
        /// one. Completeness is vacuous rather than asserted: this constructor
        /// takes a *count*, not a bitmap, so it builds the empty map itself and
        /// cannot be used to launder a bitmap that came from somewhere else.
        pub(super) fn newly_created(bit_count: u64) -> Result<Self> {
            Ok(Self {
                bits: SparseBitmap::new(bit_count)?,
                complete: true,
            })
        }

        /// Test-only escape, named at the call site so it cannot be mistaken
        /// for the production route, and compiled out of every shipped build.
        /// It exists because the type's own contract
        /// (`crc_valid_evidence_tests`) has to be exercisable without a file.
        #[cfg(test)]
        pub(super) fn from_parts_for_tests_only(bits: SparseBitmap, complete: bool) -> Self {
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

        /// The one named escape, narrowed to a compiler refusal (round 16).
        ///
        /// It used to return `&mut SparseBitmap`, held to one call site by a
        /// source gate. That is not the same thing as holding it to one
        /// *operation*: re-verification wrote
        /// `*evidence.page_index_mirror_mut() = attacker_bits;` — a whole-map
        /// replacement that leaves `complete` true, so
        /// `CompleteCrcValidEvidence::get` then reports bits no enumeration
        /// produced — and `.page_index_mirror_mut().set(ordinal, true)` in
        /// place, both from inside this file, and ran the first one to a bit
        /// read back through the F-04 witness.
        ///
        /// The return type is now [`PageIndexMirror`](super::page_index::PageIndexMirror),
        /// whose borrow is private to `mod page_index`. Assignment through it
        /// is a type error and `SparseBitmap`'s mutators are not reachable on
        /// it; the only things it can do are the two index-maintenance
        /// operations that made the escape necessary. The call-site count is
        /// kept as a second line of defence, not as the property.
        pub(super) fn page_index_mirror_mut(&mut self) -> PageIndexMirror<'_> {
            PageIndexMirror::new(&mut self.bits)
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

/// Reads one page-digest slot: `(checksum, state)`.
///
/// The only reader of that slot. It was one of two — the other seeked a
/// `&mut File` cursor and served the eager load — and the pair went with the
/// eager load in 0.5.0: both the streaming verification pass and the demand
/// fault-in hold a shared borrow and address the slot positionally, so there is
/// one reader and no cursor to move.
fn read_page_digest_at(reader: MatrixRegionReader<'_>, offset: u64) -> Result<(u32, u32)> {
    let mut bytes = [0; PAGE_DIGEST_LEN as usize];
    reader.read_exact_at(offset, &mut bytes)?;
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

/// **Deliberately not `Clone` (round 16).** `#[derive(Clone)]` on the layout
/// forced `Clone` on [`FatalAccessGate`], and a clonable gate is an installable
/// gate: `layout.fatal_access = donor.fatal_access.clone();` copied an
/// unblocked decision onto a blocked layout in one line. Nothing in the crate
/// cloned a layout, so the derive bought nothing and cost the property.
#[derive(Debug)]
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

impl Clone for MatrixLayout {
    /// Hand-written because the gate must not be `Clone`; see
    /// [`fatal_access::clone_layout`].
    fn clone(&self) -> Self {
        fatal_access::clone_layout(self)
    }
}

/// Empty on purpose: this impl exists for what it *forbids*.
///
/// A type that implements `Drop` cannot have a field moved out of it (E0509),
/// and that is the last route from "some expression yields a `MatrixLayout`" to
/// "this line holds a `FatalAccessGate`". Without it,
/// `layout.fatal_access = donor.clone().fatal_access;` and
/// `... = fatal_access::assemble_layout(..).fatal_access;` are one-line
/// laundering of the fail-closed decision from any of `matrix.rs`'s ~8000
/// lines, because the field is private to the *file* and every function that
/// returns a layout is callable from all of it. Making the field unnameable
/// instead needs `MatrixLayout` behind its own module boundary (checklist open
/// item 25, ~70 call sites); this achieves the same refusal for the field that
/// carries a security decision, and costs nothing at run time — `MatrixLayout`
/// owns `Vec`s and a `HashMap`, so it already had drop glue.
impl Drop for MatrixLayout {
    fn drop(&mut self) {}
}

#[derive(Clone, Debug)]
struct MatrixCommitLayout {
    name: String,
    kind: MatrixCommitKind,
    bit_count: u64,
    map_offset: u64,
    bits: SparseBitmap,
    /// The category's quarantine, and the *only* representation of it.
    ///
    /// Until 0.5.0 quarantine was represented twice: this finding, plus a
    /// `quarantined_raw_bits: Option<SparseBitmap>` that held the damaged map the
    /// eager load had already retained, with an empty replacement installed in
    /// `bits`. Every gate then asked the *copy* whether the category was
    /// quarantined. With verification streaming and retaining nothing there is no
    /// copy to ask, so the finding is the flag: `Some` means every access to this
    /// category fails closed with [`Error::MatrixCommitQuarantined`], and the two
    /// recovery paths that clear it — `clear_matrix_category` and
    /// `rebuild_commit_map_from_crc` — clear this one field.
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
        fatal_access::allow_for(self)
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
    // Residency first, and unconditionally: every bitmap this open produces is
    // demand-filled and bounded by the declared cache, so open reads the
    // persisted page index and retains no payload page.
    let residency = LazyResidency::declared(spec.read_limits, file)?;
    // Then verification, which is a separate decision and the only reader of the
    // allocation map. PERF-01/PERF-02: the persisted page index names the pages
    // this matrix has ever published, and one allocation-map query names the
    // pages the filesystem says hold bytes; the pass visits their union, so it
    // never loops over logical pages and never degrades to full logical bitmap
    // I/O when the platform cannot answer. Where verification is
    // `OnDemand` the map is not queried at all — the query costs a syscall and
    // drags a ~128 KiB NTFS run into an open with one live page, and nothing
    // would read the answer.
    let verify = spec.read_limits.admit_matrix_metadata_verification();
    let extents = match verify {
        true => AllocatedExtents::query(file),
        false => None,
    };
    record_open_allocation_map(extents.as_ref());
    let sources = PageSources {
        extents: extents.as_ref(),
        residency: &residency,
        verify,
    };
    let mut budget = ResidentBitmapBudget::new(spec.read_limits);
    let commit_bits = load_commit_bitmaps(
        file,
        crc_layout.as_ref(),
        &page_index_layout,
        &commit_plans,
        sources,
        &mut budget,
        &mut crc_verification,
    )?;
    let crc_valid_bits = load_crc_valid_bits(
        spec,
        file,
        crc_layout.as_ref(),
        &page_index_layout,
        &block_offsets,
        sources,
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
    let category = ensure_matrix_block::<T>(spec)?;
    let allowed = ensure_commit_publishable(layout, category)?;
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
    let (commit_index, commit_update) = prepare_cell_commit(layout, category, ordinal, false)?;
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
    let category = ensure_matrix_block::<T>(spec)?;
    let allowed = ensure_commit_publishable(layout, category)?;
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
    let (commit_index, commit_update) = prepare_cell_commit(layout, category, ordinal, false)?;
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
    reader: MatrixRegionReader<'_>,
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
    let mut payload = filled_bytes(stride, 0)?;
    reader.read_exact_at(offset, &mut payload)?;
    verify_cell_crc(layout, reader, block_index, ordinal, &payload)?;
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
    let category = ensure_matrix_block::<T>(spec)?;
    let allowed = ensure_commit_publishable(layout, category)?;
    let block_index = layout.block_index(&allowed, T::ID)?;
    let ordinal = layout.ordinal_for_block(block_index, key)?;
    let commit_index = layout.commit_index(&allowed, category)?;
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
    let category = ensure_matrix_block::<T>(spec)?;
    let allowed = ensure_commit_publishable(layout, category)?;
    let block_index = layout.block_index(&allowed, T::ID)?;
    let ordinal = layout.ordinal_for_block(block_index, key)?;
    let written_this_session = layout.blocks[block_index].current_write_bits.get(ordinal)?;
    // One pass, not two. The zero probe and the checksum pass read the same
    // slot, and this used to seek and stream it once for each. `scan_slot`
    // answers both, so a cell this session did not write costs its stride once
    // instead of twice — and the refusal below still happens before any commit
    // state is prepared, so a slot about to be refused never has a CRC written
    // for it.
    let scanned = if written_this_session {
        None
    } else {
        let scanned = scan_slot(layout, file, block_index, ordinal)?;
        if scanned.all_zero {
            return Err(Error::MatrixCellNotWritten);
        }
        Some(scanned)
    };
    let crc_valid_update = prepare_cell_crc_valid(layout, block_index, ordinal, true)?;
    let (commit_index, commit_update) = prepare_cell_commit(layout, category, ordinal, true)?;
    match scanned {
        Some(scanned) => write_cell_crc(layout, file, block_index, ordinal, scanned.crc)?,
        None => update_cell_crc(layout, file, block_index, ordinal)?,
    }
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
    let category = ensure_matrix_block::<T>(spec)?;
    let allowed = ensure_commit_publishable(layout, category)?;
    let block_index = layout.block_index(&allowed, T::ID)?;
    let ordinal = layout.ordinal_for_block(block_index, key)?;
    set_cell_commit(layout, file, category, ordinal, false)?;
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
    // A whole-category clear is the *recovery* for a quarantined category, so it
    // deliberately does not go through the quarantine refusal. What it cannot do
    // is count the bits it is discarding: they are the damaged ones, and
    // `count_committed` authenticates what it reads. It reported zero before
    // 0.5.0 as well — quarantine installed an empty replacement map and the count
    // came from that — so this states what was previously emergent instead of
    // failing the one recovery path the report recommends.
    let cleared = match layout.commits[commit_index].quarantine_finding.is_some() {
        true => 0,
        false => count_committed(&layout.commits[commit_index])?,
    };
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
    // bitmap's payload pages, the validity bitmap this clear drops with them, and
    // the page-index tracking of both. F-01: both budget terms are totalled
    // *before* anything is cleared, because `SparseBitmap::clear` drops the pages
    // and the index mirror the totals are derived from, and refunding only the
    // payload term left the page-index charge — and the validity bitmap's payload
    // charge — standing for memory that had already been released. Repeated
    // populate/clear cycles then accumulated phantom residency until the budget
    // refused a matrix that held nothing.
    //
    // A quarantined category used to add a third term here, the retained copy of
    // the damaged map. Verification retains nothing, so there is no copy and no
    // term (0.5.0).
    let mut released_pages = layout.commits[commit_index].bits.resident_bytes();
    let mut released_index = layout.commits[commit_index].bits.resident_index_bytes();
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
        // Lifting the quarantine is one assignment, because the quarantine is one
        // field.
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
    let category = ensure_matrix_block::<T>(spec)?;
    let allowed = layout.ensure_fatal_access_allowed()?;
    let block_index = layout.block_index(&allowed, T::ID)?;
    let ordinal = layout.ordinal_for_block(block_index, key)?;
    let offset = layout.slot_offset(block_index, ordinal)?;
    Ok(MatrixCommitEvent {
        block_id: T::ID,
        category,
        key,
        slot_offset: offset,
        slot_len: layout.blocks[block_index].slot_stride,
    })
}

pub(crate) fn read_cell_payload<T: VarveMatrixBlock>(
    spec: FormatSpec,
    layout: &MatrixLayout,
    reader: MatrixRegionReader<'_>,
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
    let mut payload = filled_bytes(stride, 0)?;
    reader.read_exact_at(offset, &mut payload)?;
    verify_cell_crc(layout, reader, block_index, ordinal, &payload)?;
    Ok(payload)
}

pub(crate) fn aux_len(layout: &MatrixLayout, name: &str) -> Result<u64> {
    layout.ensure_fatal_access_allowed()?;
    Ok(layout.aux(name)?.byte_len)
}

pub(crate) fn read_aux_at_len(
    layout: &MatrixLayout,
    reader: MatrixRegionReader<'_>,
    logical_file_len: u64,
    name: &str,
    offset: u64,
    len: u64,
) -> Result<Vec<u8>> {
    layout.ensure_fatal_access_allowed()?;
    layout
        .read_limits
        .check(ReadLimitKey::RecordPayloadLen, len)?;
    layout
        .read_limits
        .check(ReadLimitKey::MaterializedBytes, len)?;
    let absolute = aux_absolute_offset(layout.aux(name)?, offset, len)?;
    validate_range(absolute, len, logical_file_len)?;
    let mut payload = filled_bytes(len, 0)?;
    reader.read_exact_at(absolute, &mut payload)?;
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
    // A progress figure is an answer like any other, so a quarantined category
    // refuses it. Before 0.5.0 quarantine answered here from the empty
    // replacement map it installed, i.e. it reported `Clean` — "nothing in
    // progress" — for a category whose commit map is known to be damaged. There
    // is no map left to answer from and no honest answer to give.
    let allowed = ensure_commit_publishable(layout, category)?;
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
    // Quarantined refuses, for the reason given on `resume_signal`.
    let allowed = ensure_commit_publishable(layout, category)?;
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
pub(crate) struct MatrixSidecarReadPlan {
    pub(crate) format_magic_offset: u64,
    pub(crate) category_offset: u64,
    pub(crate) payload_offset: u64,
    pub(crate) payload_len: u64,
    pub(crate) total_len: u64,
}

pub(crate) fn check_matrix_sidecar_file_len(spec: FormatSpec, file_len: u64) -> Result<()> {
    spec.read_limits.check(ReadLimitKey::SidecarLen, file_len)
}

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
    report_from(layout, None)
}

/// The finding a failed verification raises for one commit category.
///
/// One spelling, because the open-time pass and the on-demand pass must report
/// the same damage with the same words; a caller matching on the message of one
/// would otherwise miss the other.
fn commit_map_finding(name: &str) -> MatrixRecoveryFinding {
    MatrixRecoveryFinding {
        kind: MatrixCorruptionKind::CommitMap,
        severity: MatrixCorruptionSeverity::Recoverable,
        message: format!("matrix commit map page crc mismatch for {name}"),
    }
}

/// Runs the streaming verification pass now, and reports what it found.
///
/// The on-demand half of [`crate::MatrixMetadataVerification`]: the same pass an
/// [`crate::MatrixMetadataVerification::AtOpen`] open runs, over the same
/// candidate set — the persisted page index each map already holds, unioned with
/// a fresh allocation-map query — through the same
/// [`verify_paged_bitmap`], authenticating with the same
/// [`page_bytes_are_authentic`]. Peak retention is one 4096-byte buffer, so the
/// call is `O(1)` in memory for any matrix.
///
/// # It reports; it does not quarantine
///
/// A finding it produces does **not** arm [`Error::MatrixCommitQuarantined`] and
/// does not gate a writer, because the fail-closed gate is derived once, from the
/// findings the layout is assembled with, and nothing outside
/// `mod fatal_access` may install one afterwards — that is the round-16 rule this
/// call is not permitted to launder. A caller that wants the gate reopens with
/// `AtOpen`; a caller that wants to know reads this report. `&MatrixLayout` in the
/// signature is the mechanical half of that promise: this function cannot mutate
/// the layout's state at all.
pub(crate) fn verify_matrix_metadata(
    layout: &MatrixLayout,
    file: &mut File,
) -> Result<MatrixRecoveryReport> {
    let extents = AllocatedExtents::query(file);
    let mut buffer = PageVerifyBuffer::new()?;
    let mut fresh = HashMap::new();
    try_reserve_map(
        &mut fresh,
        layout.commits.len(),
        ReadLimitKey::MatrixBitmapBytes.resource(),
    )?;
    for commit in &layout.commits {
        let Some(digest_base) = commit.digest_offset else {
            // No digest array: the region is unauthenticated by design (integrity
            // disabled), so there is nothing to verify and nothing to report.
            continue;
        };
        let intact = verify_paged_bitmap(
            MatrixRegionReader::new(&*file),
            extents.as_ref(),
            &commit.bits,
            commit.map_offset,
            digest_base,
            &mut buffer,
        )?;
        if !intact {
            fresh.insert(commit.name.clone(), commit_map_finding(&commit.name));
        }
    }
    Ok(report_from(layout, Some(&fresh)))
}

/// Assembles the report from this layout's findings, optionally adding a freshly
/// computed set of per-category commit-map findings.
///
/// One assembly implementation, two callers: [`recovery_report`] passes `None`
/// and reports the quarantine state the open established, while
/// [`verify_matrix_metadata`] passes the findings its streaming pass just
/// produced. A second copy of this walk would be a second answer to "what does
/// this matrix recommend", which is precisely the class of defect the
/// residency/verification split was made to remove.
///
/// A fresh finding *adds to* the stored one rather than replacing it: a category
/// quarantined at open stays in the report even if a later pass over the same
/// bytes were to disagree, because the fail-closed gate that quarantine armed is
/// still refusing every access to it. A report that contradicted the gate would
/// be worse than a stale one.
fn report_from(
    layout: &MatrixLayout,
    fresh: Option<&HashMap<String, MatrixRecoveryFinding>>,
) -> MatrixRecoveryReport {
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
        let quarantine = match fresh {
            Some(fresh) => fresh
                .get(&commit.name)
                .or(commit.quarantine_finding.as_ref()),
            None => commit.quarantine_finding.as_ref(),
        };
        if let Some(finding) = quarantine {
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
    let category = ensure_matrix_block::<T>(spec)?;
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
    let commit_index = layout.commit_index(&allowed, category)?;
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
        // The validity bit is consulted first because it *decides* the outcome
        // on its own: a cell whose bit is clear is not committed however its
        // slot hashes, so hashing the slot to discover that answers a question
        // already answered. The whole keyspace is still visited and every
        // ordinal still gets its bit written below, so the rebuilt map is
        // bit-identical; only the I/O follows the evidence instead of the cell
        // count. One consequence stated plainly: an I/O error on the slot or
        // CRC region of a cell whose bit is already clear is no longer
        // surfaced here, and that cell's rebuilt outcome is the same either way.
        let valid = if evidence.get(ordinal)? {
            let slot_offset = layout.slot_offset(block_index, ordinal)?;
            let actual = crc32_file_range(file, slot_offset, block.slot_stride)?;
            count_rebuild_slot_bytes_read(block.slot_stride);
            let stored = read_crc_at(
                MatrixRegionReader::new(&*file),
                indexed_crc_offset(crc_offset, ordinal)?,
            )?;
            actual == stored
        } else {
            false
        };
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
    let released = commit.bits.resident_bytes();
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
    let released_index = commit.bits.resident_index_bytes();
    commit.bits = rebuilt;
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

/// Validates `T` against the block the spec declares for `T::ID`, and returns
/// **the spec's category for that block** — which is the point, not a
/// by-product.
///
/// `T::CATEGORY` used to be checked against the descriptor here and then
/// trusted by thirteen call sites. That made a caller-declared constant the key
/// that selects which commit quarantine and which fatal-access gate apply to a
/// cell, and the check was the only thing holding it up — so the chunk path,
/// which never called this function, keyed those gates on an unvalidated
/// string. Returning the descriptor's category instead removes the load from
/// `T::CATEGORY` altogether: it is now descriptive, no code path can act on a
/// wrong value, and there is nothing left to check.
///
/// What stays checked is what a reader owes: `T` against the *file*. The spec's
/// descriptor table is tied to the file by the schema hash (`file.rs`
/// `SchemaHashMismatch`), so a `T::VERSION` or `T::DIMENSIONS` that disagrees
/// with it is a type that disagrees with the bytes on disk — which must be
/// refused rather than decoded into confidently wrong values.
pub(crate) fn ensure_matrix_block<T: VarveMatrixBlock>(spec: FormatSpec) -> Result<&'static str> {
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
    if matrix.dimensions != T::DIMENSIONS || matrix.slot_stride != T::SLOT_STRIDE {
        return Err(Error::InvalidMatrixLayout);
    }
    Ok(matrix.category)
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
/// The two gates a chunked cell access must pass, for callers outside this
/// module that address cells without a `MatrixLayout` slot.
pub(crate) fn ensure_chunk_access_allowed(layout: &MatrixLayout, category: &str) -> Result<()> {
    ensure_commit_publishable(layout, category)?;
    Ok(())
}

fn ensure_commit_publishable(layout: &MatrixLayout, category: &str) -> Result<FatalAccessAllowed> {
    let allowed = layout.ensure_fatal_access_allowed()?;
    let commit = &layout.commits[layout.commit_index(&allowed, category)?];
    if commit.quarantine_finding.is_some() {
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
    /// False when the byte already holds `byte_value`, so storing it would
    /// write back the value that is already there.
    ///
    /// [`prepare_bitmap_update`] has both operands in hand and never compared
    /// them, so every first write of a matrix cell paid a `seek` and a one-byte
    /// `write_all` to clear a commit bit that was already clear — and again for
    /// the validity bit under a checksum policy. The in-memory half of the same
    /// mutation has always treated this as a no-op (`PreparedByteWrite::changes`
    /// and the short-circuit at the top of `SparseBitmap::commit_byte_write`);
    /// this is the disk half agreeing with it.
    changes: bool,
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
        changes: byte_value != current,
    })
}

/// Stores one bitmap byte, unless the byte already holds that value.
///
/// The skip is byte-identical, not merely equivalent. `current` in
/// [`prepare_bitmap_update`] comes from [`SparseBitmap::byte`], which
/// demand-faults the page through [`SparseBitmap::faulted_store`] and
/// authenticates it against its stored digest before believing it, and answers
/// `0` only for a page that is not materialised — which is a `set_len` hole and
/// reads back as zero. So `current` *is* the durable byte, and writing
/// `byte_value == current` stores the value already on disk.
///
/// What does change is the allocation map: a bitmap page that only ever
/// received redundant writes now stays the hole `create` left, instead of being
/// allocated to hold zeros. The file's content and length are unchanged; its
/// `st_blocks` is smaller, and open can skip the page.
fn write_bitmap_byte(file: &mut File, update: &BitmapByteUpdate) -> Result<()> {
    #[cfg(test)]
    if FAIL_NEXT_BITMAP_WRITE.replace(false) {
        return Err(Error::Io(std::io::Error::other(
            "injected matrix bitmap write failure",
        )));
    }

    if !update.changes {
        return Ok(());
    }
    count_bitmap_byte_write();
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
    if commit.quarantine_finding.is_some() {
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
    //
    // And a byte that is already the intended value rehashes nothing at all. The
    // page keeps the contents its digest was taken over — `write_bitmap_byte`
    // declines the store — so the recorded digest still describes it, and
    // recomputing it produces the value already on disk. `page_bytes_are_authentic`
    // accepts the two states this can leave: an `PAGE_STATE_INITIALIZED` page
    // against its unchanged checksum, and a page never written at all, which
    // stays the hole `create` left, reads back as zero, and is exactly what
    // `PAGE_STATE_UNINITIALIZED` asserts.
    let digest = match commit.digest_offset {
        Some(_) if !bitmap.changes => None,
        Some(base) => {
            let page = bitmap.byte_index / BITMAP_PAGE_BYTES;
            let within = usize::try_from(bitmap.byte_index % BITMAP_PAGE_BYTES)
                .map_err(|_| Error::InvalidMatrixLayout)?;
            let page_bytes = commit.bits.page_bytes(page)?;
            count_bitmap_bytes_hashed(usize_to_u64(page_bytes.len())?);
            let crc = crc32_bytes_with_replacement(&page_bytes, within, bitmap.byte_value)?;
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

    /// A bitmap lent out **for persisted-page-index maintenance only**.
    ///
    /// Round 15 lent the raw `&mut SparseBitmap` for this
    /// (`CrcValidEvidence::page_index_mirror_mut`) and held it to one call site
    /// with a source gate. Re-verification showed what a `&mut SparseBitmap`
    /// is: `*evidence.page_index_mirror_mut() = attacker_bits;` replaces a
    /// block's whole CRC-validity map — completeness stays `true`, so the F-04
    /// witness then reads bits no enumeration ever produced — and
    /// `.page_index_mirror_mut().set(ordinal, true)` writes one in place. Both
    /// were two lines or fewer, and both are exactly what deleting
    /// `CrcValidEvidence::new` was meant to prevent.
    ///
    /// The wrapper is the fix, and it is a compiler refusal rather than a
    /// count: the borrow is a private field of this module, so outside it the
    /// mirror supports **only** the index-maintenance operations declared
    /// below. Assignment through it is a type error, and `SparseBitmap`'s own
    /// methods — `set`, `get`, `clear`, `prepare_byte_write` — are unreachable
    /// through it. Commit maps are wrapped the same way at the same call site,
    /// so the shared helpers keep one signature.
    pub(super) struct PageIndexMirror<'a> {
        bits: &'a mut SparseBitmap,
    }

    impl<'a> PageIndexMirror<'a> {
        /// Wraps a bitmap for index maintenance.
        ///
        /// Public to the file on purpose: it only ever *narrows* what its
        /// caller already holds. It takes a `&mut SparseBitmap` and hands back
        /// something strictly weaker, so it is no route out of an evidence
        /// type — the direction that matters is that nothing here hands a
        /// `&mut SparseBitmap` back.
        pub(super) fn new(bits: &'a mut SparseBitmap) -> Self {
            Self { bits }
        }

        /// Whether the persisted index already names `page`.
        pub(super) fn indexes_page(&self, page: u64) -> bool {
            self.bits.indexed_pages.contains_key(&page)
        }

        /// Set bits the page holding `byte_index` would have after the pending
        /// byte write — the test for "this page still needs an entry".
        pub(super) fn page_ones_after(&self, byte_index: u64, byte_value: u8) -> Result<u64> {
            self.bits.page_ones_after(byte_index, byte_value)
        }
    }

    /// Records `page` in the persisted page index before its bytes are written.
    ///
    /// PERF-01: this is what lets open enumerate the written pages without a
    /// `0..page_count` loop, including where the platform reports no allocation
    /// map at all. The order matters: an index entry whose page never reached
    /// the disk names a page that still reads as uninitialised zeros, which
    /// open already accepts, whereas a written page with no entry would be
    /// invisible to an open that has no allocation map to fall back on.
    ///
    /// Cost is one 8-byte write the first time a page is touched — one per
    /// 32,768 commit bits — and nothing at all afterwards.
    ///
    /// F-03: the whole mutation, including the compaction an already-full array
    /// needs, is prepared before the first byte reaches the disk and installed
    /// infallibly afterwards. There is no local ordering to get right here.
    pub(super) fn record_entry(
        file: &mut File,
        base: u64,
        mirror: PageIndexMirror<'_>,
        page: u64,
        budget: &mut ResidentBitmapBudget,
    ) -> Result<()> {
        let bits = mirror.bits;
        let Some(prepared) = prepare_append(base, bits, page, budget)? else {
            return Ok(());
        };
        prepared.commit(file, bits, budget)
    }

    /// Removes `page` from the persisted page index once its final set bit
    /// clears.
    ///
    /// F-03: this is what makes both the array and its in-memory mirror track
    /// *live* pages instead of every page ever published. It runs strictly
    /// after the bitmap byte that emptied the page is durable, because the safe
    /// direction is a superset: an entry naming an all-zero page costs one
    /// extra page read at open and nothing else, whereas dropping an entry for
    /// a page that still holds committed bits would hide them.
    ///
    /// `O(1)`: the vacated slot is overwritten with the array's last entry and
    /// the occupancy count is decremented — two 8-byte writes, no scan.
    pub(super) fn release_entry(
        file: &mut File,
        base: u64,
        mirror: PageIndexMirror<'_>,
        page: u64,
        budget: &mut ResidentBitmapBudget,
    ) {
        let bits = mirror.bits;
        if let Some(prepared) = prepare_release(base, bits, page) {
            prepared.commit(file, bits, budget);
        }
    }

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
        let (base, mirror) = match page_index_target(layout, target) {
            Some(parts) => parts,
            None => return Ok(()),
        };
        if mirror.indexes_page(page) || mirror.page_ones_after(byte_index, byte_value)? == 0 {
            return Ok(());
        }
        page_index::record_entry(file, base, mirror, page, &mut budget)
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
    if let Some((base, mirror)) = page_index_target(layout, target) {
        page_index::release_entry(file, base, mirror, page, &mut budget);
    }
    layout.adopt_budget(budget);
}

/// The persisted index base and bitmap a target names, or `None` where the
/// target keeps no persisted index (a session write-tracking map, or a validity
/// bitmap in a format without checksums).
fn page_index_target(
    layout: &mut MatrixLayout,
    target: PageIndexTarget,
) -> Option<(u64, page_index::PageIndexMirror<'_>)> {
    match target {
        PageIndexTarget::Commit(index) => Some((
            layout.commits[index].index_offset,
            page_index::PageIndexMirror::new(&mut layout.commits[index].bits),
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
    // Nothing below has anything to do. The durable byte already holds
    // `byte_value`, so there is no page to materialise, no residency to charge,
    // no page-index entry to publish for a live set that is not changing, and no
    // write to order against. `commit_byte_write` already answered
    // `PageResidencyDelta::NONE` for this case, so the ones counts and the
    // recency chain end up where they would have anyway.
    if !update.bitmap.changes {
        return Ok(());
    }
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
    if delta.emptied {
        // The page is now byte-for-byte the zero page, and the bitmap byte
        // that made it so is already durable, so its index entry can go
        // (F-03). Keyed on `emptied` and not on `released`: a demand-cached
        // page refunds the lazy cache rather than the budget, so it empties
        // with `released == 0` and used to keep its entry forever.
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
            .saturating_add(bits.store_mut().pages.len()),
        ReadLimitKey::MatrixBitmapBytes.resource(),
    )?;
    pages.extend(previous.indexed_pages.keys().copied());
    pages.extend(bits.store_mut().pages.keys().copied());
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
        // Faults the page in where the map is lazily backed: a republication
        // writes every page back, so it must see the published bytes rather
        // than whatever the demand cache happens to hold.
        let materialised = bits.page_is_materialised(page)?;
        if materialised {
            republish_page_index_entry(file, index_offset, bits, page, budget)?;
            rebuild_abort_stage(3);
        }
        let bytes = bits.page_bytes(page)?;
        if let Some(base) = digest_offset {
            let (crc, state) = if materialised {
                (crc32_bytes(&bytes)?, PAGE_STATE_INITIALIZED)
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
        file.write_all(&bytes)?;
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

/// Records a checksum the caller already computed over the slot's bytes.
///
/// Takes the value rather than re-reading, so a caller that has just streamed
/// the slot for another reason pays for those bytes once.
fn write_cell_crc(
    layout: &MatrixLayout,
    file: &mut File,
    block_index: usize,
    ordinal: u64,
    crc: u32,
) -> Result<()> {
    let Some(crc_offset) = layout.blocks[block_index].crc_offset else {
        return Ok(());
    };
    write_crc_at(file, indexed_crc_offset(crc_offset, ordinal)?, crc)
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
    // The commit-bit twin's short-circuit, for the same reason.
    if !update.changes {
        return Ok(());
    }
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
    if delta.emptied {
        // `emptied`, not `released` — see the commit-bit twin.
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
    reader: MatrixRegionReader<'_>,
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
    let expected = read_crc_at(reader, indexed_crc_offset(crc_offset, ordinal)?)?;
    if expected != actual {
        return Err(Error::MatrixChecksumMismatch {
            offset: layout.slot_offset(block_index, ordinal)?,
            expected,
            actual,
        });
    }
    Ok(())
}

/// Widest slot read without reaching for the wide frame.
///
/// A slot stride is a record, not a file: `SLOT_STRIDE` is 4 for a `u32` cell
/// and a few dozen bytes for a struct. Every one of those used to zero 64 KiB of
/// stack to read those bytes — the `[0u8; 64 * 1024]` local is a `memset` of the
/// whole frame before the first byte is read, and the read then fills the first
/// four of them. This bound is one page, which covers the slot widths the format
/// is for while leaving the wide frame available to the ones it is not.
const NARROW_SLOT_BYTES: usize = 4096;
/// The frame a slot wider than [`NARROW_SLOT_BYTES`] streams through. Wide
/// enough that a megabyte-class slot is 16 reads and not 256.
const WIDE_SLOT_BYTES: usize = 64 * 1024;

/// Chunk size for streaming `stride` bytes: the work, never more than the frame.
///
/// Split out from the two readers so the choice can be asserted directly. A
/// stack `memset` leaves no trace in any counter, so the *decision* is what a
/// test can hold, and the alternative — believing the frame shrank because the
/// diff says so — is how the unmeasured half of a fix ships.
const fn slot_chunk_len(stride: u64) -> usize {
    if stride <= NARROW_SLOT_BYTES as u64 {
        NARROW_SLOT_BYTES
    } else {
        WIDE_SLOT_BYTES
    }
}

/// What one pass over a slot can answer: its checksum, and whether every byte
/// of it was zero.
///
/// `commit_cell` needs both — the zero probe separates "never written" from
/// "written zeros", and the checksum is what it records — and used to take
/// them in two independent seek-and-stream passes over the same bytes.
#[derive(Clone, Copy, Debug)]
struct SlotScan {
    crc: u32,
    all_zero: bool,
}

/// Reads a slot once and answers both questions.
///
/// The CRC is computed unconditionally rather than only when the slot proves
/// non-zero: the bytes are in the buffer either way, `crc32` over them costs no
/// I/O, and computing it here is what lets the caller skip the second pass. A
/// caller that goes on to refuse the cell simply drops the value.
#[cfg(feature = "integrity")]
fn scan_slot(
    layout: &MatrixLayout,
    file: &mut File,
    block_index: usize,
    ordinal: u64,
) -> Result<SlotScan> {
    let offset = layout.slot_offset(block_index, ordinal)?;
    let stride = layout.blocks[block_index].slot_stride;
    file.seek(SeekFrom::Start(offset))?;
    if slot_chunk_len(stride) == NARROW_SLOT_BYTES {
        let mut buffer = [0u8; NARROW_SLOT_BYTES];
        read_slot_scan(file, &mut buffer, stride)
    } else {
        let mut buffer = [0u8; WIDE_SLOT_BYTES];
        read_slot_scan(file, &mut buffer, stride)
    }
}

#[cfg(feature = "integrity")]
fn read_slot_scan(file: &mut File, buffer: &mut [u8], mut remaining: u64) -> Result<SlotScan> {
    let mut hasher = crc32fast::Hasher::new();
    let mut all_zero = true;
    while remaining != 0 {
        let chunk_len = usize::try_from(remaining.min(buffer.len() as u64))
            .map_err(|_| Error::LengthOverflow { value: remaining })?;
        file.read_exact(&mut buffer[..chunk_len])?;
        count_slot_bytes_read_back(chunk_len as u64);
        let chunk = &buffer[..chunk_len];
        hasher.update(chunk);
        // Not short-circuited: the CRC needs every byte, so there is nothing to
        // gain by stopping early and the flag would then be undefined.
        all_zero &= chunk.iter().all(|byte| *byte == 0);
        remaining -= chunk_len as u64;
    }
    Ok(SlotScan {
        crc: hasher.finalize(),
        all_zero,
    })
}

/// The same single pass where the format carries no checksums: there is no CRC
/// to take, so the scan answers only the question `commit_cell` still has.
#[cfg(not(feature = "integrity"))]
fn scan_slot(
    layout: &MatrixLayout,
    file: &mut File,
    block_index: usize,
    ordinal: u64,
) -> Result<SlotScan> {
    let offset = layout.slot_offset(block_index, ordinal)?;
    let stride = layout.blocks[block_index].slot_stride;
    file.seek(SeekFrom::Start(offset))?;
    if slot_chunk_len(stride) == NARROW_SLOT_BYTES {
        let mut buffer = [0u8; NARROW_SLOT_BYTES];
        read_slot_scan(file, &mut buffer, stride)
    } else {
        let mut buffer = [0u8; WIDE_SLOT_BYTES];
        read_slot_scan(file, &mut buffer, stride)
    }
}

#[cfg(not(feature = "integrity"))]
fn read_slot_scan(file: &mut File, buffer: &mut [u8], mut remaining: u64) -> Result<SlotScan> {
    let mut all_zero = true;
    while remaining != 0 {
        let chunk_len = usize::try_from(remaining.min(buffer.len() as u64))
            .map_err(|_| Error::LengthOverflow { value: remaining })?;
        file.read_exact(&mut buffer[..chunk_len])?;
        count_slot_bytes_read_back(chunk_len as u64);
        all_zero &= buffer[..chunk_len].iter().all(|byte| *byte == 0);
        remaining -= chunk_len as u64;
    }
    Ok(SlotScan { crc: 0, all_zero })
}

#[cfg(feature = "integrity")]
fn crc32_file_range(file: &mut File, offset: u64, len: u64) -> Result<u32> {
    file.seek(SeekFrom::Start(offset))?;
    if slot_chunk_len(len) == NARROW_SLOT_BYTES {
        let mut buffer = [0u8; NARROW_SLOT_BYTES];
        crc32_streamed(file, &mut buffer, len)
    } else {
        let mut buffer = [0u8; WIDE_SLOT_BYTES];
        crc32_streamed(file, &mut buffer, len)
    }
}

#[cfg(feature = "integrity")]
fn crc32_streamed(file: &mut File, buffer: &mut [u8], mut remaining: u64) -> Result<u32> {
    let mut hasher = crc32fast::Hasher::new();
    while remaining != 0 {
        let chunk_len = usize::try_from(remaining.min(buffer.len() as u64))
            .map_err(|_| Error::LengthOverflow { value: remaining })?;
        file.read_exact(&mut buffer[..chunk_len])?;
        count_slot_bytes_read_back(chunk_len as u64);
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
    // `ones_total`, not `ones`: under a lazily backed map the maintained
    // counter describes the cached pages only, so an aggregate has to consult
    // the pages the cache is not holding. It reads them without caching them,
    // so asking for a progress figure cannot pull the whole live set resident.
    commit.bits.ones_total()
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
) -> Result<HashMap<u32, CrcValidEvidence>> {
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
        bitmaps.insert(block.block_id, CrcValidEvidence::newly_created(cell_count)?);
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
    mut crc_valid_bits: HashMap<u32, CrcValidEvidence>,
    mut commit_findings: HashMap<String, MatrixRecoveryFinding>,
    crc_findings: Vec<MatrixRecoveryFinding>,
    append_log_start: u64,
    resident_bitmap_bytes: u64,
    resident_page_index_bytes: u64,
) -> Result<MatrixLayout> {
    if commit_bits.len() != commit_plans.len() {
        return Err(Error::InvalidMatrixLayout);
    }
    // A `FatalAccessGate` no longer comes into existence here, and no longer
    // comes into existence anywhere this function can name (round 16). The
    // layout is assembled by `fatal_access::assemble_layout` at the end of this
    // function, which derives the gate from the findings the layout is built
    // with: this file scope holds no expression of the gate's type, so it can
    // neither mint one nor move one between layouts. The findings consulted are
    // the same set as before — `crc_findings` plus every entry of
    // `commit_findings`, which the loop below drains into `commits` and which
    // the check after the loop refuses to leave undrained.
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
        // The quarantine is the finding, and nothing else (0.5.0). It used to be
        // the finding *plus* the damaged map, retained beside an empty
        // replacement installed in `bits` — a second representation that existed
        // only because the eager load had the map in hand anyway. Verification
        // streams and keeps nothing, so `bits` stays the ordinary demand-backed
        // map and the finding alone fails every access to this category closed:
        // `ensure_commit_publishable` and `prepare_commit_bit` refuse before any
        // bit is addressed, and `count_committed` refuses on the damaged page
        // itself, because `ones_total` authenticates what it reads.
        let quarantine_finding = commit_findings.remove(name);
        commits.push(MatrixCommitLayout {
            name: name.clone(),
            kind: *kind,
            bit_count: *bit_count,
            map_offset,
            bits: raw_bits,
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
        let crc_valid_bits_for_block = match &crc {
            Some(_) => crc_valid_bits
                .remove(&block.block_id)
                .ok_or(Error::InvalidMatrixLayout)?,
            // Without integrity there is no validity bitmap at all, and no
            // recovery path reads one; completeness is vacuous and the refusal
            // below is unreachable for such a format.
            None => CrcValidEvidence::newly_created(0)?,
        };
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
    Ok(fatal_access::assemble_layout(
        dimensions,
        commits,
        blocks,
        aux,
        crc_findings,
        append_log_start,
        spec.read_limits,
        resident_bitmap_bytes,
        resident_page_index_bytes,
        spec,
    ))
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

/// Where one paged bitmap and its authentication metadata live on disk, plus the
/// two things every load of one needs: the allocation map its verification pass
/// unions in, and the residency policy its pages will be demand-filled under.
#[derive(Clone, Copy)]
struct PagedBitmapSource<'a> {
    base_offset: u64,
    digest_base: Option<u64>,
    index_base: u64,
    extents: Option<&'a AllocatedExtents>,
    residency: &'a LazyResidency,
}

/// Where an open is allowed to look for bitmap pages, as one value.
///
/// The two terms answer different questions and are independent (0.5.0).
/// `residency` is the demand cache every loaded bitmap is backed by; `extents`
/// is the allocation map, which exists only for the *verification* pass — it is
/// the second term of that pass's candidate set, and it is `None` when nothing
/// is going to read it, because the query itself costs a syscall and a
/// ~128 KiB NTFS run's worth of candidate pages.
#[derive(Clone, Copy)]
struct PageSources<'a> {
    extents: Option<&'a AllocatedExtents>,
    residency: &'a LazyResidency,
    /// Whether this open runs the streaming verification pass
    /// ([`crate::MatrixMetadataVerification::AtOpen`]). Resolved once, in
    /// `ReadLimits::admit_matrix_metadata_verification`.
    verify: bool,
}

/// The residency policy, resolved once per open.
///
/// Not optional any more (0.5.0): residency is always demand-filled and bounded,
/// so there is one page-loading path in this file rather than an eager one and a
/// lazy one. What the caller declares is the bound; what it used to declare —
/// "check the whole map at open" — is [`crate::MatrixMetadataVerification`].
#[derive(Clone, Debug)]
struct LazyResidency {
    /// A second descriptor for the same file, duplicated once here so a
    /// fault-in under `&self` needs no borrow of the handle a writer holds.
    file: Arc<File>,
    /// Private per-thread file objects for the fault-in read.
    ///
    /// Without this the fault-in would go through the one duplicated
    /// descriptor above, and on Windows `ReadFile` serialises on the kernel
    /// file object — so four threads missing the cache would queue behind each
    /// other's `pread` even though the lock is released for it. Measured: 1.54x
    /// *slower* on four threads than on one, which is a convoy by the same
    /// definition round 16 used. With the pool the same measurement is 0.43x,
    /// and `matrix_lazy_residency.rs` fails the build if it goes back over 1.0x.
    pool: Arc<MatrixReadPool>,
    cache_limit: u64,
}

impl LazyResidency {
    fn declared(limits: ReadLimits, file: &File) -> Result<Self> {
        // The cache is the whole of this reader's matrix bitmap payload
        // footprint, so a *declared* one is admitted against
        // `max_matrix_bitmap_bytes` — a cache larger than the ceiling would make
        // the option a way to raise a declared limit. The cache derived for an
        // undeclared policy is clamped to that ceiling instead; both decisions
        // live in `admit_matrix_metadata_residency`.
        let cache_bytes = limits.admit_matrix_metadata_residency()?;
        let pages = cache_bytes.div_ceil(BITMAP_PAGE_BYTES).max(1);
        let cache_limit = pages
            .checked_mul(BITMAP_PAGE_BYTES)
            .ok_or(Error::InvalidMatrixLayout)?;
        Ok(Self {
            file: Arc::new(file.try_clone()?),
            pool: Arc::new(MatrixReadPool::new()),
            cache_limit,
        })
    }

    fn backing(&self, source: &PagedBitmapSource<'_>) -> LazyBacking {
        LazyBacking {
            file: Arc::clone(&self.file),
            pool: Arc::clone(&self.pool),
            base_offset: source.base_offset,
            digest_base: source.digest_base,
            cache_limit: self.cache_limit,
        }
    }
}

/// Mutable state every paged-bitmap load contributes to: the resident budget
/// that admits its memory and the findings list that records its damage.
struct PagedBitmapSink<'a> {
    budget: &'a mut ResidentBitmapBudget,
    findings: &'a mut Vec<MatrixRecoveryFinding>,
}

/// Mechanical enforcement of *where completeness comes from* (round 15).
///
/// F-04's fact — "was this block's validity page index enumerated in full" — is
/// a derived boolean, and round 14 stored it in [`CrcValidEvidence`] behind a
/// `pub(super) fn new(bits, complete: bool)`. That closed forgery from outside
/// the crate and left the mint wide open inside it: one line in this file,
///
/// ```text
/// CrcValidEvidence::new(bits, true).complete()?   // compiled
/// ```
///
/// produced a completeness witness for a bitmap nothing had enumerated, which
/// is F-04 itself with the check spelled out loud instead of omitted.
///
/// Making the *type* unforgeable cannot fix that on its own, because a derived
/// fact can always be re-asserted by whoever is allowed to state it. The chain
/// has to terminate at the code that does the derivation. So the enumerator
/// lives here, in a module of its own, and returns [`PageIndexEnumeration`] — a
/// type whose two constructors have no visibility modifier at all. Nothing
/// outside this module can produce one, and `CrcValidEvidence` will not be
/// built without one, so the only route to "this evidence is complete" runs
/// through an enumeration that actually happened.
pub(crate) mod page_index_enumeration {
    use super::*;

    /// The outcome of enumerating one persisted page index.
    ///
    /// Not `Copy` and not `Clone`: it is moved into the evidence it justifies.
    #[derive(Debug)]
    #[must_use = "the enumeration outcome is what makes a validity bitmap usable \
                  as evidence; dropping it discards F-04's only input"]
    pub struct PageIndexEnumeration {
        complete: bool,
    }

    impl PageIndexEnumeration {
        /// Every page the index named was located. Private on purpose — see the
        /// module documentation; this is the constructor whose `pub(super)`
        /// equivalent was the round-14 hole.
        fn fully_enumerated() -> Self {
            Self { complete: true }
        }

        /// The index could not be enumerated in full. Every path that reaches
        /// this has already pushed a `Fatal` finding.
        fn truncated_by_damage() -> Self {
            Self { complete: false }
        }

        /// Reading the outcome is unrestricted; *stating* it is not.
        pub(super) fn is_complete(&self) -> bool {
            self.complete
        }
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
    pub(super) fn load_page_index(
        file: &mut File,
        source: &PagedBitmapSource<'_>,
        bits: &mut SparseBitmap,
        sink: &mut PagedBitmapSink<'_>,
        resource: &'static str,
        label: &str,
    ) -> Result<PageIndexEnumeration> {
        let capacity = bits.page_count.min(PAGE_INDEX_MAX_ENTRIES);
        if capacity == 0 {
            return Ok(PageIndexEnumeration::fully_enumerated());
        }
        // A region the filesystem proves to be a hole holds a zero header, which is
        // the encoding for "no entries". Reading it would answer the same thing.
        if !range_may_hold_data(source.extents, source.index_base, PAGE_INDEX_ENTRY_LEN) {
            return Ok(PageIndexEnumeration::fully_enumerated());
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
            return Ok(PageIndexEnumeration::truncated_by_damage());
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
            return Ok(PageIndexEnumeration::truncated_by_damage());
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
        Ok(if damaged == 0 {
            PageIndexEnumeration::fully_enumerated()
        } else {
            PageIndexEnumeration::truncated_by_damage()
        })
    }
}

/// Visits the deduplicated set of pages verification has to look at, without
/// materialising it.
///
/// PERF-01: it is the union of the persisted page index — the pages this matrix
/// currently holds state in — and, where the platform can answer, the pages the
/// allocation map reports as holding bytes. The first term keeps the pass bounded
/// when no allocation map exists; the second is what makes a stray byte written
/// into a page the matrix never published detectable, and it exists only where
/// the platform supplies a usable map (F-06). Neither term is derived from the
/// logical page count. **Preserving the union is not optional**: without its
/// second term a page nothing ever published is never examined at all, rather
/// than examined late.
///
/// F-07: the set is a *candidate* set, not a live-state set. For `L` indexed
/// pages and `A` allocation-derived candidates the walk costs `O(L + A)` time and
/// the caller reads up to `4096` bytes per distinct candidate. `A` follows how
/// densely the file is allocated, so a dense bitmap extent yields candidates
/// proportional to the region even when few bits are live. Sparse allocation —
/// the operating case this design targets — is what makes the union track live
/// state.
///
/// **Memory is `O(1)`.** This used to return a `Vec<u64>` of the whole union,
/// which was `Theta(L + A)` retained for the length of the load; the pass it
/// feeds now retains one page buffer and nothing else, so materialising the
/// candidate list would have been the largest thing verification held. The walk
/// is still `O(Q)` and still not `O(Q log Q)`: the index term is distinct
/// already, because it is a page-to-slot map, and the allocation term arrives in
/// ascending order, so its repeats at extent boundaries are dropped by comparing
/// with the previous unit and its overlap with the index by one hash probe. No
/// sort, and no ordering guarantee — each page is examined independently.
///
/// The page-digest array is deliberately *not* mapped back into this set. A
/// digest written without its page is a torn commit, but the index entry for
/// that page is written before either, so the index already covers it — while
/// an allocation granule spans thousands of 8-byte digest slots, so deriving
/// pages from that array would reintroduce a visit count proportional to the
/// map width.
fn for_each_candidate_page(
    extents: Option<&AllocatedExtents>,
    base_offset: u64,
    bits: &SparseBitmap,
    mut visit: impl FnMut(u64) -> Result<()>,
) -> Result<()> {
    for page in bits.indexed_pages.keys().copied() {
        visit(page)?;
    }
    let Some(extents) = extents else {
        return Ok(());
    };
    let mut previous: Option<u64> = None;
    extents.for_each_allocated_unit(
        base_offset,
        bits.byte_len,
        BITMAP_PAGE_BYTES,
        bits.page_count,
        &mut |page| {
            if previous == Some(page) || bits.indexed_pages.contains_key(&page) {
                return Ok(());
            }
            previous = Some(page);
            visit(page)
        },
    )
}

/// One reusable 4096-byte page buffer: the whole of what a verification pass
/// retains.
///
/// Stated as a type because the bound is the contract. Verification reads every
/// candidate page — `O(live pages + allocated pages)` of them — through this one
/// allocation, so its peak is `BITMAP_PAGE_BYTES` whatever the cell count, the
/// live-page count, the number of bitmaps, or the file size. Nothing it reads is
/// installed anywhere: the demand cache is filled by reads that need a page, not
/// by the pass that checks one.
struct PageVerifyBuffer {
    bytes: Vec<u8>,
}

impl PageVerifyBuffer {
    fn new() -> Result<Self> {
        Ok(Self {
            bytes: filled_bytes_for(
                BITMAP_PAGE_BYTES,
                0,
                ReadLimitKey::MatrixBitmapBytes.resource(),
            )?,
        })
    }

    /// Fills the first `len` bytes from `offset` and hands them back.
    fn read(&mut self, reader: MatrixRegionReader<'_>, offset: u64, len: u64) -> Result<&[u8]> {
        let len = usize::try_from(len).map_err(|_| Error::InvalidMatrixLayout)?;
        if len > self.bytes.len() {
            // A page is `BITMAP_PAGE_BYTES` at most by construction; a longer
            // one is a layout that cannot be trusted rather than a buffer to
            // grow, because growing it is exactly the unbounded retention this
            // type exists to refuse.
            return Err(Error::InvalidMatrixLayout);
        }
        let slice = &mut self.bytes[..len];
        reader.read_exact_at(offset, slice)?;
        Ok(&self.bytes[..len])
    }
}

/// The one implementation of "do these page bytes agree with what was published
/// for them".
///
/// `PAGE_STATE_UNINITIALIZED` asserts the page was never published and must still
/// read as zero; `PAGE_STATE_INITIALIZED` asserts the recorded checksum. The two
/// states are distinct on disk, so a page written with zeros is never confused
/// with one that was never written, and any other state is damage.
///
/// Every route that believes a commit-map page goes through here: the streaming
/// verification pass ([`verify_paged_bitmap`]), the demand fault-in
/// ([`SparseBitmap::load_page`]), and the whole-map aggregate
/// ([`SparseBitmap::ones_total`]). Two copies of this decision that could
/// disagree would be a worse defect than the one the 0.5.0 split fixed.
fn page_bytes_are_authentic(bytes: &[u8], stored: u32, state: u32) -> Result<bool> {
    Ok(match state {
        PAGE_STATE_UNINITIALIZED => stored == 0 && bytes.iter().all(|byte| *byte == 0),
        PAGE_STATE_INITIALIZED => crc32_bytes(bytes)? == stored,
        _ => false,
    })
}

/// Authenticates every candidate page of one digest-backed bitmap, retaining
/// nothing but `buffer`.
///
/// This is the verification the eager residency policy used to perform as a side
/// effect of loading. It is the same candidate set, the same per-page decision,
/// and the same `intact` answer feeding the same `Recoverable` commit-map
/// finding; what it no longer does is keep the pages.
///
/// PERF-02: a candidate page is still skipped without I/O when the filesystem
/// proves that neither the page nor its digest slot has ever been written. Such a
/// page is `PAGE_STATE_UNINITIALIZED` holding zeros, which is exactly what
/// reading it would have established, so skipping changes neither the answer nor
/// the findings — a stray byte written into an untouched page allocates it, so it
/// is still a candidate, still read, and still reported. That is about the skip
/// and presupposes the allocation map that produced it; where no map is available
/// there is nothing to skip and never-indexed pages are not candidates at all
/// (F-06).
fn verify_paged_bitmap(
    reader: MatrixRegionReader<'_>,
    extents: Option<&AllocatedExtents>,
    bits: &SparseBitmap,
    base_offset: u64,
    digest_base: u64,
    buffer: &mut PageVerifyBuffer,
) -> Result<bool> {
    let mut intact = true;
    let mut visited = 0u64;
    for_each_candidate_page(extents, base_offset, bits, |page| {
        visited = visited.saturating_add(1);
        let len = bits.page_len(page)?;
        let offset = page
            .checked_mul(BITMAP_PAGE_BYTES)
            .and_then(|delta| base_offset.checked_add(delta))
            .ok_or(Error::InvalidMatrixLayout)?;
        let digest_offset = page_digest_offset(digest_base, page)?;
        if !range_may_hold_data(extents, offset, len) {
            // The page is a hole, so it reads as zero without being read. Its
            // digest still has to agree, because a digest recorded for a page
            // whose bytes never reached the disk is a torn commit and must stay
            // detectable. Where the digest slot is itself a hole the page costs
            // no I/O at all.
            if !range_may_hold_data(extents, digest_offset, PAGE_DIGEST_LEN) {
                return Ok(());
            }
            let zeros =
                &ZERO_PAGE[..usize::try_from(len).map_err(|_| Error::InvalidMatrixLayout)?];
            let (stored, state) = read_page_digest_at(reader, digest_offset)?;
            count_open_bitmap_bytes_read(PAGE_DIGEST_LEN);
            intact &= page_bytes_are_authentic(zeros, stored, state)?;
            return Ok(());
        }
        let bytes = buffer.read(reader, offset, len)?;
        count_open_bitmap_bytes_read(len);
        let (stored, state) = read_page_digest_at(reader, digest_offset)?;
        count_open_bitmap_bytes_read(PAGE_DIGEST_LEN);
        intact &= page_bytes_are_authentic(bytes, stored, state)?;
        Ok(())
    })?;
    count_open_bitmap_pages_visited(visited);
    Ok(intact)
}

/// Attaches one bitmap region to the demand cache, and — where this open
/// verifies — authenticates every candidate page of it first.
///
/// Two independent things happen here, and 0.5.0 separated them:
///
/// * **Residency.** The persisted page index is read in full, because it is what
///   makes "not published" answerable without I/O and it is `O(live pages)` in
///   *entries* rather than in file size. Then the map is given its backing, and
///   nothing else is retained: the first bit addressed inside a page pays for
///   that page and nothing more.
/// * **Verification.** Where `source.verify` is set and the region carries a
///   digest array, [`verify_paged_bitmap`] streams the candidate set — the index
///   unioned with the allocation map — and authenticates every page in it
///   through one reusable buffer. `intact` is that verdict; it is what raises the
///   `Recoverable` commit-map finding and therefore the category quarantine.
///
/// A region with no digest array (a checksum-validity bitmap) is unauthenticated
/// by design, exactly as before this format version, so there is nothing for
/// verification to check and it is skipped — its persisted index is still
/// enumerated, and a damaged one is still a fatal finding.
fn load_paged_bitmap(
    file: &mut File,
    source: PagedBitmapSource<'_>,
    bit_count: u64,
    verifier: Option<&mut PageVerifyBuffer>,
    sink: &mut PagedBitmapSink<'_>,
    resource: &'static str,
    label: &str,
) -> Result<LoadedPagedBitmap> {
    let mut bits = SparseBitmap::new(bit_count)?;
    let index_complete =
        page_index_enumeration::load_page_index(file, &source, &mut bits, sink, resource, label)?;
    let intact = match (verifier, source.digest_base) {
        (Some(buffer), Some(digest_base)) => verify_paged_bitmap(
            MatrixRegionReader::new(&*file),
            source.extents,
            &bits,
            source.base_offset,
            digest_base,
            buffer,
        )?,
        _ => {
            count_open_bitmap_pages_visited(0);
            true
        }
    };
    bits.backing = Some(source.residency.backing(&source));
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
    index_complete: PageIndexEnumeration,
}

fn load_commit_bitmaps(
    file: &mut File,
    crc: Option<&MatrixCrcLayout>,
    page_index: &MatrixPageIndexLayout,
    commits: &[StoredCommitPlan],
    sources: PageSources<'_>,
    budget: &mut ResidentBitmapBudget,
    verification: &mut MatrixCrcVerification,
) -> Result<Vec<SparseBitmap>> {
    let mut bitmaps = Vec::new();
    try_reserve_vec(
        &mut bitmaps,
        commits.len(),
        ReadLimitKey::MatrixBitmapBytes.resource(),
    )?;
    // One buffer for the whole open, not one per category: the bound is
    // `BITMAP_PAGE_BYTES`, full stop, and a format declaring twenty commit
    // categories must not multiply it by twenty.
    let mut verifier = match sources.verify {
        true => Some(PageVerifyBuffer::new()?),
        false => None,
    };
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
                extents: sources.extents,
                residency: sources.residency,
            },
            *bit_count,
            verifier.as_mut(),
            &mut PagedBitmapSink {
                budget,
                findings: &mut verification.findings,
            },
            ReadLimitKey::MatrixBitmapBytes.resource(),
            &format!("commit category {name}"),
        )?;
        if !intact {
            verification
                .commit_findings
                .insert(name.clone(), commit_map_finding(name));
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
    sources: PageSources<'_>,
    sink: &mut PagedBitmapSink<'_>,
) -> Result<HashMap<u32, CrcValidEvidence>> {
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
                extents: sources.extents,
                residency: sources.residency,
            },
            cell_count,
            // No digest array, so there is nothing to authenticate and no
            // buffer to hand over; see `load_paged_bitmap`.
            None,
            sink,
            ReadLimitKey::MatrixCrcBytes.resource(),
            &format!("block {} validity bitmap", block.block_id),
        )?;
        // The one place a loaded bitmap becomes CRC-validity evidence: the
        // enumeration outcome is moved in with it and cannot be restated.
        valid_bits.insert(
            block.block_id,
            CrcValidEvidence::from_page_index_enumeration(loaded.bits, loaded.index_complete),
        );
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

fn read_crc_at(reader: MatrixRegionReader<'_>, offset: u64) -> Result<u32> {
    let mut bytes = [0; 4];
    reader.read_exact_at(offset, &mut bytes)?;
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

    /// The candidate-set engine, which the streaming verification pass walks
    /// instead of collecting: units must arrive in ascending order (that is what
    /// lets the caller drop cross-extent repeats by comparing with the previous
    /// one) and must never exceed the bitmap's page count.
    #[test]
    fn the_unit_walk_is_ascending_bounded_and_allocation_driven() {
        // Two allocated runs inside a region of 8 pages, the second run
        // deliberately straddling a page boundary.
        let map = extents(&[(4096, 8192), (12_000, 16_500)]);
        let mut units = Vec::new();
        map.for_each_allocated_unit(0, 8 * 4096, 4096, 8, &mut |unit| {
            units.push(unit);
            Ok(())
        })
        .expect("walk");
        assert_eq!(units, vec![1, 2, 3, 4]);
        assert!(units.windows(2).all(|pair| pair[0] < pair[1]));

        // The unit count is a hard clamp: a run past the logical end of the
        // bitmap must not yield a page the bitmap does not have.
        let mut units = Vec::new();
        map.for_each_allocated_unit(0, 8 * 4096, 4096, 2, &mut |unit| {
            units.push(unit);
            Ok(())
        })
        .expect("walk");
        assert!(units.iter().all(|unit| *unit < 2), "{units:?}");

        // A region the map proves to be a hole yields nothing at all.
        let mut visited = 0;
        extents(&[])
            .for_each_allocated_unit(0, 8 * 4096, 4096, 8, &mut |_| {
                visited += 1;
                Ok(())
            })
            .expect("walk");
        assert_eq!(visited, 0);
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

    /// The whole of what a verification pass retains, pinned as a number.
    ///
    /// The pass visits `O(live pages + allocated pages)` candidate pages and reads
    /// every one of them through this single buffer, so its peak is independent of
    /// cell count, live-page count, bitmap count and file size. The refusal is the
    /// load-bearing half: a buffer that grew to fit an unexpected page length
    /// would turn the bound into a suggestion.
    #[test]
    fn the_verification_buffer_is_one_page_and_never_grows() {
        let buffer = PageVerifyBuffer::new().expect("buffer");
        assert_eq!(
            usize_to_u64(buffer.bytes.len()).expect("len"),
            BITMAP_PAGE_BYTES
        );
        assert_eq!(buffer.bytes.capacity(), buffer.bytes.len());
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
        assert_eq!(&*map.page_bytes(1).expect("page bytes"), &[1u8]);
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

    /// Successor to `loading_an_all_zero_page_leaves_it_unmaterialized`, whose
    /// subject — `SparseBitmap::insert_loaded_page`, the eager loader's
    /// installer — no longer exists: 0.5.0 made residency demand-filled, so the
    /// only route from disk bytes to a resident page is
    /// `load_page` + `install_faulted_page`, and `load_page` answers `Ok(None)`
    /// for an all-zero page so that nothing is installed for it. That path needs
    /// a real file and is measured end to end by
    /// `matrix_lazy_residency.rs::b_resident_bytes_track_the_working_set_and_stop_at_the_ceiling`
    /// (residency rises by exactly one page per *live* page addressed).
    ///
    /// What is unit-testable here, and is now the more load-bearing half, is the
    /// decision both that path and the streaming verification pass share.
    #[test]
    fn one_authentication_decision_serves_the_fault_in_and_the_verification_pass() {
        let live = {
            let mut bytes = vec![0; BITMAP_PAGE_BYTES as usize];
            bytes[7] = 0b0000_0101;
            bytes
        };
        let zeros = vec![0; BITMAP_PAGE_BYTES as usize];

        // The half that needs no digest, and therefore holds under every feature
        // configuration. "Never published" is a distinct state from "published
        // holding zeros", and both are authentic; a page claiming the first while
        // holding bytes is not.
        assert!(page_bytes_are_authentic(&zeros, 0, PAGE_STATE_UNINITIALIZED).expect("zero page"));
        assert!(
            !page_bytes_are_authentic(&live, 0, PAGE_STATE_UNINITIALIZED).expect("stray bytes")
        );
        assert!(
            !page_bytes_are_authentic(&zeros, 1, PAGE_STATE_UNINITIALIZED).expect("torn digest")
        );
        // Any other state is damage rather than a state to interpret, decided
        // before any digest is computed.
        assert!(!page_bytes_are_authentic(&live, 0, 2).expect("unknown state"));

        // The digest half. `PAGE_STATE_INITIALIZED` is the only arm that consults
        // a checksum, and `crc32_bytes` refuses with
        // `Error::IntegrityFeatureDisabled` when the `integrity` feature is off —
        // which is also why no digest-backed bitmap exists in that build, so this
        // arm is unreachable there rather than merely untested.
        #[cfg(feature = "integrity")]
        {
            let live_crc = crc32_bytes(&live).expect("crc");
            // A published page must match its recorded checksum.
            assert!(
                page_bytes_are_authentic(&live, live_crc, PAGE_STATE_INITIALIZED)
                    .expect("authenticate")
            );
            assert!(
                !page_bytes_are_authentic(&live, live_crc ^ 1, PAGE_STATE_INITIALIZED)
                    .expect("authenticate")
            );
            assert!(
                page_bytes_are_authentic(
                    &zeros,
                    crc32_bytes(&zeros).expect("crc"),
                    PAGE_STATE_INITIALIZED
                )
                .expect("zero page")
            );
            assert!(!page_bytes_are_authentic(&live, live_crc, 2).expect("unknown state"));
        }
    }
}

/// Criterion (C), stated as a counted invariant instead of a wall clock.
///
/// The property that keeps concurrent matrix readers from serialising is not
/// that there is no lock in the read path — since demand loading there is one,
/// a `Mutex` over each bitmap's page map — but that **it is never held across
/// I/O**. These tests assert that directly: they run the read paths that fault
/// pages in and that aggregate over them, and require that the number of reads
/// issued while a page-store lock was held is zero while the number of reads
/// issued at all is not.
///
/// Deliberately here and not only in `crates/varve/tests/`: the counters are
/// compiled under `cfg(test)` as well as under the `scalable-fault-injection`
/// feature, so this module is the copy of the gate that runs in *every* feature
/// configuration, including the default one. The integration-test copy exercises
/// the same invariant through the public multi-threaded read path but needs the
/// feature to see the counters.
///
/// These are single-threaded on purpose. A violation is a property of one
/// thread's control flow — a read issued inside a critical section — so it is
/// observable without a second thread, without contention, and without a quiet
/// machine. That is the whole reason to prefer this over a ratio.
#[cfg(test)]
mod page_store_lock_audit_tests {
    use super::*;

    /// Four live pages, so an aggregate has several reads to perform and a
    /// one-page cache has something to evict.
    const PAGES: u64 = 4;

    fn reads() -> u64 {
        scaling_counters::get_always(&scaling_counters::MATRIX_REGION_READS)
    }

    fn reads_under_lock() -> u64 {
        scaling_counters::get_always(&scaling_counters::MATRIX_REGION_READS_UNDER_LOCK)
    }

    fn guards_held() -> u64 {
        scaling_counters::get_always(&scaling_counters::BITMAP_STORE_GUARDS_HELD)
    }

    fn reads_without_pool() -> u64 {
        scaling_counters::get_always(&scaling_counters::MATRIX_REGION_READS_WITHOUT_POOL)
    }

    fn reset() {
        scaling_counters::set(&scaling_counters::MATRIX_REGION_READS, 0);
        scaling_counters::set(&scaling_counters::MATRIX_REGION_READS_UNDER_LOCK, 0);
        scaling_counters::set(&scaling_counters::MATRIX_REGION_READS_WITHOUT_POOL, 0);
    }

    /// A lazily backed bitmap over a real file, every page live with one set
    /// bit, and `cache_limit` bytes of demand cache.
    ///
    /// `digest_base: None`: the subject is locking, and without digests this
    /// holds in builds without the `integrity` feature too.
    fn backed_bitmap(cache_limit: u64) -> (tempfile::TempDir, Arc<File>, SparseBitmap) {
        let directory = tempfile::tempdir().expect("temp directory");
        let path = directory.path().join("commit-map");
        let mut bytes = vec![0u8; (PAGES * BITMAP_PAGE_BYTES) as usize];
        for page in 0..PAGES {
            bytes[(page * BITMAP_PAGE_BYTES) as usize] = 0b0000_0001;
        }
        std::fs::write(&path, &bytes).expect("write map");
        let file = Arc::new(File::open(&path).expect("open map"));
        let mut map = SparseBitmap::new(PAGES * BITMAP_PAGE_BYTES * 8).expect("bitmap");
        map.backing = Some(LazyBacking {
            file: Arc::clone(&file),
            pool: Arc::new(MatrixReadPool::new()),
            base_offset: 0,
            digest_base: None,
            cache_limit,
        });
        for page in 0..PAGES {
            map.indexed_pages.insert(page, page + 1);
            map.index_slots.push(page);
        }
        (directory, file, map)
    }

    /// The first bit of `page`, which `backed_bitmap` leaves set.
    fn first_bit_of(page: u64) -> u64 {
        page * BITMAP_PAGE_BYTES * 8
    }

    /// Demand loading: `faulted_store` drops the lock for the whole of
    /// `load_page`, so a fault-in read is never issued under it.
    ///
    /// The configuration is hostile on purpose — a one-page cache against four
    /// live pages, so all but the first visit to a page misses, evicts, and
    /// enters the guarded window again.
    #[test]
    fn a_fault_in_reads_without_holding_the_page_store_lock() {
        let (_directory, _file, map) = backed_bitmap(BITMAP_PAGE_BYTES);
        reset();
        for round in 0..3 {
            for page in 0..PAGES {
                assert!(
                    map.get(first_bit_of(page)).expect("committed bit"),
                    "round {round}, page {page}"
                );
            }
        }
        assert!(
            reads() >= PAGES * 3,
            "the cache absorbed the faults, so nothing was audited: {} reads",
            reads()
        );
        assert_eq!(
            reads_under_lock(),
            0,
            "a fault-in `pread` was issued while the page store was locked; every other \
             reader of this bitmap would queue behind it"
        );
        assert_eq!(guards_held(), 0, "a page-store guard outlived its scope");
    }

    /// The whole-map aggregate: `O(live pages)` of reads, so holding the lock
    /// once for the loop would park every concurrent reader of the category for
    /// the length of the scan. Reachable under `&self` through `resume_signal`,
    /// which is what made it worth fixing rather than documenting.
    #[test]
    fn an_aggregate_reads_without_holding_the_page_store_lock() {
        let (_directory, _file, map) = backed_bitmap(BITMAP_PAGE_BYTES * PAGES);
        reset();
        assert_eq!(map.ones_total().expect("aggregate"), PAGES);
        assert!(
            reads() >= PAGES,
            "the aggregate read fewer pages than the map has: {} reads",
            reads()
        );
        assert_eq!(
            reads_under_lock(),
            0,
            "`ones_total` held the page-store lock across its reads"
        );
        assert_eq!(guards_held(), 0, "a page-store guard outlived its scope");
    }

    /// The other half of the same read path: not only must the aggregate hold
    /// no lock across its reads, it must issue them through the private-handle
    /// pool its own `LazyBacking` is already carrying. Holding the pool and
    /// reading through the shared file object is the Windows convoy in the one
    /// place it is least affordable — an `O(live pages)` loop reachable under
    /// `&self` from `resume_signal`.
    #[test]
    fn the_aggregate_reads_through_the_pool_it_holds() {
        let (_directory, _file, map) = backed_bitmap(BITMAP_PAGE_BYTES * PAGES);
        reset();
        assert_eq!(map.ones_total().expect("aggregate"), PAGES);
        assert!(
            reads() >= PAGES,
            "the aggregate read nothing, so the next assertion means nothing: {} reads",
            reads()
        );
        assert_eq!(
            reads_without_pool(),
            0,
            "`ones_total` dropped the `LazyBacking` pool it is holding and read through \
             the shared file object"
        );
    }

    /// The same question for the demand fault-in, which has been correct since
    /// the pool landed. Its value here is as the control: a fix that swaps only
    /// `load_page` leaves the aggregate's count nonzero and fails the test
    /// above while this one passes both before and after.
    #[test]
    fn a_fault_in_reads_through_the_pool_it_holds() {
        let (_directory, _file, map) = backed_bitmap(BITMAP_PAGE_BYTES * PAGES);
        reset();
        for page in 0..PAGES {
            assert!(map.get(first_bit_of(page)).expect("committed bit"));
        }
        assert!(reads() >= PAGES, "no fault-in happened: {} reads", reads());
        assert_eq!(reads_without_pool(), 0, "a fault-in dropped its pool");
    }

    /// The audit itself, which the two tests above are worthless without: a
    /// counter that can never rise proves nothing by staying at zero.
    ///
    /// This is the shape of the defect they forbid — a read issued inside the
    /// critical section — written out deliberately, and it must be counted.
    #[test]
    fn the_audit_counts_a_read_issued_under_the_lock() {
        let (_directory, file, map) = backed_bitmap(BITMAP_PAGE_BYTES);
        reset();
        let reader = MatrixRegionReader::new(file.as_ref());
        let mut byte = [0u8; 1];
        {
            let _guard = map.store();
            assert_eq!(guards_held(), 1, "the guard did not register itself");
            reader.read_exact_at(0, &mut byte).expect("read under lock");
            assert_eq!(
                reads_under_lock(),
                1,
                "the audit did not notice a read issued under the page-store lock, so its \
                 zero elsewhere means nothing"
            );
        }
        assert_eq!(guards_held(), 0, "the guard did not release its count");
        reader
            .read_exact_at(0, &mut byte)
            .expect("read outside lock");
        assert_eq!(reads(), 2, "both reads should be counted");
        assert_eq!(
            reads_under_lock(),
            1,
            "a read outside the lock was counted as a violation"
        );
        // The pool counter has to be shown able to rise for the same reason the
        // lock counter does: this reader was deliberately built with
        // `MatrixRegionReader::new`, so both of its reads are pool-less and a
        // counter stuck at zero would make the assertions above worthless.
        assert_eq!(
            reads_without_pool(),
            2,
            "the audit did not notice a read issued without the pool, so its zero \
             elsewhere means nothing"
        );
    }
}

/// The demand cache's recency order: bounded to maintain, and naming exactly
/// the pages the store holds.
///
/// Both properties used to fail, and for the same reason — the order lived in a
/// `VecDeque` beside the page map. Finding a page in it was a scan plus a
/// memmove on every cell read and write, and releasing a page from the map left
/// its entry behind, so a set/clear/re-read cycle on one page grew the deque
/// without bound against no limit.
#[cfg(test)]
mod page_store_lru_tests {
    use super::*;

    /// Published pages in the fixture. The default demand cache is 512 pages,
    /// so this is the size at which "constant per touch" and "half the cache
    /// per touch" are two orders of magnitude apart.
    const PAGES: u64 = 512;

    /// A lazily backed bitmap over a real file: pages `0..PAGES` published with
    /// one set bit each, page `PAGES` present in the address space but never
    /// published, and `cache_limit` bytes of demand cache.
    ///
    /// `digest_base: None` because the subject is the recency order, and
    /// without digests this fixture holds in builds without the `integrity`
    /// feature too.
    fn backed_bitmap(cache_limit: u64) -> (tempfile::TempDir, SparseBitmap) {
        let directory = tempfile::tempdir().expect("temp directory");
        let path = directory.path().join("commit-map");
        let mut bytes = vec![0u8; ((PAGES + 1) * BITMAP_PAGE_BYTES) as usize];
        for page in 0..PAGES {
            bytes[(page * BITMAP_PAGE_BYTES) as usize] = 0b0000_0001;
        }
        std::fs::write(&path, &bytes).expect("write map");
        let file = Arc::new(File::open(&path).expect("open map"));
        let mut map = SparseBitmap::new((PAGES + 1) * BITMAP_PAGE_BYTES * 8).expect("bitmap");
        map.backing = Some(LazyBacking {
            file,
            pool: Arc::new(MatrixReadPool::new()),
            base_offset: 0,
            digest_base: None,
            cache_limit,
        });
        for page in 0..PAGES {
            map.indexed_pages.insert(page, page + 1);
            map.index_slots.push(page);
        }
        (directory, map)
    }

    /// The first bit of `page`, which `backed_bitmap` leaves set for a
    /// published page.
    fn first_bit_of(page: u64) -> u64 {
        page * BITMAP_PAGE_BYTES * 8
    }

    fn touch_steps() -> u64 {
        scaling_counters::get_always(&scaling_counters::LRU_TOUCH_STEPS)
    }

    /// THE LEAK. A page that clears to zero is released from the store, and its
    /// place in the recency order must go with it.
    ///
    /// The cycle is the ordinary resume-and-clear one: fault a published page
    /// in, clear its last set bit so the store drops it, then read it again so
    /// it faults in afresh. `install_faulted_page` re-checks the page map and
    /// not the order, so with the order in a side container each cycle pushed a
    /// *second* entry and nothing ever removed either: growth followed
    /// clear-to-zero events, not matrix size, and `evict_one` — the only drain
    /// — never runs at all in the default profile, where the cache holds the
    /// whole commit map.
    #[test]
    fn a_page_released_by_a_clear_leaves_the_recency_order() {
        let (_directory, mut map) = backed_bitmap(BITMAP_PAGE_BYTES * PAGES);
        for round in 0..2_000 {
            assert!(
                map.get(first_bit_of(0)).expect("published bit"),
                "round {round}: the published page did not fault in"
            );
            map.set(first_bit_of(0), false).expect("clear the last bit");
            let store = map.store();
            assert!(
                !store.pages.contains_key(&0),
                "round {round}: an emptied page was retained"
            );
            store.assert_lru_consistent();
            assert_eq!(
                store.lru_len(),
                0,
                "round {round}: the recency order kept an entry for a released page"
            );
        }
    }

    /// THE SCAN. A touch costs a bounded number of map probes whatever the
    /// cache holds.
    ///
    /// Every page is cached before the measurement starts, so the only work
    /// each read performs is the recency update itself.
    ///
    /// The access order matters and it is deliberately not a sweep. The deque
    /// was kept least-recently-used first, so `0, 1, 2, …` always asked for the
    /// page at its *front* and found it in one comparison — the cheapest order
    /// for the form being replaced, not the dearest. This alternates one hot
    /// page with a sweep of the rest, which keeps the hot page near the far end
    /// of that scan.
    #[test]
    fn a_recency_touch_costs_a_bounded_number_of_probes() {
        const READS: u64 = 4_096;
        let (_directory, map) = backed_bitmap(BITMAP_PAGE_BYTES * PAGES);
        for page in 0..PAGES {
            assert!(map.get(first_bit_of(page)).expect("published bit"));
        }
        assert_eq!(
            map.store().lru_len(),
            PAGES as usize,
            "the fixture did not warm the whole cache"
        );

        scaling_counters::set(&scaling_counters::LRU_TOUCH_STEPS, 0);
        for read in 0..READS {
            let page = if read % 2 == 0 {
                0
            } else {
                1 + (read / 2) % (PAGES - 1)
            };
            assert!(map.get(first_bit_of(page)).expect("published bit"));
        }
        let steps = touch_steps();
        assert!(
            steps <= 8 * READS,
            "{READS} reads across a {PAGES}-page cache spent {steps} recency probes; the bound \
             is {} — a probe count that follows the cache size is the linear scan back",
            8 * READS
        );
        map.store().assert_lru_consistent();
    }

    /// A re-read really does make a page the most recently used, so the
    /// cheapest way to satisfy the bound above — do nothing at all — is
    /// excluded.
    #[test]
    fn eviction_drops_the_least_recently_used_page() {
        let (_directory, map) = backed_bitmap(BITMAP_PAGE_BYTES * 4);
        for page in 0..4 {
            assert!(map.get(first_bit_of(page)).expect("published bit"));
        }
        // Page 0 was the least recently used; this makes page 1 so.
        assert!(map.get(first_bit_of(0)).expect("published bit"));
        // A fifth page does not fit, so exactly one page is evicted.
        assert!(map.get(first_bit_of(4)).expect("published bit"));

        let store = map.store();
        store.assert_lru_consistent();
        assert_eq!(store.lru_len(), 4, "the cache grew past its limit");
        assert!(
            !store.pages.contains_key(&1),
            "eviction dropped a page other than the least recently used one"
        );
        assert!(
            store.pages.contains_key(&0),
            "the page the re-read made most recently used was evicted anyway"
        );
    }

    /// A page the writer materialised is not demand-cached, so it is not in the
    /// recency order and eviction cannot reach it — the property that keeps a
    /// clear of a whole category from underflowing the budget subtraction.
    #[test]
    fn a_writer_materialised_page_is_never_evicted() {
        let (_directory, mut map) = backed_bitmap(BITMAP_PAGE_BYTES * 2);
        map.set(first_bit_of(PAGES), true)
            .expect("materialise an unpublished page");
        for page in 0..PAGES {
            assert!(map.get(first_bit_of(page)).expect("published bit"));
        }
        let store = map.store();
        store.assert_lru_consistent();
        assert!(
            store.pages.contains_key(&PAGES),
            "cache pressure evicted a page the writer materialised"
        );
        assert_eq!(
            store.lru_len(),
            2,
            "the recency order does not hold exactly the demand-cached pages"
        );
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
mod slot_frame_tests {
    use super::{NARROW_SLOT_BYTES, WIDE_SLOT_BYTES, slot_chunk_len};

    /// The frame follows the slot width, and a slot narrower than a page does
    /// not reach for the wide one.
    ///
    /// This asserts a *decision*, deliberately, and it is the only thing about
    /// this change a test can hold: the cost removed is a stack `memset`, which
    /// no counter observes and no syscall count changes — a 4-byte slot is one
    /// `read` either way. What differed is that the read used to be preceded by
    /// zeroing 64 KiB of stack, 16,384 bytes of `memset` per byte of payload.
    #[test]
    fn the_slot_frame_follows_the_slot_width() {
        // The widths the format is actually for: a u32 cell, a small struct.
        for stride in [4, 8, 64, 512, NARROW_SLOT_BYTES as u64] {
            assert_eq!(
                slot_chunk_len(stride),
                NARROW_SLOT_BYTES,
                "a {stride}-byte slot reached for the wide frame"
            );
        }
        // And a slot wider than the narrow frame still streams through the wide
        // one, so a large slot is not turned into sixteen times the reads.
        for stride in [NARROW_SLOT_BYTES as u64 + 1, 1 << 20] {
            assert_eq!(
                slot_chunk_len(stride),
                WIDE_SLOT_BYTES,
                "a {stride}-byte slot was streamed through the narrow frame"
            );
        }
    }
}

#[cfg(test)]
mod crc_valid_evidence_tests {
    use super::crc_valid_evidence::CrcValidEvidence;
    use super::{Error, SparseBitmap};

    fn evidence(complete: bool) -> CrcValidEvidence {
        let mut bits = SparseBitmap::new(16).expect("bitmap");
        let prepared = bits.prepare_byte_write(0, 0b0000_0001).expect("prepare");
        bits.commit_byte_write(prepared);
        CrcValidEvidence::from_parts_for_tests_only(bits, complete)
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

/// **The in-crate bypass catalogue** (round 15).
///
/// Round 14 proved its enforcement types with `trybuild` fixtures under
/// `crates/varve/tests/ui/`. Those compile an *outside* crate against
/// `varve_core::enforcement_probe`, so they prove that a downstream user cannot
/// forge a witness — and nothing at all about `varve-core`, which is where
/// every historical defect in this class has lived. Re-verification found the
/// gap by writing one line inside this file.
///
/// This module is the missing half. It sits in `matrix.rs`, a sibling of the
/// enforcement modules, with every privilege a future defect would have. Each
/// entry below is a bypass that was **compiled and observed to fail**, quoted
/// with the diagnostic rustc emitted (rustc 1.9x, `cargo test -p varve-core
/// --lib`). Uncommenting any of them must reproduce it; if one starts
/// compiling, the property it names is gone.
///
/// ```text
/// // (1) mint a gate that says "not blocked" and spend its witness — the
/// //     named hole `FatalAccessGate::new(false).allow()`
/// let _ = FatalAccessGate::new(false);
/// //  error[E0599]: no function or associated item named `new` found for
/// //               struct `fatal_access::FatalAccessGate`
///
/// // (2) build the gate by literal instead
/// let _ = FatalAccessGate { blocked: false };
/// //  error[E0451]: field `blocked` of struct `fatal_access::FatalAccessGate`
/// //               is private
///
/// // (3) take the witness off a gate of one's own rather than off the layout
/// let gate = FatalAccessGate::evaluate([fatal_finding()].iter(), spec);
/// let _ = gate.allow();
/// //  error[E0599]: no method named `allow` found for struct
/// //               `fatal_access::FatalAccessGate`
///
/// // (4) fabricate the witness directly
/// let _ = FatalAccessAllowed(());
/// //  error[E0423]: cannot initialize a tuple struct which contains private
/// //               fields
///
/// // (5) the named hole `CrcValidEvidence::new(..)`
/// let _ = CrcValidEvidence::new(bits, true);
/// //  error[E0599]: no function or associated item named `new` found for
/// //               struct `matrix::crc_valid_evidence::CrcValidEvidence`
///
/// // (6) build the evidence by literal instead
/// let _ = CrcValidEvidence { bits, complete: true };
/// //  error[E0451]: fields `bits` and `complete` of struct
/// //               `matrix::crc_valid_evidence::CrcValidEvidence` are private
///
/// // (7) mint the enumeration outcome the evidence now demands
/// let _ = PageIndexEnumeration::fully_enumerated();
/// //  error[E0624]: associated function `fully_enumerated` is private
///
/// // (8) build the enumeration outcome by literal instead
/// let _ = PageIndexEnumeration { complete: true };
/// //  error[E0451]: field `complete` of struct
/// //               `page_index_enumeration::PageIndexEnumeration` is private
///
/// // (9) fabricate the completeness witness directly
/// let _ = crc_valid_evidence::CompleteCrcValidEvidence { bits: &bitmap };
/// //  error[E0451]: field `bits` of struct `CompleteCrcValidEvidence` is
/// //               private
/// ```
///
/// **Round 16 entries.** Re-verification of round 15 found that the two named
/// holes were closed and the *class* was not: six bypasses still compiled
/// inside `varve-core`, three of them one or two lines, and two of them
/// appeared in no report. Each of the following was compiled from a module with
/// this one's privileges and observed to fail, with the diagnostic quoted.
///
/// ```text
/// // (10) reassign the gate a layout carries — one line, with any
/// //      `&mut MatrixLayout` in scope, and it undoes `block()`
/// layout.fatal_access = FatalAccessGate::evaluate(findings.iter(), spec);
/// //  error[E0624]: associated function `evaluate` is private
///
/// // (10b) lift the gate off a layout freshly assembled with no findings
/// layout.fatal_access = fatal_access::assemble_layout(..).fatal_access;
/// //  error[E0509]: cannot move out of type `matrix::MatrixLayout`, which
/// //               implements the `Drop` trait
///
/// // (11) clone an unblocked gate off a healthy layout onto a blocked one
/// layout.fatal_access = donor.fatal_access.clone();
/// //  error[E0599]: no method named `clone` found for struct
/// //               `FatalAccessGate`
///
/// // (11b) the same by cloning the whole layout and moving the field out
/// layout.fatal_access = donor.clone().fatal_access;
/// //  error[E0509]: cannot move out of type `matrix::MatrixLayout`, which
/// //               implements the `Drop` trait
///
/// // (11c) or by swapping it in
/// core::mem::replace(&mut layout.fatal_access, donor.fatal_access.clone());
/// //  error[E0599]: no method named `clone` found for struct
/// //               `FatalAccessGate`
///
/// // (12) launder a whole bitmap into F-04 completeness through the one
/// //      named escape — this one was *executed* by re-verification: a bit
/// //      set in a bitmap no enumeration produced read back `true` through
/// //      `CompleteCrcValidEvidence::get`
/// *evidence.page_index_mirror_mut() = attacker_bits;
/// //  error[E0614]: type `PageIndexMirror<'_>` cannot be dereferenced
///
/// // (12b) or take the raw bitmap borrow back out of the escape
/// let _bits: &mut SparseBitmap = evidence.page_index_mirror_mut();
/// //  error[E0308]: mismatched types: expected `&mut SparseBitmap`, found
/// //               `PageIndexMirror<'_>`
///
/// // (13) set a validity bit in place through the escape
/// block.crc_valid_bits.page_index_mirror_mut().set(0, true);
/// //  error[E0599]: no method named `set` found for struct
/// //               `PageIndexMirror<'a>`
///
/// // (13b) or read one through it, bypassing the completeness witness
/// block.crc_valid_bits.page_index_mirror_mut().get(0);
/// //  error[E0599]: no method named `get` found for struct
/// //               `PageIndexMirror<'a>`
///
/// // (14) install a fabricated bitmap as complete evidence
/// block.crc_valid_bits = CrcValidEvidence::from_page_index_enumeration(
///     attacker_bits, PageIndexEnumeration::fully_enumerated());
/// //  error[E0624]: associated function `fully_enumerated` is private
/// ```
///
/// **What still compiles from inside this file.** Written out rather than
/// implied, because the failure this round exists to stop is a criterion
/// reported as met when half of it was. Each of these was compiled from this
/// module and observed to build:
///
/// ```text
/// // (a) fabricate a bitmap and the bitmap engine's prepared values
/// let bits = SparseBitmap { bit_count: 0, byte_len: 0, page_count: 0,
///                           pages: HashMap::new(), ones: 0,
///                           index_slots: Vec::new(),
///                           indexed_pages: HashMap::new() };                  // compiles
/// let update = BitmapByteUpdate { byte_index: 0, byte_offset: 0, byte_value: 0 };  // compiles
/// let commit = CommitBitUpdate { bitmap: update, digest: None };                   // compiles
/// let write = PreparedByteWrite { page: 0, within: 0, value: 0, fresh: None, .. }; // compiles
/// let bit = PreparedWriteBit { cost: 0, write };                                   // compiles
/// ```
///
/// These are `SparseBitmap` and its shape-A prepared values, declared at *file*
/// scope, so their private fields are nameable from all ~8000 lines of
/// `matrix.rs`: a fabricated `PreparedByteWrite` claiming `fresh: None` can be
/// handed to `commit_byte_write` for a page that is not resident. Closing it
/// means moving `SparseBitmap` and its prepared values into a
/// `mod sparse_bitmap`, a large refactor of the hot bitmap path,
/// **not attempted here** — checklist open item 26.
///
/// ```text
/// // (b) install a fabricated bitmap as a commit category's published map
/// layout.commits[0].bits = attacker_bits;                            // compiles
///
/// // (c) replace a block's CRC-validity evidence with a fresh empty one
/// block.crc_valid_bits = CrcValidEvidence::newly_created(64)?;       // compiles
///
/// // (d) build a whole block layout by literal
/// let _ = MatrixBlockLayout { block_id: 0, .. };                     // compiles
/// ```
///
/// (b) is the one with teeth: commit maps are ordinary `SparseBitmap` fields of
/// a file-scope struct, so a line in this file can publish a commit view
/// nothing committed. It is the same shape as (12) above, one level out — the
/// CRC-validity half is now behind `CrcValidEvidence` and refuses it, the
/// commit half is not behind anything. Closing it needs `MatrixLayout` and
/// `MatrixCommitLayout` behind a module boundary (checklist open item 25), so
/// that `commits` cannot be indexed for mutation from file scope.
///
/// (c) and (d) are laundering in the fail-closed direction and are recorded for
/// completeness rather than as live risk: `newly_created` builds its own empty
/// map, and an absent validity bit makes `require_meaningful` refuse and
/// `CompleteCrcValidEvidence::get` answer "not established". Neither can make a
/// cell look *verified*; that route was (14), and it is refused.
///
/// ```text
/// // (e) a decoy poison flag with 'static lifetime, returnable from a
/// //     writer's own `poison_flag()`
/// let _: &'static PoisonFlag = Box::leak(Box::new(PoisonFlag::healthy()));  // compiles
/// ```
///
/// Narrowed in round 15 (the `static DECOY` spelling is E0015) and still open
/// in this one; caught by a source gate on `poison_flag` impls only. It lives
/// in `writer_permit.rs` and wants the writer's fields split into a
/// borrow-disjoint inner struct — checklist open item 21.
#[cfg(test)]
mod bypass_catalogue {
    use super::*;

    pub(super) fn probe_spec() -> FormatSpec {
        FormatSpec::new(
            b"PROB",
            1,
            crate::Endian::Little,
            0,
            crate::IndexPolicy::ScanOnOpen,
            IntegrityPolicy::None,
            crate::RecoveryPolicy::Strict,
            crate::ManifestPolicy::None,
            &[],
        )
    }

    /// The legitimate route still works: an enumeration that really ran is the
    /// only thing that yields completeness, and it yields it.
    ///
    /// (`newly_created` is the create-time constructor; it takes a bit *count*,
    /// not a bitmap, so it cannot launder one that came from elsewhere.)
    #[test]
    fn the_checked_route_still_produces_the_evidence_it_should() {
        let fresh = CrcValidEvidence::newly_created(16).expect("fresh evidence");
        let witness = fresh
            .complete()
            .expect("a matrix that published nothing is complete");
        assert!(!witness.get(0).expect("clear bit"));
    }

    /// The gate's own derivation test now lives *inside* `mod fatal_access`
    /// (`fatal_access::tests::the_gate_is_derived_from_the_findings_not_supplied`),
    /// because `evaluate` is private to that module as of round 16. A copy of
    /// it here would not compile, and that is the point: this module has the
    /// privileges of a future defect, and a future defect can no longer name
    /// the constructor.
    ///
    /// What this module can still check is that the laundering routes
    /// re-verification executed are gone. The mirror escape hands back a
    /// [`page_index::PageIndexMirror`], so the two spellings that ran —
    /// whole-map replacement and an in-place `set` — are type errors rather
    /// than one-liners; entries (10) to (13) above quote them.
    #[test]
    fn the_mirror_escape_lends_no_route_back_to_the_bitmap() {
        let mut evidence = CrcValidEvidence::newly_created(64).expect("fresh evidence");
        let mirror = evidence.page_index_mirror_mut();
        // The whole surface the escape has outside `mod page_index`: two
        // questions about the persisted index. Neither reads a validity bit,
        // and there is no third.
        assert!(!mirror.indexes_page(0));
        assert_eq!(
            mirror.page_ones_after(0, 0).expect("page ones"),
            0,
            "a freshly created map holds no set bits"
        );
        // And the evidence still reads as the empty map it was built as: no
        // bit has been laundered into it.
        let witness = evidence.complete().expect("newly created is complete");
        assert!(!witness.get(7).expect("clear bit"));
    }
}
