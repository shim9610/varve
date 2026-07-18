use std::collections::HashMap;
use std::hash::Hash;
use std::marker::PhantomData;
use std::sync::{PoisonError, RwLock};

use crate::traits::KeyedBlockContract;
use crate::{
    BlockKind, Decoder, Endian, FormatSpec, ReadLimit, RecordIndexEntry, Result, SnapshotFile,
    VarveBlock, VarveDecode, VarveKeyedBlock, format::ReadLimitKey,
};

#[derive(Clone, Copy, Debug)]
pub(crate) struct MaterializationBudget {
    limit: u64,
    remaining: u64,
}

impl MaterializationBudget {
    pub(crate) fn new(spec: FormatSpec) -> Self {
        let limit = match spec.read_limits.resolve().max_materialized_bytes {
            ReadLimit::Finite(limit) => limit,
            ReadLimit::Missing | ReadLimit::TrustedUnbounded => u64::MAX,
        };
        Self {
            limit,
            remaining: limit,
        }
    }

    pub(crate) fn consume(&mut self, bytes: u64) -> Result<()> {
        if bytes > self.remaining {
            let consumed = self.limit.checked_sub(self.remaining).ok_or(
                crate::Error::ResourceArithmeticOverflow {
                    resource: "materialized bytes",
                },
            )?;
            let actual =
                consumed
                    .checked_add(bytes)
                    .ok_or(crate::Error::ResourceArithmeticOverflow {
                        resource: "materialized bytes",
                    })?;
            return Err(crate::Error::LimitExceeded {
                resource: ReadLimitKey::MaterializedBytes.resource(),
                actual,
                limit: self.limit,
            });
        }
        self.remaining -= bytes;
        Ok(())
    }

    pub(crate) fn remaining_policy(&mut self) -> &mut u64 {
        &mut self.remaining
    }

    pub(crate) fn decode<T: VarveDecode>(&mut self, bytes: &[u8], endian: Endian) -> Result<T> {
        Decoder::decode_from_slice_accounted(bytes, endian, self.remaining_policy())
    }
}

#[derive(Debug)]
pub struct BlockVec<T> {
    spec: FormatSpec,
    snapshot: SnapshotFile,
    entries: Vec<RecordIndexEntry>,
    _marker: PhantomData<T>,
}

impl<T> Clone for BlockVec<T> {
    fn clone(&self) -> Self {
        Self {
            spec: self.spec,
            snapshot: self.snapshot.clone(),
            entries: self.entries.clone(),
            _marker: PhantomData,
        }
    }
}

impl<T> BlockVec<T>
where
    T: VarveBlock,
{
    pub(crate) fn new(
        spec: FormatSpec,
        snapshot: SnapshotFile,
        entries: Vec<RecordIndexEntry>,
    ) -> Self {
        Self {
            spec,
            snapshot,
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
        let mut budget = MaterializationBudget::new(self.spec);
        self.get_with_budget(index, &mut budget)
    }

    pub(crate) fn get_with_budget(
        &self,
        index: usize,
        budget: &mut MaterializationBudget,
    ) -> Result<Option<T>> {
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
        let logical_len = entry.logical_payload_len_snapshot(self.spec, &self.snapshot)?;
        budget.consume(logical_len)?;
        let payload = entry.read_logical_payload_snapshot(self.spec, &self.snapshot)?;
        Ok(Some(
            budget.decode(&payload, T::ENDIAN.unwrap_or(self.spec.endian))?,
        ))
    }

    pub fn iter(&self) -> BlockIter<'_, T> {
        BlockIter {
            collection: self,
            index: 0,
            budget: MaterializationBudget::new(self.spec),
        }
    }
}

pub struct BlockIter<'a, T> {
    collection: &'a BlockVec<T>,
    index: usize,
    budget: MaterializationBudget,
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
        let result = self
            .collection
            .get_with_budget(self.index, &mut self.budget)
            .transpose();
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
        // Post-monomorphization keyedness contract: constructing a keyed
        // collection for a type whose VarveBlock impl denies being keyed is a
        // compile error, not a runtime index-consistency hazard.
        let () = KeyedBlockContract::<T>::OK;
        Self { inner, by_key }
    }

    pub fn len(&self) -> usize {
        self.by_key.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_key.is_empty()
    }

    pub fn get(&self, key: &K) -> Result<Option<T>> {
        let () = KeyedBlockContract::<T>::OK;
        let mut budget = MaterializationBudget::new(self.inner.spec);
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
        let logical_len =
            entry.logical_payload_len_snapshot(self.inner.spec, &self.inner.snapshot)?;
        budget.consume(logical_len)?;
        let payload = entry.read_logical_payload_snapshot(self.inner.spec, &self.inner.snapshot)?;
        Ok(Some(budget.decode(
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
    ensure_block_contract::<T>(spec)
}

/// First-seen keyedness and schema fingerprint per registered block identity.
#[derive(Clone, Copy)]
struct BlockContract {
    fingerprint: u64,
    keyed: bool,
}

/// Registered block identity: the format's static descriptor table address
/// scopes block ids so unrelated formats that reuse an id never collide.
type BlockContractKey = (usize, u32);

/// Process-local registry of first-seen block contracts. This deliberately
/// never touches the wire format or on-disk descriptors. The sorted vector is
/// only appended to on the first registration of a (format, block) pair, so
/// the append hot path pays one uncontended shared-lock acquire and a binary
/// search: no allocation, no syscall, no O(records) work.
static BLOCK_CONTRACTS: RwLock<Vec<(BlockContractKey, BlockContract)>> = RwLock::new(Vec::new());

fn check_block_contract<T: VarveBlock>(recorded: BlockContract) -> Result<()> {
    if recorded.keyed != T::IS_KEYED {
        return Err(crate::Error::BlockKeyednessMismatch {
            block_id: T::ID,
            registered: recorded.keyed,
            declared: T::IS_KEYED,
        });
    }
    if recorded.fingerprint != T::SCHEMA_FINGERPRINT {
        return Err(crate::Error::BlockSchemaFingerprintMismatch {
            block_id: T::ID,
            registered: recorded.fingerprint,
            declared: T::SCHEMA_FINGERPRINT,
        });
    }
    Ok(())
}

fn ensure_block_contract<T: VarveBlock>(spec: FormatSpec) -> Result<()> {
    let key: BlockContractKey = (spec.blocks.as_ptr() as usize, T::ID);
    {
        let contracts = BLOCK_CONTRACTS
            .read()
            .unwrap_or_else(PoisonError::into_inner);
        if let Ok(index) = contracts.binary_search_by_key(&key, |entry| entry.0) {
            return check_block_contract::<T>(contracts[index].1);
        }
    }
    let mut contracts = BLOCK_CONTRACTS
        .write()
        .unwrap_or_else(PoisonError::into_inner);
    match contracts.binary_search_by_key(&key, |entry| entry.0) {
        Ok(index) => check_block_contract::<T>(contracts[index].1),
        Err(index) => {
            contracts.insert(
                index,
                (
                    key,
                    BlockContract {
                        fingerprint: T::SCHEMA_FINGERPRINT,
                        keyed: T::IS_KEYED,
                    },
                ),
            );
            Ok(())
        }
    }
}
