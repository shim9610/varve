//! Derived offset sidecar: a cache of record offsets, and nothing else.
//!
//! # What this is
//!
//! One `u64` little-endian record offset per element, contiguous, so element
//! `i` lives at `DATA_START + i * 8` and lookup is a single positional read
//! with no directory, no indirection and no tree. That contiguity is the entire
//! reason a sidecar was chosen over an inline structure, and every decision
//! here is subordinate to keeping it.
//!
//! # The two conditions this file exists to satisfy
//!
//! **C1 — the sidecar is a cache and holds nothing the main file does not.**
//! The only fact stored per element is `record_offset`. The other twelve fields
//! of [`crate::RecordIndexEntry`] are recovered by reading the 32-byte record
//! header at that offset; `record_offset` itself is recovered by the forward
//! scan the crate already performs at open. So this file is a memoisation of a
//! scan, and deleting it loses no fact — it costs one slow open, which is
//! exactly what an undeclared format pays today.
//!
//! The rule that keeps C1 true as this code changes: **if a field can be read
//! from the record header, it must not appear in the sidecar.** No block ids
//! (the scope determines them), no sequences per element, no commit bits, no
//! lengths, no checksums, no key material.
//!
//! C1 has one consequence that is easy to miss and important: the scope is
//! **not** part of the computed schema hash and not recorded in the embedded
//! manifest, because a file's identity must not depend on a cache.
//!
//! The concrete cost of getting that wrong is about *evolution*, not about two
//! handles disagreeing. `computed_schema_hash()` is stored in newly created
//! files, and a format may pin it and compare it at open. So if the scope were
//! hashed, then turning the sidecar on for an existing format would make every
//! file already written refuse to open — files whose bytes did not change at
//! all. Enabling a derived cache would be a migration. It must not be.
//!
//! **C2 — every divergence between the two files is detectable and repairable
//! without loss.** Consistency is not maintained by keeping them in step at
//! every instant; that would need a two-phase commit across two objects on
//! every append, and the append hot path forbids it. Instead every way they can
//! diverge has a detection and a repair, and none of them is an error:
//!
//! | divergence | detected by | repair |
//! | --- | --- | --- |
//! | names a different object | [`SidecarHeader::native_fingerprint`] | discard, regenerate |
//! | behind the main file | `covered_main_len < main_len` | forward scan from `covered_main_len` |
//! | ahead of the main file | `covered_main_len > main_len` | truncate to the covered prefix, then scan |
//! | torn, short, bit-flipped | header check value, length arithmetic | discard the damaged suffix, scan |
//! | absent | open | full regeneration |
//!
//! # Why the header check is not a CRC
//!
//! `crc32fast` is available only under the `integrity` feature (`crc32_bytes`
//! returns [`crate::Error::IntegrityFeatureDisabled`] without it), and this
//! capability must not be feature-gated. The header therefore carries an FNV-1a
//! check value, which is a **redundancy code, not authentication** — the same
//! standing the matrix page-index occupancy header already has, in the same
//! words. It makes no claim against an actor who can rewrite the file.
//!
//! It does not have to. Under C1 the worst a forged or corrupt sidecar can do
//! is name a wrong offset, and the record header read at that offset is checked
//! exactly as it is today: a wrong offset yields a **refused read**, never a
//! wrong record. That is why per-element digests are absent as well — they
//! would also have to live outside the array to keep `i * 8` exact, and they
//! would buy detection of a condition that is already refused downstream.
//!
//! # Not yet wired
//!
//! **No caller in `file.rs` constructs any of this yet.** The declaration
//! surface ([`OffsetSidecarScope`], the `index: [offset_sidecar(..)]` DSL, the
//! spec validation) is complete and tested; the writer and open integration is
//! not written. The allow below is what that state costs, and it is temporary:
//! delete it in the same change that adds the first caller, because from then
//! on an unused item here is a real finding rather than a known gap.
//!
//! Nothing above is inert-by-accident as a result. A format that declares a
//! scope today gets the validated policy and no sidecar file, which is exactly
//! what a format that declares nothing gets — see the C1 note on why that is a
//! legitimate state for this type rather than a broken one.

#![allow(dead_code)]

use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};

use crate::error::{Error, Result};
use crate::format::{FormatSpec, OffsetSidecarScope, ReadLimitKey};
use crate::snapshot::{SnapshotFile, read_at, write_all_at};

