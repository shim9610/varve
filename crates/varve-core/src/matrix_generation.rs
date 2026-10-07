//! Copy-on-write matrix pages. Published roots and pages are immutable.
//! Only the single writer can see the working directory; each public reader
//! captures a confirmed root and owns its own bounded index-page cache.
use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

const PAGE: usize = 4096;
const FANOUT: usize = 128;
const HEADER: u64 = 3 * PAGE as u64;
const HEAD_MAGIC: &[u8; 8] = b"VMGHEAD1";
const MAGIC: &[u8; 8] = b"VARVEMG1";
const NODE_MAGIC: &[u8; 8] = b"VMGNODE1";
const CACHE: usize = 2 * 1024 * 1024;
fn bad(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}
fn read_at(file: &File, mut at: u64, mut bytes: &mut [u8]) -> io::Result<()> {
    while !bytes.is_empty() {
        let n = match crate::snapshot::read_at(file, bytes, at) {
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            result => result?,
        };
        if n == 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        at = at
            .checked_add(n as u64)
            .ok_or_else(|| bad("matrix offset overflow"))?;
        bytes = &mut bytes[n..];
    }
    Ok(())
}
fn write_at(file: &File, mut at: u64, mut bytes: &[u8]) -> io::Result<()> {
    while !bytes.is_empty() {
        #[cfg(unix)]
        let result = std::os::unix::fs::FileExt::write_at(file, bytes, at);
        #[cfg(windows)]
        let result = std::os::windows::fs::FileExt::seek_write(file, bytes, at);
        #[cfg(not(any(unix, windows)))]
        let result: io::Result<usize> = Err(io::ErrorKind::Unsupported.into());
        let n = match result {
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            r => r?,
        };
        if n == 0 {
            return Err(io::ErrorKind::WriteZero.into());
        }
        at = at
            .checked_add(n as u64)
            .ok_or_else(|| bad("matrix offset overflow"))?;
        bytes = &bytes[n..];
    }
    Ok(())
}
fn independent_base(file: &File) -> io::Result<Arc<File>> {
    #[cfg(windows)]
    {
        use std::os::windows::io::{AsRawHandle, FromRawHandle, RawHandle};
        use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
        use windows_sys::Win32::Storage::FileSystem::{
            FILE_GENERIC_READ, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, ReOpenFile,
        };
        // The COW base reader must not share the primary writer's OS cursor.
        // Reopen the pinned object, never its potentially replaced pathname.
        let raw = unsafe {
            ReOpenFile(
                file.as_raw_handle() as _,
                FILE_GENERIC_READ,
                FILE_SHARE_DELETE | FILE_SHARE_READ | FILE_SHARE_WRITE,
                0,
            )
        };
        if raw.is_null() || raw == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: ReOpenFile returned a new owned read-only handle.
        Ok(Arc::new(unsafe { File::from_raw_handle(raw as RawHandle) }))
    }
    #[cfg(not(windows))]
    {
        Ok(Arc::new(file.try_clone()?))
    }
}
fn u64_at(bytes: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(bytes[at..at + 8].try_into().unwrap())
}
fn u32_at(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap())
}
fn checksum(bytes: &[u8]) -> u32 {
    crc32fast::hash(bytes)
}
fn seal(bytes: &mut [u8; PAGE]) {
    let crc = checksum(&bytes[..PAGE - 4]);
    bytes[PAGE - 4..].copy_from_slice(&crc.to_le_bytes());
}
fn authentic(bytes: &[u8; PAGE]) -> bool {
    checksum(&bytes[..PAGE - 4]) == u32_at(bytes, PAGE - 4)
}
pub(crate) fn path(primary: &Path, nonce: [u8; 16]) -> PathBuf {
    let mut name = primary.as_os_str().to_os_string();
    name.push(".");
    for byte in nonce {
        name.push(format!("{byte:02x}"));
    }
    name.push(".vmg");
    name.into()
}

#[cfg(feature = "scalable-fault-injection")]
fn publication_fault(stage: &str) {
    if std::env::var("VARVE_MATRIX_GENERATION_ABORT").as_deref() == Ok(stage) {
        if let Some(path) = std::env::var_os("VARVE_MATRIX_GENERATION_TRACE") {
            std::fs::write(path, stage).expect("write abort witness");
        }
        std::process::abort();
    }
}
#[cfg(not(feature = "scalable-fault-injection"))]
fn publication_fault(_stage: &str) {}

