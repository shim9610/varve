//! Durability boundaries are tested through fresh snapshots and recovery,
//! including boundaries inside one iterator call.

use varve::{BatchOptions, DiskIndexOptions, Error, ImmediatePolicy, StreamOptions, varve_format};

varve_format! {
    pub format Boundaries {
        magic: b"IMMD";
        version: 1;
        schema_hash: computed;
        index: keyed_offset_chain;
        blocks {
            fixed Item(id = 1, key = [key], key_index = disk) { key: u64, value: u64 }
            fixed Durable(id = 2, key = [key], key_index = disk, durability = immediate) { key: u64 }
            fixed Marker(id = 3, durability = immediate) { value: u64 }
            fixed Conditional(id = 4, key = [key], key_index = disk, immediate_if = checkpoint) { key: u64, value: u64 }
        }
    }
}

fn checkpoint(value: &Conditional) -> bool {
    value.value == 2 || value.value == 5
}

varve_format! {
    pub format StreamBoundaries {
        magic: b"IMMS";
        version: 1;
        schema_hash: computed;
        index: block_offset_chain;
        blocks {
            fixed Sample(id = 11, immediate_if = event_checkpoint) { value: u64 }
            fixed Barrier(id = 12, durability = immediate) { value: u64 }
        }
    }
}

fn event_checkpoint(value: &Sample) -> bool {
    value.value == 2 || value.value == 5
}

fn opts() -> DiskIndexOptions {
    DiskIndexOptions::default()
}

#[test]
fn mandatory_blocks_cover_prior_writes_and_deletes() -> varve::Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("blocks.varve");
    let mut writer = Boundaries::create_indexed_writer(&path, opts())?;
    let pinned = Boundaries::open_indexed_reader(&path, opts())?;
    writer.push_item(&Item { key: 1, value: 10 })?;
    assert!(
        Boundaries::open_indexed_reader(&path, opts())?
            .get_item(&1)?
            .is_none()
    );
    writer.push_durable(&Durable { key: 7 })?;
    let current = Boundaries::open_indexed_reader(&path, opts())?;
    assert_eq!(current.get_item(&1)?, Some(Item { key: 1, value: 10 }));
    assert_eq!(current.get_durable(&7)?, Some(Durable { key: 7 }));
    assert!(pinned.get_item(&1)?.is_none());
    writer.delete_durable(&7)?;
    assert!(
        Boundaries::open_indexed_reader(&path, opts())?
            .get_durable(&7)?
            .is_none()
    );
    assert_eq!(current.get_durable(&7)?, Some(Durable { key: 7 }));
    writer.push_item(&Item { key: 1, value: 20 })?;
    writer.push_marker(&Marker { value: 1 })?;
    assert_eq!(
        Boundaries::open_indexed_reader(&path, opts())?
            .get_item(&1)?
            .unwrap()
            .value,
        20
    );
    Ok(())
}

#[test]
fn value_conditions_cut_batches_at_the_record_and_recovery_keeps_the_boundary() -> varve::Result<()>
{
    for chunk_bytes in [1, 64, 4096] {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("values.varve");
        let mut writer = Boundaries::create_indexed_writer(&path, opts())?;
        let values = (0..8).map(|value| {
            if value == 3 || value == 6 {
                let reader = Boundaries::open_indexed_reader(&path, opts()).unwrap();
                assert_eq!(
                    reader.get_conditional(&0).unwrap().unwrap().value,
                    value - 1
                );
            }
            Conditional { key: 0, value }
        });
        let info = writer
            .push_conditionals(
                values,
                BatchOptions {
                    max_records: 64,
                    max_bytes: chunk_bytes,
                },
            )
            .map_err(|e| e.source)?;
        assert_eq!(info.records, 8);
        assert_eq!(
            Boundaries::open_indexed_reader(&path, opts())?
                .get_conditional(&0)?
                .unwrap()
                .value,
            5
        );
        drop(writer);
        let mut writer = Boundaries::restore_indexed_writer(&path, opts())?;
        let reader = Boundaries::open_indexed_reader(&path, opts())?;
        assert_eq!(reader.get_conditional(&0)?.unwrap().value, 5);
        writer.push_conditional(&Conditional { key: 0, value: 9 })?;
        writer.immediate()?;
        assert_eq!(
            Boundaries::open_indexed_reader(&path, opts())?
                .get_conditional(&0)?
                .unwrap()
                .value,
            9
        );
    }
    Ok(())
}

