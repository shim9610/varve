use std::hash::Hash;
use std::path::Path;

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

pub fn compact_keyed_file<T, P>(spec: FormatSpec, input: P, output: P) -> Result<()>
where
    T: VarveMerge,
    T::Key: Eq + Hash,
    P: AsRef<Path>,
{
    let empty: &[P] = &[];
    crate::file::compact_keyed_files::<T, P>(spec, input, empty, output)
}
