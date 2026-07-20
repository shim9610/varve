// Resident-facade contract regressions for the 2026-07-19 adversarial review.
//
// API2-05 (report finding API-02): `VarveFile::push_info` and
// `VarveWriter::push_info` used to write `prev_same_key_offset = None` for a
// second record with an equal key while the generated keyed writer linked it
// correctly, leaving a silently truncated physical keyed chain. `T: VarveBlock`
// exposes no key, so those entry points now reject keyed blocks on
// keyed-chaining formats and the maintaining `push_keyed`/`delete` paths carry
// the chain.
//
// PERF2-05 (report finding PERF-05): resident block-offset chaining resolved
// the predecessor with `index.iter().rev().find(...)`, costing Theta(g) per
// append and O(N*B) overall. The predecessor now comes from a maintained tail
// table that never reads the resident index on the append path.

use std::path::Path;

#[cfg(feature = "scalable-fault-injection")]
use varve::VarveFile;
use varve::{Error, varve_format};

varve_format! {
    pub format ResidentChainFormat {
        magic: b"RESCHN";
        version: 1;
        index: keyed_offset_chain;
        blocks {
            fixed Marker(id = 10) {
                tag: u32,
            }
            variable Item(id = 11, key = [id]) {
                id: u64,
                value: u32,
            }
            fixed Note(id = 12) {
                note: u32,
            }
        }
    }
}

varve_format! {
    pub format ResidentPlainFormat {
        magic: b"RESPLN";
        version: 1;
        index: scan_on_open;
        blocks {
            variable Entry(id = 21, key = [id]) {
                id: u64,
                value: u32,
            }
        }
    }
}

/// `(record_offset, prev_same_block_offset, prev_same_key_offset)` for one
/// record.
type ChainLink = (u64, Option<u64>, Option<u64>);

/// Every [`ChainLink`] for `block_id`, read back from a freshly opened file so
/// the assertions are against the persisted footers rather than writer state.
fn chain(path: &Path, block_id: u32) -> varve::Result<Vec<ChainLink>> {
    let file = ResidentChainFormat::open(path)?;
    Ok(file
        .index_entries()
        .iter()
        .filter(|entry| entry.block_id == block_id)
        .map(|entry| {
            (
                entry.record_offset,
                entry.prev_same_block_offset,
                entry.prev_same_key_offset,
            )
        })
        .collect())
}

fn assert_keyed_chain_rejection(error: &Error) {
    assert!(
        matches!(
            error,
            Error::KeyedChainRequiresKeyedApi {
                block_id: <Item as varve::VarveBlock>::ID
            }
        ),
        "expected a typed keyed-chain rejection, got {error:?}"
    );
}

#[test]
fn generic_push_rejects_keyed_blocks_on_keyed_chaining_formats() -> varve::Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("generic-push-reject.varve");

    let mut file = ResidentChainFormat::create(&path)?;
    assert_keyed_chain_rejection(
        &file
            .push_info(&Item { id: 1, value: 1 })
            .expect_err("VarveFile::push_info must refuse a keyed block here"),
    );
    assert_keyed_chain_rejection(
        &file
            .push(&Item { id: 1, value: 1 })
            .expect_err("VarveFile::push must refuse a keyed block here"),
    );
    // Unkeyed blocks are unaffected.
    file.push(&Marker { tag: 7 })?;
    file.flush()?;
    drop(file);

    let mut writer = ResidentChainFormat::spec().open_writer(&path)?;
    assert_keyed_chain_rejection(
        &writer
            .push_info(&Item { id: 1, value: 1 })
            .expect_err("VarveWriter::push_info must refuse a keyed block here"),
    );
    assert_keyed_chain_rejection(
        &writer
            .push(&Item { id: 1, value: 1 })
            .expect_err("VarveWriter::push must refuse a keyed block here"),
    );
    drop(writer);

    // The refused pushes wrote nothing.
    assert!(chain(&path, <Item as varve::VarveBlock>::ID)?.is_empty());
    Ok(())
}

#[test]
fn generic_push_accepts_keyed_blocks_without_keyed_chaining() -> varve::Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("generic-push-unchained.varve");

    let mut file = ResidentPlainFormat::create(&path)?;
    file.push(&Entry { id: 1, value: 1 })?;
    file.push(&Entry { id: 1, value: 2 })?;
    file.flush()?;
    drop(file);

    let file = ResidentPlainFormat::open(&path)?;
    assert_eq!(file.index_entries().len(), 2);
    for entry in file.index_entries() {
        assert_eq!(entry.prev_same_key_offset, None);
    }
    Ok(())
}

