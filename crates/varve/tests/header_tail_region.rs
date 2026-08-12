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
//! A table at a fixed offset is always present and always "last", so its
//! position proves nothing about which state of the file it describes. It earns
//! that in two bounded steps: the commit marker it names must frame as one, and
//! walking forward from it must land exactly on the end of the file. Most of
//! what is pinned here aims at the second step, because it is the one a
//! signature check alone does not give you.
//!
//! What this file pins, in order: the option is inert when off; turning it on
//! changes the schema hash, because it moves every record offset in the file;
//! the region's length is fixed by the declaration alone; a commit warms
//! exactly one slot; a byte past the commit point, a table naming an older
//! marker, a `commit_offset` naming a data record, and a tail naming another
//! block's record are each refused; a torn slot is skipped and the other one
//! answers; a writer resumed from the header appends a chain the scan agrees
//! with; and the refusals.
//!
//! Not pinned here: a *real* torn write. `fault_point` aborts the child process
//! and the page cache survives that, so the harness can produce "the write did
//! not happen" but never "the write half happened". The tears here are hand
//! byte-patches, which is the only instrument in the tree that expresses it.

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
const SLOT_HEADER_LEN: usize = 2 + 2 + 4 + 4 + 8 + 8;
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
fn the_region_starts_cold_and_a_commit_warms_exactly_one_slot() -> varve::Result<()> {
    let directory = tempfile::tempdir()?;
    let capacity = declared_blocks() + RESERVED_ENTRIES;
    let slot_len = SLOT_HEADER_LEN + capacity * ENTRY_LEN + SLOT_CRC_LEN;

    // A file that has never reached a commit point. `flush` writes a marker
    // only when there is uncommitted work, so a writer that pushed nothing
    // leaves the region exactly as create wrote it.
    let cold_path = directory.path().join("cold.varve");
    drop(on_spec().create(&cold_path)?);
    let cold = std::fs::read(&cold_path)?;
    let cold_at = find_region(&cold).expect("the region is in the header");
    for slot in 0..SLOTS {
        let start = cold_at + BLOCK_FRAMING_LEN + slot * slot_len;
        let fields = slot_fields(&cold, start, slot_len);
        assert_eq!(fields.version, 1);
        assert_eq!(fields.flags, 0, "no flag is set on a cold slot");
        assert_eq!(fields.capacity as usize, capacity);
        assert_eq!(
            fields.count, 0,
            "slot {slot} claims a tail before anything has written one",
        );
        assert_eq!(fields.generation, 0);
        assert_eq!(fields.commit_offset, 0);
        assert!(fields.checksum_matches, "slot {slot}'s checksum covers it");
    }

    // And a file that has committed. Exactly one slot is warm, because a slot
    // records the file as of one commit point and only the newest one can still
    // describe the file that is there.
    let warm_path = directory.path().join("warm.varve");
    write_samples(on_spec(), &warm_path, 200)?;
    let warm = std::fs::read(&warm_path)?;
    let warm_at = find_region(&warm).expect("the region is in the header");
    let slots: Vec<SlotFields> = (0..SLOTS)
        .map(|slot| {
            slot_fields(
                &warm,
                warm_at + BLOCK_FRAMING_LEN + slot * slot_len,
                slot_len,
            )
        })
        .collect();
    for (index, fields) in slots.iter().enumerate() {
        assert!(
            fields.checksum_matches,
            "slot {index} must checksum whether it is the newest or not",
        );
    }
    let newest = slots
        .iter()
        .max_by_key(|fields| fields.generation)
        .expect("two slots");
    assert!(newest.generation > 0, "a commit advances the generation");
    assert!(newest.count > 0, "a commit writes a table");
    assert!(
        (newest.count as usize) <= capacity,
        "a table never claims more than the capacity fixed at create",
    );
    assert_eq!(newest.flags, 0, "the table fit, so no overflow flag");
    // The one field that makes the table checkable: it must name a real offset
    // inside the file, not zero.
    assert!(
        newest.commit_offset > 0 && (newest.commit_offset as usize) < warm.len(),
        "commit_offset must name a record of this file",
    );
    Ok(())
}

