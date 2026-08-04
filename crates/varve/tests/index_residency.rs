// H1 — per-block resident opt-out (index-residency spec §4.2, verified per §5).
//
// The resident index holds one 104-byte entry per record for the life of the
// handle: §1's law, resident cost proportional to record count rather than to
// bytes. A non-resident block breaks it for its own records. They are written,
// sequenced, chained and recoverable exactly as before; they are simply not
// mirrored in memory, at write *and* at open.

use std::path::{Path, PathBuf};

use varve::{BlockResidencyDescriptor, FormatSpec, VarveBlock, varve_format};

#[derive(Clone, Debug, PartialEq, VarveBlock)]
#[varve(id = 70, version = 1, kind = "fixed")]
struct Line {
    value: u32,
}

#[derive(Clone, Debug, PartialEq, VarveBlock)]
#[varve(id = 71, version = 1, kind = "variable")]
struct Summary {
    #[varve(field_id = 1)]
    first_line: u64,
}

varve_format! {
    pub struct ResidencyFormat {
        magic: b"RESIDENT";
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
        integrity: crc32;
        index: block_offset_chain;
        commit: transaction_marker(on_flush);
        blocks: [Line, Summary];
    }
}

const LINES_NON_RESIDENT: &[BlockResidencyDescriptor] = &[BlockResidencyDescriptor {
    block_id: 70,
    resident: false,
}];

fn lean_spec() -> FormatSpec {
    ResidencyFormat::spec().with_block_residency(LINES_NON_RESIDENT)
}

fn full_spec() -> FormatSpec {
    ResidencyFormat::spec()
}

fn write(spec: FormatSpec, path: &Path, lines: u32, per_summary: u32) -> varve::Result<()> {
    let mut file = spec.create(path)?;
    for value in 0..lines {
        file.push(&Line { value })?;
        if (value + 1) % per_summary == 0 {
            file.push(&Summary {
                first_line: u64::from(value + 1 - per_summary),
            })?;
        }
    }
    file.flush()?;
    Ok(())
}

// §5.1 — inertness. Residency changes no byte of the file.
#[test]
fn residency_changes_no_byte_and_no_hash() -> varve::Result<()> {
    assert_eq!(
        lean_spec().computed_schema_hash(),
        full_spec().computed_schema_hash(),
    );
    let lean = temp_path("inert_lean");
    let full = temp_path("inert_full");
    write(lean_spec(), &lean, 64, 8)?;
    write(full_spec(), &full, 64, 8)?;
    assert_eq!(std::fs::read(&*lean)?, std::fs::read(&*full)?);
    Ok(())
}

// §5.2 — the mechanism itself, and the half that was missing the first time.
// Skipping `index.install` keeps records out of the *writer's* index; if open
// rebuilds an entry for every record on disk, a reopened handle is fully
// resident again and nothing was achieved.
#[test]
fn residency_is_bounded_by_the_resident_blocks_alone() -> varve::Result<()> {
    for (lines, per_summary) in [(64u32, 8u32), (512, 8), (4096, 8)] {
        let path = temp_path("bounded");
        write(lean_spec(), &path, lines, per_summary)?;
        let summaries = lines / per_summary;

        // A commit marker per flush is resident too, and there is one.
        let file = lean_spec().open_readonly(&path)?;
        let lines_resident = file
            .index_entries()
            .iter()
            .filter(|entry| entry.block_id == 70)
            .count();
        assert_eq!(lines_resident, 0, "no line record may be resident");
        assert_eq!(
            file.index_entries()
                .iter()
                .filter(|entry| entry.block_id == 71)
                .count() as u32,
            summaries,
        );
        assert!(
            (file.index_entries().len() as u32) < summaries + 8,
            "resident entries must track summaries ({summaries}), not lines ({lines})",
        );

        // The same bytes through a retaining spec hold every record.
        let every = full_spec().open_readonly(&path)?;
        assert_eq!(
            every
                .index_entries()
                .iter()
                .filter(|entry| entry.block_id == 70)
                .count() as u32,
            lines,
        );
    }
    Ok(())
}