#[test]
fn maintaining_keyed_facades_link_the_predecessor_chain() -> varve::Result<()> {
    let item_id = <Item as varve::VarveBlock>::ID;

    // Facade 1: the maintaining generic path on VarveFile.
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("file-push-keyed.varve");
    let mut file = ResidentChainFormat::create(&path)?;
    file.push_keyed(&Item { id: 4, value: 1 })?;
    file.push_keyed(&Item { id: 9, value: 1 })?;
    file.push_keyed(&Item { id: 4, value: 2 })?;
    file.flush()?;
    drop(file);
    let records = chain(&path, item_id)?;
    assert_eq!(records.len(), 3);
    assert_eq!(
        records[0].2, None,
        "first record of key 4 has no predecessor"
    );
    assert_eq!(
        records[1].2, None,
        "first record of key 9 has no predecessor"
    );
    assert_eq!(
        records[2].2,
        Some(records[0].0),
        "second record of key 4 must link to the first"
    );

    // Facade 2: the same through VarveWriter.
    let path = directory.path().join("writer-push-keyed.varve");
    let mut writer = ResidentChainFormat::spec().create_writer(&path)?;
    writer.push_keyed(&Item { id: 4, value: 1 })?;
    writer.push_keyed(&Item { id: 4, value: 2 })?;
    writer.flush()?;
    drop(writer);
    let records = chain(&path, item_id)?;
    assert_eq!(records.len(), 2);
    assert_eq!(records[1].2, Some(records[0].0));

    // Facade 3: the generated keyed writer, which already linked correctly.
    let path = directory.path().join("generated-push-keyed.varve");
    let mut generated = ResidentChainFormat::create_writer(&path)?;
    generated.push_item(&Item { id: 4, value: 1 })?;
    generated.push_item(&Item { id: 4, value: 2 })?;
    generated.flush()?;
    drop(generated);
    let records = chain(&path, item_id)?;
    assert_eq!(records.len(), 2);
    assert_eq!(records[1].2, Some(records[0].0));

    Ok(())
}

#[test]
fn keyed_delete_links_the_predecessor_chain() -> varve::Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("delete-keyed.varve");

    let mut file = ResidentChainFormat::create(&path)?;
    let first = file.push_keyed_info(&Item { id: 4, value: 1 })?;
    file.delete::<Item>(&4)?;
    file.flush()?;
    drop(file);

    let opened = ResidentChainFormat::open(&path)?;
    let tombstone = opened
        .index_entries()
        .iter()
        .find(|entry| entry.block_id == varve::TOMBSTONE_BLOCK_ID)
        .expect("tombstone record");
    assert_eq!(
        tombstone.prev_same_key_offset,
        Some(first.record_offset),
        "the tombstone must link to the record it supersedes"
    );
    Ok(())
}

#[test]
fn keyed_chains_survive_reopen() -> varve::Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("keyed-reopen.varve");
    let item_id = <Item as varve::VarveBlock>::ID;

    let mut file = ResidentChainFormat::create(&path)?;
    file.push_keyed(&Item { id: 4, value: 1 })?;
    file.flush()?;
    drop(file);

    let mut reopened = ResidentChainFormat::open(&path)?;
    reopened.push_keyed(&Item { id: 4, value: 2 })?;
    reopened.push_keyed(&Item { id: 5, value: 1 })?;
    reopened.flush()?;
    drop(reopened);

    let mut reopened = ResidentChainFormat::open(&path)?;
    reopened.push_keyed(&Item { id: 4, value: 3 })?;
    reopened.flush()?;
    drop(reopened);

    let records = chain(&path, item_id)?;
    assert_eq!(records.len(), 4);
    assert_eq!(records[0].2, None);
    assert_eq!(
        records[1].2,
        Some(records[0].0),
        "the chain must cross the first reopen"
    );
    assert_eq!(records[2].2, None, "key 5 starts its own chain");
    assert_eq!(
        records[3].2,
        Some(records[1].0),
        "the chain must cross the second reopen"
    );
    Ok(())
}