struct SlotFields {
    version: u16,
    flags: u16,
    capacity: u32,
    count: u32,
    generation: u64,
    commit_offset: u64,
    checksum_matches: bool,
}

/// Decodes one slot straight out of the file's bytes.
///
/// Field offsets are written out rather than derived from the writer's
/// constants, for the reason the geometry above is: a test that shares the
/// writer's arithmetic agrees with a change to it, including a wrong one.
fn slot_fields(bytes: &[u8], start: usize, slot_len: usize) -> SlotFields {
    let u16_at =
        |at: usize| u16::from_le_bytes(bytes[start + at..start + at + 2].try_into().unwrap());
    let u32_at =
        |at: usize| u32::from_le_bytes(bytes[start + at..start + at + 4].try_into().unwrap());
    let u64_at =
        |at: usize| u64::from_le_bytes(bytes[start + at..start + at + 8].try_into().unwrap());
    let body_end = start + slot_len - SLOT_CRC_LEN;
    SlotFields {
        version: u16_at(0),
        flags: u16_at(2),
        capacity: u32_at(4),
        count: u32_at(8),
        generation: u64_at(12),
        commit_offset: u64_at(20),
        checksum_matches: u32::from_le_bytes(
            bytes[body_end..body_end + SLOT_CRC_LEN].try_into().unwrap(),
        ) == crc32(&bytes[start..body_end]),
    }
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

/// The published layout plan must describe the header the writer actually
/// emits, region included.
///
/// `FormatSpec::effective_layout` is where a tool outside this crate learns
/// where the file header ends and the first record begins, and
/// `schema_debug_dump` prints the same plan. The plan derived its extension
/// length from a function that knew only about the compression block, so for a
/// `header_tails` format it omitted the `extensions` field entirely and put the
/// end of the header inside the region — a wrong answer from a public accessor,
/// contradicted by the `file_header_len` sitting beside it in the same returned
/// struct, which is read from the real file.
///
/// The assertion is against the FILE, not against a restatement of the plan's
/// own arithmetic: the previous guard compared the plan with itself and passed.
#[test]
fn the_published_layout_plan_describes_the_header_the_writer_writes() -> varve::Result<()> {
    use varve::{LayoutPlanFieldSource, LayoutPlanFieldType, LayoutPlanLen, LayoutPlanPartKind};

    let directory = tempfile::tempdir()?;
    let path = directory.path().join("plan.varve");
    write_samples(on_spec(), &path, 40)?;
    let bytes = std::fs::read(&path)?;

    let plan = on_spec().effective_layout();
    let LayoutPlanPartKind::FileHeader(header) = &plan.parts[0].kind else {
        panic!("the native preset publishes a file header part");
    };

    let extensions = header
        .fields
        .iter()
        .find(|field| field.name == "extensions")
        .expect("a format declaring the region has an extension field in its plan");
    assert_eq!(
        extensions.ty,
        LayoutPlanFieldType::Bytes {
            len: LayoutPlanLen::Fixed(expected_region_len() as u64)
        },
    );
    assert!(matches!(
        extensions.source,
        LayoutPlanFieldSource::Native("file_header_extension_region"),
    ));

    // And the sum of the plan's fields is where the first record starts. The
    // region's declared length comes off the file, so this compares the plan
    // against the bytes rather than against the constants above.
    let offset = find_region(&bytes).expect("the region is in the header");
    let declared = u32::from_le_bytes(bytes[offset + 4..offset + 8].try_into().unwrap()) as usize;
    let header_end = offset + BLOCK_FRAMING_LEN + declared;
    let planned: u64 = header
        .fields
        .iter()
        .map(|field| match field.ty {
            LayoutPlanFieldType::U8 => 1,
            LayoutPlanFieldType::U16 => 2,
            LayoutPlanFieldType::U32 => 4,
            LayoutPlanFieldType::U64 => 8,
            LayoutPlanFieldType::Bytes {
                len: LayoutPlanLen::Fixed(len),
            } => len,
            ref other => panic!("unexpected field type in the native file header: {other:?}"),
        })
        .sum();
    assert_eq!(
        planned, header_end as u64,
        "the plan's header length must be where the file's header actually ends",
    );
    Ok(())
}

/// The route exists and it is the one taken, and it does not buy speed by
/// being wrong.
#[test]
fn a_lazy_open_reads_its_tails_from_the_header() -> varve::Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("resume.varve");
    write_samples(on_spec(), &path, 400)?;

    let (lazy, source) = varve::VarveFile::open_readonly_lazy_with_report(on_spec(), &path)?;
    assert_eq!(source, varve::LazyOpenSource::HeaderTails);

    let scanned = varve::VarveFile::open_readonly(on_spec(), &path)?;
    for block in [Sample::ID, Note::ID] {
        assert_eq!(
            lazy.block_tail_offset(block),
            scanned.block_tail_offset(block),
            "block {block}'s tail must be the one the scan finds",
        );
        assert!(lazy.block_tail_offset(block).is_some());
    }
    Ok(())
}