#[test]
fn runtime_conditions_and_explicit_immediate_share_the_same_counters() -> varve::Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("policy.varve");
    let mut writer = Boundaries::create_indexed_writer(&path, opts())?;
    writer.set_immediate_policy(ImmediatePolicy::new().after_records(3))?;
    writer.push_item(&Item { key: 0, value: 0 })?;
    writer.immediate()?;
    let values = (1..=6).map(|value| {
        if value == 4 {
            assert!(
                Boundaries::open_indexed_reader(&path, opts())
                    .unwrap()
                    .get_item(&3)
                    .unwrap()
                    .is_some()
            );
        }
        Item { key: value, value }
    });
    let info = writer
        .push_items(values, BatchOptions::default())
        .map_err(|e| e.source)?;
    assert_eq!(info.records, 6);
    assert_eq!(info.write_calls, 2);
    assert!(
        Boundaries::open_indexed_reader(&path, opts())?
            .get_item(&6)?
            .is_some()
    );
    writer.set_immediate_policy(ImmediatePolicy::new().when(|event| {
        event.block_id == 1 && event.operation == varve::ImmediateOperation::Delete
    }))?;
    writer.push_item(&Item { key: 7, value: 7 })?;
    writer.delete_item(&6)?;
    let reader = Boundaries::open_indexed_reader(&path, opts())?;
    assert!(reader.get_item(&6)?.is_none());
    assert!(reader.get_item(&7)?.is_some());
    writer.set_immediate_policy(ImmediatePolicy::new().after_records(1000).after_bytes(1))?;
    writer.push_item(&Item { key: 8, value: 8 })?;
    assert!(
        Boundaries::open_indexed_reader(&path, opts())?
            .get_item(&8)?
            .is_some()
    );
    assert!(matches!(
        writer.set_immediate_policy(ImmediatePolicy::new().after_records(0)),
        Err(Error::InvalidImmediatePolicy)
    ));
    // Invalid installation keeps the old byte condition.
    writer.push_item(&Item { key: 9, value: 9 })?;
    assert!(
        Boundaries::open_indexed_reader(&path, opts())?
            .get_item(&9)?
            .is_some()
    );
    Ok(())
}

#[test]
fn stream_scalar_batch_and_unindexed_batch_honor_declarations() -> varve::Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("stream.varve");
    let mut writer = StreamBoundaries::create_stream_writer(&path, StreamOptions::default())?;
    writer.push_sample(&Sample { value: 0 })?;
    assert_eq!(
        StreamBoundaries::open_stream_reader(&path, StreamOptions::default())?
            .samples()?
            .collect::<varve::Result<Vec<_>>>()?
            .len(),
        0
    );
    writer.push_barrier(&Barrier { value: 0 })?;
    StreamBoundaries::open_stream_reader(&path, StreamOptions::default())?;
    let values = (0..8).map(|value| {
        if value == 3 || value == 6 {
            StreamBoundaries::open_stream_reader(&path, StreamOptions::default()).unwrap();
        }
        Sample { value }
    });
    writer
        .push_samples(values, BatchOptions::default())
        .map_err(|e| e.source)?;
    assert_eq!(
        StreamBoundaries::open_stream_reader(&path, StreamOptions::default())?
            .samples()?
            .collect::<varve::Result<Vec<_>>>()?
            .len(),
        7
    );
    writer.immediate()?;
    writer.set_immediate_policy(ImmediatePolicy::new().after_records(2))?;
    writer.push_sample(&Sample { value: 10 })?;
    assert_eq!(
        StreamBoundaries::open_stream_reader(&path, StreamOptions::default())?
            .samples()?
            .collect::<varve::Result<Vec<_>>>()?
            .len(),
        9
    );
    writer.push_sample(&Sample { value: 11 })?;
    StreamBoundaries::open_stream_reader(&path, StreamOptions::default())?;

    let indexed_path = dir.path().join("unindexed.varve");
    let mut indexed = Boundaries::create_indexed_writer(&indexed_path, opts())?;
    let markers = (0..3).map(|value| {
        Boundaries::open_indexed_reader(&indexed_path, opts()).unwrap();
        Marker { value }
    });
    assert_eq!(
        indexed
            .push_markers(markers, BatchOptions::default())
            .map_err(|e| e.source)?
            .write_calls,
        3
    );
    Boundaries::open_indexed_reader(&indexed_path, opts())?;
    Ok(())
}

