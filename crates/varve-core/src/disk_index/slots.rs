//! Direct finite-domain tables. Array position is the enum code; no stored keys
//! or comparison tree. Immutable extents are published only at explicit sync.
use super::*;
use std::sync::Arc;
use std::{cell::RefCell, collections::VecDeque, fs::File};
use tree::{read_at, write_at};
const MAGIC: &[u8; 8] = b"VIXSLOT1";
const WIDTH: usize = 64;
// Offset, sequence, extent and verification flags/CRC. Block IDs come from the
// schema directory, enum codes from array positions; neither is stored per row.
const ROW: usize = 40;
fn pack(value: Option<[u8; LATEST_LEN]>) -> [u8; ROW] {
    let mut row = [0; ROW];
    if let Some(v) = value {
        row[..6].copy_from_slice(&v[..6]);
        row[6..30].copy_from_slice(&v[8..32]);
        row[30..34].copy_from_slice(&v[40..44]);
        row[34..38].copy_from_slice(&v[48..52]);
    }
    row
}
fn unpack(row: &[u8], block: u32) -> DiskIndexResult<Option<[u8; LATEST_LEN]>> {
    if row == [0; ROW] {
        return Ok(None);
    }
    if row[38..] != [0; 2] {
        return Err(bad());
    }
    let mut value = [0; LATEST_LEN];
    value[..6].copy_from_slice(&row[..6]);
    value[8..32].copy_from_slice(&row[6..30]);
    value[32..36].copy_from_slice(&block.to_le_bytes());
    if value[1] & LATEST_HAS_PHYSICAL != 0 {
        let id = if value[0] == 1 {
            TOMBSTONE_BLOCK_ID
        } else {
            block
        };
        value[36..40].copy_from_slice(&id.to_le_bytes());
    }
    value[40..44].copy_from_slice(&row[30..34]);
    value[48..52].copy_from_slice(&row[34..38]);
    decode_latest(&value)?;
    Ok(Some(value))
}
fn append(file: &File, bytes: &[u8]) -> DiskIndexResult<u64> {
    let offset = file.metadata()?.len();
    #[cfg(feature = "scalable-fault-injection")]
    if crate::scalable_fault::is_armed() {
        crate::scalable_fault_point("checkpoint.record_prefix");
        write_at(file, offset, &bytes[..bytes.len() / 2])?;
        crate::scalable_fault_point("checkpoint.record_prefix");
    }
    crate::scalable_fault_point("checkpoint.record_write");
    write_at(file, offset, bytes)?;
    crate::scalable_fault_point("checkpoint.record_write");
    Ok(offset)
}
fn bad() -> DiskIndexError {
    DiskIndexError::Storage("invalid finite slot table".into())
}
#[derive(Clone, Copy, Default)]
pub(super) struct Root {
    pub offset: u64,
    pub count: u32,
}
#[derive(Clone)]
struct Block {
    id: u32,
    len: usize,
    used: u32,
    chunks: Vec<u64>,
}
#[derive(Clone, Default)]
struct Directory {
    blocks: Vec<Block>,
}
impl Directory {
    fn read(file: &File, end: u64, root: Root, budget: usize) -> DiskIndexResult<Self> {
        if root.offset == 0 {
            return if root.count == 0 {
                Ok(Self::default())
            } else {
                Err(bad())
            };
        }
        let mut head = [0; 24];
        if root.offset < 24576 || root.offset.checked_add(28).is_none_or(|n| n > end) {
            return Err(bad());
        }
        read_at(file, root.offset, &mut head)?;
        let len = read_u32(&head, 8) as usize;
        let blocks = read_u32(&head, 12) as usize;
        if &head[..8] != MAGIC
            || read_u64(&head, 16) != u64::from(root.count)
            || len < 28
            || len > budget / 2
            || blocks > (len - 28) / 24
            || root.offset.checked_add(len as u64).is_none_or(|n| n > end)
        {
            return Err(bad());
        }
        let mut bytes = vec![0; len];
        read_at(file, root.offset, &mut bytes)?;
        if crc32fast::hash(&bytes[..len - 4]) != read_u32(&bytes, len - 4) {
            return Err(bad());
        }
        let mut result = Self::default();
        let mut pos = 24;
        let mut count = 0u64;
        for _ in 0..blocks {
            if pos + 16 > len - 4 {
                return Err(bad());
            }
            let id = read_u32(&bytes, pos);
            let slots = read_u32(&bytes, pos + 4) as usize;
            let used = read_u32(&bytes, pos + 8);
            let chunks = read_u32(&bytes, pos + 12) as usize;
            pos += 16;
            if slots == 0
                || slots > 4096
                || used as usize > slots
                || chunks != slots.div_ceil(WIDTH)
                || result.blocks.last().is_some_and(|b| b.id >= id)
                || pos + chunks * 8 > len - 4
            {
                return Err(bad());
            }
            let mut offsets = Vec::with_capacity(chunks);
            for i in 0..chunks {
                let offset = read_u64(&bytes, pos);
                pos += 8;
                let bytes = ((slots - i * WIDTH).min(WIDTH) * ROW + 4) as u64;
                if offset != 0
                    && (offset < 24576 || offset.checked_add(bytes).is_none_or(|n| n > root.offset))
                {
                    return Err(bad());
                }
                offsets.push(offset);
            }
            count += u64::from(used);
            result.blocks.push(Block {
                id,
                len: slots,
                used,
                chunks: offsets,
            });
        }
        if pos != len - 4 || count != u64::from(root.count) {
            return Err(bad());
        }
        Ok(result)
    }
    fn find(&self, key: &[u8]) -> DiskIndexResult<Option<(usize, usize)>> {
        if key.len() < 5 || key[0] != 1 {
            return Err(bad());
        }
        let id = u32::from_be_bytes(key[1..5].try_into().unwrap());
        let Ok(block) = self.blocks.binary_search_by_key(&id, |b| b.id) else {
            return Ok(None);
        };
        if key.len() != 7 {
            return Err(bad());
        }
        let code = u16::from_le_bytes(key[5..7].try_into().unwrap()) as usize;
        if code >= self.blocks[block].len {
            return Err(bad());
        }
        Ok(Some((block, code)))
    }
}
#[derive(Default)]
struct Cache {
    bytes: usize,
    values: BTreeMap<u64, Vec<u8>>,
    order: VecDeque<u64>,
}
pub(super) struct Reader {
    file: Arc<File>,
    end: u64,
    root: Root,
    budget: usize,
    directory: RefCell<Option<Directory>>,
    cache: RefCell<Cache>,
}
impl Reader {
    pub fn new(file: Arc<File>, end: u64, root: Root, budget: usize) -> Self {
        Self {
            file,
            end,
            root,
            budget,
            directory: RefCell::new(None),
            cache: RefCell::new(Cache::default()),
        }
    }
    fn directory(&self) -> DiskIndexResult<Directory> {
        if self.directory.borrow().is_none() {
            *self.directory.borrow_mut() = Some(Directory::read(
                &self.file,
                self.end,
                self.root,
                self.budget,
            )?);
        }
        Ok(self.directory.borrow().as_ref().unwrap().clone())
    }
    pub fn get(&self, key: &[u8]) -> DiskIndexResult<Option<Option<[u8; LATEST_LEN]>>> {
        if self.root.offset == 0 {
            return Ok(None);
        }
        // Metadata is small and fixed by the schema. Slot access itself is direct.
        if self.directory.borrow().is_none() {
            self.directory()?;
        }
        let directory = self.directory.borrow();
        let directory = directory.as_ref().unwrap();
        let Some((block, code)) = directory.find(key)? else {
            return Ok(None);
        };
        let b = &directory.blocks[block];
        let chunk = code / WIDTH;
        let offset = b.chunks[chunk];
        if offset == 0 {
            return Ok(Some(None));
        }
        let mut cache = self.cache.borrow_mut();
        if !cache.values.contains_key(&offset) {
            let len = (b.len - chunk * WIDTH).min(WIDTH) * ROW + 4;
            let mut bytes = vec![0; len];
            read_at(&self.file, offset, &mut bytes)?;
            if crc32fast::hash(&bytes[..len - 4]) != read_u32(&bytes, len - 4) {
                return Err(bad());
            }
            // Reserve half the budget for directory and cache bookkeeping.
            while cache.bytes + len + 128 > self.budget / 2 {
                let old = cache.order.pop_front().ok_or_else(bad)?;
                if let Some(bytes) = cache.values.remove(&old) {
                    cache.bytes -= bytes.len() + 128;
                }
            }
            cache.bytes += len + 128;
            cache.values.insert(offset, bytes);
            cache.order.push_back(offset);
        }
        let start = (code % WIDTH) * ROW;
        Ok(Some(unpack(
            &cache.values[&offset][start..start + ROW],
            b.id,
        )?))
    }
}
pub(super) struct Working {
    directory: Directory,
    values: Vec<Vec<Option<[u8; LATEST_LEN]>>>,
    dirty: Vec<Vec<bool>>,
}
impl Working {
    pub fn load(reader: &Reader, layout: &[(u32, u32)], budget: usize) -> DiskIndexResult<Self> {
        let mut directory = reader.directory()?;
        if directory.blocks.is_empty() {
            directory.blocks = layout
                .iter()
                .map(|&(id, len)| Block {
                    id,
                    len: len as usize,
                    used: 0,
                    chunks: vec![0; (len as usize).div_ceil(WIDTH)],
                })
                .collect();
        }
        if directory
            .blocks
            .iter()
            .map(|b| (b.id, b.len as u32))
            .ne(layout.iter().copied())
        {
            return Err(bad());
        }
        let slots = layout
            .iter()
            .try_fold(0usize, |sum, (_, n)| sum.checked_add(*n as usize))
            .ok_or_else(bad)?;
        if slots.checked_mul(64).is_none_or(|n| n > budget) {
            return Err(bad());
        }
        let mut values = Vec::new();
        let mut dirty = Vec::new();
        for b in &directory.blocks {
            let mut rows = Vec::with_capacity(b.len);
            for code in 0..b.len {
                let mut key = vec![1];
                key.extend_from_slice(&b.id.to_be_bytes());
                key.extend_from_slice(&(code as u16).to_le_bytes());
                rows.push(reader.get(&key)?.flatten());
            }
            if rows.iter().filter(|row| row.is_some()).count() != b.used as usize {
                return Err(bad());
            }
            values.push(rows);
            dirty.push(vec![false; b.chunks.len()]);
        }
        Ok(Self {
            directory,
            values,
            dirty,
        })
    }
    pub fn count(&self) -> u64 {
        self.directory
            .blocks
            .iter()
            .map(|b| u64::from(b.used))
            .sum()
    }
    pub fn get(&self, key: &[u8]) -> DiskIndexResult<Option<Option<[u8; LATEST_LEN]>>> {
        Ok(self.directory.find(key)?.map(|(b, k)| self.values[b][k]))
    }
    pub fn put(&mut self, key: &[u8], value: [u8; LATEST_LEN]) -> DiskIndexResult<bool> {
        let Some((b, k)) = self.directory.find(key)? else {
            return Ok(false);
        };
        if self.values[b][k].is_none() {
            self.directory.blocks[b].used += 1;
        }
        self.values[b][k] = Some(value);
        self.dirty[b][k / WIDTH] = true;
        Ok(true)
    }
    pub fn compact(&mut self) {
        for (b, dirty) in self.directory.blocks.iter_mut().zip(&mut self.dirty) {
            b.chunks.fill(0);
            dirty.fill(true);
        }
    }
    pub fn publish(&mut self, file: &File, force: bool) -> DiskIndexResult<Root> {
        if self.directory.blocks.is_empty() {
            return Ok(Root::default());
        }
        for ((b, rows), dirty) in self
            .directory
            .blocks
            .iter_mut()
            .zip(&self.values)
            .zip(&mut self.dirty)
        {
            for (chunk, changed) in dirty.iter_mut().enumerate() {
                if !*changed && !force {
                    continue;
                }
                let range = chunk * WIDTH..((chunk + 1) * WIDTH).min(b.len);
                if rows[range.clone()].iter().all(Option::is_none) {
                    b.chunks[chunk] = 0;
                    *changed = false;
                    continue;
                }
                let mut bytes = Vec::with_capacity(range.len() * ROW + 4);
                for row in &rows[range] {
                    bytes.extend_from_slice(&pack(*row));
                }
                bytes.extend_from_slice(&crc32fast::hash(&bytes).to_le_bytes());
                let offset = append(file, &bytes)?;
                b.chunks[chunk] = offset;
                *changed = false;
            }
        }
        let count = u32::try_from(self.count()).map_err(|_| bad())?;
        let mut bytes = vec![0; 24];
        bytes[..8].copy_from_slice(MAGIC);
        bytes[12..16].copy_from_slice(&(self.directory.blocks.len() as u32).to_le_bytes());
        bytes[16..24].copy_from_slice(&u64::from(count).to_le_bytes());
        for b in &self.directory.blocks {
            bytes.extend_from_slice(&b.id.to_le_bytes());
            bytes.extend_from_slice(&(b.len as u32).to_le_bytes());
            bytes.extend_from_slice(&b.used.to_le_bytes());
            bytes.extend_from_slice(&(b.chunks.len() as u32).to_le_bytes());
            for offset in &b.chunks {
                bytes.extend_from_slice(&offset.to_le_bytes());
            }
        }
        let len = u32::try_from(bytes.len() + 4).map_err(|_| bad())?;
        bytes[8..12].copy_from_slice(&len.to_le_bytes());
        bytes.extend_from_slice(&crc32fast::hash(&bytes).to_le_bytes());
        let offset = append(file, &bytes)?;
        Ok(Root { offset, count })
    }
    pub fn layout(reader: &Reader) -> DiskIndexResult<Vec<(u32, u32)>> {
        Ok(reader
            .directory()?
            .blocks
            .iter()
            .map(|b| (b.id, b.len as u32))
            .collect())
    }
}
