//! The header tail region: a commit boundary that a later append cannot hide.
//!
//! The open digest already writes down the three facts an open needs, but it
//! writes them as a *record*, and a record is only usable while it is the last
//! one in the file. Anything after it — a partial write, records from a run that
//! then crashed — hides it, and the open falls back to reading every record.
//! The moment a resume is needed is the moment that is most likely.
//!
//! `index: header_tails` puts the same table at a fixed offset inside the file
//! header, where nothing appended can move it or bury it.
//!
//! What this file pins, in order: the option is inert when off; turning it on
//! changes the schema hash, because it moves every record offset in the file;
//! the region's length is fixed by the declaration alone; it starts cold and
//! claims nothing; its *contents* may differ from what the spec would write
//! while its framing may not; and the two refusals.
//!
//! Not pinned here, because nothing writes a warm table yet: that the update
//! lands before the commit marker, and the crash matrix over the two slots.

// The fixture declares `integrity: crc32`, so without the `integrity` feature
// it is refused at create with `IntegrityFeatureDisabled`.
#![cfg(feature = "integrity")]

use varve::{FormatSpec, VarveBlock, varve_format};

#[derive(Clone, Debug, PartialEq, VarveBlock)]
#[varve(id = 90, version = 1, kind = "fixed")]
struct Sample {
    value: u32,
}

#[derive(Clone, Debug, PartialEq, VarveBlock)]
#[varve(id = 91, version = 1, kind = "variable")]
struct Note {
    #[varve(field_id = 1)]
    body: String,
}

varve_format! {
    pub struct TailFormat {
        magic: b"VHDRTAIL";
        version: 1;
        limits {
            file_len: 8_589_934_592;
            records: 4_000_000;
            index_bytes: 536_870_912;
            scan_bytes: 8_589_934_592;
            record_payload: 67_108_864;
            logical_payload: 268_435_456;
            materialized_bytes: 1_073_741_824;
            segments: 4_000_000;
            matrix_dimension: 16_000_000;
            matrix_cells: 16_000_000;
            matrix_bitmap: 64_000_000;
            matrix_crc: 128_000_000;
            matrix_metadata: 268_435_456;
            matrix_slot_region: 8_589_934_592;
            sidecar: 268_435_456;
            mmap: 8_589_934_592;
        }
        endian: little;
        schema_hash: computed;
        integrity: crc32;
        index: block_offset_chain;
        commit: transaction_marker(on_flush);
        blocks: [Sample, Note];
    }
}

// The same declaration with the region turned on, through the DSL keyword
// rather than the builder, so the macro path is exercised too.
varve_format! {
    pub struct TailFormatOn {
        magic: b"VHDRTAIL";
        version: 1;
        limits {
            file_len: 8_589_934_592;
            records: 4_000_000;
            index_bytes: 536_870_912;
            scan_bytes: 8_589_934_592;
            record_payload: 67_108_864;
            logical_payload: 268_435_456;
            materialized_bytes: 1_073_741_824;
            segments: 4_000_000;
            matrix_dimension: 16_000_000;
            matrix_cells: 16_000_000;
            matrix_bitmap: 64_000_000;
            matrix_crc: 128_000_000;
            matrix_metadata: 268_435_456;
            matrix_slot_region: 8_589_934_592;
            sidecar: 268_435_456;
            mmap: 8_589_934_592;
        }
        endian: little;
        schema_hash: computed;
        integrity: crc32;
        index: header_tails;
        commit: transaction_marker(on_flush);
        blocks: [Sample, Note];
    }
}

/// The region's framing, restated here rather than imported.
///
/// A test that computed this from the same constants the writer uses would
/// agree with any change to them, including a wrong one. These are the numbers
/// the format is, and a change to the layout has to change them here too.
const SLOTS: usize = 2;
const RESERVED_ENTRIES: usize = 8;
const SLOT_HEADER_LEN: usize = 2 + 2 + 4 + 4 + 8;
const ENTRY_LEN: usize = 4 + 8;
const SLOT_CRC_LEN: usize = 4;
const BLOCK_FRAMING_LEN: usize = 4 + 4;

/// CRC-32/ISO-HDLC, written out here rather than taken from `crc32fast`.
///
/// The writer uses that crate, so importing it would make this assertion "the
/// two calls agree" instead of "the bytes on disk carry this checksum".
fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for byte in bytes {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            let mask = 0u32.wrapping_sub(crc & 1);
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
        }
    }
    !crc
}

fn declared_blocks() -> usize {
    TailFormat::spec().blocks.len()
}

