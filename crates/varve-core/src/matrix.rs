use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};

use crate::{
    BlockKind, Error, FormatSpec, IntegrityPolicy, MatrixCommitKind, Result, VarveMatrixBlock,
    decode_from_slice, encode_to_vec,
};

const VMAT_MAGIC: &[u8; 4] = b"VMAT";
const VMAT_VERSION: u16 = 1;
const VMAT_HEADER_LEN: u32 = 160;
const MCRC_MAGIC: &[u8; 4] = b"MCRC";
const MCRC_VERSION: u16 = 1;
const MCRC_HEADER_LEN: u64 = 16;
const CRC_LEN: u64 = 4;

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
}

#[derive(Clone, Debug)]
struct MatrixCommitLayout {
    name: String,
    kind: MatrixCommitKind,
    bit_count: u64,
    map_offset: u64,
    bits: Vec<u8>,
    quarantined_raw_bits: Option<Vec<u8>>,
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
    crc_valid_bits: Vec<u8>,
    written_bits: Vec<u8>,
    current_write_bits: Vec<u8>,
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
    metadata_crc_offset: u64,
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
    let dimensions = dimension_values(spec, dims)?;
    let mut dimension_index = HashMap::with_capacity(dimensions.len());
    for (index, value) in dimensions.iter().enumerate() {
        dimension_index.insert(
            value.name.clone(),
            u16::try_from(index).map_err(|_| Error::InvalidMatrixLayout)?,
        );
    }
    let cell_counts = matrix_cell_counts(spec, &dimensions)?;
    let commit_plans = matrix_commit_plans(spec, &dimensions, &cell_counts)?;

    let dimension_table = encode_dimension_table(&dimensions)?;
    let (dimension_table_len, block_table_len, category_table_len) =
        matrix_descriptor_table_lengths(spec)?;
    if usize_to_u64(dimension_table.len())? != dimension_table_len {
        return Err(Error::InvalidMatrixLayout);
    }
    let commit_map_len = matrix_commit_map_len(&commit_plans)?;
    let slot_region_len = matrix_slot_region_len(spec, &cell_counts)?;

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
    let region_crc_len = if crc_enabled {
        crc_table_len(spec, &cell_counts)?
    } else {
        0
    };
    let region_crc_off = if crc_enabled { aux_region_end } else { 0 };
    let append_log_start = if crc_enabled {
        region_crc_off
            .checked_add(region_crc_len)
            .ok_or(Error::InvalidMatrixLayout)?
    } else {
        aux_region_end
    };

    let mut commit_offsets = HashMap::new();
    let mut next_commit_offset = commit_map_off;
    for (name, _, bit_count) in &commit_plans {
        let len = bit_bytes(*bit_count)?;
        commit_offsets.insert(name.clone(), (next_commit_offset, len));
        next_commit_offset = next_commit_offset
            .checked_add(len)
            .ok_or(Error::InvalidMatrixLayout)?;
    }

    let mut block_offsets = HashMap::new();
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
    let crc_table = match &crc_layout {
        Some(_) => Some(encode_crc_table(
            spec,
            &commit_plans,
            &block_offsets,
            &dimension_table,
            &block_table,
            &category_table,
        )?),
        None => None,
    };
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

    file.seek(SeekFrom::Start(header_len))?;
    file.write_all(&header)?;
    file.write_all(&dimension_table)?;
    file.write_all(&block_table)?;
    file.write_all(&category_table)?;
    write_zeros(file, commit_map_len)?;
    file.set_len(append_log_start)?;
    if let Some(crc_table) = crc_table {
        file.seek(SeekFrom::Start(region_crc_off))?;
        file.write_all(&crc_table)?;
    }
    file.seek(SeekFrom::Start(append_log_start))?;

