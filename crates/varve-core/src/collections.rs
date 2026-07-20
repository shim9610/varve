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

/// Validated endian, keyedness, and schema fingerprint per registered block
/// identity.
#[derive(Clone, Copy)]
struct BlockContract {
    fingerprint: u64,
    keyed: bool,
    /// Declared endian **override**, exactly as
    /// [`crate::VarveBlock::ENDIAN`] and
    /// [`FormatSpec::block_identities`] spell it (API-01).
    ///
    /// `None` is not "unknown": it is the positive declaration *"no override —
    /// inherit the format endian"*, which is why it is compared after
    /// resolution through [`FormatSpec::endian`] rather than as an opaque
    /// tag. See [`check_block_contract`].
    endian: Option<Endian>,
}

impl BlockContract {
    /// The contract `T` itself declares.
    fn declared_by<T: VarveBlock>() -> Self {
        Self {
            fingerprint: T::SCHEMA_FINGERPRINT,
            keyed: T::IS_KEYED,
            endian: T::ENDIAN,
        }
    }
}

/// Registered block identity for the *first-use* contract cache.
///
/// API-02: a `&'static` slice is identified by its start address **and its
/// length**. `FormatSpec` accepts arbitrary caller-supplied static descriptor
/// and identity slices, so an empty or prefix view of an array starts at the
/// same address as the full view while denoting a different logical table;
/// keying on the address alone let those two share one cached contract. Both
/// tables therefore contribute `(address, length)`.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct BlockContractKey {
    blocks_ptr: usize,
    blocks_len: usize,
    identities_ptr: usize,
    identities_len: usize,
    block_id: u32,
}

impl BlockContractKey {
    fn new(spec: FormatSpec, block_id: u32) -> Self {
        Self {
            blocks_ptr: spec.blocks.as_ptr() as usize,
            blocks_len: spec.blocks.len(),
            identities_ptr: spec.block_identities.as_ptr() as usize,
            identities_len: spec.block_identities.len(),
            block_id,
        }
    }
}

/// Process-local first-use contract cache for block ids that their format
/// declares **no** identity for.
///
/// This deliberately never touches the wire format or on-disk descriptors.
/// Block ids covered by [`FormatSpec::block_identities`] never reach this
/// cache at all (see [`ensure_block_contract`]), so every spec `varve_format!`
/// emits is validated straight from immutable `&'static` data with no
/// process-global lock on the append path. The sorted vector is only appended
/// to on the first registration of an identity-less (format, block) pair: no
/// allocation, no syscall, no O(records) work on any later call.
///
/// What two specs deliberately *do* share an entry: identity-less specs whose
/// descriptor tables are the same `&'static` slice — same address and same
/// length — even when they differ in magic, version, endian, or policies. The
/// entry then records the same constraint either spec would impose on its own,
/// because the entry stores the *declared* endian override and resolves it
/// against the caller's `spec.endian` at check time rather than at insert
/// time. Sharing here can only be redundant, never wrong.
///
/// The one identity this cannot see through is deallocation: a `&'static`
/// slice obtained by leaking a heap allocation that is later reclaimed through
/// `unsafe` code could hand a new, unrelated table the same address and length.
/// Formats built from ordinary statics — which is every format the macros
/// produce — cannot reach that state.
static BLOCK_CONTRACTS: RwLock<Vec<(BlockContractKey, BlockContract)>> = RwLock::new(Vec::new());

fn check_block_contract<T: VarveBlock>(spec: FormatSpec, recorded: BlockContract) -> Result<()> {
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
    // API-01: every typed encode/decode path resolves the block's byte order
    // as `T::ENDIAN.unwrap_or(spec.endian)`. Comparing the *resolved* byte
    // order is what makes this total and exact:
    //
    // - both sides unspecified, or both specified alike: equal, and the bytes
    //   really are identical;
    // - one side unspecified against an override that differs from
    //   `spec.endian`: rejected, which is the byte-swap the fixture caught;
    // - one side unspecified against an override *equal* to `spec.endian`:
    //   accepted, because the two declarations encode and decode the same
    //   bytes — accepting it is agreement, not a silent pass.
    //
    // Comparing the raw `Option`s instead would reject that last, genuinely
    // equivalent case while adding no protection.
    let registered = recorded.endian.unwrap_or(spec.endian);
    let declared = T::ENDIAN.unwrap_or(spec.endian);
    if registered != declared {
        return Err(crate::Error::EndianMismatch {
            expected: registered,
            actual: declared,
        });
    }
    Ok(())
}

fn ensure_block_contract<T: VarveBlock>(spec: FormatSpec) -> Result<()> {
    // Where the format declares an immutable identity for this block id, that
    // identity — not whichever `T` happened to be used first — is the
    // authority, and it is a pure function of the spec's `&'static` data.
    // Validating it directly is both cheaper (no process-global shared-lock
    // acquire on the per-record append path) and structurally immune to cache
    // aliasing: nothing keyed by an address is consulted. `varve_format!`
    // always emits identities, so this is the path every generated format
    // takes.
    if let Some((_, endian, keyed, fingerprint)) = spec.block_identity(T::ID) {
        return check_block_contract::<T>(
            spec,
            BlockContract {
                fingerprint,
                keyed,
                endian,
            },
        );
    }
    // Hand-built specs that carry no generated identity for this block id keep
    // the documented first-use escape hatch: the first type to register the id
    // defines the contract every later type must match.
    let key = BlockContractKey::new(spec, T::ID);
    {
        let contracts = BLOCK_CONTRACTS
            .read()
            .unwrap_or_else(PoisonError::into_inner);
        if let Ok(index) = contracts.binary_search_by_key(&key, |entry| entry.0) {
            return check_block_contract::<T>(spec, contracts[index].1);
        }
    }
    // Cache miss. Nothing is recorded until validation succeeds, so a rejected
    // type can never poison the id for the type that legitimately owns it.
    let contract = BlockContract::declared_by::<T>();
    check_block_contract::<T>(spec, contract)?;
    let mut contracts = BLOCK_CONTRACTS
        .write()
        .unwrap_or_else(PoisonError::into_inner);
    match contracts.binary_search_by_key(&key, |entry| entry.0) {
        Ok(index) => check_block_contract::<T>(spec, contracts[index].1),
        Err(index) => {
            contracts.insert(index, (key, contract));
            Ok(())
        }
    }
}
