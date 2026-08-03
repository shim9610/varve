use std::path::Path;

use crate::{
    Error, MatrixDimensions, Result, VarveFile, VarveReader, VarveWriter, WireType,
    WriterLockBreakPolicy, WriterLockInfo,
};

const RESERVED_BLOCK_ID_START: u32 = 0xFFFF_FF00;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ReadLimit {
    Missing,
    Finite(u64),
    TrustedUnbounded,
}

impl ReadLimit {
    pub const fn meet(self, other: Self) -> Self {
        match (self, other) {
            (Self::Finite(left), Self::Finite(right)) => {
                Self::Finite(if left < right { left } else { right })
            }
            (Self::Finite(value), _) | (_, Self::Finite(value)) => Self::Finite(value),
            (Self::Missing, _) | (_, Self::Missing) => Self::Missing,
            (Self::TrustedUnbounded, Self::TrustedUnbounded) => Self::TrustedUnbounded,
        }
    }

    pub fn require_finite(self, resource: &'static str) -> Result<u64> {
        match self {
            Self::Finite(value) => Ok(value),
            Self::Missing => Err(Error::MissingResourceLimit { resource }),
            Self::TrustedUnbounded => Err(Error::TrustedUnboundedRequiresExplicitApi { resource }),
        }
    }

    pub const fn trusted_ceiling(self) -> Option<u64> {
        match self {
            Self::Finite(value) => Some(value),
            Self::Missing | Self::TrustedUnbounded => None,
        }
    }

    const fn overlay(self, value: Self) -> Self {
        match value {
            Self::Missing => self,
            value => value,
        }
    }

    const fn tighten(self, value: Self) -> Self {
        match value {
            Self::Missing => self,
            value => self.meet(value),
        }
    }
}

/// How much of a matrix's persisted commit metadata a reader keeps resident.
///
/// Nothing about a file changes with this option: it is purely a reader-side
/// residency policy, so the same file can be opened with any bound by any
/// process, and a file written by any version reads under all of them.
///
/// **Residency is always demand-filled and bounded; the only declaration left
/// is the bound.** A commit-map page becomes resident when a bit inside it is
/// addressed and is evicted least-recently-used when admitting another would
/// exceed the declared `cache_bytes`. Memory therefore follows the working set
/// and never the file, which is the only residency model a TB-scale open can
/// have.
///
/// *When* commit metadata is checked is a **separate** policy,
/// [`MatrixMetadataVerification`]. Until 0.5.0 this enum carried an
/// `EagerVerified` variant that loaded every published page at open, and the
/// complete [`crate::MatrixRecoveryReport`] was a side effect of that load —
/// which made "verify the whole commit map" and "keep the whole commit map"
/// look like one decision. They are two, they are now two options, and
/// verification retains nothing (see [`MatrixMetadataVerification::AtOpen`]).
///
/// Which cache an *undeclared* policy resolves to is [`Self::DEFAULT`], whose
/// bound is [`Self::DEFAULT_CACHE_BYTES`] clamped to `max_matrix_bitmap_bytes`.
///
/// `#[non_exhaustive]`, like [`ReadLimit`]: a downstream `match` needs a
/// wildcard arm. Adding [`Self::Missing`] in 0.5.0 already broke every
/// exhaustive match, so the attribute went on in the same release rather than
/// costing a second break later. Match on the variant you care about and let the
/// wildcard carry the rest — or call
/// [`ReadLimits::effective_matrix_metadata_residency`], which never yields
/// [`Self::Missing`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum MatrixMetadataResidency {
    /// "Nobody declared a policy." The same role [`ReadLimit::Missing`] plays
    /// for every ceiling on [`ReadLimits`]: it is the state an unset field
    /// starts in, it never overwrites a policy someone else declared, and it
    /// resolves to varve's own choice —
    /// [`ReadLimits::effective_matrix_metadata_residency`], which resolves it to
    /// [`Self::DEFAULT`] with the cache derived from the ceiling in force.
    ///
    /// Distinguishing it from an explicit [`Self::Lazy`] is what lets
    /// [`ReadLimits::overlay`] compose: a runtime `ReadLimits` that never called
    /// [`ReadLimits::with_matrix_metadata_residency`] leaves a format-declared
    /// policy alone, and one that did wins.
    Missing,
    /// Open reads the persisted page index and nothing else. A commit-map page
    /// is read, authenticated against its stored page digest, and cached the
    /// first time a bit inside it is addressed; the least recently used cached
    /// page is dropped when admitting another would exceed `cache_bytes`.
    ///
    /// Cost: `O(live pages)` *index* bytes read at open — no page payload, no
    /// allocation-map scan — and at most `cache_bytes` resident payload bytes
    /// per demand-loaded bitmap for the session, whatever the file's size or
    /// live page count. Per bitmap, not per reader: each commit category and
    /// each block's validity map owns a cache of its own, so a format declaring
    /// `n` such maps bounds itself at `n * cache_bytes` rather than
    /// `cache_bytes`. The bound is still independent of the file.
    ///
    /// Three consequences are part of the declaration, not accidents:
    ///
    /// * **A page is authenticated when it is faulted in.** A page whose bytes
    ///   disagree with its stored digest is reported as
    ///   [`crate::Error::MatrixFatalCorruption`] by the read that touches it, so
    ///   no read ever answers from unverified bytes. That is a per-page refusal,
    ///   not a verdict on the map: the verdict —
    ///   [`crate::MatrixRecoveryReport`]'s commit-map findings, the category
    ///   quarantine ([`crate::Error::MatrixCommitQuarantined`]), and the
    ///   `RebuildCommitMap` / `ClearCategory` recommendations — comes from
    ///   [`MatrixMetadataVerification`], which is on by default and is
    ///   independent of what this bound retains.
    /// * **A page absent from the persisted index still reads as clear, and
    ///   that is not a guess.** The index is loaded in full at open and is
    ///   authoritative for which pages hold state; "not cached" and "not
    ///   published" are distinct states and only the second answers zero. An
    ///   index that could not be enumerated in full is a fatal finding at open.
    /// * **A page's contents are as of the first touch that faulted it in, not
    ///   as of open.** Pages not yet faulted in have no snapshot pinned, so a
    ///   reader that must see one consistent instant across a whole map has to
    ///   coordinate that itself — no residency bound provides it, because a
    ///   bound that pinned the whole live set would be the eager load this
    ///   design removed.
    Lazy {
        /// Ceiling on the cached commit-map payload, in bytes, per bitmap.
        /// Rounded up to a whole 4096-byte page internally, with a one-page
        /// floor, and refused at open when it exceeds
        /// `max_matrix_bitmap_bytes` — a *declared* cache above a declared
        /// ceiling would be a way to raise that ceiling. The cache varve
        /// derives for [`Self::Missing`], where [`Self::DEFAULT`] is this
        /// variant, is clamped to the ceiling instead
        /// ([`ReadLimits::default_matrix_metadata_cache_bytes`]), so a default is
        /// never the reason an open fails.
        cache_bytes: u64,
    },
}

impl MatrixMetadataResidency {
    /// The policy [`Self::Missing`] resolves to: the one place the default
    /// lives, and the one line that changes it.
    ///
    /// It is [`Self::Lazy`] with [`Self::DEFAULT_CACHE_BYTES`], clamped to
    /// whatever `max_matrix_bitmap_bytes` is in force
    /// ([`ReadLimits::default_matrix_metadata_cache_bytes`]). An open therefore
    /// reads a header and 8 bytes per live page, retains no commit-map payload
    /// at all, and can never be refused because a matrix's live set is wider
    /// than the resident ceiling.
    ///
    /// Until 0.5.0 this was `EagerVerified`, which materialised every published
    /// page for the session — 131,240 bytes over 32 commit-map pages for a
    /// matrix with *one* live page, `O(live pages)` resident for the session,
    /// and `max_matrix_bitmap_bytes` turned into an availability limit that
    /// could leave a matrix unopenable. What kept it was not its cost but that
    /// the commit-map half of [`crate::MatrixRecoveryReport`] was a *side
    /// effect* of that load: the `Recoverable` commit-map finding, the category
    /// quarantine, the `RebuildCommitMap` / `ClearCategory` recommendations and
    /// the strict-recovery writer gate were all produced by pages the demand
    /// path does not read.
    ///
    /// That coupling is gone. Verification is [`MatrixMetadataVerification`],
    /// it defaults to [`MatrixMetadataVerification::AtOpen`], it visits the same
    /// candidate set the eager load visited — the persisted page index *unioned
    /// with* the platform allocation map, whose second term is the only thing
    /// that sees stray bytes in a page nothing ever published — and it retains
    /// one 4096-byte buffer while doing it. So every one of those findings is
    /// still produced at open, and none of them costs residency.
    pub const DEFAULT: Self = Self::Lazy {
        cache_bytes: Self::DEFAULT_CACHE_BYTES,
    };

    /// Cache ceiling the unstated default resolves to, before it is clamped
    /// down to `max_matrix_bitmap_bytes`.
    ///
    /// Derived, not chosen: it is one thirty-second of
    /// `ReadLimits::STANDARD.max_matrix_bitmap_bytes` (64 MiB / 32 = 2 MiB =
    /// 512 commit-map pages). Both halves of that ratio are load-bearing.
    ///
    /// * Small enough to be honest. A cache is a *bound*, and 1/32 of the
    ///   resident ceiling is visibly a working-set bound rather than a
    ///   disguised whole-file load. It does not move when the file does: the
    ///   same 2 MiB bounds a 4 KiB matrix and a 4 TiB one.
    /// * Large enough not to thrash. `ReadLimits::STANDARD` admits at most
    ///   `max_matrix_cells` = 16,000,000 cells, whose commit map is
    ///   16,000,000 / 8 = 1,953,125 bytes — 477 pages. 512 pages therefore
    ///   holds the *entire* commit map of the largest matrix `STANDARD` will
    ///   open, so no ordinary workload evicts a page it is about to need, and
    ///   a matrix wide enough to evict is one whose owner raised
    ///   `max_matrix_cells` deliberately.
    pub const DEFAULT_CACHE_BYTES: u64 = match ReadLimits::STANDARD.max_matrix_bitmap_bytes {
        ReadLimit::Finite(ceiling) => ceiling / 32,
        _ => 2 * 1024 * 1024,
    };

    /// Call-time policy wins, `Missing` means the caller said nothing. Exactly
    /// [`ReadLimit::overlay`]'s rule, which is the point: the two fields
    /// compose the same way, so no call site has to remember that one of them
    /// is special.
    const fn overlay(self, value: Self) -> Self {
        match value {
            Self::Missing => self,
            value => value,
        }
    }

    /// A policy is a declaration, not a ceiling, so there is nothing to meet:
    /// the format's declaration stands and the runtime may only fill in a
    /// silence. (Before the `Missing` state existed this dropped a runtime
    /// declaration even when the format had made none.)
    const fn tighten(self, value: Self) -> Self {
        match self {
            Self::Missing => value,
            declared => declared,
        }
    }
}

/// The unset state is not a policy: [`MatrixMetadataResidency::DEFAULT`] is what
/// resolves it, so it must itself be resolved.
const _: () = assert!(
    !matches!(
        MatrixMetadataResidency::DEFAULT,
        MatrixMetadataResidency::Missing
    ),
    "MatrixMetadataResidency::DEFAULT must name a real policy"
);

/// The "large enough not to thrash" half of [`MatrixMetadataResidency::DEFAULT_CACHE_BYTES`],
/// enforced rather than asserted in prose: raising `max_matrix_cells` or lowering
/// `max_matrix_bitmap_bytes` in `STANDARD` breaks the build instead of silently
/// turning the default cache into a thrashing one.
const _: () = {
    let cells = match ReadLimits::STANDARD.max_matrix_cells {
        ReadLimit::Finite(cells) => cells,
        _ => 0,
    };
    assert!(
        MatrixMetadataResidency::DEFAULT_CACHE_BYTES * 8 >= cells,
        "the default matrix metadata cache no longer holds the commit map of the \
         largest matrix ReadLimits::STANDARD admits"
    );
};

/// **When** a matrix's persisted commit metadata is authenticated as a whole.
///
/// The companion of [`MatrixMetadataResidency`], and separate from it on
/// purpose. Verification reads the persisted page index *unioned with* the
/// platform's allocation map, authenticates every page in that candidate set
/// against its stored digest, and folds the result into
/// [`crate::MatrixRecoveryReport`]. Residency decides what stays in memory
/// afterwards; verification decides whether the check happens at all. Before
/// 0.5.0 one eager load did both, which made the complete recovery report look
/// like something only a whole-file-resident open could produce.
///
/// What verification produces, and nothing else does:
///
/// * the `Recoverable` [`crate::MatrixCorruptionKind::CommitMap`] finding for a
///   category whose map disagrees with its digests;
/// * the quarantine behind [`crate::Error::MatrixCommitQuarantined`], which
///   fails **every** access to that category closed — reader and writer, and so
///   also the [`crate::RecoveryPolicy::Strict`] writer gate — rather than
///   refusing only the pages a reader happens to touch;
/// * the `RebuildCommitMap` / `ClearCategory` recommendations that make the
///   documented recovery path reachable;
/// * detection of damage in a page the persisted index does not name, which is
///   what the allocation-map term of the candidate set exists for.
///
/// What it costs: `O(live pages + allocated pages)` bytes **read**, and one
/// 4096-byte page buffer **retained**, reused across every page and every
/// bitmap. Nothing it reads becomes resident, so it is `O(1)` in memory whatever
/// the cell count, the live-page count, or the file size.
///
/// Per-page authentication is *not* this option: a page faulted in by the demand
/// path is always checked against its digest before a bit of it is reported
/// ([`MatrixMetadataResidency::Lazy`]), under every variant here. That is why
/// there is no `Never`: turning verification off removes the *verdict*, never
/// the check on bytes a read actually answers from.
///
/// `#[non_exhaustive]` for the same reason as [`MatrixMetadataResidency`]: match
/// the variant you care about and let a wildcard carry the rest, or call
/// [`ReadLimits::effective_matrix_metadata_verification`], which never yields
/// [`Self::Missing`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum MatrixMetadataVerification {
    /// "Nobody declared a policy." Exactly the role [`ReadLimit::Missing`] and
    /// [`MatrixMetadataResidency::Missing`] play: it never overwrites a policy
    /// someone else declared, and it resolves — in
    /// [`ReadLimits::effective_matrix_metadata_verification`], and only there —
    /// to [`Self::DEFAULT`].
    Missing,
    /// Verify while opening, so a damaged matrix announces itself at open and a
    /// writer is refused before it can mutate a category whose map is damaged.
    ///
    /// This is [`Self::DEFAULT`]: an undeclared format behaves exactly as it did
    /// before residency and verification were separated, minus the residency.
    AtOpen,
    /// Do not verify while opening; verify when asked, by calling
    /// `verify_matrix_metadata`.
    ///
    /// Open then reads the persisted page index and nothing else, and asks the
    /// filesystem nothing — no allocation-map query either, since the union has
    /// no reader. Two consequences, both deliberate:
    ///
    /// * **The report is silent about commit-map damage until it is asked.** No
    ///   quarantine is armed, so a reader is refused only on the pages it
    ///   touches and a writer is not stopped from mutating a damaged category.
    /// * **`verify_matrix_metadata` reports; it does not retroactively
    ///   quarantine.** The fail-closed gate is derived once, at open, from the
    ///   findings the layout is assembled with, and nothing may install one
    ///   afterwards. A caller that wants the gate reopens with [`Self::AtOpen`].
    ///
    /// Use it where open latency is the scarce resource and the caller owns the
    /// integrity schedule — a scrub pass, or a reader that verifies once per
    /// file rather than once per open.
    OnDemand,
}

impl MatrixMetadataVerification {
    /// The policy [`Self::Missing`] resolves to: the one place the default
    /// lives, and the one line that changes it.
    ///
    /// [`Self::AtOpen`], because the alternative is a silent loss of behaviour.
    /// Making residency lazy is a memory decision a caller cannot observe going
    /// wrong; making verification lazy would silently stop a damaged matrix from
    /// announcing itself and silently un-gate a strict-recovery writer. The
    /// memory cost that used to accompany detection at open is gone — one page
    /// buffer, nothing retained — so the default keeps the behaviour and drops
    /// the cost.
    pub const DEFAULT: Self = Self::AtOpen;

    /// Call-time policy wins, `Missing` means the caller said nothing.
    /// [`MatrixMetadataResidency::overlay`]'s rule, and [`ReadLimit::overlay`]'s
    /// before it: the fields compose the same way, so no call site has to
    /// remember which of them is special.
    const fn overlay(self, value: Self) -> Self {
        match value {
            Self::Missing => self,
            value => value,
        }
    }

