// SHAPE A, mechanical enforcement (round 12).
//
// `ReservedIndexSlot` is the proof that the resident record index — the
// in-memory mirror of the records on disk — has capacity for one more entry.
// It is produced only by `ReservedIndexSlot::reserve`, which does the limit
// charge and the `try_reserve`, and it is consumed by `install`, which pushes
// infallibly. Fabricating one would put the fallible half back *after* the
// authoritative append, which is round 12's F-03 shape (and round 7's F-03,
// and round 5's F-02).

use varve_core::enforcement_probe::ReservedIndexSlot;

fn main() {
    let _forged: ReservedIndexSlot = ReservedIndexSlot(());
}
