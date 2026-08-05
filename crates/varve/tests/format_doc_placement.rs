//! Rustdoc-placement regression gate for `ReadLimits`' effective-policy
//! accessors.
//!
//! `effective_integrity_verification` shipped carrying the doc comment of a
//! *different* method: the block above it opened with two sentences about
//! [`MatrixMetadataVerification`], the type it never mentions in its body, and
//! `effective_matrix_metadata_verification` — the function those sentences
//! describe — carried no rustdoc at all. Nothing failed, because
//! `cargo doc` is happy to publish a summary that is about another function.
//!
//! This test reads the source text, not the compiled crate, for that reason: a
//! "it compiles" or "cargo doc succeeds" check passes both before and after the
//! move and proves nothing.

use std::fs;
use std::path::{Path, PathBuf};

fn format_rs() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("crates/varve has a workspace root two levels up")
        .join("crates/varve-core/src/format.rs")
}

/// The contiguous run of `///` lines immediately preceding `signature`.
///
/// Returns the empty string when the line above the signature is not a doc
/// comment — which is exactly the state the defect left
/// `effective_matrix_metadata_verification` in.
fn doc_block_above(text: &str, signature: &str) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let at = lines
        .iter()
        .position(|line| line.trim_start().starts_with(signature))
        .unwrap_or_else(|| panic!("`{signature}` not found in format.rs"));
    let mut start = at;
    while start > 0 {
        let candidate = lines[start - 1].trim_start();
        if candidate.starts_with("///") {
            start -= 1;
        } else {
            break;
        }
    }
    lines[start..at].join("\n")
}

#[test]
fn the_effective_policy_accessors_carry_their_own_rustdoc() {
    let path = format_rs();
    let text = fs::read_to_string(&path).unwrap_or_else(|err| panic!("{}: {err}", path.display()));

    let integrity = doc_block_above(&text, "pub const fn effective_integrity_verification");
    assert!(
        !integrity.contains("MatrixMetadataVerification"),
        "the rustdoc on `effective_integrity_verification` describes matrix \
         metadata verification, which is a different method:\n{integrity}"
    );
    assert!(
        integrity.contains("IntegrityVerification::DEFAULT"),
        "`effective_integrity_verification` lost its own summary:\n{integrity}"
    );

    let matrix = doc_block_above(&text, "pub const fn effective_matrix_metadata_verification");
    assert!(
        !matrix.trim().is_empty(),
        "`effective_matrix_metadata_verification` has no rustdoc at all; \
         deleting the misplaced sentences is not the fix, moving them is"
    );
    assert!(
        matrix.contains("MatrixMetadataVerification::DEFAULT"),
        "`effective_matrix_metadata_verification`'s rustdoc does not say what \
         `Missing` resolves to:\n{matrix}"
    );
}
