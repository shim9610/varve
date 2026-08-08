//! The lazy open and the digest record it reads.
//!
//! `decode_digest_payload` parses a length-prefixed table out of the last
//! record in the file and turns it into offsets the reader then seeks to. Every
//! byte of it is attacker-controllable — it is the tail of a file on disk — and
//! until this target existed it was reachable from none of the seven others:
//! `native_arbitrary` builds a format whose `index:` list does not name the
//! digest, and it calls `open_reader`, never `open_readonly_lazy`. Both halves
//! have to be true at once for the decoder to run, so neither the option nor
//! the entry point alone would have closed it.
//!
//! **The oracle is the scan, not the absence of a panic.** A decoder that
//! accepts a corrupt table and hands back plausible-but-wrong offsets crashes
//! nothing; it silently answers a different file. So every case here opens the
//! same bytes twice — once lazily, once by the full scan that is the defined
//! meaning of the file.
//!
//! Where that equality is *owed* is the part worth stating precisely, because
//! the first version of this target asserted it everywhere and was wrong to.
//! A lazy open reads eleven syscalls' worth of the file; the scan's ability to
//! notice mid-file rot is a by-product of framing every record, which is the
//! cost the digest exists to avoid. On a file with a flipped byte in the middle
//! the two legitimately disagree, and [`compare_routes`] says which cases still
//! owe an identical answer and which owe only soundness.

use std::fs;
use std::path::Path;

use varve::{FormatSpec, VarveBlock, VarveFile, varve_format};

varve_format! {
    pub format FuzzDigestFormat {
        magic: b"FZDG";
        version: 1;
        limits {
            file_len: 2_097_152;
            records: 4_096;
            index_bytes: 1_048_576;
            scan_bytes: 2_097_152;
            record_payload: 1_048_576;
            logical_payload: 2_097_152;
            materialized_bytes: 4_194_304;
            segments: 4_096;
            matrix_dimension: 1_024;
            matrix_cells: 65_536;
            matrix_bitmap: 1_048_576;
            matrix_crc: 1_048_576;
            matrix_metadata: 1_048_576;
            matrix_slot_region: 2_097_152;
            sidecar: 2_097_152;
            mmap: 2_097_152;
        }
        endian: little;
        integrity: crc32;
        index: block_offset_chain;
        commit: transaction_marker(on_flush);
        blocks {
            fixed DigestLine(id = 1) {
                value: u32,
            }

            variable DigestNote(id = 2) {
                body: String,
            }
        }
    }
}

/// The spec under test: the digest on, which the DSL has no clause for, so it
/// is set on the policy the way a caller would.
fn digest_spec() -> FormatSpec {
    FuzzDigestFormat::spec().with_index_policy(
        FuzzDigestFormat::spec()
            .index_policy
            .with_open_digest_on_flush(true),
    )
}

/// How many bytes at the end a mutation is allowed to treat as "the digest".
///
/// The digest record is last and small — a header, twelve bytes per block id,
/// and a trailer — so a mutation aimed anywhere in the file would spend nearly
/// all its budget on record bodies the decoder never sees. Mode 0 therefore
/// writes so the mutation *ends at end-of-file*: the first version anchored at
/// the window's start instead, and a typical short input then corrupted bytes
/// five hundred before the digest and never touched it.
const TAIL_WINDOW: usize = 512;

/// How much the two routes owe each other on this file.
#[derive(Clone, Copy, PartialEq)]
enum Oracle {
    /// The unmutated fixture, valid by construction: nothing may fail. The
    /// scan, the lazy open, and the map walk must all succeed and agree. This
    /// pass is the target's regression detector — without it, a lazy open that
    /// started erroring on every valid file would silently turn the whole
    /// target into one that asserts nothing.
    Pristine,
    /// Appended past the last commit — the case the digest's `physical_end`
    /// exists for. Both routes see the same committed prefix, so they must
    /// agree whenever the scan can read the file.
    Appended,
    /// Truncated or rotted: only soundness is owed. A lazy open cannot see
    /// mid-file rot by construction, so equality is not.
    Mutated,
}

/// The shape a scan says the file has, as the thing every other route answers to.
fn scanned_shape(
    spec: FormatSpec,
    path: &Path,
) -> Option<(Vec<(u32, u64, u64)>, Vec<Option<u64>>)> {
    let file = VarveFile::open_readonly(spec, path).ok()?;
    let entries = file
        .index_entries()
        .iter()
        .map(|entry| (entry.block_id, entry.record_offset, entry.payload_len))
        .collect();
    let tails = vec![
        file.block_tail_offset(DigestLine::ID),
        file.block_tail_offset(DigestNote::ID),
    ];
    Some((entries, tails))
}