fn expected_region_len() -> usize {
    let capacity = declared_blocks() + RESERVED_ENTRIES;
    let slot = SLOT_HEADER_LEN + capacity * ENTRY_LEN + SLOT_CRC_LEN;
    BLOCK_FRAMING_LEN + SLOTS * slot
}

fn off_spec() -> FormatSpec {
    TailFormat::spec()
}

/// The on-spec comes from its own declaration, not from
/// `off_spec().with_index_policy(..)`.
///
/// `with_index_policy` replaces the policy and leaves `schema_hash` as the
/// macro computed it, so a builder-modified spec carries the *unmodified*
/// declaration's hash — and would therefore open the other one's files. The
/// declaration is the supported way to turn this on, and it is the one that
/// makes the hash follow.
fn on_spec() -> FormatSpec {
    TailFormatOn::spec()
}

fn write_samples(spec: FormatSpec, path: &std::path::Path, count: u32) -> varve::Result<()> {
    let mut file = spec.create(path)?;
    for value in 0..count {
        file.push(&Sample { value })?;
        if value % 25 == 24 {
            file.push(&Note {
                body: format!("note {value}"),
            })?;
            file.flush()?;
        }
    }
    file.flush()?;
    Ok(())
}

/// Locates the `VBTT` block inside a file's header extension region.
///
/// Deliberately a byte search over the whole header prefix rather than a
/// re-implementation of the header layout: the point is to find the bytes the
/// writer actually put on disk, not to agree with a model of where they should
/// be.
fn find_region(bytes: &[u8]) -> Option<usize> {
    bytes
        .windows(4)
        .position(|window| window == b"VBTT")
        .filter(|offset| *offset < 512)
}

#[test]
fn the_option_is_inert_when_it_is_off() -> varve::Result<()> {
    let directory = tempfile::tempdir()?;
    let first = directory.path().join("a.varve");
    let second = directory.path().join("b.varve");
    write_samples(off_spec(), &first, 200)?;
    write_samples(off_spec(), &second, 200)?;

    assert!(!off_spec().index_policy.header_tails);
    assert_eq!(std::fs::read(&first)?, std::fs::read(&second)?);
    assert!(
        find_region(&std::fs::read(&first)?).is_none(),
        "a file written with the option off carries no region",
    );

    // The schema hash a declaration without this option computes, pinned. The
    // flag occupies a bit that was zero before it existed, so a spec that
    // leaves it off must hash to exactly what it hashed to then; this is the
    // assertion that catches a change to that.
    assert_eq!(off_spec().computed_schema_hash(), OFF_SCHEMA_HASH);
    Ok(())
}

/// Filled in from the first run and pinned; see the test above.
const OFF_SCHEMA_HASH: u64 = 16_450_116_677_799_716_449;

#[test]
fn turning_it_on_changes_the_schema_hash_and_the_file_it_describes() -> varve::Result<()> {
    let directory = tempfile::tempdir()?;
    let off = directory.path().join("off.varve");
    let on = directory.path().join("on.varve");
    write_samples(off_spec(), &off, 200)?;
    write_samples(on_spec(), &on, 200)?;

    // The region lives in the header, so `append_log_start` and every record
    // offset after it move. A build that ignored the flag would compute a
    // different append log start and fail somewhere unrelated, which is why
    // this is hashed rather than left to the manifest.
    assert_ne!(
        off_spec().computed_schema_hash(),
        on_spec().computed_schema_hash()
    );

    // And the two are not interchangeable on disk.
    assert!(matches!(
        varve::VarveFile::open_readonly(off_spec(), &on),
        Err(varve::Error::SchemaHashMismatch { .. })
    ));
    assert!(matches!(
        varve::VarveFile::open_readonly(on_spec(), &off),
        Err(varve::Error::SchemaHashMismatch { .. })
    ));
    Ok(())
}

#[test]
fn the_dsl_keyword_produces_the_builder_policy() {
    let dsl = TailFormatOn::spec().index_policy;
    assert!(dsl.header_tails);
    // The keyword turns the chain on with it: a tail offset is an entry point
    // to `prev_same_block_offset`, and without it there is nothing to follow.
    assert!(dsl.block_offset_chain);
    assert_eq!(
        dsl,
        off_spec().index_policy.with_header_tails(true),
        "the keyword and the builder must produce the same policy",
    );
}