/// Fixed header length, so `DATA_START` is a constant and the header can be
/// rewritten in place without moving the array.
pub(crate) const HEADER_LEN: u64 = 4096;
/// Element stride. Eight bytes, and this is not a knob: see the module note on
/// what may not be stored here.
pub(crate) const ELEMENT_LEN: u64 = 8;
/// Elements buffered before a positional write. 4096 bytes at 8 bytes each, to
/// match `matrix.rs`'s page size, and the reason the crash catch-up in
/// [`SidecarState::behind_by`] is bounded by 512 records.
pub(crate) const BUFFER_ELEMENTS: usize = 512;

const MAGIC: &[u8; 4] = b"VIX1";
const VERSION: u16 = 1;

const SCOPE_UNIFIED: u16 = 0;
const SCOPE_BLOCK: u16 = 1;

// Field offsets, derived from each other rather than written out, because a
// hand-counted decode range that disagreed with the encode order is exactly the
// defect this layout produced the first time it was written.
const OFF_MAGIC: usize = 0;
const OFF_VERSION: usize = OFF_MAGIC + 4;
const OFF_SCOPE: usize = OFF_VERSION + 2;
const OFF_BLOCK_ID: usize = OFF_SCOPE + 2;
const OFF_SCHEMA_HASH: usize = OFF_BLOCK_ID + 4;
const OFF_FINGERPRINT: usize = OFF_SCHEMA_HASH + 8;
const OFF_COVERED_MAIN_LEN: usize = OFF_FINGERPRINT + 32;
const OFF_COVERED_ELEMENTS: usize = OFF_COVERED_MAIN_LEN + 8;
const OFF_COVERED_SEQUENCE: usize = OFF_COVERED_ELEMENTS + 8;
/// Byte length of the header prefix the check value covers.
const CHECKED_PREFIX_LEN: usize = OFF_COVERED_SEQUENCE + 8;

/// FNV-1a over the header prefix.
///
/// A redundancy code, not authentication; see the module documentation for why
/// that is the right standing for this file rather than a compromise.
fn header_check(bytes: &[u8]) -> u64 {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut state = OFFSET;
    for byte in bytes {
        state ^= u64::from(*byte);
        state = state.wrapping_mul(PRIME);
    }
    // Fold in a non-zero constant so an all-zero prefix does not check as
    // valid: a freshly extended sparse region reads as zeros, and an all-zero
    // header must be rejected as absent rather than adopted as empty.
    state ^ 0x5646_4958_5f43_484b
}

/// The sidecar path for a scope.
///
/// `<main>.vix` for [`OffsetSidecarScope::Unified`], `<main>.vix.<block_id>`
/// for a block-scoped one. Deriving it by extension append rather than by
/// replacement keeps a main file named `data.varve` and one named `data` from
/// colliding, exactly as `disk_index::sidecar_path` does for `.vki`.
pub(crate) fn sidecar_path(main: &Path, block_id: Option<u32>) -> PathBuf {
    let mut name = main.as_os_str().to_os_string();
    match block_id {
        Some(id) => name.push(format!(".vix.{id}")),
        None => name.push(".vix"),
    }
    PathBuf::from(name)
}

/// Every sidecar path a spec's scope calls for, paired with the block id it
/// indexes.
///
/// Returned in declaration order for [`OffsetSidecarScope::PerBlock`] so a
/// session's sidecars are opened in a deterministic order.
pub(crate) fn sidecar_targets(spec: FormatSpec, main: &Path) -> Vec<(Option<u32>, PathBuf)> {
    match spec.index_policy.offset_sidecar {
        OffsetSidecarScope::None => Vec::new(),
        OffsetSidecarScope::Unified => vec![(None, sidecar_path(main, None))],
        OffsetSidecarScope::PerBlock => spec
            .blocks
            .iter()
            .map(|block| (Some(block.id), sidecar_path(main, Some(block.id))))
            .collect(),
        OffsetSidecarScope::Blocks(ids) => ids
            .iter()
            .map(|id| (Some(*id), sidecar_path(main, Some(*id))))
            .collect(),
    }
}