/// Writes a file that really does carry a digest, so the mutation has one to
/// corrupt. A target that started from arbitrary bytes would spend its whole
/// budget failing the magic check four bytes in.
fn create_fixture(spec: FormatSpec, path: &Path, seed: u8) -> bool {
    let Ok(mut file) = spec.create(path) else {
        return false;
    };
    let lines = u32::from(seed) * 3 + 8;
    let per_flush = u32::from(seed % 16) + 4;
    for value in 0..lines {
        if file.push(&DigestLine { value }).is_err() {
            return false;
        }
        if value % 5 == 4
            && file
                .push(&DigestNote {
                    body: format!("note {value}"),
                })
                .is_err()
        {
            return false;
        }
        if (value + 1) % per_flush == 0 && file.flush().is_err() {
            return false;
        }
    }
    file.flush().is_ok()
}

/// Applies the fuzzer's bytes to the file, biased at the tail where the digest
/// lives. Returns false when there is nothing to open afterwards.
fn mutate(path: &Path, control: u8, mutations: &[u8]) -> bool {
    let Ok(mut bytes) = fs::read(path) else {
        return false;
    };
    if bytes.is_empty() {
        return false;
    }

    match control % 4 {
        // Overwrite ending at end-of-file, where the digest actually is: its
        // trailer is the last eight bytes and the whole record is ~50, so a
        // write that *ends* at EOF hits decoder fields with any input length.
        0 => {
            let span = mutations.len().min(TAIL_WINDOW).min(bytes.len());
            let start = bytes.len() - span;
            bytes[start..].copy_from_slice(&mutations[..span]);
        }
        // Truncate: a digest cut short, and a file whose last commit is gone.
        1 => {
            let cut = mutations
                .first()
                .map(|byte| usize::from(*byte) * bytes.len() / 256)
                .unwrap_or(bytes.len() / 2);
            bytes.truncate(cut);
        }
        // Append past the last commit, which is the case the digest's
        // `physical_end` exists to notice.
        2 => bytes.extend_from_slice(mutations),
        // Anywhere, so the target does not only ever see a well-formed prefix.
        _ => {
            for pair in mutations.chunks_exact(3) {
                let at = (usize::from(pair[0]) << 8 | usize::from(pair[1])) % bytes.len();
                bytes[at] = pair[2];
            }
        }
    }

    fs::write(path, &bytes).is_ok()
}

pub fn run_digest(data: &[u8]) {
    let Ok(root) = tempfile::tempdir() else {
        return;
    };
    let control = data.first().copied().unwrap_or(0);
    let seed = data.get(1).copied().unwrap_or(0);
    let mutations = data.get(2..).unwrap_or_default();
    let spec = digest_spec();

    let path = root.path().join("digest.varve");
    if !create_fixture(spec, &path, seed) {
        return;
    }

    // The unmutated file first: this half is not about corruption at all, it is
    // the assertion that the fast route and the slow route agree on a file that
    // is simply valid — and that everything *works* on it, which is what keeps
    // the mutation cases from becoming a target that silently asserts nothing.
    compare_routes(spec, &path, Oracle::Pristine);

    let mode = control % 4;
    if !mutate(&path, control, mutations) {
        return;
    }
    compare_routes(
        spec,
        &path,
        if mode == 2 {
            Oracle::Appended
        } else {
            Oracle::Mutated
        },
    );
}