/// The keyed tail cache must not survive a caller-supplied predecessor: the
/// file cannot observe which key that record carried, so the next maintained
/// append has to rebuild from the resident index.
#[test]
fn maintained_keyed_tails_recover_after_an_explicit_predecessor() -> varve::Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("keyed-mixed.varve");
    let item_id = <Item as varve::VarveBlock>::ID;

    let mut file = ResidentChainFormat::create(&path)?;
    let first = file.push_keyed_info(&Item { id: 4, value: 1 })?;
    let second =
        file.push_with_prev_key_info(&Item { id: 4, value: 2 }, Some(first.record_offset))?;
    file.push_keyed(&Item { id: 4, value: 3 })?;
    file.flush()?;
    drop(file);

    let records = chain(&path, item_id)?;
    assert_eq!(records.len(), 3);
    assert_eq!(records[1].2, Some(first.record_offset));
    assert_eq!(
        records[2].2,
        Some(second.record_offset),
        "the rebuilt tail must see the explicitly linked record"
    );
    Ok(())
}

#[test]
fn interleaved_block_ids_chain_correctly_across_reopen() -> varve::Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("block-tails.varve");

    let mut file = ResidentChainFormat::create(&path)?;
    for round in 0..8u32 {
        file.push(&Marker { tag: round })?;
        file.push(&Note { note: round })?;
        file.push_keyed(&Item {
            id: u64::from(round),
            value: round,
        })?;
    }
    file.flush()?;
    drop(file);

    // Reopen and keep appending: the tails must be recovered from the resident
    // index, not restarted.
    let mut file = ResidentChainFormat::open(&path)?;
    for round in 8..12u32 {
        file.push(&Marker { tag: round })?;
        file.push(&Note { note: round })?;
        file.push_keyed(&Item {
            id: u64::from(round),
            value: round,
        })?;
    }
    file.flush()?;
    drop(file);

    for block_id in [
        <Marker as varve::VarveBlock>::ID,
        <Note as varve::VarveBlock>::ID,
        <Item as varve::VarveBlock>::ID,
    ] {
        let records = chain(&path, block_id)?;
        assert_eq!(records.len(), 12, "block {block_id}");
        assert_eq!(
            records[0].1, None,
            "block {block_id} head has no predecessor"
        );
        for window in records.windows(2) {
            assert_eq!(
                window[1].1,
                Some(window[0].0),
                "block {block_id} chain must link consecutive records"
            );
        }
    }
    Ok(())
}

/// PERF2-05: the append-time predecessor lookup must not scale with the
/// resident index. The counter records resident index entries examined by the
/// block-tail machinery; wholesale index loads pay one pass, appends must pay
/// nothing at all, so the delta over an append window is zero for any record
/// count.
#[cfg(feature = "scalable-fault-injection")]
#[test]
fn append_time_predecessor_lookup_never_reads_the_resident_index() -> varve::Result<()> {
    /// Returns `(index entries examined while appending, entries examined by
    /// the reopen that follows)`.
    fn measure(rounds: u32, name: &str) -> varve::Result<(u64, u64)> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join(name);
        let mut file = ResidentChainFormat::create(&path)?;
        // Alternating block ids maximized the old reverse scan: every append
        // walked back past the other block's record.
        let before_appends = VarveFile::block_tail_index_touches();
        for round in 0..rounds {
            file.push(&Marker { tag: round })?;
            file.push(&Note { note: round })?;
        }
        let after_appends = VarveFile::block_tail_index_touches();
        file.flush()?;
        drop(file);

        let reopened = ResidentChainFormat::open(&path)?;
        let after_open = VarveFile::block_tail_index_touches();
        drop(reopened);
        Ok((after_appends - before_appends, after_open - after_appends))
    }

    let (small_appends, small_open) = measure(256, "touches-256.varve")?;
    let (large_appends, large_open) = measure(2_048, "touches-2048.varve")?;

    assert_eq!(small_appends, 0, "appends must not read the resident index");
    assert_eq!(large_appends, 0, "appends must not read the resident index");
    // Calibration: the counter is wired, and the only index pass is the single
    // one a wholesale load pays.
    assert!(
        small_open >= 512,
        "reopen must rebuild tails once: {small_open}"
    );
    assert!(
        large_open >= 4_096,
        "reopen must rebuild tails once: {large_open}"
    );
    Ok(())
}

