//! Source-level gates for round 12's mechanical enforcement.
//!
//! The compile-fail proofs live in `tests/ui` and are driven by
//! `tests/compile.rs`; they cover the two shapes whose enforcement types can be
//! named from outside the crate. Three properties cannot be expressed that way,
//! because the types enforcing them are private to a single module *inside*
//! `varve-core` — and that privacy is exactly the enforcement, so exposing them
//! to prove it would destroy the thing being proved.
//!
//! Those three are gated here instead, by reading the source. A source gate is
//! weaker than a type: it proves the declaration still has the shape, not that
//! every use respects it. It is still strictly better than a checklist row,
//! because CI fails the day someone relaxes the declaration, and relaxing the
//! declaration is the only way to reintroduce the defect. Each assertion says
//! what it is standing in for.
//!
//! Round 12 exists because round 10 ran an "exhaustive" reading sweep over this
//! same class, rewrote 736 lines of `matrix.rs`, declared the class closed, and
//! shipped `compact_page_index` — the same shape, in the same file. Reading
//! does not close these classes. These gates are what replaces the reading.

use std::fs;
use std::path::{Path, PathBuf};

fn crate_source(name: &str) -> String {
    let path: PathBuf = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("varve-core")
        .join("src")
        .join(name);
    let source = fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
    // Normalised so that a gate spanning two lines cannot pass or fail on a
    // checkout's line-ending convention. `writer_permit.rs` is CRLF in this
    // tree and the rest of `varve-core` is LF; a gate that silently stops
    // matching is worse than no gate.
    source.replace("\r\n", "\n")
}

