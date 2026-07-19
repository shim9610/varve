use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::sync::Arc;

use crate::codec::encode_to_vec_limited;
use crate::format::ReadLimitKey;
use crate::{
    BlockKind, Decoder, Error, FormatSpec, IntegrityPolicy, MatrixCommitKind, ReadLimits, Result,
    VarveMatrixBlock,
};

const VMAT_MAGIC: &[u8; 4] = b"VMAT";
const VMAT_VERSION: u16 = 1;
const VMAT_HEADER_LEN: u32 = 160;
const MCRC_MAGIC: &[u8; 4] = b"MCRC";
const MCRC_VERSION: u16 = 1;
const MCRC_HEADER_LEN: u64 = 16;
const CRC_LEN: u64 = 4;
const MATRIX_BYTES_RESOURCE: &str = "matrix bytes";
const MATRIX_SLOT_PAYLOAD_RESOURCE: &str = "matrix slot payload";
const MATRIX_DESCRIPTOR_RESOURCE: &str = "matrix descriptors";
#[allow(dead_code)]
const MATRIX_SIDECAR_RESOURCE: &str = "matrix sidecar";

type CommitPlan = (String, MatrixCommitKind, u64);
type StoredCommitPlan = (String, MatrixCommitKind, u64, u64, u64);

#[cfg(test)]
std::thread_local! {
    static FAIL_NEXT_SLOT_WRITE: std::cell::Cell<bool> = const {
        std::cell::Cell::new(false)
    };
    static FAIL_NEXT_BITMAP_WRITE: std::cell::Cell<bool> = const {
        std::cell::Cell::new(false)
    };
}

#[cfg(test)]
pub(crate) fn inject_partial_slot_write_failure() {
    FAIL_NEXT_SLOT_WRITE.set(true);
}

#[cfg(test)]
pub(crate) fn inject_bitmap_write_failure() {
    FAIL_NEXT_BITMAP_WRITE.set(true);
}

