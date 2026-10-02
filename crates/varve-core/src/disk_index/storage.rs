//! Varve checkpoint publication. The confirmed and working heads are separate:
//! non-durable batches can never overwrite the last durable recovery anchor.
use super::tree::{self, read_at, write_at};
use super::*;
use std::fs::{File, OpenOptions};

const MAGIC: &[u8; 8] = b"VARVEIX5";
const HEAD_MAGIC: &[u8; 8] = b"VIXHEAD1";
const ROOT_MAGIC: &[u8; 8] = b"VIXROOT4";
const ROOT_HEADER: usize = 352;
// Independently synced heads occupy separate 4096-byte regions.
pub(super) const CONFIRMED: [u64; 2] = [4096, 8192];
pub(super) const WORKING: [u64; 2] = [12288, 16384];
const DIRTY: u64 = 20480;
const FILE_HEADER: usize = 24576;

fn corrupt(message: &'static str) -> DiskIndexError {
    DiskIndexError::Storage(message.into())
}

struct WriterFile(File);
impl Drop for WriterFile {
    fn drop(&mut self) {
        let _ = crate::file::unlock_native_guard(&self.0);
    }
}

pub(super) struct Store {
    pub file: Arc<File>,
    writer: Option<Arc<WriterFile>>,
    pub path: PathBuf,
    options: DiskIndexOptions,
}

#[derive(Clone, Copy, Default)]
pub(super) struct Root {
    pub tree: tree::Root,
    pub slots: slots::Root,
}
impl Root {
    pub fn count(self) -> u64 {
        self.tree.count + u64::from(self.slots.count)
    }
}
pub(super) struct Reader {
    pub tree: tree::Reader,
    pub slots: slots::Reader,
}
impl Reader {
    pub fn get(&self, root: Root, key: &[u8]) -> DiskIndexResult<Option<[u8; LATEST_LEN]>> {
        if let Some(value) = self.slots.get(key)? {
            return Ok(value);
        }
        self.tree.get(root.tree, key)
    }
}

#[derive(Clone)]
pub(super) struct Checkpoint {
    pub offset: u64,
    pub end: u64,
    pub root: Root,
    pub metadata: DiskIndexMetadata,
}

#[derive(Clone, Copy)]
pub(super) enum Publication {
    Confirmed,
    Working,
    Dirty,
}

#[derive(Clone, Copy)]
struct Head {
    offset: u64,
    end: u64,
}