/// The sidecar's claim about itself.
///
/// Every field here is either identity (checked against the main file before
/// anything is believed) or a coverage claim (validated against the main file's
/// length before it is used as a starting point). Nothing here is a fact about
/// a record.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct SidecarHeader {
    pub(crate) scope: u16,
    pub(crate) block_id: u32,
    pub(crate) schema_hash: u64,
    /// Identity of the main file's OS object folded with the schema hash and
    /// the main file's header bytes, computed exactly as
    /// [`crate::stream::primary_identity`] computes its own — the mechanism
    /// `MatrixSidecarManifest` already uses (DUR-04) to stop a same-spec
    /// sibling adopting a foreign sidecar.
    pub(crate) native_fingerprint: [u8; 32],
    /// The main-file length this sidecar describes.
    pub(crate) covered_main_len: u64,
    /// Number of valid elements in the array.
    pub(crate) covered_elements: u64,
    /// `sequence` of the last indexed record, for diagnostics and for the
    /// stale-generation check after an atomic republish.
    pub(crate) covered_sequence: u64,
}

impl SidecarHeader {
    pub(crate) fn encode(&self) -> [u8; HEADER_LEN as usize] {
        let mut bytes = [0u8; HEADER_LEN as usize];
        let mut at = 0usize;
        let mut put = |source: &[u8], at: &mut usize| {
            bytes[*at..*at + source.len()].copy_from_slice(source);
            *at += source.len();
        };
        put(MAGIC, &mut at);
        put(&VERSION.to_le_bytes(), &mut at);
        put(&self.scope.to_le_bytes(), &mut at);
        put(&self.block_id.to_le_bytes(), &mut at);
        put(&self.schema_hash.to_le_bytes(), &mut at);
        put(&self.native_fingerprint, &mut at);
        put(&self.covered_main_len.to_le_bytes(), &mut at);
        put(&self.covered_elements.to_le_bytes(), &mut at);
        put(&self.covered_sequence.to_le_bytes(), &mut at);
        debug_assert_eq!(at, CHECKED_PREFIX_LEN);
        let check = header_check(&bytes[..CHECKED_PREFIX_LEN]);
        bytes[CHECKED_PREFIX_LEN..CHECKED_PREFIX_LEN + 8].copy_from_slice(&check.to_le_bytes());
        bytes
    }

    /// Decodes a header, or reports why it is not usable.
    ///
    /// Returns `None` rather than an error for every rejection. A sidecar that
    /// cannot be decoded is a cache miss, not a fault: C1 guarantees the main
    /// file is sufficient, so the only correct response is to regenerate.
    pub(crate) fn decode(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < HEADER_LEN as usize || &bytes[..4] != MAGIC {
            return None;
        }
        let stored = u64::from_le_bytes(
            bytes[CHECKED_PREFIX_LEN..CHECKED_PREFIX_LEN + 8]
                .try_into()
                .ok()?,
        );
        if stored != header_check(&bytes[..CHECKED_PREFIX_LEN]) {
            return None;
        }
        let u16_at = |at: usize| -> Option<u16> {
            bytes[at..at + 2].try_into().ok().map(u16::from_le_bytes)
        };
        let u32_at = |at: usize| -> Option<u32> {
            bytes[at..at + 4].try_into().ok().map(u32::from_le_bytes)
        };
        let u64_at = |at: usize| -> Option<u64> {
            bytes[at..at + 8].try_into().ok().map(u64::from_le_bytes)
        };
        if u16_at(OFF_VERSION)? != VERSION {
            return None;
        }
        let scope = u16_at(OFF_SCOPE)?;
        if scope != SCOPE_UNIFIED && scope != SCOPE_BLOCK {
            return None;
        }
        let mut native_fingerprint = [0u8; 32];
        native_fingerprint.copy_from_slice(&bytes[OFF_FINGERPRINT..OFF_FINGERPRINT + 32]);
        Some(Self {
            scope,
            block_id: u32_at(OFF_BLOCK_ID)?,
            schema_hash: u64_at(OFF_SCHEMA_HASH)?,
            native_fingerprint,
            covered_main_len: u64_at(OFF_COVERED_MAIN_LEN)?,
            covered_elements: u64_at(OFF_COVERED_ELEMENTS)?,
            covered_sequence: u64_at(OFF_COVERED_SEQUENCE)?,
        })
    }

    fn matches(&self, expect_block: Option<u32>, schema_hash: u64, fingerprint: &[u8; 32]) -> bool {
        let scope_ok = match expect_block {
            None => self.scope == SCOPE_UNIFIED,
            Some(id) => self.scope == SCOPE_BLOCK && self.block_id == id,
        };
        scope_ok && self.schema_hash == schema_hash && &self.native_fingerprint == fingerprint
    }
}

