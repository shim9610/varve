// SHAPE B, the MINTING half (round 15).
//
// Round 14 proved that `FatalAccessAllowed` cannot be *forged* — see
// `fail_fabricated_fatal_access.rs` — and left `FatalAccessGate::new(blocked:
// bool)` in place. Forgery was never the shape the defects took. The one-liner
//
//     FatalAccessGate::new(false).allow()?
//
// minted a gate that says "not blocked", took a witness off it, and spent that
// witness on a layout carrying a `Fatal` recovery finding. It compiled inside
// `varve-core`, which is where every historical defect in this class has lived.
//
// The constructor that accepts the decision is gone. This fixture is the
// downstream trip-wire; the in-crate half — the position the bypass was
// actually written from — is `matrix.rs::bypass_catalogue` and
// `crates/varve/tests/enforcement_gates.rs`.

use varve_core::enforcement_probe::FatalAccessGate;

fn main() {
    let _minted = FatalAccessGate::new(false);
}
