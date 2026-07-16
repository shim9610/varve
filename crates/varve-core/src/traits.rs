use std::hash::Hash;

use crate::{BlockKind, Endian, FieldDescriptor, Result, VarveDecode, VarveEncode};

pub trait VarveBlock: VarveEncode + VarveDecode {
    const ID: u32;
    const VERSION: u16;
    const KIND: BlockKind;
    const ENDIAN: Option<Endian>;
    const FIELDS: &'static [FieldDescriptor] = &[];
}

/// Opts a block into sequence-preserving copy-on-write replacement.
///
/// Implementations may reject a replacement before any new generation is
/// published. Generated keyed blocks use this hook to require equal keys.
pub trait VarveReplaceBlock: VarveBlock {
    fn validate_replacement(old: &Self, new: &Self) -> Result<()>;
}

pub trait VarveKey: Eq + Hash + Clone + VarveEncode + VarveDecode + Send + Sync + 'static {}

impl<T> VarveKey for T where T: Eq + Hash + Clone + VarveEncode + VarveDecode + Send + Sync + 'static
{}

pub trait VarveKeyedBlock: VarveBlock {
    type Key: VarveKey;

    fn key(&self) -> Self::Key;
}

pub trait VarveMatrixBlock: VarveBlock {
    const DIMENSIONS: [&'static str; 2];
    const CATEGORY: &'static str;
    const SLOT_STRIDE: u64;
}

pub trait VarveMigration<From, To>
where
    From: VarveBlock,
    To: VarveBlock,
{
    fn migrate(from: From) -> crate::Result<To>;
}

#[cfg(feature = "zero-copy")]
/// Raw fixed blocks can be viewed directly from mmap-backed payload bytes.
///
/// # Safety
///
/// Implementors must guarantee that every payload for this block id/version is
/// a valid immutable instance of `Self` in Rust's raw memory layout, using
/// `RAW_ENDIAN`, `size_of::<Self>()`, and `align_of::<Self>()`. This is not the
/// same contract as Varve's canonical fixed encoding.
pub unsafe trait VarveRawFixedBlock:
    VarveBlock + zerocopy::FromBytes + zerocopy::Immutable + zerocopy::KnownLayout
{
    const RAW_ENDIAN: Endian;
}

#[cfg(feature = "zero-copy")]
/// Raw matrix blocks can be viewed directly from mmap-backed fixed-stride slots.
///
/// # Safety
///
/// Implementors must guarantee that every committed slot payload for this block
/// id/version/category is a valid immutable instance of `Self` in Rust's raw
/// memory layout, using `RAW_ENDIAN`, `size_of::<Self>()`, and
/// `align_of::<Self>()`. This is an explicit opt-in contract and is separate
/// from Varve's normal canonical matrix decoding.
pub unsafe trait VarveRawMatrixBlock:
    VarveMatrixBlock + zerocopy::FromBytes + zerocopy::Immutable + zerocopy::KnownLayout
{
    const RAW_ENDIAN: Endian;
}
