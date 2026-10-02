//! Reader-local, staged metadata deltas between immutable matrix generations.
//! Nothing visible changes until all I/O, integrity checks and reservations pass.
use super::*;
use crate::matrix_generation::{MatrixFile, View};

pub(super) struct Changes<'a> {
    old: &'a View,
    next: &'a View,
    pages: &'a [u64],
    limits: ReadLimits,
}
impl Changes<'_> {
    /// The directory diff is sorted. Seek directly to each metadata region;
    /// payload-only changes do not cause payload reads during follow.
    fn units(&self, base: u64, len: u64, unit: u64) -> Result<Vec<u64>> {
        let end = base.checked_add(len).ok_or(Error::InvalidMatrixLayout)?;
        let first = self.pages.partition_point(|p| (p + 1) * 4096 <= base);
        let mut result = Vec::new();
        let mut old = [0; 4096];
        let mut next = [0; 4096];
        for &page in &self.pages[first..] {
            let start = (page * 4096).max(base);
            if start >= end {
                break;
            }
            let stop = ((page + 1) * 4096).min(end);
            let n = (stop - start) as usize;
            self.old.read_exact_at(start, &mut old[..n])?;
            self.next.read_exact_at(start, &mut next[..n])?;
            for i in 0..n {
                if old[i] != next[i] {
                    let index = (start + i as u64 - base) / unit;
                    if result.last() != Some(&index) {
                        // A damaged digest extent must not turn a bounded root
                        // diff into an unbounded temporary list of bitmap pages.
                        self.limits.check(
                            ReadLimitKey::MatrixBitmapBytes,
                            (result.len() as u64 + 1).saturating_mul(8),
                        )?;
                        try_reserve_vec(&mut result, 1, "matrix follow delta")?;
                        result.push(index);
                    }
                }
            }
        }
        Ok(result)
    }
    fn unchanged(&self, base: u64, len: u64) -> Result<()> {
        if !self.units(base, len, 4096)?.is_empty() {
            return Err(Error::InvalidMatrixLayout);
        }
        Ok(())
    }
}

