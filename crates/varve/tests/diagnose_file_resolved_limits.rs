//! `diagnose_file` must read a file under the same resolved policy an ordinary
//! open uses.
//!
//! The defect this pins: the file phase of `diagnose_file` charged every record
//! against the format's *declared* `ReadLimits` and never resolved them, while
//! `VarveFile::open_readonly` resolves internally and the handle it hands back
//! carries the resolved spec. A format that declares no `limits { }` block --
//! the quickstart's own `AppFormat` -- therefore reported
//! `file.record.payload_invalid` ("missing finite read limit for record payload
//! length") for every record of a perfectly healthy file, and `passed()` was
//! false, while `open_reader` and `self_test` on that same format succeeded.
//!
//! Both directions are pinned here, because the cheap way to silence the first
//! is to stop enforcing anything: a healthy file under an undeclared-limits
//! format passes, *and* a file that genuinely exceeds a selected ceiling is
//! still reported by the same record loop.

use std::fs::remove_file;
use std::path::{Path, PathBuf};

use varve::{DiagnosticSeverity, ReadLimits, diagnose_file, varve_format};

varve_format! {
    pub format UndeclaredLimitsFormat {
        magic: b"UDLM";
        version: 1;
        endian: little;
        schema_hash: computed;
        manifest: embedded;
        index: [scan_on_open, block_offset_chain, keyed_offset_chain];

        blocks {
            fixed Point(id = 1) {
                x: u32,
                y: u32,
            }
        }
    }
}

varve_format! {
    pub format DeclaredTinyLimitFormat {
        magic: b"DTLM";
        version: 1;
        limits {
            materialized_bytes: 8;
        }
        endian: little;
        schema_hash: computed;
        manifest: embedded;
        index: [scan_on_open, block_offset_chain, keyed_offset_chain];

        blocks {
            fixed Sample(id = 1) {
                a: u32,
                b: u32,
            }
        }
    }
}

fn temp_path(name: &str) -> PathBuf {
    let mut path = std::env::temp_dir();
    path.push(format!(
        "varve_diagnose_resolved_{name}_{}.varve",
        std::process::id()
    ));
    path
}

fn cleanup(path: &Path) {
    let _ = remove_file(path);
    let mut lock = path.as_os_str().to_owned();
    lock.push(".lock");
    let _ = remove_file(PathBuf::from(lock));
}

#[test]
fn diagnose_file_resolves_limits_like_an_ordinary_open() {
    let path = temp_path("undeclared");
    cleanup(&path);

    {
        let mut writer = UndeclaredLimitsFormat::create_writer(&path).expect("create writer");
        for i in 0..3u32 {
            writer.push_point(&Point { x: i, y: i * 2 }).expect("push");
        }
        writer.flush().expect("flush");
    }

    // The premise: the ordinary read entrypoint resolves and succeeds, so a
    // diagnostic that disagrees with it is reporting on the wrong policy.
    assert!(
        UndeclaredLimitsFormat::open_reader(&path).is_ok(),
        "open_reader must succeed on a format that declares no limits block"
    );

    let report = UndeclaredLimitsFormat::diagnose_file(&path);
    let errors: Vec<_> = report
        .items
        .iter()
        .filter(|item| item.severity == DiagnosticSeverity::Error)
        .collect();
    assert!(
        errors.is_empty(),
        "a healthy file under an undeclared-limits format must produce no Error \
         diagnostics, got {errors:#?}"
    );
    assert!(
        !report
            .items
            .iter()
            .any(|item| item.code == "file.record.payload_invalid"),
        "no record of a healthy file is payload-invalid: {report:#?}"
    );
    assert!(report.passed(), "{report:#?}");

    // The other direction, on the same healthy bytes: a ceiling the file really
    // does exceed is still reported by the record loop. `Finite(8)` survives
    // resolution (a resolved policy takes the caller's value where it has one),
    // so a "fix" that swapped the declared policy for `ReadLimits::STANDARD`
    // outright would drop this and fail here.
    let tightened = UndeclaredLimitsFormat::spec()
        .tighten_read_limits(ReadLimits::missing().with_max_materialized_bytes(8));
    let tight_report = diagnose_file(tightened, &path);
    let exceeded = tight_report
        .items
        .iter()
        .find(|item| item.code == "file.record.payload_invalid")
        .unwrap_or_else(|| {
            panic!("a genuine ceiling violation must still be reported: {tight_report:#?}")
        });
    assert_eq!(exceeded.severity, DiagnosticSeverity::Error);
    assert!(
        exceeded.message.contains("materialized bytes"),
        "{}",
        exceeded.message
    );
    assert!(!tight_report.passed(), "{tight_report:#?}");

    cleanup(&path);
}

#[test]
fn diagnose_file_still_enforces_a_format_declared_ceiling() {
    let path = temp_path("declared");
    cleanup(&path);

    {
        let mut writer = DeclaredTinyLimitFormat::create_writer(&path).expect("create writer");
        writer.push_sample(&Sample { a: 1, b: 2 }).expect("push");
        writer.push_sample(&Sample { a: 3, b: 4 }).expect("push");
        writer.flush().expect("flush");
    }

    // `materialized_bytes: 8` is declared by the format itself and is below what
    // the file needs, so the record loop must still fail. Resolution fills the
    // ceilings the format left unset; it does not discard the ones it set.
    let report = DeclaredTinyLimitFormat::diagnose_file(&path);
    let exceeded = report
        .items
        .iter()
        .find(|item| item.code == "file.record.payload_invalid")
        .unwrap_or_else(|| panic!("a declared ceiling must still be enforced: {report:#?}"));
    assert_eq!(exceeded.severity, DiagnosticSeverity::Error);
    assert!(
        exceeded.message.contains("materialized bytes"),
        "{}",
        exceeded.message
    );
    assert!(!report.passed(), "{report:#?}");

    cleanup(&path);
}
