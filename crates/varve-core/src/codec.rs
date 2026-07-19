use std::cmp::Ordering;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::hash::Hash;
use std::mem::size_of;

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

pub trait VarveEncode {
    const WIRE_TYPE: WireType;

    fn encode_varve(&self, encoder: &mut Encoder) -> Result<()>;
}

pub trait VarveDecode: Sized {
    const WIRE_TYPE: WireType;

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

    fn preflight_map_count<K: VarveDecode, V: VarveDecode>(
        &mut self,
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
        self.preflight_count(len, 0, size_of::<(K, V)>(), resource)
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

        let field_ids = self.large_field_ids.get_or_insert_with(HashSet::new);
        if field_ids.contains(&field_id) {
            return Err(Error::InvalidCanonicalEncoding("duplicate variable field"));
        }
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

    fn encode_varve(&self, _encoder: &mut Encoder) -> Result<()> {
        Ok(())
    }
}

impl VarveDecode for () {
    const WIRE_TYPE: WireType = WireType::Unit;

    fn decode_varve(_decoder: &mut Decoder<'_>) -> Result<Self> {
        Ok(())
    }
}

macro_rules! unsigned {
    ($ty:ty, $wire:ident, $write:ident, $read:ident) => {
        impl VarveEncode for $ty {
            const WIRE_TYPE: WireType = WireType::$wire;

            fn encode_varve(&self, encoder: &mut Encoder) -> Result<()> {
                encoder.$write(*self);
                Ok(())
            }
        }

        impl VarveDecode for $ty {
            const WIRE_TYPE: WireType = WireType::$wire;

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

            fn encode_varve(&self, encoder: &mut Encoder) -> Result<()> {
                encoder.$write(*self as $uty);
                Ok(())
            }
        }

        impl VarveDecode for $ty {
            const WIRE_TYPE: WireType = WireType::$wire;

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

    fn encode_varve(&self, encoder: &mut Encoder) -> Result<()> {
        encoder.write_u8(u8::from(*self));
        Ok(())
    }
}

impl VarveDecode for bool {
    const WIRE_TYPE: WireType = WireType::Bool;

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

    fn encode_varve(&self, encoder: &mut Encoder) -> Result<()> {
        encoder.write_u32(self.to_bits());
        Ok(())
    }
}

impl VarveDecode for f32 {
    const WIRE_TYPE: WireType = WireType::F32;

    fn decode_varve(decoder: &mut Decoder<'_>) -> Result<Self> {
        Ok(Self::from_bits(decoder.read_u32()?))
    }
}

impl VarveEncode for f64 {
    const WIRE_TYPE: WireType = WireType::F64;

    fn encode_varve(&self, encoder: &mut Encoder) -> Result<()> {
        encoder.write_u64(self.to_bits());
        Ok(())
    }
}

impl VarveDecode for f64 {
    const WIRE_TYPE: WireType = WireType::F64;

    fn decode_varve(decoder: &mut Decoder<'_>) -> Result<Self> {
        Ok(Self::from_bits(decoder.read_u64()?))
    }
}

impl VarveEncode for Vec<u8> {
    const WIRE_TYPE: WireType = WireType::Bytes;

    fn encode_varve(&self, encoder: &mut Encoder) -> Result<()> {
        encoder.write_u64(self.len() as u64);
        encoder.write_all(self);
        Ok(())
    }
}

impl VarveDecode for Vec<u8> {
    const WIRE_TYPE: WireType = WireType::Bytes;

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

    fn encode_varve(&self, encoder: &mut Encoder) -> Result<()> {
        encoder.write_u64(self.len() as u64);
        encoder.write_all(self.as_bytes());
        Ok(())
    }
}

impl VarveDecode for String {
    const WIRE_TYPE: WireType = WireType::String;

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

    fn encode_varve(&self, encoder: &mut Encoder) -> Result<()> {
        for value in self {
            value.encode_varve(encoder)?;
        }
        Ok(())
    }
}

impl<T, const N: usize> VarveDecode for [T; N]
where
    T: VarveDecode,
{
    const WIRE_TYPE: WireType = WireType::Seq;

    fn decode_varve(decoder: &mut Decoder<'_>) -> Result<Self> {
        decoder.preflight_count(N, minimum_wire_size(T::WIRE_TYPE), size_of::<T>(), "array")?;
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
}

impl<K, V> VarveEncode for BTreeMap<K, V>
where
    K: VarveEncode + Ord,
    V: VarveEncode,
{
    const WIRE_TYPE: WireType = WireType::Nested;

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

    fn decode_varve(decoder: &mut Decoder<'_>) -> Result<Self> {
        let len = decoder.read_len()?;
        decoder.preflight_map_count::<K, V>(len, "HashMap entries")?;
        let mut values = HashMap::new();
        values
            .try_reserve(len)
            .map_err(|_| Error::AllocationFailed {
                resource: "HashMap entries",
                requested: allocation_request::<(K, V)>(len),
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

macro_rules! tuple_codec {
    ($($name:ident),+) => {
        impl<$($name),+> VarveEncode for ($($name,)+)
        where
            $($name: VarveEncode),+
        {
            const WIRE_TYPE: WireType = WireType::Nested;

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
}
