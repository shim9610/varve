// F-01, mechanical enforcement (round 13).
//
// `ReplacementTarget` is the proof that a record was selected by block id AND
// that its stored `block_version` equals `T::VERSION`. Its only constructor is
// `ReplacementTarget::resolve`, which performs the refusal. Fabricating one
// would put a caller-chosen ordinal back in reach of the in-place record
// writer with no version check, which is F-01 exactly: a v2 payload written
// under a retained v1 header, silently decoded by a v1 reader.

use varve_core::enforcement_probe::ReplacementTarget;

fn main() {
    let _forged = ReplacementTarget { position: 0 };
}