fn write_slot_payload(file: &mut File, payload: &[u8]) -> Result<()> {
    #[cfg(test)]
    if FAIL_NEXT_SLOT_WRITE.replace(false) {
        if let Some(first) = payload.first() {
            file.write_all(std::slice::from_ref(first))?;
        }
        return Err(Error::Io(std::io::Error::other(
            "injected partial matrix slot write failure",
        )));
    }

    file.write_all(payload)?;
    Ok(())
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MatrixDimensionValue {
    pub name: String,
    pub value: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MatrixDimensions {
    values: Vec<MatrixDimensionValue>,
}

impl MatrixDimensions {
    pub fn from_pairs<const N: usize>(pairs: [(&'static str, u64); N]) -> Self {
        let values = pairs
            .into_iter()
            .map(|(name, value)| MatrixDimensionValue {
                name: name.to_string(),
                value,
            })
            .collect();
        Self { values }
    }

    pub fn get(&self, name: &str) -> Option<u64> {
        self.values
            .iter()
            .find(|value| value.name == name)
            .map(|value| value.value)
    }

    pub fn values(&self) -> &[MatrixDimensionValue] {
        &self.values
    }
}

impl<const N: usize> From<[(&'static str, u64); N]> for MatrixDimensions {
    fn from(value: [(&'static str, u64); N]) -> Self {
        Self::from_pairs(value)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct MatrixKey {
    pub scan: u64,
    pub ch: u64,
}

impl MatrixKey {
    pub const fn new(scan: u64, ch: u64) -> Self {
        Self { scan, ch }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MatrixCellStatus {
    Committed,
    NotCommitted,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PackedBitmap {
    bit_len: u64,
    bytes: Vec<u8>,
}

impl PackedBitmap {
    pub fn new(bit_len: u64) -> Result<Self> {
        Ok(Self {
            bit_len,
            bytes: filled_bytes(bit_bytes(bit_len)?, 0)?,
        })
    }

    pub fn bit_len(&self) -> u64 {
        self.bit_len
    }

    pub fn get(&self, ordinal: u64) -> Result<bool> {
        if ordinal >= self.bit_len {
            return Err(Error::InvalidMatrixLayout);
        }
        get_bit(&self.bytes, ordinal)
    }

    pub fn set(&mut self, ordinal: u64, value: bool) -> Result<()> {
        if ordinal >= self.bit_len {
            return Err(Error::InvalidMatrixLayout);
        }
        set_bit(&mut self.bytes, ordinal, value)
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }
}

impl crate::VarveEncode for PackedBitmap {
    const WIRE_TYPE: crate::WireType = crate::WireType::Bytes;

    fn encode_varve(&self, encoder: &mut crate::Encoder) -> Result<()> {
        crate::VarveEncode::encode_varve(&self.bit_len, encoder)?;
        crate::VarveEncode::encode_varve(&self.bytes, encoder)
    }
}

impl crate::VarveDecode for PackedBitmap {
    const WIRE_TYPE: crate::WireType = crate::WireType::Bytes;

    fn decode_varve(decoder: &mut crate::Decoder<'_>) -> Result<Self> {
        let bit_len = <u64 as crate::VarveDecode>::decode_varve(decoder)?;
        let bytes = <Vec<u8> as crate::VarveDecode>::decode_varve(decoder)?;
        if bytes.len() as u64 != bit_bytes(bit_len)? {
            return Err(Error::InvalidMatrixLayout);
        }
        Ok(Self { bit_len, bytes })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MatrixCorruptionSeverity {
    Fatal,
    Recoverable,
    Advisory,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MatrixCorruptionKind {
    Header,
    Layout,
    CommitMap,
    Slot,
    Sidecar,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MatrixRecoveryFinding {
    pub kind: MatrixCorruptionKind,
    pub severity: MatrixCorruptionSeverity,
    pub message: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MatrixRecoveryAction {
    ClearCell { category: String, key: MatrixKey },
    ClearCategory { category: String },
    RebuildCommitMap { category: Option<String> },
    Resume,
    Restart,
    DiscardSidecar,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MatrixRecoveryReport {
    pub findings: Vec<MatrixRecoveryFinding>,
    pub recommended_actions: Vec<MatrixRecoveryAction>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MatrixResumeSignal {
    Clean,
    Partial { committed: u64, total: u64 },
    ResumeAvailable { committed: u64, total: u64 },
    RestartRecommended,
    DiscardRecommended,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MatrixCommitEvent {
    pub block_id: u32,
    pub category: &'static str,
    pub key: MatrixKey,
    pub slot_offset: u64,
    pub slot_len: u64,
}

#[derive(Clone, Debug)]
pub struct MatrixLayout {
    dimensions: Vec<MatrixDimensionValue>,
    commits: Vec<MatrixCommitLayout>,
    blocks: Vec<MatrixBlockLayout>,
    aux: Vec<MatrixAuxLayout>,
    crc_findings: Vec<MatrixRecoveryFinding>,
    append_log_start: u64,
    read_limits: ReadLimits,
    resident_bitmap_bytes: u64,
    // Precomputed at open time so accessors gate on a single flag instead of
    // scanning findings per cell access.
    fatal_access_blocked: bool,
}

#[derive(Clone, Debug)]
struct MatrixCommitLayout {
    name: String,
    kind: MatrixCommitKind,
    bit_count: u64,
    map_offset: u64,
    bits: Arc<Vec<u8>>,
    quarantined_raw_bits: Option<Arc<Vec<u8>>>,
    quarantine_finding: Option<MatrixRecoveryFinding>,
    crc_offset: Option<u64>,
}

#[derive(Clone, Debug)]
struct MatrixBlockLayout {
    block_id: u32,
    dimensions: [String; 2],
    slot_stride: u64,
    slot_region_offset: u64,
    cell_count: u64,
    crc_offset: Option<u64>,
    crc_valid_offset: Option<u64>,
    crc_valid_bits: Arc<Vec<u8>>,
    written_bits: Arc<Vec<u8>>,
    current_write_bits: Arc<Vec<u8>>,
}

#[derive(Clone, Debug)]
struct MatrixAuxLayout {
    name: String,
    offset: u64,
    byte_len: u64,
}

#[derive(Clone, Debug)]
struct MatrixCrcLayout {
    region_offset: u64,
    region_len: u64,
}

#[derive(Default)]
struct MatrixCrcVerification {
    findings: Vec<MatrixRecoveryFinding>,
    commit_findings: HashMap<String, MatrixRecoveryFinding>,
}

impl MatrixLayout {
    pub fn append_log_start(&self) -> u64 {
        self.append_log_start
    }

    // Fail-closed gate for `Fatal` recovery findings: safe accessors must not
    // consume fatal-state data unless the spec opted into forensic access.
    fn ensure_fatal_access_allowed(&self) -> Result<()> {
        if self.fatal_access_blocked {
            return Err(Error::MatrixFatalCorruption);
        }
        Ok(())
    }

    pub fn dimension(&self, name: &str) -> Option<u64> {
        self.dimensions
            .iter()
            .find(|value| value.name == name)
            .map(|value| value.value)
    }

    fn block_index(&self, block_id: u32) -> Result<usize> {
        self.blocks
            .iter()
            .position(|block| block.block_id == block_id)
            .ok_or(Error::MatrixBlockMissing(block_id))
    }

    fn commit_index(&self, name: &str) -> Result<usize> {
        self.commits
            .iter()
            .position(|commit| commit.name == name)
            .ok_or_else(|| Error::MatrixCommitMissing(name.to_string()))
    }

    fn ordinal_for_block(&self, block_index: usize, key: MatrixKey) -> Result<u64> {
        let block = &self.blocks[block_index];
        let dim0 = self
            .dimension(&block.dimensions[0])
            .ok_or_else(|| Error::MatrixDimensionMissing(block.dimensions[0].clone()))?;
        let dim1 = self
            .dimension(&block.dimensions[1])
            .ok_or_else(|| Error::MatrixDimensionMissing(block.dimensions[1].clone()))?;
        if key.scan >= dim0 || key.ch >= dim1 {
            return Err(Error::MatrixKeyOutOfBounds {
                scan: key.scan,
                ch: key.ch,
            });
        }
        key.scan
            .checked_mul(dim1)
            .and_then(|base| base.checked_add(key.ch))
            .ok_or(Error::InvalidMatrixLayout)
    }

    fn slot_offset(&self, block_index: usize, ordinal: u64) -> Result<u64> {
        let block = &self.blocks[block_index];
        ordinal
            .checked_mul(block.slot_stride)
            .and_then(|delta| block.slot_region_offset.checked_add(delta))
            .ok_or(Error::InvalidMatrixLayout)
    }

    fn aux(&self, name: &str) -> Result<&MatrixAuxLayout> {
        self.aux
            .iter()
            .find(|aux| aux.name == name)
            .ok_or_else(|| Error::MatrixAuxMissing(name.to_string()))
    }
}

pub(crate) fn create_layout(
    spec: FormatSpec,
    file: &mut File,
    header_len: u64,
    dims: &MatrixDimensions,
) -> Result<MatrixLayout> {
    let crc_enabled = matrix_crc_enabled(spec)?;
    let (dimension_table_len, block_table_len, category_table_len) =
        matrix_descriptor_table_lengths(spec)?;
    check_matrix_metadata_limit(
        spec.read_limits,
        dimension_table_len,
        block_table_len,
        category_table_len,
    )?;
    let dimensions = dimension_values(spec, dims)?;
    check_matrix_dimensions(spec.read_limits, &dimensions)?;
    let cell_counts = matrix_cell_counts(spec, &dimensions)?;
    let commit_plans = matrix_commit_plans(spec, &dimensions, &cell_counts)?;
    let commit_map_len = matrix_commit_map_len(&commit_plans)?;
    let slot_region_len = matrix_slot_region_len(spec, &cell_counts)?;
    spec.read_limits
        .check(ReadLimitKey::MatrixSlotRegionLen, slot_region_len)?;
    let block_bitmap_len = matrix_block_bitmap_len(spec, &cell_counts)?;
    let resident_bitmap_bytes = matrix_resident_bitmap_len(commit_map_len, block_bitmap_len)?;
    spec.read_limits
        .check(ReadLimitKey::MatrixBitmapBytes, resident_bitmap_bytes)?;
    let region_crc_len = if crc_enabled {
        crc_table_len(spec, &cell_counts)?
    } else {
        0
    };
    let accounted_crc_bytes =
        matrix_accounted_crc_len(crc_enabled, region_crc_len, block_bitmap_len)?;
    spec.read_limits
        .check(ReadLimitKey::MatrixCrcBytes, accounted_crc_bytes)?;

    let dimension_table_off = header_len
        .checked_add(u64::from(VMAT_HEADER_LEN))
        .ok_or(Error::InvalidMatrixLayout)?;
    let block_table_off = dimension_table_off
        .checked_add(dimension_table_len)
        .ok_or(Error::InvalidMatrixLayout)?;
    let commit_category_off = block_table_off
        .checked_add(block_table_len)
        .ok_or(Error::InvalidMatrixLayout)?;
    let commit_map_off = commit_category_off
        .checked_add(category_table_len)
        .ok_or(Error::InvalidMatrixLayout)?;
    let slot_region_off = commit_map_off
        .checked_add(commit_map_len)
        .ok_or(Error::InvalidMatrixLayout)?;
    let slot_region_end = slot_region_off
        .checked_add(slot_region_len)
        .ok_or(Error::InvalidMatrixLayout)?;
    let aux_offsets = matrix_aux_offsets(spec, slot_region_end)?;
    let aux_region_len = matrix_aux_region_len(spec)?;
    let aux_region_end = slot_region_end
        .checked_add(aux_region_len)
        .ok_or(Error::InvalidMatrixLayout)?;
    let region_crc_off = if crc_enabled { aux_region_end } else { 0 };
    let append_log_start = if crc_enabled {
        region_crc_off
            .checked_add(region_crc_len)
            .ok_or(Error::InvalidMatrixLayout)?
    } else {
        aux_region_end
    };
    spec.read_limits
        .check(ReadLimitKey::FileLen, append_log_start)?;

    let dimension_table = encode_dimension_table(&dimensions, dimension_table_len)?;
    let mut dimension_index = HashMap::new();
    try_reserve_map(
        &mut dimension_index,
        dimensions.len(),
        MATRIX_DESCRIPTOR_RESOURCE,
    )?;
    for (index, value) in dimensions.iter().enumerate() {
        dimension_index.insert(
            value.name.clone(),
            u16::try_from(index).map_err(|_| Error::InvalidMatrixLayout)?,
        );
    }

    let mut commit_offsets = HashMap::new();
    try_reserve_map(
        &mut commit_offsets,
        commit_plans.len(),
        MATRIX_DESCRIPTOR_RESOURCE,
    )?;
    let mut next_commit_offset = commit_map_off;
    for (name, _, bit_count) in &commit_plans {
        let len = bit_bytes(*bit_count)?;
        commit_offsets.insert(name.clone(), (next_commit_offset, len));
        next_commit_offset = next_commit_offset
            .checked_add(len)
            .ok_or(Error::InvalidMatrixLayout)?;
    }

    let mut block_offsets = HashMap::new();
    try_reserve_map(
        &mut block_offsets,
        spec.matrix_blocks.len(),
        MATRIX_DESCRIPTOR_RESOURCE,
    )?;
    let mut next_slot_offset = slot_region_off;
    for block in spec.matrix_blocks {
        let cell_count = *cell_counts
            .get(block.category)
            .ok_or_else(|| Error::MatrixCommitMissing(block.category.to_string()))?;
        let len = cell_count
            .checked_mul(block.slot_stride)
            .ok_or(Error::InvalidMatrixLayout)?;
        block_offsets.insert(block.block_id, (next_slot_offset, len, cell_count));
        next_slot_offset = next_slot_offset
            .checked_add(len)
            .ok_or(Error::InvalidMatrixLayout)?;
    }

    let block_table = encode_block_table(spec, &dimension_index, &commit_plans, &block_offsets)?;
    let category_table = encode_category_table(&commit_plans, &commit_offsets)?;
    if usize_to_u64(block_table.len())? != block_table_len
        || usize_to_u64(category_table.len())? != category_table_len
    {
        return Err(Error::InvalidMatrixLayout);
    }
    let crc_layout = crc_layout_from_parts(
        region_crc_off,
        region_crc_len,
        spec,
        &commit_plans,
        &block_offsets,
    )?;
    let header = encode_header(MatrixHeaderFields {
        dimension_count: u32::try_from(dimensions.len()).map_err(|_| Error::InvalidMatrixLayout)?,
        matrix_block_count: u32::try_from(spec.matrix_blocks.len())
            .map_err(|_| Error::InvalidMatrixLayout)?,
        commit_category_count: u32::try_from(spec.matrix_commits.len())
            .map_err(|_| Error::InvalidMatrixLayout)?,
        dimension_table_off,
        dimension_table_len,
        block_table_off,
        block_table_len,
        commit_category_off,
        commit_category_len: category_table_len,
        commit_map_off,
        commit_map_len,
        slot_region_off,
        slot_region_len,
        region_crc_off,
        region_crc_len,
        append_log_start,
    });
    let has_crc = crc_layout.is_some();
    let initial_crc_valid_bits = zero_crc_valid_bitmaps(spec, has_crc, &block_offsets)?;
    let layout = layout_from_parts(
        dimensions,
        spec,
        &commit_plans,
        &commit_offsets,
        &block_offsets,
        &aux_offsets,
        zero_commit_bitmaps(&commit_plans)?,
        crc_layout,
        initial_crc_valid_bits,
        HashMap::new(),
        Vec::new(),
        append_log_start,
        resident_bitmap_bytes,
    )?;

    file.seek(SeekFrom::Start(header_len))?;
    file.write_all(&header)?;
    file.write_all(&dimension_table)?;
    file.write_all(&block_table)?;
    file.write_all(&category_table)?;
    write_zeros(file, commit_map_len)?;
    file.set_len(append_log_start)?;
    if has_crc {
        file.seek(SeekFrom::Start(region_crc_off))?;
        write_crc_table(
            file,
            spec,
            &commit_plans,
            &block_offsets,
            &dimension_table,
            &block_table,
            &category_table,
        )?;
    }
    file.seek(SeekFrom::Start(append_log_start))?;
    Ok(layout)
}

pub(crate) fn read_layout_at_len(
    spec: FormatSpec,
    file: &mut File,
    header_len: u64,
    file_len: u64,
) -> Result<MatrixLayout> {
    spec.read_limits.check(ReadLimitKey::FileLen, file_len)?;
    validate_range(header_len, u64::from(VMAT_HEADER_LEN), file_len)?;
    let crc_enabled = matrix_crc_enabled(spec)?;
    let header = read_header(file, header_len)?;
    validate_header_descriptor_shape(spec, &header)?;
    check_matrix_metadata_limit(
        spec.read_limits,
        header.dimension_table_len,
        header.block_table_len,
        header.commit_category_len,
    )?;
    let aux_region_len = matrix_aux_region_len(spec)?;
    validate_layout_ranges(header_len, file_len, &header, aux_region_len)?;
    validate_crc_presence(crc_enabled, &header)?;

    let dimension_table = read_range(
        file,
        header.dimension_table_off,
        header.dimension_table_len,
        MATRIX_DESCRIPTOR_RESOURCE,
    )?;
    let dimensions = decode_dimension_table(&dimension_table, header.dimension_count)?;
    validate_dimension_names(spec, &dimensions)?;
    check_matrix_dimensions(spec.read_limits, &dimensions)?;
    let cell_counts = matrix_cell_counts(spec, &dimensions)?;
    let expected_commit_plans = matrix_commit_plans(spec, &dimensions, &cell_counts)?;
    validate_dimension_derived_lengths(
        spec,
        crc_enabled,
        &header,
        &expected_commit_plans,
        &cell_counts,
    )?;
    spec.read_limits
        .check(ReadLimitKey::MatrixSlotRegionLen, header.slot_region_len)?;
    let block_bitmap_len = matrix_block_bitmap_len(spec, &cell_counts)?;
    let resident_bitmap_bytes =
        matrix_resident_bitmap_len(header.commit_map_len, block_bitmap_len)?;
    spec.read_limits
        .check(ReadLimitKey::MatrixBitmapBytes, resident_bitmap_bytes)?;
    let accounted_crc_bytes =
        matrix_accounted_crc_len(crc_enabled, header.region_crc_len, block_bitmap_len)?;
    spec.read_limits
        .check(ReadLimitKey::MatrixCrcBytes, accounted_crc_bytes)?;

    let category_table = read_range(
        file,
        header.commit_category_off,
        header.commit_category_len,
        MATRIX_DESCRIPTOR_RESOURCE,
    )?;
    let commit_plans = decode_category_table(&category_table, header.commit_category_count)?;
    let commit_offsets = validate_commit_table(
        &expected_commit_plans,
        &commit_plans,
        header.commit_map_off,
        header.commit_map_len,
    )?;

    let block_table = read_range(
        file,
        header.block_table_off,
        header.block_table_len,
        MATRIX_DESCRIPTOR_RESOURCE,
    )?;
    let block_offsets = decode_block_table(
        spec,
        &dimensions,
        &commit_plans,
        &cell_counts,
        &block_table,
        header.matrix_block_count,
        (header.slot_region_off, header.slot_region_len),
    )?;
    let slot_region_end = header
        .slot_region_off
        .checked_add(header.slot_region_len)
        .ok_or(Error::InvalidMatrixLayout)?;
    let aux_offsets = matrix_aux_offsets(spec, slot_region_end)?;
    let crc_layout = crc_layout_from_parts(
        header.region_crc_off,
        header.region_crc_len,
        spec,
        &expected_commit_plans,
        &block_offsets,
    )?;
    let commit_bits = read_commit_bitmaps(file, &commit_plans)?;
    let crc_verification = verify_crc_table(
        file,
        crc_layout.as_ref(),
        &[&dimension_table, &block_table, &category_table],
        &commit_plans,
        &commit_bits,
    )?;
    let crc_valid_bits = read_crc_valid_bits(spec, file, crc_layout.as_ref(), &block_offsets)?;

    layout_from_parts(
        dimensions,
        spec,
        &expected_commit_plans,
        &commit_offsets,
        &block_offsets,
        &aux_offsets,
        commit_bits,
        crc_layout,
        crc_valid_bits,
        crc_verification.commit_findings,
        crc_verification.findings,
        header.append_log_start,
        resident_bitmap_bytes,
    )
}

pub(crate) fn write_cell<T: VarveMatrixBlock>(
    spec: FormatSpec,
    layout: &mut MatrixLayout,
    file: &mut File,
    key: MatrixKey,
    value: &T,
) -> Result<()> {
    ensure_matrix_block::<T>(spec)?;
    ensure_commit_publishable(layout, T::CATEGORY)?;
    let block_index = layout.block_index(T::ID)?;
    let ordinal = layout.ordinal_for_block(block_index, key)?;
    let offset = layout.slot_offset(block_index, ordinal)?;
    let slot_stride = layout.blocks[block_index].slot_stride;
    // DEF-02: the slot stride is the hard bound for the encode itself, so a
    // hostile or miswritten `VarveMatrixBlock` encode can never buffer more
    // than one byte past the stride before the typed error fires. The cap is
    // `slot_stride + 1` (not `slot_stride`) so an encode of exactly one extra
    // byte still completes and the exact-size check below reports its true
    // length; anything larger stops buffering at the first over-limit write
    // and the overflow is mapped back to the existing `MatrixSizeMismatch`
    // contract, with `actual` being the encoded length observed at cutoff.
    let payload = match encode_to_vec_limited(
        value,
        T::ENDIAN.unwrap_or(spec.endian),
        slot_stride.saturating_add(1),
        MATRIX_SLOT_PAYLOAD_RESOURCE,
    ) {
        Ok(payload) => payload,
        Err(Error::LimitExceeded {
            resource: MATRIX_SLOT_PAYLOAD_RESOURCE,
            actual,
            ..
        }) => {
            return Err(Error::MatrixSizeMismatch {
                expected: slot_stride,
                actual,
            });
        }
        Err(err) => return Err(err),
    };
    if payload.len() as u64 != slot_stride {
        return Err(Error::MatrixSizeMismatch {
            expected: slot_stride,
            actual: payload.len() as u64,
        });
    }
    let (commit_index, commit_update) = prepare_cell_commit(layout, T::CATEGORY, ordinal, false)?;
    let crc_valid_update = prepare_cell_crc_valid(layout, block_index, ordinal, false)?;

    apply_commit_bit(layout, file, commit_index, commit_update)?;
    if let Some(update) = crc_valid_update {
        apply_cell_crc_valid(layout, file, block_index, update)?;
    }
    file.seek(SeekFrom::Start(offset))?;
    write_slot_payload(file, &payload)?;
    set_bit(
        Arc::make_mut(&mut layout.blocks[block_index].written_bits).as_mut_slice(),
        ordinal,
        true,
    )?;
    set_bit(
        Arc::make_mut(&mut layout.blocks[block_index].current_write_bits).as_mut_slice(),
        ordinal,
        true,
    )?;
    Ok(())
}

pub(crate) fn write_cell_payload<T: VarveMatrixBlock>(
    spec: FormatSpec,
    layout: &mut MatrixLayout,
    file: &mut File,
    key: MatrixKey,
    payload: &[u8],
) -> Result<()> {
    ensure_matrix_block::<T>(spec)?;
    ensure_commit_publishable(layout, T::CATEGORY)?;
    let block_index = layout.block_index(T::ID)?;
    let ordinal = layout.ordinal_for_block(block_index, key)?;
    let offset = layout.slot_offset(block_index, ordinal)?;
    let slot_stride = layout.blocks[block_index].slot_stride;
    if payload.len() as u64 != slot_stride {
        return Err(Error::MatrixSizeMismatch {
            expected: slot_stride,
            actual: payload.len() as u64,
        });
    }
    let (commit_index, commit_update) = prepare_cell_commit(layout, T::CATEGORY, ordinal, false)?;
    let crc_valid_update = prepare_cell_crc_valid(layout, block_index, ordinal, false)?;

    apply_commit_bit(layout, file, commit_index, commit_update)?;
    if let Some(update) = crc_valid_update {
        apply_cell_crc_valid(layout, file, block_index, update)?;
    }
    file.seek(SeekFrom::Start(offset))?;
    write_slot_payload(file, payload)?;
    set_bit(
        Arc::make_mut(&mut layout.blocks[block_index].written_bits).as_mut_slice(),
        ordinal,
        true,
    )?;
    set_bit(
        Arc::make_mut(&mut layout.blocks[block_index].current_write_bits).as_mut_slice(),
        ordinal,
        true,
    )?;
    Ok(())
}

pub(crate) fn read_cell<T: VarveMatrixBlock>(
    spec: FormatSpec,
    layout: &MatrixLayout,
    file: &mut File,
    key: MatrixKey,
) -> Result<T> {
    ensure_matrix_block::<T>(spec)?;
    if cell_status::<T>(spec, layout, key)? == MatrixCellStatus::NotCommitted {
        return Err(Error::MatrixNotCommitted);
    }
    let block_index = layout.block_index(T::ID)?;
    let ordinal = layout.ordinal_for_block(block_index, key)?;
    let offset = layout.slot_offset(block_index, ordinal)?;
    let stride = layout.blocks[block_index].slot_stride;
    layout
        .read_limits
        .check(ReadLimitKey::RecordPayloadLen, stride)?;
    layout
        .read_limits
        .check(ReadLimitKey::MaterializedBytes, stride)?;
    file.seek(SeekFrom::Start(offset))?;
    let mut payload = filled_bytes(stride, 0)?;
    file.read_exact(&mut payload)?;
    verify_cell_crc(layout, file, block_index, ordinal, &payload)?;
    let materialized_limit = layout
        .read_limits
        .require(ReadLimitKey::MaterializedBytes)?
        .unwrap_or(u64::MAX);
    let decode_limit =
        materialized_limit
            .checked_sub(stride)
            .ok_or(Error::ResourceArithmeticOverflow {
                resource: "materialized bytes",
            })?;
    Decoder::decode_from_slice_limited(&payload, T::ENDIAN.unwrap_or(spec.endian), decode_limit)
}

pub(crate) fn cell_status<T: VarveMatrixBlock>(
    spec: FormatSpec,
    layout: &MatrixLayout,
    key: MatrixKey,
) -> Result<MatrixCellStatus> {
    ensure_matrix_block::<T>(spec)?;
    ensure_commit_publishable(layout, T::CATEGORY)?;
    let block_index = layout.block_index(T::ID)?;
    let ordinal = layout.ordinal_for_block(block_index, key)?;
    let commit_index = layout.commit_index(T::CATEGORY)?;
    if get_bit(&layout.commits[commit_index].bits, ordinal)? {
        Ok(MatrixCellStatus::Committed)
    } else {
        Ok(MatrixCellStatus::NotCommitted)
    }
}

pub(crate) fn commit_cell<T: VarveMatrixBlock>(
    spec: FormatSpec,
    layout: &mut MatrixLayout,
    file: &mut File,
    key: MatrixKey,
) -> Result<()> {
    ensure_matrix_block::<T>(spec)?;
    ensure_commit_publishable(layout, T::CATEGORY)?;
    let block_index = layout.block_index(T::ID)?;
    let ordinal = layout.ordinal_for_block(block_index, key)?;
    let written = get_bit(&layout.blocks[block_index].written_bits, ordinal)?;
    let written_this_session = get_bit(&layout.blocks[block_index].current_write_bits, ordinal)?;
    if !written || (!written_this_session && slot_is_all_zero(layout, file, block_index, ordinal)?)
    {
        return Err(Error::MatrixCellNotWritten);
    }
    let crc_valid_update = prepare_cell_crc_valid(layout, block_index, ordinal, true)?;
    let (commit_index, commit_update) = prepare_cell_commit(layout, T::CATEGORY, ordinal, true)?;
    update_cell_crc(layout, file, block_index, ordinal)?;
    if let Some(update) = crc_valid_update {
        apply_cell_crc_valid(layout, file, block_index, update)?;
    }
    apply_commit_bit(layout, file, commit_index, commit_update)
}

pub(crate) fn clear_cell<T: VarveMatrixBlock>(
    spec: FormatSpec,
    layout: &mut MatrixLayout,
    file: &mut File,
    key: MatrixKey,
) -> Result<()> {
    ensure_matrix_block::<T>(spec)?;
    ensure_commit_publishable(layout, T::CATEGORY)?;
    let block_index = layout.block_index(T::ID)?;
    let ordinal = layout.ordinal_for_block(block_index, key)?;
    set_cell_commit(layout, file, T::CATEGORY, ordinal, false)?;
    set_cell_crc_valid(layout, file, block_index, ordinal, false)?;
    clear_cell_crc(layout, file, block_index, ordinal)
}

pub(crate) fn clear_cell_by_category(
    spec: FormatSpec,
    layout: &mut MatrixLayout,
    file: &mut File,
    category: &str,
    key: MatrixKey,
) -> Result<()> {
    ensure_commit_publishable(layout, category)?;
    let block_index = block_index_for_category(spec, layout, category)?;
    let ordinal = layout.ordinal_for_block(block_index, key)?;
    set_cell_commit(layout, file, category, ordinal, false)?;
    set_cell_crc_valid(layout, file, block_index, ordinal, false)?;
    clear_cell_crc(layout, file, block_index, ordinal)
}

pub(crate) fn clear_category(
    spec: FormatSpec,
    layout: &mut MatrixLayout,
    file: &mut File,
    category: &str,
) -> Result<u64> {
    layout.ensure_fatal_access_allowed()?;
    let commit_index = layout.commit_index(category)?;
    let cleared = count_committed(&layout.commits[commit_index])?;
    let commit_kind = layout.commits[commit_index].kind;
    let map_offset = layout.commits[commit_index].map_offset;
    let crc_offset = layout.commits[commit_index].crc_offset;
    let quarantined_len = layout.commits[commit_index]
        .quarantined_raw_bits
        .as_ref()
        .map(|bits| usize_to_u64(bits.len()))
        .transpose()?
        .unwrap_or(0);
    let cleared_valid = if commit_kind == MatrixCommitKind::Cell {
        let block_index = block_index_for_category(spec, layout, category)?;
        layout.blocks[block_index]
            .crc_valid_offset
            .map(|valid_offset| (block_index, valid_offset))
    } else {
        None
    };

    if let Some((block_index, valid_offset)) = cleared_valid {
        file.seek(SeekFrom::Start(valid_offset))?;
        write_zeros(
            file,
            usize_to_u64(layout.blocks[block_index].crc_valid_bits.len())?,
        )?;
    }
    if let Some(crc_offset) = crc_offset {
        write_crc_at(
            file,
            crc_offset,
            crc32_zeroes(usize_to_u64(layout.commits[commit_index].bits.len())?)?,
        )?;
    }
    file.seek(SeekFrom::Start(map_offset))?;
    write_zeros(file, usize_to_u64(layout.commits[commit_index].bits.len())?)?;

    if let Some((block_index, _)) = cleared_valid {
        Arc::make_mut(&mut layout.blocks[block_index].crc_valid_bits).fill(0);
    }
    {
        let commit = &mut layout.commits[commit_index];
        Arc::make_mut(&mut commit.bits).fill(0);
        commit.quarantined_raw_bits = None;
        commit.quarantine_finding = None;
    }
    layout.resident_bitmap_bytes = layout
        .resident_bitmap_bytes
        .checked_sub(quarantined_len)
        .ok_or(Error::InvalidMatrixLayout)?;
    Ok(cleared)
}

pub(crate) fn commit_event<T: VarveMatrixBlock>(
    spec: FormatSpec,
    layout: &MatrixLayout,
    key: MatrixKey,
) -> Result<MatrixCommitEvent> {
    ensure_matrix_block::<T>(spec)?;
    layout.ensure_fatal_access_allowed()?;
    let block_index = layout.block_index(T::ID)?;
    let ordinal = layout.ordinal_for_block(block_index, key)?;
    let offset = layout.slot_offset(block_index, ordinal)?;
    Ok(MatrixCommitEvent {
        block_id: T::ID,
        category: T::CATEGORY,
        key,
        slot_offset: offset,
        slot_len: layout.blocks[block_index].slot_stride,
    })
}

pub(crate) fn read_cell_payload<T: VarveMatrixBlock>(
    spec: FormatSpec,
    layout: &MatrixLayout,
    file: &mut File,
    key: MatrixKey,
) -> Result<Vec<u8>> {
    ensure_matrix_block::<T>(spec)?;
    if cell_status::<T>(spec, layout, key)? == MatrixCellStatus::NotCommitted {
        return Err(Error::MatrixNotCommitted);
    }
    let block_index = layout.block_index(T::ID)?;
    let ordinal = layout.ordinal_for_block(block_index, key)?;
    let offset = layout.slot_offset(block_index, ordinal)?;
    let stride = layout.blocks[block_index].slot_stride;
    layout
        .read_limits
        .check(ReadLimitKey::RecordPayloadLen, stride)?;
    layout
        .read_limits
        .check(ReadLimitKey::MaterializedBytes, stride)?;
    file.seek(SeekFrom::Start(offset))?;
    let mut payload = filled_bytes(stride, 0)?;
    file.read_exact(&mut payload)?;
    verify_cell_crc(layout, file, block_index, ordinal, &payload)?;
    Ok(payload)
}

pub(crate) fn aux_len(layout: &MatrixLayout, name: &str) -> Result<u64> {
    layout.ensure_fatal_access_allowed()?;
    Ok(layout.aux(name)?.byte_len)
}

pub(crate) fn read_aux_at_len(
    layout: &MatrixLayout,
    file: &mut File,
    logical_file_len: u64,
    name: &str,
    offset: u64,
    len: u64,
) -> Result<Vec<u8>> {
    layout.ensure_fatal_access_allowed()?;
    layout
        .read_limits
        .check(ReadLimitKey::FileLen, logical_file_len)?;
    layout
        .read_limits
        .check(ReadLimitKey::RecordPayloadLen, len)?;
    layout
        .read_limits
        .check(ReadLimitKey::MaterializedBytes, len)?;
    let absolute = aux_absolute_offset(layout.aux(name)?, offset, len)?;
    validate_range(absolute, len, logical_file_len)?;
    file.seek(SeekFrom::Start(absolute))?;
    let mut payload = filled_bytes(len, 0)?;
    file.read_exact(&mut payload)?;
    Ok(payload)
}

pub(crate) fn write_aux_at_len(
    layout: &MatrixLayout,
    file: &mut File,
    logical_file_len: u64,
    name: &str,
    offset: u64,
    payload: &[u8],
) -> Result<()> {
    let len = payload
        .len()
        .try_into()
        .map_err(|_| Error::InvalidMatrixLayout)?;
    layout.ensure_fatal_access_allowed()?;
    layout
        .read_limits
        .check(ReadLimitKey::FileLen, logical_file_len)?;
    layout
        .read_limits
        .check(ReadLimitKey::RecordPayloadLen, len)?;
    let absolute = aux_absolute_offset(layout.aux(name)?, offset, len)?;
    validate_range(absolute, len, logical_file_len)?;
    file.seek(SeekFrom::Start(absolute))?;
    file.write_all(payload)?;
    Ok(())
}

#[cfg(feature = "mmap")]
pub(crate) fn cell_payload_parts<T: VarveMatrixBlock>(
    spec: FormatSpec,
    layout: &MatrixLayout,
    key: MatrixKey,
) -> Result<(u64, u64, Option<u64>)> {
    ensure_matrix_block::<T>(spec)?;
    if cell_status::<T>(spec, layout, key)? == MatrixCellStatus::NotCommitted {
        return Err(Error::MatrixNotCommitted);
    }
    let block_index = layout.block_index(T::ID)?;
    let ordinal = layout.ordinal_for_block(block_index, key)?;
    let offset = layout.slot_offset(block_index, ordinal)?;
    let block = &layout.blocks[block_index];
    let crc_offset = block
        .crc_offset
        .map(|crc_offset| indexed_crc_offset(crc_offset, ordinal))
        .transpose()?;
    Ok((offset, block.slot_stride, crc_offset))
}

#[cfg(feature = "mmap")]
pub(crate) fn verify_payload_crc_bytes(
    payload_offset: u64,
    payload: &[u8],
    expected: u32,
) -> Result<()> {
    let actual = crc32_bytes(payload)?;
    if actual != expected {
        return Err(Error::MatrixChecksumMismatch {
            offset: payload_offset,
            expected,
            actual,
        });
    }
    Ok(())
}

pub(crate) fn resume_signal(layout: &MatrixLayout, category: &str) -> Result<MatrixResumeSignal> {
    layout.ensure_fatal_access_allowed()?;
    let commit = &layout.commits[layout.commit_index(category)?];
    let committed = count_committed(commit)?;
    if committed == 0 || committed == commit.bit_count {
        Ok(MatrixResumeSignal::Clean)
    } else {
        Ok(MatrixResumeSignal::Partial {
            committed,
            total: commit.bit_count,
        })
    }
}

pub(crate) fn sidecar_resume_signal(
    layout: &MatrixLayout,
    category: &str,
    sidecar_exists: bool,
) -> Result<MatrixResumeSignal> {
    layout.ensure_fatal_access_allowed()?;
    layout.read_limits.check(ReadLimitKey::SidecarLen, 0)?;
    let commit = &layout.commits[layout.commit_index(category)?];
    let committed = count_committed(commit)?;
    if committed == 0 || committed == commit.bit_count {
        if sidecar_exists {
            Ok(MatrixResumeSignal::DiscardRecommended)
        } else {
            Ok(MatrixResumeSignal::Clean)
        }
    } else if sidecar_exists {
        Ok(MatrixResumeSignal::ResumeAvailable {
            committed,
            total: commit.bit_count,
        })
    } else {
        Ok(MatrixResumeSignal::RestartRecommended)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[allow(dead_code)]
pub(crate) struct MatrixSidecarReadPlan {
    pub(crate) format_magic_offset: u64,
    pub(crate) category_offset: u64,
    pub(crate) payload_offset: u64,
    pub(crate) payload_len: u64,
    pub(crate) total_len: u64,
}

#[allow(dead_code)]
pub(crate) fn check_matrix_sidecar_file_len(spec: FormatSpec, file_len: u64) -> Result<()> {
    spec.read_limits.check(ReadLimitKey::SidecarLen, file_len)
}

#[allow(dead_code)]
pub(crate) fn matrix_sidecar_write_len(
    spec: FormatSpec,
    fixed_header_len: u64,
    format_magic_len: u64,
    category_len: u64,
    payload_len: u64,
) -> Result<u64> {
    spec.read_limits
        .check(ReadLimitKey::MaterializedBytes, payload_len)?;
    let metadata_len = fixed_header_len
        .checked_add(format_magic_len)
        .and_then(|len| len.checked_add(category_len))
        .ok_or(Error::ResourceArithmeticOverflow {
            resource: MATRIX_SIDECAR_RESOURCE,
        })?;
    let total_len =
        metadata_len
            .checked_add(payload_len)
            .ok_or(Error::ResourceArithmeticOverflow {
                resource: MATRIX_SIDECAR_RESOURCE,
            })?;
    check_matrix_sidecar_file_len(spec, total_len)?;
    Ok(total_len)
}

#[allow(clippy::too_many_arguments)]
#[allow(dead_code)]
pub(crate) fn matrix_sidecar_read_plan(
    spec: FormatSpec,
    file_len: u64,
    fixed_header_len: u64,
    format_magic_len: u64,
    category_len: u64,
    payload_len: u64,
    flags: u16,
    reserved: u16,
    trailing_reserved: u32,
) -> Result<MatrixSidecarReadPlan> {
    check_matrix_sidecar_file_len(spec, file_len)?;
    if flags != 0 || reserved != 0 || trailing_reserved != 0 {
        return Err(Error::InvalidMatrixSidecar);
    }
    let total_len = matrix_sidecar_write_len(
        spec,
        fixed_header_len,
        format_magic_len,
        category_len,
        payload_len,
    )?;
    if total_len != file_len {
        return Err(Error::InvalidMatrixSidecar);
    }
    let category_offset = fixed_header_len.checked_add(format_magic_len).ok_or(
        Error::ResourceArithmeticOverflow {
            resource: MATRIX_SIDECAR_RESOURCE,
        },
    )?;
    let payload_offset =
        category_offset
            .checked_add(category_len)
            .ok_or(Error::ResourceArithmeticOverflow {
                resource: MATRIX_SIDECAR_RESOURCE,
            })?;
    Ok(MatrixSidecarReadPlan {
        format_magic_offset: fixed_header_len,
        category_offset,
        payload_offset,
        payload_len,
        total_len,
    })
}

pub(crate) fn recovery_report(layout: &MatrixLayout) -> MatrixRecoveryReport {
    let mut findings = layout.crc_findings.clone();
    let mut recommended_actions = findings
        .iter()
        .filter_map(|finding| match finding.kind {
            MatrixCorruptionKind::Slot => Some(MatrixRecoveryAction::ClearCategory {
                category: "unknown".to_string(),
            }),
            _ => None,
        })
        .collect::<Vec<_>>();
    for commit in &layout.commits {
        if let Some(finding) = &commit.quarantine_finding {
            findings.push(finding.clone());
            recommended_actions.push(match commit.kind {
                MatrixCommitKind::Cell => MatrixRecoveryAction::RebuildCommitMap {
                    category: Some(commit.name.clone()),
                },
                MatrixCommitKind::Single | MatrixCommitKind::PerChannel => {
                    MatrixRecoveryAction::ClearCategory {
                        category: commit.name.clone(),
                    }
                }
            });
        }
        if let Ok(committed) = count_committed(commit)
            && committed > 0
            && committed < commit.bit_count
        {
            findings.push(MatrixRecoveryFinding {
                kind: MatrixCorruptionKind::Sidecar,
                severity: MatrixCorruptionSeverity::Advisory,
                message: format!(
                    "partial matrix progress in category {}: {committed}/{}",
                    commit.name, commit.bit_count
                ),
            });
            recommended_actions.push(MatrixRecoveryAction::Resume);
        }
    }
    MatrixRecoveryReport {
        findings,
        recommended_actions,
    }
}

pub(crate) fn rebuild_commit_map_from_crc<T: VarveMatrixBlock>(
    spec: FormatSpec,
    layout: &mut MatrixLayout,
    file: &mut File,
) -> Result<u64> {
    ensure_matrix_block::<T>(spec)?;
    layout.ensure_fatal_access_allowed()?;
    let block_index = layout.block_index(T::ID)?;
    let block = &layout.blocks[block_index];
    let Some(crc_offset) = block.crc_offset else {
        return Err(Error::IntegrityFeatureDisabled);
    };
    let commit_index = layout.commit_index(T::CATEGORY)?;
    if layout.commits[commit_index].kind != MatrixCommitKind::Cell
        || layout.commits[commit_index].bit_count != block.cell_count
    {
        return Err(Error::InvalidMatrixLayout);
    }

    let rebuilt_len = usize_to_u64(layout.commits[commit_index].bits.len())?;
    let peak_bitmap_bytes = layout
        .resident_bitmap_bytes
        .checked_add(rebuilt_len)
        .ok_or(Error::ResourceArithmeticOverflow {
            resource: ReadLimitKey::MatrixBitmapBytes.resource(),
        })?;
    layout
        .read_limits
        .check(ReadLimitKey::MatrixBitmapBytes, peak_bitmap_bytes)?;
    let mut rebuilt = filled_bytes(rebuilt_len, 0)?;
    let mut committed = 0u64;
    for ordinal in 0..block.cell_count {
        let slot_offset = layout.slot_offset(block_index, ordinal)?;
        let actual = crc32_file_range(file, slot_offset, block.slot_stride)?;
        let stored = read_crc_at(file, indexed_crc_offset(crc_offset, ordinal)?)?;
        let valid = get_bit(&block.crc_valid_bits, ordinal)? && actual == stored;
        if valid {
            committed += 1;
        }
        set_bit(&mut rebuilt, ordinal, valid)?;
    }

    let commit = &layout.commits[commit_index];
    write_commit_crc(file, commit.crc_offset, &rebuilt)?;
    file.seek(SeekFrom::Start(commit.map_offset))?;
    file.write_all(&rebuilt)?;
    let commit = &mut layout.commits[commit_index];
    let quarantined_len = commit
        .quarantined_raw_bits
        .as_ref()
        .map(|bits| usize_to_u64(bits.len()))
        .transpose()?
        .unwrap_or(0);
    commit.bits = Arc::new(rebuilt);
    commit.quarantined_raw_bits = None;
    commit.quarantine_finding = None;
    layout.resident_bitmap_bytes = layout
        .resident_bitmap_bytes
        .checked_sub(quarantined_len)
        .ok_or(Error::InvalidMatrixLayout)?;
    Ok(committed)
}

pub(crate) fn is_single_committed(layout: &MatrixLayout, name: &str) -> Result<bool> {
    let commit = &layout.commits[layout.commit_index(name)?];
    if commit.kind != MatrixCommitKind::Single {
        return Err(Error::MatrixCommitMissing(name.to_string()));
    }
    ensure_commit_publishable(layout, name)?;
    get_bit(&commit.bits, 0)
}

pub(crate) fn set_single_committed(
    layout: &mut MatrixLayout,
    file: &mut File,
    name: &str,
    value: bool,
) -> Result<()> {
    let index = layout.commit_index(name)?;
    if layout.commits[index].kind != MatrixCommitKind::Single {
        return Err(Error::MatrixCommitMissing(name.to_string()));
    }
    set_commit_bit(layout, file, index, 0, value)
}

pub(crate) fn is_channel_committed(
    layout: &MatrixLayout,
    name: &str,
    channel: u64,
) -> Result<bool> {
    let commit = &layout.commits[layout.commit_index(name)?];
    if commit.kind != MatrixCommitKind::PerChannel || channel >= commit.bit_count {
        return Err(Error::MatrixCommitMissing(name.to_string()));
    }
    ensure_commit_publishable(layout, name)?;
    get_bit(&commit.bits, channel)
}

pub(crate) fn set_channel_committed(
    layout: &mut MatrixLayout,
    file: &mut File,
    name: &str,
    channel: u64,
    value: bool,
) -> Result<()> {
    let index = layout.commit_index(name)?;
    if layout.commits[index].kind != MatrixCommitKind::PerChannel
        || channel >= layout.commits[index].bit_count
    {
        return Err(Error::MatrixCommitMissing(name.to_string()));
    }
    set_commit_bit(layout, file, index, channel, value)
}

fn ensure_matrix_block<T: VarveMatrixBlock>(spec: FormatSpec) -> Result<()> {
    // DEF-01: run the common registration gate first, exactly like the
    // fixed/variable block paths. It enforces the process-local first-seen
    // schema-fingerprint and keyedness contract, so a manual matrix type
    // with the same shape/stride but different codec/decode semantics is
    // rejected before any cell read, write, or mmap view.
    crate::collections::ensure_registered_block::<T>(spec)?;
    let descriptor = spec.block(T::ID).ok_or(Error::UnregisteredBlock(T::ID))?;
    if descriptor.kind != BlockKind::Matrix || T::KIND != BlockKind::Matrix {
        return Err(Error::BlockKindMismatch {
            expected: BlockKind::Matrix,
            actual: T::KIND,
        });
    }
    if descriptor.version != T::VERSION {
        return Err(Error::BlockVersionMismatch {
            block_id: T::ID,
            expected: descriptor.version,
            actual: T::VERSION,
        });
    }
    let matrix = spec
        .matrix_blocks
        .iter()
        .find(|block| block.block_id == T::ID)
        .ok_or(Error::MatrixBlockMissing(T::ID))?;
    if matrix.dimensions != T::DIMENSIONS
        || matrix.category != T::CATEGORY
        || matrix.slot_stride != T::SLOT_STRIDE
    {
        return Err(Error::InvalidMatrixLayout);
    }
    Ok(())
}

fn block_index_for_category(
    spec: FormatSpec,
    layout: &MatrixLayout,
    category: &str,
) -> Result<usize> {
    let block = spec
        .matrix_blocks
        .iter()
        .find(|block| block.category == category)
        .ok_or_else(|| Error::MatrixCommitMissing(category.to_string()))?;
    layout.block_index(block.block_id)
}

fn ensure_commit_publishable(layout: &MatrixLayout, category: &str) -> Result<()> {
    layout.ensure_fatal_access_allowed()?;
    let commit = &layout.commits[layout.commit_index(category)?];
    if commit.quarantined_raw_bits.is_some() {
        return Err(Error::MatrixCommitQuarantined(category.to_string()));
    }
    Ok(())
}

fn set_cell_commit(
    layout: &mut MatrixLayout,
    file: &mut File,
    category: &str,
    ordinal: u64,
    value: bool,
) -> Result<()> {
    let (commit_index, update) = prepare_cell_commit(layout, category, ordinal, value)?;
    apply_commit_bit(layout, file, commit_index, update)
}

fn prepare_cell_commit(
    layout: &MatrixLayout,
    category: &str,
    ordinal: u64,
    value: bool,
) -> Result<(usize, CommitBitUpdate)> {
    let commit_index = layout.commit_index(category)?;
    if layout.commits[commit_index].kind != MatrixCommitKind::Cell {
        return Err(Error::MatrixCommitMissing(category.to_string()));
    }
    let update = prepare_commit_bit(layout, commit_index, ordinal, value)?;
    Ok((commit_index, update))
}

fn set_commit_bit(
    layout: &mut MatrixLayout,
    file: &mut File,
    commit_index: usize,
    ordinal: u64,
    value: bool,
) -> Result<()> {
    let update = prepare_commit_bit(layout, commit_index, ordinal, value)?;
    apply_commit_bit(layout, file, commit_index, update)
}

struct BitmapByteUpdate {
    byte_index: usize,
    byte_offset: u64,
    byte_value: u8,
}

struct CommitBitUpdate {
    bitmap: BitmapByteUpdate,
    crc: Option<(u64, u32)>,
}

fn prepare_bitmap_update(
    bits: &[u8],
    bit_count: u64,
    base_offset: u64,
    ordinal: u64,
    value: bool,
) -> Result<BitmapByteUpdate> {
    if ordinal >= bit_count {
        return Err(Error::InvalidMatrixLayout);
    }
    let byte_index = usize::try_from(ordinal / 8).map_err(|_| Error::InvalidMatrixLayout)?;
    let current = *bits.get(byte_index).ok_or(Error::InvalidMatrixLayout)?;
    let mask = 1u8 << (ordinal % 8);
    let byte_value = if value {
        current | mask
    } else {
        current & !mask
    };
    let byte_offset = base_offset
        .checked_add(u64::try_from(byte_index).map_err(|_| Error::InvalidMatrixLayout)?)
        .ok_or(Error::InvalidMatrixLayout)?;
    Ok(BitmapByteUpdate {
        byte_index,
        byte_offset,
        byte_value,
    })
}

fn write_bitmap_byte(file: &mut File, update: &BitmapByteUpdate) -> Result<()> {
    #[cfg(test)]
    if FAIL_NEXT_BITMAP_WRITE.replace(false) {
        return Err(Error::Io(std::io::Error::other(
            "injected matrix bitmap write failure",
        )));
    }

    file.seek(SeekFrom::Start(update.byte_offset))?;
    file.write_all(&[update.byte_value])?;
    Ok(())
}

fn prepare_commit_bit(
    layout: &MatrixLayout,
    commit_index: usize,
    ordinal: u64,
    value: bool,
) -> Result<CommitBitUpdate> {
    layout.ensure_fatal_access_allowed()?;
    let commit = &layout.commits[commit_index];
    if commit.quarantined_raw_bits.is_some() {
        return Err(Error::MatrixCommitQuarantined(commit.name.clone()));
    }
    let bitmap = prepare_bitmap_update(
        &commit.bits,
        commit.bit_count,
        commit.map_offset,
        ordinal,
        value,
    )?;
    let crc = match commit.crc_offset {
        Some(offset) => Some((
            offset,
            crc32_bytes_with_replacement(&commit.bits, bitmap.byte_index, bitmap.byte_value)?,
        )),
        None => None,
    };
    Ok(CommitBitUpdate { bitmap, crc })
}

fn apply_commit_bit(
    layout: &mut MatrixLayout,
    file: &mut File,
    commit_index: usize,
    update: CommitBitUpdate,
) -> Result<()> {
    if let Some((offset, crc)) = update.crc {
        write_crc_at(file, offset, crc)?;
    }
    write_bitmap_byte(file, &update.bitmap)?;
    *Arc::make_mut(&mut layout.commits[commit_index].bits)
        .get_mut(update.bitmap.byte_index)
        .ok_or(Error::InvalidMatrixLayout)? = update.bitmap.byte_value;
    Ok(())
}

fn write_commit_crc(file: &mut File, crc_offset: Option<u64>, bits: &[u8]) -> Result<()> {
    let Some(crc_offset) = crc_offset else {
        return Ok(());
    };
    let crc = crc32_bytes(bits)?;
    write_crc_at(file, crc_offset, crc)
}

fn update_cell_crc(
    layout: &MatrixLayout,
    file: &mut File,
    block_index: usize,
    ordinal: u64,
) -> Result<()> {
    let Some(crc_offset) = layout.blocks[block_index].crc_offset else {
        return Ok(());
    };
    let offset = layout.slot_offset(block_index, ordinal)?;
    let stride = layout.blocks[block_index].slot_stride;
    let crc = crc32_file_range(file, offset, stride)?;
    write_crc_at(file, indexed_crc_offset(crc_offset, ordinal)?, crc)
}

fn clear_cell_crc(
    layout: &MatrixLayout,
    file: &mut File,
    block_index: usize,
    ordinal: u64,
) -> Result<()> {
    let Some(crc_offset) = layout.blocks[block_index].crc_offset else {
        return Ok(());
    };
    let zero_crc = crc32_zeroes(layout.blocks[block_index].slot_stride)?;
    write_crc_at(file, indexed_crc_offset(crc_offset, ordinal)?, zero_crc)
}

fn set_cell_crc_valid(
    layout: &mut MatrixLayout,
    file: &mut File,
    block_index: usize,
    ordinal: u64,
    value: bool,
) -> Result<()> {
    let Some(update) = prepare_cell_crc_valid(layout, block_index, ordinal, value)? else {
        return Ok(());
    };
    apply_cell_crc_valid(layout, file, block_index, update)
}

fn prepare_cell_crc_valid(
    layout: &MatrixLayout,
    block_index: usize,
    ordinal: u64,
    value: bool,
) -> Result<Option<BitmapByteUpdate>> {
    let block = &layout.blocks[block_index];
    let Some(valid_offset) = block.crc_valid_offset else {
        return Ok(None);
    };
    prepare_bitmap_update(
        &block.crc_valid_bits,
        block.cell_count,
        valid_offset,
        ordinal,
        value,
    )
    .map(Some)
}

fn apply_cell_crc_valid(
    layout: &mut MatrixLayout,
    file: &mut File,
    block_index: usize,
    update: BitmapByteUpdate,
) -> Result<()> {
    write_bitmap_byte(file, &update)?;
    *Arc::make_mut(&mut layout.blocks[block_index].crc_valid_bits)
        .get_mut(update.byte_index)
        .ok_or(Error::InvalidMatrixLayout)? = update.byte_value;
    Ok(())
}

fn verify_cell_crc(
    layout: &MatrixLayout,
    file: &mut File,
    block_index: usize,
    ordinal: u64,
    payload: &[u8],
) -> Result<()> {
    let Some(crc_offset) = layout.blocks[block_index].crc_offset else {
        return Ok(());
    };
    let expected = read_crc_at(file, indexed_crc_offset(crc_offset, ordinal)?)?;
    let actual = crc32_bytes(payload)?;
    if expected != actual {
        return Err(Error::MatrixChecksumMismatch {
            offset: layout.slot_offset(block_index, ordinal)?,
            expected,
            actual,
        });
    }
    Ok(())
}

fn slot_is_all_zero(
    layout: &MatrixLayout,
    file: &mut File,
    block_index: usize,
    ordinal: u64,
) -> Result<bool> {
    let offset = layout.slot_offset(block_index, ordinal)?;
    let stride = layout.blocks[block_index].slot_stride;
    file.seek(SeekFrom::Start(offset))?;
    let mut buffer = [0u8; 64 * 1024];
    let mut remaining = stride;
    while remaining != 0 {
        let chunk_len = usize::try_from(remaining.min(buffer.len() as u64))
            .map_err(|_| Error::LengthOverflow { value: remaining })?;
        file.read_exact(&mut buffer[..chunk_len])?;
        if buffer[..chunk_len].iter().any(|byte| *byte != 0) {
            return Ok(false);
        }
        remaining -= chunk_len as u64;
    }
    Ok(true)
}

#[cfg(feature = "integrity")]
fn crc32_file_range(file: &mut File, offset: u64, len: u64) -> Result<u32> {
    file.seek(SeekFrom::Start(offset))?;
    let mut hasher = crc32fast::Hasher::new();
    let mut buffer = [0u8; 64 * 1024];
    let mut remaining = len;
    while remaining != 0 {
        let chunk_len = usize::try_from(remaining.min(buffer.len() as u64))
            .map_err(|_| Error::LengthOverflow { value: remaining })?;
        file.read_exact(&mut buffer[..chunk_len])?;
        hasher.update(&buffer[..chunk_len]);
        remaining -= chunk_len as u64;
    }
    Ok(hasher.finalize())
}

#[cfg(not(feature = "integrity"))]
fn crc32_file_range(_file: &mut File, _offset: u64, _len: u64) -> Result<u32> {
    Err(Error::IntegrityFeatureDisabled)
}

fn count_committed(commit: &MatrixCommitLayout) -> Result<u64> {
    let mut count = 0u64;
    for ordinal in 0..commit.bit_count {
        if get_bit(&commit.bits, ordinal)? {
            count += 1;
        }
    }
    Ok(count)
}

fn dimension_values(
    spec: FormatSpec,
    dims: &MatrixDimensions,
) -> Result<Vec<MatrixDimensionValue>> {
    let mut values = Vec::new();
    try_reserve_vec(
        &mut values,
        spec.matrix_dimensions.len(),
        MATRIX_DESCRIPTOR_RESOURCE,
    )?;
    for dimension in spec.matrix_dimensions {
        let value = dims
            .get(dimension.name)
            .ok_or_else(|| Error::MatrixDimensionMissing(dimension.name.to_string()))?;
        values.push(MatrixDimensionValue {
            name: dimension.name.to_string(),
            value,
        });
    }
    Ok(values)
}

fn matrix_cell_counts(
    spec: FormatSpec,
    dimensions: &[MatrixDimensionValue],
) -> Result<HashMap<String, u64>> {
    let mut counts = HashMap::new();
    try_reserve_map(
        &mut counts,
        spec.matrix_blocks.len(),
        ReadLimitKey::MatrixCells.resource(),
    )?;
    let mut aggregate = 0u64;
    for block in spec.matrix_blocks {
        let dim0 = dimensions
            .iter()
            .find(|dimension| dimension.name == block.dimensions[0])
            .map(|dimension| dimension.value)
            .ok_or_else(|| Error::MatrixDimensionMissing(block.dimensions[0].to_string()))?;
        let dim1 = dimensions
            .iter()
            .find(|dimension| dimension.name == block.dimensions[1])
            .map(|dimension| dimension.value)
            .ok_or_else(|| Error::MatrixDimensionMissing(block.dimensions[1].to_string()))?;
        let count = dim0
            .checked_mul(dim1)
            .ok_or(Error::ResourceArithmeticOverflow {
                resource: ReadLimitKey::MatrixCells.resource(),
            })?;
        spec.read_limits.check(ReadLimitKey::MatrixCells, count)?;
        aggregate = aggregate
            .checked_add(count)
            .ok_or(Error::ResourceArithmeticOverflow {
                resource: ReadLimitKey::MatrixCells.resource(),
            })?;
        spec.read_limits
            .check(ReadLimitKey::MatrixCells, aggregate)?;
        match counts.insert(block.category.to_string(), count) {
            Some(existing) if existing != count => return Err(Error::InvalidMatrixLayout),
            _ => {}
        }
    }
    Ok(counts)
}

fn matrix_commit_plans(
    spec: FormatSpec,
    dimensions: &[MatrixDimensionValue],
    cell_counts: &HashMap<String, u64>,
) -> Result<Vec<CommitPlan>> {
    let mut plans = Vec::new();
    try_reserve_vec(
        &mut plans,
        spec.matrix_commits.len(),
        MATRIX_DESCRIPTOR_RESOURCE,
    )?;
    for commit in spec.matrix_commits {
        let bit_count = match commit.kind {
            MatrixCommitKind::Cell => *cell_counts
                .get(commit.name)
                .ok_or_else(|| Error::MatrixCommitMissing(commit.name.to_string()))?,
            MatrixCommitKind::Single => 1,
            MatrixCommitKind::PerChannel => per_channel_count(dimensions)?,
        };
        plans.push((commit.name.to_string(), commit.kind, bit_count));
    }
    Ok(plans)
}

fn matrix_descriptor_table_lengths(spec: FormatSpec) -> Result<(u64, u64, u64)> {
    let dimension_table_len = spec
        .matrix_dimensions
        .iter()
        .try_fold(0u64, |len, dimension| {
            let entry_len = usize_to_u64(dimension.name.len())?
                .checked_add(10)
                .ok_or(Error::InvalidMatrixLayout)?;
            len.checked_add(entry_len).ok_or(Error::InvalidMatrixLayout)
        })?;
    let block_table_len = usize_to_u64(spec.matrix_blocks.len())?
        .checked_mul(44)
        .ok_or(Error::InvalidMatrixLayout)?;
    let category_table_len = spec.matrix_commits.iter().try_fold(0u64, |len, commit| {
        let entry_len = usize_to_u64(commit.name.len())?
            .checked_add(30)
            .ok_or(Error::InvalidMatrixLayout)?;
        len.checked_add(entry_len).ok_or(Error::InvalidMatrixLayout)
    })?;
    Ok((dimension_table_len, block_table_len, category_table_len))
}

fn matrix_commit_map_len(commit_plans: &[CommitPlan]) -> Result<u64> {
    commit_plans
        .iter()
        .try_fold(0u64, |len, (_, _, bit_count)| {
            len.checked_add(bit_bytes(*bit_count)?)
                .ok_or(Error::InvalidMatrixLayout)
        })
}

fn check_matrix_dimensions(limits: ReadLimits, dimensions: &[MatrixDimensionValue]) -> Result<()> {
    for dimension in dimensions {
        limits.check(ReadLimitKey::MatrixDimension, dimension.value)?;
    }
    Ok(())
}

fn check_matrix_metadata_limit(
    limits: ReadLimits,
    dimension_table_len: u64,
    block_table_len: u64,
    category_table_len: u64,
) -> Result<()> {
    let metadata_len = u64::from(VMAT_HEADER_LEN)
        .checked_add(dimension_table_len)
        .and_then(|len| len.checked_add(block_table_len))
        .and_then(|len| len.checked_add(category_table_len))
        .ok_or(Error::ResourceArithmeticOverflow {
            resource: ReadLimitKey::MatrixMetadataBytes.resource(),
        })?;
    limits.check(ReadLimitKey::MatrixMetadataBytes, metadata_len)
}

fn matrix_block_bitmap_len(spec: FormatSpec, cell_counts: &HashMap<String, u64>) -> Result<u64> {
    spec.matrix_blocks.iter().try_fold(0u64, |total, block| {
        let cell_count = *cell_counts
            .get(block.category)
            .ok_or_else(|| Error::MatrixCommitMissing(block.category.to_string()))?;
        total
            .checked_add(bit_bytes(cell_count)?)
            .ok_or(Error::ResourceArithmeticOverflow {
                resource: ReadLimitKey::MatrixBitmapBytes.resource(),
            })
    })
}

fn matrix_resident_bitmap_len(commit_map_len: u64, block_bitmap_len: u64) -> Result<u64> {
    block_bitmap_len
        .checked_mul(2)
        .and_then(|write_maps| write_maps.checked_add(commit_map_len))
        .ok_or(Error::ResourceArithmeticOverflow {
            resource: ReadLimitKey::MatrixBitmapBytes.resource(),
        })
}

fn matrix_accounted_crc_len(
    crc_enabled: bool,
    region_crc_len: u64,
    block_bitmap_len: u64,
) -> Result<u64> {
    if crc_enabled {
        region_crc_len
            .checked_add(block_bitmap_len)
            .ok_or(Error::ResourceArithmeticOverflow {
                resource: ReadLimitKey::MatrixCrcBytes.resource(),
            })
    } else {
        Ok(0)
    }
}

fn zero_commit_bitmaps(commit_plans: &[CommitPlan]) -> Result<Vec<Vec<u8>>> {
    let mut bitmaps = Vec::new();
    try_reserve_vec(
        &mut bitmaps,
        commit_plans.len(),
        ReadLimitKey::MatrixBitmapBytes.resource(),
    )?;
    for (_, _, bit_count) in commit_plans {
        bitmaps.push(filled_bytes(bit_bytes(*bit_count)?, 0)?);
    }
    Ok(bitmaps)
}

fn zero_crc_valid_bitmaps(
    spec: FormatSpec,
    crc_enabled: bool,
    block_offsets: &HashMap<u32, (u64, u64, u64)>,
) -> Result<HashMap<u32, Vec<u8>>> {
    let mut bitmaps = HashMap::new();
    if !crc_enabled {
        return Ok(bitmaps);
    }
    try_reserve_map(
        &mut bitmaps,
        spec.matrix_blocks.len(),
        ReadLimitKey::MatrixCrcBytes.resource(),
    )?;
    for block in spec.matrix_blocks {
        let (_, _, cell_count) = *block_offsets
            .get(&block.block_id)
            .ok_or(Error::MatrixBlockMissing(block.block_id))?;
        bitmaps.insert(block.block_id, filled_bytes(bit_bytes(cell_count)?, 0)?);
    }
    Ok(bitmaps)
}

fn matrix_slot_region_len(spec: FormatSpec, cell_counts: &HashMap<String, u64>) -> Result<u64> {
    spec.matrix_blocks.iter().try_fold(0u64, |len, block| {
        let cell_count = *cell_counts
            .get(block.category)
            .ok_or_else(|| Error::MatrixCommitMissing(block.category.to_string()))?;
        let block_len = cell_count
            .checked_mul(block.slot_stride)
            .ok_or(Error::InvalidMatrixLayout)?;
        len.checked_add(block_len).ok_or(Error::InvalidMatrixLayout)
    })
}

fn per_channel_count(dimensions: &[MatrixDimensionValue]) -> Result<u64> {
    dimensions
        .iter()
        .find(|dimension| {
            matches!(
                dimension.name.as_str(),
                "ch" | "channel" | "channels" | "n_channels"
            )
        })
        .or_else(|| dimensions.get(1))
        .map(|dimension| dimension.value)
        .ok_or_else(|| Error::MatrixDimensionMissing("channel".to_string()))
}

fn matrix_aux_region_len(spec: FormatSpec) -> Result<u64> {
    spec.matrix_aux.iter().try_fold(0u64, |acc, aux| {
        acc.checked_add(aux.byte_len)
            .ok_or(Error::InvalidMatrixLayout)
    })
}

fn matrix_aux_offsets(spec: FormatSpec, start: u64) -> Result<HashMap<String, (u64, u64)>> {
    let mut offsets = HashMap::new();
    try_reserve_map(
        &mut offsets,
        spec.matrix_aux.len(),
        MATRIX_DESCRIPTOR_RESOURCE,
    )?;
    let mut next = start;
    for aux in spec.matrix_aux {
        offsets.insert(aux.name.to_string(), (next, aux.byte_len));
        next = next
            .checked_add(aux.byte_len)
            .ok_or(Error::InvalidMatrixLayout)?;
    }
    Ok(offsets)
}

fn aux_absolute_offset(aux: &MatrixAuxLayout, offset: u64, len: u64) -> Result<u64> {
    let end = offset
        .checked_add(len)
        .ok_or_else(|| Error::MatrixAuxOutOfBounds {
            name: aux.name.clone(),
            offset,
            len,
            byte_len: aux.byte_len,
        })?;
    if end > aux.byte_len {
        return Err(Error::MatrixAuxOutOfBounds {
            name: aux.name.clone(),
            offset,
            len,
            byte_len: aux.byte_len,
        });
    }
    aux.offset
        .checked_add(offset)
        .ok_or(Error::InvalidMatrixLayout)
}

#[allow(clippy::too_many_arguments)]
fn layout_from_parts(
    dimensions: Vec<MatrixDimensionValue>,
    spec: FormatSpec,
    commit_plans: &[CommitPlan],
    commit_offsets: &HashMap<String, (u64, u64)>,
    block_offsets: &HashMap<u32, (u64, u64, u64)>,
    aux_offsets: &HashMap<String, (u64, u64)>,
    commit_bits: Vec<Vec<u8>>,
    crc: Option<MatrixCrcLayout>,
    mut crc_valid_bits: HashMap<u32, Vec<u8>>,
    mut commit_findings: HashMap<String, MatrixRecoveryFinding>,
    crc_findings: Vec<MatrixRecoveryFinding>,
    append_log_start: u64,
    mut resident_bitmap_bytes: u64,
) -> Result<MatrixLayout> {
    if commit_bits.len() != commit_plans.len() {
        return Err(Error::InvalidMatrixLayout);
    }
    let has_fatal_finding = crc_findings
        .iter()
        .chain(commit_findings.values())
        .any(|finding| finding.severity == MatrixCorruptionSeverity::Fatal);
    let fatal_access_blocked = has_fatal_finding && !spec.matrix_fatal_forensics;
    let mut commits = Vec::new();
    try_reserve_vec(&mut commits, commit_plans.len(), MATRIX_DESCRIPTOR_RESOURCE)?;
    for (commit_index, ((name, kind, bit_count), raw_bits)) in
        commit_plans.iter().zip(commit_bits).enumerate()
    {
        let (map_offset, map_len) = *commit_offsets
            .get(name)
            .ok_or_else(|| Error::MatrixCommitMissing(name.clone()))?;
        if usize_to_u64(raw_bits.len())? != map_len {
            return Err(Error::InvalidMatrixLayout);
        }
        let quarantine_finding = commit_findings.remove(name);
        let (bits, quarantined_raw_bits) = if quarantine_finding.is_some() {
            resident_bitmap_bytes = resident_bitmap_bytes.checked_add(map_len).ok_or(
                Error::ResourceArithmeticOverflow {
                    resource: ReadLimitKey::MatrixBitmapBytes.resource(),
                },
            )?;
            spec.read_limits
                .check(ReadLimitKey::MatrixBitmapBytes, resident_bitmap_bytes)?;
            (
                Arc::new(filled_bytes(map_len, 0)?),
                Some(Arc::new(raw_bits)),
            )
        } else {
            (Arc::new(raw_bits), None)
        };
        commits.push(MatrixCommitLayout {
            name: name.clone(),
            kind: *kind,
            bit_count: *bit_count,
            map_offset,
            bits,
            quarantined_raw_bits,
            quarantine_finding,
            crc_offset: crc
                .as_ref()
                .map(|crc| {
                    crc.commit_crc_offset(
                        u64::try_from(commit_index).map_err(|_| Error::InvalidMatrixLayout)?,
                    )
                })
                .transpose()?,
        });
    }
    if !commit_findings.is_empty() {
        return Err(Error::InvalidMatrixLayout);
    }
    let mut blocks = Vec::new();
    try_reserve_vec(
        &mut blocks,
        spec.matrix_blocks.len(),
        MATRIX_DESCRIPTOR_RESOURCE,
    )?;
    for (block_index, block) in spec.matrix_blocks.iter().enumerate() {
        let (slot_region_offset, slot_region_len, cell_count) = *block_offsets
            .get(&block.block_id)
            .ok_or(Error::MatrixBlockMissing(block.block_id))?;
        if slot_region_len
            != cell_count
                .checked_mul(block.slot_stride)
                .ok_or(Error::InvalidMatrixLayout)?
        {
            return Err(Error::InvalidMatrixLayout);
        }
        let crc_valid_len = bit_bytes(cell_count)?;
        let crc_valid_bits_for_block = match &crc {
            Some(_) => crc_valid_bits
                .remove(&block.block_id)
                .ok_or(Error::InvalidMatrixLayout)?,
            None => Vec::new(),
        };
        if crc.is_some() && usize_to_u64(crc_valid_bits_for_block.len())? != crc_valid_len {
            return Err(Error::InvalidMatrixLayout);
        }
        let written_bits = filled_bytes(crc_valid_len, 0xFF)?;
        let current_write_bits = filled_bytes(crc_valid_len, 0)?;
        blocks.push(MatrixBlockLayout {
            block_id: block.block_id,
            dimensions: [
                block.dimensions[0].to_string(),
                block.dimensions[1].to_string(),
            ],
            slot_stride: block.slot_stride,
            slot_region_offset,
            cell_count,
            crc_offset: crc
                .as_ref()
                .map(|crc| crc.block_crc_offset(spec, block_offsets, block_index))
                .transpose()?,
            crc_valid_offset: crc
                .as_ref()
                .map(|crc| crc.block_valid_offset(spec, block_offsets, block_index))
                .transpose()?,
            crc_valid_bits: Arc::new(crc_valid_bits_for_block),
            written_bits: Arc::new(written_bits),
            current_write_bits: Arc::new(current_write_bits),
        });
    }
    if !crc_valid_bits.is_empty() {
        return Err(Error::InvalidMatrixLayout);
    }
    let mut aux = Vec::new();
    try_reserve_vec(&mut aux, spec.matrix_aux.len(), MATRIX_DESCRIPTOR_RESOURCE)?;
    for descriptor in spec.matrix_aux {
        let (offset, byte_len) = *aux_offsets
            .get(descriptor.name)
            .ok_or_else(|| Error::MatrixAuxMissing(descriptor.name.to_string()))?;
        if byte_len != descriptor.byte_len {
            return Err(Error::InvalidMatrixLayout);
        }
        aux.push(MatrixAuxLayout {
            name: descriptor.name.to_string(),
            offset,
            byte_len,
        });
    }
    Ok(MatrixLayout {
        dimensions,
        commits,
        blocks,
        aux,
        crc_findings,
        append_log_start,
        read_limits: spec.read_limits,
        resident_bitmap_bytes,
        fatal_access_blocked,
    })
}

impl MatrixCrcLayout {
    fn commit_crc_offset(&self, commit_index: u64) -> Result<u64> {
        self.region_offset
            .checked_add(MCRC_HEADER_LEN)
            .and_then(|offset| {
                commit_index
                    .checked_mul(CRC_LEN)
                    .and_then(|delta| offset.checked_add(delta))
            })
            .ok_or(Error::InvalidMatrixLayout)
    }

    fn block_crc_offset(
        &self,
        spec: FormatSpec,
        block_offsets: &HashMap<u32, (u64, u64, u64)>,
        block_index: usize,
    ) -> Result<u64> {
        let mut offset = self.slot_crc_base(spec)?;
        for block in &spec.matrix_blocks[..block_index] {
            let (_, _, cell_count) = *block_offsets
                .get(&block.block_id)
                .ok_or(Error::MatrixBlockMissing(block.block_id))?;
            offset = offset
                .checked_add(
                    cell_count
                        .checked_mul(CRC_LEN)
                        .ok_or(Error::InvalidMatrixLayout)?,
                )
                .ok_or(Error::InvalidMatrixLayout)?;
        }
        Ok(offset)
    }

    fn block_valid_offset(
        &self,
        spec: FormatSpec,
        block_offsets: &HashMap<u32, (u64, u64, u64)>,
        block_index: usize,
    ) -> Result<u64> {
        let mut offset = self.slot_valid_base(spec, block_offsets)?;
        for block in &spec.matrix_blocks[..block_index] {
            let (_, _, cell_count) = *block_offsets
                .get(&block.block_id)
                .ok_or(Error::MatrixBlockMissing(block.block_id))?;
            offset = offset
                .checked_add(bit_bytes(cell_count)?)
                .ok_or(Error::InvalidMatrixLayout)?;
        }
        Ok(offset)
    }

    fn slot_crc_base(&self, spec: FormatSpec) -> Result<u64> {
        let commit_crc_len = usize_to_u64(spec.matrix_commits.len())?
            .checked_mul(CRC_LEN)
            .ok_or(Error::InvalidMatrixLayout)?;
        self.region_offset
            .checked_add(MCRC_HEADER_LEN)
            .and_then(|value| value.checked_add(commit_crc_len))
            .ok_or(Error::InvalidMatrixLayout)
    }

    fn slot_valid_base(
        &self,
        spec: FormatSpec,
        block_offsets: &HashMap<u32, (u64, u64, u64)>,
    ) -> Result<u64> {
        let mut offset = self.slot_crc_base(spec)?;
        for block in spec.matrix_blocks {
            let (_, _, cell_count) = *block_offsets
                .get(&block.block_id)
                .ok_or(Error::MatrixBlockMissing(block.block_id))?;
            offset = offset
                .checked_add(
                    cell_count
                        .checked_mul(CRC_LEN)
                        .ok_or(Error::InvalidMatrixLayout)?,
                )
                .ok_or(Error::InvalidMatrixLayout)?;
        }
        Ok(offset)
    }
}

fn matrix_crc_enabled(spec: FormatSpec) -> Result<bool> {
    match spec.integrity_policy {
        IntegrityPolicy::None => Ok(false),
        IntegrityPolicy::Crc32 | IntegrityPolicy::Crc32WithHeader => {
            #[cfg(feature = "integrity")]
            {
                Ok(true)
            }
            #[cfg(not(feature = "integrity"))]
            {
                Err(Error::IntegrityFeatureDisabled)
            }
        }
    }
}

fn validate_crc_presence(enabled: bool, header: &MatrixHeaderFields) -> Result<()> {
    match (
        enabled,
        header.region_crc_off == 0 && header.region_crc_len == 0,
    ) {
        (true, true) | (false, false) => Err(Error::InvalidMatrixLayout),
        _ => Ok(()),
    }
}

fn crc_table_len(spec: FormatSpec, cell_counts: &HashMap<String, u64>) -> Result<u64> {
    let commit_crc_len = usize_to_u64(spec.matrix_commits.len())?
        .checked_mul(CRC_LEN)
        .ok_or(Error::InvalidMatrixLayout)?;
    let mut len = MCRC_HEADER_LEN
        .checked_add(commit_crc_len)
        .ok_or(Error::InvalidMatrixLayout)?;
    for block in spec.matrix_blocks {
        let cell_count = *cell_counts
            .get(block.category)
            .ok_or_else(|| Error::MatrixCommitMissing(block.category.to_string()))?;
        len = len
            .checked_add(
                cell_count
                    .checked_mul(CRC_LEN)
                    .ok_or(Error::InvalidMatrixLayout)?,
            )
            .ok_or(Error::InvalidMatrixLayout)?;
        len = len
            .checked_add(bit_bytes(cell_count)?)
            .ok_or(Error::InvalidMatrixLayout)?;
    }
    Ok(len)
}

fn crc_layout_from_parts(
    region_crc_off: u64,
    region_crc_len: u64,
    spec: FormatSpec,
    commit_plans: &[CommitPlan],
    block_offsets: &HashMap<u32, (u64, u64, u64)>,
) -> Result<Option<MatrixCrcLayout>> {
    if region_crc_off == 0 && region_crc_len == 0 {
        return Ok(None);
    }
    if region_crc_off == 0 || region_crc_len < MCRC_HEADER_LEN {
        return Err(Error::InvalidMatrixLayout);
    }
    let commit_crc_len = usize_to_u64(commit_plans.len())?
        .checked_mul(CRC_LEN)
        .ok_or(Error::InvalidMatrixLayout)?;
    let mut expected_len = MCRC_HEADER_LEN
        .checked_add(commit_crc_len)
        .ok_or(Error::InvalidMatrixLayout)?;
    for block in spec.matrix_blocks {
        let (_, _, cell_count) = *block_offsets
            .get(&block.block_id)
            .ok_or(Error::MatrixBlockMissing(block.block_id))?;
        expected_len = expected_len
            .checked_add(
                cell_count
                    .checked_mul(CRC_LEN)
                    .ok_or(Error::InvalidMatrixLayout)?,
            )
            .ok_or(Error::InvalidMatrixLayout)?;
        expected_len = expected_len
            .checked_add(bit_bytes(cell_count)?)
            .ok_or(Error::InvalidMatrixLayout)?;
    }
    if region_crc_len != expected_len {
        return Err(Error::InvalidMatrixLayout);
    }
    Ok(Some(MatrixCrcLayout {
        region_offset: region_crc_off,
        region_len: region_crc_len,
    }))
}

fn write_crc_table(
    file: &mut File,
    spec: FormatSpec,
    commit_plans: &[CommitPlan],
    block_offsets: &HashMap<u32, (u64, u64, u64)>,
    dimension_table: &[u8],
    block_table: &[u8],
    category_table: &[u8],
) -> Result<()> {
    let mut header = [0u8; MCRC_HEADER_LEN as usize];
    header[0..4].copy_from_slice(MCRC_MAGIC);
    header[4..6].copy_from_slice(&MCRC_VERSION.to_le_bytes());
    header[8..12].copy_from_slice(
        &crc32_segments(&[dimension_table, block_table, category_table])?.to_le_bytes(),
    );
    file.write_all(&header)?;
    for (_, _, bit_count) in commit_plans {
        file.write_all(&crc32_zeroes(bit_bytes(*bit_count)?)?.to_le_bytes())?;
    }
    for block in spec.matrix_blocks {
        let (_, _, cell_count) = *block_offsets
            .get(&block.block_id)
            .ok_or(Error::MatrixBlockMissing(block.block_id))?;
        let zero_crc = crc32_zeroes(block.slot_stride)?;
        write_repeated_u32(file, zero_crc, cell_count)?;
    }
    for block in spec.matrix_blocks {
        let (_, _, cell_count) = *block_offsets
            .get(&block.block_id)
            .ok_or(Error::MatrixBlockMissing(block.block_id))?;
        write_zeros(file, bit_bytes(cell_count)?)?;
    }
    Ok(())
}

fn verify_crc_table(
    file: &mut File,
    crc: Option<&MatrixCrcLayout>,
    metadata_segments: &[&[u8]],
    commit_plans: &[StoredCommitPlan],
    commit_bits: &[Vec<u8>],
) -> Result<MatrixCrcVerification> {
    let Some(crc) = crc else {
        return Ok(MatrixCrcVerification::default());
    };
    let commit_crc_len = usize_to_u64(commit_plans.len())?
        .checked_mul(CRC_LEN)
        .ok_or(Error::InvalidMatrixLayout)?;
    let prefix_len = MCRC_HEADER_LEN
        .checked_add(commit_crc_len)
        .ok_or(Error::InvalidMatrixLayout)?;
    if prefix_len > crc.region_len {
        return Err(Error::InvalidMatrixLayout);
    }
    file.seek(SeekFrom::Start(crc.region_offset))?;
    let mut header = [0u8; MCRC_HEADER_LEN as usize];
    file.read_exact(&mut header)?;
    if &header[0..4] != MCRC_MAGIC
        || u16::from_le_bytes(header[4..6].try_into().expect("slice")) != MCRC_VERSION
        || header[6..8] != [0; 2]
        || header[12..16] != [0; 4]
    {
        return Err(Error::InvalidMatrixLayout);
    }

    let mut verification = MatrixCrcVerification::default();
    try_reserve_map(
        &mut verification.commit_findings,
        commit_plans.len(),
        MATRIX_DESCRIPTOR_RESOURCE,
    )?;
    let stored_metadata = u32::from_le_bytes(header[8..12].try_into().expect("slice"));
    let actual_metadata = crc32_segments(metadata_segments)?;
    if stored_metadata != actual_metadata {
        verification.findings.push(MatrixRecoveryFinding {
            kind: MatrixCorruptionKind::Header,
            severity: MatrixCorruptionSeverity::Fatal,
            message: format!(
                "matrix metadata crc mismatch: expected {stored_metadata:#010x}, got {actual_metadata:#010x}"
            ),
        });
    }

    if commit_bits.len() != commit_plans.len() {
        return Err(Error::InvalidMatrixLayout);
    }
    for (index, ((name, _, _, _, map_len), bits)) in
        commit_plans.iter().zip(commit_bits).enumerate()
    {
        if usize_to_u64(bits.len())? != *map_len {
            return Err(Error::InvalidMatrixLayout);
        }
        let stored = read_crc_at(file, crc.commit_crc_offset(usize_to_u64(index)?)?)?;
        let actual = crc32_bytes(bits)?;
        if stored != actual {
            let finding = MatrixRecoveryFinding {
                kind: MatrixCorruptionKind::CommitMap,
                severity: MatrixCorruptionSeverity::Recoverable,
                message: format!(
                    "matrix commit map crc mismatch for {name}: expected {stored:#010x}, got {actual:#010x}"
                ),
            };
            verification.commit_findings.insert(name.clone(), finding);
        }
    }
    Ok(verification)
}

fn read_crc_valid_bits(
    spec: FormatSpec,
    file: &mut File,
    crc: Option<&MatrixCrcLayout>,
    block_offsets: &HashMap<u32, (u64, u64, u64)>,
) -> Result<HashMap<u32, Vec<u8>>> {
    let Some(crc) = crc else {
        return Ok(HashMap::new());
    };
    let mut valid_bits = HashMap::new();
    try_reserve_map(
        &mut valid_bits,
        spec.matrix_blocks.len(),
        ReadLimitKey::MatrixCrcBytes.resource(),
    )?;
    for (block_index, block) in spec.matrix_blocks.iter().enumerate() {
        let (_, _, cell_count) = *block_offsets
            .get(&block.block_id)
            .ok_or(Error::MatrixBlockMissing(block.block_id))?;
        let offset = crc.block_valid_offset(spec, block_offsets, block_index)?;
        valid_bits.insert(
            block.block_id,
            read_range(
                file,
                offset,
                bit_bytes(cell_count)?,
                ReadLimitKey::MatrixCrcBytes.resource(),
            )?,
        );
    }
    Ok(valid_bits)
}

struct MatrixHeaderFields {
    dimension_count: u32,
    matrix_block_count: u32,
    commit_category_count: u32,
    dimension_table_off: u64,
    dimension_table_len: u64,
    block_table_off: u64,
    block_table_len: u64,
    commit_category_off: u64,
    commit_category_len: u64,
    commit_map_off: u64,
    commit_map_len: u64,
    slot_region_off: u64,
    slot_region_len: u64,
    region_crc_off: u64,
    region_crc_len: u64,
    append_log_start: u64,
}

fn encode_header(fields: MatrixHeaderFields) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(VMAT_HEADER_LEN as usize);
    bytes.extend_from_slice(VMAT_MAGIC);
    bytes.extend_from_slice(&VMAT_VERSION.to_le_bytes());
    bytes.extend_from_slice(&0u16.to_le_bytes());
    bytes.extend_from_slice(&VMAT_HEADER_LEN.to_le_bytes());
    bytes.extend_from_slice(&fields.dimension_count.to_le_bytes());
    bytes.extend_from_slice(&fields.matrix_block_count.to_le_bytes());
    bytes.extend_from_slice(&fields.commit_category_count.to_le_bytes());
    for value in [
        fields.dimension_table_off,
        fields.dimension_table_len,
        fields.block_table_off,
        fields.block_table_len,
        fields.commit_category_off,
        fields.commit_category_len,
        fields.commit_map_off,
        fields.commit_map_len,
        fields.slot_region_off,
        fields.slot_region_len,
        fields.region_crc_off,
        fields.region_crc_len,
        fields.append_log_start,
    ] {
        bytes.extend_from_slice(&value.to_le_bytes());
    }
    bytes.extend_from_slice(&[0; 32]);
    debug_assert_eq!(bytes.len(), VMAT_HEADER_LEN as usize);
    bytes
}

fn read_header(file: &mut File, header_len: u64) -> Result<MatrixHeaderFields> {
    file.seek(SeekFrom::Start(header_len))?;
    let mut bytes = [0; VMAT_HEADER_LEN as usize];
    file.read_exact(&mut bytes)?;
    if &bytes[0..4] != VMAT_MAGIC {
        return Err(Error::InvalidMatrixLayout);
    }
    let version = u16::from_le_bytes(bytes[4..6].try_into().expect("slice"));
    let header_len = u32::from_le_bytes(bytes[8..12].try_into().expect("slice"));
    if version != VMAT_VERSION
        || header_len != VMAT_HEADER_LEN
        || bytes[6..8] != [0; 2]
        || bytes[128..160] != [0; 32]
    {
        return Err(Error::InvalidMatrixLayout);
    }
    let mut pos = 12;
    let read_u32 = |bytes: &[u8], pos: &mut usize| {
        let value = u32::from_le_bytes(bytes[*pos..*pos + 4].try_into().expect("slice"));
        *pos += 4;
        value
    };
    let read_u64 = |bytes: &[u8], pos: &mut usize| {
        let value = u64::from_le_bytes(bytes[*pos..*pos + 8].try_into().expect("slice"));
        *pos += 8;
        value
    };
    let dimension_count = read_u32(&bytes, &mut pos);
    let matrix_block_count = read_u32(&bytes, &mut pos);
    let commit_category_count = read_u32(&bytes, &mut pos);
    Ok(MatrixHeaderFields {
        dimension_count,
        matrix_block_count,
        commit_category_count,
        dimension_table_off: read_u64(&bytes, &mut pos),
        dimension_table_len: read_u64(&bytes, &mut pos),
        block_table_off: read_u64(&bytes, &mut pos),
        block_table_len: read_u64(&bytes, &mut pos),
        commit_category_off: read_u64(&bytes, &mut pos),
        commit_category_len: read_u64(&bytes, &mut pos),
        commit_map_off: read_u64(&bytes, &mut pos),
        commit_map_len: read_u64(&bytes, &mut pos),
        slot_region_off: read_u64(&bytes, &mut pos),
        slot_region_len: read_u64(&bytes, &mut pos),
        region_crc_off: read_u64(&bytes, &mut pos),
        region_crc_len: read_u64(&bytes, &mut pos),
        append_log_start: read_u64(&bytes, &mut pos),
    })
}

fn encode_dimension_table(
    dimensions: &[MatrixDimensionValue],
    expected_len: u64,
) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    try_reserve_bytes(&mut bytes, expected_len, MATRIX_DESCRIPTOR_RESOURCE)?;
    for dimension in dimensions {
        write_name(&mut bytes, &dimension.name)?;
        bytes.extend_from_slice(&dimension.value.to_le_bytes());
    }
    if usize_to_u64(bytes.len())? != expected_len {
        return Err(Error::InvalidMatrixLayout);
    }
    Ok(bytes)
}

fn decode_dimension_table(bytes: &[u8], count: u32) -> Result<Vec<MatrixDimensionValue>> {
    let mut cursor = Cursor::new(bytes);
    let mut dimensions = Vec::new();
    try_reserve_vec(
        &mut dimensions,
        usize::try_from(count).map_err(|_| Error::InvalidMatrixLayout)?,
        MATRIX_DESCRIPTOR_RESOURCE,
    )?;
    for _ in 0..count {
        let name = cursor.read_name()?;
        let value = cursor.read_u64()?;
        dimensions.push(MatrixDimensionValue { name, value });
    }
    if cursor.remaining() != 0 {
        return Err(Error::InvalidMatrixLayout);
    }
    Ok(dimensions)
}

fn encode_block_table(
    spec: FormatSpec,
    dimension_index: &HashMap<String, u16>,
    commit_plans: &[CommitPlan],
    block_offsets: &HashMap<u32, (u64, u64, u64)>,
) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    let expected_len = usize_to_u64(spec.matrix_blocks.len())?
        .checked_mul(44)
        .ok_or(Error::ResourceArithmeticOverflow {
            resource: MATRIX_DESCRIPTOR_RESOURCE,
        })?;
    try_reserve_bytes(&mut bytes, expected_len, MATRIX_DESCRIPTOR_RESOURCE)?;
    for block in spec.matrix_blocks {
        let descriptor = spec
            .block(block.block_id)
            .ok_or(Error::MatrixBlockMissing(block.block_id))?;
        let dim0 = *dimension_index
            .get(block.dimensions[0])
            .ok_or_else(|| Error::MatrixDimensionMissing(block.dimensions[0].to_string()))?;
        let dim1 = *dimension_index
            .get(block.dimensions[1])
            .ok_or_else(|| Error::MatrixDimensionMissing(block.dimensions[1].to_string()))?;
        let category = commit_plans
            .iter()
            .position(|(name, _, _)| name == block.category)
            .ok_or_else(|| Error::MatrixCommitMissing(block.category.to_string()))?;
        let category = u16::try_from(category).map_err(|_| Error::InvalidMatrixLayout)?;
        let (slot_region_off, slot_region_len, cell_count) = *block_offsets
            .get(&block.block_id)
            .ok_or(Error::MatrixBlockMissing(block.block_id))?;
        bytes.extend_from_slice(&block.block_id.to_le_bytes());
        bytes.extend_from_slice(&descriptor.version.to_le_bytes());
        bytes.extend_from_slice(&dim0.to_le_bytes());
        bytes.extend_from_slice(&dim1.to_le_bytes());
        bytes.extend_from_slice(&category.to_le_bytes());
        bytes.extend_from_slice(&block.slot_stride.to_le_bytes());
        bytes.extend_from_slice(&cell_count.to_le_bytes());
        bytes.extend_from_slice(&slot_region_off.to_le_bytes());
        bytes.extend_from_slice(&slot_region_len.to_le_bytes());
    }
    if usize_to_u64(bytes.len())? != expected_len {
        return Err(Error::InvalidMatrixLayout);
    }
    Ok(bytes)
}

fn decode_block_table(
    spec: FormatSpec,
    dimensions: &[MatrixDimensionValue],
    commits: &[StoredCommitPlan],
    cell_counts: &HashMap<String, u64>,
    bytes: &[u8],
    count: u32,
    slot_region: (u64, u64),
) -> Result<HashMap<u32, (u64, u64, u64)>> {
    const ENTRY_LEN: usize = 44;
    let count_usize = usize::try_from(count).map_err(|_| Error::InvalidMatrixLayout)?;
    let expected_len = count_usize
        .checked_mul(ENTRY_LEN)
        .ok_or(Error::InvalidMatrixLayout)?;
    if bytes.len() != expected_len || count_usize != spec.matrix_blocks.len() {
        return Err(Error::InvalidMatrixLayout);
    }
    let mut offsets = HashMap::new();
    try_reserve_map(&mut offsets, count_usize, MATRIX_DESCRIPTOR_RESOURCE)?;
    let mut cursor = Cursor::new(bytes);
    let (slot_region_base, slot_region_len) = slot_region;
    let mut next_slot_offset = slot_region_base;
    for expected in spec.matrix_blocks {
        let block_id = cursor.read_u32()?;
        let block_version = cursor.read_u16()?;
        let dim0 = usize::from(cursor.read_u16()?);
        let dim1 = usize::from(cursor.read_u16()?);
        let category = usize::from(cursor.read_u16()?);
        let slot_stride = cursor.read_u64()?;
        let cell_count = cursor.read_u64()?;
        let slot_region_off = cursor.read_u64()?;
        let slot_region_len = cursor.read_u64()?;
        if block_id != expected.block_id
            || block_version
                != spec
                    .block(expected.block_id)
                    .ok_or(Error::MatrixBlockMissing(expected.block_id))?
                    .version
            || slot_stride != expected.slot_stride
        {
            return Err(Error::InvalidMatrixLayout);
        }
        let dim0_name = dimensions
            .get(dim0)
            .ok_or(Error::InvalidMatrixLayout)?
            .name
            .as_str();
        let dim1_name = dimensions
            .get(dim1)
            .ok_or(Error::InvalidMatrixLayout)?
            .name
            .as_str();
        let category_name = commits
            .get(category)
            .ok_or(Error::InvalidMatrixLayout)?
            .0
            .as_str();
        if expected.dimensions != [dim0_name, dim1_name] || expected.category != category_name {
            return Err(Error::InvalidMatrixLayout);
        }
        let expected_cell_count = *cell_counts
            .get(expected.category)
            .ok_or_else(|| Error::MatrixCommitMissing(expected.category.to_string()))?;
        let expected_slot_len = expected_cell_count
            .checked_mul(expected.slot_stride)
            .ok_or(Error::InvalidMatrixLayout)?;
        if cell_count != expected_cell_count
            || slot_region_off != next_slot_offset
            || slot_region_len != expected_slot_len
        {
            return Err(Error::InvalidMatrixLayout);
        }
        next_slot_offset = next_slot_offset
            .checked_add(slot_region_len)
            .ok_or(Error::InvalidMatrixLayout)?;
        if offsets
            .insert(block_id, (slot_region_off, slot_region_len, cell_count))
            .is_some()
        {
            return Err(Error::InvalidMatrixLayout);
        }
    }
    let slot_region_end = slot_region_base
        .checked_add(slot_region_len)
        .ok_or(Error::InvalidMatrixLayout)?;
    if next_slot_offset != slot_region_end {
        return Err(Error::InvalidMatrixLayout);
    }
    Ok(offsets)
}

fn encode_category_table(
    commits: &[CommitPlan],
    offsets: &HashMap<String, (u64, u64)>,
) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    let expected_len = commits.iter().try_fold(0u64, |len, (name, _, _)| {
        len.checked_add(usize_to_u64(name.len())?.checked_add(30).ok_or(
            Error::ResourceArithmeticOverflow {
                resource: MATRIX_DESCRIPTOR_RESOURCE,
            },
        )?)
        .ok_or(Error::ResourceArithmeticOverflow {
            resource: MATRIX_DESCRIPTOR_RESOURCE,
        })
    })?;
    try_reserve_bytes(&mut bytes, expected_len, MATRIX_DESCRIPTOR_RESOURCE)?;
    for (name, kind, bit_count) in commits {
        let (map_off, map_len) = *offsets
            .get(name)
            .ok_or_else(|| Error::MatrixCommitMissing(name.clone()))?;
        write_name(&mut bytes, name)?;
        bytes.push(commit_kind_byte(*kind));
        bytes.extend_from_slice(&[0; 3]);
        bytes.extend_from_slice(&bit_count.to_le_bytes());
        bytes.extend_from_slice(&map_off.to_le_bytes());
        bytes.extend_from_slice(&map_len.to_le_bytes());
    }
    if usize_to_u64(bytes.len())? != expected_len {
        return Err(Error::InvalidMatrixLayout);
    }
    Ok(bytes)
}

fn decode_category_table(bytes: &[u8], count: u32) -> Result<Vec<StoredCommitPlan>> {
    let mut cursor = Cursor::new(bytes);
    let mut commits = Vec::new();
    try_reserve_vec(
        &mut commits,
        usize::try_from(count).map_err(|_| Error::InvalidMatrixLayout)?,
        MATRIX_DESCRIPTOR_RESOURCE,
    )?;
    for _ in 0..count {
        let name = cursor.read_name()?;
        let kind = commit_kind_from_byte(cursor.read_u8()?)?;
        if cursor.read_exact(3)? != [0; 3] {
            return Err(Error::InvalidMatrixLayout);
        }
        let bit_count = cursor.read_u64()?;
        let map_off = cursor.read_u64()?;
        let map_len = cursor.read_u64()?;
        commits.push((name, kind, bit_count, map_off, map_len));
    }
    if cursor.remaining() != 0 {
        return Err(Error::InvalidMatrixLayout);
    }
    Ok(commits)
}

fn validate_dimension_names(spec: FormatSpec, dimensions: &[MatrixDimensionValue]) -> Result<()> {
    if dimensions.len() != spec.matrix_dimensions.len() {
        return Err(Error::InvalidMatrixLayout);
    }
    for (actual, expected) in dimensions.iter().zip(spec.matrix_dimensions) {
        if actual.name != expected.name {
            return Err(Error::InvalidMatrixLayout);
        }
    }
    Ok(())
}

fn validate_header_descriptor_shape(spec: FormatSpec, header: &MatrixHeaderFields) -> Result<()> {
    let (dimension_table_len, block_table_len, category_table_len) =
        matrix_descriptor_table_lengths(spec)?;
    if header.dimension_count
        != u32::try_from(spec.matrix_dimensions.len()).map_err(|_| Error::InvalidMatrixLayout)?
        || header.matrix_block_count
            != u32::try_from(spec.matrix_blocks.len()).map_err(|_| Error::InvalidMatrixLayout)?
        || header.commit_category_count
            != u32::try_from(spec.matrix_commits.len()).map_err(|_| Error::InvalidMatrixLayout)?
        || header.dimension_table_len != dimension_table_len
        || header.block_table_len != block_table_len
        || header.commit_category_len != category_table_len
    {
        return Err(Error::InvalidMatrixLayout);
    }
    Ok(())
}

fn validate_dimension_derived_lengths(
    spec: FormatSpec,
    crc_enabled: bool,
    header: &MatrixHeaderFields,
    commit_plans: &[CommitPlan],
    cell_counts: &HashMap<String, u64>,
) -> Result<()> {
    let expected_crc_len = if crc_enabled {
        crc_table_len(spec, cell_counts)?
    } else {
        0
    };
    if header.commit_map_len != matrix_commit_map_len(commit_plans)?
        || header.slot_region_len != matrix_slot_region_len(spec, cell_counts)?
        || header.region_crc_len != expected_crc_len
    {
        return Err(Error::InvalidMatrixLayout);
    }
    Ok(())
}

fn validate_commit_table(
    expected: &[CommitPlan],
    stored: &[StoredCommitPlan],
    commit_map_base: u64,
    commit_map_len: u64,
) -> Result<HashMap<String, (u64, u64)>> {
    if stored.len() != expected.len() {
        return Err(Error::InvalidMatrixLayout);
    }
    let mut offsets = HashMap::new();
    try_reserve_map(&mut offsets, expected.len(), MATRIX_DESCRIPTOR_RESOURCE)?;
    let mut next_offset = commit_map_base;
    for ((expected_name, expected_kind, expected_bits), actual) in expected.iter().zip(stored) {
        let expected_map_len = bit_bytes(*expected_bits)?;
        if actual.0 != *expected_name
            || actual.1 != *expected_kind
            || actual.2 != *expected_bits
            || actual.3 != next_offset
            || actual.4 != expected_map_len
        {
            return Err(Error::InvalidMatrixLayout);
        }
        if offsets
            .insert(expected_name.clone(), (actual.3, actual.4))
            .is_some()
        {
            return Err(Error::InvalidMatrixLayout);
        }
        next_offset = next_offset
            .checked_add(expected_map_len)
            .ok_or(Error::InvalidMatrixLayout)?;
    }
    let commit_map_end = commit_map_base
        .checked_add(commit_map_len)
        .ok_or(Error::InvalidMatrixLayout)?;
    if next_offset != commit_map_end {
        return Err(Error::InvalidMatrixLayout);
    }
    Ok(offsets)
}

fn read_commit_bitmaps(file: &mut File, commits: &[StoredCommitPlan]) -> Result<Vec<Vec<u8>>> {
    let mut bitmaps = Vec::new();
    try_reserve_vec(
        &mut bitmaps,
        commits.len(),
        ReadLimitKey::MatrixBitmapBytes.resource(),
    )?;
    for (_, _, _, map_offset, map_len) in commits {
        bitmaps.push(read_range(
            file,
            *map_offset,
            *map_len,
            ReadLimitKey::MatrixBitmapBytes.resource(),
        )?);
    }
    Ok(bitmaps)
}

fn validate_layout_ranges(
    header_len: u64,
    file_len: u64,
    header: &MatrixHeaderFields,
    aux_region_len: u64,
) -> Result<()> {
    let vmat_end = header_len
        .checked_add(u64::from(VMAT_HEADER_LEN))
        .ok_or(Error::InvalidMatrixLayout)?;
    let dimension_end = validate_range(
        header.dimension_table_off,
        header.dimension_table_len,
        file_len,
    )?;
    let block_end = validate_range(header.block_table_off, header.block_table_len, file_len)?;
    let category_end = validate_range(
        header.commit_category_off,
        header.commit_category_len,
        file_len,
    )?;
    let commit_end = validate_range(header.commit_map_off, header.commit_map_len, file_len)?;
    let slot_end = validate_range(header.slot_region_off, header.slot_region_len, file_len)?;
    let aux_end = validate_range(slot_end, aux_region_len, file_len)?;
    let crc_end = if header.region_crc_off == 0 && header.region_crc_len == 0 {
        aux_end
    } else {
        let crc_end = validate_range(header.region_crc_off, header.region_crc_len, file_len)?;
        if header.region_crc_off != aux_end {
            return Err(Error::InvalidMatrixLayout);
        }
        crc_end
    };
    if header.dimension_table_off != vmat_end
        || header.block_table_off != dimension_end
        || header.commit_category_off != block_end
        || header.commit_map_off != category_end
        || header.slot_region_off != commit_end
        || header.append_log_start != crc_end
        || header.append_log_start > file_len
    {
        return Err(Error::InvalidMatrixLayout);
    }
    Ok(())
}

fn validate_range(offset: u64, len: u64, file_len: u64) -> Result<u64> {
    let end = offset.checked_add(len).ok_or(Error::InvalidMatrixLayout)?;
    if end > file_len {
        return Err(Error::InvalidMatrixLayout);
    }
    Ok(end)
}

fn read_range(file: &mut File, offset: u64, len: u64, resource: &'static str) -> Result<Vec<u8>> {
    file.seek(SeekFrom::Start(offset))?;
    let mut bytes = filled_bytes_for(len, 0, resource)?;
    file.read_exact(&mut bytes)?;
    Ok(bytes)
}

fn filled_bytes(len: u64, value: u8) -> Result<Vec<u8>> {
    filled_bytes_for(len, value, MATRIX_BYTES_RESOURCE)
}

fn filled_bytes_for(len: u64, value: u8, resource: &'static str) -> Result<Vec<u8>> {
    let len_usize = usize::try_from(len).map_err(|_| Error::LengthOverflow { value: len })?;
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(len_usize)
        .map_err(|_| Error::AllocationFailed {
            resource,
            requested: len,
        })?;
    bytes.resize(len_usize, value);
    Ok(bytes)
}

fn try_reserve_bytes(bytes: &mut Vec<u8>, requested: u64, resource: &'static str) -> Result<()> {
    let requested_usize =
        usize::try_from(requested).map_err(|_| Error::LengthOverflow { value: requested })?;
    bytes
        .try_reserve_exact(requested_usize)
        .map_err(|_| Error::AllocationFailed {
            resource,
            requested,
        })
}

fn try_reserve_vec<T>(
    values: &mut Vec<T>,
    additional: usize,
    resource: &'static str,
) -> Result<()> {
    let requested = additional
        .checked_mul(std::mem::size_of::<T>().max(1))
        .and_then(|bytes| u64::try_from(bytes).ok())
        .ok_or(Error::ResourceArithmeticOverflow { resource })?;
    values
        .try_reserve_exact(additional)
        .map_err(|_| Error::AllocationFailed {
            resource,
            requested,
        })
}

fn try_reserve_map<K: Eq + std::hash::Hash, V>(
    values: &mut HashMap<K, V>,
    additional: usize,
    resource: &'static str,
) -> Result<()> {
    let requested = additional
        .checked_mul(std::mem::size_of::<(K, V)>().max(1))
        .and_then(|bytes| u64::try_from(bytes).ok())
        .ok_or(Error::ResourceArithmeticOverflow { resource })?;
    values
        .try_reserve(additional)
        .map_err(|_| Error::AllocationFailed {
            resource,
            requested,
        })
}

fn usize_to_u64(value: usize) -> Result<u64> {
    u64::try_from(value).map_err(|_| Error::InvalidMatrixLayout)
}

fn read_crc_at(file: &mut File, offset: u64) -> Result<u32> {
    file.seek(SeekFrom::Start(offset))?;
    let mut bytes = [0; 4];
    file.read_exact(&mut bytes)?;
    Ok(u32::from_le_bytes(bytes))
}

fn write_crc_at(file: &mut File, offset: u64, crc: u32) -> Result<()> {
    file.seek(SeekFrom::Start(offset))?;
    file.write_all(&crc.to_le_bytes())?;
    Ok(())
}

fn indexed_crc_offset(crc_offset: u64, ordinal: u64) -> Result<u64> {
    ordinal
        .checked_mul(CRC_LEN)
        .and_then(|delta| crc_offset.checked_add(delta))
        .ok_or(Error::InvalidMatrixLayout)
}

#[cfg(feature = "integrity")]
fn crc32_bytes(bytes: &[u8]) -> Result<u32> {
    Ok(crc32fast::hash(bytes))
}

#[cfg(not(feature = "integrity"))]
fn crc32_bytes(_bytes: &[u8]) -> Result<u32> {
    Err(Error::IntegrityFeatureDisabled)
}

#[cfg(feature = "integrity")]
fn crc32_bytes_with_replacement(bytes: &[u8], index: usize, value: u8) -> Result<u32> {
    let suffix = index.checked_add(1).ok_or(Error::InvalidMatrixLayout)?;
    if suffix > bytes.len() {
        return Err(Error::InvalidMatrixLayout);
    }
    let mut hasher = crc32fast::Hasher::new();
    hasher.update(&bytes[..index]);
    hasher.update(&[value]);
    hasher.update(&bytes[suffix..]);
    Ok(hasher.finalize())
}

#[cfg(not(feature = "integrity"))]
fn crc32_bytes_with_replacement(_bytes: &[u8], _index: usize, _value: u8) -> Result<u32> {
    Err(Error::IntegrityFeatureDisabled)
}

#[cfg(feature = "integrity")]
fn crc32_segments(segments: &[&[u8]]) -> Result<u32> {
    let mut hasher = crc32fast::Hasher::new();
    for segment in segments {
        hasher.update(segment);
    }
    Ok(hasher.finalize())
}

#[cfg(not(feature = "integrity"))]
fn crc32_segments(_segments: &[&[u8]]) -> Result<u32> {
    Err(Error::IntegrityFeatureDisabled)
}

#[cfg(feature = "integrity")]
fn crc32_zeroes(len: u64) -> Result<u32> {
    const ZERO_CHUNK: [u8; 8192] = [0; 8192];
    let mut hasher = crc32fast::Hasher::new();
    let mut remaining = len;
    while remaining > 0 {
        let chunk = remaining.min(ZERO_CHUNK.len() as u64);
        hasher.update(&ZERO_CHUNK[..chunk as usize]);
        remaining -= chunk;
    }
    Ok(hasher.finalize())
}

#[cfg(not(feature = "integrity"))]
fn crc32_zeroes(_len: u64) -> Result<u32> {
    Err(Error::IntegrityFeatureDisabled)
}

fn write_zeros(file: &mut File, len: u64) -> Result<()> {
    const ZERO_CHUNK: [u8; 8192] = [0; 8192];
    let mut remaining = len;
    while remaining > 0 {
        let chunk = remaining.min(ZERO_CHUNK.len() as u64);
        file.write_all(&ZERO_CHUNK[..chunk as usize])?;
        remaining -= chunk;
    }
    Ok(())
}

fn write_repeated_u32(file: &mut File, value: u32, count: u64) -> Result<()> {
    const ENTRIES_PER_CHUNK: usize = 2048;
    let value = value.to_le_bytes();
    let mut chunk = [0u8; ENTRIES_PER_CHUNK * 4];
    for entry in chunk.chunks_exact_mut(4) {
        entry.copy_from_slice(&value);
    }
    let mut remaining = count;
    while remaining > 0 {
        let entries = remaining.min(ENTRIES_PER_CHUNK as u64);
        let bytes = usize::try_from(entries)
            .map_err(|_| Error::InvalidMatrixLayout)?
            .checked_mul(4)
            .ok_or(Error::InvalidMatrixLayout)?;
        file.write_all(&chunk[..bytes])?;
        remaining -= entries;
    }
    Ok(())
}

fn bit_bytes(bit_count: u64) -> Result<u64> {
    bit_count
        .checked_add(7)
        .map(|value| value / 8)
        .ok_or(Error::InvalidMatrixLayout)
}

fn get_bit(bits: &[u8], ordinal: u64) -> Result<bool> {
    let byte = usize::try_from(ordinal / 8).map_err(|_| Error::InvalidMatrixLayout)?;
    let mask = 1u8 << (ordinal % 8);
    Ok(bits.get(byte).ok_or(Error::InvalidMatrixLayout)? & mask != 0)
}

fn set_bit(bits: &mut [u8], ordinal: u64, value: bool) -> Result<()> {
    let byte = usize::try_from(ordinal / 8).map_err(|_| Error::InvalidMatrixLayout)?;
    let mask = 1u8 << (ordinal % 8);
    let target = bits.get_mut(byte).ok_or(Error::InvalidMatrixLayout)?;
    if value {
        *target |= mask;
    } else {
        *target &= !mask;
    }
    Ok(())
}

fn write_name(bytes: &mut Vec<u8>, name: &str) -> Result<()> {
    let len = u16::try_from(name.len()).map_err(|_| Error::InvalidMatrixLayout)?;
    bytes.extend_from_slice(&len.to_le_bytes());
    bytes.extend_from_slice(name.as_bytes());
    Ok(())
}

fn commit_kind_byte(kind: MatrixCommitKind) -> u8 {
    match kind {
        MatrixCommitKind::Cell => 1,
        MatrixCommitKind::Single => 2,
        MatrixCommitKind::PerChannel => 3,
    }
}

fn commit_kind_from_byte(value: u8) -> Result<MatrixCommitKind> {
    match value {
        1 => Ok(MatrixCommitKind::Cell),
        2 => Ok(MatrixCommitKind::Single),
        3 => Ok(MatrixCommitKind::PerChannel),
        _ => Err(Error::InvalidMatrixLayout),
    }
}

struct Cursor<'a> {
    bytes: &'a [u8],
    offset: usize,
}

impl<'a> Cursor<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, offset: 0 }
    }

    fn remaining(&self) -> usize {
        self.bytes.len().saturating_sub(self.offset)
    }

    fn read_exact(&mut self, len: usize) -> Result<&'a [u8]> {
        let end = self
            .offset
            .checked_add(len)
            .ok_or(Error::InvalidMatrixLayout)?;
        let bytes = self
            .bytes
            .get(self.offset..end)
            .ok_or(Error::InvalidMatrixLayout)?;
        self.offset = end;
        Ok(bytes)
    }

    fn read_u8(&mut self) -> Result<u8> {
        Ok(self.read_exact(1)?[0])
    }

    fn read_u16(&mut self) -> Result<u16> {
        let mut bytes = [0; 2];
        bytes.copy_from_slice(self.read_exact(2)?);
        Ok(u16::from_le_bytes(bytes))
    }

    fn read_u32(&mut self) -> Result<u32> {
        let mut bytes = [0; 4];
        bytes.copy_from_slice(self.read_exact(4)?);
        Ok(u32::from_le_bytes(bytes))
    }

    fn read_u64(&mut self) -> Result<u64> {
        let mut bytes = [0; 8];
        bytes.copy_from_slice(self.read_exact(8)?);
        Ok(u64::from_le_bytes(bytes))
    }

    fn read_name(&mut self) -> Result<String> {
        let len = self.read_u16()? as usize;
        let bytes = self.read_exact(len)?;
        let mut owned = Vec::new();
        try_reserve_vec(&mut owned, len, MATRIX_DESCRIPTOR_RESOURCE)?;
        owned.extend_from_slice(bytes);
        String::from_utf8(owned).map_err(|_| Error::InvalidMatrixLayout)
    }
}

#[cfg(test)]
mod limit_tests {
    use super::*;

    fn sidecar_spec(limits: ReadLimits) -> FormatSpec {
        FormatSpec::new(
            b"SIDE",
            1,
            crate::Endian::Little,
            0,
            crate::IndexPolicy::ScanOnOpen,
            IntegrityPolicy::None,
            crate::RecoveryPolicy::Strict,
            crate::ManifestPolicy::None,
            &[],
        )
        .with_read_limits(limits)
    }

    #[test]
    fn sidecar_plan_checks_exact_lengths_limits_and_reserved_fields() {
        let spec = sidecar_spec(ReadLimits::finite_all(100));
        let plan = matrix_sidecar_read_plan(spec, 60, 48, 4, 4, 4, 0, 0, 0).unwrap();
        assert_eq!(
            plan,
            MatrixSidecarReadPlan {
                format_magic_offset: 48,
                category_offset: 52,
                payload_offset: 56,
                payload_len: 4,
                total_len: 60,
            }
        );

        assert!(matches!(
            matrix_sidecar_read_plan(spec, 61, 48, 4, 4, 4, 0, 0, 0),
            Err(Error::InvalidMatrixSidecar)
        ));
        assert!(matches!(
            matrix_sidecar_read_plan(spec, 60, 48, 4, 4, 4, 1, 0, 0),
            Err(Error::InvalidMatrixSidecar)
        ));

        let sidecar_limited = sidecar_spec(ReadLimits::finite_all(100).with_max_sidecar_len(59));
        assert!(matches!(
            matrix_sidecar_read_plan(sidecar_limited, 60, 48, 4, 4, 4, 0, 0, 0),
            Err(Error::LimitExceeded {
                resource: "sidecar length",
                actual: 60,
                limit: 59,
            })
        ));

        let payload_limited =
            sidecar_spec(ReadLimits::finite_all(100).with_max_materialized_bytes(3));
        assert!(matches!(
            matrix_sidecar_read_plan(payload_limited, 60, 48, 4, 4, 4, 0, 0, 0),
            Err(Error::LimitExceeded {
                resource: "materialized bytes",
                actual: 4,
                limit: 3,
            })
        ));

        let unbounded = sidecar_spec(ReadLimits::finite_all(u64::MAX));
        for result in [
            matrix_sidecar_read_plan(unbounded, u64::MAX, 48, u64::MAX, 4, 4, 0, 0, 0),
            matrix_sidecar_read_plan(unbounded, u64::MAX, 48, 4, 4, u64::MAX, 0, 0, 0),
        ] {
            assert!(matches!(
                result,
                Err(Error::ResourceArithmeticOverflow { .. })
            ));
        }
    }
}