/// One byte past the last commit point and the table is no longer adopted.
///
/// This is the check the whole design turns on. The table sits at a fixed
/// offset, so its position proves nothing about which state of the file it
/// describes; what proves it is walking forward from the commit marker the
/// table names and landing exactly on the end of the file. A byte past that end
/// means something happened after the commit the table describes, and the table
/// has to stop being believed.
#[test]
fn a_byte_past_the_commit_point_takes_the_table_out_of_use() -> varve::Result<()> {
    use std::io::Write;

    let directory = tempfile::tempdir()?;
    let path = directory.path().join("appended.varve");
    write_samples(on_spec(), &path, 200)?;

    let (before, source) = varve::VarveFile::open_readonly_lazy_with_report(on_spec(), &path)?;
    assert_eq!(source, varve::LazyOpenSource::HeaderTails);
    let expected = before.block_tail_offset(Sample::ID);
    drop(before);

    std::fs::OpenOptions::new()
        .append(true)
        .open(&path)?
        .write_all(&[0u8])?;

    let (after, source) = varve::VarveFile::open_readonly_lazy_with_report(on_spec(), &path)?;
    assert_eq!(
        source,
        varve::LazyOpenSource::FullScan,
        "the forward walk no longer reaches the end of the file",
    );
    // Falling back is not the same as being broken: the scan answers, and it
    // answers what the table would have.
    assert_eq!(after.block_tail_offset(Sample::ID), expected);
    Ok(())
}

