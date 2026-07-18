use std::hash::Hash;
use std::marker::PhantomData;

use crate::{BlockKind, Endian, FieldDescriptor, Result, VarveDecode, VarveEncode};

pub trait VarveBlock: VarveEncode + VarveDecode {
    const ID: u32;
    const VERSION: u16;
    const KIND: BlockKind;
    const ENDIAN: Option<Endian>;
    /// Whether this block has a generated logical key.
    ///
    /// Generated blocks set this exactly. The scalable I/O feature makes the
    /// fact mandatory for manual implementations so keyedness cannot silently
    /// default to the chain-unsafe value.
    #[cfg(feature = "high-cardinality-dev")]
    const IS_KEYED: bool;
    #[cfg(not(feature = "high-cardinality-dev"))]
    const IS_KEYED: bool = false;
    /// Process-local identity of this block's declared schema.
    ///
    /// `#[derive(VarveBlock)]` computes this deterministically (FNV-1a 64 over
    /// the canonical schema: id, version, kind, endian, keyedness, and ordered
    /// field name/type identities). Typed registration rejects two
    /// implementations that claim the same block id with different
    /// fingerprints, so a manual implementation cannot impersonate a
    /// registered type by matching only id/version/kind. Manual
    /// implementations mirroring a generated block should reuse that block's
    /// const instead of inventing a value. This is deliberately not part of
    /// the wire format or on-disk descriptors.
    const SCHEMA_FINGERPRINT: u64;
    const FIELDS: &'static [FieldDescriptor] = &[];
}

/// Compile-time proof that a keyed implementation agrees with its declared
/// [`VarveBlock::IS_KEYED`] value.
///
/// Keyed-only generic entry points evaluate [`KeyedBlockContract::OK`], which
/// turns `impl VarveKeyedBlock` + `IS_KEYED = false` into a
/// post-monomorphization compile error at every keyed use site instead of a
/// silent index-consistency hazard.
pub struct KeyedBlockContract<T: VarveKeyedBlock>(PhantomData<T>);

impl<T: VarveKeyedBlock> KeyedBlockContract<T> {
    pub const OK: () = assert!(
        T::IS_KEYED,
        "this type implements VarveKeyedBlock but declares VarveBlock::IS_KEYED = false; \
         keyed blocks must declare IS_KEYED = true"
    );
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
