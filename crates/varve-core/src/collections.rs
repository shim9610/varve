use std::collections::HashMap;
use std::hash::Hash;
use std::marker::PhantomData;
use std::path::PathBuf;

use crate::{
    BlockKind, FormatSpec, RecordIndexEntry, Result, VarveBlock, VarveKeyedBlock, decode_from_slice,
};

#[derive(Debug)]
pub struct BlockVec<T> {
    spec: FormatSpec,
    path: PathBuf,
    entries: Vec<RecordIndexEntry>,
    _marker: PhantomData<T>,
}

impl<T> Clone for BlockVec<T> {
    fn clone(&self) -> Self {
        Self {
            spec: self.spec,
            path: self.path.clone(),
            entries: self.entries.clone(),
            _marker: PhantomData,
        }
    }
}

impl<T> BlockVec<T>
where
    T: VarveBlock,
{
    pub(crate) fn new(spec: FormatSpec, path: PathBuf, entries: Vec<RecordIndexEntry>) -> Self {
        Self {
            spec,
            path,
            entries,
            _marker: PhantomData,
        }
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn get(&self, index: usize) -> Result<Option<T>> {
        let Some(entry) = self.entries.get(index) else {
            return Ok(None);
        };
        if entry.block_version != T::VERSION {
            return Err(crate::Error::BlockVersionMismatch {
                block_id: T::ID,
                expected: T::VERSION,
                actual: entry.block_version,
            });
        }
        let payload = entry.read_logical_payload(self.spec, &self.path)?;
        Ok(Some(decode_from_slice(
            &payload,
            T::ENDIAN.unwrap_or(self.spec.endian),
        )?))
    }

    pub fn iter(&self) -> BlockIter<'_, T> {
        BlockIter {
            collection: self,
            index: 0,
        }
    }
}

pub struct BlockIter<'a, T> {
    collection: &'a BlockVec<T>,
    index: usize,
}

impl<T> Iterator for BlockIter<'_, T>
where
    T: VarveBlock,
{
    type Item = Result<T>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.index >= self.collection.len() {
            return None;
        }
        let result = self.collection.get(self.index).transpose();
        self.index += 1;
        result
    }
}

#[derive(Debug)]
pub struct KeyedBlockVec<K, T> {
    inner: BlockVec<T>,
    by_key: HashMap<K, RecordIndexEntry>,
}

impl<K, T> KeyedBlockVec<K, T>
where
    K: Eq + Hash + Clone,
    T: VarveKeyedBlock<Key = K>,
{
    pub(crate) fn from_parts(inner: BlockVec<T>, by_key: HashMap<K, RecordIndexEntry>) -> Self {
        Self { inner, by_key }
    }

    pub fn len(&self) -> usize {
        self.by_key.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_key.is_empty()
    }

    pub fn get(&self, key: &K) -> Result<Option<T>> {
        let Some(entry) = self.by_key.get(key) else {
            return Ok(None);
        };
        if entry.block_version != T::VERSION {
            return Err(crate::Error::BlockVersionMismatch {
                block_id: T::ID,
                expected: T::VERSION,
                actual: entry.block_version,
            });
        }
        let payload = entry.read_logical_payload(self.inner.spec, &self.inner.path)?;
        Ok(Some(decode_from_slice(
            &payload,
            T::ENDIAN.unwrap_or(self.inner.spec.endian),
        )?))
    }

    pub fn keys(&self) -> impl Iterator<Item = &K> {
        self.by_key.keys()
    }

    pub fn as_blocks(&self) -> &BlockVec<T> {
        &self.inner
    }
}

pub(crate) fn ensure_registered_block<T: VarveBlock>(spec: FormatSpec) -> Result<()> {
    let descriptor = spec
        .block(T::ID)
        .ok_or(crate::Error::UnregisteredBlock(T::ID))?;
    if descriptor.kind != T::KIND && descriptor.kind != BlockKind::Internal {
        return Err(crate::Error::BlockKindMismatch {
            expected: descriptor.kind,
            actual: T::KIND,
        });
    }
    if descriptor.version != T::VERSION {
        return Err(crate::Error::BlockVersionMismatch {
            block_id: T::ID,
            expected: descriptor.version,
            actual: T::VERSION,
        });
    }
    Ok(())
}