    /// A policy is a declaration, not a ceiling, so there is nothing to meet:
    /// the format's declaration stands and the runtime may only fill in a
    /// silence.
    const fn tighten(self, value: Self) -> Self {
        match self {
            Self::Missing => value,
            declared => declared,
        }
    }
}

/// The unset state is not a policy: [`MatrixMetadataVerification::DEFAULT`] is
/// what resolves it, so it must itself be resolved.
const _: () = assert!(
    !matches!(
        MatrixMetadataVerification::DEFAULT,
        MatrixMetadataVerification::Missing
    ),
    "MatrixMetadataVerification::DEFAULT must name a real policy"
);

/// When record integrity is verified.
///
/// Separate from [`IntegrityPolicy`], which says *whether* a checksum exists,
/// because the two answer different questions and conflating them is what made
/// open cost scale with the file. This is the same split `matrix.rs` made in
/// 0.5.0 between residency and verification, for the same defect and with the
/// same standing: **a choice about when damage is announced, not about whether
/// bytes are checked.**
///
/// Every typed read already re-verifies the record it returns —
/// `read_payload_snapshot` re-reads the header, compares all seven fields
/// against the index entry, checks the footer, and recomputes the payload CRC.
/// So verifying every record again while scanning at open is redundant for
/// everything a caller actually reads, and it is what makes open read the whole
/// file instead of its framing.
///
/// Measured on Linux x86_64, cold cache, one 2,052 MB file of 50,000 records:
/// open took 7,096 ms verifying at open and 3,621 ms not verifying at open.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum IntegrityVerification {
    /// "Nobody declared a policy." Never overwrites a declaration, and resolves
    /// to [`Self::DEFAULT`] in [`ReadLimits::effective_integrity_verification`]
    /// and only there.
    Missing,
    /// Verify every record while scanning at open.
    ///
    /// The behaviour before this policy existed. Costs a full read of every
    /// payload in the file, and detects damage in records the caller never
    /// touches.
    AtOpen,
    /// Do not verify while scanning at open. Verify on the read that returns the
    /// record, which already happens, and on an explicit `verify_all`.
    ///
    /// **This is [`Self::DEFAULT`].** What it gives up is the *announcement*, not
    /// the check: damage in a record is reported when that record is read rather
    /// than when the file is opened, and damage in a record that is never read is
    /// never reported unless `verify_all` is called.
    ///
    /// One thing does not follow this policy, deliberately: a **recovery** scan
    /// verifies unconditionally, because a checksum mismatch is the evidence
    /// `RecoveryPolicy::TruncateTail` truncates on. Losing it there would turn a
    /// recoverable tail into a silently accepted one.
    OnDemand,
}

impl IntegrityVerification {
    /// The policy [`Self::Missing`] resolves to, and the one line that changes it.
    ///
    /// [`Self::OnDemand`], because the standing requirement is that open reads a
    /// header and work is bounded by what is used, and because the read path
    /// already performs the identical check on every record it returns. A caller
    /// that wants damage announced up front declares [`Self::AtOpen`] or calls
    /// `verify_all` — both are one line, and neither is silently lost.
    pub const DEFAULT: Self = Self::OnDemand;

    /// A runtime declaration replaces the format's; a runtime silence does not.
    /// Same rule as `MatrixMetadataVerification::overlay`.
    const fn overlay(self, value: Self) -> Self {
        match value {
            Self::Missing => self,
            value => value,
        }
    }

    /// A declaration wins over the unset state, and only over that. Same
    /// composition rule as `MatrixMetadataVerification::tighten`, so a runtime
    /// `ReadLimits` cannot silently drop a policy the format declared.
    const fn tighten(self, value: Self) -> Self {
        match self {
            Self::Missing => value,
            declared => declared,
        }
    }
}

/// The unset state is not a policy: [`IntegrityVerification::DEFAULT`] is what
/// resolves it, so it must itself be resolved.
const _: () = assert!(
    !matches!(
        IntegrityVerification::DEFAULT,
        IntegrityVerification::Missing
    ),
    "IntegrityVerification::DEFAULT must be a resolved policy"
);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct ReadLimits {
    pub max_file_len: ReadLimit,
    pub max_records: ReadLimit,
    pub max_index_bytes: ReadLimit,
    pub max_scan_bytes: ReadLimit,
    pub max_record_payload_len: ReadLimit,
    pub max_logical_payload_len: ReadLimit,
    pub max_materialized_bytes: ReadLimit,
    pub max_segments: ReadLimit,
    pub max_matrix_dimension: ReadLimit,
    pub max_matrix_cells: ReadLimit,
    pub max_matrix_bitmap_bytes: ReadLimit,
    pub max_matrix_crc_bytes: ReadLimit,
    pub max_matrix_metadata_bytes: ReadLimit,
    pub max_matrix_slot_region_len: ReadLimit,
    pub max_sidecar_len: ReadLimit,
    pub max_mmap_len: ReadLimit,
    /// Ceiling on the resident keyed-tail cache of a single keyed block id -
    /// the map that lets a keyed append resolve its predecessor in O(1)
    /// instead of rescanning the resident index (API3-02).
    ///
    /// The charged value is the map's inline storage plus the key payload
    /// bytes it owns, checked before the memory is taken. It is checked per
    /// keyed block id, not summed across block ids.
    pub max_keyed_tail_bytes: ReadLimit,
    /// Declared matrix commit-metadata residency policy.
    ///
    /// [`MatrixMetadataResidency::Missing`] unless somebody declared one, which
    /// resolves to [`MatrixMetadataResidency::DEFAULT`] — see
    /// [`Self::effective_matrix_metadata_residency`]. It lives on `ReadLimits`
    /// rather than on `FormatSpec` because it decides *how*
    /// `max_matrix_bitmap_bytes` is enforced — as an admission limit on the
    /// whole live set, or as a bound on a demand-filled cache — and because it
    /// is a property of one reader's open, not of the file's format.
    pub matrix_metadata_residency: MatrixMetadataResidency,
    /// Declared matrix commit-metadata verification policy.
    ///
    /// [`MatrixMetadataVerification::Missing`] unless somebody declared one,
    /// which resolves to [`MatrixMetadataVerification::DEFAULT`] — see
    /// [`Self::effective_matrix_metadata_verification`]. It sits beside the
    /// residency policy rather than inside it because the two are independent:
    /// verification decides whether the whole commit map is authenticated and
    /// reported on, residency decides what is kept in memory afterwards, and
    /// every combination of the two is meaningful.
    pub matrix_metadata_verification: MatrixMetadataVerification,
    /// Declared record-integrity verification policy; see
    /// [`IntegrityVerification`]. Like the matrix policies it lives on
    /// `ReadLimits` rather than on `FormatSpec`, because it is a property of
    /// one open and not of the file: it changes no byte, so it is neither
    /// hashed nor persisted.
    pub integrity_verification: IntegrityVerification,
    trusted_api: bool,
}

macro_rules! read_limit_setters {
    ($(($method:ident, $field:ident)),+ $(,)?) => {
        $(
            pub const fn $method(mut self, value: u64) -> Self {
                self.$field = ReadLimit::Finite(value);
                self
            }
        )+
    };
}

impl ReadLimits {
    pub const MISSING: Self = Self::all(ReadLimit::Missing);
    pub const TRUSTED_UNBOUNDED: Self = Self::all(ReadLimit::TrustedUnbounded);
    pub const STANDARD: Self = Self {
        max_file_len: ReadLimit::Finite(u64::MAX),
        max_records: ReadLimit::Finite(u64::MAX),
        max_index_bytes: ReadLimit::Finite(u64::MAX),
        max_scan_bytes: ReadLimit::Finite(u64::MAX),
        max_record_payload_len: ReadLimit::Finite(64 * 1024 * 1024),
        max_logical_payload_len: ReadLimit::Finite(256 * 1024 * 1024),
        max_materialized_bytes: ReadLimit::Finite(1024 * 1024 * 1024),
        max_segments: ReadLimit::Finite(u64::MAX),
        max_matrix_dimension: ReadLimit::Finite(16_000_000),
        max_matrix_cells: ReadLimit::Finite(16_000_000),
        max_matrix_bitmap_bytes: ReadLimit::Finite(64 * 1024 * 1024),
        max_matrix_crc_bytes: ReadLimit::Finite(128 * 1024 * 1024),
        max_matrix_metadata_bytes: ReadLimit::Finite(256 * 1024 * 1024),
        max_matrix_slot_region_len: ReadLimit::Finite(8 * 1024 * 1024 * 1024),
        max_sidecar_len: ReadLimit::Finite(256 * 1024 * 1024),
        max_mmap_len: ReadLimit::Finite(8 * 1024 * 1024 * 1024),
        max_keyed_tail_bytes: ReadLimit::Finite(u64::MAX),
        // Deliberately `Missing` rather than a concrete policy: `STANDARD` is
        // what `resolve` overlays onto, so a concrete value here would be
        // indistinguishable from a caller's declaration and could not be
        // clamped to whatever `max_matrix_bitmap_bytes` ends up being.
        matrix_metadata_residency: MatrixMetadataResidency::Missing,
        // `Missing` for the same reason: a preset that declared a verification
        // policy would be indistinguishable from a caller who declared one, and
        // would win over a format's declaration through `overlay`.
        matrix_metadata_verification: MatrixMetadataVerification::Missing,
        integrity_verification: IntegrityVerification::Missing,
        trusted_api: false,
    };
    /// Finite companion to [`Self::STANDARD`] for input from untrusted
    /// sources. Every aggregate dimension that `STANDARD` leaves effectively
    /// unbounded (total file length, record count, scan bytes, resident index
    /// bytes, segment count) is finite here, so a hostile file cannot choose
    /// the reader's CPU, I/O, or memory. This is the recommended default when
    /// opening files from untrusted sources with the resident API; large
    /// trusted files should use the scalable APIs or explicit wider limits.
    pub const UNTRUSTED: Self = Self {
        max_file_len: ReadLimit::Finite(16 * 1024 * 1024 * 1024),
        max_records: ReadLimit::Finite(16_000_000),
        max_index_bytes: ReadLimit::Finite(1024 * 1024 * 1024),
        max_scan_bytes: ReadLimit::Finite(16 * 1024 * 1024 * 1024),
        max_segments: ReadLimit::Finite(65_536),
        max_keyed_tail_bytes: ReadLimit::Finite(256 * 1024 * 1024),
        ..Self::STANDARD
    };

    const fn all(value: ReadLimit) -> Self {
        Self {
            max_file_len: value,
            max_records: value,
            max_index_bytes: value,
            max_scan_bytes: value,
            max_record_payload_len: value,
            max_logical_payload_len: value,
            max_materialized_bytes: value,
            max_segments: value,
            max_matrix_dimension: value,
            max_matrix_cells: value,
            max_matrix_bitmap_bytes: value,
            max_matrix_crc_bytes: value,
            max_matrix_metadata_bytes: value,
            max_matrix_slot_region_len: value,
            max_sidecar_len: value,
            max_mmap_len: value,
            max_keyed_tail_bytes: value,
            // Every field `all` builds is the unset one, this included: `MISSING`
            // and `TRUSTED_UNBOUNDED` declare no residency and no verification
            // policy, exactly as they declare no ceilings.
            matrix_metadata_residency: MatrixMetadataResidency::Missing,
            matrix_metadata_verification: MatrixMetadataVerification::Missing,
            integrity_verification: IntegrityVerification::Missing,
            trusted_api: false,
        }
    }

    /// Declares the matrix commit-metadata residency policy (see
    /// [`MatrixMetadataResidency`]). Leaving it unset resolves to
    /// [`MatrixMetadataResidency::DEFAULT`]; pass
    /// [`MatrixMetadataResidency::Lazy`] for an open bounded by the working set
    /// rather than by the file.
    pub const fn with_matrix_metadata_residency(
        mut self,
        residency: MatrixMetadataResidency,
    ) -> Self {
        self.matrix_metadata_residency = residency;
        self
    }

    /// Declares the matrix commit-metadata verification policy (see
    /// [`MatrixMetadataVerification`]). Leaving it unset resolves to
    /// [`MatrixMetadataVerification::DEFAULT`], which verifies at open; pass
    /// [`MatrixMetadataVerification::OnDemand`] to move that work to an explicit
    /// `verify_matrix_metadata` call.
    pub const fn with_matrix_metadata_verification(
        mut self,
        verification: MatrixMetadataVerification,
    ) -> Self {
        self.matrix_metadata_verification = verification;
        self
    }

    /// The residency policy an open actually uses.
    ///
    /// [`MatrixMetadataResidency::Missing`] — nobody declared one — resolves
    /// here, and only here, to [`MatrixMetadataResidency::DEFAULT`], taking the
    /// cache [`Self::default_matrix_metadata_cache_bytes`] derives. A declared
    /// policy is returned unchanged, cache and all.
    pub const fn effective_matrix_metadata_residency(self) -> MatrixMetadataResidency {
        match self.matrix_metadata_residency {
            MatrixMetadataResidency::Missing => match MatrixMetadataResidency::DEFAULT {
                // A default's cache is derived from the ceiling in force, never
                // a literal, so it cannot be the reason an open fails.
                MatrixMetadataResidency::Lazy { .. } => MatrixMetadataResidency::Lazy {
                    cache_bytes: self.default_matrix_metadata_cache_bytes(),
                },
                default => default,
            },
            declared => declared,
        }
    }

    /// The verification policy an open actually uses.
    ///
    /// [`MatrixMetadataVerification::Missing`] resolves here, and only here, to
    /// [`MatrixMetadataVerification::DEFAULT`]. A declared policy is returned
    /// unchanged.
    /// The verification policy in force: the declared one, or
    /// [`IntegrityVerification::DEFAULT`] when nobody declared one.
    pub const fn effective_integrity_verification(self) -> IntegrityVerification {
        match self.integrity_verification {
            IntegrityVerification::Missing => IntegrityVerification::DEFAULT,
            declared => declared,
        }
    }

    /// Declares when record integrity is verified. See [`IntegrityVerification`].
    pub const fn with_integrity_verification(mut self, policy: IntegrityVerification) -> Self {
        self.integrity_verification = policy;
        self
    }

    pub const fn effective_matrix_metadata_verification(self) -> MatrixMetadataVerification {
        match self.matrix_metadata_verification {
            MatrixMetadataVerification::Missing => MatrixMetadataVerification::DEFAULT,
            declared => declared,
        }
    }

    /// The cache a lazy default would use when nobody declared one:
    /// [`MatrixMetadataResidency::DEFAULT_CACHE_BYTES`], clamped down to
    /// `max_matrix_bitmap_bytes`.
    ///
    /// The clamp is the whole difference between a default and a declaration. A
    /// caller who writes `with_max_matrix_bitmap_bytes(64 * 1024)` is tightening
    /// a memory ceiling, not asking for every matrix open to fail; a default
    /// cache above that ceiling therefore becomes a cache *at* it, down to the
    /// one-page floor the page store applies. A cache the caller declared
    /// themselves is refused instead, because that is a contradiction only they
    /// can resolve.
    pub const fn default_matrix_metadata_cache_bytes(self) -> u64 {
        let default = MatrixMetadataResidency::DEFAULT_CACHE_BYTES;
        match self.max_matrix_bitmap_bytes {
            ReadLimit::Finite(ceiling) => {
                if ceiling < default {
                    ceiling
                } else {
                    default
                }
            }
            ReadLimit::Missing | ReadLimit::TrustedUnbounded => default,
        }
    }

    /// Resolves the residency policy for one open and admits its cache.
    ///
    /// The two ways of arriving at a cache differ on purpose: a *declared* one is
    /// checked against `max_matrix_bitmap_bytes` and refuses the open when it
    /// exceeds it, while the *derived* default is clamped by
    /// [`Self::default_matrix_metadata_cache_bytes`] and can never be the reason
    /// an open fails.
    pub(crate) fn admit_matrix_metadata_residency(self) -> Result<u64> {
        match self.effective_matrix_metadata_residency() {
            MatrixMetadataResidency::Lazy { cache_bytes } => {
                if let MatrixMetadataResidency::Lazy { .. } = self.matrix_metadata_residency {
                    self.check(ReadLimitKey::MatrixBitmapBytes, cache_bytes)?;
                }
                Ok(cache_bytes)
            }
            // `Missing` cannot survive `effective_matrix_metadata_residency`,
            // which is the single resolution point for the unset state; a
            // default that named it is rejected at compile time. Residency has
            // no other shape: the eager variant was removed in 0.5.0 because
            // what callers wanted from it — verification — became its own
            // policy, and keeping it would have been a second residency
            // implementation with no reason to exist.
            MatrixMetadataResidency::Missing => Ok(self.default_matrix_metadata_cache_bytes()),
        }
    }

