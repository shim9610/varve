//! The lazy open and the tail table in the *file header*.
//!
//! `decode_header_tails_slots` parses two fixed-length slots out of a region
//! inside the file header and turns them into offsets the reader then seeks to.
//! Every byte of it is attacker-controllable, and unlike every other structure
//! this crate fuzzes it is **rewritten in place at every commit point**, so a
//! half-written slot is an expected state rather than an exotic one.
//!
//! It was reachable from none of the eight existing targets. `native_arbitrary`
//! builds a format whose `index:` list does not name it and calls `open_reader`
//! rather than `open_readonly_lazy`; `digest_arbitrary` declares the *other*
//! option, which this one is refused alongside. Both halves have to be true at
//! once, so neither the option nor the entry point alone would have closed it.
//!
//! **The mutation is anchored at the head, and that is the whole difference
//! from `digest_arbitrary`.** That target writes so its bytes *end* at
//! end-of-file, because the digest is the last record. This region is at a fixed
//! offset near the *start*, so a tail-anchored mutation would spend its whole
//! budget on record bodies the decoder never sees. Mode 0 finds `VBTT` and
//! writes forward from there.
//!
//! **The oracle is the scan, and where equality is owed is narrower than it
//! looks.** A table that survives corroboration can still name an *older*
//! record of the right block — every check the reader applies passes, because
//! nothing proves a tail is the newest of its block, exactly as nothing does
//! for the digest. So equality is asserted on the unmutated fixture and on a
//! file appended past its last commit, and for rot the requirement is
//! soundness.

use std::fs;
use std::path::Path;

use varve::{FormatSpec, LazyOpenSource, VarveBlock, VarveFile, varve_format};

varve_format! {
    pub format FuzzHeaderTailsFormat {
        magic: b"FZHT";
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
        index: header_tails;
        commit: transaction_marker(on_flush);
        blocks {
            fixed TailLine(id = 1) {
                value: u32,
            }

            variable TailNote(id = 2) {
                body: String,
            }
        }
    }
}

/// How many bytes from the start of the region a head-anchored mutation writes.
///
/// The region is `8 + 2 x (28 + 12 x (blocks + 10) + 4)` bytes — 360 for this
/// two-block format — so a window a little larger than that covers both slots
/// and a few bytes past, and stops well before the records.
const HEAD_WINDOW: usize = 448;

/// How much the two routes owe each other on this file.
#[derive(Clone, Copy, PartialEq)]
enum Oracle {
    /// The unmutated fixture, valid by construction: nothing may fail and the
    /// two routes must agree. This pass is the regression detector — without
    /// it, a header route that started refusing every valid file would turn
    /// the mutation cases into a target that asserts nothing.
    Pristine,
    /// Appended past the last commit, by **at least one byte**. The forward
    /// walk cannot reach the end of the file any more, so the table must be
    /// dropped and the open must fall back — and the fallback is the scan, so
    /// the answers must be the scan's.
    Appended,
    /// Truncated or rotted: only soundness is owed. A table that corroborates
    /// may legitimately name an older record of the right block, and a scan
    /// stopped by mid-file damage legitimately reports an earlier tail.
    Mutated,
}

/// The shape a scan says the file has, as the thing every other route answers
/// to.
fn scanned_shape(spec: FormatSpec, path: &Path) -> Option<(Vec<(u32, u64, u64)>, Vec<Option<u64>>)> {
    let file = VarveFile::open_readonly(spec, path).ok()?;
    let entries = file
        .index_entries()
        .iter()
        .map(|entry| (entry.block_id, entry.record_offset, entry.payload_len))
        .collect();
    let tails = vec![
        file.block_tail_offset(TailLine::ID),
        file.block_tail_offset(TailNote::ID),
    ];
    Some((entries, tails))
}