/// A table naming an *older* commit marker is refused, and that marker is a
/// real one.
///
/// This is the case a signature check alone cannot catch. The offset the stale
/// table carries still points at a genuine commit marker with a valid checksum,
/// so "is there a commit marker here" passes. What fails is "and nothing was
/// committed after it". Without that second half the reader would resume at the
/// older boundary and the next append would write a `prev_same_block_offset`
/// that skips every record in between — a chain that is silently short, with
/// every link's checksum intact.
#[test]
fn a_table_naming_an_older_commit_marker_is_refused() -> varve::Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("stale.varve");

    // Commit once, and keep the region exactly as that commit left it.
    let mut file = on_spec().create(&path)?;
    for value in 0..40 {
        file.push(&Sample { value })?;
    }
    file.flush()?;
    drop(file);
    let stale_region = {
        let bytes = std::fs::read(&path)?;
        let at = find_region(&bytes).expect("the region is in the header");
        bytes[at..at + expected_region_len()].to_vec()
    };

    // Commit again, several times, so the tails genuinely move.
    let mut file = varve::VarveWriter::open(on_spec(), &path)?;
    for value in 40..200 {
        file.push(&Sample { value })?;
        if value % 20 == 19 {
            file.flush()?;
        }
    }
    file.flush()?;
    drop(file);

    let current = varve::VarveFile::open_readonly(on_spec(), &path)?;
    let current_tail = current.block_tail_offset(Sample::ID).expect("a tail");
    drop(current);

    // Put the first commit's table back over the current file's region.
    let mut bytes = std::fs::read(&path)?;
    let at = find_region(&bytes).expect("the region is in the header");
    bytes[at..at + expected_region_len()].copy_from_slice(&stale_region);
    std::fs::write(&path, &bytes)?;

    // The premise: the offset it carries is a REAL commit marker, so a check
    // that only looked for a signature there would accept it.
    let slot_len =
        SLOT_HEADER_LEN + (declared_blocks() + RESERVED_ENTRIES) * ENTRY_LEN + SLOT_CRC_LEN;
    let stale = (0..SLOTS)
        .map(|slot| slot_fields(&bytes, at + BLOCK_FRAMING_LEN + slot * slot_len, slot_len))
        .filter(|fields| fields.checksum_matches && fields.count > 0)
        .max_by_key(|fields| fields.generation)
        .expect("the first commit left a warm slot");
    assert!(stale.commit_offset > 0);
    assert!(
        stale.commit_offset < current_tail,
        "the stale table names a marker from before the later commits",
    );

    let (file, source) = varve::VarveFile::open_readonly_lazy_with_report(on_spec(), &path)?;
    assert_eq!(
        source,
        varve::LazyOpenSource::FullScan,
        "a table naming an older commit marker must not be adopted",
    );
    assert_eq!(
        file.block_tail_offset(Sample::ID),
        Some(current_tail),
        "and the fallback answers with the file's real tail",
    );
    Ok(())
}

/// The two options answer the same question and are declared apart.
#[test]
fn the_region_and_the_open_digest_are_not_declared_together() {
    let spec = on_spec().with_index_policy(on_spec().index_policy.with_open_digest_on_flush(true));
    let error = spec.validate().expect_err("two answers, one question");
    assert!(
        matches!(
            error,
            varve::Error::InvalidFormatSpec(
                "header_tails and open_digest_on_flush are two answers to the same question; declare one"
            )
        ),
        "{error:?}",
    );
}

/// Rewrites **every warm slot** in `bytes` through `mutate`, fixing checksums.
///
/// Every warm slot, not just the newest, and the reason is a real property of
/// the format rather than test convenience: a commit point that appends nothing
/// still closes, so two consecutive slots can name the *same* commit marker and
/// both corroborate. Mutating one leaves the other adoptable, and a test that
/// did so would pass while proving nothing. (That fallback is the one thing the
/// second slot actually buys.)
fn rewrite_warm_slots(bytes: &mut [u8], at: usize, mut mutate: impl FnMut(&mut [u8])) {
    let slot_len =
        SLOT_HEADER_LEN + (declared_blocks() + RESERVED_ENTRIES) * ENTRY_LEN + SLOT_CRC_LEN;
    let mut rewritten = 0;
    for slot in 0..SLOTS {
        let start = at + BLOCK_FRAMING_LEN + slot * slot_len;
        let fields = slot_fields(bytes, start, slot_len);
        if !fields.checksum_matches || fields.count == 0 {
            continue;
        }
        let body_end = start + slot_len - SLOT_CRC_LEN;
        mutate(&mut bytes[start..body_end]);
        // Recomputed so the mutation is not caught by the wrong check: these
        // tests are about what the *corroboration* rejects, and a slot that
        // fails its CRC never reaches it.
        let checksum = crc32(&bytes[start..body_end]).to_le_bytes();
        bytes[body_end..body_end + SLOT_CRC_LEN].copy_from_slice(&checksum);
        rewritten += 1;
    }
    assert!(rewritten > 0, "the file must carry a warm slot to rewrite");
}

