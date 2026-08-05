//! `SidecarPolicy::verify`: the verdict alone, reading only what the policy's
//! own flags consult.
//!
//! `inspect` hashes the whole main file whenever the sidecar exists — before it
//! has looked at `verify_main_len`, at `verify_main_fingerprint`, or at whether
//! `expected` was even supplied — because the report it returns publishes a
//! `SidecarIdentity` with a plain `u64` fingerprint in it. That is a
//! defensible design for a *report*, and it is left exactly as it is; what it
//! is not is a reason that a policy asking for a length comparison should be
//! unable to get one on a main file above the fingerprint scan ceiling.
//!
//! Two things are asserted here, and the second is the one that matters over
//! time:
//!
//! 1. the capability — a length-only policy answers under a ceiling smaller
//!    than the file, and a fingerprinting policy still refuses there;
//! 2. parity — for every row of the status table, `verify` and `inspect` agree.
//!    A `verify` that short-circuits to `Passed` without comparing the length,
//!    or that forgets `SidecarMode::Required`, is caught by a named row rather
//!    than by review.

use std::fs::{remove_file, write};
use std::path::{Path, PathBuf};

use varve::{AdapterCheckStatus, Error, SidecarIdentity, SidecarMode, SidecarPolicy};

struct TempPath {
    path: PathBuf,
    _dir: tempfile::TempDir,
}

impl AsRef<Path> for TempPath {
    fn as_ref(&self) -> &Path {
        &self.path
    }
}

fn temp_path(name: &str, extension: &str) -> TempPath {
    let dir = tempfile::tempdir().expect("create per-test temp directory");
    let path = dir
        .path()
        .join(format!("varve_{name}_{}.{}", std::process::id(), extension));
    TempPath { path, _dir: dir }
}

fn policy(
    mode: SidecarMode,
    verify_main_len: bool,
    verify_main_fingerprint: bool,
) -> SidecarPolicy {
    SidecarPolicy {
        extension: "idx",
        mode,
        verify_main_len,
        verify_main_fingerprint,
    }
}

/// The capability the fingerprint ceiling denied, and the two ways of
/// "restoring" it that would be wrong.
#[test]
fn a_length_only_policy_answers_below_the_fingerprint_ceiling() -> varve::Result<()> {
    let main = temp_path("sidecar_verify_ceiling", "bin");
    write(&main, b"main")?;
    let len_only = policy(SidecarMode::Optional, true, false);
    write(len_only.sidecar_path(&main), b"index")?;
    let expected = SidecarIdentity::from_main_file(&main)?;
    assert_eq!(expected.main_len, 4);
    let both = SidecarPolicy {
        verify_main_fingerprint: true,
        ..len_only
    };

    // (1) The capability: a ceiling of 3 is below the 4-byte main file, and a
    // policy that never asked for a fingerprint gets its answer anyway.
    assert_eq!(
        len_only.verify_with_scan_limit(&main, Some(&expected), 3)?,
        AdapterCheckStatus::Passed
    );
    // ... and it is a real comparison, not an unconditional `Passed`.
    let wrong_len = SidecarIdentity {
        main_len: expected.main_len + 1,
        ..expected
    };
    assert_eq!(
        len_only.verify_with_scan_limit(&main, Some(&wrong_len), 3)?,
        AdapterCheckStatus::Failed
    );

    // (2) A genuinely required hash still honours the ceiling. An
    // implementation that simply never fingerprints passes (1) and fails here.
    assert!(matches!(
        both.verify_with_scan_limit(&main, Some(&expected), 3),
        Err(Error::LimitExceeded {
            resource: "sidecar fingerprint scan bytes",
            actual: 4,
            limit: 3,
        })
    ));

    // (3) `inspect` is deliberately unchanged: it publishes the identity, so it
    // must still refuse. An implementation that "fixed" `inspect` too changes a
    // published behaviour at 0.5.0 and fails here.
    assert!(matches!(
        len_only.inspect_with_scan_limit(&main, Some(&expected), 3),
        Err(Error::LimitExceeded { .. })
    ));
    Ok(())
}

