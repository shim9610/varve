//! Every self-contained `varve_format!` example in the published docs, compiled.
//!
//! `doc_claims.rs` reads prose for retracted sentences. This reads code: the
//! DSL blocks in `docs/` are Rust that nothing compiled, so a syntax change, a
//! removed clause, or an example that was never valid in the first place shipped
//! unnoticed.
//!
//! It found two on its first run, both in `format-author-guide.md`'s matrix
//! example and both of which had been published for some time:
//!
//! * `commit: cell_bitmap { .. }` with no closing `;`, which does not parse;
//! * `rfu: [f32; n_wells_max]` with `n_wells_max` declared under `dims` — a
//!   runtime dimension used as a compile-time array length, which does not
//!   resolve.
//!
//! **Scope, stated so the gate is not mistaken for more than it is.** Only the
//! examples whose `varve_format!` block declares its blocks inline are extracted;
//! the `blocks: [Name, ..]` form names structs that live outside the fence and
//! cannot be lifted on its own. Five of the thirteen `varve_format!` blocks in
//! the docs qualify. The extraction is textual and deliberately dumb — it
//! renames the format and rewrites the magic so the examples can coexist, and
//! changes nothing else.

use std::fs;
use std::path::{Path, PathBuf};

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("workspace root")
        .to_path_buf()
}

/// The examples below are lifted verbatim from the files named above each one.
/// When one of these fails to compile, fix the document — not this file — and
/// `docs_still_contain_the_examples_compiled_here` is what stops the copy here
/// from drifting away from the copy a reader sees.
const WELLS_PER_CELL: usize = 96;

mod author_guide_native {
    // docs/format-author-guide.md — the first full declaration.
    varve::varve_format! {
        pub format DocAuthorNative {
            magic: b"DOCAUTH1";
            version: 1;
            limits {
                records: 4_000_000;
                index_bytes: 536_870_912;
                scan_bytes: 8_589_934_592;
                record_payload: 67_108_864;
                logical_payload: 268_435_456;
                materialized_bytes: 1_073_741_824;
            }
            endian: little;
            schema_hash: computed;
            extension: "vrv";
            index: [scan_on_open, checkpoint_on_flush, block_offset_chain, keyed_offset_chain];
            commit: transaction_marker(on_flush);
            manifest: embedded;

            blocks {
                fixed Point(id = 100) {
                    x: u32,
                    y: u32,
                }

                variable User(id = 101, key = [id]) {
                    id: u64,
                    name: String,
                }
            }
        }
    }
}

mod author_guide_matrix {
    use super::WELLS_PER_CELL;
    // docs/format-author-guide.md — the matrix authoring shape.
    varve::varve_format! {
        pub format DocAuthorMatrix {
            magic: b"DOCAUTH2";
            version: 1;
            limits {
                record_payload: 67_108_864;
                materialized_bytes: 268_435_456;
                matrix_dimension: 16_000_000;
                matrix_cells: 16_000_000;
                matrix_bitmap: 64_000_000;
                matrix_crc: 128_000_000;
                matrix_metadata: 268_435_456;
                matrix_slot_region: 8_589_934_592;
                sidecar: 268_435_456;
            }
            schema_hash: computed;

            dims {
                n_scans: u32,
                n_channels: u32,
            }

            commit: cell_bitmap {
                keyspace = [scan, ch];
                categories = [analysis];
            };

            blocks {
                matrix AnalysisCell(id = 10, dims = [scan, ch], category = analysis) {
                    rfu: [f32; WELLS_PER_CELL],
                }
            }
        }
    }
}

/// Every ```` ```rust ```` fence in a document, in order.
///
/// **Fences, not the whole file.** The first version of this checked the raw
/// text and failed on the paragraph in `format-author-guide.md` that *explains*
/// the `[f32; n_wells_max]` mistake by quoting it — the same trap `doc_claims.rs`
/// carves out for review documents. A document has to be able to name the thing
/// it is warning about.
fn rust_fences(text: &str) -> Vec<&str> {
    let mut fences = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find("```rust\n") {
        let body = &rest[start + "```rust\n".len()..];
        let Some(end) = body.find("```") else { break };
        fences.push(&body[..end]);
        rest = &body[end..];
    }
    fences
}

/// The two properties above are only worth anything if the documents still say
/// what this file compiles. Checked on the two fragments that actually broke: a
/// `cell_bitmap` block must close with `};`, and no example may use a
/// `dims`-declared name as an array length.
#[test]
fn docs_still_contain_the_examples_compiled_here() {
    let root = repo_root();
    for name in ["format-author-guide.md", "quickstart.md"] {
        let path = root.join("docs").join(name);
        let text = fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {name}: {e}"));
        for fence in rust_fences(&text) {
            for (index, _) in fence.match_indices("commit: cell_bitmap {") {
                let rest = &fence[index..];
                let close = rest.find("\n        }").unwrap_or_else(|| {
                    panic!("{name}: unterminated cell_bitmap block");
                });
                assert!(
                    rest[close..].starts_with("\n        };"),
                    "{name}: `commit: cell_bitmap {{ .. }}` must close with `;` or the \
                     example does not parse"
                );
            }
            assert!(
                !fence.contains("[f32; n_"),
                "{name}: a `dims`-declared runtime dimension is not a compile-time \
                 array length; the example will not compile"
            );
        }
    }
}