/// `commit_offset` must name a *commit marker*, not merely a record that
/// happens to end where the file does.
///
/// The forward walk alone does not catch this. A file whose last record is an
/// uncommitted append ends exactly at that record, so a table pointing there
/// walks zero records and lands on the end of the file — the check that catches
/// a stale table passes. What would follow is a reader treating an uncommitted
/// record's end as the commit boundary.
#[test]
fn commit_offset_must_name_a_commit_marker() -> varve::Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("uncommitted.varve");
    write_samples(on_spec(), &path, 100)?;

    // One append with no flush. `Drop` writes no commit marker, so the file now
    // ends with an uncommitted data record.
    let mut writer = varve::VarveWriter::open(on_spec(), &path)?;
    // `push_info`, not `push`: the offset has to come from the append itself.
    // A read-only scan reports the newest *committed* record of the block, so
    // it would name the record before the marker and the forward walk would
    // reject on the marker instead — which is the other check.
    let uncommitted = writer.push_info(&Sample { value: 4242 })?.record_offset;
    drop(writer);
    assert_eq!(
        uncommitted
            + varve::VarveFile::open_readonly(on_spec(), &path)?
                .index_entries()
                .last()
                .map(|_| 0)
                .unwrap_or(0),
        uncommitted,
    );

    let mut bytes = std::fs::read(&path)?;
    let at = find_region(&bytes).expect("the region is in the header");
    rewrite_warm_slots(&mut bytes, at, |body| {
        body[20..28].copy_from_slice(&uncommitted.to_le_bytes());
    });
    std::fs::write(&path, &bytes)?;

    let (_, source) = varve::VarveFile::open_readonly_lazy_with_report(on_spec(), &path)?;
    assert_eq!(
        source,
        varve::LazyOpenSource::FullScan,
        "a data record is not a commit point, however conveniently it is placed",
    );
    Ok(())
}

/// Every offset the table hands out is re-framed against the file.
///
/// A table can name a real, well-framed record of the right file and still be
/// wrong about which block it belongs to. Both corroboration steps pass — the
/// commit marker is genuine and nothing was committed after it — so this is the
/// only check that separates a table that is merely *plausible* from one that
/// is *true*.
#[test]
fn a_tail_naming_another_blocks_record_is_refused() -> varve::Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("crossed.varve");
    write_samples(on_spec(), &path, 100)?;

    let scanned = varve::VarveFile::open_readonly(on_spec(), &path)?;
    let note_tail = scanned.block_tail_offset(Note::ID).expect("a Note tail");
    let sample_tail = scanned
        .block_tail_offset(Sample::ID)
        .expect("a Sample tail");
    assert_ne!(note_tail, sample_tail);
    drop(scanned);

    let mut bytes = std::fs::read(&path)?;
    let at = find_region(&bytes).expect("the region is in the header");
    rewrite_warm_slots(&mut bytes, at, |body| {
        // Point `Sample`'s entry at `Note`'s newest record. Ids are ascending
        // and `Sample::ID` is the lower of the two, so it is the first entry
        // whose id matches.
        let count = u32::from_le_bytes(body[8..12].try_into().unwrap()) as usize;
        for entry in 0..count {
            let field = SLOT_HEADER_LEN + entry * ENTRY_LEN;
            if u32::from_le_bytes(body[field..field + 4].try_into().unwrap()) == Sample::ID {
                body[field + 4..field + 12].copy_from_slice(&note_tail.to_le_bytes());
                return;
            }
        }
        panic!("the table must carry a Sample tail");
    });
    std::fs::write(&path, &bytes)?;

    let (file, source) = varve::VarveFile::open_readonly_lazy_with_report(on_spec(), &path)?;
    assert_eq!(
        source,
        varve::LazyOpenSource::FullScan,
        "a tail naming another block's record must not be adopted",
    );
    assert_eq!(file.block_tail_offset(Sample::ID), Some(sample_tail));
    Ok(())
}

/// Flips a byte inside one slot's table and does **not** fix its checksum.
fn tear_slot(bytes: &mut [u8], at: usize, slot: usize) {
    let slot_len =
        SLOT_HEADER_LEN + (declared_blocks() + RESERVED_ENTRIES) * ENTRY_LEN + SLOT_CRC_LEN;
    let start = at + BLOCK_FRAMING_LEN + slot * slot_len;
    bytes[start + SLOT_HEADER_LEN + 4] ^= 0xFF;
}