// §5.3 — sequences are file-global and gapless, so a non-resident append must
// still publish one, and a reopen must continue past it rather than re-issuing
// numbers already on disk. That is the hazard that stopped the first attempt:
// `SequenceState::from_index` took the maximum of the *resident* index.
#[test]
fn a_reopen_does_not_reissue_a_non_resident_sequence() -> varve::Result<()> {
    let path = temp_path("no_reissue");
    write(lean_spec(), &path, 24, 8)?;
    {
        let mut file = lean_spec().open(&path)?;
        for value in 24..32u32 {
            file.push(&Line { value })?;
        }
        file.flush()?;
    }
    let every = full_spec().open_readonly(&path)?;
    // Not vacuous: if the appended region were unreachable the assertions below
    // would hold over the surviving prefix alone.
    assert_eq!(
        every
            .index_entries()
            .iter()
            .filter(|entry| entry.block_id == 70)
            .count(),
        32,
        "the appended non-resident records must be committed and visible",
    );
    let sequences: Vec<u64> = every
        .index_entries()
        .iter()
        .map(|entry| entry.sequence)
        .collect();
    let mut sorted = sequences.clone();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(
        sorted.len(),
        sequences.len(),
        "a reopen re-issued a sequence that was already on disk",
    );
    assert_eq!(sequences, (0..sequences.len() as u64).collect::<Vec<_>>());
    Ok(())
}

// §5.3, second half, and H2. The chain is the only way back, so its entry point
// must survive a reopen for a block the resident index cannot see.
#[test]
fn the_chain_reaches_every_non_resident_record_exactly_once() -> varve::Result<()> {
    let lines = 40u32;
    let path = temp_path("chain_walk");
    write(lean_spec(), &path, lines, 8)?;

    let lean = lean_spec().open_readonly(&path)?;
    let tail = lean
        .block_tail_offset(70)
        .expect("a non-resident block still publishes its tail after a reopen");

    let every = full_spec().open_readonly(&path)?;
    let by_offset: std::collections::HashMap<u64, &varve::RecordIndexEntry> = every
        .index_entries()
        .iter()
        .map(|entry| (entry.record_offset, entry))
        .collect();

    let mut visited = Vec::new();
    let mut next = Some(tail);
    while let Some(offset) = next {
        let entry = by_offset.get(&offset).expect("chain names a real record");
        assert_eq!(entry.block_id, 70, "the chain must stay within its block");
        visited.push(entry.sequence);
        next = entry.prev_same_block_offset;
    }
    assert_eq!(visited.len() as u32, lines, "exactly once, and all of them");
    visited.sort_unstable();
    visited.dedup();
    assert_eq!(visited.len() as u32, lines);
    Ok(())
}

// §5.6 — the lost capabilities fail loudly.
#[test]
fn a_non_resident_block_refuses_the_resident_readers() -> varve::Result<()> {
    let path = temp_path("loud_refusal");
    write(lean_spec(), &path, 32, 8)?;
    let file = lean_spec().open_readonly(&path)?;
    assert!(matches!(
        file.blocks::<Line>(),
        Err(varve::Error::BlockNotResident { block_id: 70 }),
    ));
    assert_eq!(file.blocks::<Summary>()?.len(), 4);
    Ok(())
}

// A generation rewrite rebuilds the file from the resident index, which no
// longer mirrors every record - the non-resident records would be dropped from
// the published file. Refused at the format level, because the loss is of
// blocks the caller did not name.
#[test]
fn a_generation_rewrite_is_refused_for_a_non_resident_format() -> varve::Result<()> {
    let path = temp_path("rewrite_refused");
    write(lean_spec(), &path, 16, 8)?;
    let mut file = lean_spec().open(&path)?;
    assert!(matches!(
        file.replace_block(0, &Summary { first_line: 99 }),
        Err(varve::Error::InvalidFormatSpec(_)),
    ));
    // Every record is still there.
    drop(file);
    let every = full_spec().open_readonly(&path)?;
    assert_eq!(every.blocks::<Line>()?.len(), 16);
    Ok(())
}

