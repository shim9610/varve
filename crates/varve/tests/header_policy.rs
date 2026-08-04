// §3.2 of the header-extension note: a native file that records the policies it
// was written under.
//
// A native file otherwise says nothing about them. The header `flags` byte is a
// reserved zero; the schema hash is a hash, so the bits are not recoverable,
// and it is not even compared when a format declares no hash — which is the
// default. A spec with the wrong commit policy therefore opens someone else's
// file and misreads it, which is how the 2026-08-02 review reached its second
// defect.

use std::path::{Path, PathBuf};

use varve::{CommitPolicy, Error, FormatSpec, VarveBlock, varve_format};

/// A different commit policy that still needs a record footer, so the container
/// marker cannot tell the two apart and the policy block is the only evidence.
/// The marker already refuses `CommitPolicy::None` here, which is why the
/// 2026-08-02 review's own repro used two footer-bearing policies.
fn footer_bearing_mismatch() -> CommitPolicy {
    CommitPolicy::RecordFooter
}

#[derive(Clone, Debug, PartialEq, VarveBlock)]
#[varve(id = 80, version = 1, kind = "fixed")]
struct Point {
    value: u32,
}

varve_format! {
    pub struct PolicyFormat {
        magic: b"HDRPOLIC";
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
        commit: transaction_marker(on_flush);
        blocks: [Point];
    }
}

fn recording() -> FormatSpec {
    PolicyFormat::spec().with_header_policy_block(true)
}

fn write(spec: FormatSpec, path: &Path) -> varve::Result<()> {
    let mut file = spec.create(path)?;
    for value in 0..8u32 {
        file.push(&Point { value })?;
    }
    file.flush()?;
    Ok(())
}

// Defaults are inert: off writes exactly what it wrote before the option
// existed, and hashes to the same value.
#[test]
fn the_option_is_inert_when_off() -> varve::Result<()> {
    assert!(!PolicyFormat::spec().header_policy_block);
    assert_eq!(
        PolicyFormat::spec().computed_schema_hash(),
        PolicyFormat::spec()
            .with_header_policy_block(false)
            .computed_schema_hash(),
    );
    assert_ne!(
        recording().computed_schema_hash(),
        PolicyFormat::spec().computed_schema_hash(),
        "a file that records its policies is not the file the plain spec describes",
    );

    let plain = temp_path("inert_plain");
    let rebuilt = temp_path("inert_rebuilt");
    write(PolicyFormat::spec(), &plain)?;
    write(
        PolicyFormat::spec().with_header_policy_block(false),
        &rebuilt,
    )?;
    assert_eq!(std::fs::read(&*plain)?, std::fs::read(&*rebuilt)?);
    Ok(())
}

#[test]
fn recording_the_policies_costs_header_bytes_and_nothing_else() -> varve::Result<()> {
    let plain = temp_path("size_plain");
    let recorded = temp_path("size_recorded");
    write(PolicyFormat::spec(), &plain)?;
    write(recording(), &recorded)?;

    let grew = std::fs::metadata(&*recorded)?.len() - std::fs::metadata(&*plain)?.len();
    // magic(4) + len(4) + payload(12). The four-byte extension-length field is
    // already there: this format needs record footers, so its header is
    // `VARVE3` and carries the field whether or not the region is empty.
    assert_eq!(
        grew, 20,
        "the block is a fixed header cost, not a per-record one"
    );

    // And the records read back the same.
    let file = recording().open_readonly(&recorded)?;
    let points = file.blocks::<Point>()?;
    assert_eq!(points.len(), 8);
    assert_eq!(points.get(3)?, Some(Point { value: 3 }));
    Ok(())
}

// The point of the block. A spec whose commit policy differs from the file's
// must be refused, and told which policy differs.
#[test]
fn a_policy_mismatch_is_refused_by_name() -> varve::Result<()> {
    let path = temp_path("mismatch");
    write(recording(), &path)?;

    let wrong = recording().with_commit_policy(footer_bearing_mismatch());
    let error = wrong.open_readonly(&path).expect_err("must refuse");
    match error {
        Error::HeaderPolicyMismatch {
            policy,
            stored,
            declared,
        } => {
            assert_eq!(policy, "commit policy");
            assert_ne!(stored, declared);
        }
        other => panic!("expected a named policy mismatch, got {other:?}"),
    }

    // The spec that wrote it still opens it.
    assert_eq!(
        recording().open_readonly(&path)?.blocks::<Point>()?.len(),
        8
    );
    Ok(())
}

// A spec that does not declare the block must not skip the comparison — that
// would be the mismatch reopening the door it was added to close.
#[test]
fn a_spec_without_the_block_still_honours_a_file_that_has_one() -> varve::Result<()> {
    let path = temp_path("unaware");
    write(recording(), &path)?;

    let unaware_and_wrong = PolicyFormat::spec().with_commit_policy(footer_bearing_mismatch());
    assert!(matches!(
        unaware_and_wrong.open_readonly(&path),
        Err(Error::HeaderPolicyMismatch {
            policy: "commit policy",
            ..
        }),
    ));
    Ok(())
}

// Without the block there is nothing to refuse on: this is the behaviour the
// option exists to change, pinned so the gap is visible rather than assumed.
#[test]
fn without_the_block_a_mismatched_spec_opens_the_file() -> varve::Result<()> {
    let path = temp_path("no_block");
    write(PolicyFormat::spec(), &path)?;
    let wrong = PolicyFormat::spec().with_commit_policy(footer_bearing_mismatch());
    assert!(
        wrong.open_readonly(&path).is_ok(),
        "a file that records nothing cannot refuse anyone",
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
        .join(format!("varve_hdrpol_{name}_{}.vrv", std::process::id()));
    TempPath { path, _dir: dir }
}