    /// Resolves the verification policy for one open: `true` when the streaming
    /// verification pass runs while opening.
    ///
    /// The single admission point, matching
    /// [`Self::admit_matrix_metadata_residency`]. There is nothing to charge — a
    /// pass retains one 4096-byte page buffer, which is the one-page floor the
    /// demand cache already has — so this admits rather than checks, and states
    /// that fact in one place instead of at each call site.
    pub(crate) fn admit_matrix_metadata_verification(self) -> bool {
        match self.effective_matrix_metadata_verification() {
            MatrixMetadataVerification::AtOpen => true,
            MatrixMetadataVerification::OnDemand => false,
            // Unreachable through `effective_…`, which is the single resolution
            // point; a default that named it is rejected at compile time. Fail
            // *towards* verification, because the cost of a needless pass is
            // bytes read and the cost of a missed one is silence.
            MatrixMetadataVerification::Missing => true,
        }
    }

    pub const fn missing() -> Self {
        Self::MISSING
    }

    pub const fn trusted_unbounded() -> Self {
        Self::TRUSTED_UNBOUNDED
    }

    pub const fn finite_all(value: u64) -> Self {
        Self::all(ReadLimit::Finite(value))
    }

    pub const fn standard() -> Self {
        Self::STANDARD
    }

    pub const fn untrusted() -> Self {
        Self::UNTRUSTED
    }

    read_limit_setters! {
        (with_max_file_len, max_file_len),
        (with_max_records, max_records),
        (with_max_index_bytes, max_index_bytes),
        (with_max_scan_bytes, max_scan_bytes),
        (with_max_record_payload_len, max_record_payload_len),
        (with_max_logical_payload_len, max_logical_payload_len),
        (with_max_materialized_bytes, max_materialized_bytes),
        (with_max_segments, max_segments),
        (with_max_matrix_dimension, max_matrix_dimension),
        (with_max_matrix_cells, max_matrix_cells),
        (with_max_matrix_bitmap_bytes, max_matrix_bitmap_bytes),
        (with_max_matrix_crc_bytes, max_matrix_crc_bytes),
        (with_max_matrix_metadata_bytes, max_matrix_metadata_bytes),
        (with_max_matrix_slot_region_len, max_matrix_slot_region_len),
        (with_max_sidecar_len, max_sidecar_len),
        (with_max_mmap_len, max_mmap_len),
        (with_max_keyed_tail_bytes, max_keyed_tail_bytes),
    }

    pub const fn tighten(self, runtime: Self) -> Self {
        Self {
            // Not a limit: a residency policy is a declaration, not a ceiling,
            // so there is nothing to meet. The format's declaration stands; a
            // runtime one is taken only where the format made none.
            matrix_metadata_residency: self
                .matrix_metadata_residency
                .tighten(runtime.matrix_metadata_residency),
            // Composes identically, for the identical reason.
            matrix_metadata_verification: self
                .matrix_metadata_verification
                .tighten(runtime.matrix_metadata_verification),
            integrity_verification: self
                .integrity_verification
                .tighten(runtime.integrity_verification),
            max_file_len: self.max_file_len.tighten(runtime.max_file_len),
            max_records: self.max_records.tighten(runtime.max_records),
            max_index_bytes: self.max_index_bytes.tighten(runtime.max_index_bytes),
            max_scan_bytes: self.max_scan_bytes.tighten(runtime.max_scan_bytes),
            max_record_payload_len: self
                .max_record_payload_len
                .tighten(runtime.max_record_payload_len),
            max_logical_payload_len: self
                .max_logical_payload_len
                .tighten(runtime.max_logical_payload_len),
            max_materialized_bytes: self
                .max_materialized_bytes
                .tighten(runtime.max_materialized_bytes),
            max_segments: self.max_segments.tighten(runtime.max_segments),
            max_matrix_dimension: self
                .max_matrix_dimension
                .tighten(runtime.max_matrix_dimension),
            max_matrix_cells: self.max_matrix_cells.tighten(runtime.max_matrix_cells),
            max_matrix_bitmap_bytes: self
                .max_matrix_bitmap_bytes
                .tighten(runtime.max_matrix_bitmap_bytes),
            max_matrix_crc_bytes: self
                .max_matrix_crc_bytes
                .tighten(runtime.max_matrix_crc_bytes),
            max_matrix_metadata_bytes: self
                .max_matrix_metadata_bytes
                .tighten(runtime.max_matrix_metadata_bytes),
            max_matrix_slot_region_len: self
                .max_matrix_slot_region_len
                .tighten(runtime.max_matrix_slot_region_len),
            max_sidecar_len: self.max_sidecar_len.tighten(runtime.max_sidecar_len),
            max_mmap_len: self.max_mmap_len.tighten(runtime.max_mmap_len),
            max_keyed_tail_bytes: self
                .max_keyed_tail_bytes
                .tighten(runtime.max_keyed_tail_bytes),
            trusted_api: false,
        }
    }

    /// Overlays explicitly supplied fields without treating the existing
    /// values as permanent format ceilings.
    pub const fn overlay(self, runtime: Self) -> Self {
        Self {
            // Composes like every ceiling above it: a runtime `ReadLimits` that
            // never called `with_matrix_metadata_residency` carries `Missing`
            // and leaves a format-declared policy alone. Taking the runtime
            // value unconditionally is what made
            // `open_reader_with_resource_limits(path, STANDARD.with_max_…())`
            // revert a spec-declared policy to the eager one.
            matrix_metadata_residency: self
                .matrix_metadata_residency
                .overlay(runtime.matrix_metadata_residency),
            // Composes identically: a runtime `ReadLimits` that never called
            // `with_matrix_metadata_verification` must not silently switch a
            // format that declared `OnDemand` back to verifying at open.
            matrix_metadata_verification: self
                .matrix_metadata_verification
                .overlay(runtime.matrix_metadata_verification),
            integrity_verification: self
                .integrity_verification
                .overlay(runtime.integrity_verification),
            max_file_len: self.max_file_len.overlay(runtime.max_file_len),
            max_records: self.max_records.overlay(runtime.max_records),
            max_index_bytes: self.max_index_bytes.overlay(runtime.max_index_bytes),
            max_scan_bytes: self.max_scan_bytes.overlay(runtime.max_scan_bytes),
            max_record_payload_len: self
                .max_record_payload_len
                .overlay(runtime.max_record_payload_len),
            max_logical_payload_len: self
                .max_logical_payload_len
                .overlay(runtime.max_logical_payload_len),
            max_materialized_bytes: self
                .max_materialized_bytes
                .overlay(runtime.max_materialized_bytes),
            max_segments: self.max_segments.overlay(runtime.max_segments),
            max_matrix_dimension: self
                .max_matrix_dimension
                .overlay(runtime.max_matrix_dimension),
            max_matrix_cells: self.max_matrix_cells.overlay(runtime.max_matrix_cells),
            max_matrix_bitmap_bytes: self
                .max_matrix_bitmap_bytes
                .overlay(runtime.max_matrix_bitmap_bytes),
            max_matrix_crc_bytes: self
                .max_matrix_crc_bytes
                .overlay(runtime.max_matrix_crc_bytes),
            max_matrix_metadata_bytes: self
                .max_matrix_metadata_bytes
                .overlay(runtime.max_matrix_metadata_bytes),
            max_matrix_slot_region_len: self
                .max_matrix_slot_region_len
                .overlay(runtime.max_matrix_slot_region_len),
            max_sidecar_len: self.max_sidecar_len.overlay(runtime.max_sidecar_len),
            max_mmap_len: self.max_mmap_len.overlay(runtime.max_mmap_len),
            max_keyed_tail_bytes: self
                .max_keyed_tail_bytes
                .overlay(runtime.max_keyed_tail_bytes),
            trusted_api: false,
        }
    }

    pub const fn resolve(self) -> Self {
        Self::STANDARD.overlay(self)
    }

    pub(crate) const fn authorize_trusted_api(mut self) -> Self {
        self.trusted_api = true;
        self
    }

    const fn clear_trusted_api(mut self) -> Self {
        self.trusted_api = false;
        self
    }

    pub(crate) fn require(self, key: ReadLimitKey) -> Result<Option<u64>> {
        match key.value(self) {
            ReadLimit::Finite(value) => Ok(Some(value)),
            ReadLimit::Missing if self.trusted_api => Ok(None),
            ReadLimit::TrustedUnbounded if self.trusted_api => Ok(None),
            ReadLimit::Missing => Err(Error::MissingResourceLimit {
                resource: key.resource(),
            }),
            ReadLimit::TrustedUnbounded => Err(Error::TrustedUnboundedRequiresExplicitApi {
                resource: key.resource(),
            }),
        }
    }

    pub(crate) fn check(self, key: ReadLimitKey, actual: u64) -> Result<()> {
        if let Some(limit) = self.require(key)?
            && actual > limit
        {
            return Err(Error::LimitExceeded {
                resource: key.resource(),
                actual,
                limit,
            });
        }
        Ok(())
    }
}

impl Default for ReadLimits {
    fn default() -> Self {
        Self::MISSING
    }
}

/// Runtime resource policy for readers and writers.
///
/// `ReadLimits` remains the canonical name for source compatibility.
pub type ResourceLimits = ReadLimits;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ReadLimitKey {
    FileLen,
    Records,
    IndexBytes,
    ScanBytes,
    RecordPayloadLen,
    LogicalPayloadLen,
    MaterializedBytes,
    Segments,
    MatrixDimension,
    MatrixCells,
    MatrixBitmapBytes,
    MatrixCrcBytes,
    MatrixMetadataBytes,
    MatrixSlotRegionLen,
    SidecarLen,
    KeyedTailBytes,
    #[cfg(feature = "mmap")]
    MmapLen,
}