#[test]
fn a_non_resident_block_without_a_chain_is_refused() {
    let no_chain = ResidencyFormat::spec()
        .with_index_policy(varve::IndexPolicy::ScanOnOpen)
        .with_block_residency(LINES_NON_RESIDENT);
    assert!(matches!(
        no_chain.validate(),
        Err(varve::Error::InvalidFormatSpec(
            "a non-resident block requires block_offset_chain to stay reachable"
        )),
    ));
    assert!(lean_spec().validate().is_ok());
}

#[test]
fn residency_declared_for_an_undeclared_block_is_refused() {
    const STRAY: &[BlockResidencyDescriptor] = &[BlockResidencyDescriptor {
        block_id: 999,
        resident: false,
    }];
    assert!(matches!(
        ResidencyFormat::spec()
            .with_block_residency(STRAY)
            .validate(),
        Err(varve::Error::InvalidFormatSpec(
            "residency declared for a block the format does not declare"
        )),
    ));
}

// Appending to a non-resident block is the whole point, so the write path must
// not share the read path's refusal, and the records must survive a reopen.
#[test]
fn a_non_resident_block_still_appends_and_survives_a_reopen() -> varve::Result<()> {
    let path = temp_path("append_and_reopen");
    write(lean_spec(), &path, 24, 8)?;
    {
        let mut file = lean_spec().open(&path)?;
        for value in 24..48u32 {
            file.push(&Line { value })?;
        }
        file.flush()?;
    }
    let every = full_spec().open_readonly(&path)?;
    let read = every.blocks::<Line>()?;
    assert_eq!(read.len(), 48);
    for value in 0..48u32 {
        assert_eq!(read.get(value as usize)?, Some(Line { value }));
    }
    Ok(())
}

// Under a marker policy the marker is always the last record and always
// carries the highest sequence, so the resident maximum happens to bound the
// file. Without markers there is nothing resident behind the last line, and the
// high-water mark has to come from records the index cannot see. This is the
// hazard that stopped the first H1 attempt.
#[test]
fn a_markerless_reopen_does_not_reissue_a_non_resident_sequence() -> varve::Result<()> {
    let markerless = ResidencyFormat::spec()
        .with_commit_policy(varve::CommitPolicy::None)
        .with_block_residency(LINES_NON_RESIDENT);
    let full_markerless = ResidencyFormat::spec().with_commit_policy(varve::CommitPolicy::None);

    let path = temp_path("markerless_reissue");
    {
        // Ends on a line: nothing resident sits behind it.
        let mut file = markerless.create(&path)?;
        for value in 0..12u32 {
            file.push(&Line { value })?;
        }
        file.flush()?;
    }
    {
        let mut file = markerless.open(&path)?;
        for value in 12..20u32 {
            file.push(&Line { value })?;
        }
        file.flush()?;
    }

    let every = full_markerless.open_readonly(&path)?;
    assert_eq!(
        every
            .index_entries()
            .iter()
            .filter(|entry| entry.block_id == 70)
            .count(),
        20,
    );
    let mut sequences: Vec<u64> = every
        .index_entries()
        .iter()
        .map(|entry| entry.sequence)
        .collect();
    let total = sequences.len();
    sequences.sort_unstable();
    sequences.dedup();
    assert_eq!(
        sequences.len(),
        total,
        "the reopen re-issued a sequence already on disk",
    );
    Ok(())
}

struct TempPath {
    path: PathBuf,
    _dir: tempfile::TempDir,
}

impl std::ops::Deref for TempPath {
    type Target = PathBuf;

    fn deref(&self) -> &PathBuf {
        &self.path
    }
}

impl AsRef<Path> for TempPath {
    fn as_ref(&self) -> &Path {
        &self.path
    }
}

fn temp_path(name: &str) -> TempPath {
    let dir = tempfile::tempdir().expect("create per-test temp directory");
    let path = dir
        .path()
        .join(format!("varve_residency_{name}_{}.vrv", std::process::id()));
    TempPath { path, _dir: dir }
}