// PERF3-03 (report finding PERF-03): `BlockTails::from_index` claimed
// `O(N log B)` but inserted every first-seen block id into a sorted vector, so
// an index whose first appearances descend by id moved `0 + 1 + ... + (B - 1)`
// tuples - a real `O(N log B + B^2)`. The previous test could not see this
// because it counted resident *index visits*, which the insertion cost does
// not touch. Construction now orders the distinct ids once.
//
// PERF3-05 (report finding PERF-05): every resident input open could copy all
// `N` sequences into an `8N` temporary and sort it, and `KeyedMergeEstimate`
// charged neither the temporary nor the sort. The estimate is the published
// way to size this resident-only family, so an estimate that omits a live
// allocation is a defect in the deliverable itself.

varve_format! {
    pub format ResidentWideFormat {
        magic: b"RESWID";
        version: 1;
        index: scan_on_open;
        blocks {
            fixed Wide30(id = 30) {
                tag: u32,
            }
            fixed Wide31(id = 31) {
                tag: u32,
            }
            fixed Wide32(id = 32) {
                tag: u32,
            }
            fixed Wide33(id = 33) {
                tag: u32,
            }
            fixed Wide34(id = 34) {
                tag: u32,
            }
            fixed Wide35(id = 35) {
                tag: u32,
            }
            fixed Wide36(id = 36) {
                tag: u32,
            }
            fixed Wide37(id = 37) {
                tag: u32,
            }
            fixed Wide38(id = 38) {
                tag: u32,
            }
            fixed Wide39(id = 39) {
                tag: u32,
            }
            fixed Wide40(id = 40) {
                tag: u32,
            }
            fixed Wide41(id = 41) {
                tag: u32,
            }
            fixed Wide42(id = 42) {
                tag: u32,
            }
            fixed Wide43(id = 43) {
                tag: u32,
            }
            fixed Wide44(id = 44) {
                tag: u32,
            }
            fixed Wide45(id = 45) {
                tag: u32,
            }
        }
    }
}

/// PERF3-03: wholesale block-tail construction must move no tuples at all, in
/// the exact worst case that used to be quadratic. This counter observes
/// vector movement, which is the cost the index-visit counter cannot see.
#[cfg(feature = "scalable-fault-injection")]
#[test]
fn block_tail_construction_has_no_quadratic_term() -> varve::Result<()> {
    /// Appends `rounds` descending sweeps over all 16 block ids and reopens the
    /// file. Returns `(tuples moved while appending, tuples moved by the
    /// reopen)`.
    fn measure(rounds: u32, name: &str) -> varve::Result<(u64, u64)> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join(name);
        let mut file = ResidentWideFormat::create(&path)?;
        let before_appends = VarveFile::block_tail_entries_moved();
        for round in 0..rounds {
            // Descending first appearances are the worst case for insertion
            // into a vector kept sorted ascending by block id.
            file.push(&Wide45 { tag: round })?;
            file.push(&Wide44 { tag: round })?;
            file.push(&Wide43 { tag: round })?;
            file.push(&Wide42 { tag: round })?;
            file.push(&Wide41 { tag: round })?;
            file.push(&Wide40 { tag: round })?;
            file.push(&Wide39 { tag: round })?;
            file.push(&Wide38 { tag: round })?;
            file.push(&Wide37 { tag: round })?;
            file.push(&Wide36 { tag: round })?;
            file.push(&Wide35 { tag: round })?;
            file.push(&Wide34 { tag: round })?;
            file.push(&Wide33 { tag: round })?;
            file.push(&Wide32 { tag: round })?;
            file.push(&Wide31 { tag: round })?;
            file.push(&Wide30 { tag: round })?;
        }
        let after_appends = VarveFile::block_tail_entries_moved();
        file.flush()?;
        drop(file);

        let reopened = ResidentWideFormat::open(&path)?;
        let after_open = VarveFile::block_tail_entries_moved();
        drop(reopened);
        Ok((after_appends - before_appends, after_open - after_appends))
    }

    let (small_appends, small_open) = measure(4, "wide-tails-4.varve")?;
    let (large_appends, large_open) = measure(64, "wide-tails-64.varve")?;

    // Calibration: the counter is wired and this really is the worst case. The
    // append path still inserts, but only once per distinct id for the whole
    // life of the file, so its movement does not grow with the record count.
    assert_eq!(
        small_appends, large_appends,
        "insertion movement must depend on distinct ids, not record count"
    );
    assert!(
        small_appends >= (16 * 15) / 2,
        "descending ids must be the insertion worst case: {small_appends}"
    );
    // The regression: rebuilding the tails on open must not repeat that
    // movement, for any record count.
    assert_eq!(
        small_open, 0,
        "tail construction must not move tuples: {small_open}"
    );
    assert_eq!(
        large_open, 0,
        "tail construction must not move tuples: {large_open}"
    );
    Ok(())
}