impl ReadLimitKey {
    pub(crate) const fn resource(self) -> &'static str {
        match self {
            Self::FileLen => "file length",
            Self::Records => "record count",
            Self::IndexBytes => "index bytes",
            Self::ScanBytes => "scan bytes",
            Self::RecordPayloadLen => "record payload length",
            Self::LogicalPayloadLen => "logical payload length",
            Self::MaterializedBytes => "materialized bytes",
            Self::Segments => "segment count",
            Self::MatrixDimension => "matrix dimension",
            Self::MatrixCells => "matrix cells",
            Self::MatrixBitmapBytes => "matrix bitmap bytes",
            Self::MatrixCrcBytes => "matrix checksum bytes",
            Self::MatrixMetadataBytes => "matrix metadata bytes",
            Self::MatrixSlotRegionLen => "matrix slot region length",
            Self::SidecarLen => "sidecar length",
            Self::KeyedTailBytes => "keyed tail bytes",
            #[cfg(feature = "mmap")]
            Self::MmapLen => "mmap length",
        }
    }

    const fn value(self, limits: ReadLimits) -> ReadLimit {
        match self {
            Self::FileLen => limits.max_file_len,
            Self::Records => limits.max_records,
            Self::IndexBytes => limits.max_index_bytes,
            Self::ScanBytes => limits.max_scan_bytes,
            Self::RecordPayloadLen => limits.max_record_payload_len,
            Self::LogicalPayloadLen => limits.max_logical_payload_len,
            Self::MaterializedBytes => limits.max_materialized_bytes,
            Self::Segments => limits.max_segments,
            Self::MatrixDimension => limits.max_matrix_dimension,
            Self::MatrixCells => limits.max_matrix_cells,
            Self::MatrixBitmapBytes => limits.max_matrix_bitmap_bytes,
            Self::MatrixCrcBytes => limits.max_matrix_crc_bytes,
            Self::MatrixMetadataBytes => limits.max_matrix_metadata_bytes,
            Self::MatrixSlotRegionLen => limits.max_matrix_slot_region_len,
            Self::SidecarLen => limits.max_sidecar_len,
            Self::KeyedTailBytes => limits.max_keyed_tail_bytes,
            #[cfg(feature = "mmap")]
            Self::MmapLen => limits.max_mmap_len,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Endian {
    Little,
    Big,
}

impl Endian {
    pub const fn to_byte(self) -> u8 {
        match self {
            Self::Little => 1,
            Self::Big => 2,
        }
    }

    pub const fn from_byte(value: u8) -> Option<Self> {
        match value {
            1 => Some(Self::Little),
            2 => Some(Self::Big),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BlockKind {
    Fixed,
    Variable,
    Matrix,
    Internal,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MatrixDimensionDescriptor {
    pub name: &'static str,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MatrixCommitKind {
    Cell,
    Single,
    PerChannel,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MatrixCommitDescriptor {
    pub name: &'static str,
    pub kind: MatrixCommitKind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MatrixBlockDescriptor {
    pub block_id: u32,
    pub dimensions: [&'static str; 2],
    pub category: &'static str,
    pub slot_stride: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MatrixAuxDescriptor {
    pub name: &'static str,
    pub byte_len: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IntegrityPolicy {
    None,
    Crc32,
    Crc32WithHeader,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IndexPolicy {
    pub scan_on_open: bool,
    pub checkpoint_on_flush: bool,
    pub block_offset_chain: bool,
    pub keyed_offset_chain: bool,
    /// Whether a commit point appends an internal *segment* record.
    ///
    /// A segment is varve's own lookup unit, not a unit the declaration names:
    /// one segment covers exactly the records a single commit point added, and
    /// its record's footer chains back to the previous segment through
    /// `prev_same_block_offset`. Open follows that chain backwards from the
    /// last record footer and never reads a data record.
    ///
    /// Deliberately absent from the layout DSL. Blocks are what a user
    /// declares; segment granularity is varve's decision, so there is nothing
    /// here for a declaration to choose.
    pub segment_on_flush: bool,
}

#[allow(non_upper_case_globals)]
impl IndexPolicy {
    pub const ScanOnOpen: Self = Self {
        scan_on_open: true,
        checkpoint_on_flush: false,
        block_offset_chain: false,
        keyed_offset_chain: false,
        segment_on_flush: false,
    };

    pub const CheckpointOnFlush: Self = Self {
        scan_on_open: true,
        checkpoint_on_flush: true,
        block_offset_chain: false,
        keyed_offset_chain: false,
        segment_on_flush: false,
    };

    pub const BlockOffsetChain: Self = Self {
        scan_on_open: true,
        checkpoint_on_flush: false,
        block_offset_chain: true,
        keyed_offset_chain: false,
        segment_on_flush: false,
    };

    pub const KeyedOffsetChain: Self = Self {
        scan_on_open: true,
        checkpoint_on_flush: false,
        block_offset_chain: true,
        keyed_offset_chain: true,
        segment_on_flush: false,
    };

    /// Segment-chained open, built on the block offset chain it needs.
    pub const SegmentOnFlush: Self = Self {
        scan_on_open: true,
        checkpoint_on_flush: false,
        block_offset_chain: true,
        keyed_offset_chain: false,
        segment_on_flush: true,
    };

    pub const fn new(
        scan_on_open: bool,
        checkpoint_on_flush: bool,
        block_offset_chain: bool,
        keyed_offset_chain: bool,
    ) -> Self {
        Self {
            scan_on_open,
            checkpoint_on_flush,
            block_offset_chain,
            keyed_offset_chain,
            segment_on_flush: false,
        }
    }

    pub const fn with_scan_on_open(mut self, enabled: bool) -> Self {
        self.scan_on_open = enabled;
        self
    }

    pub const fn with_checkpoint_on_flush(mut self, enabled: bool) -> Self {
        self.checkpoint_on_flush = enabled;
        if enabled {
            self.scan_on_open = true;
        }
        self
    }

    pub const fn with_block_offset_chain(mut self, enabled: bool) -> Self {
        self.block_offset_chain = enabled;
        if enabled {
            self.scan_on_open = true;
        }
        self
    }

    pub const fn with_keyed_offset_chain(mut self, enabled: bool) -> Self {
        self.keyed_offset_chain = enabled;
        if enabled {
            self.scan_on_open = true;
            self.block_offset_chain = true;
        }
        self
    }

    /// Enables the internal segment chain.
    ///
    /// The chain *is* `prev_same_block_offset` in the record footer, so this
    /// turns the block offset chain on with it; there is no segment chain
    /// without a footer to carry it.
    pub const fn with_segment_on_flush(mut self, enabled: bool) -> Self {
        self.segment_on_flush = enabled;
        if enabled {
            self.scan_on_open = true;
            self.block_offset_chain = true;
        }
        self
    }

    pub const fn requires_record_footer(self) -> bool {
        self.block_offset_chain || self.keyed_offset_chain || self.segment_on_flush
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TransactionMarkerMode {
    OnFlush,
    Explicit,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CommitPolicy {
    None,
    RecordFooter,
    TransactionMarker(TransactionMarkerMode),
}

impl CommitPolicy {
    pub const fn requires_record_footer(self) -> bool {
        !matches!(self, Self::None)
    }

    pub const fn marker_on_flush(self) -> bool {
        matches!(
            self,
            Self::TransactionMarker(TransactionMarkerMode::OnFlush)
        )
    }

    pub const fn is_transaction_marker(self) -> bool {
        matches!(self, Self::TransactionMarker(_))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecoveryPolicy {
    Strict,
    TruncateTail,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ManifestPolicy {
    None,
    Embedded,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CompressionAlgorithm {
    Zstd,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CompressionLevel {
    Fast,
    Default,
    Best,
    Exact(i32),
}

impl CompressionLevel {
    pub const fn to_zstd_level(self) -> i32 {
        match self {
            Self::Fast => 1,
            Self::Default => 3,
            Self::Best => 19,
            Self::Exact(level) => level,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CompressionHeaderMode {
    RecordExplicit,
    FileExplicit,
    FormatContract,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VariableCompression {
    pub algorithm: CompressionAlgorithm,
    pub level: CompressionLevel,
    pub header_mode: CompressionHeaderMode,
    pub min_uncompressed_len: u64,
    pub only_if_smaller: bool,
    pub max_uncompressed_len: u64,
}

impl VariableCompression {
    pub const DEFAULT_MAX_UNCOMPRESSED_LEN: u64 = 64 * 1024 * 1024;

    pub const fn zstd(header_mode: CompressionHeaderMode) -> Self {
        Self {
            algorithm: CompressionAlgorithm::Zstd,
            level: CompressionLevel::Default,
            header_mode,
            min_uncompressed_len: 0,
            only_if_smaller: true,
            max_uncompressed_len: Self::DEFAULT_MAX_UNCOMPRESSED_LEN,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CompressionPolicy {
    None,
    VariableBlocks(VariableCompression),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BlockCompressionDescriptor {
    pub block_id: u32,
    pub compression: VariableCompression,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FieldPresence {
    Required,
    Defaulted,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FieldDescriptor {
    pub id: u32,
    pub name: &'static str,
    pub wire_type: WireType,
    pub presence: FieldPresence,
}

#[derive(Clone, Copy, Debug)]
pub struct BlockDescriptor {
    pub id: u32,
    pub name: &'static str,
    pub version: u16,
    pub kind: BlockKind,
    pub fields: &'static [FieldDescriptor],
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LayoutPreset {
    VarveNative,
    None,
    Custom,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SegmentRepeat {
    Once,
    UntilEof,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LayoutPartKind {
    FileHeader(FileHeaderDescriptor),
    Segment(SegmentDescriptor),
    LeadIn(LeadInDescriptor),
    Metadata(MetadataDescriptor),
    RawRegion(RawRegionDescriptor),
    Footer(FooterDescriptor),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LayoutPartDescriptor {
    pub name: &'static str,
    pub kind: LayoutPartKind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FileHeaderDescriptor {
    pub name: &'static str,
    pub fields: &'static [LayoutFieldDescriptor],
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SegmentDescriptor {
    pub name: &'static str,
    pub repeat: SegmentRepeat,
    pub lead_in: LeadInDescriptor,
    pub metadata: MetadataDescriptor,
    pub raw_region: RawRegionDescriptor,
    pub footer: Option<FooterDescriptor>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LeadInDescriptor {
    pub name: &'static str,
    pub fields: &'static [LayoutFieldDescriptor],
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MetadataDescriptor {
    pub name: &'static str,
    pub source: LayoutBytesSource,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RawRegionDescriptor {
    pub name: &'static str,
    pub source: LayoutBytesSource,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FooterDescriptor {
    pub name: &'static str,
    pub fields: &'static [LayoutFieldDescriptor],
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LayoutBytesSource {
    Caller,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LayoutFieldDescriptor {
    pub name: &'static str,
    pub ty: LayoutFieldType,
    pub source: LayoutFieldSource,
    pub endian: Option<Endian>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LayoutFieldType {
    Bytes { len: u64 },
    U8,
    U16,
    U32,
    U64,
    I64,
}

impl LayoutFieldType {
    pub const fn byte_len(self) -> u64 {
        match self {
            Self::Bytes { len } => len,
            Self::U8 => 1,
            Self::U16 => 2,
            Self::U32 => 4,
            Self::U64 | Self::I64 => 8,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LayoutFieldSource {
    LiteralBytes(&'static [u8]),
    LiteralU64(u64),
    LiteralI64(i64),
    Caller,
    Finalize(LayoutFinalize),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LayoutFinalize {
    pub target: LayoutAnchor,
    pub relative_to: LayoutAnchor,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LayoutAnchor {
    SegmentStart,
    AfterLeadIn,
    MetadataStart,
    RawRegionStart,
    SegmentEnd,
    FooterStart,
    FooterEnd,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LayoutSpec {
    pub preset: LayoutPreset,
    pub parts: &'static [LayoutPartDescriptor],
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LayoutPlan {
    pub preset: LayoutPreset,
    pub parts: Vec<LayoutPlanPartDescriptor>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LayoutPlanPartDescriptor {
    pub name: String,
    pub kind: LayoutPlanPartKind,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LayoutPlanPartKind {
    FileHeader(LayoutPlanFieldGroup),
    Segment(LayoutPlanSegment),
    LeadIn(LayoutPlanFieldGroup),
    Metadata(LayoutPlanRegion),
    RawRegion(LayoutPlanRegion),
    Footer(LayoutPlanFieldGroup),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LayoutPlanSegment {
    pub name: String,
    pub repeat: SegmentRepeat,
    pub lead_in: LayoutPlanFieldGroup,
    pub metadata: LayoutPlanRegion,
    pub raw_region: LayoutPlanRegion,
    pub footer: Option<LayoutPlanFieldGroup>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LayoutPlanFieldGroup {
    pub name: String,
    pub fields: Vec<LayoutPlanField>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LayoutPlanRegion {
    pub name: String,
    pub source: LayoutPlanRegionSource,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LayoutPlanRegionSource {
    Caller,
    Native(&'static str),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LayoutPlanField {
    pub name: String,
    pub ty: LayoutPlanFieldType,
    pub source: LayoutPlanFieldSource,
    pub endian: Option<Endian>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LayoutPlanFieldType {
    Bytes { len: LayoutPlanLen },
    U8,
    U16,
    U32,
    U64,
    I64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LayoutPlanLen {
    Fixed(u64),
    Dynamic,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LayoutPlanFieldSource {
    LiteralBytes(Vec<u8>),
    LiteralU64(u64),
    LiteralI64(i64),
    Caller,
    Finalize(LayoutFinalize),
    Native(&'static str),
}

impl LayoutSpec {
    pub const fn varve_native() -> Self {
        Self {
            preset: LayoutPreset::VarveNative,
            parts: &[],
        }
    }

    pub const fn none() -> Self {
        Self {
            preset: LayoutPreset::None,
            parts: &[],
        }
    }

    pub const fn custom(parts: &'static [LayoutPartDescriptor]) -> Self {
        Self {
            preset: LayoutPreset::Custom,
            parts,
        }
    }

    pub const fn none_with_parts(parts: &'static [LayoutPartDescriptor]) -> Self {
        Self {
            preset: LayoutPreset::None,
            parts,
        }
    }

    pub const fn is_varve_native_default(self) -> bool {
        matches!(self.preset, LayoutPreset::VarveNative) && self.parts.is_empty()
    }

    pub fn first_segment(self) -> Option<SegmentDescriptor> {
        self.parts.iter().find_map(|part| match part.kind {
            LayoutPartKind::Segment(segment) => Some(segment),
            _ => None,
        })
    }

    pub fn file_header(self) -> Option<FileHeaderDescriptor> {
        self.parts.iter().find_map(|part| match part.kind {
            LayoutPartKind::FileHeader(header) => Some(header),
            _ => None,
        })
    }
}

#[derive(Clone, Copy, Debug)]
pub struct FormatSpec {
    pub magic: &'static [u8],
    pub version: u16,
    pub endian: Endian,
    /// Declared schema hash compared against the value stored in a file's
    /// native header when the file is opened.
    ///
    /// Schema-hash comparison is opt-in: leaving this at `0` (the default)
    /// records the computed hash when files are created but **disables** the
    /// open-time equality check, so any schema-hash-bearing file of this
    /// format opens without a [`Error::SchemaHashMismatch`]. Pin a non-zero
    /// value (typically [`FormatSpec::computed_schema_hash`], via
    /// `schema_hash: computed;` in `varve_format!` or
    /// [`FormatSpec::with_computed_schema_hash`]) to make mismatched schemas
    /// fail closed at open.
    pub schema_hash: u64,
    pub extension: Option<&'static str>,
    pub index_policy: IndexPolicy,
    pub commit_policy: CommitPolicy,
    pub integrity_policy: IntegrityPolicy,
    pub recovery_policy: RecoveryPolicy,
    pub manifest_policy: ManifestPolicy,
    pub compression_policy: CompressionPolicy,
    pub block_compression: &'static [BlockCompressionDescriptor],
    pub blocks: &'static [BlockDescriptor],
    pub matrix_dimensions: &'static [MatrixDimensionDescriptor],
    pub matrix_commits: &'static [MatrixCommitDescriptor],
    pub matrix_blocks: &'static [MatrixBlockDescriptor],
    pub matrix_aux: &'static [MatrixAuxDescriptor],
    /// Per-block schema identity of the implementations backing
    /// [`FormatSpec::blocks`], as `(block_id, declared endian override,
    /// keyedness, generated schema fingerprint)` tuples (API2-01).
    ///
    /// `varve_format!` fills this from the registered block types so
    /// [`FormatSpec::computed_schema_hash`] covers the per-block encoding
    /// inputs that [`BlockDescriptor`] alone does not carry (endian override,
    /// keyedness, and the generated codec identity). Hand-built specs may
    /// leave it empty; the computed hash then records the absence explicitly.
    /// The fingerprint values themselves stay process-local: they are never
    /// compared against on-disk descriptors, only folded into the computed
    /// hash and checked by the in-process registration gate.
    ///
    /// Where an entry exists for a block id it is **authoritative** for that
    /// id's registration contract, including the endian override (API-01):
    /// typed registration rejects any [`crate::VarveBlock`] implementation
    /// whose `ENDIAN`, resolved through [`FormatSpec::endian`], disagrees with
    /// the entry's — a manual type can no longer request a block written
    /// big-endian through a little-endian implementation and read
    /// byte-swapped values. `None` in the endian position means "no override;
    /// inherit [`FormatSpec::endian`]", which is how it is both hashed
    /// (as an explicit absence marker) and compared (after resolution).
    ///
    /// The slices supplied here and to [`FormatSpec::blocks`] are identified
    /// by address *and* length wherever varve caches per-block validation, so
    /// an empty or prefix view of an array is never mistaken for the full view
    /// (API-02).
    pub block_identities: &'static [(u32, Option<Endian>, bool, u64)],
    pub layout: LayoutSpec,
    pub read_limits: ReadLimits,
    /// Explicit opt-in that keeps matrix data access available when the
    /// matrix recovery report contains `Fatal` findings. Defaults to false:
    /// safe accessors fail closed with [`Error::MatrixFatalCorruption`].
    pub matrix_fatal_forensics: bool,
}

#[derive(Clone, Copy, Debug)]
pub struct FormatSpecBuilder {
    magic: Option<&'static [u8]>,
    version: u16,
    endian: Endian,
    schema_hash: u64,
    extension: Option<&'static str>,
    index_policy: IndexPolicy,
    commit_policy: CommitPolicy,
    integrity_policy: IntegrityPolicy,
    recovery_policy: RecoveryPolicy,
    manifest_policy: ManifestPolicy,
    compression_policy: CompressionPolicy,
    block_compression: &'static [BlockCompressionDescriptor],
    blocks: &'static [BlockDescriptor],
    matrix_dimensions: &'static [MatrixDimensionDescriptor],
    matrix_commits: &'static [MatrixCommitDescriptor],
    matrix_blocks: &'static [MatrixBlockDescriptor],
    matrix_aux: &'static [MatrixAuxDescriptor],
    block_identities: &'static [(u32, Option<Endian>, bool, u64)],
    layout: LayoutSpec,
    read_limits: ReadLimits,
}

impl FormatSpec {
    /// Version of the [`FormatSpec::computed_schema_hash`] algorithm.
    ///
    /// v2 (API2-01, pre-1.0 breaking change): hashes the field encoding
    /// ordinal in declaration order and folds in per-block endian,
    /// keyedness, and generated codec identity from
    /// [`FormatSpec::block_identities`]. Values computed by v1 do not match
    /// v2 for any spec.
    ///
    /// v3 (API-04, pre-1.0 breaking change): the generated block fingerprints
    /// folded in from [`FormatSpec::block_identities`] now resolve every
    /// field's codec through [`crate::VarveEncode::SCHEMA_ID`] instead of the
    /// field type's source spelling, so two custom nested codecs that spell
    /// their field type identically but emit different bytes no longer share
    /// a hash. Values computed by v1 or v2 do not match v3 for any spec.
    pub const SCHEMA_HASH_ALGORITHM_VERSION: u16 = 3;

    #[allow(clippy::too_many_arguments)]
    pub const fn new(
        magic: &'static [u8],
        version: u16,
        endian: Endian,
        schema_hash: u64,
        index_policy: IndexPolicy,
        integrity_policy: IntegrityPolicy,
        recovery_policy: RecoveryPolicy,
        manifest_policy: ManifestPolicy,
        blocks: &'static [BlockDescriptor],
    ) -> Self {
        Self {
            magic,
            version,
            endian,
            schema_hash,
            extension: None,
            index_policy,
            commit_policy: CommitPolicy::None,
            integrity_policy,
            recovery_policy,
            manifest_policy,
            compression_policy: CompressionPolicy::None,
            block_compression: &[],
            blocks,
            matrix_dimensions: &[],
            matrix_commits: &[],
            matrix_blocks: &[],
            matrix_aux: &[],
            block_identities: &[],
            layout: LayoutSpec::varve_native(),
            read_limits: ReadLimits::MISSING,
            matrix_fatal_forensics: false,
        }
    }

    pub const fn with_extension(mut self, extension: Option<&'static str>) -> Self {
        self.extension = extension;
        self
    }

    /// Pins [`FormatSpec::schema_hash`] to the computed hash, opting this
    /// format into the open-time schema-hash equality check. Without this
    /// (or an explicit non-zero literal), `schema_hash` stays `0` and the
    /// comparison is disabled; see [`FormatSpec::schema_hash`].
    pub fn with_computed_schema_hash(mut self) -> Self {
        self.schema_hash = self.computed_schema_hash();
        self
    }

    /// Attaches the per-block schema identities folded into
    /// [`FormatSpec::computed_schema_hash`]; see
    /// [`FormatSpec::block_identities`]. Call before
    /// [`FormatSpec::with_computed_schema_hash`] so the pinned hash covers
    /// the identities.
    pub const fn with_block_identities(
        mut self,
        block_identities: &'static [(u32, Option<Endian>, bool, u64)],
    ) -> Self {
        self.block_identities = block_identities;
        self
    }

    pub const fn with_index_policy(mut self, index_policy: IndexPolicy) -> Self {
        self.index_policy = index_policy;
        self
    }

    pub const fn with_commit_policy(mut self, commit_policy: CommitPolicy) -> Self {
        self.commit_policy = commit_policy;
        self
    }

    pub const fn with_integrity_policy(mut self, integrity_policy: IntegrityPolicy) -> Self {
        self.integrity_policy = integrity_policy;
        self
    }

    pub const fn with_recovery_policy(mut self, recovery_policy: RecoveryPolicy) -> Self {
        self.recovery_policy = recovery_policy;
        self
    }

    pub const fn with_manifest_policy(mut self, manifest_policy: ManifestPolicy) -> Self {
        self.manifest_policy = manifest_policy;
        self
    }

    pub const fn with_compression_policy(mut self, compression_policy: CompressionPolicy) -> Self {
        self.compression_policy = compression_policy;
        self
    }

    pub const fn with_block_compression(
        mut self,
        block_compression: &'static [BlockCompressionDescriptor],
    ) -> Self {
        self.block_compression = block_compression;
        self
    }

    pub const fn with_matrix_spec(
        mut self,
        dimensions: &'static [MatrixDimensionDescriptor],
        commits: &'static [MatrixCommitDescriptor],
        blocks: &'static [MatrixBlockDescriptor],
    ) -> Self {
        self.matrix_dimensions = dimensions;
        self.matrix_commits = commits;
        self.matrix_blocks = blocks;
        self
    }

    pub const fn with_matrix_aux(mut self, aux: &'static [MatrixAuxDescriptor]) -> Self {
        self.matrix_aux = aux;
        self
    }

    /// Explicit forensic/recovery opt-in for matrix files whose recovery
    /// report contains `Fatal` findings (for example a matrix metadata CRC
    /// mismatch). Without this opt-in, every safe matrix accessor on such a
    /// file fails closed with [`Error::MatrixFatalCorruption`]; the recovery
    /// report itself stays readable either way.
    pub const fn with_matrix_fatal_forensics(mut self) -> Self {
        self.matrix_fatal_forensics = true;
        self
    }

    pub const fn with_layout(mut self, layout: LayoutSpec) -> Self {
        self.layout = layout;
        self
    }

    pub const fn with_read_limits(mut self, read_limits: ReadLimits) -> Self {
        self.read_limits = read_limits.clear_trusted_api();
        self
    }

    /// Sets optional format-level defaults. These are operational defaults,
    /// not wire-format or schema ceilings.
    pub const fn with_resource_defaults(self, limits: ResourceLimits) -> Self {
        self.with_read_limits(limits)
    }

    /// Resolves standard and format defaults, then overlays call-time policy.
    pub const fn with_resource_limits(mut self, limits: ResourceLimits) -> Self {
        self.read_limits = self.read_limits.resolve().overlay(limits);
        self
    }

    pub const fn tighten_read_limits(mut self, read_limits: ReadLimits) -> Self {
        self.read_limits = self.read_limits.resolve().tighten(read_limits);
        self
    }

    pub(crate) const fn authorize_trusted_read(mut self) -> Self {
        self.read_limits = self.read_limits.authorize_trusted_api();
        self
    }

    pub(crate) const fn ordinary_read(mut self) -> Self {
        self.read_limits = self.read_limits.resolve().clear_trusted_api();
        self
    }

    pub(crate) const fn resolve_entrypoint(self) -> Self {
        if self.read_limits.trusted_api {
            self
        } else {
            self.ordinary_read()
        }
    }

    pub const fn builder() -> FormatSpecBuilder {
        FormatSpecBuilder::new()
    }

    pub fn create<P: AsRef<Path>>(self, path: P) -> Result<VarveFile> {
        let self_ = self.ordinary_read();
        self_.validate()?;
        VarveFile::create(self_, path)
    }

    pub fn create_with_limits<P: AsRef<Path>>(
        self,
        path: P,
        limits: ReadLimits,
    ) -> Result<VarveFile> {
        self.tighten_read_limits(limits).create(path)
    }

    pub fn create_with_resource_limits<P: AsRef<Path>>(
        self,
        path: P,
        limits: ResourceLimits,
    ) -> Result<VarveFile> {
        self.with_resource_limits(limits).create(path)
    }

    pub fn create_trusted_unbounded<P: AsRef<Path>>(self, path: P) -> Result<VarveFile> {
        let self_ = self.authorize_trusted_read();
        self_.validate()?;
        VarveFile::create(self_, path)
    }

    pub fn create_writer<P: AsRef<Path>>(self, path: P) -> Result<VarveWriter> {
        let self_ = self.ordinary_read();
        self_.validate()?;
        VarveWriter::create(self_, path)
    }

    pub fn create_writer_with_limits<P: AsRef<Path>>(
        self,
        path: P,
        limits: ReadLimits,
    ) -> Result<VarveWriter> {
        self.tighten_read_limits(limits).create_writer(path)
    }

    pub fn create_writer_with_resource_limits<P: AsRef<Path>>(
        self,
        path: P,
        limits: ResourceLimits,
    ) -> Result<VarveWriter> {
        self.with_resource_limits(limits).create_writer(path)
    }

    pub fn create_writer_trusted_unbounded<P: AsRef<Path>>(self, path: P) -> Result<VarveWriter> {
        let self_ = self.authorize_trusted_read();
        self_.validate()?;
        VarveWriter::create(self_, path)
    }

    pub fn create_with_dims<P: AsRef<Path>>(
        self,
        path: P,
        dims: MatrixDimensions,
    ) -> Result<VarveFile> {
        let self_ = self.ordinary_read();
        self_.validate()?;
        VarveFile::create_with_dims(self_, path, dims)
    }

    pub fn create_with_dims_and_limits<P: AsRef<Path>>(
        self,
        path: P,
        dims: MatrixDimensions,
        limits: ReadLimits,
    ) -> Result<VarveFile> {
        self.tighten_read_limits(limits)
            .create_with_dims(path, dims)
    }

    pub fn create_with_dims_and_resource_limits<P: AsRef<Path>>(
        self,
        path: P,
        dims: MatrixDimensions,
        limits: ResourceLimits,
    ) -> Result<VarveFile> {
        self.with_resource_limits(limits)
            .create_with_dims(path, dims)
    }

    pub fn create_with_dims_trusted_unbounded<P: AsRef<Path>>(
        self,
        path: P,
        dims: MatrixDimensions,
    ) -> Result<VarveFile> {
        let self_ = self.authorize_trusted_read();
        self_.validate()?;
        VarveFile::create_with_dims(self_, path, dims)
    }

    pub fn create_writer_with_dims<P: AsRef<Path>>(
        self,
        path: P,
        dims: MatrixDimensions,
    ) -> Result<VarveWriter> {
        let self_ = self.ordinary_read();
        self_.validate()?;
        VarveWriter::create_with_dims(self_, path, dims)
    }

    pub fn create_writer_with_dims_and_limits<P: AsRef<Path>>(
        self,
        path: P,
        dims: MatrixDimensions,
        limits: ReadLimits,
    ) -> Result<VarveWriter> {
        self.tighten_read_limits(limits)
            .create_writer_with_dims(path, dims)
    }

    pub fn create_writer_with_dims_and_resource_limits<P: AsRef<Path>>(
        self,
        path: P,
        dims: MatrixDimensions,
        limits: ResourceLimits,
    ) -> Result<VarveWriter> {
        self.with_resource_limits(limits)
            .create_writer_with_dims(path, dims)
    }

    pub fn create_writer_with_dims_trusted_unbounded<P: AsRef<Path>>(
        self,
        path: P,
        dims: MatrixDimensions,
    ) -> Result<VarveWriter> {
        let self_ = self.authorize_trusted_read();
        self_.validate()?;
        VarveWriter::create_with_dims(self_, path, dims)
    }

    pub fn open<P: AsRef<Path>>(self, path: P) -> Result<VarveFile> {
        let self_ = self.ordinary_read();
        self_.validate()?;
        VarveFile::open(self_, path)
    }

    pub fn open_with_limits<P: AsRef<Path>>(
        self,
        path: P,
        limits: ReadLimits,
    ) -> Result<VarveFile> {
        self.tighten_read_limits(limits).open(path)
    }

    pub fn open_with_resource_limits<P: AsRef<Path>>(
        self,
        path: P,
        limits: ResourceLimits,
    ) -> Result<VarveFile> {
        self.with_resource_limits(limits).open(path)
    }

    pub fn open_trusted_unbounded<P: AsRef<Path>>(self, path: P) -> Result<VarveFile> {
        let self_ = self.authorize_trusted_read();
        self_.validate()?;
        VarveFile::open(self_, path)
    }

    pub fn open_writer<P: AsRef<Path>>(self, path: P) -> Result<VarveWriter> {
        let self_ = self.ordinary_read();
        self_.validate()?;
        VarveWriter::open(self_, path)
    }

    pub fn open_writer_with_limits<P: AsRef<Path>>(
        self,
        path: P,
        limits: ReadLimits,
    ) -> Result<VarveWriter> {
        self.tighten_read_limits(limits).open_writer(path)
    }

    pub fn open_writer_with_resource_limits<P: AsRef<Path>>(
        self,
        path: P,
        limits: ResourceLimits,
    ) -> Result<VarveWriter> {
        self.with_resource_limits(limits).open_writer(path)
    }

    pub fn open_writer_trusted_unbounded<P: AsRef<Path>>(self, path: P) -> Result<VarveWriter> {
        let self_ = self.authorize_trusted_read();
        self_.validate()?;
        VarveWriter::open(self_, path)
    }

    pub fn open_with_lock_policy<P: AsRef<Path>>(
        self,
        path: P,
        policy: WriterLockBreakPolicy,
    ) -> Result<VarveFile> {
        let self_ = self.ordinary_read();
        self_.validate()?;
        VarveFile::open_with_lock_policy(self_, path, policy)
    }

    pub fn open_writer_with_lock_policy<P: AsRef<Path>>(
        self,
        path: P,
        policy: WriterLockBreakPolicy,
    ) -> Result<VarveWriter> {
        let self_ = self.ordinary_read();
        self_.validate()?;
        VarveWriter::open_with_lock_policy(self_, path, policy)
    }

    /// Clears a stale writer lock without opening or scanning the data file.
    pub fn clear_stale_writer_lock<P: AsRef<Path>>(
        self,
        path: P,
        policy: WriterLockBreakPolicy,
    ) -> Result<()> {
        self.validate()?;
        crate::clear_stale_writer_lock(path, policy)
    }

    pub fn open_readonly<P: AsRef<Path>>(self, path: P) -> Result<VarveFile> {
        let self_ = self.ordinary_read();
        self_.validate()?;
        VarveFile::open_readonly(self_, path)
    }

    pub fn open_readonly_with_limits<P: AsRef<Path>>(
        self,
        path: P,
        limits: ReadLimits,
    ) -> Result<VarveFile> {
        self.tighten_read_limits(limits).open_readonly(path)
    }

    pub fn open_readonly_with_resource_limits<P: AsRef<Path>>(
        self,
        path: P,
        limits: ResourceLimits,
    ) -> Result<VarveFile> {
        self.with_resource_limits(limits).open_readonly(path)
    }

    pub fn open_readonly_trusted_unbounded<P: AsRef<Path>>(self, path: P) -> Result<VarveFile> {
        let self_ = self.authorize_trusted_read();
        self_.validate()?;
        VarveFile::open_readonly(self_, path)
    }

    pub fn open_reader<P: AsRef<Path>>(self, path: P) -> Result<VarveReader> {
        let self_ = self.ordinary_read();
        self_.validate()?;
        VarveReader::open(self_, path)
    }

    pub fn open_reader_with_limits<P: AsRef<Path>>(
        self,
        path: P,
        limits: ReadLimits,
    ) -> Result<VarveReader> {
        self.tighten_read_limits(limits).open_reader(path)
    }

    pub fn open_reader_with_resource_limits<P: AsRef<Path>>(
        self,
        path: P,
        limits: ResourceLimits,
    ) -> Result<VarveReader> {
        self.with_resource_limits(limits).open_reader(path)
    }

    pub fn open_reader_trusted_unbounded<P: AsRef<Path>>(self, path: P) -> Result<VarveReader> {
        let self_ = self.authorize_trusted_read();
        self_.validate()?;
        VarveReader::open(self_, path)
    }

    pub fn open_recover<P: AsRef<Path>>(self, path: P) -> Result<VarveFile> {
        let self_ = self.ordinary_read();
        self_.validate()?;
        VarveFile::open_recover(self_, path)
    }

    pub fn open_recover_with_limits<P: AsRef<Path>>(
        self,
        path: P,
        limits: ReadLimits,
    ) -> Result<VarveFile> {
        self.tighten_read_limits(limits).open_recover(path)
    }

    pub fn open_recover_with_resource_limits<P: AsRef<Path>>(
        self,
        path: P,
        limits: ResourceLimits,
    ) -> Result<VarveFile> {
        self.with_resource_limits(limits).open_recover(path)
    }

    pub fn open_recover_trusted_unbounded<P: AsRef<Path>>(self, path: P) -> Result<VarveFile> {
        let self_ = self.authorize_trusted_read();
        self_.validate()?;
        VarveFile::open_recover(self_, path)
    }

    pub fn open_recover_writer<P: AsRef<Path>>(self, path: P) -> Result<VarveWriter> {
        let self_ = self.ordinary_read();
        self_.validate()?;
        VarveWriter::open_recover(self_, path)
    }

    pub fn open_recover_writer_with_limits<P: AsRef<Path>>(
        self,
        path: P,
        limits: ReadLimits,
    ) -> Result<VarveWriter> {
        self.tighten_read_limits(limits).open_recover_writer(path)
    }

    pub fn open_recover_writer_with_resource_limits<P: AsRef<Path>>(
        self,
        path: P,
        limits: ResourceLimits,
    ) -> Result<VarveWriter> {
        self.with_resource_limits(limits).open_recover_writer(path)
    }

    pub fn open_recover_writer_trusted_unbounded<P: AsRef<Path>>(
        self,
        path: P,
    ) -> Result<VarveWriter> {
        let self_ = self.authorize_trusted_read();
        self_.validate()?;
        VarveWriter::open_recover(self_, path)
    }

    pub fn open_recover_with_report<P: AsRef<Path>>(
        self,
        path: P,
    ) -> Result<(VarveFile, crate::RecoveryReport)> {
        let self_ = self.ordinary_read();
        self_.validate()?;
        VarveFile::open_recover_with_report(self_, path)
    }

    pub fn open_recover_with_report_and_limits<P: AsRef<Path>>(
        self,
        path: P,
        limits: ReadLimits,
    ) -> Result<(VarveFile, crate::RecoveryReport)> {
        self.tighten_read_limits(limits)
            .open_recover_with_report(path)
    }

    pub fn open_recover_with_report_and_resource_limits<P: AsRef<Path>>(
        self,
        path: P,
        limits: ResourceLimits,
    ) -> Result<(VarveFile, crate::RecoveryReport)> {
        self.with_resource_limits(limits)
            .open_recover_with_report(path)
    }

    pub fn open_recover_with_report_trusted_unbounded<P: AsRef<Path>>(
        self,
        path: P,
    ) -> Result<(VarveFile, crate::RecoveryReport)> {
        let self_ = self.authorize_trusted_read();
        self_.validate()?;
        VarveFile::open_recover_with_report(self_, path)
    }

    pub fn open_recover_writer_with_report<P: AsRef<Path>>(
        self,
        path: P,
    ) -> Result<(VarveWriter, crate::RecoveryReport)> {
        let self_ = self.ordinary_read();
        self_.validate()?;
        VarveWriter::open_recover_with_report(self_, path)
    }

    pub fn open_recover_writer_with_report_and_limits<P: AsRef<Path>>(
        self,
        path: P,
        limits: ReadLimits,
    ) -> Result<(VarveWriter, crate::RecoveryReport)> {
        self.tighten_read_limits(limits)
            .open_recover_writer_with_report(path)
    }

    pub fn open_recover_writer_with_report_and_resource_limits<P: AsRef<Path>>(
        self,
        path: P,
        limits: ResourceLimits,
    ) -> Result<(VarveWriter, crate::RecoveryReport)> {
        self.with_resource_limits(limits)
            .open_recover_writer_with_report(path)
    }

    pub fn open_recover_writer_with_report_trusted_unbounded<P: AsRef<Path>>(
        self,
        path: P,
    ) -> Result<(VarveWriter, crate::RecoveryReport)> {
        let self_ = self.authorize_trusted_read();
        self_.validate()?;
        VarveWriter::open_recover_with_report(self_, path)
    }

    pub fn inspect_writer_lock<P: AsRef<Path>>(self, path: P) -> Result<Option<WriterLockInfo>> {
        self.validate()?;
        VarveFile::inspect_writer_lock(path)
    }

    pub fn diagnostics(self) -> crate::FormatDiagnostics {
        crate::diagnose_spec(self)
    }

    pub fn diagnose_file<P: AsRef<Path>>(self, path: P) -> crate::FormatDiagnostics {
        crate::diagnose_file(self, path)
    }

    pub fn self_test<P: AsRef<Path>>(self, path: P) -> crate::FormatSelfTest {
        crate::FormatSelfTest::new(self, path)
    }

    pub fn block(self, id: u32) -> Option<BlockDescriptor> {
        self.blocks.iter().copied().find(|block| block.id == id)
    }

    pub fn effective_layout(&self) -> LayoutPlan {
        if self.layout.is_varve_native_default() {
            native_layout_plan(*self)
        } else {
            layout_spec_to_plan(self.layout)
        }
    }

    pub fn schema_debug_dump(&self) -> String {
        let mut output = String::new();
        output.push_str("varve schema\n");
        output.push_str(&format!("version: {}\n", self.version));
        output.push_str(&format!("endian: {:?}\n", self.endian));
        output.push_str(&format!("extension: {:?}\n", self.extension));
        output.push_str(&format!("schema_hash: {}\n", self.schema_hash));
        output.push_str(&format!(
            "computed_schema_hash: {}\n",
            self.computed_schema_hash()
        ));
        output.push_str(&format!("index_policy: {:?}\n", self.index_policy));
        output.push_str(&format!("commit_policy: {:?}\n", self.commit_policy));
        output.push_str(&format!("integrity_policy: {:?}\n", self.integrity_policy));
        output.push_str(&format!("recovery_policy: {:?}\n", self.recovery_policy));
        output.push_str(&format!("manifest_policy: {:?}\n", self.manifest_policy));
        output.push_str(&format!(
            "compression_policy: {:?}\n",
            self.compression_policy
        ));
        output.push_str(&format!("layout_preset: {:?}\n", self.layout.preset));
        for part in self.layout.parts {
            output.push_str(&format!("layout_part {} {:?}\n", part.name, part.kind));
        }
        let effective_layout = self.effective_layout();
        output.push_str(&format!(
            "effective_layout_preset: {:?}\n",
            effective_layout.preset
        ));
        for part in &effective_layout.parts {
            output.push_str(&format!(
                "effective_layout_part {} {:?}\n",
                part.name, part.kind
            ));
        }
        for override_policy in self.block_compression {
            output.push_str(&format!(
                "block_compression {} {:?}\n",
                override_policy.block_id, override_policy.compression
            ));
        }
        if !self.matrix_dimensions.is_empty()
            || !self.matrix_commits.is_empty()
            || !self.matrix_blocks.is_empty()
            || !self.matrix_aux.is_empty()
        {
            output.push_str("matrix:\n");
            for dimension in self.matrix_dimensions {
                output.push_str(&format!("  dim {}\n", dimension.name));
            }
            for commit in self.matrix_commits {
                output.push_str(&format!("  commit {} {:?}\n", commit.name, commit.kind));
            }
            for block in self.matrix_blocks {
                output.push_str(&format!(
                    "  block {} dims [{}, {}] category {} stride {}\n",
                    block.block_id,
                    block.dimensions[0],
                    block.dimensions[1],
                    block.category,
                    block.slot_stride
                ));
            }
            for aux in self.matrix_aux {
                output.push_str(&format!("  aux {} bytes {}\n", aux.name, aux.byte_len));
            }
        }
        let mut blocks = self.blocks.to_vec();
        blocks.sort_by_key(|block| block.id);
        for block in blocks {
            output.push_str(&format!(
                "block {} {} v{} {:?}\n",
                block.id, block.name, block.version, block.kind
            ));
            // Declaration order: fixed/matrix payload bytes follow it, and
            // the v2 schema hash covers the encoding ordinal.
            for field in block.fields {
                output.push_str(&format!(
                    "  field {} {} {:?} {:?}\n",
                    field.id, field.name, field.wire_type, field.presence
                ));
            }
        }
        output
    }

    /// Deterministic hash of the wire-relevant schema declaration
    /// (algorithm version [`FormatSpec::SCHEMA_HASH_ALGORITHM_VERSION`]).
    ///
    /// Version 2 (API2-01) additionally covers, per block: the field
    /// **encoding ordinal** (fixed/matrix payload bytes follow field
    /// declaration order, so two blocks with the same field id/name/type set
    /// in different declaration order must hash differently), and — when
    /// [`FormatSpec::block_identities`] is populated — the block's endian
    /// override, keyedness, and generated codec fingerprint.
    ///
    /// Version 3 (API-04) inherits transitive custom-codec identity: the
    /// generated fingerprints in [`FormatSpec::block_identities`] resolve each
    /// field through [`crate::VarveEncode::SCHEMA_ID`], so a nested codec that
    /// changes its emitted bytes changes this hash even when every declared
    /// type name stays the same.
    ///
    /// The computed value is stored in newly created files. Whether it is
    /// *compared* at open is a separate opt-in; see
    /// [`FormatSpec::schema_hash`].
    pub fn computed_schema_hash(&self) -> u64 {
        let mut hash = Fnv1a64::new();
        hash.write_bytes(b"varve-schema-v3");
        hash.write_u16(self.version);
        hash.write_u8(self.endian.to_byte());
        match self.extension {
            Some(extension) => {
                hash.write_u8(1);
                hash.write_str(extension);
            }
            None => hash.write_u8(0),
        }
        hash.write_u8(index_policy_hash_byte(self.index_policy));
        hash.write_u8(commit_policy_hash_byte(self.commit_policy));
        hash.write_u8(integrity_policy_hash_byte(self.integrity_policy));
        hash.write_u8(recovery_policy_hash_byte(self.recovery_policy));
        hash.write_u8(manifest_policy_hash_byte(self.manifest_policy));
        hash_compression_policy(&mut hash, self.compression_policy);
        let mut block_compression = self.block_compression.to_vec();
        block_compression.sort_by_key(|descriptor| descriptor.block_id);
        for descriptor in block_compression {
            hash.write_u32(descriptor.block_id);
            hash_variable_compression(&mut hash, descriptor.compression);
        }
        for dimension in self.matrix_dimensions {
            hash.write_str(dimension.name);
        }
        for commit in self.matrix_commits {
            hash.write_str(commit.name);
            hash.write_u8(matrix_commit_kind_hash_byte(commit.kind));
        }
        let mut matrix_blocks = self.matrix_blocks.to_vec();
        matrix_blocks.sort_by_key(|block| block.block_id);
        for block in matrix_blocks {
            hash.write_u32(block.block_id);
            hash.write_str(block.dimensions[0]);
            hash.write_str(block.dimensions[1]);
            hash.write_str(block.category);
            hash.write_bytes(&block.slot_stride.to_le_bytes());
        }
        for aux in self.matrix_aux {
            hash.write_str(aux.name);
            hash.write_bytes(&aux.byte_len.to_le_bytes());
        }
        if !self.layout.is_varve_native_default() {
            hash.write_bytes(b"layout-v1");
            hash_layout_spec(&mut hash, self.layout);
        }
        let mut blocks = self.blocks.to_vec();
        blocks.sort_by_key(|block| block.id);
        for block in blocks {
            hash.write_u32(block.id);
            hash.write_str(block.name);
            hash.write_u16(block.version);
            hash.write_u8(block_kind_hash_byte(block.kind));
            // Per-block identity: endian override, keyedness, and generated
            // codec fingerprint, with an explicit absence marker so a spec
            // without identities can never collide with one that has them.
            match self.block_identity(block.id) {
                Some((_, endian, keyed, fingerprint)) => {
                    hash.write_u8(1);
                    hash.write_u8(match endian {
                        Some(endian) => endian.to_byte(),
                        None => 0,
                    });
                    hash.write_u8(u8::from(keyed));
                    hash.write_bytes(&fingerprint.to_le_bytes());
                }
                None => hash.write_u8(0),
            }
            // Fixed/matrix encoding follows field declaration order, so hash
            // the ordinal alongside each field instead of sorting by id.
            hash.write_bytes(&(block.fields.len() as u64).to_le_bytes());
            for (ordinal, field) in block.fields.iter().enumerate() {
                hash.write_u32(ordinal as u32);
                hash.write_u32(field.id);
                hash.write_str(field.name);
                hash.write_u16(field.wire_type as u16);
                hash.write_u8(field_presence_hash_byte(field.presence));
            }
        }
        hash.finish()
    }

    /// Returns the `(block_id, endian override, keyedness, fingerprint)`
    /// identity declared for `id`, if any; see
    /// [`FormatSpec::block_identities`].
    pub fn block_identity(&self, id: u32) -> Option<(u32, Option<Endian>, bool, u64)> {
        self.block_identities
            .iter()
            .copied()
            .find(|identity| identity.0 == id)
    }

    pub fn validate(self) -> Result<()> {
        if self.magic.is_empty() {
            return Err(Error::InvalidFormatSpec("magic must not be empty"));
        }
        if let Some(extension) = self.extension {
            if extension.is_empty() {
                return Err(Error::InvalidFormatSpec("extension must not be empty"));
            }
            if extension.contains(['/', '\\']) {
                return Err(Error::InvalidFormatSpec(
                    "extension must not contain path separators",
                ));
            }
        }
        if self.index_policy.keyed_offset_chain && !self.index_policy.block_offset_chain {
            return Err(Error::InvalidFormatSpec(
                "keyed_offset_chain requires block_offset_chain",
            ));
        }
        // A full checkpoint answers "what is the whole index?" by serialising
        // every entry from scratch; a segment answers it incrementally, and
        // unlike the checkpoint it is the thing open actually reads - open
        // validates a checkpoint it walks past and discards its entries. With
        // the chain on, `checkpoint_on_flush` writes a periodic full copy of
        // the index that nothing will ever read, and stops fitting in a record
        // at all past `(max_record_payload_len - 22) / 73` entries. Refusing is
        // louder than quietly clearing it: a format that declared the
        // checkpoint asked for something, and silently not delivering it is how
        // a policy becomes folklore.
        if self.index_policy.segment_on_flush && self.index_policy.checkpoint_on_flush {
            return Err(Error::InvalidFormatSpec(
                "segment_on_flush supersedes checkpoint_on_flush; declare one",
            ));
        }
        if self.index_policy.segment_on_flush && !self.index_policy.block_offset_chain {
            return Err(Error::InvalidFormatSpec(
                "segment_on_flush requires block_offset_chain",
            ));
        }
        // A record scan reads each record's own header, so damage to one record
        // misindexes that record. A segment chain reads one payload that
        // describes many records, so damage to it misindexes records whose own
        // bytes are intact - and open would have no way to notice. The record
        // checksum is what closes that, and the chain walk verifies it on every
        // link, so the chain is available exactly where there is a checksum to
        // verify.
        if self.index_policy.segment_on_flush
            && !matches!(
                self.integrity_policy,
                IntegrityPolicy::Crc32 | IntegrityPolicy::Crc32WithHeader
            )
        {
            return Err(Error::InvalidFormatSpec(
                "segment_on_flush requires a crc32 integrity policy",
            ));
        }
        for (index, block) in self.blocks.iter().enumerate() {
            if block.id >= RESERVED_BLOCK_ID_START {
                return Err(Error::InvalidFormatSpec("user block id is reserved"));
            }
            if block.version == 0 {
                return Err(Error::InvalidFormatSpec("block version must be non-zero"));
            }
            if block.name.is_empty() {
                return Err(Error::InvalidFormatSpec("block name must not be empty"));
            }
            if block.kind == BlockKind::Matrix
                && !self
                    .matrix_blocks
                    .iter()
                    .any(|matrix| matrix.block_id == block.id)
            {
                return Err(Error::InvalidFormatSpec(
                    "matrix block missing matrix descriptor",
                ));
            }
            for (field_index, field) in block.fields.iter().enumerate() {
                if field.id == 0 {
                    return Err(Error::InvalidFormatSpec("field id must be non-zero"));
                }
                if field.name.is_empty() {
                    return Err(Error::InvalidFormatSpec("field name must not be empty"));
                }
                for other in &block.fields[(field_index + 1)..] {
                    if field.id == other.id {
                        return Err(Error::InvalidFormatSpec("duplicate field id"));
                    }
                }
            }
            for other in &self.blocks[(index + 1)..] {
                if block.id == other.id {
                    return Err(Error::InvalidFormatSpec("duplicate block id"));
                }
            }
        }
        for (index, identity) in self.block_identities.iter().enumerate() {
            if !self.blocks.iter().any(|block| block.id == identity.0) {
                return Err(Error::InvalidFormatSpec(
                    "block identity references an unregistered block",
                ));
            }
            for other in &self.block_identities[(index + 1)..] {
                if identity.0 == other.0 {
                    return Err(Error::InvalidFormatSpec("duplicate block identity"));
                }
            }
        }
        self.validate_matrix_spec()?;
        self.validate_layout_spec()?;
        if let CompressionPolicy::VariableBlocks(compression) = self.compression_policy {
            self.validate_variable_compression(compression)?;
        }
        for (index, descriptor) in self.block_compression.iter().enumerate() {
            let block = self
                .block(descriptor.block_id)
                .ok_or(Error::InvalidFormatSpec(
                    "block compression id is not registered",
                ))?;
            if block.kind != BlockKind::Variable {
                return Err(Error::InvalidFormatSpec(
                    "block compression requires a variable block",
                ));
            }
            if descriptor.compression.header_mode != CompressionHeaderMode::RecordExplicit {
                return Err(Error::InvalidFormatSpec(
                    "block compression requires record_explicit header",
                ));
            }
            self.validate_variable_compression(descriptor.compression)?;
            for other in &self.block_compression[(index + 1)..] {
                if descriptor.block_id == other.block_id {
                    return Err(Error::InvalidFormatSpec("duplicate block compression"));
                }
            }
        }
        Ok(())
    }

    pub const fn spec_needs_record_footer(self) -> bool {
        self.commit_policy.requires_record_footer() || self.index_policy.requires_record_footer()
    }

    pub const fn has_matrix_blocks(self) -> bool {
        !self.matrix_blocks.is_empty()
    }

    fn validate_layout_spec(self) -> Result<()> {
        match self.layout.preset {
            LayoutPreset::VarveNative => {
                if !self.layout.parts.is_empty() {
                    return Err(Error::InvalidFormatSpec(
                        "varve_native layout must not declare custom parts",
                    ));
                }
                crate::native_layout::ensure_native_file_header_layout_contract(self)?;
                crate::native_layout::ensure_native_record_layout_contract()?;
            }
            LayoutPreset::None | LayoutPreset::Custom => {
                if self.has_matrix_blocks() {
                    return Err(Error::InvalidFormatSpec(
                        "custom layout does not support matrix blocks in this release",
                    ));
                }
                if self.commit_policy != CommitPolicy::None {
                    return Err(Error::InvalidFormatSpec(
                        "custom layout does not support native commit policy in this release",
                    ));
                }
                if self.index_policy.checkpoint_on_flush
                    || self.index_policy.block_offset_chain
                    || self.index_policy.keyed_offset_chain
                {
                    return Err(Error::InvalidFormatSpec(
                        "custom layout does not support native index records in this release",
                    ));
                }
                if self.manifest_policy != ManifestPolicy::None {
                    return Err(Error::InvalidFormatSpec(
                        "custom layout does not support embedded native manifests in this release",
                    ));
                }
                if self.compression_policy != CompressionPolicy::None
                    || !self.block_compression.is_empty()
                {
                    return Err(Error::InvalidFormatSpec(
                        "custom layout does not support native record compression in this release",
                    ));
                }
                if self.layout.parts.is_empty() {
                    return Err(Error::InvalidFormatSpec(
                        "custom layout requires at least one layout part",
                    ));
                }
            }
        }

        for (index, part) in self.layout.parts.iter().enumerate() {
            if part.name.is_empty() {
                return Err(Error::InvalidFormatSpec(
                    "layout part name must not be empty",
                ));
            }
            for other in &self.layout.parts[(index + 1)..] {
                if part.name == other.name {
                    return Err(Error::InvalidFormatSpec("duplicate layout part name"));
                }
            }
            match part.kind {
                LayoutPartKind::FileHeader(header) => {
                    self.validate_layout_fields(header.fields)?;
                    if header
                        .fields
                        .iter()
                        .any(|field| matches!(field.source, LayoutFieldSource::Finalize(_)))
                    {
                        return Err(Error::InvalidFormatSpec(
                            "layout file_header does not support finalized fields in this release",
                        ));
                    }
                }
                LayoutPartKind::Segment(segment) => self.validate_segment_layout(segment)?,
                LayoutPartKind::LeadIn(lead_in) => self.validate_layout_fields(lead_in.fields)?,
                LayoutPartKind::Metadata(metadata) => {
                    if metadata.name.is_empty() {
                        return Err(Error::InvalidFormatSpec(
                            "layout metadata name must not be empty",
                        ));
                    }
                }
                LayoutPartKind::RawRegion(raw) => {
                    if raw.name.is_empty() {
                        return Err(Error::InvalidFormatSpec(
                            "layout raw region name must not be empty",
                        ));
                    }
                }
                LayoutPartKind::Footer(footer) => self.validate_layout_fields(footer.fields)?,
            }
        }
        if !matches!(self.layout.preset, LayoutPreset::VarveNative) {
            crate::layout::validate_layout_segment_dispatch(self)?;
        }
        Ok(())
    }

    fn validate_segment_layout(self, segment: SegmentDescriptor) -> Result<()> {
        if segment.name.is_empty()
            || segment.lead_in.name.is_empty()
            || segment.metadata.name.is_empty()
            || segment.raw_region.name.is_empty()
        {
            return Err(Error::InvalidFormatSpec(
                "layout segment names must not be empty",
            ));
        }
        self.validate_layout_fields(segment.lead_in.fields)?;
        if let Some(footer) = segment.footer {
            if footer.name.is_empty() {
                return Err(Error::InvalidFormatSpec(
                    "layout footer name must not be empty",
                ));
            }
            self.validate_layout_fields(footer.fields)?;
        }
        let has_segment_end = segment.lead_in.fields.iter().any(|field| {
            matches!(
                field.source,
                LayoutFieldSource::Finalize(LayoutFinalize {
                    target: LayoutAnchor::SegmentEnd,
                    relative_to: LayoutAnchor::SegmentStart
                        | LayoutAnchor::AfterLeadIn
                        | LayoutAnchor::MetadataStart
                        | LayoutAnchor::RawRegionStart
                })
            )
        });
        let has_raw_start = segment.lead_in.fields.iter().any(|field| {
            matches!(
                field.source,
                LayoutFieldSource::Finalize(LayoutFinalize {
                    target: LayoutAnchor::RawRegionStart,
                    relative_to: LayoutAnchor::SegmentStart
                        | LayoutAnchor::AfterLeadIn
                        | LayoutAnchor::MetadataStart
                })
            )
        });
        if !has_segment_end || !has_raw_start {
            return Err(Error::InvalidFormatSpec(
                "layout segment requires scan-resolvable finalized segment_end and raw_region_start fields",
            ));
        }
        Ok(())
    }

    fn validate_layout_fields(self, fields: &[LayoutFieldDescriptor]) -> Result<()> {
        if fields.is_empty() {
            return Err(Error::InvalidFormatSpec("layout fields must not be empty"));
        }
        let mut total_len = 0u64;
        for (index, field) in fields.iter().enumerate() {
            if field.name.is_empty() {
                return Err(Error::InvalidFormatSpec(
                    "layout field name must not be empty",
                ));
            }
            let len = field.ty.byte_len();
            if len == 0 {
                return Err(Error::InvalidFormatSpec(
                    "layout field length must be non-zero",
                ));
            }
            total_len = total_len
                .checked_add(len)
                .ok_or(Error::InvalidFormatSpec("layout fields overflow"))?;
            if let LayoutFieldSource::LiteralBytes(bytes) = field.source
                && !matches!(field.ty, LayoutFieldType::Bytes { len } if len == bytes.len() as u64)
            {
                return Err(Error::InvalidFormatSpec(
                    "literal bytes length must match layout field length",
                ));
            }
            if matches!(field.source, LayoutFieldSource::LiteralBytes(_))
                && !matches!(field.ty, LayoutFieldType::Bytes { .. })
            {
                return Err(Error::InvalidFormatSpec(
                    "literal bytes require a bytes layout field",
                ));
            }
            if matches!(field.source, LayoutFieldSource::LiteralI64(_))
                && !matches!(field.ty, LayoutFieldType::I64)
            {
                return Err(Error::InvalidFormatSpec(
                    "literal i64 requires an i64 layout field",
                ));
            }
            if matches!(field.source, LayoutFieldSource::Finalize(_))
                && matches!(field.ty, LayoutFieldType::Bytes { .. })
            {
                return Err(Error::InvalidFormatSpec(
                    "finalized layout fields must be numeric",
                ));
            }
            for other in &fields[(index + 1)..] {
                if field.name == other.name {
                    return Err(Error::InvalidFormatSpec("duplicate layout field name"));
                }
            }
        }
        Ok(())
    }

    fn validate_matrix_spec(self) -> Result<()> {
        for (index, dimension) in self.matrix_dimensions.iter().enumerate() {
            if dimension.name.is_empty() {
                return Err(Error::InvalidFormatSpec(
                    "matrix dimension name must not be empty",
                ));
            }
            for other in &self.matrix_dimensions[(index + 1)..] {
                if dimension.name == other.name {
                    return Err(Error::InvalidFormatSpec("duplicate matrix dimension"));
                }
            }
        }
        for (index, commit) in self.matrix_commits.iter().enumerate() {
            if commit.name.is_empty() {
                return Err(Error::InvalidFormatSpec(
                    "matrix commit category name must not be empty",
                ));
            }
            for other in &self.matrix_commits[(index + 1)..] {
                if commit.name == other.name {
                    return Err(Error::InvalidFormatSpec("duplicate matrix commit category"));
                }
            }
        }
        for (index, block) in self.matrix_blocks.iter().enumerate() {
            if block.slot_stride == 0 {
                return Err(Error::InvalidFormatSpec(
                    "matrix slot_stride must be non-zero",
                ));
            }
            let descriptor = self.block(block.block_id).ok_or(Error::InvalidFormatSpec(
                "matrix block id is not registered",
            ))?;
            if descriptor.kind != BlockKind::Matrix {
                return Err(Error::InvalidFormatSpec(
                    "matrix descriptor points at a non-matrix block",
                ));
            }
            for dimension in block.dimensions {
                if !self
                    .matrix_dimensions
                    .iter()
                    .any(|candidate| candidate.name == dimension)
                {
                    return Err(Error::InvalidFormatSpec(
                        "matrix block references unknown dimension",
                    ));
                }
            }
            if !self.matrix_commits.iter().any(|commit| {
                commit.name == block.category && commit.kind == MatrixCommitKind::Cell
            }) {
                return Err(Error::InvalidFormatSpec(
                    "matrix block references unknown cell commit category",
                ));
            }
            for other in &self.matrix_blocks[(index + 1)..] {
                if block.block_id == other.block_id {
                    return Err(Error::InvalidFormatSpec(
                        "duplicate matrix block descriptor",
                    ));
                }
                if block.category == other.category {
                    return Err(Error::InvalidFormatSpec(
                        "matrix cell commit category must be unique per block",
                    ));
                }
            }
        }
        if !self.matrix_aux.is_empty() && self.matrix_blocks.is_empty() {
            return Err(Error::InvalidFormatSpec(
                "matrix aux requires at least one matrix block",
            ));
        }
        for (index, aux) in self.matrix_aux.iter().enumerate() {
            if aux.name.is_empty() {
                return Err(Error::InvalidFormatSpec(
                    "matrix aux name must not be empty",
                ));
            }
            if aux.byte_len == 0 {
                return Err(Error::InvalidFormatSpec(
                    "matrix aux byte_len must be non-zero",
                ));
            }
            for other in &self.matrix_aux[(index + 1)..] {
                if aux.name == other.name {
                    return Err(Error::InvalidFormatSpec("duplicate matrix aux"));
                }
            }
        }
        Ok(())
    }

    fn validate_variable_compression(self, compression: VariableCompression) -> Result<()> {
        if compression.max_uncompressed_len == 0 {
            return Err(Error::InvalidFormatSpec(
                "compression max_uncompressed_len must be non-zero",
            ));
        }
        if compression.min_uncompressed_len > compression.max_uncompressed_len {
            return Err(Error::InvalidFormatSpec(
                "compression min_uncompressed_len exceeds max_uncompressed_len",
            ));
        }
        if compression.header_mode != CompressionHeaderMode::RecordExplicit
            && compression.max_uncompressed_len > u64::from(u32::MAX)
        {
            return Err(Error::InvalidFormatSpec(
                "file/contract compression max_uncompressed_len must fit u32",
            ));
        }
        if compression.header_mode == CompressionHeaderMode::FormatContract && self.schema_hash == 0
        {
            return Err(Error::InvalidFormatSpec(
                "format contract compression requires a non-zero schema_hash",
            ));
        }
        if compression.header_mode == CompressionHeaderMode::FormatContract
            && self.schema_hash != self.computed_schema_hash()
        {
            return Err(Error::InvalidFormatSpec(
                "format contract compression requires schema_hash to equal computed_schema_hash",
            ));
        }
        Ok(())
    }
}

struct Fnv1a64 {
    state: u64,
}

impl Fnv1a64 {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;

    fn new() -> Self {
        Self {
            state: Self::OFFSET,
        }
    }

    fn write_bytes(&mut self, bytes: &[u8]) {
        for byte in bytes {
            self.state ^= u64::from(*byte);
            self.state = self.state.wrapping_mul(Self::PRIME);
        }
    }

    fn write_u8(&mut self, value: u8) {
        self.write_bytes(&[value]);
    }

    fn write_u16(&mut self, value: u16) {
        self.write_bytes(&value.to_le_bytes());
    }

    fn write_u32(&mut self, value: u32) {
        self.write_bytes(&value.to_le_bytes());
    }

    fn write_str(&mut self, value: &str) {
        self.write_bytes(&(value.len() as u64).to_le_bytes());
        self.write_bytes(value.as_bytes());
    }

    fn finish(self) -> u64 {
        self.state
    }
}

const fn block_kind_hash_byte(kind: BlockKind) -> u8 {
    match kind {
        BlockKind::Fixed => 1,
        BlockKind::Variable => 2,
        BlockKind::Matrix => 3,
        BlockKind::Internal => 4,
    }
}

const fn matrix_commit_kind_hash_byte(kind: MatrixCommitKind) -> u8 {
    match kind {
        MatrixCommitKind::Cell => 1,
        MatrixCommitKind::Single => 2,
        MatrixCommitKind::PerChannel => 3,
    }
}

const fn index_policy_hash_byte(policy: IndexPolicy) -> u8 {
    // Bit 4 is `segment_on_flush`. It is hashed because it changes the bytes
    // a writer produces - a segment record is a record - so a file written
    // with it is not the file a spec without it describes. A spec that leaves
    // it off hashes to exactly the byte it hashed to before the bit existed.
    (if policy.scan_on_open { 1 } else { 0 })
        | (if policy.checkpoint_on_flush {
            1 << 1
        } else {
            0
        })
        | (if policy.block_offset_chain { 1 << 2 } else { 0 })
        | (if policy.keyed_offset_chain { 1 << 3 } else { 0 })
        | (if policy.segment_on_flush { 1 << 4 } else { 0 })
}

const fn commit_policy_hash_byte(policy: CommitPolicy) -> u8 {
    match policy {
        CommitPolicy::None => 1,
        CommitPolicy::RecordFooter => 2,
        CommitPolicy::TransactionMarker(TransactionMarkerMode::OnFlush) => 3,
        CommitPolicy::TransactionMarker(TransactionMarkerMode::Explicit) => 4,
    }
}

const fn integrity_policy_hash_byte(policy: IntegrityPolicy) -> u8 {
    match policy {
        IntegrityPolicy::None => 1,
        IntegrityPolicy::Crc32 => 2,
        IntegrityPolicy::Crc32WithHeader => 3,
    }
}

const fn recovery_policy_hash_byte(policy: RecoveryPolicy) -> u8 {
    match policy {
        RecoveryPolicy::Strict => 1,
        RecoveryPolicy::TruncateTail => 2,
    }
}

const fn manifest_policy_hash_byte(policy: ManifestPolicy) -> u8 {
    match policy {
        ManifestPolicy::None => 1,
        ManifestPolicy::Embedded => 2,
    }
}

const fn field_presence_hash_byte(presence: FieldPresence) -> u8 {
    match presence {
        FieldPresence::Required => 1,
        FieldPresence::Defaulted => 2,
    }
}

fn hash_compression_policy(hash: &mut Fnv1a64, policy: CompressionPolicy) {
    match policy {
        CompressionPolicy::None => hash.write_u8(0),
        CompressionPolicy::VariableBlocks(compression) => {
            hash.write_u8(1);
            hash_variable_compression(hash, compression);
        }
    }
}

fn hash_variable_compression(hash: &mut Fnv1a64, compression: VariableCompression) {
    hash.write_u8(compression_algorithm_hash_byte(compression.algorithm));
    hash.write_u8(compression_level_hash_byte(compression.level));
    if let CompressionLevel::Exact(level) = compression.level {
        hash.write_bytes(&level.to_le_bytes());
    }
    hash.write_u8(compression_header_mode_hash_byte(compression.header_mode));
    hash.write_bytes(&compression.min_uncompressed_len.to_le_bytes());
    hash.write_u8(u8::from(compression.only_if_smaller));
    hash.write_bytes(&compression.max_uncompressed_len.to_le_bytes());
}

fn hash_layout_spec(hash: &mut Fnv1a64, layout: LayoutSpec) {
    hash.write_u8(layout_preset_hash_byte(layout.preset));
    hash.write_bytes(&(layout.parts.len() as u64).to_le_bytes());
    for part in layout.parts {
        hash.write_str(part.name);
        match part.kind {
            LayoutPartKind::FileHeader(header) => {
                hash.write_u8(1);
                hash.write_str(header.name);
                hash_layout_fields(hash, header.fields);
            }
            LayoutPartKind::Segment(segment) => {
                hash.write_u8(2);
                hash_segment_descriptor(hash, segment);
            }
            LayoutPartKind::LeadIn(lead_in) => {
                hash.write_u8(3);
                hash.write_str(lead_in.name);
                hash_layout_fields(hash, lead_in.fields);
            }
            LayoutPartKind::Metadata(metadata) => {
                hash.write_u8(4);
                hash.write_str(metadata.name);
                hash.write_u8(layout_bytes_source_hash_byte(metadata.source));
            }
            LayoutPartKind::RawRegion(raw) => {
                hash.write_u8(5);
                hash.write_str(raw.name);
                hash.write_u8(layout_bytes_source_hash_byte(raw.source));
            }
            LayoutPartKind::Footer(footer) => {
                hash.write_u8(6);
                hash.write_str(footer.name);
                hash_layout_fields(hash, footer.fields);
            }
        }
    }
}

fn layout_spec_to_plan(layout: LayoutSpec) -> LayoutPlan {
    LayoutPlan {
        preset: layout.preset,
        parts: layout
            .parts
            .iter()
            .map(|part| LayoutPlanPartDescriptor {
                name: part.name.to_string(),
                kind: match part.kind {
                    LayoutPartKind::FileHeader(header) => LayoutPlanPartKind::FileHeader(
                        layout_field_group_to_plan(header.name, header.fields),
                    ),
                    LayoutPartKind::Segment(segment) => {
                        LayoutPlanPartKind::Segment(layout_segment_to_plan(segment))
                    }
                    LayoutPartKind::LeadIn(lead_in) => LayoutPlanPartKind::LeadIn(
                        layout_field_group_to_plan(lead_in.name, lead_in.fields),
                    ),
                    LayoutPartKind::Metadata(metadata) => LayoutPlanPartKind::Metadata(
                        layout_region_to_plan(metadata.name, metadata.source),
                    ),
                    LayoutPartKind::RawRegion(raw) => {
                        LayoutPlanPartKind::RawRegion(layout_region_to_plan(raw.name, raw.source))
                    }
                    LayoutPartKind::Footer(footer) => LayoutPlanPartKind::Footer(
                        layout_field_group_to_plan(footer.name, footer.fields),
                    ),
                },
            })
            .collect(),
    }
}

fn layout_segment_to_plan(segment: SegmentDescriptor) -> LayoutPlanSegment {
    LayoutPlanSegment {
        name: segment.name.to_string(),
        repeat: segment.repeat,
        lead_in: layout_field_group_to_plan(segment.lead_in.name, segment.lead_in.fields),
        metadata: layout_region_to_plan(segment.metadata.name, segment.metadata.source),
        raw_region: layout_region_to_plan(segment.raw_region.name, segment.raw_region.source),
        footer: segment
            .footer
            .map(|footer| layout_field_group_to_plan(footer.name, footer.fields)),
    }
}

fn layout_field_group_to_plan(
    name: &'static str,
    fields: &[LayoutFieldDescriptor],
) -> LayoutPlanFieldGroup {
    LayoutPlanFieldGroup {
        name: name.to_string(),
        fields: fields.iter().copied().map(layout_field_to_plan).collect(),
    }
}

fn layout_region_to_plan(name: &'static str, source: LayoutBytesSource) -> LayoutPlanRegion {
    LayoutPlanRegion {
        name: name.to_string(),
        source: match source {
            LayoutBytesSource::Caller => LayoutPlanRegionSource::Caller,
        },
    }
}

fn layout_field_to_plan(field: LayoutFieldDescriptor) -> LayoutPlanField {
    LayoutPlanField {
        name: field.name.to_string(),
        ty: layout_field_type_to_plan(field.ty),
        source: match field.source {
            LayoutFieldSource::LiteralBytes(bytes) => {
                LayoutPlanFieldSource::LiteralBytes(bytes.to_vec())
            }
            LayoutFieldSource::LiteralU64(value) => LayoutPlanFieldSource::LiteralU64(value),
            LayoutFieldSource::LiteralI64(value) => LayoutPlanFieldSource::LiteralI64(value),
            LayoutFieldSource::Caller => LayoutPlanFieldSource::Caller,
            LayoutFieldSource::Finalize(finalize) => LayoutPlanFieldSource::Finalize(finalize),
        },
        endian: field.endian,
    }
}

fn layout_field_type_to_plan(ty: LayoutFieldType) -> LayoutPlanFieldType {
    match ty {
        LayoutFieldType::Bytes { len } => LayoutPlanFieldType::Bytes {
            len: LayoutPlanLen::Fixed(len),
        },
        LayoutFieldType::U8 => LayoutPlanFieldType::U8,
        LayoutFieldType::U16 => LayoutPlanFieldType::U16,
        LayoutFieldType::U32 => LayoutPlanFieldType::U32,
        LayoutFieldType::U64 => LayoutPlanFieldType::U64,
        LayoutFieldType::I64 => LayoutPlanFieldType::I64,
    }
}

fn native_layout_plan(spec: FormatSpec) -> LayoutPlan {
    let footer = if spec.spec_needs_record_footer() {
        Some(LayoutPlanFieldGroup {
            name: "VarveRecordFooter".to_string(),
            fields: crate::native_layout::native_record_footer_plan_fields(),
        })
    } else {
        None
    };

    LayoutPlan {
        preset: LayoutPreset::VarveNative,
        parts: vec![
            LayoutPlanPartDescriptor {
                name: "VarveFileHeader".to_string(),
                kind: LayoutPlanPartKind::FileHeader(LayoutPlanFieldGroup {
                    name: "VarveFileHeader".to_string(),
                    fields: crate::native_layout::native_file_header_plan_fields(spec),
                }),
            },
            LayoutPlanPartDescriptor {
                name: "VarveRecord".to_string(),
                kind: LayoutPlanPartKind::Segment(LayoutPlanSegment {
                    name: "VarveRecord".to_string(),
                    repeat: SegmentRepeat::UntilEof,
                    lead_in: LayoutPlanFieldGroup {
                        name: "VarveRecordHeader".to_string(),
                        fields: crate::native_layout::native_record_header_plan_fields(),
                    },
                    metadata: LayoutPlanRegion {
                        name: "NoMetadata".to_string(),
                        source: LayoutPlanRegionSource::Native("none"),
                    },
                    raw_region: LayoutPlanRegion {
                        name: "Payload".to_string(),
                        source: LayoutPlanRegionSource::Native("record_payload"),
                    },
                    footer,
                }),
            },
        ],
    }
}

fn hash_segment_descriptor(hash: &mut Fnv1a64, segment: SegmentDescriptor) {
    hash.write_str(segment.name);
    hash.write_u8(segment_repeat_hash_byte(segment.repeat));
    hash.write_str(segment.lead_in.name);
    hash_layout_fields(hash, segment.lead_in.fields);
    hash.write_str(segment.metadata.name);
    hash.write_u8(layout_bytes_source_hash_byte(segment.metadata.source));
    hash.write_str(segment.raw_region.name);
    hash.write_u8(layout_bytes_source_hash_byte(segment.raw_region.source));
    match segment.footer {
        Some(footer) => {
            hash.write_u8(1);
            hash.write_str(footer.name);
            hash_layout_fields(hash, footer.fields);
        }
        None => hash.write_u8(0),
    }
}

fn hash_layout_fields(hash: &mut Fnv1a64, fields: &[LayoutFieldDescriptor]) {
    hash.write_bytes(&(fields.len() as u64).to_le_bytes());
    for field in fields {
        hash.write_str(field.name);
        hash.write_u8(layout_field_type_hash_byte(field.ty));
        if let LayoutFieldType::Bytes { len } = field.ty {
            hash.write_bytes(&len.to_le_bytes());
        }
        hash.write_u8(match field.endian {
            Some(endian) => endian.to_byte(),
            None => 0,
        });
        hash_layout_field_source(hash, field.source);
    }
}

fn hash_layout_field_source(hash: &mut Fnv1a64, source: LayoutFieldSource) {
    match source {
        LayoutFieldSource::LiteralBytes(bytes) => {
            hash.write_u8(1);
            hash.write_bytes(&(bytes.len() as u64).to_le_bytes());
            hash.write_bytes(bytes);
        }
        LayoutFieldSource::LiteralU64(value) => {
            hash.write_u8(2);
            hash.write_bytes(&value.to_le_bytes());
        }
        LayoutFieldSource::LiteralI64(value) => {
            hash.write_u8(3);
            hash.write_bytes(&value.to_le_bytes());
        }
        LayoutFieldSource::Caller => hash.write_u8(4),
        LayoutFieldSource::Finalize(finalize) => {
            hash.write_u8(5);
            hash.write_u8(layout_anchor_hash_byte(finalize.target));
            hash.write_u8(layout_anchor_hash_byte(finalize.relative_to));
        }
    }
}

const fn layout_preset_hash_byte(preset: LayoutPreset) -> u8 {
    match preset {
        LayoutPreset::VarveNative => 1,
        LayoutPreset::None => 2,
        LayoutPreset::Custom => 3,
    }
}

const fn segment_repeat_hash_byte(repeat: SegmentRepeat) -> u8 {
    match repeat {
        SegmentRepeat::Once => 1,
        SegmentRepeat::UntilEof => 2,
    }
}

const fn layout_bytes_source_hash_byte(source: LayoutBytesSource) -> u8 {
    match source {
        LayoutBytesSource::Caller => 1,
    }
}

const fn layout_field_type_hash_byte(ty: LayoutFieldType) -> u8 {
    match ty {
        LayoutFieldType::Bytes { .. } => 1,
        LayoutFieldType::U8 => 2,
        LayoutFieldType::U16 => 3,
        LayoutFieldType::U32 => 4,
        LayoutFieldType::U64 => 5,
        LayoutFieldType::I64 => 6,
    }
}

const fn layout_anchor_hash_byte(anchor: LayoutAnchor) -> u8 {
    match anchor {
        LayoutAnchor::SegmentStart => 1,
        LayoutAnchor::AfterLeadIn => 2,
        LayoutAnchor::MetadataStart => 3,
        LayoutAnchor::RawRegionStart => 4,
        LayoutAnchor::SegmentEnd => 5,
        LayoutAnchor::FooterStart => 6,
        LayoutAnchor::FooterEnd => 7,
    }
}

const fn compression_algorithm_hash_byte(algorithm: CompressionAlgorithm) -> u8 {
    match algorithm {
        CompressionAlgorithm::Zstd => 1,
    }
}

const fn compression_level_hash_byte(level: CompressionLevel) -> u8 {
    match level {
        CompressionLevel::Fast => 1,
        CompressionLevel::Default => 2,
        CompressionLevel::Best => 3,
        CompressionLevel::Exact(_) => 4,
    }
}

const fn compression_header_mode_hash_byte(mode: CompressionHeaderMode) -> u8 {
    match mode {
        CompressionHeaderMode::RecordExplicit => 1,
        CompressionHeaderMode::FileExplicit => 2,
        CompressionHeaderMode::FormatContract => 3,
    }
}

impl FormatSpecBuilder {
    pub const fn new() -> Self {
        Self {
            magic: None,
            version: 1,
            endian: Endian::Little,
            schema_hash: 0,
            extension: None,
            index_policy: IndexPolicy::ScanOnOpen,
            commit_policy: CommitPolicy::None,
            integrity_policy: IntegrityPolicy::None,
            recovery_policy: RecoveryPolicy::Strict,
            manifest_policy: ManifestPolicy::None,
            compression_policy: CompressionPolicy::None,
            block_compression: &[],
            blocks: &[],
            matrix_dimensions: &[],
            matrix_commits: &[],
            matrix_blocks: &[],
            matrix_aux: &[],
            block_identities: &[],
            layout: LayoutSpec::varve_native(),
            read_limits: ReadLimits::MISSING,
        }
    }

    pub const fn magic(mut self, magic: &'static [u8]) -> Self {
        self.magic = Some(magic);
        self
    }

    pub const fn version(mut self, version: u16) -> Self {
        self.version = version;
        self
    }

    pub const fn endian(mut self, endian: Endian) -> Self {
        self.endian = endian;
        self
    }

    pub const fn schema_hash(mut self, schema_hash: u64) -> Self {
        self.schema_hash = schema_hash;
        self
    }

    pub const fn extension(mut self, extension: Option<&'static str>) -> Self {
        self.extension = extension;
        self
    }

    pub const fn index_policy(mut self, index_policy: IndexPolicy) -> Self {
        self.index_policy = index_policy;
        self
    }

    pub const fn commit_policy(mut self, commit_policy: CommitPolicy) -> Self {
        self.commit_policy = commit_policy;
        self
    }

    pub const fn integrity_policy(mut self, integrity_policy: IntegrityPolicy) -> Self {
        self.integrity_policy = integrity_policy;
        self
    }

    pub const fn recovery_policy(mut self, recovery_policy: RecoveryPolicy) -> Self {
        self.recovery_policy = recovery_policy;
        self
    }

    pub const fn manifest_policy(mut self, manifest_policy: ManifestPolicy) -> Self {
        self.manifest_policy = manifest_policy;
        self
    }

    pub const fn compression_policy(mut self, compression_policy: CompressionPolicy) -> Self {
        self.compression_policy = compression_policy;
        self
    }

    pub const fn block_compression(
        mut self,
        block_compression: &'static [BlockCompressionDescriptor],
    ) -> Self {
        self.block_compression = block_compression;
        self
    }

    pub const fn blocks(mut self, blocks: &'static [BlockDescriptor]) -> Self {
        self.blocks = blocks;
        self
    }

    pub const fn matrix_spec(
        mut self,
        dimensions: &'static [MatrixDimensionDescriptor],
        commits: &'static [MatrixCommitDescriptor],
        blocks: &'static [MatrixBlockDescriptor],
    ) -> Self {
        self.matrix_dimensions = dimensions;
        self.matrix_commits = commits;
        self.matrix_blocks = blocks;
        self
    }

    pub const fn matrix_aux(mut self, aux: &'static [MatrixAuxDescriptor]) -> Self {
        self.matrix_aux = aux;
        self
    }

    /// See [`FormatSpec::block_identities`].
    pub const fn block_identities(
        mut self,
        block_identities: &'static [(u32, Option<Endian>, bool, u64)],
    ) -> Self {
        self.block_identities = block_identities;
        self
    }

    pub const fn layout(mut self, layout: LayoutSpec) -> Self {
        self.layout = layout;
        self
    }

    pub const fn read_limits(mut self, read_limits: ReadLimits) -> Self {
        self.read_limits = read_limits;
        self
    }

    pub fn build(self) -> Result<FormatSpec> {
        let magic = self
            .magic
            .ok_or(Error::InvalidFormatSpec("missing magic"))?;
        let spec = FormatSpec::new(
            magic,
            self.version,
            self.endian,
            self.schema_hash,
            self.index_policy,
            self.integrity_policy,
            self.recovery_policy,
            self.manifest_policy,
            self.blocks,
        )
        .with_extension(self.extension)
        .with_commit_policy(self.commit_policy)
        .with_compression_policy(self.compression_policy)
        .with_block_compression(self.block_compression)
        .with_matrix_spec(
            self.matrix_dimensions,
            self.matrix_commits,
            self.matrix_blocks,
        )
        .with_matrix_aux(self.matrix_aux)
        .with_block_identities(self.block_identities)
        .with_layout(self.layout)
        .with_read_limits(self.read_limits);
        spec.validate()?;
        Ok(spec)
    }
}

impl Default for FormatSpecBuilder {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod read_limit_tests {
    use super::*;

    #[test]
    fn scalar_meet_never_promotes_missing_to_trust() {
        use ReadLimit::{Finite, Missing, TrustedUnbounded};

        assert_eq!(Finite(9).meet(Finite(4)), Finite(4));
        assert_eq!(Finite(0).meet(TrustedUnbounded), Finite(0));
        assert_eq!(Missing.meet(Finite(7)), Finite(7));
        assert_eq!(Missing.meet(TrustedUnbounded), Missing);
        assert_eq!(TrustedUnbounded.meet(Missing), Missing);
        assert_eq!(TrustedUnbounded.meet(TrustedUnbounded), TrustedUnbounded);
    }

    #[test]
    fn runtime_limits_only_tighten_finite_declarations() {
        let declared = ReadLimits::finite_all(100)
            .with_max_file_len(50)
            .with_max_records(0);
        let runtime = ReadLimits::trusted_unbounded()
            .with_max_file_len(75)
            .with_max_records(10);
        let effective = declared.tighten(runtime);

        assert_eq!(effective.max_file_len, ReadLimit::Finite(50));
        assert_eq!(effective.max_records, ReadLimit::Finite(0));
        assert!(!effective.trusted_api);
    }

    #[test]
    fn missing_fields_resolve_from_standard_policy() {
        let resolved = ReadLimits::missing()
            .with_max_record_payload_len(7)
            .resolve();

        assert_eq!(resolved.max_record_payload_len, ReadLimit::Finite(7));
        assert_eq!(resolved.max_file_len, ReadLimit::Finite(u64::MAX));
        assert_eq!(resolved.max_scan_bytes, ReadLimit::Finite(u64::MAX));
        assert_eq!(resolved.max_records, ReadLimit::Finite(u64::MAX));
        assert_eq!(resolved.max_segments, ReadLimit::Finite(u64::MAX));
        assert_eq!(resolved.max_index_bytes, ReadLimit::Finite(u64::MAX));
        assert_eq!(
            resolved.max_logical_payload_len,
            ReadLimits::STANDARD.max_logical_payload_len
        );
    }

    #[test]
    fn resource_overlay_can_raise_or_lower_format_defaults() {
        let spec = FormatSpec::new(
            b"LIMITS",
            1,
            Endian::Little,
            0,
            IndexPolicy::ScanOnOpen,
            IntegrityPolicy::None,
            RecoveryPolicy::Strict,
            ManifestPolicy::None,
            &[],
        )
        .with_resource_defaults(
            ReadLimits::missing()
                .with_max_record_payload_len(10)
                .with_max_materialized_bytes(100),
        );

        let raised =
            spec.with_resource_limits(ReadLimits::missing().with_max_record_payload_len(20));
        assert_eq!(
            raised.read_limits.max_record_payload_len,
            ReadLimit::Finite(20)
        );
        assert_eq!(
            raised.read_limits.max_materialized_bytes,
            ReadLimit::Finite(100)
        );

        let lowered =
            spec.with_resource_limits(ReadLimits::missing().with_max_record_payload_len(5));
        assert_eq!(
            lowered.read_limits.max_record_payload_len,
            ReadLimit::Finite(5)
        );

        let tightened =
            spec.tighten_read_limits(ReadLimits::missing().with_max_record_payload_len(20));
        assert_eq!(
            tightened.read_limits.max_record_payload_len,
            ReadLimit::Finite(10)
        );
    }

    #[test]
    fn ordinary_and_trusted_resolution_are_distinct() {
        let missing = ReadLimits::missing();
        assert!(matches!(
            missing.require(ReadLimitKey::FileLen),
            Err(Error::MissingResourceLimit { .. })
        ));

        let trusted = ReadLimits::trusted_unbounded();
        assert!(matches!(
            trusted.require(ReadLimitKey::FileLen),
            Err(Error::TrustedUnboundedRequiresExplicitApi { .. })
        ));
        assert_eq!(
            trusted
                .authorize_trusted_api()
                .require(ReadLimitKey::FileLen)
                .unwrap(),
            None
        );

        let bounded = trusted.with_max_file_len(3).authorize_trusted_api();
        assert!(bounded.check(ReadLimitKey::FileLen, 3).is_ok());
        assert!(matches!(
            bounded.check(ReadLimitKey::FileLen, 4),
            Err(Error::LimitExceeded { limit: 3, .. })
        ));
    }

    #[test]
    fn untrusted_preset_is_finite_in_every_aggregate_dimension() {
        let limits = ReadLimits::untrusted();
        for (name, limit) in [
            ("file length", limits.max_file_len),
            ("record count", limits.max_records),
            ("index bytes", limits.max_index_bytes),
            ("scan bytes", limits.max_scan_bytes),
            ("segment count", limits.max_segments),
        ] {
            match limit {
                ReadLimit::Finite(value) => {
                    assert!(value < u64::MAX, "{name} must have a real finite bound");
                }
                other => panic!("{name} must be finite, got {other:?}"),
            }
        }
        assert_eq!(
            limits.max_record_payload_len,
            ReadLimits::STANDARD.max_record_payload_len
        );
        assert_eq!(
            limits.max_materialized_bytes,
            ReadLimits::STANDARD.max_materialized_bytes
        );
    }

    #[test]
    fn read_limits_do_not_change_schema_hash() {
        let base = FormatSpec::new(
            b"LIMITS",
            1,
            Endian::Little,
            0,
            IndexPolicy::ScanOnOpen,
            IntegrityPolicy::None,
            RecoveryPolicy::Strict,
            ManifestPolicy::None,
            &[],
        );
        let bounded = base.with_read_limits(ReadLimits::finite_all(1));
        assert_eq!(base.computed_schema_hash(), bounded.computed_schema_hash());
    }
}