/// What an open found, and therefore what the caller owes.
///
/// Deliberately not a `Result`: **no variant is an error.** Each one names a
/// bounded repair, and the worst of them costs exactly what an undeclared
/// format pays at every open today.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SidecarState {
    /// `covered_main_len == main_len`. Adopt as is; no scan.
    Current { elements: u64 },
    /// The sidecar describes a prefix. Scan forward from `from_offset`.
    Behind { elements: u64, from_offset: u64 },
    /// Absent, foreign, torn, or ahead of a main file that was truncated.
    /// Rebuild from scratch by full scan — today's `scan_on_open` cost.
    Rebuild,
}

impl SidecarState {
    /// Records the catch-up scan owes, for the bounded-recovery assertion.
    pub(crate) fn behind_by(&self, main_len: u64) -> u64 {
        match self {
            Self::Current { .. } => 0,
            Self::Behind { from_offset, .. } => main_len.saturating_sub(*from_offset),
            Self::Rebuild => main_len,
        }
    }
}

/// One open sidecar: the file, its claim, and the write buffer.
///
/// The buffer is the whole of the resident cost — `BUFFER_ELEMENTS * 8` bytes,
/// a constant, independent of record count. That is the property the resident
/// index does not have and the reason this type exists.
#[derive(Debug)]
pub(crate) struct OffsetSidecar {
    file: File,
    path: PathBuf,
    block_id: Option<u32>,
    schema_hash: u64,
    native_fingerprint: [u8; 32],
    /// Elements durably in the array, buffer excluded.
    written_elements: u64,
    buffer: Vec<u64>,
    /// Set when a write failed. A failed sidecar stops updating for the
    /// session and the next open catches up by scan; it never fails an append.
    /// See the invariant-3 obligation in [`Self::push`].
    disabled: bool,
}

/// Who is opening, and therefore whether the sidecar may be written.
///
/// This distinction is load-bearing, not a convenience.
///
/// **A read-only open must never create, extend, or repair the sidecar.** Three
/// things go wrong the moment it does:
///
/// 1. It makes a *read* mutate the filesystem. Every read entry point takes
///    `&self` so one handle can serve concurrent readers; a read that repairs
///    would need `&mut`, or interior mutability over a file two threads are
///    both trying to rebuild.
/// 2. Read-only opens do not take the single-writer lock — that lock protects
///    the main file. Two concurrent read-only opens finding the sidecar stale
///    would both rebuild it, interleaving header and array writes on one file
///    with nothing serialising them. The tear is detected and repaired, so
///    nothing is *wrong*, but every open would rewrite it, forever.
/// 3. A file on a read-only medium, or in a directory the process cannot write,
///    could not be opened at all — an accelerator's absence would become a
///    refusal to open the data. That is a direct C1 violation.
///
/// So a reader uses the sidecar when it is valid and current, and otherwise
/// simply does not use it: it falls back to the scan, which is exactly what an
/// undeclared format pays at every open today. Creation and repair belong to
/// the writer, which already holds the lock that serialises them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SidecarAccess {
    /// Read-only. Opens an existing sidecar without `create` and without
    /// `write`, and never mutates it.
    ReadOnly,
    /// The single-writer handle. May create, extend and repair.
    Writable,
}