/// The source with every comment line removed.
///
/// Round 15 added in-crate *bypass catalogues* — `#[cfg(test)]` modules whose
/// documentation quotes each forbidden spelling next to the diagnostic rustc
/// emits for it. Those quotations are the point: they are what a future round
/// re-runs. But a gate that counts occurrences of a spelling must not count
/// them, or documenting a bypass would break the gate that forbids it.
fn crate_code(name: &str) -> String {
    crate_source(name)
        .lines()
        .filter(|line| {
            let trimmed = line.trim_start();
            !trimmed.starts_with("//")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// An attribute that compiles its item only under `cfg(test)`.
///
/// `#[cfg(test)]` is the common spelling, but `matrix.rs` and `file.rs` also
/// carry `#[cfg(all(test, feature = "..."))]` items. Round 16's stripper knew
/// only the first, so those counted as shipped code — conservative for the
/// gates, but it made "what ships" mean two different things in one helper.
fn is_test_gate_attribute(trimmed: &str) -> bool {
    trimmed == "#[cfg(test)]"
        || trimmed.starts_with("#[cfg(all(test,")
        || trimmed.starts_with("#[cfg(all(test)")
}

/// The source with every comment line and every test-only item removed: what
/// actually ships.
///
/// The bypass catalogues exercise the legitimate constructors on purpose, so a
/// gate that says "this constructor is called exactly once" has to mean once in
/// the library. Counting the tests would make proving the property break the
/// gate that states it.
///
/// # This is a textual stripper, and it fails loudly rather than quietly
///
/// Re-verification of round 16 named the honest risk: the skip is driven by
/// text, from a line that is exactly `#[cfg(test)]` to the next line that is
/// exactly the same indentation followed by `}`. Anything else — a
/// `#[cfg(test)] use ...;`, a `#[cfg(test)] fn` nested one level in, a
/// `#[cfg(all(test, feature = "..."))]` item, a `let` whose initialiser wraps —
/// either escaped the strip or would have swallowed arbitrary shipped code up
/// to the next same-indent closing brace, silently *narrowing every gate built
/// on it*. Silent narrowing is the one failure mode a gate must not have.
///
/// So every assumption is now checked, and a violation panics naming the file,
/// the line number and the offending text instead of returning a quietly
/// shortened string:
///
/// 1. the item introduced by a test-gate attribute must either open a brace at
///    the attribute's own indentation or end in `;` within
///    `MAX_ITEM_HEADER_LINES` (further attributes and doc comments in between
///    are allowed, because that is how the catalogues are written);
/// 2. every skip that starts must end before the file does.
///
/// `the_shipped_code_stripper_removes_test_modules_and_nothing_else` pins the
/// result: only whole lines are ever dropped, in order, and nothing bearing
/// `#[test]` or a test-gate attribute survives.
fn crate_shipped_code(name: &str) -> String {
    let source = crate_source(name);
    let mut kept: Vec<&str> = Vec::new();
    // The indentation of the `#[cfg(test)]` being skipped, so that a test
    // module *nested inside* an enforcement module is stripped too. Round 16
    // moved `fatal_access`'s own derivation test inside the module (its
    // constructor is now private to it), and a stripper that only knew about
    // column-zero `#[cfg(test)]` would have counted those calls against the
    // gate that says the constructor is called once in the library.
    let mut skipping: Option<String> = None;
    // Set between the `#[cfg(test)]` line and the item it introduces, carrying
    // the attribute's indentation and how many lines of that item have been
    // seen. The counter is what keeps a mis-parse loud: a well-formed item
    // resolves within a couple of lines, so running past the bound means the
    // stripper no longer understands the source and must say so.
    let mut awaiting_item: Option<(String, usize)> = None;
    const MAX_ITEM_HEADER_LINES: usize = 8;
    for (number, line) in source.lines().enumerate() {
        let number = number + 1;
        if let Some(indent) = skipping.as_deref() {
            if line == format!("{indent}}}") {
                skipping = None;
            }
            continue;
        }
        if let Some((indent, seen)) = awaiting_item.clone() {
            let trimmed = line.trim_start();
            // Further attributes and doc comments still belong to the item.
            if trimmed.starts_with("#[") || trimmed.starts_with("//") || trimmed.is_empty() {
                continue;
            }
            // A braced item — `mod`, `fn`, `impl` — opens its scope at the
            // attribute's own indentation and closes it at `{indent}}`.
            if line.ends_with('{') && line.starts_with(&indent) {
                awaiting_item = None;
                skipping = Some(indent);
                continue;
            }
            // A statement — `use ...;`, or a `let` whose initialiser wraps
            // onto following lines — ends at the first line ending in `;`.
            if line.ends_with(';') {
                awaiting_item = None;
                continue;
            }
            assert!(
                seen < MAX_ITEM_HEADER_LINES,
                "{name}:{number}: a `#[cfg(test)]` item is still unresolved {seen} lines in, at \
                 `{}`. This stripper understands a braced item opening at the attribute's own \
                 indentation and a statement ending in `;`. Teach `crate_shipped_code` about \
                 the new shape — do not leave it, because every gate built on this helper \
                 would silently narrow.",
                line.trim()
            );
            awaiting_item = Some((indent, seen + 1));
            continue;
        }
        if is_test_gate_attribute(line.trim_start()) {
            let indent = &line[..line.len() - line.trim_start().len()];
            awaiting_item = Some((indent.to_string(), 0));
            continue;
        }
        if line.trim_start().starts_with("//") {
            continue;
        }
        kept.push(line);
    }
    assert!(
        awaiting_item.is_none(),
        "{name}: a trailing `#[cfg(test)]` introduces no item"
    );
    assert!(
        skipping.is_none(),
        "{name}: a `#[cfg(test)]` module was never closed by a line that is exactly its \
         indentation plus `}}` — the stripper ran to end of file and would have hidden \
         everything after it from every gate that uses it"
    );
    kept.join("\n")
}

/// The stripper's own contract, stated as a test rather than as a comment.
///
/// Re-verification called the `#[cfg(test)]` skip "a real narrowing of those
/// gates" whose heuristic "will silently mis-scope if anyone reformats". It no
/// longer can be silent — but the property worth pinning is the one the gates
/// actually depend on: the stripper removes test modules and *only* test
/// modules, leaving shipped code byte-identical.
#[test]
fn the_shipped_code_stripper_removes_test_modules_and_nothing_else() {
    for name in ["matrix.rs", "file.rs", "writer_permit.rs"] {
        let code = crate_code(name);
        let shipped = crate_shipped_code(name);
        assert!(
            shipped.len() < code.len(),
            "{name}: stripping `#[cfg(test)]` removed nothing; the catalogues live in one"
        );
        // Nothing survives that only a test module could have contained...
        assert!(
            !shipped.contains("#[cfg(test)]"),
            "{name}: a `#[cfg(test)]` attribute survived stripping"
        );
        assert!(
            !shipped.contains("#[test]"),
            "{name}: a `#[test]` function survived stripping"
        );
        // ...and everything that survives is a line of the comment-stripped
        // source, in the same order: the stripper only ever drops lines.
        let mut shipped_lines = shipped.lines();
        let mut dropped = 0usize;
        for line in code.lines() {
            match shipped_lines.clone().next() {
                Some(next) if next == line => {
                    shipped_lines.next();
                }
                _ => dropped += 1,
            }
        }
        assert_eq!(
            shipped_lines.next(),
            None,
            "{name}: stripping produced a line the source does not have"
        );
        assert!(dropped > 0, "{name}: nothing was dropped");
    }
}

/// SHAPE A, `matrix.rs`. The two functions that put persisted page-index bytes
/// on disk are declared inside a private `mod page_index` **without**
/// `pub(super)`, so nothing in the other ~7000 lines of the file can call them;
/// the only route is one of the prepared values, whose construction performs
/// every fallible step. Adding `pub(super)` — or moving either function out of
/// the module — is the single edit that would reopen F-03, and it fails here.
#[test]
fn the_matrix_page_index_writers_stay_unreachable_outside_their_gate() {
    let source = crate_source("matrix.rs");
    let gate_start = source
        .find("\nmod page_index {")
        .expect("matrix.rs must still hold the private `mod page_index` gate");
    // The module is top-level, so its closing brace is the next line that is
    // exactly `}` at column zero.
    let gate_end = source[gate_start + 1..]
        .find("\n}\n")
        .map(|offset| gate_start + 1 + offset)
        .expect("`mod page_index` must be closed");
    let gate = &source[gate_start..gate_end];

    for writer in ["fn write_entry_run", "fn write_header_bytes"] {
        assert!(
            gate.contains(writer),
            "{writer} must stay inside `mod page_index`: outside it, the fallible \
             mirror work stops being a compile-time precondition of the disk write"
        );
        assert!(
            !gate.contains(&format!("pub(super) {writer}"))
                && !gate.contains(&format!("pub(crate) {writer}"))
                && !gate.contains(&format!("pub {writer}")),
            "{writer} must stay private to `mod page_index`; exporting it is the \
             one edit that reopens F-03"
        );
    }
    assert_eq!(
        source.matches("fn write_entry_run").count(),
        1,
        "a second page-index entry writer outside the gate would bypass it"
    );
    assert_eq!(
        source.matches("fn write_header_bytes").count(),
        1,
        "a second page-index header writer outside the gate would bypass it"
    );
}

/// Every `.rs` file that ships in `varve-core`. Widened deliberately: a gate
/// that names its modules cannot see the module somebody adds next.
fn crate_modules() -> Vec<String> {
    let dir: PathBuf = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("varve-core")
        .join("src");
    let mut modules: Vec<String> = fs::read_dir(&dir)
        .unwrap_or_else(|error| panic!("read {}: {error}", dir.display()))
        .map(|entry| entry.expect("directory entry").file_name())
        .map(|name| name.to_string_lossy().into_owned())
        .filter(|name| name.ends_with(".rs"))
        .collect();
    modules.sort();
    assert!(
        modules.len() > 15,
        "the module sweep found {} files, which cannot be right",
        modules.len()
    );
    modules
}

/// SHAPE B, crate-wide. `crate::writer_permit::PoisonFlag` is the only holder
/// of a writer poison boolean. Round 12 found three independent hand-written
/// flags (`stream.rs`, `file.rs`, `layout.rs`), each with its own guard and its
/// own convention about which methods must consult it; F-05 was two of them
/// disagreeing. A fourth would recreate the class, so a bare `poisoned: bool`
/// field anywhere in the crate fails here.
#[test]
fn no_writer_keeps_its_own_poison_boolean() {
    for module in [
        "file.rs",
        "layout.rs",
        "stream.rs",
        "indexed.rs",
        "matrix.rs",
        "disk_index.rs",
    ] {
        let source = crate_source(module);
        assert!(
            !source.contains("poisoned: bool"),
            "{module} declares its own poison boolean; use `crate::writer_permit::PoisonFlag`, \
             so that the guarded operations can demand a `MutationPermit` and a mutating \
             method written later cannot skip the check"
        );
    }
    let permit = crate_source("writer_permit.rs");
    assert!(
        permit.contains("poisoned: bool") && permit.contains("in_flight: bool"),
        "`writer_permit` must remain the one holder of the state"
    );
    assert!(
        permit.contains("pub struct MutationPermit<W: ?Sized>(PhantomData<fn() -> W>);"),
        "the permit's field must stay private: a public field makes the witness forgeable, \
         which is what tests/ui/fail_fabricated_mutation_permit.rs asserts"
    );
}

/// SHAPE B, the level above F-05. Round 12's permit proved that *a* poison flag
/// had been checked, not that *the writer being mutated* had been:
/// `PoisonFlag::healthy()` is a `const fn` and the permit constructor was
/// `pub(crate)`, so re-verification compiled a three-line bypass —
/// `PoisonFlag::healthy().permit("stream")` handed straight to
/// `append_prepared_chunk` on a poisoned writer. The witness was launderable
/// from a decoy.
///
/// The constructor is now **private to `writer_permit`**, so no other module can
/// mint a permit from any flag, and the only route out of the module is
/// `GuardedWriter::writer_permit`, which takes `&self` of the writer and reads
/// that writer's own flag. Restoring any visibility on the constructor is the
/// single edit that reopens the bypass, and it fails here.
///
/// This one cannot be a `tests/ui` fixture: the bypass was *in-crate*, and every
/// spelling of it is already unreachable from a downstream crate, so a
/// compile-fail fixture would pass for the wrong reason. The cross-writer half
/// of the property — that a permit for one writer is not a permit for another —
/// *is* expressible downstream and is asserted by
/// `tests/ui/fail_cross_writer_mutation_permit.rs`.
#[test]
fn a_permit_can_only_be_minted_from_the_writers_own_poison_flag() {
    let permit = crate_source("writer_permit.rs");
    assert!(
        permit.contains(
            "    fn issue<W: ?Sized>(&self, context: &'static str) -> Result<MutationPermit<W>> {"
        ),
        "`PoisonFlag::issue` must remain the sole constructor and must keep its exact private \
         declaration"
    );
    for visibility in ["pub fn issue", "pub(crate) fn issue", "pub(super) fn issue"] {
        assert!(
            !permit.contains(visibility),
            "`{visibility}` puts permit minting back in reach of every module in the crate: a \
             throwaway `PoisonFlag::healthy()` then speaks for a poisoned writer, which is the \
             bypass that capped round 12's F-05 fix at partial"
        );
    }
    assert_eq!(
        crate_code("writer_permit.rs")
            .matches("MutationPermit(PhantomData)")
            .count(),
        1,
        "there must be exactly one place where a permit value comes into existence"
    );
    assert!(
        permit.contains("    pub fn healthy() -> Self {"),
        "`PoisonFlag::healthy` must stay a non-`const` fn (round 15). While it was `const` a          `static DECOY: PoisonFlag = PoisonFlag::healthy();` could be declared anywhere in the          crate and returned from a writer's own `poison_flag`, at which point `writer_permit`          reads the decoy and every guarded operation is permitted on a poisoned writer"
    );
    assert!(
        permit.contains("fn writer_permit(&self, context: &'static str) -> Result<MutationPermit<Self>> {\n        self.poison_flag().issue(context)\n    }"),
        "the only exported route to a permit must read the flag of the writer the permit names; \
         a free function taking `&PoisonFlag` would restore the laundering"
    );

    // Belt and braces: no module may hold a `PoisonFlag` anywhere but in a
    // writer's own field, so a decoy flag does not even come into being.
    for module in ["file.rs", "layout.rs", "stream.rs", "indexed.rs"] {
        let source = crate_code(module);
        for (number, line) in source.lines().enumerate() {
            if !line.contains("PoisonFlag::healthy()") {
                continue;
            }
            assert!(
                line.trim_start()
                    .starts_with("poison: PoisonFlag::healthy()"),
                "{module}:{}: a `PoisonFlag` may only be created as a writer's own field; a local \
                 one is a decoy, and the permit constructor's privacy is what stops it speaking \
                 for a real writer",
                number + 1
            );
        }
    }
}

/// SHAPE A/B, `file.rs`. Round 12 claimed that a replacement path added later
/// "cannot reach `self.index[..]` for a caller-chosen ordinal without going
/// through" the version check, and that `overwrite_record_bytes_in_place` was
/// "the only function in the crate that rewrites the bytes of an already-indexed
/// record". Re-verification compiled both bypasses: a new function that resolved
/// its own ordinal and called the writer, and a new `self.file.seek(..)` /
/// `self.file.write_all(..)` pair that skipped the writer entirely. Both claims
/// were about today's code, not about what the compiler permits.
///
/// The compiler now refuses both, because the primary handle is a `RecordFile`
/// whose `File` is unnameable outside `mod record_file` and which exposes no
/// general-purpose write. This gate stands for the part a compile-fail fixture
/// cannot express from outside the crate: that the handle stays wrapped and
/// that no second write pair is introduced against the raw field.
#[test]
fn the_primary_record_handle_is_only_written_through_its_two_gated_operations() {
    let source = crate_source("file.rs");
    assert!(
        source.contains("    file: RecordFile,"),
        "`VarveFile` must hold the primary handle as a `RecordFile`; a bare `File` field puts \
         `seek`/`write_all` back in reach of every one of this file's ~12,000 lines, which is \
         how F-01 and F-02 were both reproducible against round 12's fixes"
    );
    let gate_start = source
        .find("\nmod record_file {")
        .expect("file.rs must still hold the private `mod record_file` gate");
    let gate_end = source[gate_start + 1..]
        .find("\n}\n")
        .map(|offset| gate_start + 1 + offset)
        .expect("`mod record_file` must be closed");
    let gate = &source[gate_start..gate_end];
    // Everything outside the gate. The gate's own body is where the two
    // permitted writes are implemented; anywhere else, a `self.file` write or
    // seek is a second route to record bytes.
    let outside: String = format!("{}{}", &source[..gate_start], &source[gate_end..])
        .lines()
        .filter(|line| !line.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n");
    for bypass in [
        "self.file.write_all(",
        "self.file.write(",
        "self.file.seek(",
        "self.file.write_vectored(",
    ] {
        assert!(
            !outside.contains(bypass),
            "`{bypass}` writes or positions the primary handle directly; record bytes may only \
             be placed by `RecordFile::append_record_at_end` (which seeks to the end itself) or \
             by `RecordFile::overwrite_indexed_record` (which consumes a `RecordOverwrite`)"
        );
    }
    // Comment-stripped, because the fields carry doc comments and a gate that
    // breaks when someone rewords a comment teaches people to relax the gate.
    let gate_code: String = gate
        .lines()
        .filter(|line| !line.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        gate_code.contains(
            "    pub struct RecordFile {\n        file: File,\n        matrix_read_pool: \
             crate::matrix::MatrixReadPool,\n    }"
        ),
        "the wrapped handle must stay a private field of `RecordFile`, and the only other field \
         may be the read-only `MatrixReadPool` (whose own handles are opened `FILE_GENERIC_READ` \
         and never leave `mod region_reader` — see \
         `the_private_matrix_read_handles_are_read_only_and_never_lent_out`); any further field \
         has to be argued for here"
    );
    assert!(
        !gate.contains("impl Write for RecordFile")
            && !gate.contains("impl Seek for RecordFile")
            && !gate.contains("fn as_file")
            && !gate.contains("-> &File"),
        "`RecordFile` must not implement a general write, nor lend out a `&File`: `&File` \
         implements `Write`, so lending one is equivalent to lending a mutable handle"
    );
    // The single deliberate exception, named in the module doc rather than left
    // to be discovered.
    assert_eq!(
        source.matches("fn matrix_region(").count(),
        1,
        "there must be exactly one escape hatch to the raw handle"
    );
}

/// The named exception to the gate above. `crate::matrix`'s functions and the
/// public `MatrixDurabilityBarrier` trait take `&mut File`, so the handle has to
/// be lent for matrix work. The matrix region is disjoint from the record region
/// and carries its own shape-A gate (`matrix.rs`, `mod page_index`). This gate
/// keeps the exception from silently widening: every use of the accessor must be
/// an argument to a `crate::matrix::` call or to a matrix durability sync.
#[test]
fn the_primary_handle_escape_is_only_for_the_matrix_region() {
    let source = crate_source("file.rs");
    for (number, line) in source.lines().enumerate() {
        if !line.contains(".matrix_region()") {
            continue;
        }
        let context: String = source
            .lines()
            .skip(number.saturating_sub(6))
            .take(7)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            context.contains("crate::matrix::")
                || context.contains("sync_matrix_data")
                || context.contains("sync_matrix_commit")
                || line.contains("pub(super) fn matrix_region"),
            "file.rs:{}: `matrix_region()` is the one escape from the record-write gate and may \
             only hand the handle to `crate::matrix` or to a matrix durability barrier; this use \
             is something else:\n{context}",
            number + 1
        );
    }
}

/// The per-thread matrix read handles are a *second* set of open file objects
/// for the same file, so they get the same treatment as the primary handle:
/// read-only at the OS level, and never lent out of the module that owns them.
///
/// They exist because `&self` on the read entry points bought the right to share
/// a handle and, on Windows, none of the throughput: `ReadFile` against a
/// synchronous handle serialises on the file object, so four threads sharing one
/// handle measured 0.27x of one thread. Each reading thread now gets its own
/// file object derived from the open handle with `ReOpenFile`. That is only
/// acceptable while the derived handles cannot write.
#[test]
fn the_private_matrix_read_handles_are_read_only_and_never_lent_out() {
    let source = crate_source("matrix.rs");
    let start = source
        .find("\nmod region_reader {")
        .expect("matrix.rs must still hold `mod region_reader`");
    let end = source[start + 1..]
        .find("\n}\n")
        .map(|offset| start + 1 + offset)
        .expect("`mod region_reader` must be closed");
    let module: String = source[start..end]
        .lines()
        .filter(|line| {
            !line.trim_start().starts_with("//") && !line.trim_start().starts_with("///")
        })
        .collect::<Vec<_>>()
        .join("\n");

    assert!(
        module.contains("FILE_GENERIC_READ"),
        "the private handles must be reopened read-only"
    );
    for writable in ["GENERIC_WRITE", "FILE_GENERIC_WRITE", "FILE_WRITE_DATA"] {
        assert!(
            !module.contains(writable),
            "`{writable}` in `mod region_reader` would make a private read handle writable, and \
             it is reachable from `&self` — i.e. from a shared borrow of a `VarveReader`"
        );
    }
    assert_eq!(
        module.matches("ReOpenFile(").count(),
        1,
        "one derivation site; `ReOpenFile` takes no path, so there is no window in which a \
         different file could be opened under the same name"
    );
    assert!(
        module.contains("FILE_SHARE_DELETE"),
        "a cached private handle must not block deletion of a file its owner has closed"
    );
    for lent in [
        "pub(crate) fn reopen",
        "pub fn reopen",
        "-> &File",
        "-> Arc<File>",
    ] {
        assert!(
            !module.contains(lent),
            "`{lent}`: the private handles must never leave `mod region_reader`; `&File` \
             implements `Write`, so lending one out is lending a write handle"
        );
    }
    // The pool is owned by `RecordFile` and the thread-local cache holds only
    // `Weak`s, so the handles close when the file does.
    assert!(
        module.contains("PrivateHandle::Open(Weak<File>)") || module.contains("Open(Weak<File>)"),
        "the thread-local cache must hold `Weak`s: an `Arc` there would keep one handle per \
         thread per file open for the life of the process, long after the `RecordFile` is gone"
    );
}

/// F-01/F-02, `file.rs`. The two enforcement types keep their private fields and
/// their single constructors, and the in-place writer keeps taking the
/// version-checked token rather than a caller-supplied offset. The compile-fail
/// fixtures assert the fields; this asserts the wiring they are pointless
/// without.
#[test]
fn an_in_place_record_write_still_demands_a_version_checked_target() {
    let source = crate_source("file.rs");
    assert!(
        source.contains("pub struct ReplacementTarget {\n        position: usize,\n    }"),
        "the target token's field must stay private, which is what \
         tests/ui/fail_fabricated_replacement_target.rs asserts"
    );
    assert_eq!(
        source.matches("fn resolve<T: VarveBlock>").count(),
        1,
        "`ReplacementTarget::resolve` must remain the only constructor, because it is where the \
         `T::VERSION` refusal lives"
    );
    assert_eq!(
        source.matches("fn prepare(").count(),
        1,
        "`RecordOverwrite::prepare` must remain the only constructor, because it is where the \
         keyed-tail invalidation lives"
    );
    assert!(
        source.contains("keyed_tails.invalidate(entry.block_id);"),
        "producing the write permission must drop the target block's keyed-tail map, before the \
         permission exists and therefore before any byte can reach disk (F-02)"
    );
    assert!(
        source.contains("target: ReplacementTarget,"),
        "the in-place writer must take the version-checked token, not a caller-chosen offset"
    );
    assert!(
        source.contains("write: RecordOverwrite,"),
        "`RecordFile::overwrite_indexed_record` must consume the permission by value"
    );
}

/// SHAPE B, `matrix.rs`. F-04's fix was judged *partial* on re-verification:
/// round 12 recorded completeness in a `crc_valid_complete: bool` beside a plain
/// `SparseBitmap` and added one call to a checking function, so a new consumer
/// that wrote `block.crc_valid_bits.get(ordinal)?` compiled cleanly. The check
/// bound nothing — it was the "the next function will simply fail to repeat it"
/// shape, in the file that had just been rewritten to close that class.
///
/// The bitmap now lives inside `mod crc_valid_evidence` and is unnameable
/// outside it, so the only operation in the crate that yields a validity *bit*
/// is `CompleteCrcValidEvidence::get`, whose witness is produced solely by the
/// completeness refusal. The compile-fail fixture
/// `tests/ui/fail_fabricated_crc_valid_completeness.rs` asserts the witness
/// cannot be forged. This gate stands for the two parts a fixture cannot express
/// from outside the crate: that the field keeps the evidence type rather than a
/// bare bitmap, and that the module's two deliberate escapes — the fail-closed
/// reader and the page-index maintenance lend — keep exactly one call site each.
#[test]
fn the_crc_validity_bitmap_is_only_read_through_its_evidence_type() {
    let source = crate_source("matrix.rs");
    assert!(
        source.contains("    crc_valid_bits: CrcValidEvidence,"),
        "`MatrixBlockLayout::crc_valid_bits` must stay a `CrcValidEvidence`; a bare \
         `SparseBitmap` field puts `get` back in reach of all ~7000 lines of matrix.rs, which \
         is the configuration F-04 was found in and the reason its round-12 fix was judged \
         partial"
    );
    let code: String = source
        .lines()
        .filter(|line| {
            let trimmed = line.trim_start();
            !trimmed.starts_with("//") && !trimmed.starts_with("///")
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        !code.contains("crc_valid_complete"),
        "completeness must travel *inside* the evidence type; a separate boolean beside the \
         bitmap is a fact a reader may consult, not a precondition of addressing it"
    );

    let gate_start = source
        .find("\npub(crate) mod crc_valid_evidence {")
        .expect("matrix.rs must still hold the `mod crc_valid_evidence` gate");
    let gate_end = source[gate_start + 1..]
        .find("\n}\n")
        .map(|offset| gate_start + 1 + offset)
        .expect("`mod crc_valid_evidence` must be closed");
    let gate = &source[gate_start..gate_end];
    // Everything outside the gate that ships in the library: comments cannot
    // call anything, and the crate's own `#[cfg(test)]` modules are where the
    // evidence type's unit contract is stated, so counting their calls would
    // punish testing the enforcement.
    let mut outside = format!("{}{}", &source[..gate_start], &source[gate_end..]);
    for unit_test_module in [
        "\n#[cfg(test)]\nmod crc_valid_evidence_tests {",
        // Round 16: the in-crate bypass catalogue proves the narrowed escape
        // by *using* it, which is the only way to prove what it no longer
        // lends. Counting that call against the one-call-site gate would make
        // demonstrating the property break the gate that states it.
        "\n#[cfg(test)]\nmod bypass_catalogue {",
    ] {
        if let Some(unit_tests) = outside.find(unit_test_module) {
            let end = outside[unit_tests + 1..]
                .find("\n}\n")
                .map(|offset| unit_tests + 1 + offset + 3)
                .unwrap_or(outside.len());
            outside.replace_range(unit_tests..end, "\n");
        }
    }
    let outside: String = outside
        .lines()
        .filter(|line| {
            let trimmed = line.trim_start();
            !trimmed.starts_with("//") && !trimmed.starts_with("///")
        })
        .collect::<Vec<_>>()
        .join("\n");

    assert!(
        gate.contains("bits: SparseBitmap,"),
        "the bitmap must stay a private field of `CrcValidEvidence`"
    );
    assert_eq!(
        gate.matches("fn complete(&self)").count(),
        1,
        "`CrcValidEvidence::complete` must remain the only producer of the witness, because it \
         is where the F-04 refusal lives"
    );
    assert_eq!(
        gate.matches("fn get(&self, ordinal: u64) -> Result<bool>")
            .count(),
        1,
        "`CompleteCrcValidEvidence::get` must remain the only bit read; a second one on \
         `CrcValidEvidence` itself would hand back absence as data without the proof"
    );

    // The two deliberate escapes, held to one use each. Anything more is a new
    // route from incomplete evidence to a value a caller can publish.
    assert_eq!(
        outside.matches(".require_meaningful(").count(),
        1,
        "the fail-closed reader has exactly one legitimate caller (`verify_cell_crc`, which \
         turns an absent bit into a refusal). A second caller must justify itself here rather \
         than appear silently, and `.is_ok()` on its result is the one way its `Result<()>` \
         can be laundered back into a bit"
    );
    for (offset, _) in outside.match_indices(".require_meaningful(") {
        let window = &outside[offset..outside.len().min(offset + 400)];
        assert!(
            !window.contains(".is_ok()") && !window.contains(".is_err()"),
            "a fail-closed read must never be turned back into a boolean: its `Result<()>` \
             carries no bit precisely so that an absent one cannot become published state"
        );
    }
    assert_eq!(
        outside.matches(".page_index_mirror_mut()").count(),
        1,
        "lending the bitmap for page-index maintenance is the one escape from the evidence \
         type; every extra call site is a fresh route to `SparseBitmap::get`"
    );
    // Round 16: the count above is the second line of defence, not the
    // property. Re-verification executed `*evidence.page_index_mirror_mut() =
    // attacker_bits;` at the single permitted call site and read a laundered
    // bit back through `CompleteCrcValidEvidence::get` — a count over call
    // sites says nothing about what one call site may do. The escape must
    // therefore hand back the narrowed mirror, whose borrow is private to `mod
    // page_index`, and never a `&mut SparseBitmap`.
    assert!(
        gate.contains("pub(super) fn page_index_mirror_mut(&mut self) -> PageIndexMirror<'_> {"),
        "the page-index escape must return `PageIndexMirror`, not `&mut SparseBitmap`: with the \
         raw borrow, `*evidence.page_index_mirror_mut() = attacker_bits` replaces a block's \
         whole validity map while `complete` stays true, which is precisely the F-04 property \
         `CrcValidEvidence::new` was deleted to protect"
    );
    let mirror_start = source
        .find("    pub(super) struct PageIndexMirror<'a> {")
        .expect("`mod page_index` must hold the narrowed mirror");
    let mirror_end = source[mirror_start..]
        .find("\n    /// Records `page` in the persisted page index")
        .map(|offset| mirror_start + offset)
        .expect("`PageIndexMirror` must keep its impl before the entry writers");
    let mirror = &source[mirror_start..mirror_end];
    assert!(
        mirror.contains("        bits: &'a mut SparseBitmap,"),
        "the mirror's borrow must stay a private field of `mod page_index`; a `pub` one is the \
         raw borrow again with an extra dot"
    );
    for lent_back in ["-> &mut SparseBitmap", "-> &SparseBitmap", "impl DerefMut"] {
        assert!(
            !mirror.contains(lent_back),
            "`PageIndexMirror` must not hand the bitmap back (`{lent_back}`): its whole purpose \
             is that page-index maintenance cannot reach `set`, `get` or assignment"
        );
    }
    assert!(
        outside.contains("let evidence = block.crc_valid_bits.complete()?;"),
        "the CRC commit-map rebuild must keep obtaining the completeness witness before it \
         reads any validity bit (F-04)"
    );
}

/// SHAPE B, the inventory itself. The predecessor of this gate matched the
/// literal string `poisoned: bool` in six named files, which by construction
/// could not find a validity flag with another name — and re-verification found
/// two: `matrix.rs`'s `fatal_access_blocked` (a `Fatal` recovery finding
/// blocking safe access to matrix state, consulted by
/// `ensure_fatal_access_allowed` at eleven sites) and, added by the round that
/// was supposed to be closing this class, `crc_valid_complete` (F-04's
/// evidence-completeness fact, consulted by one `ensure_` call). Both were
/// guards a later function simply had to not call.
///
/// The rule is therefore stated by *shape* rather than by name: no struct in
/// the crate may declare a `bool` field that an `fn ensure_*` guard consults.
/// A boolean that gates an operation belongs inside a type whose only output is
/// a witness, so that the operations demand the check instead of remembering
/// it — `writer_permit::PoisonFlag`, `matrix::fatal_access::FatalAccessGate`
/// and `matrix::crc_valid_evidence::CrcValidEvidence` are the three that exist,
/// and each keeps its boolean unnameable outside its module.
///
/// The scan is deliberately conservative about what an "`ensure_` guard" is: it
/// reads the forty lines following each `fn ensure_` signature. Guards are
/// short, and a guard that is not would be its own finding.
#[test]
fn no_guard_consults_a_bare_boolean_field() {
    for module in crate_modules() {
        let source = crate_source(&module);
        let fields: Vec<String> = source
            .lines()
            .map(str::trim)
            .filter(|line| line.ends_with(": bool,") && !line.starts_with("//"))
            .filter_map(|line| line.split(':').next().map(|name| name.trim().to_string()))
            .filter(|name| !name.is_empty() && !name.contains(' '))
            .collect();
        if fields.is_empty() {
            continue;
        }
        let lines: Vec<&str> = source.lines().collect();
        for (number, line) in lines.iter().enumerate() {
            let trimmed = line.trim_start();
            let is_guard = [
                "fn ensure_",
                "pub fn ensure_",
                "pub(crate) fn ensure_",
                "pub(super) fn ensure_",
            ]
            .iter()
            .any(|prefix| trimmed.starts_with(prefix));
            if !is_guard {
                continue;
            }
            // Bounded by the guard's own closing brace rather than by a line
            // count: a window that overruns reads the *next* function's
            // bookkeeping and reports it as a gate, which is a false finding
            // and would train a reader to ignore this test.
            let indent = &line[..line.len() - trimmed.len()];
            let closing = format!("{indent}}}");
            let body_end = lines[number + 1..]
                .iter()
                .position(|candidate| *candidate == closing)
                .map(|offset| number + 1 + offset)
                .unwrap_or(lines.len() - 1);
            for field in &fields {
                // What makes a boolean a *gate* is that the guard refuses
                // because of it. A guard that reads a bookkeeping flag on its
                // way to doing something — `indexed.rs::ensure_batch` and its
                // `dirty` — is not this class, and reporting it would be a
                // false finding that trains a reader to ignore this test.
                let refuses =
                    lines[number..=body_end]
                        .iter()
                        .enumerate()
                        .any(|(offset, candidate)| {
                            let position = number + offset;
                            candidate.contains(&format!(".{field}"))
                                && candidate.trim_start().starts_with("if ")
                                && lines[position..lines.len().min(position + 5)]
                                    .iter()
                                    .any(|following| following.contains("return Err("))
                        });
                assert!(
                    !refuses,
                    "{module}:{}: the guard `{}` refuses on the bare boolean field `{field}`. A \
                     boolean that gates an operation is a guard a later function can fail to \
                     call, which is how `fatal_access_blocked` and `crc_valid_complete` \
                     survived the round that closed this class elsewhere. Put it in a type \
                     whose only output is a witness the guarded operation demands.",
                    number + 1,
                    line.trim()
                );
            }
        }
    }
}

/// SHAPE A, `file.rs`, the ordering half. Re-verification compiled F-03's exact
/// shape against round 12's token — an authoritative `write_all`, then the
/// fallible `ReservedIndexSlot::reserve`, then the install — because the token
/// proved a reservation *existed*, not that it *preceded* the write. It also
/// compiled a mirror grown around the token entirely, via `core::mem::take` on
/// the `Vec` field.
///
/// Both are now refused: the mirror is a `ResidentIndex` whose `Vec` is
/// unnameable outside `mod resident_index`, and the append demands the
/// reservation token as an argument, so the write cannot be reached before the
/// fallible half has succeeded. This gate stands for the declarations that make
/// that true; the downstream half is
/// `tests/ui/fail_fabricated_index_reservation.rs`.
#[test]
fn the_resident_record_index_cannot_be_grown_or_outrun() {
    let source = crate_source("file.rs");
    assert!(
        source.contains("    index: ResidentIndex,"),
        "`VarveFile::index` must stay a `ResidentIndex`; a bare `Vec` field puts `push`, \
         `insert`, `extend` and `mem::take` back in reach of every one of this file's ~12,000 \
         lines, and the reservation token then binds nothing"
    );
    assert!(
        source.contains(
            "    pub struct ResidentIndex {\n        entries: Vec<RecordIndexEntry>,\n    }"
        ),
        "the mirror's `Vec` must stay a private field of `ResidentIndex`"
    );
    assert!(
        !source.contains("impl DerefMut for ResidentIndex")
            && !source.contains("fn entries_mut")
            && !source.contains("-> &mut Vec<RecordIndexEntry>"),
        "`ResidentIndex` must not lend out its `Vec`: a `&mut Vec` is every growth operation \
         at once"
    );
    assert!(
        !source.contains("Default)]\n    pub struct ResidentIndex"),
        "`ResidentIndex` must not be `Default`: `core::mem::take` on the field is the bypass \
         that re-verification compiled"
    );
    assert!(
        source.contains("pub struct ReservedIndexSlot(());"),
        "the reservation token's field must stay private, which is what \
         tests/ui/fail_fabricated_index_reservation.rs asserts"
    );
    assert_eq!(
        source.matches("self.entries.push(").count(),
        1,
        "there must be exactly one growth of the resident index in the crate, and it must be \
         `ResidentIndex::install`, which consumes the token"
    );
    assert!(
        source.contains("            _reserved: &ReservedIndexSlot,"),
        "`RecordFile::append_record_at_end` must demand the reservation token: that is what \
         makes the fallible mirror half a precondition of the authoritative write, rather than \
         a comment above it"
    );
    assert!(
        source.contains("self.index.install(index_slot, entry);"),
        "the single post-append mirror install must stay routed through the token"
    );
    // Carried over from the round-12 gate this supersedes: the spelling that
    // grew the mirror directly must not come back, even though the type now
    // refuses it.
    assert!(
        !source.contains("self.index.push("),
        "the append path must install through `ResidentIndex::install`, whose token is \
         produced only by the reservation that runs before the write"
    );
}

/// SHAPE B, `matrix.rs`, the flag the name-based sweep could not see.
///
/// `fatal_access_blocked` gated access to matrix state through an
/// `ensure_fatal_access_allowed` helper called by convention, and was absent
/// from round 12's shape-B table because that inventory was built by grepping
/// for `poisoned`. The boolean now lives in `mod fatal_access`, and the two
/// functions that turn a block id or a commit category into a position in the
/// layout's state demand the witness that reading it produces.
///
/// The witness cannot be forged from outside the crate
/// (`tests/ui/fail_fabricated_fatal_access.rs`); this gate stands for the
/// in-crate half — that the flag stays wrapped, that the resolvers keep
/// demanding the witness, and that there remains exactly one place a witness
/// comes into existence.
#[test]
fn matrix_state_is_only_addressed_through_the_fatal_access_witness() {
    let source = crate_source("matrix.rs");
    assert!(
        source.contains("    fatal_access: FatalAccessGate,"),
        "`MatrixLayout` must hold the fail-closed state as a `FatalAccessGate`; a bare \
         `fatal_access_blocked: bool` is a guard every accessor has to remember, which is the \
         configuration re-verification found it in"
    );
    let gate_start = source
        .find("\npub(crate) mod fatal_access {")
        .expect("matrix.rs must hold the `mod fatal_access` gate");
    let gate_end = source[gate_start + 1..]
        .find("\n}\n")
        .map(|offset| gate_start + 1 + offset)
        .expect("`mod fatal_access` must be closed");
    let gate = &source[gate_start..gate_end];
    assert!(
        gate.contains("        blocked: bool,"),
        "the boolean must stay a private field of `FatalAccessGate`"
    );
    assert!(
        gate.contains("pub struct FatalAccessAllowed(());"),
        "the witness's field must stay private, which is what \
         tests/ui/fail_fabricated_fatal_access.rs asserts"
    );
    let code = crate_shipped_code("matrix.rs");
    assert_eq!(
        code.matches("FatalAccessAllowed(())").count(),
        2,
        "the witness must come into existence in exactly one place: its declaration, and the          single `Ok(FatalAccessAllowed(()))` inside `fatal_access::allow_for`, which IS the          refusal"
    );

    // Round 15: the minting half. Round 14 made the witness unforgeable and
    // stopped there, so `FatalAccessGate::new(false).allow()` - three tokens,
    // inside matrix.rs - produced a witness for a layout that was blocked.
    assert!(
        !gate.contains("fn new("),
        "`FatalAccessGate` must have no constructor that accepts the decision. `new(blocked:          bool)` was the named one-line bypass: mint a gate that says `false`, take its witness,          and address the state of a layout carrying a `Fatal` finding"
    );
    assert!(
        gate.contains("        fn evaluate<'a>("),
        "the only constructor must be `FatalAccessGate::evaluate`, which *performs* the          classification from the recovery findings and the spec rather than accepting it"
    );
    // Round 16. `pub(super)` on the constructor made it *file*-visible, and a
    // file-visible function returning a gate is a file-visible way to write
    // one: `layout.fatal_access = FatalAccessGate::evaluate(clean.iter(),
    // spec);` is a single field assignment (`MatrixLayout`'s fields are private
    // to the file, not to a module) and it undoes `block()`. The enforced
    // property is no longer "one call site" but "no expression of this type
    // exists outside the module".
    assert!(
        !gate.contains("pub(super) fn evaluate")
            && !gate.contains("pub(crate) fn evaluate")
            && !gate.contains("pub fn evaluate"),
        "`FatalAccessGate::evaluate` must stay private to `mod fatal_access`: exported, it is a          one-line reassignment of any layout's fail-closed decision, `block()` included"
    );
    assert_eq!(
        code.matches("FatalAccessGate::evaluate(").count(),
        1,
        "a gate must come into existence exactly once, on the open path, from that layout's own          findings"
    );
    assert!(
        !gate.contains("#[derive(Clone, Debug)]\n    pub struct FatalAccessGate")
            && !code.contains("impl Clone for FatalAccessGate"),
        "`FatalAccessGate` must not be `Clone`: `layout.fatal_access =          donor.fatal_access.clone();` copies an unblocked decision off a healthy layout onto a          blocked one, in one line, and appeared in no report until re-verification wrote it"
    );
    assert!(
        code.contains("impl Drop for MatrixLayout {"),
        "`MatrixLayout` must implement `Drop`, whose only purpose is E0509: without it a gate          can be *moved* out of any expression that yields a layout          (`donor.clone().fatal_access`, `assemble_layout(..).fatal_access`), which is the same          one-line laundering with two more tokens"
    );
    assert!(
        gate.contains("pub(super) fn assemble_layout(")
            && gate.contains("    let layout = MatrixLayout {"),
        "the layout literal must live inside `mod fatal_access`, so that constructing a gate and          constructing the state it speaks for are the same act"
    );
    assert!(
        !gate.contains("fn allow(&self)"),
        "there must be no method that turns a gate into a witness: a gate built anywhere else          would then speak for a layout it has nothing to do with, which is the laundering shape          round 14 closed for `MutationPermit` and left open here"
    );
    assert!(
        gate.contains(
            "pub(super) fn allow_for(layout: &MatrixLayout) -> Result<FatalAccessAllowed> {"
        ),
        "the sole producer of the witness must take the layout, so the gate consulted is the          gate of the state the caller is about to address"
    );
    assert_eq!(
        code.matches("= MatrixLayout {").count(),
        1,
        "`MatrixLayout` must keep exactly one declaration and one literal. The residual after          round 15 is that a literal can carry a gate of the caller's choosing; holding the crate          to the single construction in `layout_from_parts` is what keeps that residual the size          it is documented to be"
    );
    for resolver in [
        "fn block_index(&self, _allowed: &FatalAccessAllowed, block_id: u32) -> Result<usize> {",
        "fn commit_index(&self, _allowed: &FatalAccessAllowed, name: &str) -> Result<usize> {",
    ] {
        assert!(
            source.contains(resolver),
            "`{resolver}` must keep demanding the witness: these two are the only routes from \
             an identifier to a position in the layout's state, so a matrix accessor written \
             next month cannot address what it wants to touch without the fail-closed check"
        );
    }
}

/// F-04, the **minting** half (round 15).
///
/// Round 14 made `CompleteCrcValidEvidence` unforgeable and left
/// `CrcValidEvidence::new(bits, complete: bool)` `pub(super)`, so one line
/// inside `matrix.rs` - `CrcValidEvidence::new(bits, true).complete()?` -
/// produced the completeness witness for a bitmap nothing had enumerated. That
/// is F-04 with the check spelled out loud instead of omitted, and the round-14
/// `trybuild` fixture could not see it: it compiles an outside crate, where
/// `new` was never reachable in the first place.
///
/// Making the witness type unforgeable cannot fix a *derived* fact on its own,
/// because whoever may state it may restate it. The chain has to terminate at
/// the code that derives it, so the enumerator now lives in
/// `mod page_index_enumeration` and returns a `PageIndexEnumeration` whose two
/// constructors have no visibility modifier at all. This gate holds that shape.
#[test]
fn crc_validity_completeness_can_only_come_from_an_enumeration_that_ran() {
    let source = crate_source("matrix.rs");
    let code = crate_code("matrix.rs");
    let shipped = crate_shipped_code("matrix.rs");

    let gate_start = source
        .find("\npub(crate) mod crc_valid_evidence {")
        .expect("matrix.rs must still hold the `mod crc_valid_evidence` gate");
    let gate_end = source[gate_start + 1..]
        .find("\n}\n")
        .map(|offset| gate_start + 1 + offset)
        .expect("`mod crc_valid_evidence` must be closed");
    let gate = &source[gate_start..gate_end];

    assert!(
        !gate.contains("fn new(bits: SparseBitmap, complete: bool)"),
        "`CrcValidEvidence` must have no constructor that accepts completeness as a `bool`.          That was the named one-line bypass: assert completeness for a bitmap that was never          enumerated and take the F-04 witness off it"
    );
    assert!(
        gate.contains("pub(super) fn from_page_index_enumeration("),
        "the load-path constructor must take the enumeration outcome by value, so the          completeness fact travels from the code that derived it"
    );
    assert!(
        gate.contains("pub(super) fn newly_created(bit_count: u64) -> Result<Self> {"),
        "the create-path constructor must take a bit *count* and build its own empty bitmap;          one that accepted a `SparseBitmap` would launder an arbitrary map as complete"
    );
    for spelling in [
        "pub fn fully_enumerated",
        "pub(crate) fn fully_enumerated",
        "pub(super) fn fully_enumerated",
        "pub fn truncated_by_damage",
        "pub(crate) fn truncated_by_damage",
        "pub(super) fn truncated_by_damage",
    ] {
        assert!(
            !code.contains(spelling),
            "`{spelling}` puts the completeness fact back in reach of every line of matrix.rs,              which is the shape `CrcValidEvidence::new(bits, true)` had"
        );
    }
    assert!(
        code.contains("        fn fully_enumerated() -> Self {")
            && code.contains("        fn truncated_by_damage() -> Self {"),
        "`PageIndexEnumeration`'s two constructors must keep their exact private declarations"
    );
    assert!(
        code.contains("pub(crate) mod page_index_enumeration {")
            && code.contains("    ) -> Result<PageIndexEnumeration> {"),
        "the enumerator must live inside `mod page_index_enumeration` and return the outcome          type; moving it out, or returning a bare `bool`, restores the mint"
    );
    assert_eq!(
        shipped
            .matches("CrcValidEvidence::from_page_index_enumeration(")
            .count(),
        1,
        "there must be exactly one place a loaded bitmap becomes CRC-validity evidence"
    );
    assert!(
        !shipped.contains("index_complete: bool"),
        "the enumeration outcome must not be carried as a bare `bool` anywhere between the          enumerator and the evidence; that intermediate is where it becomes restatable"
    );
    assert_eq!(
        code.matches("from_parts_for_tests_only").count(),
        2,
        "the deliberate test-only escape must keep its name (which says so at the call site),          its `#[cfg(test)]`, and its single caller in the type's own unit tests"
    );
}

/// The in-crate bypass catalogues themselves (round 15).
///
/// The reason the minting holes survived round 14 is that its proofs were
/// `trybuild` fixtures compiled from *outside* `varve-core`, while every
/// historical defect in this class was written inside it. The catalogues are the
/// in-crate half: `#[cfg(test)]` modules that sit beside the enforcement modules
/// with the same privileges a future defect would have, quoting each forbidden
/// spelling next to the diagnostic it produces.
///
/// They are documentation, so nothing but this gate stops them being deleted the
/// first time one becomes inconvenient.
#[test]
fn the_in_crate_bypass_catalogues_are_still_there() {
    for module in ["matrix.rs", "file.rs"] {
        assert!(
            crate_source(module).contains("mod bypass_catalogue {"),
            "{module} must keep its in-crate bypass catalogue: the `tests/ui` fixtures prove              only that a downstream crate cannot forge these witnesses, and no defect in this              class has ever come from downstream"
        );
    }
    let matrix = crate_source("matrix.rs");
    for quoted in [
        "let _ = FatalAccessGate::new(false);",
        "let _ = CrcValidEvidence::new(bits, true);",
        "let _ = PageIndexEnumeration::fully_enumerated();",
        "let _ = FatalAccessAllowed(());",
        // Round 16. The first two are the one-line reassignment and the clone
        // that re-verification found still compiling; the third is the mirror
        // laundering it *executed*, reading a bit no enumeration produced back
        // through the F-04 witness. All three are now compiler refusals, and
        // the catalogue must keep quoting them next to the diagnostic, because
        // a residual that stops being written down is how this class survived
        // rounds 5, 7 and 9 to 15.
        "layout.fatal_access = FatalAccessGate::evaluate(findings.iter(), spec);",
        "layout.fatal_access = donor.fatal_access.clone();",
        "*evidence.page_index_mirror_mut() = attacker_bits;",
    ] {
        assert!(
            matrix.contains(quoted),
            "the catalogue must keep quoting `{quoted}` together with the error it produces; a              future round re-runs these by uncommenting them"
        );
    }
    let file = crate_source("file.rs");
    for quoted in [
        "let _ = ReservedIndexSlot(());",
        "let _ = ReplacementTarget { position: 0 };",
        "let _ = RecordOverwrite { record_offset: 0, payload_offset: 0 };",
    ] {
        assert!(
            file.contains(quoted),
            "the catalogue must keep quoting `{quoted}` together with the error it produces"
        );
    }
}
