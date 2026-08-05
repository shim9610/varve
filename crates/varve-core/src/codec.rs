use std::cmp::Ordering;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::hash::Hash;
use std::mem::{align_of, size_of};

use crate::{Endian, Error, Result};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u16)]
pub enum WireType {
    Unit = 0,
    Bool = 1,
    U8 = 2,
    I8 = 3,
    U16 = 4,
    I16 = 5,
    U32 = 6,
    I32 = 7,
    U64 = 8,
    I64 = 9,
    U128 = 10,
    I128 = 11,
    F32 = 12,
    F64 = 13,
    Bytes = 14,
    String = 15,
    Seq = 16,
    Nested = 17,
}

impl WireType {
    pub const fn from_u16(value: u16) -> Option<Self> {
        match value {
            0 => Some(Self::Unit),
            1 => Some(Self::Bool),
            2 => Some(Self::U8),
            3 => Some(Self::I8),
            4 => Some(Self::U16),
            5 => Some(Self::I16),
            6 => Some(Self::U32),
            7 => Some(Self::I32),
            8 => Some(Self::U64),
            9 => Some(Self::I64),
            10 => Some(Self::U128),
            11 => Some(Self::I128),
            12 => Some(Self::F32),
            13 => Some(Self::F64),
            14 => Some(Self::Bytes),
            15 => Some(Self::String),
            16 => Some(Self::Seq),
            17 => Some(Self::Nested),
            _ => None,
        }
    }
}

/// FNV-1a 64 state for structural codec identities (API-04).
///
/// The derive expands the same folding inline so a generated
/// `SCHEMA_FINGERPRINT` is a const expression over its fields' real
/// `SCHEMA_ID` values rather than over their source spelling.
const SCHEMA_ID_SEED: u64 = 0xcbf2_9ce4_8422_2325;
const SCHEMA_ID_PRIME: u64 = 0x0000_0100_0000_01b3;

const fn schema_id_bytes(acc: u64, bytes: &[u8]) -> u64 {
    let mut acc = acc;
    let mut index = 0;
    while index < bytes.len() {
        acc ^= bytes[index] as u64;
        acc = acc.wrapping_mul(SCHEMA_ID_PRIME);
        index += 1;
    }
    acc
}

const fn schema_id_u64(acc: u64, value: u64) -> u64 {
    schema_id_bytes(acc, &value.to_le_bytes())
}

/// Identity of a leaf codec whose byte layout is fully described by `tag`.
pub(crate) const fn leaf_schema_id(tag: &[u8]) -> u64 {
    schema_id_bytes(SCHEMA_ID_SEED, tag)
}

/// Identity of a container codec, folded transitively over its elements.
///
/// **Absence is contagious.** An element whose `SCHEMA_ID` is `0` declares no
/// identity, so the container built over it cannot claim one either: the
/// result is `0`, not a hash of `0`. Without this rule `Option<Packed>` or
/// `Vec<Packed>` would manufacture a non-zero identity out of two *different*
/// identity-less `Packed` codecs and hand them the same value — exactly the
/// collision `SCHEMA_ID` exists to prevent, merely one container deep. It also
/// keeps the derive's per-field assertion transitively complete: rejecting
/// zero at the field type rejects an identity-less codec at any nesting depth.
const fn container_schema_id(tag: &[u8], elements: &[u64]) -> u64 {
    container_schema_id_with_arity(tag, elements.len() as u64, elements)
}

/// [`container_schema_id`] for containers whose arity is part of the layout
/// rather than an element identity (arrays), so `[T; 0]` is not mistaken for
/// an identity-less element.
pub(crate) const fn container_schema_id_with_arity(
    tag: &[u8],
    arity: u64,
    elements: &[u64],
) -> u64 {
    let mut acc = schema_id_u64(leaf_schema_id(tag), arity);
    let mut index = 0;
    while index < elements.len() {
        if elements[index] == 0 {
            return 0;
        }
        acc = schema_id_u64(acc, elements[index]);
        index += 1;
    }
    acc
}

pub trait VarveEncode {
    const WIRE_TYPE: WireType;

    /// Structural identity of the bytes this codec emits (API-04).
    ///
    /// [`WireType`] is far too coarse to separate two custom codecs that both
    /// encode as [`WireType::Nested`] but disagree on layout, and a derived
    /// block's fingerprint cannot see through a field type's source spelling.
    /// `SCHEMA_ID` is the value that does distinguish them: built-in scalars
    /// and containers define it structurally (containers fold their element
    /// ids transitively), and `#[derive(VarveBlock)]` computes it from the
    /// block identity plus every field's own `SCHEMA_ID`. Blocks therefore
    /// fingerprint their fields by resolved codec identity, and
    /// [`crate::FormatSpec::computed_schema_hash`] inherits that transitively.
    ///
    /// **Trust boundary.** The default `0` means "this codec declares no
    /// identity". A manual codec that leaves it at `0` is indistinguishable
    /// from any other identity-less codec with the same field type spelling,
    /// so every hand-written codec must declare a value that changes whenever
    /// its emitted bytes change. That requirement is enforced, not merely
    /// documented, wherever it can collide: `#[derive(VarveBlock)]` rejects at
    /// compile time *any* field whose codec resolves to `SCHEMA_ID == 0`,
    /// regardless of [`WireType`], and the crate-private `container_schema_id`
    /// helper propagates the
    /// zero outward so wrapping an identity-less codec in `Option`, `Vec`, an
    /// array, a map or a tuple cannot launder it into an identity.
    const SCHEMA_ID: u64 = 0;

    fn encode_varve(&self, encoder: &mut Encoder) -> Result<()>;
}

pub trait VarveDecode: Sized {
    const WIRE_TYPE: WireType;

    /// Structural identity of the bytes this codec accepts; see
    /// [`VarveEncode::SCHEMA_ID`]. Derived block fingerprints fold both the
    /// encode and the decode identity of every field, so an asymmetric manual
    /// codec cannot pass itself off as its own mirror image.
    const SCHEMA_ID: u64 = 0;

    fn decode_varve(decoder: &mut Decoder<'_>) -> Result<Self>;
}

#[derive(Debug)]
pub struct Encoder {
    endian: Endian,
    output: Vec<u8>,
    max_len: Option<u64>,
    limit_resource: &'static str,
    overflow: Option<(u64, u64)>,
}

impl Encoder {
    pub fn new(endian: Endian) -> Self {
        Self {
            endian,
            output: Vec::new(),
            max_len: None,
            limit_resource: "encoded payload",
            overflow: None,
        }
    }

    pub(crate) fn new_limited(endian: Endian, max_len: u64, resource: &'static str) -> Self {
        Self {
            endian,
            output: Vec::new(),
            max_len: Some(max_len),
            limit_resource: resource,
            overflow: None,
        }
    }

    pub fn endian(&self) -> Endian {
        self.endian
    }

    pub fn into_inner(self) -> Vec<u8> {
        self.output
    }

    pub(crate) fn try_into_inner(self) -> Result<Vec<u8>> {
        if let Some((actual, limit)) = self.overflow {
            return Err(Error::LimitExceeded {
                resource: self.limit_resource,
                actual,
                limit,
            });
        }
        Ok(self.output)
    }