impl OffsetSidecar {
    /// Opens the sidecar and reports what state it is in.
    ///
    /// Returns `Ok(None)` — never `Err` — when the sidecar cannot be used at
    /// all: absent under [`SidecarAccess::ReadOnly`], unopenable, or on a
    /// medium that refuses the access. C1 makes that the correct outcome
    /// rather than a fault: the main file is sufficient, so the caller scans.
    ///
    /// A returned sidecar under [`SidecarAccess::ReadOnly`] is usable for
    /// reads; whether it may be *believed* is [`SidecarState`]'s answer, and a
    /// read-only caller that gets anything but [`SidecarState::Current`] must
    /// fall back to the scan rather than repair.
    pub(crate) fn open(
        spec: FormatSpec,
        path: PathBuf,
        block_id: Option<u32>,
        schema_hash: u64,
        native_fingerprint: [u8; 32],
        main_len: u64,
        access: SidecarAccess,
    ) -> Result<Option<(Self, SidecarState)>> {
        let mut options = OpenOptions::new();
        options.read(true);
        if access == SidecarAccess::Writable {
            options.write(true).create(true).truncate(false);
        }
        // An open failure is a cache miss, not a fault. A missing sidecar under
        // ReadOnly, a read-only directory, a permission denial: all mean "no
        // accelerator", and none of them may stop the main file from opening.
        let Ok(file) = options.open(&path) else {
            return Ok(None);
        };
        let mut sidecar = Self {
            file,
            path,
            block_id,
            schema_hash,
            native_fingerprint,
            written_elements: 0,
            buffer: Vec::new(),
            // A read-only sidecar is permanently in the state a failed write
            // leaves a writable one in: readable, never written. Reusing the
            // flag means there is one predicate guarding every mutation rather
            // than two that could disagree.
            disabled: access == SidecarAccess::ReadOnly,
        };
        let state = sidecar.classify(spec, main_len)?;
        if let SidecarState::Current { elements } | SidecarState::Behind { elements, .. } = state {
            sidecar.written_elements = elements;
        }
        Ok(Some((sidecar, state)))
    }

    /// Whether this handle may be believed without further work.
    ///
    /// A read-only handle answers `true` only for [`SidecarState::Current`]:
    /// it cannot perform the catch-up that [`SidecarState::Behind`] calls for,
    /// and must not try.
    pub(crate) fn is_usable_without_repair(state: SidecarState) -> bool {
        matches!(state, SidecarState::Current { .. })
    }

    /// Classifies the sidecar. Total by construction: **every** rejection is a
    /// [`SidecarState`], never an `Err`.
    ///
    /// A metadata failure, a length over `max_sidecar_len`, an unreadable
    /// header — under C1 all of these mean the same thing, that the cache
    /// cannot be believed, and the main file answers anyway. Propagating any of
    /// them would let a damaged accelerator fail an open.
    fn classify(&mut self, spec: FormatSpec, main_len: u64) -> Result<SidecarState> {
        let Ok(metadata) = self.file.metadata() else {
            return Ok(SidecarState::Rebuild);
        };
        let physical_len = metadata.len();
        if physical_len < HEADER_LEN {
            return Ok(SidecarState::Rebuild);
        }
        if spec
            .read_limits
            .check(ReadLimitKey::SidecarLen, physical_len)
            .is_err()
        {
            return Ok(SidecarState::Rebuild);
        }
        let mut bytes = vec![0u8; HEADER_LEN as usize];
        if read_exact_at(&self.file, &mut bytes, 0).is_err() {
            return Ok(SidecarState::Rebuild);
        }
        let Some(header) = SidecarHeader::decode(&bytes) else {
            return Ok(SidecarState::Rebuild);
        };
        if !header.matches(self.block_id, self.schema_hash, &self.native_fingerprint) {
            return Ok(SidecarState::Rebuild);
        }
        // The array must physically hold what the header claims. A short file
        // under a valid header is a torn write; the claim is not believed.
        let claimed_len = header
            .covered_elements
            .checked_mul(ELEMENT_LEN)
            .and_then(|bytes| bytes.checked_add(HEADER_LEN));
        let Some(claimed_len) = claimed_len else {
            return Ok(SidecarState::Rebuild);
        };
        if physical_len < claimed_len {
            return Ok(SidecarState::Rebuild);
        }
        if header.covered_main_len > main_len {
            // Ahead: the main file was truncated by uncommitted-tail recovery,
            // or replaced. The covered prefix cannot be trusted to describe the
            // current object even where it overlaps, because a republished
            // generation moves every record offset. Rebuild.
            return Ok(SidecarState::Rebuild);
        }
        if header.covered_main_len == main_len {
            return Ok(SidecarState::Current {
                elements: header.covered_elements,
            });
        }
        Ok(SidecarState::Behind {
            elements: header.covered_elements,
            from_offset: header.covered_main_len,
        })
    }

    /// Discards the array and starts over. Used when the state is
    /// [`SidecarState::Rebuild`] and the caller has scanned the main file.
    pub(crate) fn reset(&mut self) {
        self.written_elements = 0;
        self.buffer.clear();
    }