#[test]
fn the_region_length_is_fixed_by_the_declaration() -> varve::Result<()> {
    let directory = tempfile::tempdir()?;
    let off = directory.path().join("off.varve");
    let on = directory.path().join("on.varve");
    write_samples(off_spec(), &off, 200)?;
    write_samples(on_spec(), &on, 200)?;

    let on_bytes = std::fs::read(&on)?;
    let offset = find_region(&on_bytes).expect("the region is in the header");
    let declared =
        u32::from_le_bytes(on_bytes[offset + 4..offset + 8].try_into().unwrap()) as usize;
    assert_eq!(declared + BLOCK_FRAMING_LEN, expected_region_len());

    // Two files with identical content differ by exactly the region, which is
    // the claim that the region costs a constant and not a rate.
    assert_eq!(
        on_bytes.len() - std::fs::read(&off)?.len(),
        expected_region_len(),
    );
    Ok(())
}

#[test]
fn the_region_starts_cold() -> varve::Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("cold.varve");
    write_samples(on_spec(), &path, 200)?;

    let bytes = std::fs::read(&path)?;
    let offset = find_region(&bytes).expect("the region is in the header");
    let capacity = declared_blocks() + RESERVED_ENTRIES;
    let slot_len = SLOT_HEADER_LEN + capacity * ENTRY_LEN + SLOT_CRC_LEN;

    for slot in 0..SLOTS {
        let start = offset + BLOCK_FRAMING_LEN + slot * slot_len;
        let end = start + slot_len;
        assert_eq!(
            u16::from_le_bytes(bytes[start..start + 2].try_into().unwrap()),
            1
        );
        assert_eq!(
            u16::from_le_bytes(bytes[start + 2..start + 4].try_into().unwrap()),
            0,
            "no flag is set on a cold slot",
        );
        assert_eq!(
            u32::from_le_bytes(bytes[start + 4..start + 8].try_into().unwrap()) as usize,
            capacity,
        );
        assert_eq!(
            u32::from_le_bytes(bytes[start + 8..start + 12].try_into().unwrap()),
            0,
            "slot {slot} claims a tail before anything has written one",
        );
        let recorded = u32::from_le_bytes(bytes[end - SLOT_CRC_LEN..end].try_into().unwrap());
        assert_eq!(
            recorded,
            crc32(&bytes[start..end - SLOT_CRC_LEN]),
            "slot {slot}'s checksum covers the slot",
        );
    }
    Ok(())
}

/// The contents of this block are the file's to change; its framing is not.
///
/// Every other known header block is compared byte for byte at open, which is
/// what makes a header a fixed description. This one cannot be — its whole
/// purpose is to be rewritten as the file grows — so an open five commits later
/// legitimately sees bytes no spec would produce. That relaxation has to be
/// exactly as wide as it needs to be, so this asserts both halves.
#[test]
fn the_region_contents_may_drift_but_its_framing_may_not() -> varve::Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("drift.varve");
    write_samples(on_spec(), &path, 200)?;
    let original = std::fs::read(&path)?;
    let offset = find_region(&original).expect("the region is in the header");

    // A payload byte: accepted.
    let mut drifted = original.clone();
    drifted[offset + BLOCK_FRAMING_LEN + 8] ^= 0xFF;
    let drifted_path = directory.path().join("payload.varve");
    std::fs::write(&drifted_path, &drifted)?;
    varve::VarveFile::open_readonly(on_spec(), &drifted_path)
        .expect("a changed region payload is what this option exists to produce");

    // The declared length: refused. Everything derived from `header_len` —
    // `append_log_start` and so every record offset — rides on this number.
    let mut relengthed = original.clone();
    relengthed[offset + 4] = relengthed[offset + 4].wrapping_add(1);
    let relengthed_path = directory.path().join("length.varve");
    std::fs::write(&relengthed_path, &relengthed)?;
    assert!(
        varve::VarveFile::open_readonly(on_spec(), &relengthed_path).is_err(),
        "a region that changed length must not open",
    );

    // The magic: refused. A block this build does not know is skipped, so this
    // reads as "the region this spec requires is missing".
    let mut remagicked = original.clone();
    remagicked[offset + 3] = b'X';
    let remagicked_path = directory.path().join("magic.varve");
    std::fs::write(&remagicked_path, &remagicked)?;
    assert!(
        varve::VarveFile::open_readonly(on_spec(), &remagicked_path).is_err(),
        "a spec that declares the region must not open a file without it",
    );
    Ok(())
}