#[derive(Clone, Debug, PartialEq, varve::VarveBlock)]
#[varve(id = 60, version = 1, kind = "variable", key = "user_id")]
struct ResidentMergeUser {
    #[varve(field_id = 1)]
    user_id: u64,
    #[varve(field_id = 2)]
    name: String,
}

#[derive(Clone, Debug, PartialEq, varve::VarveBlock)]
#[varve(id = 61, version = 1, kind = "variable")]
struct ResidentMergeUserOp {
    #[varve(field_id = 1)]
    rename_to: String,
}

impl varve::VarveMerge for ResidentMergeUser {
    type Op = ResidentMergeUserOp;

    fn apply_op(&mut self, op: Self::Op) -> varve::Result<()> {
        self.name = op.rename_to;
        Ok(())
    }
}

varve_format! {
    pub struct ResidentEstimateFormat {
        magic: b"RESEST";
        version: 1;
        endian: little;
        blocks: [ResidentMergeUser, ResidentMergeUserOp];
    }
}

/// PERF3-05: the pre-flight estimate must charge the transient an input open
/// allocates alongside its resident index, so `peak_resident_bytes` bounds the
/// real peak instead of understating it.
#[test]
fn resident_merge_estimate_includes_the_open_time_transient() -> varve::Result<()> {
    const BASE_KEYS: u64 = 300;
    const DELTA_KEYS: u64 = 40;

    fn write_input(path: &Path, keys: u64) -> varve::Result<()> {
        let mut file = ResidentEstimateFormat::create(path)?;
        for key in 0..keys {
            file.push(&ResidentMergeUser {
                user_id: key,
                name: format!("user-{key}"),
            })?;
        }
        file.flush()?;
        Ok(())
    }

    let directory = tempfile::tempdir()?;
    let base = directory.path().join("estimate-base.varve");
    let delta = directory.path().join("estimate-delta.varve");
    write_input(&base, BASE_KEYS)?;
    write_input(&delta, DELTA_KEYS)?;

    let estimate = varve::estimate_keyed_merge::<ResidentMergeUser, _>(
        ResidentEstimateFormat::spec(),
        base.as_path(),
        &[delta.as_path()],
    )?;

    // Derived floor: the largest input holds at least `BASE_KEYS` records and
    // the uniqueness witness copies one `u64` per record of that input.
    let floor = BASE_KEYS * (size_of::<u64>() as u64);
    assert!(
        estimate.largest_input_open_transient_bytes >= floor,
        "the open transient must be charged: {} < {floor}",
        estimate.largest_input_open_transient_bytes
    );
    assert!(
        estimate.peak_resident_bytes()
            >= estimate.max_state_bytes + estimate.largest_input_index_bytes + floor,
        "the peak must include the state, the largest index, and the transient"
    );
    // The transient is a per-input cost, so it tracks the largest input rather
    // than the delta or the total.
    let base_only = varve::estimate_keyed_merge::<ResidentMergeUser, _>(
        ResidentEstimateFormat::spec(),
        base.as_path(),
        &[],
    )?;
    assert_eq!(
        base_only.largest_input_open_transient_bytes, estimate.largest_input_open_transient_bytes,
        "a smaller delta must not change the largest input's transient"
    );

    let bigger = directory.path().join("estimate-bigger.varve");
    write_input(&bigger, BASE_KEYS * 4)?;
    let bigger_estimate = varve::estimate_keyed_merge::<ResidentMergeUser, _>(
        ResidentEstimateFormat::spec(),
        bigger.as_path(),
        &[base.as_path()],
    )?;
    assert!(
        bigger_estimate.largest_input_open_transient_bytes
            > estimate.largest_input_open_transient_bytes,
        "the transient must scale with the largest input's record count"
    );
    assert!(
        bigger_estimate.peak_resident_bytes() > estimate.peak_resident_bytes(),
        "the peak bound must grow with the inputs"
    );
    Ok(())
}