/// The crash matrix, as far as this tree can express it.
///
/// The region is overwritten in place at a constant length, so its failure mode
/// is a slot that is half the previous generation and half the next — framed
/// perfectly, and naming records that are not there. Only the per-slot checksum
/// separates that from a good write, which is why a crc32 integrity policy is
/// mandatory for the option.
///
/// **The harness cannot produce a real torn write**: `fault_point` aborts the
/// child process and the page cache survives a process kill, so a `write()`
/// that returned is already visible to the next open. It can produce "the write
/// did not happen", never "the write half happened". A hand byte-patch is the
/// only instrument in the tree that expresses it, so that is what this is.
#[test]
fn a_torn_slot_is_skipped_and_the_other_one_answers() -> varve::Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("torn.varve");
    write_samples(on_spec(), &path, 100)?;

    let scanned = varve::VarveFile::open_readonly(on_spec(), &path)?;
    let expected = scanned.block_tail_offset(Sample::ID);
    drop(scanned);

    let clean = std::fs::read(&path)?;
    let at = find_region(&clean).expect("the region is in the header");

    // One slot torn. The final `flush` closes a commit point that appends
    // nothing, so both slots name the same commit marker and the survivor is
    // still true of this file — the one thing the second slot buys.
    for torn in 0..SLOTS {
        let mut bytes = clean.clone();
        tear_slot(&mut bytes, at, torn);
        let one = directory.path().join(format!("torn-{torn}.varve"));
        std::fs::write(&one, &bytes)?;
        let (file, source) = varve::VarveFile::open_readonly_lazy_with_report(on_spec(), &one)?;
        assert_eq!(
            source,
            varve::LazyOpenSource::HeaderTails,
            "slot {torn} torn, the other still describes this file",
        );
        assert_eq!(file.block_tail_offset(Sample::ID), expected);
    }

    // Both torn. There is nothing left to corroborate, and the answer is the
    // scan — slower, never wrong.
    let mut bytes = clean.clone();
    for torn in 0..SLOTS {
        tear_slot(&mut bytes, at, torn);
    }
    let both = directory.path().join("torn-both.varve");
    std::fs::write(&both, &bytes)?;
    let (file, source) = varve::VarveFile::open_readonly_lazy_with_report(on_spec(), &both)?;
    assert_eq!(source, varve::LazyOpenSource::FullScan);
    assert_eq!(file.block_tail_offset(Sample::ID), expected);

    // A tear that lands in the slot's *unused* entries. Nothing else in the
    // corroboration would notice — the table is still true — so this is the
    // case that says the checksum covers the whole slot rather than only the
    // part a reader happens to look at. A torn write lands where it lands.
    let mut bytes = clean.clone();
    let slot_len =
        SLOT_HEADER_LEN + (declared_blocks() + RESERVED_ENTRIES) * ENTRY_LEN + SLOT_CRC_LEN;
    for slot in 0..SLOTS {
        let start = at + BLOCK_FRAMING_LEN + slot * slot_len;
        bytes[start + slot_len - SLOT_CRC_LEN - 1] ^= 0xFF;
    }
    let padded = directory.path().join("torn-padding.varve");
    std::fs::write(&padded, &bytes)?;
    let (file, source) = varve::VarveFile::open_readonly_lazy_with_report(on_spec(), &padded)?;
    assert_eq!(
        source,
        varve::LazyOpenSource::FullScan,
        "the checksum covers every byte of the slot, not only the live entries",
    );
    assert_eq!(file.block_tail_offset(Sample::ID), expected);
    Ok(())
}