impl Store {
    pub fn open(
        path: &Path,
        options: DiskIndexOptions,
        create: bool,
        writable: bool,
    ) -> DiskIndexResult<Self> {
        let writer = if writable {
            let file = OpenOptions::new()
                .read(true)
                .write(true)
                .create(create)
                .truncate(false)
                .open(path)?;
            match crate::file::try_lock_native_guard(&file) {
                Ok(()) => {}
                Err(crate::file::WriterGuardLockError::WouldBlock) => {
                    return Err(DiskIndexError::Busy);
                }
                Err(crate::file::WriterGuardLockError::Io(error)) => return Err(error.into()),
            }
            Some(Arc::new(WriterFile(file)))
        } else {
            None
        };
        // Independent read-only file description; snapshots cannot retain a writer lock.
        let file = Arc::new(File::open(path)?);
        if let Some(writer) = &writer
            && opened_sidecar_identity(&writer.0)? != opened_sidecar_identity(&file)?
        {
            return Err(DiskIndexError::CheckpointMismatch(
                "index pathname changed while opening writer",
            ));
        }
        let result = Self {
            file,
            writer,
            path: path.to_path_buf(),
            options,
        };
        if create {
            let writer = result.writer()?;
            if writer.metadata()?.len() != 0 {
                return Err(corrupt("index already contains data"));
            }
            let mut header = [0; FILE_HEADER];
            header[..8].copy_from_slice(MAGIC);
            header[8..12].copy_from_slice(&5u32.to_le_bytes());
            let checksum = crc32fast::hash(&header[..12]);
            header[12..16].copy_from_slice(&checksum.to_le_bytes());
            write_at(writer, 0, &header)?;
        } else {
            let mut header = [0; 16];
            read_at(&result.file, 0, &mut header)?;
            if &header[..8] != MAGIC
                || read_u32(&header, 8) != 5
                || crc32fast::hash(&header[..12]) != read_u32(&header, 12)
            {
                return Err(corrupt("invalid Varve index file header"));
            }
        }
        Ok(result)
    }
    pub fn fork(&self) -> Self {
        Self {
            file: self.file.clone(),
            writer: self.writer.clone(),
            path: self.path.clone(),
            options: self.options,
        }
    }
    pub fn writer(&self) -> DiskIndexResult<&File> {
        self.writer
            .as_ref()
            .map(|f| &f.0)
            .ok_or_else(|| corrupt("index handle is read-only"))
    }
    pub fn reader(&self, checkpoint: &Checkpoint) -> Reader {
        let budget = if checkpoint.root.slots.offset == 0 {
            self.options.cache_bytes
        } else {
            self.options.cache_bytes / 2
        };
        Reader {
            tree: tree::Reader::new(
                self.file.clone(),
                checkpoint.offset,
                self.options
                    .max_key_bytes
                    .saturating_add(DiskIndexKeyTag::ENCODED_LEN),
                budget,
            ),
            slots: slots::Reader::new(
                self.file.clone(),
                checkpoint.offset,
                checkpoint.root.slots,
                self.options.cache_bytes / 2,
            ),
        }
    }
    fn head(&self, slot: u64) -> DiskIndexResult<Option<Head>> {
        let mut bytes = [0; 64];
        read_at(&self.file, slot, &mut bytes)?;
        if &bytes[..8] != HEAD_MAGIC
            || bytes[24..28] != [0; 4]
            || bytes[32..] != [0; 32]
            || crc32fast::hash(&bytes[..28]) != read_u32(&bytes, 28)
        {
            return Ok(None);
        }
        let head = Head {
            offset: read_u64(&bytes, 8),
            end: read_u64(&bytes, 16),
        };
        if head.offset < FILE_HEADER as u64
            || head
                .end
                .checked_sub(head.offset)
                .is_none_or(|n| n < ROOT_HEADER as u64)
        {
            return Ok(None);
        }
        Ok(Some(head))
    }
    pub fn at(&self, offset: u64) -> DiskIndexResult<Checkpoint> {
        let mut bytes = [0; ROOT_HEADER];
        if offset < FILE_HEADER as u64 {
            return Err(corrupt("invalid checkpoint offset"));
        }
        read_at(&self.file, offset, &mut bytes)?;
        if &bytes[..8] != ROOT_MAGIC || crc32fast::hash(&bytes[..348]) != read_u32(&bytes, 348) {
            return Err(corrupt("invalid checkpoint header"));
        }
        let metadata = decode_metadata(&bytes[32..332])?;
        let tails = read_u32(&bytes, 332);
        if tails != metadata.working_tail_count {
            return Err(DiskIndexError::TailCountMismatch {
                expected: metadata.working_tail_count,
                actual: tails,
            });
        }
        let len = ROOT_HEADER as u64 + u64::from(tails) * TAIL_LEN as u64;
        let end = offset
            .checked_add(len)
            .ok_or_else(|| corrupt("checkpoint extent overflow"))?;
        if read_u64(&bytes, 8) != len || end > self.file.metadata()?.len() {
            return Err(corrupt("checkpoint exceeds file"));
        }
        let tree = tree::Root {
            offset: read_u64(&bytes, 16),
            count: read_u64(&bytes, 24),
        };
        if (tree.offset == 0) != (tree.count == 0)
            || tree.count > metadata.working.record_count
            || (tree.offset != 0
                && (tree.offset < tree::PAGE as u64
                    || tree
                        .offset
                        .checked_add(tree::PAGE as u64)
                        .is_none_or(|n| n > offset)))
        {
            return Err(corrupt("invalid checkpoint tree root"));
        }
        let slots = slots::Root {
            offset: read_u64(&bytes, 336),
            count: read_u32(&bytes, 344),
        };
        let root = Root { tree, slots };
        if tree
            .count
            .checked_add(u64::from(slots.count))
            .is_none_or(|n| n > metadata.working.record_count)
            || (slots.offset == 0 && slots.count != 0)
            || (slots.offset != 0
                && (slots.offset < FILE_HEADER as u64
                    || slots.offset.checked_add(28).is_none_or(|n| n > offset)))
        {
            return Err(corrupt("invalid finite slot root"));
        }
        if metadata.state == DiskIndexState::Dirty
            && metadata.checkpoint.is_none_or(|c| {
                c.checkpoint_offset >= offset || c.checkpoint_offset < FILE_HEADER as u64
            })
        {
            return Err(corrupt("invalid rollback checkpoint offset"));
        }
        Ok(Checkpoint {
            offset,
            end,
            root,
            metadata,
        })
    }
    fn latest(&self, confirmed_only: bool) -> DiskIndexResult<Checkpoint> {
        // Retry only an incomplete publication. Old immutable checkpoints remain readable.
        for _ in 0..8 {
            let mut heads = Vec::with_capacity(5);
            for (slot, durable) in
                CONFIRMED
                    .into_iter()
                    .map(|s| (s, true))
                    .chain(if confirmed_only {
                        vec![]
                    } else {
                        vec![(WORKING[0], false), (WORKING[1], false), (DIRTY, true)]
                    })
            {
                if let Some(head) = self.head(slot)? {
                    heads.push((head, durable));
                }
            }
            heads.sort_unstable_by_key(|(head, _)| std::cmp::Reverse(head.offset));
            for (head, durable) in heads {
                match self.at(head.offset) {
                    Ok(checkpoint)
                        if checkpoint.end == head.end
                            && (!confirmed_only
                                || checkpoint.metadata.state == DiskIndexState::Clean) =>
                    {
                        return Ok(checkpoint);
                    }
                    // A durable head is only written after its checkpoint is synced.
                    // A valid head naming invalid data is corruption, never permission
                    // to silently forget an acknowledged generation.
                    Err(error) if durable => return Err(error),
                    Ok(_) if durable => return Err(corrupt("durable head/checkpoint mismatch")),
                    _ => {}
                }
            }
            std::hint::spin_loop();
        }
        Err(DiskIndexError::MetadataMissing)
    }
    pub fn current(&self) -> DiskIndexResult<Checkpoint> {
        self.latest(false)
    }
    pub fn confirmed(&self) -> DiskIndexResult<Checkpoint> {
        self.latest(true)
    }
    pub fn state(
        &self,
        checkpoint: &Checkpoint,
        expected: Option<u32>,
    ) -> DiskIndexResult<DiskIndexPersistentState> {
        let metadata = checkpoint.metadata;
        if let Some(limit) = expected {
            validate_expected_tail_limit(metadata, limit)?;
        }
        let count = metadata.working_tail_count as usize;
        let bytes = count
            .checked_mul(TAIL_LEN)
            .ok_or_else(|| corrupt("tail extent overflow"))?;
        if bytes > self.options.batch.max_bytes {
            return Err(corrupt("tail state exceeds configured batch budget"));
        }
        let mut tails = Vec::new();
        tails
            .try_reserve_exact(count)
            .map_err(|_| corrupt("tail allocation failed"))?;
        let mut previous = None;
        for i in 0..count {
            let mut row = [0; TAIL_LEN];
            read_at(
                &self.file,
                checkpoint.offset + ROOT_HEADER as u64 + (i * TAIL_LEN) as u64,
                &mut row,
            )?;
            let id = read_u32(&row, 0);
            if previous.is_some_and(|p| p >= id) {
                return Err(DiskIndexError::InvalidTail("tails are not canonical"));
            }
            previous = Some(id);
            tails.push(decode_tail(id, &row)?);
        }
        validate_working_tails(metadata, &tails)?;
        Ok(DiskIndexPersistentState { metadata, tails })
    }
    pub fn publish(
        &self,
        metadata: DiskIndexMetadata,
        tails: &[DiskIndexTail],
        root: Root,
        publication: Publication,
    ) -> DiskIndexResult<Checkpoint> {
        validate_metadata(metadata)?;
        validate_working_tails(metadata, tails)?;
        if tails
            .len()
            .checked_mul(TAIL_LEN)
            .is_none_or(|n| n > self.options.batch.max_bytes)
        {
            return Err(corrupt("tail state exceeds configured batch budget"));
        }
        let file = self.writer()?;
        let len = ROOT_HEADER + tails.len() * TAIL_LEN;
        let mut bytes = vec![0; len];
        bytes[..8].copy_from_slice(ROOT_MAGIC);
        bytes[8..16].copy_from_slice(&(len as u64).to_le_bytes());
        bytes[16..24].copy_from_slice(&root.tree.offset.to_le_bytes());
        bytes[24..32].copy_from_slice(&root.tree.count.to_le_bytes());
        bytes[32..332].copy_from_slice(&encode_metadata(metadata));
        bytes[332..336].copy_from_slice(&(tails.len() as u32).to_le_bytes());
        bytes[336..344].copy_from_slice(&root.slots.offset.to_le_bytes());
        bytes[344..348].copy_from_slice(&root.slots.count.to_le_bytes());
        let checksum = crc32fast::hash(&bytes[..348]);
        bytes[348..352].copy_from_slice(&checksum.to_le_bytes());
        for (tail, into) in tails
            .iter()
            .zip(bytes[ROOT_HEADER..].chunks_exact_mut(TAIL_LEN))
        {
            into.copy_from_slice(&encode_tail(*tail));
        }
        let offset = file.metadata()?.len();
        let end = offset
            .checked_add(len as u64)
            .ok_or_else(|| corrupt("checkpoint extent overflow"))?;
        #[cfg(feature = "scalable-fault-injection")]
        if crate::scalable_fault::is_armed() {
            crate::scalable_fault_point("checkpoint.record_prefix");
            write_at(file, offset, &bytes[..bytes.len() / 2])?;
            crate::scalable_fault_point("checkpoint.record_prefix");
        }
        crate::scalable_fault_point("checkpoint.record_write");
        write_at(file, offset, &bytes)?;
        crate::scalable_fault_point("checkpoint.record_write");
        let durable = !matches!(publication, Publication::Working);
        if durable {
            crate::scalable_fault_point("checkpoint.data_sync");
            file.sync_data()?;
            crate::scalable_fault_point("checkpoint.data_sync");
        }
        let slot = match publication {
            Publication::Dirty => DIRTY,
            Publication::Confirmed | Publication::Working => {
                let slots = if matches!(publication, Publication::Confirmed) {
                    CONFIRMED
                } else {
                    WORKING
                };
                let first = self.head(slots[0])?.map_or(0, |h| h.offset);
                let second = self.head(slots[1])?.map_or(0, |h| h.offset);
                slots[usize::from(first > second)]
            }
        };
        let mut head = [0; 64];
        head[..8].copy_from_slice(HEAD_MAGIC);
        head[8..16].copy_from_slice(&offset.to_le_bytes());
        head[16..24].copy_from_slice(&end.to_le_bytes());
        let checksum = crc32fast::hash(&head[..28]);
        head[28..32].copy_from_slice(&checksum.to_le_bytes());
        let mut slots = vec![slot];
        if matches!(publication, Publication::Confirmed) {
            slots.push(CONFIRMED[usize::from(slot == CONFIRMED[0])]);
        }
        for slot in slots {
            #[cfg(feature = "scalable-fault-injection")]
            if crate::scalable_fault::is_armed() {
                crate::scalable_fault_point("checkpoint.head_prefix");
                write_at(file, slot, &head[..24])?;
                crate::scalable_fault_point("checkpoint.head_prefix");
            }
            crate::scalable_fault_point("checkpoint.head_write");
            write_at(file, slot, &head)?;
            crate::scalable_fault_point("checkpoint.head_write");
            if durable {
                crate::scalable_fault_point("checkpoint.head_sync");
                file.sync_data()?;
                crate::scalable_fault_point("checkpoint.head_sync");
            }
        }
        Ok(Checkpoint {
            offset,
            end,
            root,
            metadata,
        })
    }
    pub fn apply(
        &self,
        base: &Checkpoint,
        updates: &BTreeMap<Vec<u8>, [u8; LATEST_LEN]>,
    ) -> DiskIndexResult<Root> {
        Ok(Root {
            tree: tree::apply(
                self.writer()?,
                &self.reader(base).tree,
                base.root.tree,
                updates,
            )?,
            slots: base.root.slots,
        })
    }
}
