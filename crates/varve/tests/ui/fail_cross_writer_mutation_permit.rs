// SHAPE B, the level above F-05 (round 12 re-verification).
//
// Round 12's permit was a bare zero-sized token: it proved that *a* poison flag
// had been checked, not that *the writer being mutated* had been. A permit taken
// from anything at all could be spent on any guarded mutation of any writer.
//
// The witness is now typed by the writer it speaks for, so a permit for one
// writer is not a permit for another and the guarded signatures say which flag
// must have been read. This fixture hands a `VarveFile`'s permit to a function
// that demands a `VarveStreamWriter`'s, and must keep failing to compile.
//
// The other half of the fix — that the constructor is private to
// `crate::writer_permit`, so a throwaway `PoisonFlag::healthy()` cannot mint a
// witness at all — is in-crate and therefore not expressible from a downstream
// crate: every spelling of it is already unreachable here, so a fixture would
// pass for the wrong reason. It is gated instead by
// `tests/enforcement_gates.rs::a_permit_can_only_be_minted_from_the_writers_own_poison_flag`.

use varve_core::enforcement_probe::MutationPermit;
use varve_core::{VarveFile, VarveStreamWriter};

fn guarded_stream_mutation(_permit: MutationPermit<VarveStreamWriter>) {}

fn some_other_writers_permit() -> MutationPermit<VarveFile> {
    unimplemented!("a permit for a different writer; how it was obtained is irrelevant")
}

fn main() {
    guarded_stream_mutation(some_other_writers_permit());
}