/// O(changed slots + changed bitmap pages), never a clone of a live index.
/// Private fields prevent callers from manufacturing completeness evidence.
pub(super) struct BitmapDelta {
    count: usize,
    slots: Vec<(usize, u64)>,
    mappings: HashMap<u64, Option<u64>>,
    invalidate: Vec<u64>,
}
impl BitmapDelta {
    pub(super) fn prepare(
        bits: &mut SparseBitmap,
        index: u64,
        changes: &Changes<'_>,
        budget: &mut ResidentBitmapBudget,
        verify: bool,
    ) -> Result<Self> {
        let capacity = bits.page_count.min(PAGE_INDEX_MAX_ENTRIES);
        let mut header = [0; 8];
        let count = if capacity == 0 {
            0
        } else {
            changes.next.read_exact_at(index, &mut header)?;
            count_open_bitmap_bytes_read(8);
            page_index_header_count(u64::from_le_bytes(header), capacity)
                .ok_or(Error::MatrixFatalCorruption)?
        };
        let count = usize::try_from(count).map_err(|_| Error::InvalidMatrixLayout)?;
        // Check the minimum retained footprint before staging new entries.
        let slot_bytes = (count as u64)
            .checked_mul(PAGE_INDEX_SLOT_RESIDENT_BYTES)
            .ok_or(Error::InvalidMatrixLayout)?;
        budget
            .limits
            .check(ReadLimitKey::MatrixBitmapBytes, slot_bytes)?;
        let changed = changes.units(index + 8, count as u64 * 8, 8)?;
        let mut slots = Vec::new();
        let mut mappings = HashMap::new();
        let mut affected = |page: u64| -> Result<()> {
            if !mappings.contains_key(&page) {
                try_reserve_map(&mut mappings, 1, "matrix follow delta")?;
                mappings.insert(page, None);
            }
            Ok(())
        };
        for slot in changed {
            let at = usize::try_from(slot).map_err(|_| Error::InvalidMatrixLayout)?;
            changes
                .next
                .read_exact_at(index + 8 + slot * 8, &mut header)?;
            count_open_bitmap_bytes_read(8);
            let raw = u64::from_le_bytes(header);
            if raw == 0 || raw > bits.page_count {
                return Err(Error::MatrixFatalCorruption);
            }
            if let Some(&old) = bits.index_slots.get(at) {
                affected(old)?;
            }
            affected(raw - 1)?;
            try_reserve_vec(&mut slots, 1, "matrix follow delta")?;
            slots.push((at, raw - 1));
        }
        for &page in bits.index_slots.get(count..).unwrap_or(&[]) {
            affected(page)?;
        }
        // Every newly occupied slot must have been in a changed physical page.
        // Previously unused bytes can legitimately contain an old index entry;
        // a header-only growth must therefore read those entries as well.
        let changed_slot_count = slots.len();
        for at in bits.index_slots.len()..count {
            if slots[..changed_slot_count]
                .binary_search_by_key(&at, |s| s.0)
                .is_ok()
            {
                continue;
            }
            changes
                .next
                .read_exact_at(index + 8 + at as u64 * 8, &mut header)?;
            count_open_bitmap_bytes_read(8);
            let raw = u64::from_le_bytes(header);
            if raw == 0 || raw > bits.page_count {
                return Err(Error::MatrixFatalCorruption);
            }
            affected(raw - 1)?;
            try_reserve_vec(&mut slots, 1, "matrix follow delta")?;
            slots.push((at, raw - 1));
            // Appended slots can precede a changed slot already collected.
            // Sort once below; use the changed-prefix lookup above instead.
        }
        slots.sort_unstable_by_key(|s| s.0);
        for (&page, first) in &mut mappings {
            if let Some(&at) = bits.indexed_pages.get(&page)
                && (at as usize) < count
                && slots
                    .binary_search_by_key(&(at as usize), |s| s.0)
                    .map_or(true, |i| slots[i].1 == page)
            {
                *first = Some(at);
            }
        }
        for &(at, page) in &slots {
            let first = mappings.get_mut(&page).unwrap();
            *first = Some(first.map_or(at as u64, |old| old.min(at as u64)));
        }
        // Duplicate slots are accepted by crash recovery. If the first copy
        // moved, find the surviving copy within this bitmap, not the layout.
        if bits.index_slots.len() != bits.indexed_pages.len() && !mappings.is_empty() {
            for (at, &old) in bits.index_slots.iter().take(count).enumerate() {
                let page = slots
                    .binary_search_by_key(&at, |s| s.0)
                    .map_or(old, |i| slots[i].1);
                if let Some(first) = mappings.get_mut(&page) {
                    *first = Some(first.map_or(at as u64, |v| v.min(at as u64)));
                }
            }
        }
        let mut distinct = bits.indexed_pages.len();
        for (&page, first) in &mappings {
            distinct -= usize::from(bits.indexed_pages.contains_key(&page));
            distinct += usize::from(first.is_some());
        }
        let bytes = slot_bytes
            .checked_add(
                (distinct as u64)
                    .checked_mul(PAGE_INDEX_MAP_RESIDENT_BYTES)
                    .ok_or(Error::InvalidMatrixLayout)?,
            )
            .ok_or(Error::InvalidMatrixLayout)?;
        budget.release_index(bits.resident_index_bytes());
        budget.charge_index(bytes)?;
        let extra = count.saturating_sub(bits.index_slots.len());
        try_reserve_vec(
            &mut bits.index_slots,
            extra,
            ReadLimitKey::MatrixBitmapBytes.resource(),
        )?;
        let additions = mappings
            .iter()
            .filter(|(p, at)| at.is_some() && !bits.indexed_pages.contains_key(p))
            .count();
        try_reserve_map(
            &mut bits.indexed_pages,
            additions,
            ReadLimitKey::MatrixBitmapBytes.resource(),
        )?;
        let mut invalidate = Vec::new();
        if let Some(backing) = &bits.backing {
            invalidate = changes.units(backing.base_offset, bits.byte_len, BITMAP_PAGE_BYTES)?;
            if let Some(digest) = backing.digest_base {
                let digest_pages =
                    changes.units(digest, bits.page_count * PAGE_DIGEST_LEN, PAGE_DIGEST_LEN)?;
                try_reserve_vec(&mut invalidate, digest_pages.len(), "matrix follow delta")?;
                invalidate.extend(digest_pages);
            }
            try_reserve_vec(&mut invalidate, mappings.len(), "matrix follow delta")?;
            invalidate.extend(mappings.keys().copied());
            invalidate.sort_unstable();
            invalidate.dedup();
            if verify && let Some(digest) = backing.digest_base {
                let mut bytes = [0; 4096];
                for &page in &invalidate {
                    let len = bits.page_len(page)? as usize;
                    changes.next.read_exact_at(
                        backing.base_offset + page * BITMAP_PAGE_BYTES,
                        &mut bytes[..len],
                    )?;
                    changes
                        .next
                        .read_exact_at(page_digest_offset(digest, page)?, &mut header)?;
                    let stored = u32::from_le_bytes(header[..4].try_into().unwrap());
                    let state = u32::from_le_bytes(header[4..].try_into().unwrap());
                    if !page_bytes_are_authentic(&bytes[..len], stored, state)? {
                        return Err(Error::MatrixFatalCorruption);
                    }
                    count_open_bitmap_bytes_read(len as u64 + 8);
                    count_open_bitmap_pages_visited(1);
                }
            }
        }
        Ok(Self {
            count,
            slots,
            mappings,
            invalidate,
        })
    }
    pub(super) fn apply(self, bits: &mut SparseBitmap) {
        for (at, page) in self.slots {
            if at < bits.index_slots.len() {
                bits.index_slots[at] = page;
            } else {
                debug_assert_eq!(at, bits.index_slots.len());
                bits.index_slots.push(page);
            }
        }
        bits.index_slots.truncate(self.count);
        for (page, at) in self.mappings {
            match at {
                Some(at) => {
                    bits.indexed_pages.insert(page, at);
                }
                None => {
                    bits.indexed_pages.remove(&page);
                }
            }
        }
        let store = bits.store_mut();
        for page in self.invalidate {
            store.lru_unlink(page);
            if let Some(held) = store.pages.remove(&page) {
                store.ones = store.ones.saturating_sub(held.ones);
                if held.cached {
                    store.cached_bytes -= held.bytes.len() as u64;
                } else {
                    store.charged_bytes -= held.bytes.len() as u64;
                }
            }
        }
    }
}

