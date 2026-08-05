// F-16/F-17 — what byte range a read-only handle may look at.
//
// `open_readonly` used to bind its snapshot to the last entry of the *resident*
// index (`validated_snapshot_len`). The resident index is filtered — a
// non-resident block's records are on disk and not in it, which `open_locked`'s
// own comment says out loud — so under a markerless commit policy that ends on
// a non-resident block, the snapshot stopped before every record of that block.
// `block_chain` is the published way to reach a non-resident block and it walks
// through the snapshot, so the whole chain became unreachable through a
// read-only handle. Not a missing tail: none of it.
//
// The fix carries the physical end of the scan on `ScannedIndex`, beside the
// three other values that already exist because the index is filtered. This
// file pins the three things that value must be, because any one of them alone
// admits a wrong implementation:
//
//   1. it must reach past the last resident record (the defect), and
//   2. it must NOT be `file.metadata()?.len()` — under a marker policy the
//      uncommitted tail a writer open deletes must stay invisible here, and
//   3. a torn trailing record must still leave it at the end of the last
//      COMPLETE record.
//
// Plus a no-regression guard: with no `block_residency` declared, the value is
// the one the old code computed, because nothing is filtered.

use std::path::{Path, PathBuf};

use varve::{BlockResidencyDescriptor, CommitPolicy, FormatSpec, VarveBlock, varve_format};

#[derive(Clone, Debug, PartialEq, VarveBlock)]
#[varve(id = 70, version = 1, kind = "fixed")]
struct Line {
    value: u32,
}

varve_format! {
    pub struct ExtentFormat {
        magic: b"RDEXTENT";
        version: 1;
        limits {
            file_len: 1_073_741_824;
            records: 1_000_000;
            index_bytes: 134_217_728;
            scan_bytes: 1_073_741_824;
            record_payload: 16_777_216;
            logical_payload: 67_108_864;
            materialized_bytes: 268_435_456;
            segments: 1_000_000;
            matrix_dimension: 1_000_000;
            matrix_cells: 1_000_000;
            matrix_bitmap: 8_000_000;
            matrix_crc: 16_000_000;
            matrix_metadata: 33_554_432;
            matrix_slot_region: 1_073_741_824;
            sidecar: 33_554_432;
            mmap: 1_073_741_824;
        }
        endian: little;
        integrity: crc32;
        index: block_offset_chain;
        commit: transaction_marker(on_flush);
        blocks: [Line];
    }
}

const LINES_NON_RESIDENT: &[BlockResidencyDescriptor] = &[BlockResidencyDescriptor {
    block_id: 70,
    resident: false,
}];

/// The exact affected configuration: at least one block declared
/// `resident: false`, and a commit policy that leaves no resident record after
/// the last non-resident one. `CommitPolicy::None` guarantees the second
/// whenever the file ends on a non-resident block, and it is what the
/// `varve_format!` default (`CommitChoice::None`) lands on when `commit:` is
/// omitted.
fn markerless_lean() -> FormatSpec {
    ExtentFormat::spec()
        .with_commit_policy(CommitPolicy::None)
        .with_block_residency(LINES_NON_RESIDENT)
}

fn write_lines(spec: FormatSpec, path: &Path, lines: u32) -> varve::Result<()> {
    let mut file = spec.create(path)?;
    for value in 0..lines {
        file.push(&Line { value })?;
    }
    file.flush()?;
    Ok(())
}

// (1) The defect itself. Under a markerless policy that ends on a non-resident
// block, a read-only handle must reach the whole chain.
//
// Before the fix this failed on the FIRST offset the walk produced, with
// `snapshot range is out of bounds` — 0 of 12, not a missing tail.
#[test]
fn a_markerless_read_only_handle_reaches_every_non_resident_record() -> varve::Result<()> {
    const LINES: u32 = 12;
    let path = temp_path("markerless_chain");
    write_lines(markerless_lean(), &path, LINES)?;

    let file = markerless_lean().open_readonly(&path)?;
    let mut walked = Vec::new();
    for step in file.block_chain(Line::ID)? {
        let entry = step?;
        walked.push(file.read_block_at::<Line>(entry.record_offset)?);
    }
    walked.reverse();
    assert_eq!(
        walked,
        (0..LINES).map(|value| Line { value }).collect::<Vec<_>>(),
        "the read-only snapshot must cover every record the chain names",
    );

    // Not a longer prefix — the whole append log. A markerless format writes
    // nothing after the last record, so its end is the physical end here.
    let physical = std::fs::metadata(&*path)?.len();
    let newest = file
        .block_chain(Line::ID)?
        .next()
        .expect("at least one link")?;
    assert_eq!(newest.physical_end(), physical);
    Ok(())
}