    /// Appends one record offset.
    ///
    /// # The invariant-3 obligation
    ///
    /// Every call here happens **after** the authoritative commit — the record
    /// is already appended to the main file. That places this in the class the
    /// invariant checklist tracks (a fallible step after an authoritative
    /// commit), and the obligation this type accepts is that it is **not
    /// fallible in the direction that matters**: a sidecar write that fails
    /// disables the sidecar for the session and returns `Ok`. It never fails
    /// the append, never poisons the writer, and is never reported as an error.
    ///
    /// That is sound only because of C1. It is also why this must never become
    /// the last step *of* a commit: it is a separate, best-effort action after
    /// one.
    ///
    /// Allocation-free on the hot path: the buffer is reserved once at open and
    /// `push` into reserved capacity cannot allocate.
    pub(crate) fn push(&mut self, record_offset: u64) {
        if self.disabled {
            return;
        }
        if self.buffer.capacity() < BUFFER_ELEMENTS {
            // One reservation per session. A failure here disables rather than
            // propagates, for the same reason a write failure does.
            if self.buffer.try_reserve_exact(BUFFER_ELEMENTS).is_err() {
                self.disabled = true;
                return;
            }
        }
        self.buffer.push(record_offset);
        if self.buffer.len() >= BUFFER_ELEMENTS {
            self.drain_buffer();
        }
    }

    fn drain_buffer(&mut self) {
        if self.disabled || self.buffer.is_empty() {
            return;
        }
        let mut bytes = Vec::new();
        if bytes.try_reserve_exact(self.buffer.len() * 8).is_err() {
            self.disabled = true;
            return;
        }
        for offset in &self.buffer {
            bytes.extend_from_slice(&offset.to_le_bytes());
        }
        let at = HEADER_LEN + self.written_elements * ELEMENT_LEN;
        if write_all_at(&self.file, &bytes, at).is_err() {
            self.disabled = true;
            return;
        }
        self.written_elements += self.buffer.len() as u64;
        self.buffer.clear();
    }

    /// Flushes the buffer and republishes the header.
    ///
    /// # Ordering
    ///
    /// The array chunk is written before the header that admits it, and the
    /// caller must have made the main file's bytes at least as durable first.
    /// A crash can therefore only leave the sidecar **behind**, never ahead —
    /// behind is repairable by scan, while ahead would be a sidecar claiming to
    /// describe bytes that do not exist.
    pub(crate) fn publish(&mut self, covered_main_len: u64, covered_sequence: u64) {
        if self.disabled {
            return;
        }
        self.drain_buffer();
        if self.disabled {
            return;
        }
        let header = SidecarHeader {
            scope: match self.block_id {
                Some(_) => SCOPE_BLOCK,
                None => SCOPE_UNIFIED,
            },
            block_id: self.block_id.unwrap_or(0),
            schema_hash: self.schema_hash,
            native_fingerprint: self.native_fingerprint,
            covered_main_len,
            covered_elements: self.written_elements,
            covered_sequence,
        };
        if write_all_at(&self.file, &header.encode(), 0).is_err() {
            self.disabled = true;
        }
    }

    /// Durably persists the sidecar. Called after the main file's own sync.
    pub(crate) fn sync(&mut self) {
        if self.disabled {
            return;
        }
        if self.file.sync_all().is_err() {
            self.disabled = true;
        }
    }

    /// Reads element `index`: one positional read, no indirection.
    ///
    /// Takes `&self` — the all-reads-take-`&self` policy holds here with no
    /// interior mutability at all, because the read is positional.
    pub(crate) fn offset_at(&self, index: u64) -> Result<Option<u64>> {
        if index >= self.written_elements {
            return Ok(None);
        }
        let at = HEADER_LEN
            .checked_add(
                index
                    .checked_mul(ELEMENT_LEN)
                    .ok_or(Error::LengthOverflow { value: index })?,
            )
            .ok_or(Error::LengthOverflow { value: index })?;
        let mut bytes = [0u8; 8];
        read_exact_at(&self.file, &mut bytes, at)?;
        Ok(Some(u64::from_le_bytes(bytes)))
    }

    pub(crate) fn elements(&self) -> u64 {
        self.written_elements + self.buffer.len() as u64
    }

    pub(crate) fn is_disabled(&self) -> bool {
        self.disabled
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }
}