    /// Encodes a nested value into its own buffer while inheriting this
    /// encoder's remaining output budget (DEF-02).
    ///
    /// A nested field encoded through this method can never buffer past the
    /// limit its parent writer entry point imposed: the child encoder is
    /// capped at the parent's remaining budget and reports the same typed
    /// [`Error::LimitExceeded`] resource on overflow. An unlimited parent
    /// yields an unlimited child, matching [`encode_to_vec`].
    pub fn encode_nested_to_vec<T: VarveEncode>(&self, value: &T) -> Result<Vec<u8>> {
        let Some(max_len) = self.max_len else {
            return encode_to_vec(value, self.endian);
        };
        let remaining = max_len.saturating_sub(self.output.len() as u64);
        encode_to_vec_limited(value, self.endian, remaining, self.limit_resource)
    }

    pub fn write_all(&mut self, bytes: &[u8]) {
        if self.max_len.is_none() {
            self.output.extend_from_slice(bytes);
            return;
        }
        if self.overflow.is_some() {
            return;
        }
        let actual = (self.output.len() as u64).checked_add(bytes.len() as u64);
        if let Some(limit) = self.max_len
            && actual.is_none_or(|actual| actual > limit)
        {
            self.overflow = Some((actual.unwrap_or(u64::MAX), limit));
            return;
        }
        self.output.extend_from_slice(bytes);
    }

    pub fn write_u8(&mut self, value: u8) {
        self.write_all(&[value]);
    }

    pub fn write_u16(&mut self, value: u16) {
        match self.endian {
            Endian::Little => self.write_all(&value.to_le_bytes()),
            Endian::Big => self.write_all(&value.to_be_bytes()),
        }
    }

    pub fn write_u32(&mut self, value: u32) {
        match self.endian {
            Endian::Little => self.write_all(&value.to_le_bytes()),
            Endian::Big => self.write_all(&value.to_be_bytes()),
        }
    }

    pub fn write_u64(&mut self, value: u64) {
        match self.endian {
            Endian::Little => self.write_all(&value.to_le_bytes()),
            Endian::Big => self.write_all(&value.to_be_bytes()),
        }
    }

    pub fn write_u128(&mut self, value: u128) {
        match self.endian {
            Endian::Little => self.write_all(&value.to_le_bytes()),
            Endian::Big => self.write_all(&value.to_be_bytes()),
        }
    }
}

#[derive(Debug)]
pub struct Decoder<'a> {
    endian: Endian,
    input: &'a [u8],
    position: usize,
    small_field_ids: u64,
    large_field_ids: Option<HashSet<u32>>,
    materialization_limit: u64,
    materialization_remaining: u64,
}

/// Materialization charged per distinct variable field id above the small-id
/// mask (RES-01). A `HashSet<u32>` bucket costs four payload bytes plus one
/// control byte at a 7/8 load factor; eight bytes over-approximates that so
/// the charge can never undercount the reservation it guards.
const FIELD_ID_SET_ENTRY_BYTES: u64 = 2 * size_of::<u32>() as u64;

impl<'a> Decoder<'a> {
    pub const STANDARD_MATERIALIZATION_LIMIT: u64 = 1024 * 1024 * 1024;

    pub fn new(input: &'a [u8], endian: Endian) -> Self {
        Self::new_limited(input, endian, Self::STANDARD_MATERIALIZATION_LIMIT)
    }

    pub fn new_limited(input: &'a [u8], endian: Endian, materialization_limit: u64) -> Self {
        Self {
            endian,
            input,
            position: 0,
            small_field_ids: 0,
            large_field_ids: None,
            materialization_limit,
            materialization_remaining: materialization_limit,
        }
    }

    pub fn decode_from_slice_limited<T: VarveDecode>(
        input: &'a [u8],
        endian: Endian,
        materialization_limit: u64,
    ) -> Result<T> {
        let mut decoder = Self::new_limited(input, endian, materialization_limit);
        decoder.decode_complete()
    }

    pub fn decode_from_slice_accounted<T: VarveDecode>(
        input: &'a [u8],
        endian: Endian,
        materialization_remaining: &mut u64,
    ) -> Result<T> {
        let mut decoder = Self::new_limited(input, endian, *materialization_remaining);
        let result = decoder.decode_complete();
        *materialization_remaining = decoder.materialization_remaining;
        result
    }

    pub fn endian(&self) -> Endian {
        self.endian
    }

    pub fn remaining(&self) -> usize {
        self.input.len().saturating_sub(self.position)
    }

    pub fn materialization_remaining(&self) -> u64 {
        self.materialization_remaining
    }

    pub fn decode_nested<T: VarveDecode>(&mut self, input: &[u8]) -> Result<T> {
        let mut nested = Decoder::new_limited(input, self.endian, self.materialization_remaining);
        let result = nested.decode_complete();
        self.materialization_remaining = nested.materialization_remaining;
        result
    }

    pub fn charge_materialization(&mut self, bytes: u64, resource: &'static str) -> Result<()> {
        let consumed = self
            .materialization_limit
            .checked_sub(self.materialization_remaining)
            .ok_or(Error::ResourceArithmeticOverflow { resource })?;
        let actual = consumed
            .checked_add(bytes)
            .ok_or(Error::ResourceArithmeticOverflow { resource })?;
        if actual > self.materialization_limit {
            return Err(Error::LimitExceeded {
                resource,
                actual,
                limit: self.materialization_limit,
            });
        }
        self.materialization_remaining -= bytes;
        Ok(())
    }

    fn decode_complete<T: VarveDecode>(&mut self) -> Result<T> {
        let value = T::decode_varve(self)?;
        if self.remaining() != 0 {
            return Err(Error::TrailingBytes {
                remaining: self.remaining(),
            });
        }
        Ok(value)
    }

