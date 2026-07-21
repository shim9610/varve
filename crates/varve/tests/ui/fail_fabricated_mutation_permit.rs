// SHAPE B, mechanical enforcement (round 12).
//
// `MutationPermit` is the witness that a writer's poison flag was observed
// clear. Its only constructor is `PoisonFlag::issue`, which performs the check
// and is private to `crate::writer_permit`. If the witness could be fabricated,
// every guarded mutation in `stream.rs`, `indexed.rs`, `file.rs` and `layout.rs`
// would be back to being protected by a convention — which is the exact
// configuration F-05 was found in. This fixture fails to compile, and must keep
// failing.

use std::marker::PhantomData;
use varve_core::VarveStreamWriter;
use varve_core::enforcement_probe::MutationPermit;

fn main() {
    // The field is private to `crate::writer_permit`, so no code anywhere else
    // — inside the crate or outside it — can build the proof without making
    // the check.
    let _forged: MutationPermit<VarveStreamWriter> = MutationPermit(PhantomData);
}