fn read_exact_at(file: &File, buffer: &mut [u8], offset: u64) -> Result<()> {
    let mut read = 0usize;
    while read < buffer.len() {
        let at = offset
            .checked_add(read as u64)
            .ok_or(Error::LengthOverflow { value: offset })?;
        match read_at(file, &mut buffer[read..], at) {
            Ok(0) => return Err(Error::UnexpectedEof),
            Ok(count) => read += count,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) => return Err(Error::from(error)),
        }
    }
    Ok(())
}

/// Recomputes the identity a sidecar is bound to.
///
/// Reuses `stream::primary_identity`'s construction rather than inventing a
/// second one: eight `crc32`-free FNV lanes over `(lane, schema_hash,
/// object identity, main file header)`. The object identity is the volume and
/// file id on Windows and the device and inode on Unix, which is what stops a
/// same-spec sibling adopting this sidecar (DUR-04).
pub(crate) fn native_fingerprint(
    spec: FormatSpec,
    snapshot: &SnapshotFile,
    header_len: u64,
) -> Result<[u8; 32]> {
    let file = snapshot.try_clone_file()?;
    let header = snapshot.read_vec_at(0, header_len, header_len, "file header")?;
    let object_identity = opened_object_identity(&file)?;
    let schema_hash = if spec.schema_hash == 0 {
        spec.computed_schema_hash()
    } else {
        spec.schema_hash
    };
    let mut fingerprint = [0u8; 32];
    for lane in 0..8u32 {
        let mut lane_bytes = Vec::new();
        lane_bytes.extend_from_slice(&lane.to_le_bytes());
        lane_bytes.extend_from_slice(&schema_hash.to_le_bytes());
        lane_bytes.extend_from_slice(&object_identity);
        lane_bytes.extend_from_slice(&header);
        let value = header_check(&lane_bytes);
        fingerprint[(lane as usize) * 4..(lane as usize + 1) * 4]
            .copy_from_slice(&(value as u32).to_le_bytes());
    }
    Ok(fingerprint)
}

#[cfg(unix)]
fn opened_object_identity(file: &File) -> Result<Vec<u8>> {
    use std::os::unix::fs::MetadataExt;

    let metadata = file.metadata()?;
    let mut bytes = Vec::with_capacity(16);
    bytes.extend_from_slice(&metadata.dev().to_le_bytes());
    bytes.extend_from_slice(&metadata.ino().to_le_bytes());
    Ok(bytes)
}

#[cfg(windows)]
fn opened_object_identity(file: &File) -> Result<Vec<u8>> {
    use std::os::windows::fs::MetadataExt;

    let metadata = file.metadata()?;
    let mut bytes = Vec::with_capacity(24);
    bytes.extend_from_slice(&metadata.volume_serial_number().unwrap_or(0).to_le_bytes());
    bytes.extend_from_slice(&metadata.file_index().unwrap_or(0).to_le_bytes());
    Ok(bytes)
}

