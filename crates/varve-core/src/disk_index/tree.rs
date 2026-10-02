//! Immutable, append-only index pages. Each handle owns its bounded cache.
//! Only the sole writer appends pages; published pages are never reused.
use super::{DiskIndexError, DiskIndexResult, LATEST_LEN};
use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::fs::File;
use std::sync::Arc;

pub(super) const PAGE: usize = 4096;
const HEADER: usize = 32;
const LEAF_ENTRY: usize = 28 + LATEST_LEN;
const BRANCH_ENTRY: usize = 28 + 16;
const MAGIC: &[u8; 8] = b"VIXPAGE2";
fn entry_size(height: u8) -> usize {
    if height == 0 {
        LEAF_ENTRY
    } else {
        BRANCH_ENTRY
    }
}
fn fanout(height: u8) -> usize {
    (PAGE - HEADER) / entry_size(height)
}

fn corrupt(message: &'static str) -> DiskIndexError {
    DiskIndexError::Storage(message.into())
}

pub(super) fn read_at(file: &File, mut offset: u64, mut bytes: &mut [u8]) -> std::io::Result<()> {
    while !bytes.is_empty() {
        #[cfg(unix)]
        let result = std::os::unix::fs::FileExt::read_at(file, bytes, offset);
        #[cfg(windows)]
        let result = std::os::windows::fs::FileExt::seek_read(file, bytes, offset);
        #[cfg(not(any(unix, windows)))]
        let result: std::io::Result<usize> = Err(std::io::ErrorKind::Unsupported.into());
        let n = match result {
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            other => other?,
        };
        if n == 0 {
            return Err(std::io::ErrorKind::UnexpectedEof.into());
        }
        offset = offset
            .checked_add(n as u64)
            .ok_or(std::io::ErrorKind::InvalidInput)?;
        bytes = &mut bytes[n..];
    }
    Ok(())
}

