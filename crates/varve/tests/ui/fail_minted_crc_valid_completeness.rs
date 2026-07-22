// F-04, the MINTING half (round 15).
//
// `CompleteCrcValidEvidence` could not be forged after round 14, and did not
// need to be: `CrcValidEvidence::new(bits, complete: bool)` handed out evidence
// that *claimed* completeness for a bitmap nothing had enumerated, and
// `.complete()` then issued the witness for it without complaint.
//
// Completeness is now only expressible by moving in a `PageIndexEnumeration`,
// which only the enumerator produces.

#![allow(unreachable_code)]

use varve_core::enforcement_probe::CrcValidEvidence;

fn main() {
    let _minted = CrcValidEvidence::new(unreachable!(), true);
}
