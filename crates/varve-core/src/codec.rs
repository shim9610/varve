use std::cmp::Ordering;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::hash::Hash;

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
}

impl Encoder {
    pub fn new(endian: Endian) -> Self {
        Self {
            endian,
            output: Vec::new(),
        }
    }

    pub fn endian(&self) -> Endian {
        self.endian
    }

    pub fn into_inner(self) -> Vec<u8> {
        self.output
    }

    pub fn write_all(&mut self, bytes: &[u8]) {
        self.output.extend_from_slice(bytes);
    }

    pub fn write_u8(&mut self, value: u8) {
        self.output.push(value);
    }

    pub fn write_u16(&mut self, value: u16) {
        match self.endian {
            Endian::Little => self.output.extend_from_slice(&value.to_le_bytes()),
            Endian::Big => self.output.extend_from_slice(&value.to_be_bytes()),
        }
    }

    pub fn write_u32(&mut self, value: u32) {
        match self.endian {
            Endian::Little => self.output.extend_from_slice(&value.to_le_bytes()),
            Endian::Big => self.output.extend_from_slice(&value.to_be_bytes()),
        }
    }

    pub fn write_u64(&mut self, value: u64) {
        match self.endian {
            Endian::Little => self.output.extend_from_slice(&value.to_le_bytes()),
            Endian::Big => self.output.extend_from_slice(&value.to_be_bytes()),
        }
    }

    pub fn write_u128(&mut self, value: u128) {
        match self.endian {
            Endian::Little => self.output.extend_from_slice(&value.to_le_bytes()),
            Endian::Big => self.output.extend_from_slice(&value.to_be_bytes()),
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
}

impl<'a> Decoder<'a> {
    pub fn new(input: &'a [u8], endian: Endian) -> Self {
        Self {
            endian,
            input,
            position: 0,
            small_field_ids: 0,
            large_field_ids: None,
        }
    }

    pub fn endian(&self) -> Endian {
        self.endian
    }

    pub fn remaining(&self) -> usize {
        self.input.len().saturating_sub(self.position)
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
    let _: usize =
        usize::try_from(payload_len).map_err(|_| Error::LengthOverflow { value: payload_len })?;
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

pub fn decode_from_slice<T: VarveDecode>(bytes: &[u8], endian: Endian) -> Result<T> {
    let mut decoder = Decoder::new(bytes, endian);
    let value = T::decode_varve(&mut decoder)?;
    if decoder.remaining() != 0 {
        return Err(Error::TrailingBytes {
            remaining: decoder.remaining(),
        });
    }
    Ok(value)
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
        Ok(decoder.read_exact(len)?.to_vec())
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
        let bytes = decoder.read_exact(len)?;
        String::from_utf8(bytes.to_vec()).map_err(|_| Error::InvalidUtf8)
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
        let mut values = Vec::with_capacity(N);
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
        let mut values = HashMap::new();
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
    ($($ty:ty),+ $(,)?) => {
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
                    let mut values = Vec::new();
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
    bool, i8, u16, i16, u32, i32, u64, i64, u128, i128, f32, f64, String
);

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