    pub fn read_exact(&mut self, len: usize) -> Result<&'a [u8]> {
        let end = self.position.checked_add(len).ok_or(Error::UnexpectedEof)?;
        if end > self.input.len() {
            return Err(Error::UnexpectedEof);
        }
        let bytes = &self.input[self.position..end];
        self.position = end;
        Ok(bytes)
    }

    pub fn read_u8(&mut self) -> Result<u8> {
        Ok(self.read_exact(1)?[0])
    }

    pub fn read_u16(&mut self) -> Result<u16> {
        let mut bytes = [0; 2];
        bytes.copy_from_slice(self.read_exact(2)?);
        Ok(match self.endian {
            Endian::Little => u16::from_le_bytes(bytes),
            Endian::Big => u16::from_be_bytes(bytes),
        })
    }

    pub fn read_u32(&mut self) -> Result<u32> {
        let mut bytes = [0; 4];
        bytes.copy_from_slice(self.read_exact(4)?);
        Ok(match self.endian {
            Endian::Little => u32::from_le_bytes(bytes),
            Endian::Big => u32::from_be_bytes(bytes),
        })
    }

    pub fn read_u64(&mut self) -> Result<u64> {
        let mut bytes = [0; 8];
        bytes.copy_from_slice(self.read_exact(8)?);
        Ok(match self.endian {
            Endian::Little => u64::from_le_bytes(bytes),
            Endian::Big => u64::from_be_bytes(bytes),
        })
    }

    pub fn read_u128(&mut self) -> Result<u128> {
        let mut bytes = [0; 16];
        bytes.copy_from_slice(self.read_exact(16)?);
        Ok(match self.endian {
            Endian::Little => u128::from_le_bytes(bytes),
            Endian::Big => u128::from_be_bytes(bytes),
        })
    }

    pub fn read_len(&mut self) -> Result<usize> {
        let value = self.read_u64()?;
        usize::try_from(value).map_err(|_| Error::LengthOverflow { value })
    }

    fn preflight_count(
        &mut self,
        len: usize,
        minimum_wire_bytes: usize,
        materialized_entry_bytes: usize,
        resource: &'static str,
    ) -> Result<()> {
        if minimum_wire_bytes != 0 && len > self.remaining() / minimum_wire_bytes {
            return Err(Error::UnexpectedEof);
        }
        if minimum_wire_bytes == 0
            && u64::try_from(len).unwrap_or(u64::MAX) > self.materialization_remaining
        {
            return Err(Error::InvalidCanonicalEncoding(
                "collection count cannot make bounded input progress",
            ));
        }
        let charged_entry_bytes = materialized_entry_bytes.max(1);
        let bytes = len
            .checked_mul(charged_entry_bytes)
            .ok_or(Error::ResourceArithmeticOverflow { resource })?;
        self.charge_materialization(
            u64::try_from(bytes).map_err(|_| Error::ResourceArithmeticOverflow { resource })?,
            resource,
        )
    }

    /// Wire-length screen shared by both map codecs.
    ///
    /// Returns the declared count unchanged once the remaining input could
    /// physically carry it; the caller then charges its own materialization
    /// model on top.
    fn screen_map_wire_length<K: VarveDecode, V: VarveDecode>(
        &self,
        len: usize,
        resource: &'static str,
    ) -> Result<()> {
        let key_bytes = minimum_wire_size(K::WIRE_TYPE);
        let entry_bytes = key_bytes.saturating_add(minimum_wire_size(V::WIRE_TYPE));
        let minimum_wire_bytes = len
            .checked_mul(entry_bytes)
            .ok_or(Error::ResourceArithmeticOverflow { resource })?;
        if minimum_wire_bytes > self.remaining() {
            return Err(Error::UnexpectedEof);
        }
        Ok(())
    }

    /// Preflight for a node-per-entry map (`BTreeMap`), whose storage really is
    /// driven by the entries it has already accepted.
    ///
    /// API3-04: the charge used to be `len * size_of::<(K, V)>()`, which
    /// treats a `BTreeMap` as if it stored entries packed end to end. It does
    /// not. A std B-tree node carries a fixed-capacity array - it allocates
    /// room for `2B - 1 = 11` entries whatever its fill - behind a header
    /// (parent pointer, parent index, length), and internal nodes additionally
    /// carry `2B = 12` child pointers. A node is only guaranteed to hold
    /// `B - 1 = 5` entries, so per *live* entry the array alone can cost
    /// `11/5 = 2.2x` the naive model before any header is counted.
    ///
    /// The model below is deliberately an over-estimate, not a measurement:
    /// [`BTREE_ENTRY_SLOT_FACTOR`] covers the fixed-capacity array at minimum
    /// fill plus the internal-node fan-out, and [`BTREE_ENTRY_HEADER_BYTES`]
    /// covers node headers and child pointers amortised over the same minimum
    /// fill. `size_of::<(K, V)>()` is itself an upper bound on the two
    /// separate `K` and `V` arrays a node really holds. This charge is smaller
    /// than the hash-table charge in relative terms because a `BTreeMap` only
    /// grows per *accepted* entry, so the input must already carry at least
    /// one wire byte per entry.
    fn preflight_map_count<K: VarveDecode, V: VarveDecode>(
        &mut self,
        len: usize,
        resource: &'static str,
    ) -> Result<()> {
        self.screen_map_wire_length::<K, V>(len, resource)?;
        let entry_bytes = size_of::<(K, V)>()
            .max(1)
            .checked_mul(BTREE_ENTRY_SLOT_FACTOR)
            .and_then(|slots| slots.checked_add(BTREE_ENTRY_HEADER_BYTES))
            .ok_or(Error::ResourceArithmeticOverflow { resource })?;
        self.preflight_count(len, 0, entry_bytes, resource)
    }

    /// Preflight for an open-addressed hash table (`HashMap`), whose storage is
    /// driven by the *declared* count long before any entry is validated
    /// (SAFE-01).
    ///
    /// `preflight_count` floors a zero-byte entry at a one-byte charge, which
    /// is a wild under-estimate for a hash table: a `HashMap<(), ()>` pays a
    /// control byte per bucket and rounds the bucket count up to a power of
    /// two, so a one-byte-per-entry charge let the standard 1 GiB budget admit
    /// close to one billion declared entries and hand `try_reserve` a
    /// multi-gigabyte table. Charge [`hash_table_reservation_bytes`] instead,
    /// which over-approximates what hashbrown actually allocates.
    fn preflight_hash_map_count<K: VarveDecode, V: VarveDecode>(
        &mut self,
        len: usize,
        resource: &'static str,
    ) -> Result<()> {
        self.screen_map_wire_length::<K, V>(len, resource)?;
        // Preserved from `preflight_count`: a count that no amount of bounded
        // input can make progress against is a canonical-encoding fault, not a
        // budget fault, and rejecting it here keeps hostile `u64::MAX` counts
        // out of the arithmetic below.
        if u64::try_from(len).unwrap_or(u64::MAX) > self.materialization_remaining {
            return Err(Error::InvalidCanonicalEncoding(
                "collection count cannot make bounded input progress",
            ));
        }
        let bytes = hash_table_reservation_bytes::<(K, V)>(len).ok_or(Error::LimitExceeded {
            resource,
            actual: u64::MAX,
            limit: self.materialization_limit,
        })?;
        self.charge_materialization(bytes, resource)
    }

    fn note_field_id(&mut self, field_id: u32) -> Result<()> {
        if field_id < u64::BITS {
            let mask = 1u64 << field_id;
            if self.small_field_ids & mask != 0 {
                return Err(Error::InvalidCanonicalEncoding("duplicate variable field"));
            }
            self.small_field_ids |= mask;
            return Ok(());
        }

        if self
            .large_field_ids
            .as_ref()
            .is_some_and(|field_ids| field_ids.contains(&field_id))
        {
            return Err(Error::InvalidCanonicalEncoding("duplicate variable field"));
        }
        // RES-01: every other decoder-owned container charges the
        // materialization budget before it reserves. Duplicate detection for
        // ids above the small-id mask is the one decoder-owned set whose
        // growth is driven purely by attacker-chosen field ids, so charge
        // before reserving rather than after.
        self.charge_materialization(FIELD_ID_SET_ENTRY_BYTES, "variable field ids")?;
        let field_ids = self.large_field_ids.get_or_insert_with(HashSet::new);
        let requested = u64::try_from(field_ids.len())
            .unwrap_or(u64::MAX)
            .saturating_add(1)
            .saturating_mul(4);
        field_ids
            .try_reserve(1)
            .map_err(|_| Error::AllocationFailed {
                resource: "variable field ids",
                requested,
            })?;
        field_ids.insert(field_id);
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FieldHeader {
    pub field_id: u32,
    pub wire_type: WireType,
    pub payload_len: u64,
}

pub fn write_field(
    encoder: &mut Encoder,
    field_id: u32,
    wire_type: WireType,
    payload: &[u8],
) -> Result<()> {
    encoder.write_u32(field_id);
    encoder.write_u16(wire_type as u16);
    encoder.write_u16(0);
    encoder.write_u64(payload.len() as u64);
    encoder.write_all(payload);
    Ok(())
}

pub fn read_field_header(decoder: &mut Decoder<'_>) -> Result<FieldHeader> {
    let field_id = decoder.read_u32()?;
    let wire_type = decoder.read_u16()?;
    let flags = decoder.read_u16()?;
    let payload_len = decoder.read_u64()?;
    if flags != 0 {
        return Err(Error::InvalidCanonicalEncoding(
            "variable field flags must be zero",
        ));
    }
    let wire_type = WireType::from_u16(wire_type).ok_or(Error::UnknownWireType(wire_type))?;
    let payload_len_usize: usize =
        usize::try_from(payload_len).map_err(|_| Error::LengthOverflow { value: payload_len })?;
    if payload_len_usize > decoder.remaining() {
        return Err(Error::UnexpectedEof);
    }
    decoder.note_field_id(field_id)?;
    Ok(FieldHeader {
        field_id,
        wire_type,
        payload_len,
    })
}

pub fn encode_to_vec<T: VarveEncode>(value: &T, endian: Endian) -> Result<Vec<u8>> {
    let mut encoder = Encoder::new(endian);
    value.encode_varve(&mut encoder)?;
    Ok(encoder.into_inner())
}

pub(crate) fn encode_to_vec_limited<T: VarveEncode>(
    value: &T,
    endian: Endian,
    max_len: u64,
    resource: &'static str,
) -> Result<Vec<u8>> {
    let mut encoder = Encoder::new_limited(endian, max_len, resource);
    value.encode_varve(&mut encoder)?;
    encoder.try_into_inner()
}

pub fn decode_from_slice<T: VarveDecode>(bytes: &[u8], endian: Endian) -> Result<T> {
    Decoder::decode_from_slice_limited(bytes, endian, Decoder::STANDARD_MATERIALIZATION_LIMIT)
}

impl VarveEncode for () {
    const WIRE_TYPE: WireType = WireType::Unit;
    const SCHEMA_ID: u64 = leaf_schema_id(b"unit");

    fn encode_varve(&self, _encoder: &mut Encoder) -> Result<()> {
        Ok(())
    }
}

impl VarveDecode for () {
    const WIRE_TYPE: WireType = WireType::Unit;
    const SCHEMA_ID: u64 = leaf_schema_id(b"unit");

    fn decode_varve(_decoder: &mut Decoder<'_>) -> Result<Self> {
        Ok(())
    }
}

macro_rules! unsigned {
    ($ty:ty, $wire:ident, $write:ident, $read:ident) => {
        impl VarveEncode for $ty {
            const WIRE_TYPE: WireType = WireType::$wire;
            const SCHEMA_ID: u64 = leaf_schema_id(stringify!($ty).as_bytes());

            fn encode_varve(&self, encoder: &mut Encoder) -> Result<()> {
                encoder.$write(*self);
                Ok(())
            }
        }

        impl VarveDecode for $ty {
            const WIRE_TYPE: WireType = WireType::$wire;
            const SCHEMA_ID: u64 = leaf_schema_id(stringify!($ty).as_bytes());

            fn decode_varve(decoder: &mut Decoder<'_>) -> Result<Self> {
                decoder.$read()
            }
        }
    };
}

macro_rules! signed {
    ($ty:ty, $wire:ident, $uty:ty, $write:ident, $read:ident) => {
        impl VarveEncode for $ty {
            const WIRE_TYPE: WireType = WireType::$wire;
            const SCHEMA_ID: u64 = leaf_schema_id(stringify!($ty).as_bytes());

            fn encode_varve(&self, encoder: &mut Encoder) -> Result<()> {
                encoder.$write(*self as $uty);
                Ok(())
            }
        }

        impl VarveDecode for $ty {
            const WIRE_TYPE: WireType = WireType::$wire;
            const SCHEMA_ID: u64 = leaf_schema_id(stringify!($ty).as_bytes());

            fn decode_varve(decoder: &mut Decoder<'_>) -> Result<Self> {
                Ok(decoder.$read()? as Self)
            }
        }
    };
}

unsigned!(u8, U8, write_u8, read_u8);
unsigned!(u16, U16, write_u16, read_u16);
unsigned!(u32, U32, write_u32, read_u32);
unsigned!(u64, U64, write_u64, read_u64);
unsigned!(u128, U128, write_u128, read_u128);
signed!(i8, I8, u8, write_u8, read_u8);
signed!(i16, I16, u16, write_u16, read_u16);
signed!(i32, I32, u32, write_u32, read_u32);
signed!(i64, I64, u64, write_u64, read_u64);
signed!(i128, I128, u128, write_u128, read_u128);

impl VarveEncode for bool {
    const WIRE_TYPE: WireType = WireType::Bool;
    const SCHEMA_ID: u64 = leaf_schema_id(b"bool");

    fn encode_varve(&self, encoder: &mut Encoder) -> Result<()> {
        encoder.write_u8(u8::from(*self));
        Ok(())
    }
}

impl VarveDecode for bool {
    const WIRE_TYPE: WireType = WireType::Bool;
    const SCHEMA_ID: u64 = leaf_schema_id(b"bool");

    fn decode_varve(decoder: &mut Decoder<'_>) -> Result<Self> {
        match decoder.read_u8()? {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err(Error::InvalidCanonicalEncoding(
                "boolean value must be 0 or 1",
            )),
        }
    }
}

impl VarveEncode for f32 {
    const WIRE_TYPE: WireType = WireType::F32;
    const SCHEMA_ID: u64 = leaf_schema_id(b"f32");

    fn encode_varve(&self, encoder: &mut Encoder) -> Result<()> {
        encoder.write_u32(self.to_bits());
        Ok(())
    }
}

impl VarveDecode for f32 {
    const WIRE_TYPE: WireType = WireType::F32;
    const SCHEMA_ID: u64 = leaf_schema_id(b"f32");

    fn decode_varve(decoder: &mut Decoder<'_>) -> Result<Self> {
        Ok(Self::from_bits(decoder.read_u32()?))
    }
}

impl VarveEncode for f64 {
    const WIRE_TYPE: WireType = WireType::F64;
    const SCHEMA_ID: u64 = leaf_schema_id(b"f64");

    fn encode_varve(&self, encoder: &mut Encoder) -> Result<()> {
        encoder.write_u64(self.to_bits());
        Ok(())
    }
}

impl VarveDecode for f64 {
    const WIRE_TYPE: WireType = WireType::F64;
    const SCHEMA_ID: u64 = leaf_schema_id(b"f64");

    fn decode_varve(decoder: &mut Decoder<'_>) -> Result<Self> {
        Ok(Self::from_bits(decoder.read_u64()?))
    }
}

impl VarveEncode for Vec<u8> {
    const WIRE_TYPE: WireType = WireType::Bytes;
    const SCHEMA_ID: u64 = leaf_schema_id(b"bytes");

    fn encode_varve(&self, encoder: &mut Encoder) -> Result<()> {
        encoder.write_u64(self.len() as u64);
        encoder.write_all(self);
        Ok(())
    }
}

impl VarveDecode for Vec<u8> {
    const WIRE_TYPE: WireType = WireType::Bytes;
    const SCHEMA_ID: u64 = leaf_schema_id(b"bytes");

    fn decode_varve(decoder: &mut Decoder<'_>) -> Result<Self> {
        let len = decoder.read_len()?;
        if len > decoder.remaining() {
            return Err(Error::UnexpectedEof);
        }
        decoder.charge_materialization(len as u64, "byte vector")?;
        let bytes = decoder.read_exact(len)?;
        let mut value = Vec::new();
        value
            .try_reserve_exact(len)
            .map_err(|_| Error::AllocationFailed {
                resource: "byte vector",
                requested: len as u64,
            })?;
        value.extend_from_slice(bytes);
        Ok(value)
    }
}

impl VarveEncode for String {
    const WIRE_TYPE: WireType = WireType::String;
    const SCHEMA_ID: u64 = leaf_schema_id(b"string");

    fn encode_varve(&self, encoder: &mut Encoder) -> Result<()> {
        encoder.write_u64(self.len() as u64);
        encoder.write_all(self.as_bytes());
        Ok(())
    }
}

impl VarveDecode for String {
    const WIRE_TYPE: WireType = WireType::String;
    const SCHEMA_ID: u64 = leaf_schema_id(b"string");

    fn decode_varve(decoder: &mut Decoder<'_>) -> Result<Self> {
        let len = decoder.read_len()?;
        if len > decoder.remaining() {
            return Err(Error::UnexpectedEof);
        }
        decoder.charge_materialization(len as u64, "string")?;
        let bytes = decoder.read_exact(len)?;
        let mut value = Vec::new();
        value
            .try_reserve_exact(len)
            .map_err(|_| Error::AllocationFailed {
                resource: "string",
                requested: len as u64,
            })?;
        value.extend_from_slice(bytes);
        String::from_utf8(value).map_err(|_| Error::InvalidUtf8)
    }
}

impl<T> VarveEncode for Option<T>
where
    T: VarveEncode,
{
    const WIRE_TYPE: WireType = WireType::Nested;
    const SCHEMA_ID: u64 = container_schema_id(b"option", &[T::SCHEMA_ID]);

    fn encode_varve(&self, encoder: &mut Encoder) -> Result<()> {
        match self {
            Some(value) => {
                true.encode_varve(encoder)?;
                value.encode_varve(encoder)?;
            }
            None => {
                false.encode_varve(encoder)?;
            }
        }
        Ok(())
    }
}

impl<T> VarveDecode for Option<T>
where
    T: VarveDecode,
{
    const WIRE_TYPE: WireType = WireType::Nested;
    const SCHEMA_ID: u64 = container_schema_id(b"option", &[T::SCHEMA_ID]);

    fn decode_varve(decoder: &mut Decoder<'_>) -> Result<Self> {
        if bool::decode_varve(decoder)? {
            Ok(Some(T::decode_varve(decoder)?))
        } else {
            Ok(None)
        }
    }
}

impl<T, const N: usize> VarveEncode for [T; N]
where
    T: VarveEncode,
{
    const WIRE_TYPE: WireType = WireType::Seq;
    const SCHEMA_ID: u64 = container_schema_id_with_arity(b"array", N as u64, &[T::SCHEMA_ID]);

    fn encode_varve(&self, encoder: &mut Encoder) -> Result<()> {
        for value in self {
            value.encode_varve(encoder)?;
        }
        Ok(())
    }
}

/// Ceiling, in bytes, on the transient `[Option<T>; N]` a fixed-array decode
/// may build on the stack.
///
/// Above it the decode keeps the fallible heap reservation it has always used.
/// A format author's declared array length must not be able to turn into an
/// unbounded stack frame: that would trade a bounded, fallible heap allocation
/// for an unbounded one that cannot be refused, which is the opposite of what
/// this crate's allocation discipline exists for.
const ARRAY_STACK_DECODE_BUDGET: usize = 16 * 1024;

/// Decodes `[T; N]` through a stack `[Option<T>; N]`, allocating nothing.
///
/// `#[inline(never)]` is load-bearing here, not a hint. The budget test in
/// `<[T; N] as VarveDecode>::decode_varve` is an ordinary runtime `if` over
/// const-foldable operands, so were this body inlined the `[Option<T>; N]` slot
/// would sit in that function's frame whichever branch runs — and a debug
/// build, which is how `cargo test` runs, folds nothing away. Kept as its own
/// non-inlinable function the slot exists only while this function is on the
/// stack, which for an above-budget array is never, in debug and release
/// alike.
///
/// `Option<T>` rather than `MaybeUninit<T>` is what keeps the crate's most
/// adversarially fuzzed surface free of `unsafe`: an early `?` drops the
/// partially filled array through its ordinary `Drop`, the initialised prefix
/// as `Some` and the rest as `None`.
#[inline(never)]
fn decode_array_on_stack<T, const N: usize>(decoder: &mut Decoder<'_>) -> Result<[T; N]>
where
    T: VarveDecode,
{
    let mut slots: [Option<T>; N] = [const { None }; N];
    for slot in &mut slots {
        *slot = Some(T::decode_varve(decoder)?);
    }
    Ok(slots.map(|value| value.expect("every array slot was filled by the loop above")))
}

/// Decodes `[T; N]` through one fallible heap reservation — the behaviour every
/// array decode had before the stack path, kept verbatim for arrays above
/// `ARRAY_STACK_DECODE_BUDGET`.
fn decode_array_on_heap<T, const N: usize>(decoder: &mut Decoder<'_>) -> Result<[T; N]>
where
    T: VarveDecode,
{
    let mut values = Vec::new();
    values
        .try_reserve_exact(N)
        .map_err(|_| Error::AllocationFailed {
            resource: "array",
            requested: allocation_request::<T>(N),
        })?;
    for _ in 0..N {
        values.push(T::decode_varve(decoder)?);
    }
    values.try_into().map_err(|_| Error::UnexpectedEof)
}

impl<T, const N: usize> VarveDecode for [T; N]
where
    T: VarveDecode,
{
    const WIRE_TYPE: WireType = WireType::Seq;
    const SCHEMA_ID: u64 = container_schema_id_with_arity(b"array", N as u64, &[T::SCHEMA_ID]);

    fn decode_varve(decoder: &mut Decoder<'_>) -> Result<Self> {
        // Unchanged: this is the EOF screen and the materialization charge, and
        // the charge stays at `size_of::<T>()` rather than
        // `size_of::<Option<T>>()`. The budget models retained heap
        // materialization; the `Option` padding is transient stack already
        // bounded by `ARRAY_STACK_DECODE_BUDGET`, and raising the charge would
        // turn previously accepted decodes into `LimitExceeded` for niche-less
        // `T` under a tight `max_materialized_bytes`.
        decoder.preflight_count(N, minimum_wire_size(T::WIRE_TYPE), size_of::<T>(), "array")?;
        if size_of::<Option<T>>().saturating_mul(N) <= ARRAY_STACK_DECODE_BUDGET {
            decode_array_on_stack(decoder)
        } else {
            decode_array_on_heap(decoder)
        }
    }
}

impl<K, V> VarveEncode for BTreeMap<K, V>
where
    K: VarveEncode + Ord,
    V: VarveEncode,
{
    const WIRE_TYPE: WireType = WireType::Nested;
    const SCHEMA_ID: u64 = container_schema_id(b"btreemap", &[K::SCHEMA_ID, V::SCHEMA_ID]);

    fn encode_varve(&self, encoder: &mut Encoder) -> Result<()> {
        encoder.write_u64(self.len() as u64);
        for (key, value) in self {
            key.encode_varve(encoder)?;
            value.encode_varve(encoder)?;
        }
        Ok(())
    }
}

impl<K, V> VarveDecode for BTreeMap<K, V>
where
    K: VarveDecode + Ord,
    V: VarveDecode,
{
    const WIRE_TYPE: WireType = WireType::Nested;
    const SCHEMA_ID: u64 = container_schema_id(b"btreemap", &[K::SCHEMA_ID, V::SCHEMA_ID]);

    fn decode_varve(decoder: &mut Decoder<'_>) -> Result<Self> {
        let len = decoder.read_len()?;
        decoder.preflight_map_count::<K, V>(len, "BTreeMap entries")?;
        let mut values = BTreeMap::new();
        if len == 0 {
            return Ok(values);
        }

        let mut pending_key = K::decode_varve(decoder)?;
        for _ in 1..len {
            let pending_value = V::decode_varve(decoder)?;
            let next_key = K::decode_varve(decoder)?;
            match pending_key.cmp(&next_key) {
                Ordering::Less => {}
                Ordering::Equal => {
                    return Err(Error::InvalidCanonicalEncoding("duplicate BTreeMap key"));
                }
                Ordering::Greater => {
                    return Err(Error::InvalidCanonicalEncoding(
                        "BTreeMap keys must be strictly increasing",
                    ));
                }
            }
            if values.insert(pending_key, pending_value).is_some() {
                return Err(Error::InvalidCanonicalEncoding("duplicate BTreeMap key"));
            }
            pending_key = next_key;
        }

        let pending_value = V::decode_varve(decoder)?;
        if values.insert(pending_key, pending_value).is_some() {
            return Err(Error::InvalidCanonicalEncoding("duplicate BTreeMap key"));
        }
        Ok(values)
    }
}

impl<K, V> VarveEncode for HashMap<K, V>
where
    K: VarveEncode + Ord + Eq + Hash,
    V: VarveEncode,
{
    const WIRE_TYPE: WireType = WireType::Nested;
    const SCHEMA_ID: u64 = container_schema_id(b"hashmap", &[K::SCHEMA_ID, V::SCHEMA_ID]);

    fn encode_varve(&self, encoder: &mut Encoder) -> Result<()> {
        let mut entries: Vec<(&K, &V)> = self.iter().collect();
        entries.sort_by_key(|(key, _)| *key);
        encoder.write_u64(entries.len() as u64);
        for (key, value) in entries {
            key.encode_varve(encoder)?;
            value.encode_varve(encoder)?;
        }
        Ok(())
    }
}

impl<K, V> VarveDecode for HashMap<K, V>
where
    K: VarveDecode + Ord + Eq + Hash,
    V: VarveDecode,
{
    const WIRE_TYPE: WireType = WireType::Nested;
    const SCHEMA_ID: u64 = container_schema_id(b"hashmap", &[K::SCHEMA_ID, V::SCHEMA_ID]);

    fn decode_varve(decoder: &mut Decoder<'_>) -> Result<Self> {
        let len = decoder.read_len()?;
        decoder.preflight_hash_map_count::<K, V>(len, "HashMap entries")?;
        let mut values = HashMap::new();
        // SAFE-01: reserve only a bounded prefix of the declared count, so a
        // hostile count cannot materialize a table before a single entry has
        // been proven decodable. The charge above already bounds the table this
        // map can reach; this bounds what it can reach *on a claim alone*.
        let preallocated = len.min(MAP_PREALLOCATION_ENTRIES);
        values
            .try_reserve(preallocated)
            .map_err(|_| Error::AllocationFailed {
                resource: "HashMap entries",
                requested: allocation_request::<(K, V)>(preallocated),
            })?;
        if len == 0 {
            return Ok(values);
        }

        let mut pending_key = K::decode_varve(decoder)?;
        for _ in 1..len {
            let pending_value = V::decode_varve(decoder)?;
            let next_key = K::decode_varve(decoder)?;
            match pending_key.cmp(&next_key) {
                Ordering::Less => {}
                Ordering::Equal => {
                    return Err(Error::InvalidCanonicalEncoding("duplicate HashMap key"));
                }
                Ordering::Greater => {
                    return Err(Error::InvalidCanonicalEncoding(
                        "HashMap keys must be strictly increasing",
                    ));
                }
            }
            if values.insert(pending_key, pending_value).is_some() {
                return Err(Error::InvalidCanonicalEncoding("duplicate HashMap key"));
            }
            pending_key = next_key;
        }

        let pending_value = V::decode_varve(decoder)?;
        if values.insert(pending_key, pending_value).is_some() {
            return Err(Error::InvalidCanonicalEncoding("duplicate HashMap key"));
        }
        Ok(values)
    }
}

macro_rules! vec_seq_codec {
    ($(($ty:ty, $minimum_wire_bytes:expr)),+ $(,)?) => {
        $(
            impl VarveEncode for Vec<$ty> {
                const WIRE_TYPE: WireType = WireType::Seq;
                const SCHEMA_ID: u64 =
                    container_schema_id(b"seq", &[<$ty as VarveEncode>::SCHEMA_ID]);

                fn encode_varve(&self, encoder: &mut Encoder) -> Result<()> {
                    encoder.write_u64(self.len() as u64);
                    for value in self {
                        value.encode_varve(encoder)?;
                    }
                    Ok(())
                }
            }

            impl VarveDecode for Vec<$ty> {
                const WIRE_TYPE: WireType = WireType::Seq;
                const SCHEMA_ID: u64 =
                    container_schema_id(b"seq", &[<$ty as VarveDecode>::SCHEMA_ID]);

                fn decode_varve(decoder: &mut Decoder<'_>) -> Result<Self> {
                    let len = decoder.read_len()?;
                    decoder.preflight_count(
                        len,
                        $minimum_wire_bytes,
                        size_of::<$ty>(),
                        "sequence entries",
                    )?;
                    let mut values = Vec::new();
                    values.try_reserve_exact(len).map_err(|_| Error::AllocationFailed {
                        resource: "sequence entries",
                        requested: allocation_request::<$ty>(len),
                    })?;
                    for _ in 0..len {
                        values.push(<$ty>::decode_varve(decoder)?);
                    }
                    Ok(values)
                }
            }
        )+
    }
}

vec_seq_codec!(
    (bool, 1),
    (i8, 1),
    (u16, 2),
    (i16, 2),
    (u32, 4),
    (i32, 4),
    (u64, 8),
    (i64, 8),
    (u128, 16),
    (i128, 16),
    (f32, 4),
    (f64, 8),
    (String, 8)
);

const fn minimum_wire_size(wire_type: WireType) -> usize {
    match wire_type {
        WireType::Unit => 0,
        WireType::Bool | WireType::U8 | WireType::I8 => 1,
        WireType::U16 | WireType::I16 => 2,
        WireType::U32 | WireType::I32 | WireType::F32 => 4,
        WireType::U64 | WireType::I64 | WireType::F64 => 8,
        WireType::U128 | WireType::I128 => 16,
        WireType::Bytes | WireType::String => 8,
        WireType::Seq | WireType::Nested => 0,
    }
}

fn allocation_request<T>(len: usize) -> u64 {
    u64::try_from(len.saturating_mul(size_of::<T>())).unwrap_or(u64::MAX)
}

/// Control-group width the reservation model assumes for hashbrown, in bytes.
///
/// This single constant governs three separate parts of the real allocation,
/// all of which scale with `Group::WIDTH`:
///
/// * the trailing duplicated control group appended after the control array;
/// * the minimum bucket count of a small table (SAFE2-01: hashbrown rounds a
///   small table with a one-byte-or-smaller entry up to a whole group so the
///   control array is never shorter than one SIMD load);
/// * the control array's alignment, which is `max(Group::WIDTH, align_of::<T>())`
///   and therefore bounds the padding inserted between the bucket array and the
///   control array.
///
/// Shipping hashbrown uses a 16-byte SSE2 group on x86-64 and an 8-byte group
/// on NEON and the generic fallback, so the true width on every target this
/// project supports today is at most 16. The model deliberately assumes **32**
/// instead: the value cannot be read from the standard library at compile time,
/// a wider group is the only way this model can silently start undercharging,
/// and one doubling buys immunity to a future 32-byte-group implementation at a
/// cost of at most a few tens of bytes per map plus a bounded over-charge on
/// tables of fewer than fifteen entries. Charging more than hashbrown allocates
/// is always sound here; charging less is the defect this constant exists to
/// prevent.
/// Entry-slot multiplier for the `BTreeMap` materialization model (API3-04).
///
/// A std B-tree node allocates a fixed array of `2B - 1 = 11` entry slots
/// whatever its fill, and is only guaranteed to hold `B - 1 = 5` of them, so
/// the array costs up to `11/5 = 2.2` slots per live entry. Internal nodes add
/// a further `1/(B - 1)` of the leaf population. Three is the next integer
/// above that bound and is chosen so a future change of `B` in the standard
/// library cannot silently turn the model into an under-charge.
const BTREE_ENTRY_SLOT_FACTOR: usize = 3;

/// Per-entry allowance for B-tree node headers and child pointers (API3-04).
///
/// A leaf node header is a parent pointer, a parent index and a length; an
/// internal node adds `2B = 12` child pointers. Amortised over the guaranteed
/// minimum fill of five entries that is under four bytes per entry for leaves
/// and under twenty for internal nodes, of which there are at most a fifth as
/// many. Sixteen bytes per entry covers both comfortably.
const BTREE_ENTRY_HEADER_BYTES: usize = 16;

const HASH_TABLE_GROUP_BYTES: u64 = 32;

/// Entries a map codec reserves for before it has validated a single entry
/// (SAFE-01).
///
/// The declared count is attacker-chosen, so a table sized from it up front is
/// an allocation granted on nothing but a claim. Reserving a small fixed prefix
/// instead keeps the common small-map decode allocation-exact while leaving a
/// hostile count nothing to pre-allocate; beyond this bound the table grows
/// through `insert`'s own amortized doubling, which performs no syscalls and no
/// per-entry allocation.
const MAP_PREALLOCATION_ENTRIES: usize = 1024;

/// Upper bound on the heap a `HashMap<K, V>` allocates when asked to hold `len`
/// entries, with `T = (K, V)`.
///
/// Derivation, from what hashbrown actually allocates:
///
/// * `try_reserve(len)` on an empty map requests a table with capacity `len`.
/// * Entries live at a maximum 7/8 load factor, so a large table needs at least
///   `ceil(len * 8 / 7)` buckets (hashbrown itself floors that division; the
///   ceiling used here can only round the model up).
/// * The bucket count is rounded **up to a power of two**, which can almost
///   double that figure again.
/// * Below that, hashbrown does **not** allocate less: it applies *small-table
///   capacity classes* with a hard floor. On the review toolchain (Rust 1.95,
///   x86-64) an empty `HashMap` asked for any capacity in `1..=14` reports
///   capacity 14, i.e. a **16-bucket** table, for a zero-sized or one-byte
///   entry; larger entries floor at 4 or 8 buckets. This is why the previous
///   comment here — that "small-table specializations only ever allocate less"
///   — was false, and why the old two-bucket model undercharged
///   `HashMap<(), ()>` by charging 19 bytes for a table that really costs 32
///   bytes of control storage alone (SAFE2-01/F-02).
/// * Every bucket owns one `T` *and* one control byte — the true floor is never
///   one byte per entry, not even for a zero-sized `T`.
/// * The allocation additionally carries one trailing duplicated control group,
///   plus the padding that aligns the control array, which is bounded by
///   `max(Group::WIDTH, align_of::<T>())` rather than by `align_of::<T>()`
///   alone (the old model charged only the latter and could undercharge a
///   low-alignment entry by up to a group).
///
/// So the charge is
///
/// ```text
/// buckets = max(next_power_of_two(ceil(len * 8 / 7)), HASH_TABLE_GROUP_BYTES)
/// bytes   = buckets * (size_of::<T>() + 1)
///         + HASH_TABLE_GROUP_BYTES
///         + max(HASH_TABLE_GROUP_BYTES, align_of::<T>())
/// ```
///
/// The bucket floor is *not* a transcription of one observed capacity class: it
/// is [`HASH_TABLE_GROUP_BYTES`], deliberately set to a wider control group
/// (32) than any supported toolchain uses (8 or 16). Every small-table class
/// hashbrown can pick is either a fixed small count (4 or 8 buckets) or one
/// whole control group, so on any implementation whose group is at most 32 wide
/// the class is at most 32 buckets and the floor dominates it — for `len <= 14`
/// this is decided by that floor alone, without hard-coding what any one build
/// was observed to do. From `len = 15` upwards the power-of-two term takes
/// over, and it is already at least 32 there.
///
/// The result is therefore an over-estimate at every capacity, MSRV 1.95 and
/// later stable toolchains alike, and the guard must never charge less than the
/// reservation it guards. `None` means the model overflowed `u64`, which the
/// caller must report as a budget rejection rather than attempt.
pub(crate) fn hash_table_reservation_bytes<T>(len: usize) -> Option<u64> {
    if len == 0 {
        return Some(0);
    }
    let len = u64::try_from(len).ok()?;
    let buckets = len
        .checked_mul(8)?
        .checked_add(6)?
        .checked_div(7)?
        .checked_next_power_of_two()?
        .max(HASH_TABLE_GROUP_BYTES);
    buckets
        .checked_mul((size_of::<T>() as u64).checked_add(1)?)?
        .checked_add(HASH_TABLE_GROUP_BYTES)?
        .checked_add(HASH_TABLE_GROUP_BYTES.max(align_of::<T>() as u64))
}

macro_rules! tuple_codec {
    ($($name:ident),+) => {
        impl<$($name),+> VarveEncode for ($($name,)+)
        where
            $($name: VarveEncode),+
        {
            const WIRE_TYPE: WireType = WireType::Nested;
            const SCHEMA_ID: u64 =
                container_schema_id(b"tuple", &[$(<$name as VarveEncode>::SCHEMA_ID),+]);

            #[allow(non_snake_case)]
            fn encode_varve(&self, encoder: &mut Encoder) -> Result<()> {
                let ($($name,)+) = self;
                $($name.encode_varve(encoder)?;)+
                Ok(())
            }
        }

        impl<$($name),+> VarveDecode for ($($name,)+)
        where
            $($name: VarveDecode),+
        {
            const WIRE_TYPE: WireType = WireType::Nested;
            const SCHEMA_ID: u64 =
                container_schema_id(b"tuple", &[$(<$name as VarveDecode>::SCHEMA_ID),+]);

            fn decode_varve(decoder: &mut Decoder<'_>) -> Result<Self> {
                Ok(($($name::decode_varve(decoder)?,)+))
            }
        }
    };
}

tuple_codec!(A, B);
tuple_codec!(A, B, C);
tuple_codec!(A, B, C, D);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn limited_encoder_stops_before_the_over_limit_allocation() {
        let mut encoder = Encoder::new_limited(Endian::Little, 8, "test payload");
        encoder.write_all(&[1; 8]);
        encoder.write_all(&[2; 1024]);
        encoder.write_all(&[3; 1024]);
        assert_eq!(encoder.output.len(), 8);
        assert!(matches!(
            encoder.try_into_inner(),
            Err(Error::LimitExceeded {
                resource: "test payload",
                actual: 1032,
                limit: 8,
            })
        ));
    }

    #[test]
    fn nested_encode_inherits_the_parent_remaining_budget() {
        let mut encoder = Encoder::new_limited(Endian::Little, 16, "test payload");
        encoder.write_all(&[1; 4]);
        // 12 bytes of budget remain; a Vec<u8> encodes as an 8-byte length
        // prefix plus its bytes, so a 4-byte nested value fits exactly ...
        let nested = encoder
            .encode_nested_to_vec(&vec![2u8; 4])
            .expect("nested value within the remaining budget");
        assert_eq!(nested.len(), 12);
        // ... while a value larger than the remaining budget is refused with
        // the parent's typed resource before it is buffered.
        let error = encoder
            .encode_nested_to_vec(&vec![3u8; 1024])
            .expect_err("nested value beyond the remaining budget");
        assert!(matches!(
            error,
            Error::LimitExceeded {
                resource: "test payload",
                limit: 12,
                ..
            }
        ));

        // An unlimited parent still yields an unlimited child.
        let unlimited = Encoder::new(Endian::Little);
        let payload = unlimited
            .encode_nested_to_vec(&vec![4u8; 1024])
            .expect("unlimited nested encode");
        assert_eq!(payload.len(), 8 + 1024);
    }

    /// Smallest table the *running* standard library can be shown to allocate
    /// for `try_reserve(len)` on an empty map, in bytes.
    ///
    /// The allocation size is not observable, so this reconstructs a lower
    /// bound from the one number that is: the reported capacity. A hashbrown
    /// table always has a power-of-two bucket count and always keeps at least
    /// one bucket free, so `buckets >= next_power_of_two(capacity + 1)`. Each
    /// bucket owns one `T` and one control byte, and the control array carries
    /// one trailing duplicated group (16 bytes for the widest group any
    /// currently supported target uses).
    ///
    /// This is deliberately a *lower* bound on the real allocation: the model
    /// under test must dominate it at every capacity, and a toolchain whose
    /// small-table classes grew past the model would fail this assertion
    /// instead of silently undercharging.
    fn observed_table_floor_bytes<K, V>(len: usize) -> u64
    where
        K: Hash + Eq,
    {
        let mut map: HashMap<K, V> = HashMap::new();
        map.try_reserve(len).expect("probe reservation");
        let buckets = (map.capacity() as u64 + 1).next_power_of_two();
        buckets * (size_of::<(K, V)>() as u64 + 1) + 16
    }

    /// SAFE2-01/F-02. The reservation model must never charge less than the
    /// table `try_reserve` really allocates, at *every* capacity — including
    /// the small-table classes `1..=14`, where the previous two-bucket model
    /// charged `HashMap<(), ()>` 19 bytes for a 16-bucket table.
    #[test]
    fn hash_table_model_dominates_the_real_reservation_at_every_capacity() {
        macro_rules! check {
            ($k:ty, $v:ty) => {
                for len in 1..=64usize {
                    let modelled =
                        hash_table_reservation_bytes::<($k, $v)>(len).expect("model fits in u64");
                    let observed = observed_table_floor_bytes::<$k, $v>(len);
                    assert!(
                        modelled >= observed,
                        "{}/{} at len {len}: charged {modelled} < real floor {observed}",
                        stringify!($k),
                        stringify!($v),
                    );
                }
            };
        }

        check!((), ());
        check!(u8, ());
        check!(u8, u8);
        check!(u16, u32);
        check!(u64, u64);
        check!(String, Vec<u8>);
        check!([u8; 96], [u8; 96]);
    }

    /// The model is monotonic in the declared count and never zero for a
    /// non-empty map: a budget guard that could be made cheaper by declaring
    /// *more* entries would be defeatable by inflating the count.
    #[test]
    fn hash_table_model_is_monotonic_and_never_free() {
        let mut previous = 0u64;
        for len in 1..=4096usize {
            let charge = hash_table_reservation_bytes::<(u32, u32)>(len).expect("model fits");
            assert!(charge > 0, "len {len} charged nothing");
            assert!(
                charge >= previous,
                "len {len} charged less than len {}",
                len - 1
            );
            previous = charge;
        }
        assert_eq!(hash_table_reservation_bytes::<((), ())>(0), Some(0));
        // A count whose table cannot be represented is a rejection, never a
        // wrap-around to a small charge. On targets where the arithmetic still
        // fits, the charge must at least exceed the declared count.
        match hash_table_reservation_bytes::<(u64, u64)>(usize::MAX) {
            None => {}
            Some(charge) => assert!(charge >= usize::MAX as u64),
        }
    }
}
