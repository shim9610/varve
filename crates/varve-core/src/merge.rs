use std::hash::Hash;
use std::path::Path;

use crate::traits::KeyedBlockContract;
use crate::{FormatSpec, Result, VarveKeyedBlock};

pub trait VarveMerge: VarveKeyedBlock + Clone {
    type Op: crate::VarveEncode + crate::VarveDecode;

    fn apply_op(&mut self, op: Self::Op) -> Result<()>;
}

pub enum MergeAction<T>
where
    T: VarveMerge,
{
    Put(T),
    Delete(T::Key),
    Op { key: T::Key, op: T::Op },
}

pub struct SequencedMergeAction<T>
where
    T: VarveMerge,
{
    pub sequence: u64,
    pub action: MergeAction<T>,
}

/// Compacts `input` into `output`, keeping only the final value per key.
///
/// # Scale contract
///
/// This is a **resident** operation and is deliberately not PB-scale. It is a
/// thin delegate to [`compact_keyed_files`](crate::compact_keyed_files) with no
/// delta shards and inherits that function's bounds exactly. It opens `input`
/// as a whole [`VarveFile`](crate::VarveFile) and accumulates one map entry per
/// distinct key ever seen - including keys whose latest record is a tombstone -
/// plus the live values that survive to the output:
///
/// - time: `Theta(records + decoded bytes) + O(K-live log K-live)`;
/// - memory: `O(K-ever + resident input index + retained live values)`, where
///   `K-ever` is the number of distinct keys in `input`.
///
/// Nothing here spills to disk, so `K-ever` must fit in memory. Varve exports
/// no bounded-memory external merge/compact; the scalable stream and indexed
/// writers cover bounded *ingest*, not bounded compaction. Callers whose key
/// cardinality is not known to be resident-sized should size the operation
/// first with [`estimate_keyed_merge`](crate::estimate_keyed_merge) (pass an
/// empty delta slice) or bound it with [`compact_keyed_file_with_key_limit`],
/// which fails with a typed limit error instead of exhausting memory.
pub fn compact_keyed_file<T, P>(spec: FormatSpec, input: P, output: P) -> Result<()>
where
    T: VarveMerge,
    T::Key: Eq + Hash,
    P: AsRef<Path>,
{
    // API2-03: every public keyed generic entry point evaluates the
    // compile-time keyedness contract post-monomorphization.
    let () = KeyedBlockContract::<T>::OK;
    let empty: &[P] = &[];
    crate::file::compact_keyed_files::<T, P>(spec, input, empty, output)
}

/// [`compact_keyed_file`] with an explicit ceiling on distinct retained keys.
///
/// The collector checks `max_distinct_keys` before it admits each new key, so
/// an input whose cardinality exceeds the caller's memory budget fails with
/// [`Error::LimitExceeded`](crate::Error::LimitExceeded)
/// (`resource = "merge distinct keys"`) at the boundary instead of being
/// discovered by the allocator. The ceiling counts tombstoned keys, matching
/// what the state actually retains. Nothing is published when the ceiling is
/// exceeded: `output` is not created.
///
/// See [`compact_keyed_file`] for the full resident scale contract.
pub fn compact_keyed_file_with_key_limit<T, P>(
    spec: FormatSpec,
    input: P,
    output: P,
    max_distinct_keys: u64,
) -> Result<()>
where
    T: VarveMerge,
    T::Key: Eq + Hash,
    P: AsRef<Path>,
{
    // API2-03: every public keyed generic entry point evaluates the
    // compile-time keyedness contract post-monomorphization.
    let () = KeyedBlockContract::<T>::OK;
    let empty: &[P] = &[];
    crate::file::compact_keyed_files_with_key_limit::<T, P>(
        spec,
        input,
        empty,
        output,
        max_distinct_keys,
    )
}
