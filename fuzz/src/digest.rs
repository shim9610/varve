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

use varve::{FormatSpec, LazyOpenSource, VarveBlock, VarveFile, varve_format};

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
/// all its budget on record bodies the decoder never sees. This is the window
/// that makes the target about the decoder.
const TAIL_WINDOW: usize = 512;

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
        // Overwrite inside the digest window: the decoder's own fields.
        0 => {
            let start = bytes.len().saturating_sub(TAIL_WINDOW);
            for (index, byte) in mutations.iter().enumerate() {
                let at = start + index;
                if at >= bytes.len() {
                    break;
                }
                bytes[at] = *byte;
            }
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
    // is simply valid. A digest that were wrong here would never reach the
    // mutation cases.
    compare_routes(spec, &path, true);

    let mode = control % 4;
    if !mutate(&path, control, mutations) {
        return;
    }
    // Only the append case still owes the scan an identical answer; see
    // `compare_routes`.
    compare_routes(spec, &path, mode == 2);
}

/// Opens the same bytes both ways.
///
/// `strict` is the whole subtlety of this target, and getting it wrong is what
/// the first run of it did. **A lazy open cannot agree with the scan on a file
/// with mid-file rot, and that is not a defect.** The scan detects a corrupt
/// record only because it frames every record; when it hits one it stops, and
/// its idea of a block's tail becomes the last good record *before* the damage.
/// The digest sits at the end, is checksummed independently, and reports the
/// true tail past it. Measured: one flipped byte at offset 17969 put the scan's
/// tail at 16804 and the digest's at 37868 — both correct for their own
/// definition. Requiring equality there would be requiring the lazy open to do
/// the hundred thousand syscalls it exists to avoid.
///
/// So equality is asserted exactly where it is owed:
///
/// * an unmutated file — the plain correctness of the fast route;
/// * a file appended past its last commit — the case the digest's
///   `physical_end` field exists for, where the two must still agree.
///
/// For truncation and rot the requirement is only that the path be sound, which
/// the sanitizers and the read-back below judge.
fn compare_routes(spec: FormatSpec, path: &Path, strict: bool) {
    // The scan is read first and is never allowed to be the thing that changed
    // the file — `open_readonly` does not recover in place, so both routes see
    // identical bytes.
    let Some((scanned_entries, scanned_tails)) = scanned_shape(spec, path) else {
        // The scan itself refuses these bytes. Then the file has no defined
        // content to compare against and the only requirement on the lazy open
        // is that it not misbehave; libFuzzer's sanitizers judge that part.
        let _ = VarveFile::open_readonly_lazy_with_report(spec, path);
        return;
    };

    let Ok((file, source)) = VarveFile::open_readonly_lazy_with_report(spec, path) else {
        // Degrading to an error where the scan succeeds is permitted and is the
        // documented fallback's failure mode.
        return;
    };

    let line_tail = file.block_tail_offset(DigestLine::ID);
    let note_tail = file.block_tail_offset(DigestNote::ID);

    if strict {
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
    // here and nowhere else.
    let mut buffer = Vec::new();
    if let Ok(mut map) = file.record_map(&mut buffer)
        && map.fill().is_ok()
    {
        let walked: Vec<_> = map
            .entries()
            .iter()
            .map(|entry| (entry.block_id, entry.record_offset, entry.payload_len))
            .collect();
        if strict {
            assert_eq!(
                walked, scanned_entries,
                "the map walked from a lazy open ({source:?}) is not the scan's index"
            );
        }
    }

    // The report is a claim about which route ran, and a Digest answer on a
    // file with no readable digest would be the fallback failing silently.
    if source == LazyOpenSource::Digest {
        assert!(
            spec.index_policy.open_digest_on_flush,
            "reported a digest open for a spec that never writes one"
        );
    }
}
