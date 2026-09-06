//! Documentation-truth regression gate.
//!
//! Invariant 4 (TRUTHFUL) is the one invariant this repository has repeatedly
//! failed to hold after it was *already* corrected once: a false claim was
//! removed from one file and left standing verbatim in the design document, the
//! specification, the changelog, and the crate rustdoc that `cargo doc`
//! publishes to users. Nothing failed, because no test and no CI gate reads
//! prose.
//!
//! This test reads prose. It is deliberately mechanical: it forbids the exact
//! sentences that were retracted, and it compares the layout version the
//! documentation states against the layout version the code actually writes, so
//! a future version bump that updates only some documents fails here instead of
//! shipping.
//!
//! Scope: the normative text a user can read — `README.md`, `CHANGELOG.md`,
//! everything under `docs/`, and every `.rs` source file under `crates/`.
//! Review and audit documents under `docs/` are excluded, and only those: a
//! review's job is to quote the defective sentence verbatim, so forbidding the
//! quotation there would forbid reporting the defect. Their filenames contain
//! `review`. No other exclusion exists, and in particular this file itself is
//! *not* excluded — the forbidden strings below are assembled from fragments so
//! that the gate cannot pass merely by exempting its own source.

use std::fs;
use std::path::{Path, PathBuf};

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("crates/varve has a workspace root two levels up")
        .to_path_buf()
}

/// Every normative file, as `(repo-relative path, contents)`.
fn normative_files() -> Vec<(String, String)> {
    let root = repo_root();
    let mut files = Vec::new();
    for name in ["README.md", "CHANGELOG.md"] {
        let path = root.join(name);
        let text = fs::read_to_string(&path).unwrap_or_else(|err| panic!("{name}: {err}"));
        files.push((name.to_string(), text));
    }
    collect(&root.join("docs"), "md", &root, &mut files);
    collect(&root.join("crates"), "rs", &root, &mut files);
    assert!(
        files.len() > 20,
        "the normative file set collapsed to {} entries; the walk is broken and \
         this gate would pass vacuously",
        files.len()
    );
    files
}

fn collect(dir: &Path, extension: &str, root: &Path, out: &mut Vec<(String, String)>) {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(err) => panic!("read_dir {}: {err}", dir.display()),
    };
    for entry in entries {
        let path = entry.expect("directory entry").path();
        if path.is_dir() {
            // `target/` never appears under docs/ or crates/*/src, but a stray
            // build directory inside a crate must not be scanned.
            if path.file_name().is_some_and(|name| name == "target") {
                continue;
            }
            collect(&path, extension, root, out);
            continue;
        }
        if path.extension().is_none_or(|ext| ext != extension) {
            continue;
        }
        let relative = path
            .strip_prefix(root)
            .unwrap_or(&path)
            .to_string_lossy()
            .replace('\\', "/");
        // Review/audit documents quote the defect they report, by definition.
        if relative.starts_with("docs/") && relative.contains("review") {
            continue;
        }
        let text = fs::read_to_string(&path).unwrap_or_else(|err| panic!("{relative}: {err}"));
        out.push((relative, text));
    }
}

/// Reads the layout version the code actually writes and enforces.
fn shipped_vmat_version() -> u16 {
    let source = fs::read_to_string(repo_root().join("crates/varve-core/src/matrix.rs"))
        .expect("crates/varve-core/src/matrix.rs is readable");
    let marker = "const VMAT_VERSION: u16 = ";
    let start = source
        .find(marker)
        .expect("matrix.rs declares const VMAT_VERSION")
        + marker.len();
    let rest = &source[start..];
    let end = rest.find(';').expect("VMAT_VERSION declaration terminates");
    rest[..end]
        .trim()
        .parse()
        .expect("VMAT_VERSION is a u16 literal")
}

/// Every numeral a current-state claim of the given shape publishes, as
/// `(repo-relative path, line number, numeral)`.
///
/// The claim is located by a literal prefix; the numeral is whatever decimal
/// digits immediately follow it. `suffix`, when non-empty, must follow the
/// digits, so `## VMAT Version 4 Header` is a claim and `## VMAT Version 4
/// Rationale` is not.
///
/// This exists because iterating the versions *below* the shipped one only
/// catches text left behind by a bump. It cannot catch text that ran ahead of
/// the code — a document edited to `5` while `VMAT_VERSION` is still `4` is
/// equally false, and is exactly what a half-applied correction produces.
fn published_version_claims(
    claims: &[(String, String)],
    prefix: &str,
    suffix: &str,
) -> Vec<(String, usize, u16)> {
    let mut found = Vec::new();
    for (path, text) in claims {
        for (number, line) in text.lines().enumerate() {
            let mut rest = line;
            while let Some(at) = rest.find(prefix) {
                let after = &rest[at + prefix.len()..];
                let digits: String = after.chars().take_while(char::is_ascii_digit).collect();
                let tail = &after[digits.len()..];
                if !digits.is_empty() && tail.starts_with(suffix) {
                    let value = digits.parse().unwrap_or_else(|_| {
                        panic!("{path}:{}: {digits:?} is not a u16", number + 1)
                    });
                    found.push((path.clone(), number + 1, value));
                }
                rest = &rest[at + prefix.len()..];
            }
        }
    }
    found
}

