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
        permit.matches("MutationPermit(PhantomData)").count(),
        1,
        "there must be exactly one place where a permit value comes into existence"
    );
    assert!(
        permit.contains("fn writer_permit(&self, context: &'static str) -> Result<MutationPermit<Self>> {\n        self.poison_flag().issue(context)\n    }"),
        "the only exported route to a permit must read the flag of the writer the permit names; \
         a free function taking `&PoisonFlag` would restore the laundering"
    );

    // Belt and braces: no module may hold a `PoisonFlag` anywhere but in a
    // writer's own field, so a decoy flag does not even come into being.
    for module in ["file.rs", "layout.rs", "stream.rs", "indexed.rs"] {
        let source = crate_source(module);
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
    assert!(
        gate.contains("    pub struct RecordFile {\n        file: File,\n    }"),
        "the wrapped handle must stay a private field of `RecordFile`"
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
    if let Some(unit_tests) = outside.find("\n#[cfg(test)]\nmod crc_valid_evidence_tests {") {
        let end = outside[unit_tests + 1..]
            .find("\n}\n")
            .map(|offset| unit_tests + 1 + offset + 3)
            .unwrap_or(outside.len());
        outside.replace_range(unit_tests..end, "\n");
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
        "lending the raw bitmap for page-index maintenance is the one escape from the evidence \
         type (the shared helpers take `&mut SparseBitmap` because commit maps use them too); \
         every extra call site is a fresh route to `SparseBitmap::get`"
    );
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
    assert_eq!(
        source.matches("FatalAccessAllowed(())").count(),
        2,
        "the witness must come into existence in exactly one place — its declaration, and the \
         single `Ok(FatalAccessAllowed(()))` inside `FatalAccessGate::allow`, which IS the \
         refusal"
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
