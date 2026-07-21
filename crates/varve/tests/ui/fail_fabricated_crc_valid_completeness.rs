// F-04, mechanical enforcement (round 14).
//
// `CompleteCrcValidEvidence` is the proof that a block's CRC-validity page
// index was enumerated in full, and it is the only thing in the crate that can
// return a validity *bit*. Its sole constructor, `CrcValidEvidence::complete`,
// IS the F-04 refusal.
//
// Round 12 tracked completeness in a `bool` beside a plain `SparseBitmap`, so a
// new consumer that wrote `block.crc_valid_bits.get(ordinal)?` compiled
// cleanly — the check bound nothing, and re-verification judged the fix partial
// for exactly that reason. Fabricating this witness would restore that: a
// rebuild could read a clear bit from a bitmap it never finished loading and
// republish committed cells as uncommitted.

#![allow(unreachable_code)]

use varve_core::enforcement_probe::CompleteCrcValidEvidence;

fn main() {
    let _forged: CompleteCrcValidEvidence<'static> = CompleteCrcValidEvidence {
        bits: unreachable!(),
    };
}