pub(crate) struct MatrixDelta {
    commits: Vec<BitmapDelta>,
    validity: Vec<Option<BitmapDelta>>,
    source: Source,
    resident_index: u64,
}
impl MatrixDelta {
    pub(crate) fn prepare(
        layout: &mut MatrixLayout,
        old: &MatrixFile,
        next: &MatrixFile,
        layout_start: u64,
    ) -> Result<Self> {
        // An incomplete mirror cannot prove a delta complete. Keep the captured
        // generation on failure; a fresh open is the explicit recovery route.
        if layout
            .crc_findings
            .iter()
            .any(|f| f.severity == MatrixCorruptionSeverity::Fatal)
            || layout
                .commits
                .iter()
                .any(|c| c.quarantine_finding.is_some())
        {
            return Err(Error::MatrixFatalCorruption);
        }
        let pages = old.changed_pages(next)?;
        let changes = Changes {
            old: old.paged().unwrap(),
            next: next.paged().unwrap(),
            pages: &pages,
            limits: layout.read_limits,
        };
        let header = read_header(&mut old.read_clone(), layout_start)?;
        changes.unchanged(0, header.commit_map_off)?;
        if header.region_crc_len != 0 {
            changes.unchanged(header.region_crc_off, MCRC_HEADER_LEN)?;
        }
        let mut budget = ResidentBitmapBudget::resume(
            layout.read_limits,
            layout.resident_bitmap_bytes,
            layout.resident_page_index_bytes,
        );
        let verify = layout.read_limits.admit_matrix_metadata_verification();
        let mut commits = Vec::new();
        try_reserve_vec(&mut commits, layout.commits.len(), "matrix follow delta")?;
        for commit in &mut layout.commits {
            commits.push(BitmapDelta::prepare(
                &mut commit.bits,
                commit.index_offset,
                &changes,
                &mut budget,
                verify,
            )?);
        }
        let mut validity = Vec::new();
        try_reserve_vec(&mut validity, layout.blocks.len(), "matrix follow delta")?;
        for block in &mut layout.blocks {
            validity.push(match block.crc_valid_index_offset {
                Some(index) => Some(block.crc_valid_bits.prepare_follow(
                    index,
                    &changes,
                    &mut budget,
                )?),
                None => None,
            });
        }
        Ok(Self {
            commits,
            validity,
            source: next.source()?,
            resident_index: budget.index,
        })
    }
    pub(crate) fn apply(self, layout: &mut MatrixLayout) {
        for (delta, commit) in self.commits.into_iter().zip(&mut layout.commits) {
            delta.apply(&mut commit.bits);
        }
        for (delta, block) in self.validity.into_iter().zip(&mut layout.blocks) {
            if let Some(delta) = delta {
                block.crc_valid_bits.apply_follow(delta);
            }
        }
        layout.resident_page_index_bytes = self.resident_index;
        rebind_generation_backing(layout, &self.source);
        record_open_resident_bitmap_bytes(layout);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Seek, Write};