#[cfg(feature = "scalable-fault-injection")]
#[test]
fn automatic_immediate_failure_reports_the_already_written_prefix() -> varve::Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("failed.varve");
    let mut writer = Boundaries::create_indexed_writer(&path, opts())?;
    writer.set_immediate_policy(ImmediatePolicy::new().after_records(3))?;
    varve::VarveStreamWriter::inject_generation_restamp_failures(1);
    let error = writer
        .push_items(
            (0..8).map(|key| Item { key, value: key }),
            BatchOptions::default(),
        )
        .unwrap_err();
    varve::VarveStreamWriter::inject_generation_restamp_failures(0);
    assert_eq!(error.written.records, 3);
    assert!(
        matches!(error.source, Error::AppendedButImmediateFailed { sequence, .. } if Some(sequence) == error.written.last_sequence)
    );
    assert!(matches!(writer.immediate(), Err(Error::WriterPoisoned(_))));
    drop(writer);
    let _writer = Boundaries::restore_indexed_writer(&path, opts())?;
    assert!(
        Boundaries::open_indexed_reader(&path, opts())?
            .get_item(&0)?
            .is_none()
    );
    Ok(())
}

#[test]
fn process_exit_after_immediate_preserves_acknowledged_records() -> varve::Result<()> {
    for mode in ["explicit", "block", "policy", "value"] {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("crash.varve");
        let status = std::process::Command::new(std::env::current_exe()?)
            .args(["--exact", "immediate_crash_child", "--ignored"])
            .env("VARVE_IMMEDIATE_CHILD_PATH", &path)
            .env("VARVE_IMMEDIATE_CHILD_MODE", mode)
            .status()?;
        assert_eq!(status.code(), Some(73), "child mode {mode}");
        varve::clear_stale_writer_lock(&path, varve::WriterLockBreakPolicy::BreakIfProcessAbsent)?;
        let _writer = Boundaries::restore_indexed_writer(&path, opts())?;
        let reader = Boundaries::open_indexed_reader(&path, opts())?;
        for key in 0..3 {
            assert_eq!(reader.get_item(&key)?.unwrap().value, key, "mode {mode}");
        }
        assert!(reader.get_item(&3)?.is_none(), "unacknowledged mode {mode}");
        if mode == "block" {
            assert!(reader.get_durable(&1)?.is_some());
        }
        if mode == "value" {
            assert_eq!(reader.get_conditional(&1)?.unwrap().value, 2);
        }
    }
    Ok(())
}

#[test]
#[ignore = "subprocess entry point; exits without running writer destructors"]
fn immediate_crash_child() -> varve::Result<()> {
    let path = std::env::var_os("VARVE_IMMEDIATE_CHILD_PATH").expect("child path");
    let mode = std::env::var("VARVE_IMMEDIATE_CHILD_MODE").expect("child mode");
    let mut writer = Boundaries::create_indexed_writer(path, opts())?;
    if mode == "policy" {
        writer.set_immediate_policy(ImmediatePolicy::new().after_records(3))?;
    }
    writer
        .push_items(
            (0..3).map(|key| Item { key, value: key }),
            BatchOptions::default(),
        )
        .map_err(|e| e.source)?;
    match mode.as_str() {
        "explicit" => writer.immediate()?,
        "block" => {
            writer.push_durable(&Durable { key: 1 })?;
        }
        "value" => {
            writer.push_conditional(&Conditional { key: 1, value: 2 })?;
        }
        "policy" => {}
        _ => panic!("unknown child mode"),
    }
    writer.set_immediate_policy(ImmediatePolicy::default())?;
    writer
        .push_items(
            (3..9).map(|key| Item { key, value: key }),
            BatchOptions {
                max_records: 1,
                max_bytes: 1,
            },
        )
        .map_err(|e| e.source)?;
    writer.flush()?;
    // Unlike returning from the test this bypasses redb's close-time commit.
    std::process::exit(73)
}