/// Opens the same bytes both ways.
///
/// The [`Oracle`] is the whole subtlety of this target, and getting it wrong
/// is what the first run of it did. **A lazy open cannot agree with the scan
/// on a file with mid-file rot, and that is not a defect.** The scan detects a
/// corrupt record only because it frames every record; when it hits one it
/// stops, and its idea of a block's tail becomes the last good record *before*
/// the damage. The digest sits at the end, is checksummed independently, and
/// reports the true tail past it. Measured: one flipped byte at offset 17969
/// put the scan's tail at 16804 and the digest's at 37868 — both correct for
/// their own definition. Requiring equality there would be requiring the lazy
/// open to do the hundred thousand syscalls it exists to avoid.
///
/// So equality is asserted exactly where it is owed — [`Oracle::Pristine`] and
/// [`Oracle::Appended`] — and for truncation and rot the requirement is that
/// the path be sound, which the sanitizers and the read-back below judge.
///
/// One property holds in *every* mode: **the lazy open may not fail where the
/// scan succeeds.** `open_readonly_lazy_with_report`'s contract is that every
/// digest failure short of a spec-level refusal falls back to exactly the scan
/// — so on a file the scan can read, `Err` is not a permitted degradation, it
/// is the fallback broken. The first version of this function excused those
/// errors in every mode, which would have hidden a lazy open regressed to
/// erroring on every valid file.
fn compare_routes(spec: FormatSpec, path: &Path, oracle: Oracle) {
    // The scan is read first and is never allowed to be the thing that changed
    // the file — `open_readonly` does not recover in place, so both routes see
    // identical bytes.
    let scanned = scanned_shape(spec, path);
    if oracle == Oracle::Pristine {
        assert!(
            scanned.is_some(),
            "the scan refused the unmutated fixture it just wrote"
        );
    }
    let Some((scanned_entries, scanned_tails)) = scanned else {
        // The scan refuses these bytes. The lazy open may still legitimately
        // succeed — a digest at the end is checksummed independently of the
        // mid-file damage that stopped the scan — so nothing further is owed
        // beyond not misbehaving, which libFuzzer's sanitizers judge.
        let _ = VarveFile::open_readonly_lazy_with_report(spec, path);
        return;
    };

    let (file, source) = match VarveFile::open_readonly_lazy_with_report(spec, path) {
        Ok(opened) => opened,
        Err(error) => panic!(
            "the lazy open failed where the scan succeeds; its fallback is that scan: {error:?}"
        ),
    };

    let line_tail = file.block_tail_offset(DigestLine::ID);
    let note_tail = file.block_tail_offset(DigestNote::ID);

    if oracle != Oracle::Mutated {
        // Whichever route it took, the facts must be the scan's.
        assert_eq!(
            line_tail, scanned_tails[0],
            "lazy open ({source:?}) disagrees with the scan on the DigestLine tail"
        );
        assert_eq!(
            note_tail, scanned_tails[1],
            "lazy open ({source:?}) disagrees with the scan on the DigestNote tail"
        );
    }

    // Sound on any file, mutated or not: an offset the digest hands back is one
    // the reader will seek to, so it has to name a record of the block the
    // digest said it did. `read_block_at` refuses a mismatched id, so a decoder
    // that let a corrupt table through would surface here as an error rather
    // than as a wrong-typed read — and a nonsensical offset would surface to
    // the sanitizers. An `Err` is a fine answer: rot at the tail record is
    // caught by its own checksum, which is detection deferred, not lost.
    if let Some(offset) = line_tail {
        let _ = file.read_block_at::<DigestLine>(offset);
    }
    if let Some(offset) = note_tail {
        let _ = file.read_block_at::<DigestNote>(offset);
    }

    // A digest open keeps no directory, so the map is the only way to ask it
    // where the file ends. A handle that stopped one record short would show up
    // here and nowhere else. On the unmutated fixture the walk itself must
    // work — a broken `record_map` would otherwise skip this block silently on
    // every input, and the comparison below would never run again.
    let mut buffer = Vec::new();
    match file.record_map(&mut buffer) {
        Ok(mut map) => {
            let filled = map.fill();
            if oracle == Oracle::Pristine {
                filled
                    .as_ref()
                    .expect("the map walk failed on the unmutated fixture");
            }
            if filled.is_ok() && oracle != Oracle::Mutated {
                let walked: Vec<_> = map
                    .entries()
                    .iter()
                    .map(|entry| (entry.block_id, entry.record_offset, entry.payload_len))
                    .collect();
                assert_eq!(
                    walked, scanned_entries,
                    "the map walked from a lazy open ({source:?}) is not the scan's index"
                );
            }
        }
        Err(error) => {
            if oracle == Oracle::Pristine {
                panic!("record_map failed on the unmutated fixture: {error:?}");
            }
        }
    }

    // No assertion ties `source` to the file's tail, deliberately. The obvious
    // one — "a truncated file must answer FullScan" — is wrong: a truncation
    // that lands exactly on an earlier flush's boundary leaves that flush's
    // digest as the last record, and a Digest answer is then correct. And
    // re-checking the spec flag here would be a tautology; the spec never
    // changes. What Digest-vs-FullScan is owed is already asserted above: the
    // same answers as the scan, on any file the scan can read.
}