/// A writer resumed from the header table appends a chain the scan agrees with.
///
/// The point of the tails is to seed `prev_same_block_offset` for the next
/// append. A resume that adopted a wrong tail would produce a chain that is
/// silently short — every link checksums, and only a full walk notices. So this
/// resumes lazily, appends, and then compares the chain against the scan.
#[test]
fn a_writer_resumed_from_the_header_appends_a_chain_the_scan_agrees_with() -> varve::Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("resumed.varve");
    write_samples(on_spec(), &path, 120)?;

    let (mut writer, source) = varve::VarveWriter::open_lazy_with_report(on_spec(), &path)?;
    assert_eq!(source, varve::LazyOpenSource::HeaderTails);
    for value in 1000..1010 {
        writer.push(&Sample { value })?;
    }
    writer.flush()?;
    drop(writer);

    // The chain, walked from the tail the scan finds, must reach every Sample.
    let scanned = varve::VarveFile::open_readonly(on_spec(), &path)?;
    let samples = scanned
        .index_entries()
        .iter()
        .filter(|entry| entry.block_id == Sample::ID)
        .count();
    let walked = scanned.block_chain(Sample::ID)?.count();
    assert_eq!(
        walked, samples,
        "the chain must reach every Sample record, not stop at the resume point",
    );
    assert_eq!(samples, 130);
    Ok(())
}

/// A format that rewrites part of its header at every commit can still be
/// memory-mapped.
///
/// The mapping starts at the append log rather than at byte 0, so the bytes a
/// commit rewrites are outside every reference `MmapPayloads` constructs. What
/// this test can check is that the windows are still right; that no `&[u8]`
/// covers the header is an aliasing property, and ASan is the tool for it.
#[cfg(feature = "mmap")]
#[test]
fn a_header_tails_file_can_still_be_mapped() -> varve::Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("mapped.varve");
    write_samples(on_spec(), &path, 60)?;

    let file = varve::VarveFile::open_readonly(on_spec(), &path)?;
    // SAFETY: nothing else holds this file open for writing for the duration.
    let mapped = unsafe { file.mmap_payloads()? };
    let mut compared = 0;
    for entry in file.index_entries() {
        if entry.block_id != Sample::ID {
            continue;
        }
        assert_eq!(
            mapped.payload_window(&entry)?,
            entry.read_payload(&path)?,
            "the mapped window must be the payload the indexed read returns",
        );
        compared += 1;
    }
    assert_eq!(compared, 60);
    Ok(())
}

/// What the route costs at open, in records framed.
///
/// The number that matters is that it does not move with the file. A scan
/// frames every record; this frames the commit marker, whatever follows it, and
/// one record per distinct block id — all three bounded by the *declaration*.
#[cfg(feature = "scalable-fault-injection")]
#[test]
fn the_open_cost_does_not_move_with_the_file() -> varve::Result<()> {
    fn framed<T>(body: impl FnOnce() -> T) -> (T, u64) {
        let before = varve::VarveFile::records_framed();
        let value = body();
        (value, varve::VarveFile::records_framed() - before)
    }

    let directory = tempfile::tempdir()?;
    let mut costs = Vec::new();
    for records in [200u32, 2_000] {
        let path = directory.path().join(format!("cost-{records}.varve"));
        write_samples(on_spec(), &path, records)?;

        let (_, scan) = framed(|| {
            varve::VarveFile::open_readonly(on_spec(), &path).expect("the scanning open")
        });
        let ((_, source), lazy) = framed(|| {
            varve::VarveFile::open_readonly_lazy_with_report(on_spec(), &path)
                .expect("the header route")
        });
        assert_eq!(source, varve::LazyOpenSource::HeaderTails);
        assert!(
            scan >= u64::from(records),
            "the scan frames every record: {scan} for {records}",
        );
        costs.push(lazy);
    }

    assert_eq!(
        costs[0], costs[1],
        "a ten-fold larger file must frame the same number of records: {costs:?}",
    );
    // Pinned, so a change that starts framing more has to say so here: the
    // commit marker, plus one per distinct block id in the table.
    assert_eq!(
        costs[0], 4,
        "marker + Sample + Note + the marker's own tail"
    );
    Ok(())
}