#[cfg(not(any(unix, windows)))]
fn opened_object_identity(_file: &File) -> Result<Vec<u8>> {
    // No object identity available. An empty identity is still folded in, so
    // the fingerprint degrades to schema hash plus file header rather than
    // silently matching anything.
    Ok(Vec::new())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header() -> SidecarHeader {
        SidecarHeader {
            scope: SCOPE_BLOCK,
            block_id: 6,
            schema_hash: 0x0123_4567_89ab_cdef,
            native_fingerprint: [7u8; 32],
            covered_main_len: 4096,
            covered_elements: 12,
            covered_sequence: 11,
        }
    }

    #[test]
    fn a_header_round_trips() {
        let encoded = header().encode();
        assert_eq!(SidecarHeader::decode(&encoded), Some(header()));
    }

    #[test]
    fn an_all_zero_header_is_not_adopted_as_empty() {
        // A freshly extended sparse region reads as zeros. Adopting that as a
        // valid empty sidecar would let a torn create answer reads.
        let zeros = [0u8; HEADER_LEN as usize];
        assert_eq!(SidecarHeader::decode(&zeros), None);
    }

    #[test]
    fn a_flipped_bit_anywhere_in_the_prefix_is_rejected() {
        let encoded = header().encode();
        for position in 0..CHECKED_PREFIX_LEN {
            for bit in 0..8 {
                let mut damaged = encoded;
                damaged[position] ^= 1 << bit;
                assert_eq!(
                    SidecarHeader::decode(&damaged),
                    None,
                    "byte {position} bit {bit} survived"
                );
            }
        }
    }

    #[test]
    fn a_foreign_fingerprint_does_not_match() {
        let header = header();
        assert!(header.matches(Some(6), header.schema_hash, &[7u8; 32]));
        assert!(!header.matches(Some(6), header.schema_hash, &[8u8; 32]));
        assert!(!header.matches(Some(7), header.schema_hash, &[7u8; 32]));
        assert!(!header.matches(None, header.schema_hash, &[7u8; 32]));
        assert!(!header.matches(Some(6), header.schema_hash ^ 1, &[7u8; 32]));
    }

    fn test_spec() -> FormatSpec {
        FormatSpec::builder()
            .magic(b"VIXT")
            .build()
            .expect("minimal spec")
    }

    fn scratch_dir() -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let id = NEXT.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("varve-sidecar-{}-{id}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("scratch dir");
        dir
    }

    #[test]
    fn a_read_only_open_never_creates_the_sidecar() {
        // A read that mutates the filesystem is the defect this guards. It
        // would also need `&mut` on a read path, and would let two concurrent
        // read-only opens race to rebuild one file with no lock between them.
        let dir = scratch_dir();
        let path = dir.join("data.varve.vix");
        let result = OffsetSidecar::open(
            test_spec(),
            path.clone(),
            None,
            1,
            [0u8; 32],
            0,
            SidecarAccess::ReadOnly,
        )
        .expect("open must not error");
        assert!(result.is_none(), "read-only open must report no sidecar");
        assert!(
            !path.exists(),
            "read-only open created {}; a read must not write",
            path.display()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_writable_open_of_a_fresh_path_asks_for_a_rebuild() {
        let dir = scratch_dir();
        let path = dir.join("data.varve.vix");
        let (sidecar, state) = OffsetSidecar::open(
            test_spec(),
            path.clone(),
            None,
            1,
            [0u8; 32],
            0,
            SidecarAccess::Writable,
        )
        .expect("open must not error")
        .expect("writable open creates the sidecar");
        assert_eq!(state, SidecarState::Rebuild);
        assert!(!sidecar.is_disabled());
        assert!(path.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_read_only_handle_refuses_to_mutate() {
        // The read-only handle is born in the same state a failed write leaves
        // a writable one in, so one predicate guards every mutation.
        let dir = scratch_dir();
        let path = dir.join("data.varve.vix");
        // Create it with a writable handle first, so ReadOnly can open it.
        let _ = OffsetSidecar::open(
            test_spec(),
            path.clone(),
            None,
            1,
            [0u8; 32],
            0,
            SidecarAccess::Writable,
        );
        let (mut sidecar, _) = OffsetSidecar::open(
            test_spec(),
            path.clone(),
            None,
            1,
            [0u8; 32],
            0,
            SidecarAccess::ReadOnly,
        )
        .expect("open must not error")
        .expect("an existing sidecar opens read-only");
        assert!(sidecar.is_disabled());
        let before = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        sidecar.push(4096);
        sidecar.publish(4096, 1);
        sidecar.sync();
        assert_eq!(sidecar.elements(), 0, "a read-only handle buffers nothing");
        assert_eq!(
            std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0),
            before,
            "a read-only handle changed the sidecar's bytes"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn only_a_current_sidecar_may_be_believed_without_repair() {
        // A read-only caller cannot perform the catch-up `Behind` calls for,
        // so `Behind` must not read as usable to it.
        assert!(OffsetSidecar::is_usable_without_repair(
            SidecarState::Current { elements: 3 }
        ));
        assert!(!OffsetSidecar::is_usable_without_repair(
            SidecarState::Behind {
                elements: 3,
                from_offset: 64,
            }
        ));
        assert!(!OffsetSidecar::is_usable_without_repair(
            SidecarState::Rebuild
        ));
    }

    #[test]
    fn sidecar_paths_append_rather_than_replace() {
        // `data.varve` and `data` must not collide, which is why the extension
        // is appended and not set.
        assert_eq!(
            sidecar_path(Path::new("data.varve"), None),
            PathBuf::from("data.varve.vix")
        );
        assert_eq!(
            sidecar_path(Path::new("data.varve"), Some(6)),
            PathBuf::from("data.varve.vix.6")
        );
        assert_ne!(
            sidecar_path(Path::new("data.varve"), None),
            sidecar_path(Path::new("data"), None)
        );
    }
}