#[test]
fn the_region_requires_the_chain_it_hands_out_entry_points_to() {
    let spec = off_spec().with_index_policy(varve::IndexPolicy {
        header_tails: true,
        block_offset_chain: false,
        ..off_spec().index_policy
    });
    let directory = tempfile::tempdir().expect("tempdir");
    let error = spec
        .create(directory.path().join("refused.varve"))
        .expect_err("header_tails without the chain is refused");
    assert!(
        matches!(
            error,
            varve::Error::InvalidFormatSpec("header_tails requires block_offset_chain")
        ),
        "{error:?}",
    );
}

/// The region and a matrix declaration are refused together, deliberately.
///
/// The matrix creation nonce and the matrix layout header sit at fixed offsets
/// *after* the file header, so a header that grew by a tail region moves both.
/// Not every consumer of those two offsets has been walked, so this refuses
/// rather than guesses. Lifting it means establishing that all of them derive
/// from `header_len` and none is a stored constant — and then this test is the
/// one to delete.
mod matrix_refusal {
    use varve::{
        BlockDescriptor, BlockKind, Endian, Error, FormatSpec, IndexPolicy, IntegrityPolicy,
        MatrixBlockDescriptor, MatrixCommitDescriptor, MatrixCommitKind, MatrixDimensionDescriptor,
        ReadLimits, VarveBlock, VarveMatrixBlock,
    };

    #[derive(Clone, Debug, PartialEq, Eq, VarveBlock)]
    #[varve(id = 92, version = 1, kind = "matrix")]
    struct Cell {
        value: u32,
    }

    impl VarveMatrixBlock for Cell {
        const DIMENSIONS: [&'static str; 2] = ["scan", "ch"];
        const CATEGORY: &'static str = "cells";
        const SLOT_STRIDE: u64 = 4;
    }

    static BLOCKS: &[BlockDescriptor] = &[BlockDescriptor {
        id: Cell::ID,
        name: "Cell",
        version: Cell::VERSION,
        kind: BlockKind::Matrix,
        fields: &[],
    }];
    static DIMS: &[MatrixDimensionDescriptor] = &[
        MatrixDimensionDescriptor { name: "scan" },
        MatrixDimensionDescriptor { name: "ch" },
    ];
    static COMMITS: &[MatrixCommitDescriptor] = &[MatrixCommitDescriptor {
        name: Cell::CATEGORY,
        kind: MatrixCommitKind::Cell,
    }];
    static MATRIX_BLOCKS: &[MatrixBlockDescriptor] = &[MatrixBlockDescriptor {
        block_id: Cell::ID,
        dimensions: Cell::DIMENSIONS,
        category: Cell::CATEGORY,
        slot_stride: Cell::SLOT_STRIDE,
    }];

    fn matrix_spec(header_tails: bool) -> FormatSpec {
        FormatSpec::new(
            b"HDRTMTX0",
            1,
            Endian::Little,
            0,
            IndexPolicy::BlockOffsetChain.with_header_tails(header_tails),
            IntegrityPolicy::Crc32,
            varve::RecoveryPolicy::Strict,
            varve::ManifestPolicy::None,
            BLOCKS,
        )
        .with_read_limits(ReadLimits::finite_all(u64::MAX))
        .with_matrix_spec(DIMS, COMMITS, MATRIX_BLOCKS)
    }

    #[test]
    fn a_matrix_format_may_not_declare_the_region() {
        // The same declaration without the region validates, so the refusal
        // below is about the combination and not about the fixture.
        matrix_spec(false)
            .validate()
            .expect("the fixture itself is a valid matrix format");

        let error = matrix_spec(true)
            .validate()
            .expect_err("the region and a matrix declaration are refused together");
        assert!(
            matches!(
                error,
                Error::InvalidFormatSpec(
                    "header_tails is not supported for a format declaring matrix blocks"
                )
            ),
            "{error:?}",
        );
    }
}

/// The region needs a checksum, so it needs the policy that provides one.
///
/// The failure mode this guards against is unlike every other structure's: the
/// region is overwritten in place over a live file, and its slot length never
/// changes, so a torn write leaves a region that frames perfectly and names
/// records that are not where it says. Only the per-slot checksum separates
/// that from a good write.
#[test]
fn the_region_requires_a_crc32_integrity_policy() {
    let spec = off_spec()
        .with_integrity_policy(varve::IntegrityPolicy::None)
        .with_index_policy(off_spec().index_policy.with_header_tails(true));
    let error = spec.validate().expect_err("refused without a checksum");
    assert!(
        matches!(
            error,
            varve::Error::InvalidFormatSpec("header_tails requires a crc32 integrity policy")
        ),
        "{error:?}",
    );
}