pub(super) fn write_at(file: &File, mut offset: u64, mut bytes: &[u8]) -> std::io::Result<()> {
    while !bytes.is_empty() {
        #[cfg(unix)]
        let result = std::os::unix::fs::FileExt::write_at(file, bytes, offset);
        #[cfg(windows)]
        let result = std::os::windows::fs::FileExt::seek_write(file, bytes, offset);
        #[cfg(not(any(unix, windows)))]
        let result: std::io::Result<usize> = Err(std::io::ErrorKind::Unsupported.into());
        let n = match result {
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            other => other?,
        };
        if n == 0 {
            return Err(std::io::ErrorKind::WriteZero.into());
        }
        offset = offset
            .checked_add(n as u64)
            .ok_or(std::io::ErrorKind::InvalidInput)?;
        bytes = &bytes[n..];
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct Root {
    pub offset: u64,
    pub count: u64,
}

#[derive(Clone, Debug)]
struct Key {
    offset: u64,
    len: usize,
    prefix: [u8; 16],
    decoded: Option<(Arc<Vec<u8>>, usize)>,
}
impl PartialEq for Key {
    fn eq(&self, other: &Self) -> bool {
        self.offset == other.offset && self.len == other.len && self.prefix == other.prefix
    }
}
impl Eq for Key {}
impl Key {
    fn cached(&self) -> Option<&[u8]> {
        if self.len <= 16 {
            Some(&self.prefix[..self.len])
        } else {
            self.decoded
                .as_ref()
                .map(|(bytes, at)| &bytes[*at..*at + self.len])
        }
    }
}

#[derive(Clone, Debug)]
struct Entry {
    key: Key,
    value: [u8; LATEST_LEN],
}

impl Entry {
    fn child(&self) -> Root {
        Root {
            offset: u64::from_le_bytes(self.value[..8].try_into().unwrap()),
            count: u64::from_le_bytes(self.value[8..16].try_into().unwrap()),
        }
    }
    fn branch(key: Key, root: Root) -> Self {
        let mut value = [0; LATEST_LEN];
        value[..8].copy_from_slice(&root.offset.to_le_bytes());
        value[8..16].copy_from_slice(&root.count.to_le_bytes());
        Self { key, value }
    }
}

#[derive(Debug)]
struct Node {
    height: u8,
    count: u64,
    entries: Vec<Entry>,
}

#[derive(Clone, Copy)]
enum Cached {
    Page(u64),
    Key(u64),
}

#[derive(Default)]
struct Cache {
    pages: HashMap<u64, Arc<Node>>,
    keys: HashMap<u64, Arc<Vec<u8>>>,
    order: VecDeque<Cached>,
    bytes: usize,
}

/// Send, not Sync: a reader's cache is never shared between reading threads.
pub(super) struct Reader {
    file: Arc<File>,
    end: u64,
    max_key: usize,
    budget: usize,
    cache: RefCell<Cache>,
    #[cfg(test)]
    page_reads: std::cell::Cell<usize>,
    #[cfg(test)]
    key_reads: std::cell::Cell<usize>,
}

impl Reader {
    pub fn new(file: Arc<File>, end: u64, max_key: usize, budget: usize) -> Self {
        Self {
            file,
            end,
            max_key,
            budget,
            cache: RefCell::new(Cache::default()),
            #[cfg(test)]
            page_reads: std::cell::Cell::new(0),
            #[cfg(test)]
            key_reads: std::cell::Cell::new(0),
        }
    }
    fn node(&self, root: Root) -> DiskIndexResult<Arc<Node>> {
        if root.offset < PAGE as u64
            || root
                .offset
                .checked_add(PAGE as u64)
                .is_none_or(|e| e > self.end)
        {
            return Err(corrupt("index page outside checkpoint extent"));
        }
        if let Some(node) = self.cache.borrow().pages.get(&root.offset) {
            if node.count != root.count {
                return Err(corrupt("index subtree count mismatch"));
            }
            return Ok(node.clone());
        }
        let mut bytes = [0; PAGE];
        read_at(&self.file, root.offset, &mut bytes)?;
        #[cfg(test)]
        self.page_reads.set(self.page_reads.get() + 1);
        if &bytes[..8] != MAGIC || bytes[9] != 0 || bytes[20..HEADER] != [0; 12] {
            return Err(corrupt("invalid index page header"));
        }
        let checksum = u32::from_le_bytes(bytes[16..20].try_into().unwrap());
        bytes[16..20].fill(0);
        if crc32fast::hash(&bytes) != checksum {
            return Err(corrupt("index page checksum mismatch"));
        }
        let height = bytes[8];
        let len = u16::from_le_bytes(bytes[10..12].try_into().unwrap()) as usize;
        if height > 31 || len == 0 || len > fanout(height) || bytes[12..16] != [0; 4] {
            return Err(corrupt("invalid index page shape"));
        }
        if bytes[HEADER + len * entry_size(height)..]
            .iter()
            .any(|x| *x != 0)
        {
            return Err(corrupt("nonzero index page padding"));
        }
        let mut entries = Vec::with_capacity(len);
        let mut count = 0u64;
        for item in
            bytes[HEADER..HEADER + len * entry_size(height)].chunks_exact(entry_size(height))
        {
            let key = Key {
                offset: u64::from_le_bytes(item[..8].try_into().unwrap()),
                len: u32::from_le_bytes(item[8..12].try_into().unwrap()) as usize,
                prefix: item[12..28].try_into().unwrap(),
                decoded: None,
            };
            if key.len == 0 || key.len > self.max_key {
                return Err(corrupt("invalid index key length"));
            }
            if key.len <= 16 {
                if key.offset != 0 || key.prefix[key.len..].iter().any(|x| *x != 0) {
                    return Err(corrupt("invalid inline index key"));
                }
            } else if key.offset < PAGE as u64
                || key
                    .offset
                    .checked_add(key.len as u64 + 4)
                    .is_none_or(|e| e > root.offset)
            {
                return Err(corrupt("invalid index key extent"));
            }
            let mut value = [0; LATEST_LEN];
            value[..item.len() - 28].copy_from_slice(&item[28..]);
            let entry = Entry { key, value };
            let increment = if height == 0 {
                1
            } else {
                let child = entry.child();
                if child.count == 0
                    || child.offset < PAGE as u64
                    || child
                        .offset
                        .checked_add(PAGE as u64)
                        .is_none_or(|e| e > root.offset)
                    || entry.value[16..].iter().any(|x| *x != 0)
                {
                    return Err(corrupt("invalid index child"));
                }
                child.count
            };
            count = count
                .checked_add(increment)
                .ok_or_else(|| corrupt("index count overflow"))?;
            entries.push(entry);
        }
        if count != root.count {
            return Err(corrupt("index subtree count mismatch"));
        }
        // Validate order from inline prefixes whenever possible. Only equal
        // long prefixes require external bytes. Cache those ambiguous keys in
        // one packed allocation; distinct-prefix keys remain demand-loaded.
        let mut needed = [false; (PAGE - HEADER) / BRANCH_ENTRY];
        for (i, pair) in entries.windows(2).enumerate() {
            let left = &pair[0].key;
            let right = &pair[1].key;
            let order = left.prefix[..left.len.min(16)].cmp(&right.prefix[..right.len.min(16)]);
            if order.is_eq() && left.len > 16 && right.len > 16 {
                needed[i] = true;
                needed[i + 1] = true;
            } else if !order.then(left.len.cmp(&right.len)).is_lt() {
                return Err(corrupt("index keys are not strictly sorted"));
            }
        }
        let key_bytes = entries
            .iter()
            .zip(needed)
            .filter(|(_, needed)| *needed)
            .try_fold(0usize, |n, (e, _)| n.checked_add(e.key.len + 4));
        if let Some(len) = key_bytes.filter(|len| *len <= self.budget / 2)
            && len != 0
        {
            let mut bytes = vec![0; len];
            let mut at = 0;
            let mut run_offset = 0u64;
            let mut run_start = 0usize;
            // Combine exactly adjacent extents, never pull unrelated key bytes
            // or gaps into memory merely to reduce system calls.
            for (entry, needed) in entries.iter().zip(needed) {
                if !needed {
                    continue;
                }
                let key = &entry.key;
                if at > run_start && run_offset + (at - run_start) as u64 != key.offset {
                    self.read_key_bytes(run_offset, &mut bytes[run_start..at])?;
                    run_start = at;
                }
                if at == run_start {
                    run_offset = key.offset;
                }
                at += key.len + 4;
            }
            if at > run_start {
                self.read_key_bytes(run_offset, &mut bytes[run_start..at])?;
            }
            let mut at = 0;
            for (entry, needed) in entries.iter().zip(needed) {
                if !needed {
                    continue;
                }
                let key = &entry.key;
                let checksum =
                    u32::from_le_bytes(bytes[at + key.len..at + key.len + 4].try_into().unwrap());
                if crc32fast::hash(&bytes[at..at + key.len]) != checksum
                    || bytes[at..at + 16] != key.prefix
                {
                    return Err(corrupt("index key checksum mismatch"));
                }
                at += key.len + 4;
            }
            let bytes = Arc::new(bytes);
            let mut at = 0;
            for (entry, needed) in entries.iter_mut().zip(needed) {
                if !needed {
                    continue;
                }
                entry.key.decoded = Some((bytes.clone(), at));
                at += entry.key.len + 4;
            }
        }
        for (i, pair) in entries.windows(2).enumerate() {
            if !needed[i] || !needed[i + 1] {
                continue;
            }
            let left = &pair[0].key;
            let right = &pair[1].key;
            if left.prefix != right.prefix {
                continue;
            }
            let ordered = if let Some(key) = right.cached() {
                self.compare(left, key)?.is_lt()
            } else {
                self.compare(left, self.long_key(right)?.as_slice())?
                    .is_lt()
            };
            if !ordered {
                return Err(corrupt("index keys are not strictly sorted"));
            }
        }
        let node = Arc::new(Node {
            height,
            count,
            entries,
        });
        let charge = Self::charge(&node);
        let mut cache = self.cache.borrow_mut();
        if Self::reserve(&mut cache, charge, self.budget) {
            cache.order.push_back(Cached::Page(root.offset));
            cache.pages.insert(root.offset, node.clone());
        }
        Ok(node)
    }
    fn charge(node: &Node) -> usize {
        256 + node.entries.capacity() * std::mem::size_of::<Entry>()
            + node
                .entries
                .iter()
                .find_map(|e| e.key.decoded.as_ref().map(|(data, _)| data.capacity() + 64))
                .unwrap_or(0)
    }
    fn reserve(cache: &mut Cache, charge: usize, budget: usize) -> bool {
        if charge > budget {
            return false;
        }
        while cache.bytes.saturating_add(charge) > budget {
            match cache
                .order
                .pop_front()
                .expect("cache accounting has an owner")
            {
                Cached::Page(offset) => {
                    cache.bytes -= Self::charge(&cache.pages.remove(&offset).unwrap());
                }
                Cached::Key(offset) => {
                    cache.bytes -= cache.keys.remove(&offset).unwrap().capacity() + 128;
                }
            }
        }
        cache.bytes += charge;
        true
    }
    fn read_key_bytes(&self, offset: u64, bytes: &mut [u8]) -> DiskIndexResult<()> {
        read_at(&self.file, offset, bytes)?;
        #[cfg(test)]
        self.key_reads.set(self.key_reads.get() + 1);
        Ok(())
    }
    fn long_key(&self, key: &Key) -> DiskIndexResult<Arc<Vec<u8>>> {
        if let Some(bytes) = self.cache.borrow().keys.get(&key.offset) {
            if bytes.len() != key.len || bytes[..16] != key.prefix {
                return Err(corrupt("inconsistent cached index key"));
            }
            return Ok(bytes.clone());
        }
        let mut bytes = vec![0; key.len + 4];
        self.read_key_bytes(key.offset, &mut bytes)?;
        let checksum = u32::from_le_bytes(bytes[key.len..].try_into().unwrap());
        if crc32fast::hash(&bytes[..key.len]) != checksum || bytes[..16] != key.prefix {
            return Err(corrupt("index key checksum mismatch"));
        }
        bytes.truncate(key.len);
        let bytes = Arc::new(bytes);
        let mut cache = self.cache.borrow_mut();
        if Self::reserve(&mut cache, bytes.capacity() + 128, self.budget) {
            cache.order.push_back(Cached::Key(key.offset));
            cache.keys.insert(key.offset, bytes.clone());
        }
        Ok(bytes)
    }
    fn key(&self, key: &Key) -> DiskIndexResult<Vec<u8>> {
        if let Some(bytes) = key.cached() {
            Ok(bytes.to_vec())
        } else {
            Ok(self.long_key(key)?.as_ref().clone())
        }
    }
    fn compare(&self, stored: &Key, key: &[u8]) -> DiskIndexResult<std::cmp::Ordering> {
        if let Some(bytes) = stored.cached() {
            return Ok(bytes.cmp(key));
        }
        let prefix = &stored.prefix[..stored.len.min(16)];
        let comparison = prefix.cmp(&key[..key.len().min(16)]);
        if comparison != std::cmp::Ordering::Equal || stored.len <= 16 || key.len() <= 16 {
            return Ok(comparison.then(stored.len.cmp(&key.len())));
        }
        Ok(self.long_key(stored)?.as_slice().cmp(key))
    }
    fn search(&self, entries: &[Entry], key: &[u8]) -> DiskIndexResult<Result<usize, usize>> {
        let (mut low, mut high) = (0, entries.len());
        while low < high {
            let mid = low + (high - low) / 2;
            match self.compare(&entries[mid].key, key)? {
                std::cmp::Ordering::Less => low = mid + 1,
                std::cmp::Ordering::Greater => high = mid,
                std::cmp::Ordering::Equal => return Ok(Ok(mid)),
            }
        }
        Ok(Err(low))
    }
    pub fn get(&self, mut root: Root, key: &[u8]) -> DiskIndexResult<Option<[u8; LATEST_LEN]>> {
        if root.offset == 0 {
            if root.count != 0 {
                return Err(corrupt("invalid empty index"));
            }
            return Ok(None);
        }
        let mut expected_height = None;
        let mut minimum: Option<Key> = None;
        loop {
            let node = self.node(root)?;
            if expected_height.is_some_and(|h| h != node.height) {
                return Err(corrupt("index height mismatch"));
            }
            if minimum
                .as_ref()
                .is_some_and(|key| *key != node.entries[0].key)
            {
                return Err(corrupt("index separator mismatch"));
            }
            let position = self.search(&node.entries, key)?;
            if node.height == 0 {
                return Ok(position.ok().map(|i| node.entries[i].value));
            }
            let at = match position {
                Ok(i) => i,
                Err(0) => return Ok(None),
                Err(i) => i - 1,
            };
            expected_height = Some(node.height - 1);
            minimum = Some(node.entries[at].key.clone());
            root = node.entries[at].child();
        }
    }
    pub fn visit(
        &self,
        root: Root,
        visitor: &mut impl FnMut(Vec<u8>, [u8; LATEST_LEN]) -> DiskIndexResult<()>,
    ) -> DiskIndexResult<()> {
        if root.offset == 0 {
            return Ok(());
        }
        self.visit_at(root, None, visitor)
    }
    fn visit_at(
        &self,
        root: Root,
        height: Option<u8>,
        visitor: &mut impl FnMut(Vec<u8>, [u8; LATEST_LEN]) -> DiskIndexResult<()>,
    ) -> DiskIndexResult<()> {
        let node = self.node(root)?;
        if height.is_some_and(|h| h != node.height) {
            return Err(corrupt("index height mismatch"));
        }
        for entry in &node.entries {
            if node.height == 0 {
                visitor(self.key(&entry.key)?, entry.value)?;
            } else {
                self.visit_at(entry.child(), Some(node.height - 1), visitor)?;
            }
        }
        Ok(())
    }
}

/// Rewrites each affected node once per batch, retaining unchanged subtrees.
pub(super) fn apply(
    file: &File,
    reader: &Reader,
    root: Root,
    updates: &BTreeMap<Vec<u8>, [u8; LATEST_LEN]>,
) -> DiskIndexResult<Root> {
    if updates.is_empty() {
        return Ok(root);
    }
    let mut writer = Writer {
        file,
        reader,
        end: file.metadata()?.len().max(PAGE as u64),
    };
    let updates: Vec<_> = updates.iter().collect();
    let height = if root.offset == 0 {
        0
    } else {
        reader.node(root)?.height
    };
    let mut branches = writer.update(root, height, &updates)?;
    let mut level = height;
    while branches.len() > 1 {
        level = level
            .checked_add(1)
            .filter(|h| *h <= 31)
            .ok_or_else(|| corrupt("index height exhausted"))?;
        branches = writer.nodes(level, branches)?;
    }
    Ok(branches[0].child())
}

struct Writer<'a> {
    file: &'a File,
    reader: &'a Reader,
    end: u64,
}

impl Writer<'_> {
    fn append(&mut self, bytes: &[u8]) -> DiskIndexResult<u64> {
        let start = self.end;
        self.end = start
            .checked_add(bytes.len() as u64)
            .ok_or_else(|| corrupt("index file size overflow"))?;
        write_at(self.file, start, bytes)?;
        Ok(start)
    }
    fn key(&mut self, bytes: &[u8]) -> DiskIndexResult<Key> {
        if bytes.is_empty() || bytes.len() > self.reader.max_key || bytes.len() > u32::MAX as usize
        {
            return Err(corrupt("invalid index update key"));
        }
        let mut prefix = [0; 16];
        prefix[..bytes.len().min(16)].copy_from_slice(&bytes[..bytes.len().min(16)]);
        let offset = if bytes.len() > 16 {
            let offset = self.append(bytes)?;
            self.append(&crc32fast::hash(bytes).to_le_bytes())?;
            offset
        } else {
            0
        };
        Ok(Key {
            offset,
            len: bytes.len(),
            prefix,
            decoded: None,
        })
    }
    fn nodes(&mut self, height: u8, entries: Vec<Entry>) -> DiskIndexResult<Vec<Entry>> {
        let mut output = Vec::with_capacity(entries.len().div_ceil(fanout(height)));
        for chunk in entries.chunks(fanout(height)) {
            let mut bytes = [0; PAGE];
            bytes[..8].copy_from_slice(MAGIC);
            bytes[8] = height;
            bytes[10..12].copy_from_slice(&(chunk.len() as u16).to_le_bytes());
            let mut count = 0u64;
            for (entry, into) in chunk
                .iter()
                .zip(bytes[HEADER..].chunks_exact_mut(entry_size(height)))
            {
                into[..8].copy_from_slice(&entry.key.offset.to_le_bytes());
                into[8..12].copy_from_slice(&(entry.key.len as u32).to_le_bytes());
                into[12..28].copy_from_slice(&entry.key.prefix);
                let value_len = into.len() - 28;
                into[28..].copy_from_slice(&entry.value[..value_len]);
                count = count
                    .checked_add(if height == 0 { 1 } else { entry.child().count })
                    .ok_or_else(|| corrupt("index count overflow"))?;
            }
            let checksum = crc32fast::hash(&bytes);
            bytes[16..20].copy_from_slice(&checksum.to_le_bytes());
            let offset = self.append(&bytes)?;
            output.push(Entry::branch(chunk[0].key.clone(), Root { offset, count }));
        }
        Ok(output)
    }
    fn update(
        &mut self,
        root: Root,
        height: u8,
        updates: &[(&Vec<u8>, &[u8; LATEST_LEN])],
    ) -> DiskIndexResult<Vec<Entry>> {
        let node = if root.offset == 0 {
            None
        } else {
            Some(self.reader.node(root)?)
        };
        if node.as_ref().is_some_and(|n| n.height != height) {
            return Err(corrupt("index height mismatch"));
        }
        let entries = node.as_ref().map_or(&[][..], |n| n.entries.as_slice());
        let mut output = Vec::new();
        if height == 0 {
            let mut old = 0;
            for (key, value) in updates {
                while old < entries.len() && self.reader.compare(&entries[old].key, key)?.is_lt() {
                    output.push(entries[old].clone());
                    old += 1;
                }
                let stored_key = if old < entries.len()
                    && self.reader.compare(&entries[old].key, key)?.is_eq()
                {
                    let k = entries[old].key.clone();
                    old += 1;
                    k
                } else {
                    self.key(key)?
                };
                output.push(Entry {
                    key: stored_key,
                    value: **value,
                });
            }
            output.extend_from_slice(&entries[old..]);
        } else {
            let mut consumed = 0;
            for (i, entry) in entries.iter().enumerate() {
                let begin = consumed;
                while consumed < updates.len()
                    && (i + 1 == entries.len()
                        || self
                            .reader
                            .compare(&entries[i + 1].key, updates[consumed].0)?
                            .is_gt())
                {
                    consumed += 1;
                }
                if begin == consumed {
                    output.push(entry.clone());
                } else {
                    output.extend(self.update(
                        entry.child(),
                        height - 1,
                        &updates[begin..consumed],
                    )?);
                }
            }
            if consumed != updates.len() {
                return Err(corrupt("unapplied index updates"));
            }
        }
        self.nodes(height, output)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::OpenOptions;

    fn fixture() -> (tempfile::TempDir, Arc<File>) {
        let dir = tempfile::tempdir().unwrap();
        let file = Arc::new(
            OpenOptions::new()
                .create_new(true)
                .read(true)
                .write(true)
                .open(dir.path().join("tree"))
                .unwrap(),
        );
        file.set_len(PAGE as u64).unwrap();
        (dir, file)
    }
    fn reader(file: &Arc<File>, budget: usize) -> Reader {
        Reader::new(file.clone(), file.metadata().unwrap().len(), 4096, budget)
    }
    fn value(id: u64) -> [u8; LATEST_LEN] {
        let mut value = [0; LATEST_LEN];
        value[..8].copy_from_slice(&id.to_le_bytes());
        value
    }
    fn repair_crc(bytes: &mut [u8; PAGE]) {
        bytes[16..20].fill(0);
        let crc = crc32fast::hash(bytes);
        bytes[16..20].copy_from_slice(&crc.to_le_bytes());
    }
    fn assert_cache_accounting(reader: &Reader) {
        let cache = reader.cache.borrow();
        let actual = cache
            .pages
            .values()
            .map(|n| Reader::charge(n))
            .sum::<usize>()
            + cache
                .keys
                .values()
                .map(|k| k.capacity() + 128)
                .sum::<usize>();
        assert_eq!(cache.bytes, actual);
        assert!(actual <= reader.budget);
        assert_eq!(cache.order.len(), cache.pages.len() + cache.keys.len());
    }

    #[test]
    fn distinct_long_keys_are_loaded_on_demand_and_crc_checked() {
        fn assert_send<T: Send>() {}
        assert_send::<Reader>();
        let (_dir, file) = fixture();
        let updates: BTreeMap<_, _> = (0..40u64)
            .map(|id| (id.to_be_bytes().repeat(16), value(id)))
            .collect();
        let root = apply(
            &file,
            &reader(&file, 1024 * 1024),
            Root::default(),
            &updates,
        )
        .unwrap();
        let read = reader(&file, 1024 * 1024);
        let node = read.node(root).unwrap();
        assert_eq!(
            read.key_reads.get(),
            0,
            "opening a page must not fetch unrelated long keys"
        );
        let query = 20u64.to_be_bytes().repeat(16);
        assert_eq!(read.get(root, &query).unwrap(), Some(value(20)));
        assert_eq!(read.key_reads.get(), 1);
        for _ in 0..100 {
            assert_eq!(read.get(root, &query).unwrap(), Some(value(20)));
        }
        assert_eq!(
            read.key_reads.get(),
            1,
            "repeated lookup should reuse the verified key"
        );
        assert_eq!(read.page_reads.get(), 1);
        assert_cache_accounting(&read);
        write_at(&file, node.entries[5].key.offset + 20, &[0xff]).unwrap();
        let read = reader(&file, 1024 * 1024);
        assert_eq!(read.get(root, &query).unwrap(), Some(value(20)));
        assert!(read.get(root, &5u64.to_be_bytes().repeat(16)).is_err());
    }

    #[test]
    fn ambiguous_long_prefixes_still_validate_full_order() {
        let (_dir, file) = fixture();
        let updates: BTreeMap<_, _> = (0..40u64)
            .map(|id| {
                let mut key = vec![b'x'; 128];
                key.extend_from_slice(&id.to_be_bytes());
                (key, value(id))
            })
            .collect();
        let root = apply(
            &file,
            &reader(&file, 1024 * 1024),
            Root::default(),
            &updates,
        )
        .unwrap();
        let read = reader(&file, 1024 * 1024);
        read.node(root).unwrap();
        assert_eq!(
            read.key_reads.get(),
            1,
            "adjacent ambiguous keys share one positional read"
        );
        for (key, value) in &updates {
            assert_eq!(read.get(root, key).unwrap(), Some(*value));
        }
        assert_eq!(
            read.key_reads.get(),
            1,
            "adjacent ambiguous keys share one positional read"
        );
        let mut bytes = [0; PAGE];
        read_at(&file, root.offset, &mut bytes).unwrap();
        for i in 0..LEAF_ENTRY {
            bytes.swap(HEADER + i, HEADER + LEAF_ENTRY + i);
        }
        repair_crc(&mut bytes);
        write_at(&file, root.offset, &bytes).unwrap();
        assert!(
            reader(&file, 1024 * 1024).node(root).is_err(),
            "even a repaired page CRC must not hide unsorted keys"
        );
    }

    #[test]
    fn compact_branches_preserve_split_boundaries_and_old_roots() {
        let (_dir, file) = fixture();
        // One more than a full root's leaf capacity forces a second branch level.
        let count = fanout(0) * fanout(1) + 1;
        let initial: BTreeMap<_, _> = (0..count as u64)
            .map(|id| ((id * 2 + 2).to_be_bytes().to_vec(), value(id)))
            .collect();
        let old = apply(
            &file,
            &reader(&file, 1024 * 1024),
            Root::default(),
            &initial,
        )
        .unwrap();
        let pinned = reader(&file, 1024 * 1024);
        assert_eq!(pinned.node(old).unwrap().height, 2);
        let mut changes: BTreeMap<_, _> = initial
            .keys()
            .step_by(49)
            .map(|key| (key.clone(), value(u64::MAX)))
            .collect();
        changes.insert(0u64.to_be_bytes().to_vec(), value(7)); // new global minimum
        let root = apply(&file, &pinned, old, &changes).unwrap();
        let read = reader(&file, 1024 * 1024);
        for (key, before) in &initial {
            assert_eq!(pinned.get(old, key).unwrap(), Some(*before));
            assert_eq!(
                read.get(root, key).unwrap(),
                Some(*changes.get(key).unwrap_or(before))
            );
        }
        for id in 0..=count as u64 {
            assert_eq!(read.get(root, &(id * 2 + 1).to_be_bytes()).unwrap(), None);
        }
        assert_eq!(read.get(root, &0u64.to_be_bytes()).unwrap(), Some(value(7)));
        assert_eq!(pinned.get(old, &0u64.to_be_bytes()).unwrap(), None);
        let mut bytes = [0; PAGE];
        read_at(&file, root.offset, &mut bytes).unwrap();
        bytes[HEADER + 28..HEADER + 36].copy_from_slice(&root.offset.to_le_bytes());
        repair_crc(&mut bytes);
        write_at(&file, root.offset, &bytes).unwrap();
        assert!(
            reader(&file, 1024 * 1024)
                .get(root, &0u64.to_be_bytes())
                .is_err()
        );
    }

    #[test]
    fn mixed_page_and_key_eviction_stays_bounded() {
        let (_dir, file) = fixture();
        let updates: BTreeMap<_, _> = (0..500u64)
            .map(|id| (id.to_be_bytes().repeat(16), value(id)))
            .collect();
        let root = apply(
            &file,
            &reader(&file, 1024 * 1024),
            Root::default(),
            &updates,
        )
        .unwrap();
        for budget in [1024, 8192, 16384] {
            let read = reader(&file, budget);
            for _ in 0..2 {
                for id in (0..500u64).map(|n| n * 137 % 500) {
                    assert_eq!(
                        read.get(root, &id.to_be_bytes().repeat(16)).unwrap(),
                        Some(value(id))
                    );
                    assert_cache_accounting(&read);
                }
            }
        }
    }

    #[test]
    fn previous_page_format_is_rejected() {
        let (_dir, file) = fixture();
        let updates = BTreeMap::from([(b"key".to_vec(), value(1))]);
        let root = apply(
            &file,
            &reader(&file, 1024 * 1024),
            Root::default(),
            &updates,
        )
        .unwrap();
        let mut bytes = [0; PAGE];
        read_at(&file, root.offset, &mut bytes).unwrap();
        bytes[..8].copy_from_slice(b"VIXPAGE1");
        repair_crc(&mut bytes);
        write_at(&file, root.offset, &bytes).unwrap();
        assert!(reader(&file, 1024 * 1024).get(root, b"key").is_err());
    }
}