    fn index_case(old_slots: &[u64], physical_slots: &[u64], new_slots: &[u64]) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index.varve");
        let base = std::fs::OpenOptions::new()
            .create_new(true)
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        base.set_len(4096).unwrap();
        let mut writer = MatrixFile::create(&path, &base, 4096, [1; 16], 64).unwrap();
        writer
            .write_all(
                &page_index_header_value(old_slots.len() as u64)
                    .unwrap()
                    .to_le_bytes(),
            )
            .unwrap();
        for &page in physical_slots {
            writer.write_all(&(page + 1).to_le_bytes()).unwrap();
        }
        writer.publish(4096).unwrap();
        let old = writer.refreshed().unwrap();
        let mut bits = SparseBitmap::new(8 * 32768).unwrap();
        let mut budget = ResidentBitmapBudget::new(ReadLimits::STANDARD);
        for &page in old_slots {
            bits.note_indexed_page(page, &mut budget).unwrap();
        }
        writer.seek(SeekFrom::Start(0)).unwrap();
        writer
            .write_all(
                &page_index_header_value(new_slots.len() as u64)
                    .unwrap()
                    .to_le_bytes(),
            )
            .unwrap();
        for &page in new_slots {
            writer.write_all(&(page + 1).to_le_bytes()).unwrap();
        }
        writer.publish(4096).unwrap();
        let next = writer.refreshed().unwrap();
        let pages = old.changed_pages(&next).unwrap();
        let changes = Changes {
            old: old.paged().unwrap(),
            next: next.paged().unwrap(),
            pages: &pages,
            limits: ReadLimits::STANDARD,
        };
        let delta = BitmapDelta::prepare(&mut bits, 0, &changes, &mut budget, false).unwrap();
        assert_eq!(
            bits.index_slots, old_slots,
            "preparation must not change the visible mirror"
        );
        delta.apply(&mut bits);
        assert_eq!(bits.index_slots, new_slots);
        let mut expected = HashMap::new();
        for (at, &page) in new_slots.iter().enumerate() {
            expected.entry(page).or_insert(at as u64);
        }
        assert_eq!(bits.indexed_pages, expected);
        assert_eq!(budget.index, bits.resident_index_bytes());
    }

    #[test]
    fn deltas_preserve_duplicate_survivors_swap_removals_and_header_only_growth() {
        index_case(&[2, 1, 2], &[2, 1, 2], &[1, 2]);
        index_case(&[2, 1, 2], &[2, 1, 2], &[1, 1]);
        index_case(&[0, 1, 2], &[0, 1, 2], &[2, 1]);
        index_case(&[0], &[0, 1, 2], &[0, 1, 2]);
        index_case(&[0], &[0, 1, 2, 3, 4], &[0, 1, 2, 5, 4]);
        index_case(&[0, 1, 2], &[0, 1, 2], &[]);
    }
}
