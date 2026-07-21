// SHAPE B (round 14): the matrix's fail-closed gate for `Fatal` recovery
// findings.
//
// `FatalAccessAllowed` is the witness that no fatal finding blocks access to
// matrix state, and `MatrixLayout::block_index` / `commit_index` — the only two
// routes from a block id or a commit category to a position in that state —
// demand it. Its sole constructor is `FatalAccessGate::allow`, which IS the
// refusal.
//
// Round 12 kept the state in a `fatal_access_blocked: bool` consulted by an
// `ensure_fatal_access_allowed` helper at eleven call sites, so an accessor
// written later simply had to not call it; re-verification named that as an
// unclosed instance of the very class the round existed for. Fabricating this
// witness would restore it: a reader could consume fatal-state data that the
// spec never opted into reading forensically.

#![allow(unreachable_code)]

use varve_core::enforcement_probe::FatalAccessAllowed;

fn main() {
    let _forged: FatalAccessAllowed = FatalAccessAllowed(unreachable!());
}