/// The versions an artifact can carry that this build refuses, rendered the way
/// the documentation renders them: `1`, `1 or 2`, `1, 2, or 3`, ...
fn refused_versions_phrase(shipped: u16) -> String {
    let stale: Vec<String> = (1..shipped).map(|version| version.to_string()).collect();
    match stale.len() {
        0 => String::new(),
        1 => stale[0].clone(),
        2 => format!("{} or {}", stale[0], stale[1]),
        _ => {
            let (last, head) = stale.split_last().expect("at least three entries");
            format!("{}, or {last}", head.join(", "))
        }
    }
}

fn assert_absent(claims: &[(String, String)], needle: &str, why: &str) {
    let mut offenders = Vec::new();
    for (path, text) in claims {
        for (number, line) in text.lines().enumerate() {
            if line.contains(needle) {
                offenders.push(format!("{path}:{}", number + 1));
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "retracted claim {needle:?} is still published in {offenders:?}.\n{why}"
    );
}

fn assert_present(claims: &[(String, String)], path_suffix: &str, needle: &str, why: &str) {
    let (path, text) = claims
        .iter()
        .find(|(path, _)| path.ends_with(path_suffix))
        .unwrap_or_else(|| panic!("{path_suffix} is not in the normative file set"));
    assert!(
        text.contains(needle),
        "{path} no longer states {needle:?}.\n{why}"
    );
}

/// The cost claim for matrix open that the review retracted.
///
/// Open enumerates the union of the persisted page index and the filesystem
/// allocation map in `O(Q)` over candidate pages. Stating the cost in terms of
/// the bytes the matrix has written is the claim that was found too strong, and
/// deleting it from the first document that carried it left it standing verbatim
/// in the matrix design notes and in the `matrix.rs` rustdoc — which is why this
/// gate sweeps every normative file instead of the one that was corrected.
#[test]
fn the_retracted_matrix_open_cost_claim_is_not_published_anywhere() {
    let files = normative_files();
    let claim = format!("O({} actually written)", "bytes");
    assert_absent(
        &files,
        &claim,
        "Matrix open costs O(Q) over the union of the persisted page index and \
         the filesystem allocation map, independently of the logical width and \
         of how many pages the matrix published historically. State that, not a \
         bytes-written figure.",
    );
}

/// The pre-v3 full-logical-scan fallback, which no longer exists.
///
/// `for_each_candidate_page` streams only the persisted index pages when the
/// allocation map is absent, so no full scan occurs. Text describing one is not
/// merely stale, it is false: it tells a reader that a missing allocation map
/// costs `Theta(cells / 8)` at open.
#[test]
fn the_deleted_full_scan_fallback_is_not_described_anywhere() {
    let files = normative_files();
    let tail = "read exactly as";
    for variant in [
        format!("every page is {tail} before"),
        format!("every page is {tail} it was before"),
        format!("every page was {tail} it was before"),
        format!("every byte is {tail} before"),
    ] {
        assert_absent(
            &files,
            &variant,
            "There is no full-logical-scan fallback since VMAT v3. Without an \
             allocation map the persisted page index alone drives enumeration \
             and open still costs O(live pages); the only loss is the ability \
             to skip reading an indexed page.",
        );
    }
}

/// Current-state documentation must name the layout version the code writes.
///
/// Superseded versions may be described historically ("layout version 3 added
/// the page-index region"), but the header section, the endianness statement,
/// the header diagram, and the specification's storage sentence describe the
/// artifact a reader will actually find on disk.
#[test]
fn documented_vmat_layout_version_matches_the_shipped_constant() {
    let files = normative_files();
    let shipped = shipped_vmat_version();
    assert!(shipped >= 4, "unexpected VMAT_VERSION {shipped}");

    // Every current-state claim, whatever numeral it carries, must name the
    // shipped version. Checking the shape rather than a list of stale numerals
    // catches text that ran ahead of the code as well as text left behind by a
    // bump, and it catches a *second* copy of the claim in a file whose first
    // copy was corrected — the half-applied fix that produced this gate.
    let mut offenders = Vec::new();
    for (prefix, suffix) in [
        ("## VMAT Version ", " Header"),
        ("layout_version        u16 = ", ""),
        ("little-endian in layout version ", ""),
        ("`VMAT` layout version ", " stores"),
        ("matrix layout is `VMAT` version ", ""),
        ("| Matrix layout | `VMAT_VERSION` | **", "**"),
    ] {
        for (path, line, version) in published_version_claims(&files, prefix, suffix) {
            if version != shipped {
                offenders.push(format!("{path}:{line} says {prefix}{version}{suffix}"));
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "these describe the current on-disk artifact but do not name the \
         shipped VMAT layout version {shipped}: {offenders:?}.\nThe code writes \
         and enforces VMAT_VERSION = {shipped} and refuses every other version \
         with FormatVersionMismatch."
    );

    let why = "The shipped VMAT layout version changed; update the current-state \
               documentation to match crates/varve-core/src/matrix.rs.";
    // Two shapes swept above are, for now, pinned by nothing, because no
    // published file carries them: the byte-level `VMAT` version N header
    // diagram (the `layout_version        u16 = N` field and the field order and
    // widths around it) and the statement that all integers in `VMAT` metadata
    // are little-endian in layout version N. Both lived only in the matrix
    // storage design notes, which are no longer published. They belong in
    // docs/spec.md under "Preallocated Matrix Wire Contract", which today states
    // the header's field *set* and the field-13/14 ordering but neither the
    // widths nor the metadata endianness. Their prefixes stay in the sweep so a
    // stale numeral is caught the moment they land there.
    assert_present(
        &files,
        "docs/api-changes.md",
        &format!("| Matrix layout | `VMAT_VERSION` | **{shipped}** |"),
        why,
    );
    assert_present(
        &files,
        "docs/spec.md",
        &format!("`VMAT` layout version {shipped} stores"),
        why,
    );
    assert_present(
        &files,
        "docs/migration-guide.md",
        &format!("matrix layout is `VMAT` version {shipped}"),
        why,
    );
}

/// Exactly one `FormatVersionMismatch` contract may be published for `VMAT`.
///
/// The defect this closes is narrower and nastier than a stale sentence: a
/// correction pass updated one bullet of a release section to the version-4
/// contract and left an earlier bullet of the *same* section publishing
/// `expected: 3, actual: 2`. Both bullets were internally coherent, so reading
/// either one alone showed nothing wrong. A reader matching on the error would
/// have written code against a value this build never returns.
///
/// So the rule is per document, not per sentence: in any normative prose that
/// discusses `VMAT`, every `expected: N` must be the version the code enforces,
/// and the refusal list must be the versions the code actually refuses.
#[test]
fn only_the_shipped_vmat_version_mismatch_contract_is_published() {
    let files = normative_files();
    let shipped = shipped_vmat_version();
    let phrase = refused_versions_phrase(shipped);

    // Rust sources carry `expected:` in unrelated struct literals (container
    // versions, sidecar records, test fixtures), so the paragraph sweep is
    // scoped to prose. The Rust side of this contract is asserted by
    // `crates/varve/tests/matrix.rs`, which constructs the error itself.
    let prose: Vec<(String, String)> = files
        .iter()
        .filter(|(path, _)| path.ends_with(".md"))
        .cloned()
        .collect();
    assert!(
        prose.len() > 10,
        "the prose file set collapsed to {}",
        prose.len()
    );

    let mut offenders = Vec::new();
    for (path, text) in &prose {
        for paragraph in text.split("\n\n") {
            if !paragraph.contains("VMAT") {
                continue;
            }
            for (_, _, version) in
                published_version_claims(&[(path.clone(), paragraph.to_string())], "expected: ", "")
            {
                if version != shipped {
                    offenders.push(format!(
                        "{path}: a VMAT paragraph publishes expected: {version}"
                    ));
                }
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "{offenders:?}\nThe code returns FormatVersionMismatch {{ expected: \
         {shipped}, .. }}. A superseded contract must be rewritten or deleted, \
         not left standing beside the current one."
    );

    let canonical =
        format!("`Error::FormatVersionMismatch {{ expected: {shipped}, actual: <{phrase}> }}`");
    let why = "The refusal contract is generated from VMAT_VERSION: every \
               version below it is refused as stale-regenerable. Update the \
               rendered contract wherever it is published.";
    // These are every published normative file that renders the contract in
    // full. The matrix design and matrix final-spec notes rendered it too and
    // were checked here; they are no longer published, and the contract they
    // carried is unchanged and still pinned by the three below.
    // docs/api-reference.md renders it in table form and is checked after this.
    for document in ["docs/spec.md", "docs/migration-guide.md", "CHANGELOG.md"] {
        assert_present(&prose, document, &canonical, why);
    }
    assert_present(
        &prose,
        "docs/api-reference.md",
        &format!("`VMAT` layout version {phrase} matrix file (v{shipped} is current"),
        why,
    );
}

/// The zero-range counters are thread-local and per *request*, not per
/// operation. Documentation that omits either qualification tells a caller it
/// can prove the cheap path when it cannot.
#[test]
fn the_zero_range_counter_qualifications_are_documented() {
    let files = normative_files();
    // The scale-contract notes that carried these qualifications are no longer
    // published; docs/known-limitations.md §1.7 is their published home and
    // states both scope rules in place.
    for needle in [
        "matrix_total_zero_range_streamed_bytes",
        "thread-local",
        "does not qualify the whole clear",
    ] {
        assert_present(
            &files,
            "docs/known-limitations.md",
            needle,
            "F-08's runtime proof must document that the counters are \
             thread-local and that the whole-operation proof is the before/after \
             delta of the cumulative counter, not the last-request counter.",
        );
    }
}

/// The keyed-tail charge names its own limit, and covers the map's build.
///
/// Two false claims shipped with the round-6 keyed-tail work and are retracted
/// here:
///
/// 1. The rustdoc on `VarveWriter::reserve_keyed_tail_slot` said the growth is
///    charged against `ReadLimits::max_index_bytes`. The code checks
///    `ReadLimitKey::KeyedTailBytes`, i.e. `max_keyed_tail_bytes`. Naming the
///    wrong limit is worse than naming none: a caller raises a ceiling that
///    cannot bind and believes the growth is now admitted.
/// 2. `docs/api-reference.md`, and the `limits { }` key table that documented
///    the same ceiling, said the limit
///    "bounds the resident keyed-tail cache" while the generated writers
///    built their whole tail map from file content, at construction, with no
///    charge at all. The ceiling - including `UNTRUSTED`'s 256 MiB - therefore
///    did not bound what an attacker-chosen key count made a writer allocate.
///
/// Both are fixed; this gate keeps them fixed. The build charge is asserted by
/// `crates/varve/tests/resident_contracts.rs`, which is where behaviour belongs;
/// what is asserted here is that the prose says so.
#[test]
fn the_keyed_tail_charge_is_documented_against_its_own_limit_and_its_build() {
    let files = normative_files();

    let wrong_limit = format!("charged** against [`ReadLimits::max_{}_bytes`]", "index");
    assert_absent(
        &files,
        &wrong_limit,
        "The keyed-tail slot reservation checks `ReadLimitKey::KeyedTailBytes`, \
         i.e. `ReadLimits::max_keyed_tail_bytes`. Name that limit, not \
         `max_index_bytes`.",
    );

    for needle in ["initial build", "key_tail_offsets", "structural **peak**"] {
        assert_present(
            &files,
            "docs/api-reference.md",
            needle,
            "API3-05: the map the generated writers and the resident cache \
             start from is built from file content by \
             `VarveFile::key_tail_offsets`. That build is charged, and its \
             charge is the structural peak - larger than the map it produces. \
             Both facts must be published, because a ceiling sized from the \
             steady-state map alone can refuse an open.",
        );
    }

    // The `limits { }` key table that also carried this row is no longer
    // published, so docs/api-reference.md is its only published home. It states
    // the same thing in prose, and this pins the half the table made explicit:
    // the charge covers the build, not only the growth.
    assert_present(
        &files,
        "docs/api-reference.md",
        "its later growth are both refused",
        "The keyed-tail charge covers the map's initial build from file content \
         as well as its later growth. Both halves must be published, or a caller \
         sizes the ceiling for the growth alone and an open it expects to \
         succeed is refused.",
    );
}

/// The `VDIG` payload's flags word and its sequence field, as the code writes
/// them.
///
/// Two claims in the specification's digest layout were retracted by the change
/// that made the sequence field's presence a flag:
///
/// 1. The flags word was published as "currently `0`". Bit 0 is
///    `DIGEST_FLAG_SEQUENCE`, and both writing paths always have a mark to
///    report, so every digest varve writes sets it and the word is `1`. A
///    parser that requires a zero flags word refuses every digest there is.
/// 2. The sequence field was published with a reserved value meaning that no
///    record carries a sequence. There is no reserved value: absence is the
///    flag bit being clear, and the largest `u64` is a mark like any other, so
///    a parser applying the retracted rule reads a file's last usable sequence
///    as an absent one.
///
/// The file-observable half is measured by `crates/varve/tests/open_digest.rs`,
/// which parses a real digest out of a real file the way this section tells a
/// reader to; what is asserted here is that the prose says the same thing.
#[test]
fn the_retracted_digest_sequence_sentinel_is_not_published_anywhere() {
    let files = normative_files();
    let sentinel = format!("or `u64::MAX` for {}", "\"no record carries one\"");
    assert_absent(
        &files,
        &sentinel,
        "The digest's sequence field is meaningful only when flags bit 0 \
         (`DIGEST_FLAG_SEQUENCE`, `0x0001`) is set, and is written as `0` when \
         that bit is clear. No value is reserved: `u64::MAX` is a mark like any \
         other, which `the_last_sequence_number_is_not_the_absence_of_one` in \
         crates/varve-core/src/file.rs pins.",
    );

    // The flags half cannot be forbidden by its text: the `VSEG` bullet above
    // publishes the same words truthfully, because a segment's flags word
    // really is `0`. What is pinned instead is that the digest bullet states
    // the flag and the absence of a reserved value.
    for needle in [
        "bit 0 (`0x0001`) says the sequence field carries a mark",
        "**No value is reserved**",
    ] {
        assert_present(
            &files,
            "docs/spec.md",
            needle,
            "The `VDIG` flags word is `1` in every digest varve writes, and the \
             sequence field it guards has no reserved encoding. Both must be \
             published, or a parser written from this section refuses every \
             digest and misreads the one number it does accept.",
        );
    }
}

/// No document may still say a matrix reader captures commit maps at open.
///
/// This is the exact failure mode this file exists for.
/// `MatrixMetadataResidency` has two variants — `Missing` and `Lazy` — and
/// `DEFAULT` is `Lazy`, so a commit-map page is as of the first read that
/// faulted it in. When 0.5.0 removed the whole-live-set `EagerVerified` policy
/// the claim was corrected in `README.md`, `docs/api-reference.md`,
/// `docs/known-limitations.md` and `docs/durability-model.md`, and left
/// standing in `docs/quickstart.md` and `docs/format-author-guide.md` — in the
/// first stated as the *opposite* of the concurrency rule, in the document a
/// new user reads first, for four minor releases.
///
/// Two documents, two wordings, which is why the needles below are a list and
/// not one string: a sweep for the quickstart's sentence alone finds one of
/// them and reports the file set clean.
///
/// The behaviour is asserted by `crates/varve/tests/matrix_lazy_residency.rs`,
/// in `a_commit_map_page_is_as_of_its_first_touch_not_as_of_open`. What is
/// asserted here is that the prose says so.
#[test]
fn no_document_says_a_matrix_reader_captures_commit_maps_at_open() {
    let files = normative_files();
    // Foldings of the two retracted sentences. `assert_absent` scans line by
    // line, so a needle that spans the 80-column fold would never match; these
    // are the folds each sentence admits.
    let verb = "captured";
    for variant in [
        format!("commit maps are also {verb}"),
        format!("commit maps are {verb}"),
        format!("commit map is {verb}"),
        format!("readers snapshot layout and commit {}", "maps"),
        format!("snapshot layout and commit {}", "maps"),
    ] {
        assert_absent(
            &files,
            &variant,
            "MatrixMetadataResidency::EagerVerified was removed in 0.5.0 and \
             the unset default resolves to Lazy, so each commit-map page is as \
             of the first read that faulted it in, not as of open. A matrix \
             reader owns no whole-map instant; only the matrix layout is \
             snapshotted at open.",
        );
    }

    // These are the two documents that carried the retracted claim, and a
    // deletion is not a correction: each has to state the rule it got wrong.
    for document in ["docs/quickstart.md", "docs/format-author-guide.md"] {
        assert_present(
            &files,
            document,
            "first read that faulted it in",
            "A concurrency paragraph that mentions matrix commit maps must \
             state the first-touch rule for them. Deleting the sentence \
             instead of correcting it leaves a reader with no statement of the \
             one matrix rule that governs reader/writer overlap.",
        );
    }
}

/// The wall-clock scaling gate `matrix_concurrent_reads.rs` no longer has.
///
/// The ratio was demoted twice. `report_the_scaling_of_one_shared_handle`
/// measures 1 thread against N through one handle and *prints* the result,
/// labelled `MEASUREMENT ONLY`; the strict `ratio <= 1.0` form of it,
/// `shared_handle_scaling_beats_one_thread_on_an_idle_host`, is `#[ignore]`d.
/// It was demoted because it could not fail for the reason it was written: two
/// consecutive `ubuntu-latest` runs of identical, healthy code reported 2.14x
/// and 1.43x, and no threshold loose enough to survive that spread would still
/// catch the 1.55x convoy it exists to find.
///
/// What replaced it is counted rather than timed — matrix reads issued while a
/// commit-map page-store lock was held must be zero while reads issued at all
/// must not be — and it is asserted by `varve-core`'s
/// `matrix::page_store_lock_audit_tests` in every feature configuration and,
/// through the public multi-threaded read path, by
/// `no_read_is_issued_while_a_bitmap_page_store_lock_is_held` under
/// `scalable-fault-injection`.
///
/// Publishing the ratio as an assertion is worse than publishing nothing: it
/// offers a green run as evidence of throughput that no green run establishes.
/// So the sweep forbids the retracted sentences, and the presence checks forbid
/// the other half-fix — deleting them and leaving a reader unable to tell a
/// gate from a printed number.
#[test]
fn the_retired_wall_clock_scaling_gate_is_not_published_as_a_contract() {
    let files = normative_files();
    let why = "`matrix_concurrent_reads.rs` prints its 1-vs-N ratio and disclaims it \
               as a gate in the line it prints, and the strict `ratio <= 1.0` form \
               of it is `#[ignore]`d. What it re-checks is the counted invariant: \
               matrix reads issued while a commit-map page-store lock was held must \
               be zero while reads issued at all must not be. Publish what is \
               gated, what is only printed, and what is a manual benchmark - not a \
               wall-clock assertion the suite no longer makes.";
    for claim in [
        format!("asserts the scaling in {}", "wall"),
        format!("two {}-clock scaling contracts", "wall"),
        format!("scaling contracts have never been executed on {}", "Unix"),
        format!("No {} measurement has been published", "Linux"),
        format!("No measurement has been taken {}", "here"),
        format!("contract asserted in the tests is only `<= 1.0{}`", "x"),
    ] {
        assert_absent(&files, &claim, why);
    }

    for needle in [
        "page_store_lock_audit_tests",
        "MEASUREMENT ONLY",
        "`#[ignore]`d manual benchmark",
    ] {
        assert_present(
            &files,
            "docs/api-reference.md",
            needle,
            "The matrix concurrency section must name all three: the counted gate \
             that runs in every feature configuration, the ratio that is measured \
             and printed and decides nothing, and the `#[ignore]`d benchmark that \
             holds the strict threshold. A reader who cannot tell them apart reads \
             a printed number as a passing contract.",
        );
    }
}

/// The editable header region's per-entry framing, as the encoder charges it.
///
/// `capacity` is the entry area, and one entry is
/// `block_id u32 | block_version u16 | reserved u16 | len u32` followed by its
/// payload: twelve bytes, declared by `HEADER_SLOT_ENTRY_HEADER_LEN` and
/// charged against `capacity` by `write_header_slot_bytes` before it refuses
/// with `Error::LimitExceeded`. Eight is a different number from the same
/// section — `HEADER_SLOTS_BLOCK_FRAMING_LEN`, the region's outer
/// `magic[4] | len u32`, counted once and *outside* `capacity` — and it is the
/// one that was published as the per-entry cost.
///
/// That under-budgets the sizing rule by four bytes per block held, and
/// `capacity` is folded into the schema hash, so the file a reader sized from
/// it cannot be widened afterwards. It shipped in two places at once: the
/// format author guide's sizing paragraph, and the rustdoc on
/// `header_slots_free_bytes` that a caller reads instead of the guide.
///
/// Nothing measures the figure itself — `crates/varve/tests/header_slots.rs`
/// exercises the accounting (a rewrite consumes no more of the region, a
/// removal frees it) without asserting the per-entry cost — so the constant is
/// the only source of truth, and this gate reads it rather than restating it.
#[test]
fn the_header_slot_entry_framing_is_published_as_twelve_bytes() {
    let files = normative_files();

    // Derived, not typed a third time: a field added to the entry header moves
    // this sum, and the assertion below then fails until the prose is revisited.
    let source = fs::read_to_string(repo_root().join("crates/varve-core/src/file.rs"))
        .expect("crates/varve-core/src/file.rs is readable");
    let marker = "const HEADER_SLOT_ENTRY_HEADER_LEN: usize = ";
    let start = source
        .find(marker)
        .expect("file.rs declares const HEADER_SLOT_ENTRY_HEADER_LEN")
        + marker.len();
    let rest = &source[start..];
    let end = rest
        .find(';')
        .expect("HEADER_SLOT_ENTRY_HEADER_LEN declaration terminates");
    let framing: usize = rest[..end]
        .split('+')
        .map(|term| term.trim().parse::<usize>().expect("a decimal term"))
        .sum();
    assert_eq!(
        framing, 12,
        "HEADER_SLOT_ENTRY_HEADER_LEN is now {framing}; every needle below \
         spells the number out, so the constant and the prose have to move \
         together."
    );

    let why = "One header-slot entry is `block_id u32 | block_version u16 | \
               reserved u16 | len u32` then its payload: twelve bytes of framing, \
               charged against `capacity`. Eight is \
               HEADER_SLOTS_BLOCK_FRAMING_LEN, the region's outer `magic[4] | len \
               u32`, which is counted once and sits outside `capacity`. Publishing \
               the outer figure as the per-entry one under-budgets the sizing rule \
               by four bytes per block held, and `capacity` is folded into the \
               schema hash, so a file created against the wrong number cannot be \
               widened afterwards.";
    // Two folds, because the guide carries the figure on one line and the
    // rustdoc breaks it across two; a sweep for either alone reports the other
    // file clean.
    for variant in [
        format!("plus {} bytes of entry framing", "eight"),
        format!("payload plus {} bytes of", "eight"),
    ] {
        assert_absent(&files, &variant, why);
    }

    // A deletion is not a correction: both places that carried the wrong figure
    // have to state the right one.
    assert_present(
        &files,
        "docs/format-author-guide.md",
        "twelve bytes of entry framing",
        why,
    );
    assert_present(
        &files,
        "crates/varve-core/src/file.rs",
        "payload plus twelve bytes of",
        why,
    );
}

/// Three assurance claims that were corrected in one document and left standing
/// in another.
///
/// The shape is the one this whole file exists for, applied to what the project
/// says it has verified. "Executed" is a term of art here — `README.md`'s
/// platform table uses it to mean the suite ran on that target — so a claim
/// built on it cannot be read loosely.
///
/// 1. `README.md` called the resident append-log family the only part of the
///    library that any executed suite covers, three bullets above the matrix
///    entry of the same list, in a file that publishes matrix figures labelled
///    *Measured* and records Windows x86_64 as executed. Thirty-one of the
///    tests in `matrix.rs`, `matrix_hardening.rs` and `matrix_limits.rs` carry
///    no feature gate at all.
/// 2. Commit a7ad079 recorded that `disk_index.rs`, `stream.rs`, `indexed.rs`
///    and `scan_control.rs` had been walked against the project's five internal
///    invariants with no defect found, and corrected `README.md` and
///    `docs/scalable-io.md`. §3 of `docs/known-limitations.md` was missed and
///    kept the negative — and that is the section `docs/scalable-io.md` cites as
///    the detail behind its affirmative sentence, so the claim linked to the
///    document denying it.
/// 3. §6.4 of `docs/known-limitations.md` was titled and opened as though no
///    fuzz, Miri or ASan run existed, while its own third bullet, `README.md`
///    and `.github/workflows/sanitizers.yml` all have the three running weekly.
///    The `fuzz-supply-chain` job in `ci.yml` is a supply-chain job; its name is
///    the only fuzzing in it.
///
/// The workflows are the authority for the third claim and this test does not
/// read them, so the presence check names the workflow file rather than
/// restating a schedule out of prose.
#[test]
fn the_superseded_assurance_claims_are_not_still_published() {
    let files = normative_files();

    assert_absent(
        &files,
        &format!("library with an executed test {}", "history"),
        "The default append-log path is not the only family an executed suite \
         covers: 31 of the tests in matrix.rs, matrix_hardening.rs and \
         matrix_limits.rs carry no feature gate, README lists matrix storage \
         under the same heading, and its platform table records Windows x86_64 \
         as executed. State what distinguishes the path - it is the default one \
         - rather than what it is the only one of.",
    );

    let why_walk = "The four scalable modules were walked against the project's \
                    five internal invariants in 0.7.0 and the walk found no \
                    defect. README.md and docs/scalable-io.md say so; a third \
                    copy of the retracted sentence must not survive, least of \
                    all in the section docs/scalable-io.md cites as its \
                    detail. Deleting it is not a correction either - all three \
                    documents have to give the same answer.";
    assert_absent(
        &files,
        &format!(
            "has **not** been walked against the project's five {}",
            "internal"
        ),
        why_walk,
    );

    let why_fuzz = "sanitizers.yml runs fuzz-smoke, asan and miri weekly, \
                    guarded only by the public-repository check. What is true \
                    is the size of the run rather than its absence - 90 seconds \
                    per target is a smoke test, which is what this section's own \
                    bullets already argue and what README.md already says.";
    for claim in [
        format!("CI runs **no {}**", "fuzzing"),
        format!("No fuzz, Miri or ASan run covers this {}", "release"),
    ] {
        assert_absent(&files, &claim, why_fuzz);
    }

    // A deletion is not a correction: the three documents that disagreed have
    // to agree, and the retitled section has to say what is still true.
    assert_present(
        &files,
        "README.md",
        "**have now been walked against the",
        why_walk,
    );
    assert_present(
        &files,
        "docs/scalable-io.md",
        "**has** been walked against the project's",
        why_walk,
    );
    assert_present(
        &files,
        "docs/known-limitations.md",
        "the walk found no defect",
        why_walk,
    );

    for needle in ["weekly smoke, not a campaign", "sanitizers.yml"] {
        assert_present(&files, "docs/known-limitations.md", needle, why_fuzz);
    }
}

/// The record scanner's old name, which four published documents and one
/// source comment still used.
///
/// The whole-region scanner is `scan_records_range`. It was renamed when a
/// reader learned to advance past the end it opened at, and the rename touched
/// nothing outside `file.rs`: the specification, the API reference and two
/// paragraphs of the limitations document went on naming a function that no
/// longer exists, and so did a comment three lines above the call site.
///
/// A dead identifier is worse than a vague sentence. A reader greps for the
/// name a document gave them, finds nothing, and cannot tell whether the
/// function was removed or the document is stale. The old name is spelled here
/// only as a `format!` fragment, for the reason the module comment gives: this
/// sweep reads this file too.
#[test]
fn the_renamed_record_scanner_is_not_published_under_its_old_name() {
    let files = normative_files();
    let dead = format!("scan_records_{}", "from");
    assert_absent(
        &files,
        &dead,
        "The whole-region scanner is `scan_records_range`, in \
         crates/varve-core/src/file.rs. The name this needle forbids was its \
         name until the reader-follow work renamed it, and nothing in the crate \
         answers to it now.",
    );
}

/// No document may still say a matrix dimension cannot grow.
///
/// `FormatSpec::with_growing_matrix_dimension(name, rows_per_chunk)` has
/// been exported and ungated since 0.6.0: rows past the declared extent land in
/// *chunks*, ordinary internal records in the append log, one buffered at a
/// time. `docs/api-changes.md` §C.2 and the 0.7.0 changelog entry describe the
/// editable-chunk half of it in full, and three published documents were never
/// revisited around them — `docs/how-it-works.md` said a matrix block is always
/// a grid whose dimensions are known at create, `docs/known-limitations.md`
/// §1.8 said the growth path did not exist and none was planned, and
/// `README.md` said it twice, once under "Not ready for" and once in the matrix
/// section. Each was internally coherent, which is why reading any one of them
/// showed nothing wrong; the API they contradict is what refutes all four.
///
/// The last needle is the same defect inside the crate: the rustdoc on
/// `GrowingMatrixDimension` still described the pre-0.7.0 refusal, one
/// paragraph below the sentence that documents the feature correctly.
/// `Error::MatrixChunkClosed` is constructed nowhere any more, which its own
/// rustdoc says.
///
/// The behaviour is asserted by `crates/varve/tests/matrix_chunks.rs` —
/// `a_write_into_a_written_chunk_edits_it_in_place` measures that the file
/// length does not change, and `the_growing_dimension_must_be_declared_and_first`
/// pins the declaration refusals. What is asserted here is that the prose says
/// the feature exists.
#[test]
fn no_document_says_a_matrix_dimension_cannot_grow() {
    let files = normative_files();
    let why = "One matrix dimension may be declared growing with \
               FormatSpec::with_growing_matrix_dimension(name, \
               rows_per_chunk), shipped in 0.6.0. Its declared value at create \
               must equal rows_per_chunk - the matrix region is chunk 0 - and \
               rows past it land in ordinary internal records in the append \
               log, one chunk buffered at a time, with a late write into an \
               already-written chunk rewriting that record where it sits. \
               Publish the growth and what it costs, not the absence of it.";
    for claim in [
        format!("cannot represent an {} growing stream", "indefinitely"),
        format!("fixed at create time; there is no {} path", "grow"),
        format!("fixed at create time, with no {} path", "grow"),
        format!("No {} path is planned", "grow"),
        format!("They are for bounded {}", "grids"),
        format!("Only the newest chunk {} writes", "accepts"),
    ] {
        assert_absent(&files, &claim, why);
    }

    // A deletion is not a correction: each document that carried the retracted
    // claim has to name the API that refutes it. §6.9's `header_slots` sentence
    // is a different subject and is untouched by this - that region really
    // cannot grow, and its refusal is a `SchemaHashMismatch`.
    for document in [
        "README.md",
        "docs/how-it-works.md",
        "docs/known-limitations.md",
    ] {
        assert_present(&files, document, "with_growing_matrix_dimension", why);
    }
}

/// No published file may declare an ordered barrier as a `varve_format!` key.
///
/// `docs/durability-model.md` published a fenced `varve_format!` fragment keyed
/// `durability:`, carrying an `ordered_barrier` value, a three-entry `phases`
/// list and a hook flag. None of it has ever existed. The macro's format-key
/// loop ends in a catch-all, so the block does not compile against this tree -
/// `error: unsupported varve_format key`, measured; `git log -S` over `crates/`
/// finds neither the value nor the flag at any revision; and
/// `MatrixDurabilityBarrier` has two sync phases, not three. What is callable is
/// a per-call argument - `write_matrix_cell_durable`, or
/// `write_matrix_cell_durable_with_barrier` with a caller-supplied barrier -
/// which the same document already stated correctly under "Sync Granularity",
/// a hundred and forty lines below the block that contradicted it.
///
/// `doc_dsl_examples.rs` could not have caught this and still cannot: it
/// extracts whole `varve_format!` blocks whose blocks are declared inline, and
/// this was a bare key fragment belonging to no block. After the correction
/// `docs/durability-model.md` carries no `varve_format!` block at all, so adding
/// it to that gate's file list would add no coverage.
///
/// The ban is conditional on the macro rather than fixed, so a `durability:` key
/// that is one day implemented retires this gate instead of turning its own
/// documentation into a test failure.
#[test]
fn no_document_declares_an_ordered_barrier_as_a_format_key() {
    let files = normative_files();
    let key = "durability";
    let macro_source = fs::read_to_string(repo_root().join("crates/varve-macros/src/lib.rs"))
        .expect("crates/varve-macros/src/lib.rs is readable");
    assert!(
        !macro_source.contains(&format!("key == {key:?}")),
        "`{key}` is a varve_format! key now. This gate forbids documenting a key \
         the macro rejects, so retire it rather than editing the documents."
    );

    let why = "The macro's format-key loop ends in a catch-all returning \
               `unsupported varve_format key`, and neither name has ever appeared \
               in crates/ at any revision. The ordered barrier is a per-call \
               argument: `write_matrix_cell_durable`, or \
               `write_matrix_cell_durable_with_barrier` with a caller-supplied \
               `MatrixDurabilityBarrier` - which has two sync phases, not three.";
    for variant in [
        format!("{key}: {}", "ordered_barrier"),
        format!("post_commit_{}", "hook"),
    ] {
        assert_absent(&files, &variant, why);
    }

    // Deleting the fence is not a correction. The section has to say what a
    // caller calls instead, or a reader is left where the retracted block left
    // them: looking for a declaration that does not exist.
    for needle in [
        "write_matrix_cell_durable_with_barrier",
        "There is no `durability:` key",
    ] {
        assert_present(
            &files,
            "docs/durability-model.md",
            needle,
            "The ordered-barrier section must name the callable pair and state \
             that no format key declares them.",
        );
    }
}
