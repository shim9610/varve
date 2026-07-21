// SHAPE B, mechanical enforcement (round 12).
//
// A writer's in-flight mutation window (the shape `layout.rs` uses: refuse
// while a segment write is running, and for good if its rollback failed) can
// only be opened by handing over a `MutationPermit`. This fixture tries to
// open one without the check and must not compile.

use varve_core::LayoutWriter;
use varve_core::enforcement_probe::{MutationInFlight, PoisonFlag};

fn main() {
    let mut flag = PoisonFlag::healthy();
    // No permit: the poison check was skipped, so there is nothing to pass.
    let _window: MutationInFlight<LayoutWriter> = flag.begin_mutation();
}