/// With both flags false and an `expected` supplied, `verify` reads nothing at
/// all — provable because the main file is removed from under it while the
/// sidecar stays, a state in which `inspect` cannot even open the file.
#[test]
fn a_policy_that_verifies_nothing_reads_nothing() -> varve::Result<()> {
    let main = temp_path("sidecar_verify_no_flags", "bin");
    write(&main, b"main")?;
    let neither = policy(SidecarMode::Optional, false, false);
    write(neither.sidecar_path(&main), b"index")?;
    let expected = SidecarIdentity::from_main_file(&main)?;
    remove_file(&main.path).expect("remove the main file");

    assert_eq!(
        neither.verify(&main, Some(&expected))?,
        AdapterCheckStatus::Passed
    );
    // The documented divergence, asserted rather than assumed: `inspect` opens
    // the main file whenever the sidecar exists, so it reports the removal.
    assert!(neither.inspect(&main, Some(&expected)).is_err());
    Ok(())
}

/// The parity sweep. Every row of the status table, under a scan ceiling large
/// enough that `inspect` succeeds, so the two are answering the same question.
#[test]
fn verify_and_inspect_agree_on_every_row_of_the_status_table() -> varve::Result<()> {
    let main = temp_path("sidecar_verify_parity", "bin");
    write(&main, b"main file contents")?;
    let truth = SidecarIdentity::from_main_file(&main)?;
    let wrong_len = SidecarIdentity {
        main_len: truth.main_len + 7,
        ..truth
    };
    let wrong_fingerprint = SidecarIdentity {
        main_fingerprint: truth.main_fingerprint ^ 0xffff_ffff,
        ..truth
    };
    let expectations: [(&str, Option<SidecarIdentity>); 4] = [
        ("none", None),
        ("matching", Some(truth)),
        ("length-mismatched", Some(wrong_len)),
        ("fingerprint-mismatched", Some(wrong_fingerprint)),
    ];

    let mut rows = 0usize;
    for mode in [
        SidecarMode::Required,
        SidecarMode::Optional,
        SidecarMode::GeneratedOnFlush,
    ] {
        for verify_len in [false, true] {
            for verify_fingerprint in [false, true] {
                let policy = policy(mode, verify_len, verify_fingerprint);
                let sidecar = policy.sidecar_path(&main);
                for present in [false, true] {
                    if present {
                        write(&sidecar, b"index")?;
                    } else {
                        let _ = remove_file(&sidecar);
                    }
                    for (label, expected) in &expectations {
                        let expected = expected.as_ref();
                        let reported = policy.inspect(&main, expected)?.status;
                        let verdict = policy.verify(&main, expected)?;
                        assert_eq!(
                            verdict, reported,
                            "row disagreed: mode {mode:?}, verify_main_len {verify_len}, \
                             verify_main_fingerprint {verify_fingerprint}, sidecar present \
                             {present}, expected {label}"
                        );
                        rows += 1;
                    }
                }
            }
        }
    }
    assert_eq!(rows, 96, "the sweep collapsed and would pass vacuously");

    // The sweep is only evidence if the table it walks has more than one
    // outcome in it, so name the three that must appear.
    let required = policy(SidecarMode::Required, true, true);
    let _ = remove_file(required.sidecar_path(&main));
    assert_eq!(
        required.verify(&main, Some(&truth))?,
        AdapterCheckStatus::Failed
    );
    let optional = policy(SidecarMode::Optional, true, true);
    assert_eq!(
        optional.verify(&main, Some(&truth))?,
        AdapterCheckStatus::Warning
    );
    write(optional.sidecar_path(&main), b"index")?;
    assert_eq!(
        optional.verify(&main, Some(&truth))?,
        AdapterCheckStatus::Passed
    );
    assert_eq!(
        optional.verify(&main, Some(&wrong_len))?,
        AdapterCheckStatus::Failed
    );
    assert_eq!(
        optional.verify(&main, Some(&wrong_fingerprint))?,
        AdapterCheckStatus::Failed
    );
    Ok(())
}
