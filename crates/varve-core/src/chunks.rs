use crate::{CompressionLevel, Error, Result, VarveDecode, VarveEncode, WireType};

const CHUNKED_MAGIC: &[u8; 4] = b"VCHK";
const CHUNKED_VERSION: u16 = 1;
const CHUNKED_HEADER_LEN: usize = 32;
const CHUNK_ENTRY_LEN: usize = 16;
const ALGORITHM_ZSTD: u8 = 1;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChunkedBytes {
    encoded: Vec<u8>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ChunkEntry {
    uncompressed_len: u32,
    stored_len: u32,
    crc32: u32,
}

impl ChunkedBytes {
    pub fn from_zstd_chunks(
        payload: &[u8],
        chunk_len: usize,
        level: CompressionLevel,
    ) -> Result<Self> {
        if chunk_len == 0 || chunk_len > u32::MAX as usize {
            return Err(Error::InvalidChunkedBytes);
        }
        let mut entries = Vec::new();
        let mut chunks = Vec::new();
        for chunk in payload.chunks(chunk_len) {
            let compressed = zstd_compress(chunk, level)?;
            let stored_len =
                u32::try_from(compressed.len()).map_err(|_| Error::InvalidChunkedBytes)?;
            entries.push(ChunkEntry {
                uncompressed_len: chunk
                    .len()
                    .try_into()
                    .map_err(|_| Error::InvalidChunkedBytes)?,
                stored_len,
                crc32: crc32_bytes(chunk)?,
            });
            chunks.push(compressed);
        }
        let chunk_count = u32::try_from(entries.len()).map_err(|_| Error::InvalidChunkedBytes)?;
        let mut encoded = Vec::new();
        encoded.extend_from_slice(CHUNKED_MAGIC);
        encoded.extend_from_slice(&CHUNKED_VERSION.to_le_bytes());
        encoded.push(ALGORITHM_ZSTD);
        encoded.push(0);
        encoded.extend_from_slice(&(chunk_len as u64).to_le_bytes());
        encoded.extend_from_slice(&(payload.len() as u64).to_le_bytes());
        encoded.extend_from_slice(&chunk_count.to_le_bytes());
        encoded.extend_from_slice(&0u32.to_le_bytes());
        debug_assert_eq!(encoded.len(), CHUNKED_HEADER_LEN);
        for entry in &entries {
            encoded.extend_from_slice(&entry.uncompressed_len.to_le_bytes());
            encoded.extend_from_slice(&entry.stored_len.to_le_bytes());
            encoded.extend_from_slice(&entry.crc32.to_le_bytes());
            encoded.extend_from_slice(&0u32.to_le_bytes());
        }
        for chunk in chunks {
            encoded.extend_from_slice(&chunk);
        }
        Self::from_encoded(encoded)
    }

    pub fn from_encoded(encoded: Vec<u8>) -> Result<Self> {
        validate_chunked_bytes(&encoded)?;
        Ok(Self { encoded })
    }

    pub fn encoded(&self) -> &[u8] {
        &self.encoded
    }

    pub fn into_encoded(self) -> Vec<u8> {
        self.encoded
    }

    pub fn decode_to_vec(&self) -> Result<Vec<u8>> {
        let header = parse_header(&self.encoded)?;
        let mut entries = Vec::with_capacity(header.chunk_count as usize);
        let mut entry_offset = CHUNKED_HEADER_LEN;
        for _ in 0..header.chunk_count {
            entries.push(parse_entry(&self.encoded, entry_offset)?);
            entry_offset = entry_offset
                .checked_add(CHUNK_ENTRY_LEN)
                .ok_or(Error::InvalidChunkedBytes)?;
        }
        let mut payload_offset = entry_offset;
        let mut output = Vec::with_capacity(
            usize::try_from(header.uncompressed_len).map_err(|_| Error::InvalidChunkedBytes)?,
        );
        for (index, entry) in entries.iter().enumerate() {
            let stored_len = entry.stored_len as usize;
            let end = payload_offset
                .checked_add(stored_len)
                .ok_or(Error::InvalidChunkedBytes)?;
            let stored = self
                .encoded
                .get(payload_offset..end)
                .ok_or(Error::InvalidChunkedBytes)?;
            payload_offset = end;
            let chunk = match header.algorithm {
                ALGORITHM_ZSTD => zstd_decompress(stored, entry.uncompressed_len as usize)?,
                _ => return Err(Error::InvalidCompressionHeader),
            };
            if chunk.len() != entry.uncompressed_len as usize {
                return Err(Error::DecompressedLengthMismatch {
                    expected: entry.uncompressed_len as u64,
                    actual: chunk.len() as u64,
                });
            }
            let actual = crc32_bytes(&chunk)?;
            if actual != entry.crc32 {
                return Err(Error::ChunkChecksumMismatch {
                    chunk_index: index as u32,
                    expected: entry.crc32,
                    actual,
                });
            }
            output.extend_from_slice(&chunk);
        }
        if output.len() as u64 != header.uncompressed_len {
            return Err(Error::DecompressedLengthMismatch {
                expected: header.uncompressed_len,
                actual: output.len() as u64,
            });
        }
        Ok(output)
    }
}

impl VarveEncode for ChunkedBytes {
    const WIRE_TYPE: WireType = WireType::Bytes;

    fn encode_varve(&self, encoder: &mut crate::Encoder) -> Result<()> {
        self.encoded.encode_varve(encoder)
    }
}

impl VarveDecode for ChunkedBytes {
    const WIRE_TYPE: WireType = WireType::Bytes;

    fn decode_varve(decoder: &mut crate::Decoder<'_>) -> Result<Self> {
        Self::from_encoded(Vec::<u8>::decode_varve(decoder)?)
    }
}

#[derive(Clone, Copy)]
struct ChunkedHeader {
    algorithm: u8,
    uncompressed_len: u64,
    chunk_count: u32,
}

fn validate_chunked_bytes(encoded: &[u8]) -> Result<()> {
    let header = parse_header(encoded)?;
    let entries_len = (header.chunk_count as usize)
        .checked_mul(CHUNK_ENTRY_LEN)
        .ok_or(Error::InvalidChunkedBytes)?;
    let entries_end = CHUNKED_HEADER_LEN
        .checked_add(entries_len)
        .ok_or(Error::InvalidChunkedBytes)?;
    let mut payload_len = 0usize;
    let mut uncompressed_len = 0u64;
    for offset in (CHUNKED_HEADER_LEN..entries_end).step_by(CHUNK_ENTRY_LEN) {
        let entry = parse_entry(encoded, offset)?;
        payload_len = payload_len
            .checked_add(entry.stored_len as usize)
            .ok_or(Error::InvalidChunkedBytes)?;
        uncompressed_len = uncompressed_len
            .checked_add(entry.uncompressed_len as u64)
            .ok_or(Error::InvalidChunkedBytes)?;
    }
    let expected_len = entries_end
        .checked_add(payload_len)
        .ok_or(Error::InvalidChunkedBytes)?;
    if expected_len != encoded.len() || uncompressed_len != header.uncompressed_len {
        return Err(Error::InvalidChunkedBytes);
    }
    Ok(())
}

fn parse_header(encoded: &[u8]) -> Result<ChunkedHeader> {
    if encoded.len() < CHUNKED_HEADER_LEN || &encoded[0..4] != CHUNKED_MAGIC {
        return Err(Error::InvalidChunkedBytes);
    }
    let version = u16::from_le_bytes(encoded[4..6].try_into().expect("slice"));
    if version != CHUNKED_VERSION {
        return Err(Error::InvalidChunkedBytes);
    }
    let algorithm = encoded[6];
    if algorithm != ALGORITHM_ZSTD {
        return Err(Error::InvalidCompressionHeader);
    }
    let chunk_len = u64::from_le_bytes(encoded[8..16].try_into().expect("slice"));
    if chunk_len == 0 || chunk_len > u64::from(u32::MAX) {
        return Err(Error::InvalidChunkedBytes);
    }
    Ok(ChunkedHeader {
        algorithm,
        uncompressed_len: u64::from_le_bytes(encoded[16..24].try_into().expect("slice")),
        chunk_count: u32::from_le_bytes(encoded[24..28].try_into().expect("slice")),
    })
}

fn parse_entry(encoded: &[u8], offset: usize) -> Result<ChunkEntry> {
    let end = offset
        .checked_add(CHUNK_ENTRY_LEN)
        .ok_or(Error::InvalidChunkedBytes)?;
    let entry = encoded.get(offset..end).ok_or(Error::InvalidChunkedBytes)?;
    Ok(ChunkEntry {
        uncompressed_len: u32::from_le_bytes(entry[0..4].try_into().expect("slice")),
        stored_len: u32::from_le_bytes(entry[4..8].try_into().expect("slice")),
        crc32: u32::from_le_bytes(entry[8..12].try_into().expect("slice")),
    })
}

#[cfg(feature = "compression-zstd")]
fn zstd_compress(payload: &[u8], level: CompressionLevel) -> Result<Vec<u8>> {
    Ok(zstd::bulk::compress(payload, level.to_zstd_level())?)
}

#[cfg(not(feature = "compression-zstd"))]
fn zstd_compress(_payload: &[u8], _level: CompressionLevel) -> Result<Vec<u8>> {
    Err(Error::CompressionFeatureDisabled)
}

#[cfg(feature = "compression-zstd")]
fn zstd_decompress(payload: &[u8], len: usize) -> Result<Vec<u8>> {
    Ok(zstd::bulk::decompress(payload, len)?)
}

#[cfg(not(feature = "compression-zstd"))]
fn zstd_decompress(_payload: &[u8], _len: usize) -> Result<Vec<u8>> {
    Err(Error::CompressionFeatureDisabled)
}

#[cfg(feature = "integrity")]
fn crc32_bytes(bytes: &[u8]) -> Result<u32> {
    Ok(crc32fast::hash(bytes))
}

#[cfg(not(feature = "integrity"))]
fn crc32_bytes(_bytes: &[u8]) -> Result<u32> {
    Err(Error::IntegrityFeatureDisabled)
}
