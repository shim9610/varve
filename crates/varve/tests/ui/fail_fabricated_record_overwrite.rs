// F-02, mechanical enforcement (round 13).
//
// `RecordOverwrite` is the permission to rewrite the bytes of an
// already-indexed record, and the only route to `RecordFile`'s in-place write.
// Its only constructor, `RecordOverwrite::prepare`, consumes a version-checked
// `ReplacementTarget` and drops the target block's resident keyed-tail map
// before the permission exists. Fabricating one would write record bytes with
// a stale keyed-tail cache still resident — F-02 — and without the F-01
// version refusal.

use varve_core::enforcement_probe::RecordOverwrite;

fn main() {
    let _forged = RecordOverwrite {
        record_offset: 0,
        payload_offset: 0,
    };
}