/// Writes a file that really carries a warm table, so the mutation has one to
/// corrupt. A target that started from arbitrary bytes would spend its whole
/// budget failing the magic check four bytes in.
fn create_fixture(spec: FormatSpec, path: &Path, seed: u8) -> bool {
    let Ok(mut file) = spec.create(path) else {
        return false;
    };
    let lines = u32::from(seed) * 3 + 8;
    let per_flush = u32::from(seed % 16) + 4;
    for value in 0..lines {
        if file.push(&TailLine { value }).is_err() {
            return false;
        }
        if value % 5 == 4
            && file
                .push(&TailNote {
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

/// Where the `VBTT` block starts, searched for rather than computed.
///
/// Computing it from the header layout would make the mutation agree with a
/// model of where the region should be; searching finds where the writer
/// actually put it, which is the thing being fuzzed.
fn region_offset(bytes: &[u8]) -> Option<usize> {
    bytes
        .windows(4)
        .take(512)
        .position(|window| window == b"VBTT")
}

/// Applies the fuzzer's bytes to the file, biased at the head where the region
/// lives. Returns false when there is nothing to open afterwards.
fn mutate(path: &Path, control: u8, mutations: &[u8]) -> bool {
    let Ok(mut bytes) = fs::read(path) else {
        return false;
    };
    if bytes.is_empty() {
        return false;
    }

    match control % 4 {
        // Overwrite forward from the region, which is where the decoder reads.
        // A tail-anchored write — what `digest_arbitrary` does — would never
        // reach these bytes at all.
        0 => {
            let Some(start) = region_offset(&bytes) else {
                return false;
            };
            let span = mutations.len().min(HEAD_WINDOW).min(bytes.len() - start);
            bytes[start..start + span].copy_from_slice(&mutations[..span]);
        }
        // Truncate: a file whose last commit is gone, so the offsets the table
        // carries point past the end.
        1 => {
            let cut = mutations
                .first()
                .map(|byte| usize::from(*byte) * bytes.len() / 256)
                .unwrap_or(bytes.len() / 2);
            bytes.truncate(cut);
        }
        // Append past the last commit. The table is still perfectly valid and
        // still names a real commit marker; what it no longer is, is current.
        2 => bytes.extend_from_slice(mutations),
        // Anywhere, so the target does not only ever see a well-formed body.
        _ => {
            for pair in mutations.chunks_exact(3) {
                let at = (usize::from(pair[0]) << 8 | usize::from(pair[1])) % bytes.len();
                bytes[at] = pair[2];
            }
        }
    }

    fs::write(path, &bytes).is_ok()
}

pub fn run_header_tails(data: &[u8]) {
    let Ok(root) = tempfile::tempdir() else {
        return;
    };
    let control = data.first().copied().unwrap_or(0);
    let seed = data.get(1).copied().unwrap_or(0);
    let mutations = data.get(2..).unwrap_or_default();
    let spec = FuzzHeaderTailsFormat::spec();

    let path = root.path().join("header-tails.varve");
    if !create_fixture(spec, &path, seed) {
        return;
    }

    compare_routes(spec, &path, Oracle::Pristine);

    let mode = control % 4;
    if !mutate(&path, control, mutations) {
        return;
    }
    compare_routes(
        spec,
        &path,
        // `mutations` empty means mode 2 appended nothing, so the file is the
        // one that was just written and its table is still current. The first
        // run of this target asserted `FullScan` there and libFuzzer found the
        // one-byte input that proves it wrong inside three minutes — the
        // oracle was, not the code.
        if mode == 2 && !mutations.is_empty() {
            Oracle::Appended
        } else {
            Oracle::Mutated
        },
    );
}

/// Opens the same bytes both ways.
///
/// One property holds in every mode: **the lazy open may not fail where the
/// scan succeeds.** The header route's contract is that any failure short of a
/// spec-level refusal falls back to exactly that scan, so on a file the scan
/// can read, `Err` is not a permitted degradation — it is the fallback broken.
///
/// Equality is asserted where it is owed and not elsewhere, which for this
/// structure is a sharper distinction than it was for the digest. Corroboration
/// proves that the table names the newest *commit marker*; it does not prove
/// that each tail is the newest record of its block, because nothing reads far
/// enough to know that — the same gap the digest has, and for the same reason.
/// A crafted table can therefore pass every check and still disagree with the
/// scan, which is why [`Oracle::Mutated`] owes soundness only.
fn compare_routes(spec: FormatSpec, path: &Path, oracle: Oracle) {
    let scanned = scanned_shape(spec, path);
    if oracle == Oracle::Pristine {
        assert!(
            scanned.is_some(),
            "the scan refused the unmutated fixture it just wrote"
        );
    }
    let Some((scanned_entries, scanned_tails)) = scanned else {
        // The scan refuses these bytes. The header route may still legitimately
        // succeed — the region is checksummed independently of the mid-file
        // damage that stopped the scan — so nothing further is owed beyond not
        // misbehaving, which libFuzzer's sanitizers judge.
        let _ = VarveFile::open_readonly_lazy_with_report(spec, path);
        return;
    };

    let (file, source) = match VarveFile::open_readonly_lazy_with_report(spec, path) {
        Ok(opened) => opened,
        Err(error) => panic!(
            "the lazy open failed where the scan succeeds; its fallback is that scan: {error:?}"
        ),
    };

    if oracle == Oracle::Pristine {
        assert_eq!(
            source,
            LazyOpenSource::HeaderTails,
            "the unmutated fixture carries a table the file corroborates"
        );
    }
    if oracle == Oracle::Appended {
        // The one place a `source` assertion is owed rather than tautological:
        // appending is exactly what the forward walk exists to notice, so a
        // `HeaderTails` answer here would mean the check that separates a
        // current table from a stale one had stopped working. `digest_arbitrary`
        // deliberately makes no such assertion, because a truncation can land
        // on an earlier digest and legitimately answer `Digest`; appending past
        // the end has no such escape.
        assert_eq!(
            source,
            LazyOpenSource::FullScan,
            "a file appended past its last commit must not be answered from the table"
        );
    }

    let line_tail = file.block_tail_offset(TailLine::ID);
    let note_tail = file.block_tail_offset(TailNote::ID);

    if oracle != Oracle::Mutated {
        assert_eq!(
            line_tail, scanned_tails[0],
            "lazy open ({source:?}) disagrees with the scan on the TailLine tail"
        );
        assert_eq!(
            note_tail, scanned_tails[1],
            "lazy open ({source:?}) disagrees with the scan on the TailNote tail"
        );
    }

    // Sound on any file, mutated or not: an offset the table hands back is one
    // the reader will seek to, so it has to name a record of the block the
    // table said it did. `read_block_at` refuses a mismatched id, so a decoder
    // that let a corrupt table through surfaces here as an error rather than as
    // a wrong-typed read, and a nonsensical offset surfaces to the sanitizers.
    if let Some(offset) = line_tail {
        let _ = file.read_block_at::<TailLine>(offset);
    }
    if let Some(offset) = note_tail {
        let _ = file.read_block_at::<TailNote>(offset);
    }

    // The header route keeps no directory, so the map is the only way to ask it
    // where the file ends. A handle that stopped one record short shows up here
    // and nowhere else.
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
}