// (2) The assertion that kills the `file.metadata()?.len()` shortcut, which
// would pass (1) perfectly.
//
// Under a marker policy the records a writer appended past the last commit
// marker are uncommitted: `truncate_uncommitted_tail_if_needed` deletes them on
// the next read-write open, so a read-only handle must not expose them either.
// They are on disk while this test runs — the file is physically longer than
// the snapshot, which is the whole point.
#[test]
fn an_uncommitted_tail_stays_outside_the_read_only_snapshot() -> varve::Result<()> {
    let spec = ExtentFormat::spec().with_block_residency(LINES_NON_RESIDENT);
    let path = temp_path("uncommitted_tail");
    {
        let mut file = spec.create(&path)?;
        for value in 0..8u32 {
            file.push(&Line { value })?;
        }
        file.flush()?;
    }
    let committed_physical = std::fs::metadata(&*path)?.len();
    {
        // Appended, never flushed: no marker covers these.
        let mut file = spec.open(&path)?;
        for value in 8..12u32 {
            file.push(&Line { value })?;
        }
    }
    let physical = std::fs::metadata(&*path)?.len();
    assert!(
        physical > committed_physical,
        "the uncommitted records must really be on disk for this to test anything",
    );

    let file = spec.open_readonly(&path)?;
    let visible = file.block_chain(Line::ID)?.count();
    assert_eq!(
        visible, 8,
        "a read-only handle must not expose records a writer open would delete",
    );
    // The first uncommitted record starts where the committed bytes end, and
    // must be outside the snapshot.
    assert!(
        file.read_block_at::<Line>(committed_physical).is_err(),
        "the snapshot must stop at the commit boundary, not at the physical end",
    );
    Ok(())
}

// (3) A torn trailing record. The value comes from the scan, and the scan stops
// at the last record it could frame, so a half-written record must not be
// inside the snapshot even though its bytes are in the file.
#[test]
fn a_torn_trailing_record_leaves_the_snapshot_at_the_last_complete_one() -> varve::Result<()> {
    const LINES: u32 = 12;
    // `CommitPolicy::RecordFooter` tolerates a partial record at any offset, so
    // the scan reports a recoverable tail rather than refusing the file. The
    // residency declaration is what makes the length come from the scan.
    let spec = ExtentFormat::spec()
        .with_commit_policy(CommitPolicy::RecordFooter)
        .with_block_residency(LINES_NON_RESIDENT);
    let path = temp_path("torn_tail");
    write_lines(spec, &path, LINES)?;

    let last_offset = {
        let file = spec.open_readonly(&path)?;
        file.block_chain(Line::ID)?
            .next()
            .expect("at least one link")?
            .record_offset
    };
    let physical = std::fs::metadata(&*path)?.len();
    let torn = std::fs::OpenOptions::new().write(true).open(&*path)?;
    torn.set_len(physical - 4)?;
    torn.sync_all()?;
    drop(torn);

    let file = spec.open_readonly(&path)?;
    let mut walked = 0usize;
    for step in file.block_chain(Line::ID)? {
        let entry = step?;
        file.read_block_at::<Line>(entry.record_offset)?;
        walked += 1;
    }
    assert_eq!(
        walked,
        LINES as usize - 1,
        "the walk must reach every complete record and stop there",
    );
    assert!(
        file.read_block_at::<Line>(last_offset).is_err(),
        "the torn record must be outside the snapshot even though its bytes are in the file",
    );
    Ok(())
}

// No-regression guard. With no `block_residency` declared nothing is filtered,
// so the last scanned record IS the last index entry and the computed length is
// the one `validated_snapshot_len` returned. Asserted against that formula
// directly — `entries.last()?.physical_end()` — on both commit policies, and
// with an uncommitted tail present under the marker policy so the two
// candidates (last entry vs. physical length) actually differ.
#[test]
fn without_residency_the_snapshot_still_ends_at_the_last_index_entry() -> varve::Result<()> {
    for (name, spec) in [
        (
            "markerless",
            ExtentFormat::spec().with_commit_policy(CommitPolicy::None),
        ),
        ("marker", ExtentFormat::spec()),
    ] {
        let path = temp_path(name);
        write_lines(spec, &path, 12)?;
        {
            // Uncommitted under the marker policy; under `CommitPolicy::None`
            // these are committed on arrival and stay visible.
            let mut file = spec.open(&path)?;
            for value in 12..16u32 {
                file.push(&Line { value })?;
            }
        }

        let file = spec.open_readonly(&path)?;
        let entries = file.index_entries();
        let expected_len = entries.last().expect("a record").physical_end();

        // Every entry the index carries is inside the snapshot, and the byte
        // range stops exactly at the last one: reading a record framed at
        // `expected_len` fails, and there is either nothing there or an
        // uncommitted record that must stay invisible.
        for entry in entries {
            if entry.block_id == Line::ID {
                file.read_block_at::<Line>(entry.record_offset)?;
            }
        }
        assert!(
            file.read_block_at::<Line>(expected_len).is_err(),
            "{name}: nothing past the last index entry may be readable",
        );

        let physical = std::fs::metadata(&*path)?.len();
        match name {
            "marker" => assert!(
                physical > expected_len,
                "marker: the uncommitted tail must really be on disk",
            ),
            _ => assert_eq!(
                physical, expected_len,
                "markerless: every record is committed, so the two agree",
            ),
        }
    }
    Ok(())
}

struct TempPath {
    path: PathBuf,
    _dir: tempfile::TempDir,
}

impl std::ops::Deref for TempPath {
    type Target = Path;

    fn deref(&self) -> &Self::Target {
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
        .join(format!("varve_extent_{name}_{}.vrv", std::process::id()));
    TempPath { path, _dir: dir }
}