fn publication_error(stage: &str) -> io::Result<()> {
    #[cfg(feature = "scalable-fault-injection")]
    if std::env::var("VARVE_MATRIX_GENERATION_FAIL").as_deref() == Ok(stage) {
        return Err(io::Error::other("injected matrix publication failure"));
    }
    let _ = stage;
    Ok(())
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Entry {
    offset: u64,
    crc: u32,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Head {
    pub generation: u64,
    root: u64,
    end: u64,
    pub native_eof: u64,
    /// Physical-file replacement epoch; ordinary publications preserve it.
    pub compaction_epoch: u64,
}
#[derive(Debug, Default)]
struct Cache {
    pages: HashMap<(u64, Option<u32>), Arc<[u8; PAGE]>>,
    order: VecDeque<(u64, Option<u32>)>,
}
// This is writer-internal state, never attached to a public read-only handle.
// Backing bitmap views need to read the writer's current pages after eviction.
// The skip map shares immutable physical addresses, not mutable page buffers.
#[derive(Debug)]
struct Working {
    pages: crossbeam_skiplist::SkipMap<u64, Entry>,
    root: AtomicU64,
    end: AtomicU64,
}
#[derive(Debug)]
pub(crate) struct View {
    base: Arc<File>,
    log: Arc<File>,
    head: Head,
    len: u64,
    height: u8,
    cache: RefCell<Cache>,
    cache_bytes: usize,
    working: Option<Arc<Working>>,
}
impl Clone for View {
    fn clone(&self) -> Self {
        Self {
            base: self.base.clone(),
            log: self.log.clone(),
            head: self.head,
            len: self.len,
            height: self.height,
            cache: RefCell::default(),
            cache_bytes: self.cache_bytes,
            working: self.working.clone(),
        }
    }
}
impl View {
    fn cache_page(&self, offset: (u64, Option<u32>), page: Arc<[u8; PAGE]>) {
        let limit = self.cache_bytes / PAGE;
        if limit == 0 {
            return;
        }
        let mut cache = self.cache.borrow_mut();
        if cache.pages.contains_key(&offset) {
            return;
        }
        while cache.pages.len() >= limit {
            if let Some(old) = cache.order.pop_front() {
                cache.pages.remove(&old);
            }
        }
        cache.pages.insert(offset, page);
        cache.order.push_back(offset);
    }
    fn root_end(&self) -> (u64, u64) {
        self.working
            .as_ref()
            .map_or((self.head.root, self.head.end), |w| {
                (
                    w.root.load(Ordering::Relaxed),
                    w.end.load(Ordering::Relaxed),
                )
            })
    }
    fn node(&self, offset: u64, height: u8) -> io::Result<Arc<[u8; PAGE]>> {
        let (_, end) = self.root_end();
        if offset < HEADER
            || !offset.is_multiple_of(PAGE as u64)
            || offset.checked_add(PAGE as u64).is_none_or(|v| v > end)
        {
            return Err(bad("matrix node outside checkpoint"));
        }
        if let Some(page) = self.cache.borrow().pages.get(&(offset, None)) {
            if page[8] != height {
                return Err(bad("invalid cached matrix directory height"));
            }
            return Ok(page.clone());
        }
        let mut page = [0; PAGE];
        read_at(&self.log, offset, &mut page)?;
        if &page[..8] != NODE_MAGIC || page[8] != height || !authentic(&page) {
            return Err(bad("invalid matrix page directory"));
        }
        let page = Arc::new(page);
        self.cache_page((offset, None), page.clone());
        Ok(page)
    }
    fn entry(&self, page: u64) -> io::Result<Entry> {
        let (mut offset, _) = self.root_end();
        for height in (0..self.height).rev() {
            if offset == 0 {
                return Ok(Entry::default());
            }
            let node = self.node(offset, height)?;
            let at = 32 + (((page >> (height * 7)) & 127) as usize) * 16;
            let entry = Entry {
                offset: u64_at(&node[..], at),
                crc: u32_at(&node[..], at + 8),
            };
            if height == 0 {
                return Ok(entry);
            }
            offset = entry.offset;
        }
        Err(bad("invalid matrix directory height"))
    }
    fn page(&self, page: u64, bytes: &mut [u8; PAGE]) -> io::Result<()> {
        let at = page
            .checked_mul(PAGE as u64)
            .ok_or_else(|| bad("matrix page overflow"))?;
        if at >= self.len {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        if let Some(working) = &self.working
            && let Some(entry) = working.pages.get(&page)
        {
            let entry = *entry.value();
            if entry.offset == 1 {
                bytes.fill(0);
            } else {
                read_at(&self.log, entry.offset, bytes)?;
            }
            return Ok(());
        }
        let entry = self.entry(page)?;
        match entry.offset {
            0 => {
                bytes.fill(0);
                let n = ((self.len - at).min(PAGE as u64)) as usize;
                read_at(&self.base, at, &mut bytes[..n])?;
            }
            1 => bytes.fill(0),
            offset => {
                let (_, end) = self.root_end();
                if offset < HEADER
                    || !offset.is_multiple_of(PAGE as u64)
                    || offset.checked_add(PAGE as u64).is_none_or(|v| v > end)
                {
                    return Err(bad("matrix data outside checkpoint"));
                }
                if let Some(page) = self.cache.borrow().pages.get(&(offset, Some(entry.crc))) {
                    bytes.copy_from_slice(&page[..]);
                    return Ok(());
                }
                read_at(&self.log, offset, bytes)?;
                if checksum(bytes) != entry.crc {
                    return Err(bad("matrix generation page checksum mismatch"));
                }
                self.cache_page((offset, Some(entry.crc)), Arc::new(*bytes));
            }
        }
        Ok(())
    }
    pub(crate) fn read_exact_at(&self, mut at: u64, mut bytes: &mut [u8]) -> io::Result<()> {
        if at
            .checked_add(bytes.len() as u64)
            .is_none_or(|end| end > self.len)
        {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        while !bytes.is_empty() {
            let within = at as usize % PAGE;
            let n = bytes.len().min(PAGE - within);
            let mut page = [0; PAGE];
            self.page(at / PAGE as u64, &mut page)?;
            bytes[..n].copy_from_slice(&page[within..within + n]);
            at += n as u64;
            bytes = &mut bytes[n..];
        }
        Ok(())
    }
    /// Compare immutable radix roots. Equal physical pointers identify an entire
    /// unchanged subtree, but only within the same compaction epoch.
    pub(crate) fn changed_pages(&self, next: &Self, limit: usize) -> io::Result<Vec<u64>> {
        if self.head.compaction_epoch != next.head.compaction_epoch
            || self.len != next.len
            || self.height != next.height
            || self.working.is_some()
            || next.working.is_some()
        {
            return Err(bad("matrix roots belong to different physical files"));
        }
        let mut pages = Vec::new();
        self.diff_node(
            next,
            (self.head.root, next.head.root),
            self.height - 1,
            0,
            &mut pages,
            limit,
        )?;
        Ok(pages)
    }
    #[allow(clippy::too_many_arguments)]
    fn diff_node(
        &self,
        next: &Self,
        roots: (u64, u64),
        height: u8,
        prefix: u64,
        pages: &mut Vec<u64>,
        limit: usize,
    ) -> io::Result<()> {
        if roots.0 == roots.1 {
            return Ok(());
        }
        let old = (roots.0 != 0)
            .then(|| self.node(roots.0, height))
            .transpose()?;
        let new = (roots.1 != 0)
            .then(|| next.node(roots.1, height))
            .transpose()?;
        for i in 0..FANOUT {
            let entry = |node: &Option<Arc<[u8; PAGE]>>| {
                node.as_ref().map_or(Entry::default(), |n| Entry {
                    offset: u64_at(&n[..], 32 + i * 16),
                    crc: u32_at(&n[..], 40 + i * 16),
                })
            };
            let a = entry(&old);
            let b = entry(&new);
            if a == b {
                continue;
            }
            let key = prefix | ((i as u64) << (height * 7));
            if height == 0 {
                if key >= self.len.div_ceil(PAGE as u64) {
                    return Err(bad("matrix diff outside region"));
                }
                if pages.len() >= limit {
                    return Err(bad("matrix follow diff budget exceeded"));
                }
                pages.try_reserve(1).map_err(io::Error::other)?;
                pages.push(key);
            } else {
                self.diff_node(next, (a.offset, b.offset), height - 1, key, pages, limit)?;
            }
        }
        Ok(())
    }
    pub(crate) fn allocated_ranges(&self) -> io::Result<Vec<(u64, u64)>> {
        let mut pages = Vec::new();
        self.visit(
            self.root_end().0,
            self.height - 1,
            0,
            &mut pages,
            0,
            u64::MAX,
            65_536,
        )?;
        if let Some(working) = &self.working {
            pages.extend(working.pages.iter().map(|entry| *entry.key()));
            pages.sort_unstable();
            pages.dedup();
        }
        let mut ranges = Vec::new();
        for page in pages {
            let entry = if let Some(entry) = self.working.as_ref().and_then(|w| w.pages.get(&page))
            {
                *entry.value()
            } else {
                self.entry(page)?
            };
            if entry.offset > 1 {
                ranges.push((page * PAGE as u64, ((page + 1) * PAGE as u64).min(self.len)));
            }
        }
        Ok(ranges)
    }
    #[allow(clippy::too_many_arguments)]
    fn visit(
        &self,
        offset: u64,
        height: u8,
        prefix: u64,
        out: &mut Vec<u64>,
        first: u64,
        last: u64,
        limit: usize,
    ) -> io::Result<()> {
        if offset == 0 {
            return Ok(());
        }
        let node = self.node(offset, height)?;
        for i in 0..FANOUT {
            let key = prefix | ((i as u64) << (height * 7));
            let max = key | ((1u64 << (height * 7)) - 1);
            if max < first || key > last {
                continue;
            }
            let child = u64_at(&node[..], 32 + i * 16);
            if child == 0 {
                continue;
            }
            if height == 0 {
                if key >= self.len.div_ceil(PAGE as u64) {
                    return Err(bad("matrix directory key outside region"));
                }
                if out.len() >= limit {
                    return Err(bad("matrix directory enumeration budget exceeded"));
                }
                out.try_reserve(1).map_err(io::Error::other)?;
                out.push(key);
            } else {
                self.visit(child, height - 1, key, out, first, last, limit)?;
            }
        }
        Ok(())
    }
}

#[derive(Debug)]
pub(crate) struct MatrixFile {
    view: View,
    writer: Option<File>,
    cursor: u64,
    path: PathBuf,
    nonce: [u8; 16],
    max_pending: usize,
}
impl MatrixFile {
    pub(crate) fn create(
        primary: &Path,
        base: &File,
        len: u64,
        nonce: [u8; 16],
        max_pending: usize,
    ) -> io::Result<Self> {
        let path = path(primary, nonce);
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&path)?;
        let mut header = [0; PAGE];
        header[..8].copy_from_slice(MAGIC);
        header[8..24].copy_from_slice(&nonce);
        header[24..32].copy_from_slice(&len.to_le_bytes());
        seal(&mut header);
        write_at(&file, 0, &header)?;
        file.set_len(HEADER)?;
        base.sync_all()?;
        file.sync_all()?;
        let head = Head {
            generation: 0,
            root: 0,
            end: HEADER,
            native_eof: len,
            compaction_epoch: 0,
        };
        Self::write_head(&file, head)?;
        file.sync_all()?;
        Self::open(primary, base, nonce, true, max_pending)
    }
    pub(crate) fn open(
        primary: &Path,
        base: &File,
        nonce: [u8; 16],
        writable: bool,
        max_pending: usize,
    ) -> io::Result<Self> {
        let path = path(primary, nonce);
        let log = Arc::new(File::open(&path)?);
        let mut header = [0; PAGE];
        read_at(&log, 0, &mut header)?;
        if &header[..8] != MAGIC || header[8..24] != nonce || !authentic(&header) {
            return Err(bad("matrix generation identity mismatch"));
        }
        let len = u64_at(&header, 24);
        if len == 0 {
            return Err(bad("empty matrix generation"));
        }
        let head = Self::latest(&log, len)?;
        let mut height = 1;
        let pages = (len - 1) / PAGE as u64;
        while pages >> (height * 7) != 0 {
            height += 1;
        }
        let writer = if writable {
            Some(OpenOptions::new().read(true).write(true).open(&path)?)
        } else {
            None
        };
        let working = writer.as_ref().map(|_| {
            Arc::new(Working {
                pages: crossbeam_skiplist::SkipMap::new(),
                root: AtomicU64::new(head.root),
                end: AtomicU64::new(head.end),
            })
        });
        Ok(Self {
            view: View {
                base: independent_base(base)?,
                log,
                head,
                len,
                height,
                cache: RefCell::default(),
                cache_bytes: CACHE,
                working,
            },
            writer,
            cursor: 0,
            path,
            nonce,
            max_pending,
        })
    }
    fn write_head(file: &File, head: Head) -> io::Result<()> {
        let mut bytes = [0; PAGE];
        bytes[..8].copy_from_slice(HEAD_MAGIC);
        for (at, value) in [
            (8, head.generation),
            (16, head.root),
            (24, head.end),
            (32, head.native_eof),
            (40, head.compaction_epoch),
        ] {
            bytes[at..at + 8].copy_from_slice(&value.to_le_bytes());
        }
        seal(&mut bytes);
        let at = PAGE as u64 * (1 + head.generation % 2);
        write_at(file, at, &bytes[..PAGE / 2])?;
        publication_fault("head_prefix");
        publication_error("head_prefix")?;
        write_at(file, at + PAGE as u64 / 2, &bytes[PAGE / 2..])?;
        publication_fault("head");
        publication_error("head")?;
        Ok(())
    }
    fn latest(log: &File, len: u64) -> io::Result<Head> {
        let mut previous = None;
        for _ in 0..4 {
            let mut bytes = [[0; PAGE]; 2];
            for (slot, page) in bytes.iter_mut().enumerate() {
                read_at(log, (slot as u64 + 1) * PAGE as u64, page)?;
            }
            // Capture length after heads so a concurrently completed publication
            // cannot name pages beyond an earlier length sample.
            let file_len = log.metadata()?.len();
            let mut found = None;
            for page in &bytes {
                if &page[..8] != HEAD_MAGIC || !authentic(page) {
                    continue;
                }
                let head = Head {
                    generation: u64_at(page, 8),
                    root: u64_at(page, 16),
                    end: u64_at(page, 24),
                    native_eof: u64_at(page, 32),
                    compaction_epoch: u64_at(page, 40),
                };
                if head.end < HEADER
                    || head.end > file_len
                    || head.native_eof < len
                    || (head.root != 0
                        && (head.root < HEADER
                            || !head.root.is_multiple_of(PAGE as u64)
                            || head
                                .root
                                .checked_add(PAGE as u64)
                                .is_none_or(|n| n > head.end)))
                {
                    return Err(bad("invalid matrix checkpoint bounds"));
                }
                if found.is_none_or(|old: Head| old.generation < head.generation) {
                    found = Some(head);
                }
            }
            if let Some(head) = found {
                return Ok(head);
            }
            if previous.as_ref() == Some(&bytes) {
                return Err(bad("no valid matrix generation checkpoint"));
            }
            previous = Some(bytes);
        }
        Err(io::Error::new(
            io::ErrorKind::WouldBlock,
            "matrix publication changed during open; retry",
        ))
    }
    pub(crate) fn set_cache_bytes(&mut self, bytes: usize) {
        self.view.cache_bytes = bytes;
        self.view.cache.get_mut().pages.clear();
        self.view.cache.get_mut().order.clear();
    }
    pub(crate) fn read_clone(&self) -> Self {
        Self {
            view: self.view.clone(),
            writer: None,
            cursor: 0,
            path: self.path.clone(),
            nonce: self.nonce,
            max_pending: self.max_pending,
        }
    }
    pub(crate) fn barrier_file(&mut self) -> io::Result<&mut File> {
        self.writer
            .as_mut()
            .ok_or_else(|| io::ErrorKind::PermissionDenied.into())
    }
    pub(crate) fn is_dirty(&self) -> bool {
        self.view
            .working
            .as_ref()
            .is_some_and(|w| !w.pages.is_empty())
    }
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }
    pub(crate) fn head(&self) -> Head {
        self.view.head
    }
    #[cfg(feature = "mmap")]
    pub(crate) fn mapped_pages(&self) -> io::Result<Vec<u64>> {
        let mut pages = Vec::new();
        self.view.visit(
            self.view.root_end().0,
            self.view.height - 1,
            0,
            &mut pages,
            0,
            u64::MAX,
            self.max_pending,
        )?;
        if let Some(working) = &self.view.working {
            pages.extend(working.pages.iter().map(|entry| *entry.key()));
            pages.sort_unstable();
            pages.dedup();
        }
        Ok(pages)
    }
    #[cfg(feature = "mmap")]
    pub(crate) fn patch_mapping(&self, bytes: &mut [u8], pages: &[u64]) -> io::Result<()> {
        for page in pages {
            let at = page * PAGE as u64;
            let n = ((self.view.len - at).min(PAGE as u64)) as usize;
            let at = usize::try_from(at).map_err(io::Error::other)?;
            let target = bytes
                .get_mut(at..at + n)
                .ok_or_else(|| bad("matrix mapping outside snapshot"))?;
            self.view.read_exact_at(at as u64, target)?;
        }
        Ok(())
    }
    pub(crate) fn changed_pages(&self, next: &Self) -> io::Result<Vec<u64>> {
        self.view.changed_pages(&next.view, self.max_pending)
    }
    /// Physical cache addresses remain valid within an append-only epoch.
    /// Move the cache at adoption, without cloning it during preparation.
    pub(crate) fn inherit_cache(&mut self, old: &mut Self) {
        if self.view.head.compaction_epoch == old.view.head.compaction_epoch {
            std::mem::swap(self.view.cache.get_mut(), old.view.cache.get_mut());
        }
    }
    pub(crate) fn refreshed(&self) -> io::Result<Self> {
        Self::open_from_log(self)
    }
    fn open_from_log(old: &Self) -> io::Result<Self> {
        let log = Arc::new(File::open(&old.path)?);
        let mut header = [0; PAGE];
        read_at(&log, 0, &mut header)?;
        if &header[..8] != MAGIC
            || header[8..24] != old.nonce
            || !authentic(&header)
            || u64_at(&header, 24) != old.view.len
        {
            return Err(bad("matrix generation identity changed"));
        }
        let head = Self::latest(&log, old.view.len)?;
        let mut view = old.view.clone();
        view.log = log;
        view.head = head;
        view.working = None;
        Ok(Self {
            view,
            writer: None,
            cursor: 0,
            path: old.path.clone(),
            nonce: old.nonce,
            max_pending: old.max_pending,
        })
    }
    fn append_page(&self, bytes: &[u8; PAGE]) -> io::Result<u64> {
        let file = self
            .writer
            .as_ref()
            .ok_or(io::ErrorKind::PermissionDenied)?;
        let len = file.metadata()?.len();
        let offset = len
            .checked_add(PAGE as u64 - 1)
            .ok_or_else(|| bad("matrix log overflow"))?
            / PAGE as u64
            * PAGE as u64;
        write_at(file, offset, bytes)?;
        Ok(offset)
    }
    fn update_node(&self, root: u64, height: u8, updates: &[(u64, Entry)]) -> io::Result<u64> {
        let mut bytes = if root == 0 {
            [0; PAGE]
        } else {
            *self.view.node(root, height)?
        };
        bytes[..8].copy_from_slice(NODE_MAGIC);
        bytes[8] = height;
        let mut at = 0;
        while at < updates.len() {
            let slot = ((updates[at].0 >> (height * 7)) & 127) as usize;
            let start = at;
            while at < updates.len() && ((updates[at].0 >> (height * 7)) & 127) as usize == slot {
                at += 1;
            }
            let position = 32 + slot * 16;
            let entry = if height == 0 {
                updates[start].1
            } else {
                Entry {
                    offset: self.update_node(
                        u64_at(&bytes, position),
                        height - 1,
                        &updates[start..at],
                    )?,
                    crc: 0,
                }
            };
            bytes[position..position + 8].copy_from_slice(&entry.offset.to_le_bytes());
            bytes[position + 8..position + 12].copy_from_slice(&entry.crc.to_le_bytes());
        }
        seal(&mut bytes);
        self.append_page(&bytes)
    }
    pub(crate) fn publish(&mut self, native_eof: u64) -> io::Result<()> {
        let file = self
            .writer
            .as_ref()
            .ok_or(io::ErrorKind::PermissionDenied)?;
        let working = self.view.working.as_ref().unwrap();
        if working.pages.is_empty() && self.view.head.native_eof == native_eof {
            return Ok(());
        }
        let mut updates = Vec::new();
        updates
            .try_reserve(working.pages.len())
            .map_err(io::Error::other)?;
        for entry in working.pages.iter() {
            let mut value = *entry.value();
            if value.offset > 1 {
                let mut bytes = [0; PAGE];
                read_at(file, value.offset, &mut bytes)?;
                value.crc = checksum(&bytes);
            }
            updates.push((*entry.key(), value));
        }
        publication_fault("data");
        publication_error("data")?;
        let root = if updates.is_empty() {
            self.view.head.root
        } else {
            self.update_node(self.view.head.root, self.view.height - 1, &updates)?
        };
        publication_fault("index");
        publication_error("index")?;
        let end = file.metadata()?.len();
        let head = Head {
            generation: self
                .view
                .head
                .generation
                .checked_add(1)
                .ok_or_else(|| bad("matrix generations exhausted"))?,
            root,
            end,
            native_eof,
            compaction_epoch: self.view.head.compaction_epoch,
        };
        file.sync_all()?;
        publication_fault("data_sync");
        publication_error("data_sync")?;
        Self::write_head(file, head)?;
        // Published pages must never be mutable again, even if this last sync
        // fails. The caller poisons the writer on any publication failure.
        self.view.head = head;
        working.root.store(root, Ordering::Relaxed);
        working.end.store(end, Ordering::Relaxed);
        working.pages.clear();
        file.sync_all()?;
        publication_fault("head_sync");
        publication_error("head_sync")?;
        Ok(())
    }
    pub(crate) fn compact(&mut self) -> crate::Result<()> {
        let working = self
            .view
            .working
            .as_ref()
            .ok_or_else(|| io::Error::from(io::ErrorKind::PermissionDenied))?;
        if !working.pages.is_empty() {
            return Err(crate::Error::InvalidFormatSpec(
                "sync matrix changes before compaction",
            ));
        }
        let mut keys = Vec::new();
        self.view.visit(
            self.view.head.root,
            self.view.height - 1,
            0,
            &mut keys,
            0,
            u64::MAX,
            self.max_pending,
        )?;
        if keys.len() > self.max_pending {
            return Err(crate::Error::LimitExceeded {
                resource: "matrix compaction page directory",
                limit: self.max_pending as u64,
                actual: keys.len() as u64,
            });
        }
        let parent = self.path.parent().unwrap_or(Path::new("."));
        let temp = tempfile::NamedTempFile::new_in(parent)?;
        let mut header = [0; PAGE];
        read_at(&self.view.log, 0, &mut header)?;
        write_at(temp.as_file(), 0, &header)?;
        temp.as_file().set_len(HEADER)?;
        let head = Head {
            generation: self.view.head.generation,
            root: 0,
            end: HEADER,
            native_eof: 0,
            compaction_epoch: self
                .view
                .head
                .compaction_epoch
                .checked_add(1)
                .ok_or_else(|| bad("matrix compaction epochs exhausted"))?,
        };
        let next_working = Arc::new(Working {
            pages: crossbeam_skiplist::SkipMap::new(),
            root: AtomicU64::new(0),
            end: AtomicU64::new(HEADER),
        });
        let view = View {
            base: self.view.base.clone(),
            log: Arc::new(File::open(temp.path())?),
            head,
            len: self.view.len,
            height: self.view.height,
            cache: RefCell::default(),
            cache_bytes: self.view.cache_bytes,
            working: Some(next_working),
        };
        let mut next = Self {
            view,
            writer: Some(temp.as_file().try_clone()?),
            cursor: 0,
            path: self.path.clone(),
            nonce: self.nonce,
            max_pending: self.max_pending,
        };
        for key in keys {
            let mut bytes = [0; PAGE];
            self.view.page(key, &mut bytes)?;
            let offset = if bytes.iter().all(|b| *b == 0) {
                1
            } else {
                next.append_page(&bytes)?
            };
            next.view
                .working
                .as_ref()
                .unwrap()
                .pages
                .insert(key, Entry { offset, crc: 0 });
        }
        next.publish(self.view.head.native_eof)?;
        let outcome =
            crate::file::publish_open_temp_path_atomically(temp.into_temp_path(), &self.path)?;
        *self = next;
        match outcome {
            crate::file::ReplaceDurability::Durable => Ok(()),
            crate::file::ReplaceDurability::ParentSyncPending(source) => {
                Err(crate::Error::PublishedButParentSyncPending {
                    path: self.path.display().to_string(),
                    source: Box::new(source),
                })
            }
        }
    }
    pub(crate) fn zero(&mut self, offset: u64, len: u64) -> io::Result<()> {
        if len == 0 {
            return Ok(());
        }
        let end = offset
            .checked_add(len)
            .filter(|n| *n <= self.view.len)
            .ok_or_else(|| bad("matrix clear outside region"))?;
        let first = offset / PAGE as u64;
        let last = (end - 1) / PAGE as u64;
        let mut pages = Vec::new();
        self.view.visit(
            self.view.root_end().0,
            self.view.height - 1,
            0,
            &mut pages,
            first,
            last,
            self.max_pending,
        )?;
        let working = self
            .view
            .working
            .as_ref()
            .ok_or_else(|| io::Error::from(io::ErrorKind::PermissionDenied))?;
        // crossbeam-skiplist 0.1.3's Range retains the reference to every
        // preceding node when it advances. Use the entry cursor, which releases
        // that reference, while preserving the bounded range walk.
        // Upstream fix: https://github.com/crossbeam-rs/crossbeam/pull/1217.
        // TODO(crossbeam-skiplist upgrade): once a released dependency includes
        // that fix, rerun the range reclamation probe and matrix ASan tests,
        // then replace this workaround with the fixed range(first..=last).
        if let Some(mut entry) = working.pages.lower_bound(std::ops::Bound::Included(&first)) {
            loop {
                let page = *entry.key();
                if page > last {
                    break;
                }
                pages.push(page);
                if !entry.move_next() {
                    break;
                }
            }
        }
        pages.push(first);
        pages.push(last);
        pages.sort_unstable();
        pages.dedup();
        if working
            .pages
            .len()
            .checked_add(
                pages
                    .iter()
                    .filter(|page| !working.pages.contains_key(page))
                    .count(),
            )
            .is_none_or(|n| n > self.max_pending)
        {
            return Err(bad(
                "matrix working page budget exceeded; sync before continuing",
            ));
        }
        // Whole interior pages of clearable matrix regions have an all-zero
        // base image. Descriptor tables and the fixed CRC header are excluded
        // by the matrix layout's clear ranges. Partial boundary pages preserve
        // any adjacent immutable descriptors.
        for page in pages {
            let base = page * PAGE as u64;
            let start = offset.saturating_sub(base) as usize;
            let stop = ((end - base).min(PAGE as u64)) as usize;
            if start == 0 && stop == PAGE {
                working.pages.insert(page, Entry { offset: 1, crc: 0 });
            } else {
                let mut bytes = [0; PAGE];
                self.view.page(page, &mut bytes)?;
                bytes[start..stop].fill(0);
                let at = if let Some(entry) = working
                    .pages
                    .get(&page)
                    .filter(|entry| entry.value().offset > 1)
                {
                    let at = entry.value().offset;
                    write_at(self.writer.as_ref().unwrap(), at, &bytes)?;
                    at
                } else {
                    self.append_page(&bytes)?
                };
                working.pages.insert(page, Entry { offset: at, crc: 0 });
            }
        }
        Ok(())
    }
}
impl Read for MatrixFile {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        let n = bytes
            .len()
            .min(self.view.len.saturating_sub(self.cursor) as usize);
        self.view.read_exact_at(self.cursor, &mut bytes[..n])?;
        self.cursor += n as u64;
        Ok(n)
    }
}
impl Seek for MatrixFile {
    fn seek(&mut self, from: SeekFrom) -> io::Result<u64> {
        let n = match from {
            SeekFrom::Start(n) => n as i128,
            SeekFrom::Current(n) => self.cursor as i128 + n as i128,
            SeekFrom::End(n) => self.view.len as i128 + n as i128,
        };
        self.cursor = u64::try_from(n).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
        Ok(self.cursor)
    }
}
impl Write for MatrixFile {
    fn write(&mut self, mut bytes: &[u8]) -> io::Result<usize> {
        let length = bytes.len();
        if self
            .cursor
            .checked_add(length as u64)
            .is_none_or(|n| n > self.view.len)
        {
            return Err(bad("matrix write outside fixed region"));
        }
        let file = self
            .writer
            .as_ref()
            .ok_or(io::ErrorKind::PermissionDenied)?;
        let working = self.view.working.as_ref().unwrap();
        while !bytes.is_empty() {
            let page = self.cursor / PAGE as u64;
            let within = self.cursor as usize % PAGE;
            let n = bytes.len().min(PAGE - within);
            let current = working.pages.get(&page).map(|e| *e.value());
            if let Some(entry) = current.filter(|e| e.offset > 1) {
                write_at(file, entry.offset + within as u64, &bytes[..n])?;
            } else {
                if current.is_none() && working.pages.len() >= self.max_pending {
                    return Err(bad(
                        "matrix working page budget exceeded; sync before continuing",
                    ));
                }
                let mut buffer = [0; PAGE];
                self.view.page(page, &mut buffer)?;
                buffer[within..within + n].copy_from_slice(&bytes[..n]);
                let offset = self.append_page(&buffer)?;
                working.pages.insert(page, Entry { offset, crc: 0 });
            }
            self.cursor += n as u64;
            bytes = &bytes[n..];
        }
        Ok(length)
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Matrix algorithms operate on either a raw creation/test file or a COW view.
/// No caller receives a write-capable handle from the read-only adapter.
pub(crate) trait MatrixIo: Read + Write + Seek + std::fmt::Debug {
    fn raw(&self) -> &File;
    fn source(&self) -> io::Result<Source>;
    fn paged(&self) -> Option<&View> {
        None
    }
    fn zero_pages(&mut self, _offset: u64, _len: u64) -> io::Result<bool> {
        Ok(false)
    }
    fn set_len(&self, len: u64) -> io::Result<()>;
    fn sync_data(&self) -> io::Result<()>;
}
impl MatrixIo for File {
    fn raw(&self) -> &File {
        self
    }
    fn source(&self) -> io::Result<Source> {
        Ok(Source::Raw(Arc::new(self.try_clone()?)))
    }
    fn set_len(&self, len: u64) -> io::Result<()> {
        File::set_len(self, len)
    }
    fn sync_data(&self) -> io::Result<()> {
        File::sync_data(self)
    }
}
impl MatrixIo for MatrixFile {
    fn raw(&self) -> &File {
        &self.view.base
    }
    fn source(&self) -> io::Result<Source> {
        let mut view = self.view.clone();
        view.cache_bytes = 0;
        Ok(Source::Paged(view))
    }
    fn paged(&self) -> Option<&View> {
        Some(&self.view)
    }
    fn zero_pages(&mut self, offset: u64, len: u64) -> io::Result<bool> {
        self.zero(offset, len)?;
        Ok(true)
    }
    fn set_len(&self, _len: u64) -> io::Result<()> {
        Err(io::ErrorKind::Unsupported.into())
    }
    fn sync_data(&self) -> io::Result<()> {
        self.writer
            .as_ref()
            .ok_or(io::ErrorKind::PermissionDenied)?;
        Ok(())
    }
}
#[derive(Clone, Debug)]
pub(crate) enum Source {
    Raw(Arc<File>),
    Paged(View),
}
impl Source {
    pub(crate) fn raw(&self) -> &File {
        match self {
            Self::Raw(f) => f,
            Self::Paged(v) => &v.base,
        }
    }
    pub(crate) fn paged(&self) -> Option<&View> {
        match self {
            Self::Raw(_) => None,
            Self::Paged(v) => Some(v),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture(len: u64) -> (tempfile::TempDir, PathBuf, File, MatrixFile) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("matrix.varve");
        let base = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&path)
            .unwrap();
        base.set_len(len).unwrap();
        let store = MatrixFile::create(&path, &base, len, [7; 16], 65536).unwrap();
        (dir, path, base, store)
    }
    #[test]
    fn root_diff_skips_unchanged_subtrees_and_tracks_compaction_epochs() {
        let (_dir, _path, _base, mut writer) = fixture(8 * 1024 * 1024);
        for page in 0..1024 {
            writer.seek(SeekFrom::Start(page * PAGE as u64)).unwrap();
            writer.write_all(&[1]).unwrap();
        }
        writer.publish(writer.view.len).unwrap();
        let old = writer.refreshed().unwrap();
        writer.seek(SeekFrom::Start(900 * PAGE as u64)).unwrap();
        writer.write_all(&[2]).unwrap();
        writer.publish(writer.view.len).unwrap();
        let next = old.refreshed().unwrap();
        assert_eq!(old.changed_pages(&next).unwrap(), vec![900]);
        assert_eq!(old.view.cache.borrow().pages.len(), 2);
        assert_eq!(next.view.cache.borrow().pages.len(), 2);
        assert_eq!(old.head().compaction_epoch, next.head().compaction_epoch);
        writer.compact().unwrap();
        writer.seek(SeekFrom::Start(0)).unwrap();
        writer.write_all(&[3]).unwrap();
        writer.publish(writer.view.len).unwrap();
        let compacted = old.refreshed().unwrap();
        assert_eq!(
            compacted.head().compaction_epoch,
            old.head().compaction_epoch + 1
        );
        assert!(old.changed_pages(&compacted).is_err());
    }
    #[test]
    fn seeded_page_writes_match_a_byte_model_across_sync_reopen_and_compaction() {
        let (_dir, path, base, mut store) = fixture(2 * 1024 * 1024 + 37);
        let mut model = vec![0u8; store.view.len as usize];
        let mut state = 0x923aec76u64;
        for batch in 0..25 {
            let old = store.refreshed().unwrap();
            let before = model.clone();
            for _ in 0..60 {
                state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
                let at = (state as usize) % (model.len() - 6000);
                let bytes = vec![(state >> 32) as u8; 6000];
                store.seek(SeekFrom::Start(at as u64)).unwrap();
                store.write_all(&bytes).unwrap();
                model[at..at + 6000].copy_from_slice(&bytes);
            }
            let mut bytes = vec![0; model.len()];
            old.view.read_exact_at(0, &mut bytes).unwrap();
            assert_eq!(bytes, before);
            store.publish(store.view.len).unwrap();
            let mut current = MatrixFile::open(&path, &base, [7; 16], false, 65536).unwrap();
            current.set_cache_bytes(if batch % 2 == 0 { 0 } else { 8192 });
            current.view.read_exact_at(0, &mut bytes).unwrap();
            assert_eq!(bytes, model);
            assert!(current.view.cache.borrow().pages.len() * PAGE <= current.view.cache_bytes);
            old.view.read_exact_at(0, &mut bytes).unwrap();
            assert_eq!(bytes, before);
            if batch % 5 == 0 {
                store.compact().unwrap();
                current.view.read_exact_at(0, &mut bytes).unwrap();
                assert_eq!(bytes, model);
            }
        }
    }
    #[test]
    fn cold_reads_cross_radix_boundaries_in_a_large_sparse_region() {
        let len = 32 * 1024 * 1024 * 1024u64 + 37;
        let (_dir, _path, _base, mut store) = fixture(len);
        let old = store.refreshed().unwrap();
        let positions = [
            4093,
            128 * 4096 - 3,
            128 * 128 * 4096 - 3,
            128 * 128 * 128 * 4096 - 3,
            len - 17,
        ];
        for (i, at) in positions.iter().enumerate() {
            store.seek(SeekFrom::Start(*at)).unwrap();
            store.write_all(&[i as u8 + 1; 17]).unwrap();
        }
        store.publish(len).unwrap();
        let new = store.refreshed().unwrap();
        for (i, at) in positions.iter().enumerate() {
            let mut bytes = [0; 17];
            old.view.read_exact_at(*at, &mut bytes).unwrap();
            assert_eq!(bytes, [0; 17]);
            new.view.read_exact_at(*at, &mut bytes).unwrap();
            assert_eq!(bytes, [i as u8 + 1; 17]);
        }
    }
    #[test]
    fn damaged_data_is_refused_and_a_torn_head_falls_back_to_the_previous_root() {
        let (_dir, _path, _base, mut store) = fixture(32768);
        store.seek(SeekFrom::Start(8190)).unwrap();
        store.write_all(&[1; 17]).unwrap();
        store.publish(32768).unwrap();
        store.seek(SeekFrom::Start(8190)).unwrap();
        store.write_all(&[2; 17]).unwrap();
        store.publish(32768).unwrap();
        let head = store.head();
        let log = store.writer.as_ref().unwrap();
        let at = PAGE as u64 * (1 + head.generation % 2);
        write_at(log, at, b"tornhead").unwrap();
        let previous = store.refreshed().unwrap();
        assert_eq!(previous.head().generation, 1);
        let mut bytes = [0; 17];
        previous.view.read_exact_at(8190, &mut bytes).unwrap();
        assert_eq!(bytes, [1; 17]);
        let entry = previous.view.entry(8190 / PAGE as u64).unwrap();
        write_at(log, entry.offset, b"X").unwrap();
        let fresh = store.refreshed().unwrap();
        assert_eq!(
            fresh
                .view
                .read_exact_at(8190, &mut bytes)
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidData
        );
        write_at(
            log,
            PAGE as u64 * (1 + previous.head().generation % 2),
            b"tornhead",
        )
        .unwrap();
        assert_eq!(
            store.refreshed().unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }
    #[test]
    fn unpublished_pages_coalesce_and_zeroing_preserves_adjacent_bytes() {
        let (_dir, _path, _base, mut store) = fixture(16384);
        store.write_all(&[3; 8192]).unwrap();
        let size = store.writer.as_ref().unwrap().metadata().unwrap().len();
        for i in 0..100 {
            store.seek(SeekFrom::Start(4095)).unwrap();
            store.write_all(&[i; 2]).unwrap();
        }
        assert_eq!(
            store.writer.as_ref().unwrap().metadata().unwrap().len(),
            size
        );
        store.publish(16384).unwrap();
        let old = store.refreshed().unwrap();
        store.zero(4094, 4).unwrap();
        store.publish(16384).unwrap();
        let mut bytes = [0; 6];
        store.view.read_exact_at(4093, &mut bytes).unwrap();
        assert_eq!(bytes, [3, 0, 0, 0, 0, 3]);
        old.view.read_exact_at(4093, &mut bytes).unwrap();
        assert_eq!(bytes, [3, 3, 99, 99, 3, 3]);
    }
}