    layout_from_parts(
        dimensions,
        spec,
        &commit_plans,
        &commit_offsets,
        &block_offsets,
        &aux_offsets,
        commit_map_off,
        filled_bytes(commit_map_len, 0)?,
        crc_layout,
        HashMap::new(),
        HashMap::new(),
        Vec::new(),
        append_log_start,
    )
}

pub(crate) fn read_layout(
    spec: FormatSpec,
    file: &mut File,
    header_len: u64,
) -> Result<MatrixLayout> {
    let crc_enabled = matrix_crc_enabled(spec)?;
    let header = read_header(file, header_len)?;
    validate_header_descriptor_shape(spec, &header)?;
    let file_len = file.metadata()?.len();
    let aux_region_len = matrix_aux_region_len(spec)?;
    validate_layout_ranges(header_len, file_len, &header, aux_region_len)?;
    validate_crc_presence(crc_enabled, &header)?;

    let dimension_table = read_range(file, header.dimension_table_off, header.dimension_table_len)?;
    let dimensions = decode_dimension_table(&dimension_table, header.dimension_count)?;
    validate_dimension_names(spec, &dimensions)?;
    let cell_counts = matrix_cell_counts(spec, &dimensions)?;
    let expected_commit_plans = matrix_commit_plans(spec, &dimensions, &cell_counts)?;
    validate_dimension_derived_lengths(
        spec,
        crc_enabled,
        &header,
        &expected_commit_plans,
        &cell_counts,
    )?;

    let category_table = read_range(file, header.commit_category_off, header.commit_category_len)?;
    let commit_plans = decode_category_table(&category_table, header.commit_category_count)?;
    let commit_offsets = validate_commit_table(
        &expected_commit_plans,
        &commit_plans,
        header.commit_map_off,
        header.commit_map_len,
    )?;
    let commit_bits = read_range(file, header.commit_map_off, header.commit_map_len)?;

    let block_table = read_range(file, header.block_table_off, header.block_table_len)?;
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
    let crc_verification = verify_crc_table(
        file,
        crc_layout.as_ref(),
        &[&dimension_table, &block_table, &category_table],
        &commit_plans,
        &commit_bits,
        header.commit_map_off,
    )?;
    let crc_valid_bits = read_crc_valid_bits(spec, file, crc_layout.as_ref(), &block_offsets)?;

    layout_from_parts(
        dimensions,
        spec,
        &expected_commit_plans,
        &commit_offsets,
        &block_offsets,
        &aux_offsets,
        header.commit_map_off,
        commit_bits,
        crc_layout,
        crc_valid_bits,
        crc_verification.commit_findings,
        crc_verification.findings,
        header.append_log_start,
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
    let payload = encode_to_vec(value, T::ENDIAN.unwrap_or(spec.endian))?;
    if payload.len() as u64 != slot_stride {
        return Err(Error::MatrixSizeMismatch {
            expected: slot_stride,
            actual: payload.len() as u64,
        });
    }
    let mut written_bits = layout.blocks[block_index].written_bits.clone();
    let mut current_write_bits = layout.blocks[block_index].current_write_bits.clone();
    set_bit(&mut written_bits, ordinal, true)?;
    set_bit(&mut current_write_bits, ordinal, true)?;
    let (commit_index, commit_update) = prepare_cell_commit(layout, T::CATEGORY, ordinal, false)?;
    let crc_valid_update = prepare_cell_crc_valid(layout, block_index, ordinal, false)?;

    apply_commit_bit(layout, file, commit_index, commit_update)?;
    if let Some(update) = crc_valid_update {
        apply_cell_crc_valid(layout, file, block_index, update)?;
    }
    file.seek(SeekFrom::Start(offset))?;
    write_slot_payload(file, &payload)?;
    layout.blocks[block_index].written_bits = written_bits;
    layout.blocks[block_index].current_write_bits = current_write_bits;
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
    let mut written_bits = layout.blocks[block_index].written_bits.clone();
    let mut current_write_bits = layout.blocks[block_index].current_write_bits.clone();
    set_bit(&mut written_bits, ordinal, true)?;
    set_bit(&mut current_write_bits, ordinal, true)?;
    let (commit_index, commit_update) = prepare_cell_commit(layout, T::CATEGORY, ordinal, false)?;
    let crc_valid_update = prepare_cell_crc_valid(layout, block_index, ordinal, false)?;

    apply_commit_bit(layout, file, commit_index, commit_update)?;
    if let Some(update) = crc_valid_update {
        apply_cell_crc_valid(layout, file, block_index, update)?;
    }
    file.seek(SeekFrom::Start(offset))?;
    write_slot_payload(file, payload)?;
    layout.blocks[block_index].written_bits = written_bits;
    layout.blocks[block_index].current_write_bits = current_write_bits;
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
    file.seek(SeekFrom::Start(offset))?;
    let mut payload = filled_bytes(stride, 0)?;
    file.read_exact(&mut payload)?;
    verify_cell_crc(layout, file, block_index, ordinal, &payload)?;
    decode_from_slice(&payload, T::ENDIAN.unwrap_or(spec.endian))
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
    let commit_index = layout.commit_index(category)?;
    let cleared = count_committed(&layout.commits[commit_index])?;
    let cleared_bits = vec![0; layout.commits[commit_index].bits.len()];
    let commit_kind = layout.commits[commit_index].kind;
    let map_offset = layout.commits[commit_index].map_offset;
    let crc_offset = layout.commits[commit_index].crc_offset;
    let cleared_valid = if commit_kind == MatrixCommitKind::Cell {
        let block_index = block_index_for_category(spec, layout, category)?;
        let bits = vec![0; layout.blocks[block_index].crc_valid_bits.len()];
        Some((
            block_index,
            layout.blocks[block_index].crc_valid_offset,
            bits,
        ))
    } else {
        None
    };

    if let Some((_, Some(valid_offset), bits)) = &cleared_valid {
        file.seek(SeekFrom::Start(*valid_offset))?;
        file.write_all(bits)?;
    }
    write_commit_crc(file, crc_offset, &cleared_bits)?;
    file.seek(SeekFrom::Start(map_offset))?;
    file.write_all(&cleared_bits)?;

    if let Some((block_index, _, bits)) = cleared_valid {
        layout.blocks[block_index].crc_valid_bits = bits;
    }
    let commit = &mut layout.commits[commit_index];
    commit.bits = cleared_bits;
    commit.quarantined_raw_bits = None;
    commit.quarantine_finding = None;
    Ok(cleared)
}

pub(crate) fn commit_event<T: VarveMatrixBlock>(
    spec: FormatSpec,
    layout: &MatrixLayout,
    key: MatrixKey,
) -> Result<MatrixCommitEvent> {
    ensure_matrix_block::<T>(spec)?;
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
    file.seek(SeekFrom::Start(offset))?;
    let mut payload = filled_bytes(stride, 0)?;
    file.read_exact(&mut payload)?;
    verify_cell_crc(layout, file, block_index, ordinal, &payload)?;
    Ok(payload)
}

pub(crate) fn aux_len(layout: &MatrixLayout, name: &str) -> Result<u64> {
    Ok(layout.aux(name)?.byte_len)
}

pub(crate) fn read_aux(
    layout: &MatrixLayout,
    file: &mut File,
    name: &str,
    offset: u64,
    len: u64,
) -> Result<Vec<u8>> {
    let absolute = aux_absolute_offset(layout.aux(name)?, offset, len)?;
    file.seek(SeekFrom::Start(absolute))?;
    let len = usize::try_from(len).map_err(|_| Error::MatrixAuxOutOfBounds {
        name: name.to_string(),
        offset,
        len,
        byte_len: layout.aux(name).map(|aux| aux.byte_len).unwrap_or(0),
    })?;
    let mut payload = vec![0; len];
    file.read_exact(&mut payload)?;
    Ok(payload)
}

pub(crate) fn write_aux(
    layout: &MatrixLayout,
    file: &mut File,
    name: &str,
    offset: u64,
    payload: &[u8],
) -> Result<()> {
    let len = payload
        .len()
        .try_into()
        .map_err(|_| Error::InvalidMatrixLayout)?;
    let absolute = aux_absolute_offset(layout.aux(name)?, offset, len)?;
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

    let mut rebuilt = vec![0; layout.commits[commit_index].bits.len()];
    let mut committed = 0u64;
    for ordinal in 0..block.cell_count {
        let slot_offset = layout.slot_offset(block_index, ordinal)?;
        file.seek(SeekFrom::Start(slot_offset))?;
        let mut payload = filled_bytes(block.slot_stride, 0)?;
        file.read_exact(&mut payload)?;
        let actual = crc32_bytes(&payload)?;
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
    commit.bits = rebuilt;
    commit.quarantined_raw_bits = None;
    commit.quarantine_finding = None;
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
    bits: Vec<u8>,
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
    let mut staged = bits.to_vec();
    set_bit(&mut staged, ordinal, value)?;
    let byte_index = usize::try_from(ordinal / 8).map_err(|_| Error::InvalidMatrixLayout)?;
    let byte_value = *staged.get(byte_index).ok_or(Error::InvalidMatrixLayout)?;
    let byte_offset = base_offset
        .checked_add(u64::try_from(byte_index).map_err(|_| Error::InvalidMatrixLayout)?)
        .ok_or(Error::InvalidMatrixLayout)?;
    Ok(BitmapByteUpdate {
        bits: staged,
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
        Some(offset) => Some((offset, crc32_bytes(&bitmap.bits)?)),
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
    layout.commits[commit_index].bits = update.bitmap.bits;
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
    file.seek(SeekFrom::Start(offset))?;
    let mut payload = filled_bytes(stride, 0)?;
    file.read_exact(&mut payload)?;
    let crc = crc32_bytes(&payload)?;
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
    layout.blocks[block_index].crc_valid_bits = update.bits;
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
    let mut payload = filled_bytes(stride, 0)?;
    file.read_exact(&mut payload)?;
    Ok(payload.iter().all(|byte| *byte == 0))
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
    let mut values = Vec::with_capacity(spec.matrix_dimensions.len());
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
    let values = dimensions
        .iter()
        .map(|dimension| (dimension.name.as_str(), dimension.value))
        .collect::<HashMap<_, _>>();
    let mut counts = HashMap::new();
    for block in spec.matrix_blocks {
        let dim0 = *values
            .get(block.dimensions[0])
            .ok_or_else(|| Error::MatrixDimensionMissing(block.dimensions[0].to_string()))?;
        let dim1 = *values
            .get(block.dimensions[1])
            .ok_or_else(|| Error::MatrixDimensionMissing(block.dimensions[1].to_string()))?;
        let count = dim0.checked_mul(dim1).ok_or(Error::InvalidMatrixLayout)?;
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
    let mut plans = Vec::with_capacity(spec.matrix_commits.len());
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
    let mut offsets = HashMap::with_capacity(spec.matrix_aux.len());
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
    commit_map_base: u64,
    commit_bits: Vec<u8>,
    crc: Option<MatrixCrcLayout>,
    crc_valid_bits: HashMap<u32, Vec<u8>>,
    mut commit_findings: HashMap<String, MatrixRecoveryFinding>,
    crc_findings: Vec<MatrixRecoveryFinding>,
    append_log_start: u64,
) -> Result<MatrixLayout> {
    let mut commits = Vec::with_capacity(commit_plans.len());
    for (commit_index, (name, kind, bit_count)) in commit_plans.iter().enumerate() {
        let (map_offset, map_len) = *commit_offsets
            .get(name)
            .ok_or_else(|| Error::MatrixCommitMissing(name.clone()))?;
        let relative_offset = map_offset
            .checked_sub(commit_map_base)
            .ok_or(Error::InvalidMatrixLayout)?;
        let start = usize::try_from(relative_offset).map_err(|_| Error::InvalidMatrixLayout)?;
        let end = start
            .checked_add(usize::try_from(map_len).map_err(|_| Error::InvalidMatrixLayout)?)
            .ok_or(Error::InvalidMatrixLayout)?;
        let raw_bits = commit_bits
            .get(start..end)
            .ok_or(Error::InvalidMatrixLayout)?
            .to_vec();
        let quarantine_finding = commit_findings.remove(name);
        let (bits, quarantined_raw_bits) = if quarantine_finding.is_some() {
            (vec![0; raw_bits.len()], Some(raw_bits))
        } else {
            (raw_bits, None)
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
    let mut blocks = Vec::with_capacity(spec.matrix_blocks.len());
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
        let crc_valid_bits_for_block = match crc_valid_bits.get(&block.block_id) {
            Some(bits) => bits.clone(),
            None => filled_bytes(crc_valid_len, 0)?,
        };
        if usize_to_u64(crc_valid_bits_for_block.len())? != crc_valid_len {
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
            crc_valid_bits: crc_valid_bits_for_block,
            written_bits,
            current_write_bits,
        });
    }
    let mut aux = Vec::with_capacity(spec.matrix_aux.len());
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
        metadata_crc_offset: region_crc_off
            .checked_add(8)
            .ok_or(Error::InvalidMatrixLayout)?,
    }))
}

fn encode_crc_table(
    spec: FormatSpec,
    commit_plans: &[CommitPlan],
    block_offsets: &HashMap<u32, (u64, u64, u64)>,
    dimension_table: &[u8],
    block_table: &[u8],
    category_table: &[u8],
) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(MCRC_MAGIC);
    bytes.extend_from_slice(&MCRC_VERSION.to_le_bytes());
    bytes.extend_from_slice(&0u16.to_le_bytes());
    bytes.extend_from_slice(
        &crc32_segments(&[dimension_table, block_table, category_table])?.to_le_bytes(),
    );
    bytes.extend_from_slice(&0u32.to_le_bytes());
    for (_, _, bit_count) in commit_plans {
        bytes.extend_from_slice(&crc32_zeroes(bit_bytes(*bit_count)?)?.to_le_bytes());
    }
    for block in spec.matrix_blocks {
        let (_, _, cell_count) = *block_offsets
            .get(&block.block_id)
            .ok_or(Error::MatrixBlockMissing(block.block_id))?;
        let zero_crc = crc32_zeroes(block.slot_stride)?;
        for _ in 0..cell_count {
            bytes.extend_from_slice(&zero_crc.to_le_bytes());
        }
    }
    for block in spec.matrix_blocks {
        let (_, _, cell_count) = *block_offsets
            .get(&block.block_id)
            .ok_or(Error::MatrixBlockMissing(block.block_id))?;
        bytes.resize(
            bytes
                .len()
                .checked_add(
                    usize::try_from(bit_bytes(cell_count)?)
                        .map_err(|_| Error::InvalidMatrixLayout)?,
                )
                .ok_or(Error::InvalidMatrixLayout)?,
            0,
        );
    }
    Ok(bytes)
}

fn verify_crc_table(
    file: &mut File,
    crc: Option<&MatrixCrcLayout>,
    metadata_segments: &[&[u8]],
    commit_plans: &[StoredCommitPlan],
    commit_bits: &[u8],
    commit_map_base: u64,
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
    let table = read_range(file, crc.region_offset, prefix_len)?;
    if table.len() < usize::try_from(MCRC_HEADER_LEN).map_err(|_| Error::InvalidMatrixLayout)?
        || &table[0..4] != MCRC_MAGIC
        || u16::from_le_bytes(table[4..6].try_into().expect("slice")) != MCRC_VERSION
    {
        return Err(Error::InvalidMatrixLayout);
    }

    let mut verification = MatrixCrcVerification::default();
    let stored_metadata = read_u32_from_table(&table, crc.metadata_crc_offset - crc.region_offset)?;
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

    let commit_start = MCRC_HEADER_LEN;
    for (index, (name, _, _, map_offset, map_len)) in commit_plans.iter().enumerate() {
        let crc_delta = usize_to_u64(index)?
            .checked_mul(CRC_LEN)
            .ok_or(Error::InvalidMatrixLayout)?;
        let stored = read_u32_from_table(
            &table,
            commit_start
                .checked_add(crc_delta)
                .ok_or(Error::InvalidMatrixLayout)?,
        )?;
        let relative_offset = map_offset
            .checked_sub(commit_map_base)
            .ok_or(Error::InvalidMatrixLayout)?;
        let start = usize::try_from(relative_offset).map_err(|_| Error::InvalidMatrixLayout)?;
        let end = start
            .checked_add(usize::try_from(*map_len).map_err(|_| Error::InvalidMatrixLayout)?)
            .ok_or(Error::InvalidMatrixLayout)?;
        let actual = crc32_bytes(
            commit_bits
                .get(start..end)
                .ok_or(Error::InvalidMatrixLayout)?,
        )?;
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
    for (block_index, block) in spec.matrix_blocks.iter().enumerate() {
        let (_, _, cell_count) = *block_offsets
            .get(&block.block_id)
            .ok_or(Error::MatrixBlockMissing(block.block_id))?;
        let offset = crc.block_valid_offset(spec, block_offsets, block_index)?;
        valid_bits.insert(
            block.block_id,
            read_range(file, offset, bit_bytes(cell_count)?)?,
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
    if version != VMAT_VERSION || header_len != VMAT_HEADER_LEN {
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

fn encode_dimension_table(dimensions: &[MatrixDimensionValue]) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    for dimension in dimensions {
        write_name(&mut bytes, &dimension.name)?;
        bytes.extend_from_slice(&dimension.value.to_le_bytes());
    }
    Ok(bytes)
}

fn decode_dimension_table(bytes: &[u8], count: u32) -> Result<Vec<MatrixDimensionValue>> {
    let mut cursor = Cursor::new(bytes);
    let mut dimensions = Vec::new();
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
    Ok(bytes)
}

fn decode_category_table(bytes: &[u8], count: u32) -> Result<Vec<StoredCommitPlan>> {
    let mut cursor = Cursor::new(bytes);
    let mut commits = Vec::new();
    for _ in 0..count {
        let name = cursor.read_name()?;
        let kind = commit_kind_from_byte(cursor.read_u8()?)?;
        cursor.read_exact(3)?;
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
    let mut offsets = HashMap::with_capacity(expected.len());
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

fn read_range(file: &mut File, offset: u64, len: u64) -> Result<Vec<u8>> {
    let len = usize::try_from(len).map_err(|_| Error::InvalidMatrixLayout)?;
    file.seek(SeekFrom::Start(offset))?;
    let mut bytes = vec![0; len];
    file.read_exact(&mut bytes)?;
    Ok(bytes)
}

fn filled_bytes(len: u64, value: u8) -> Result<Vec<u8>> {
    let len = usize::try_from(len).map_err(|_| Error::InvalidMatrixLayout)?;
    Ok(vec![value; len])
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

fn read_u32_from_table(table: &[u8], offset: u64) -> Result<u32> {
    let offset = usize::try_from(offset).map_err(|_| Error::InvalidMatrixLayout)?;
    let end = offset.checked_add(4).ok_or(Error::InvalidMatrixLayout)?;
    let bytes = table.get(offset..end).ok_or(Error::InvalidMatrixLayout)?;
    Ok(u32::from_le_bytes(bytes.try_into().expect("slice")))
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
        String::from_utf8(bytes.to_vec()).map_err(|_| Error::InvalidMatrixLayout)
    }
}
