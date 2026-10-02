use varve::{MatrixCellStatus, varve_format};
varve_format! {
    pub format Generations {
        magic: b"MGEN";
        version: 1;
        dims { scan: u32, ch: u32, }
        commit: cell_bitmap { keyspace = [scan, ch]; categories = [data]; };
        aux { note: 32, }
        blocks {
            fixed Log(id = 2) { value: u64, }
            matrix Cell(id = 1, dims = [scan, ch], category = data) { value: u64, }
        }
    }
}
#[test]
fn dirty_overwrites_and_aux_remain_invisible_until_sync_and_follow() -> varve::Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("matrix.varve");
    let dims = GenerationsDims { scan: 2, ch: 2 };
    let key = CellKey { scan: 0, ch: 0 };
    let mut writer = Generations::create_writer_with_dims(&path, dims)?;
    writer.write_cell(key, &Cell { value: 100 })?;
    writer.commit_cell(key)?;
    writer.write_note_aux(0, &[1, 2, 3])?;
    writer.sync()?;
    let mut old = Generations::open_reader(&path)?;
    assert_eq!(old.cell(key)?.value, 100);
    writer.write_cell(key, &Cell { value: 200 })?;
    assert_eq!(old.cell(key)?.value, 100);
    writer.commit_cell(key)?;
    writer.write_note_aux(0, &[4, 5, 6])?;
    writer.flush()?;
    let dirty = Generations::open_reader(&path)?;
    assert_eq!(dirty.cell(key)?.value, 100);
    assert_eq!(dirty.read_note_aux(0, 3)?, [1, 2, 3]);
    old.follow()?;
    assert_eq!(old.cell(key)?.value, 100);
    writer.sync()?;
    assert_eq!(old.cell(key)?.value, 100);
    assert_eq!(dirty.cell(key)?.value, 100);
    old.follow()?;
    assert_eq!(old.cell(key)?.value, 200);
    assert_eq!(old.read_note_aux(0, 3)?, [4, 5, 6]);
    assert_eq!(old.cell_status(key)?, MatrixCellStatus::Committed);
    Ok(())
}

#[test]
fn readers_keep_one_generation_across_concurrent_overwrites_and_clear() -> varve::Result<()> {
    let policies = [
        varve::IntegrityPolicy::None,
        #[cfg(feature = "integrity")]
        varve::IntegrityPolicy::Crc32,
    ];
    for integrity in policies {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("threads.varve");
        let spec = Generations::spec().with_integrity_policy(integrity);
        let mut writer = spec.create_writer_with_dims(
            &path,
            varve::MatrixDimensions::from_pairs([("scan", 1024), ("ch", 2)]),
        )?;
        for ch in 0..2 {
            let key = varve::MatrixKey::new(ch * 1023, ch);
            writer.write_matrix_cell(key, &Cell { value: 0 })?;
            writer.commit_matrix_cell::<Cell>(key)?;
        }
        writer.sync()?;
        let ready = std::sync::Barrier::new(5);
        std::thread::scope(|scope| {
            for _ in 0..4 {
                let mut reader = spec.open_reader(&path).unwrap();
                let ready = &ready;
                scope.spawn(move || {
                    ready.wait();
                    for _ in 0..200 {
                        reader.follow().unwrap();
                        let a = reader
                            .read_matrix_cell::<Cell>(varve::MatrixKey::new(0, 0))
                            .unwrap()
                            .value;
                        let b = reader
                            .read_matrix_cell::<Cell>(varve::MatrixKey::new(1023, 1))
                            .unwrap()
                            .value;
                        assert_eq!(
                            a, b,
                            "two cells must come from the same captured generation"
                        );
                    }
                });
            }
            ready.wait();
            for generation in 1..=40 {
                for ch in 0..2 {
                    let key = varve::MatrixKey::new(ch * 1023, ch);
                    writer
                        .write_matrix_cell(key, &Cell { value: generation })
                        .unwrap();
                    writer.commit_matrix_cell::<Cell>(key).unwrap();
                }
                writer.sync().unwrap();
            }
        });
        let mut reader = spec.open_reader(&path)?;
        writer.clear_matrix_category("data")?;
        assert_eq!(
            reader
                .read_matrix_cell::<Cell>(varve::MatrixKey::new(0, 0))?
                .value,
            40
        );
        writer.sync()?;
        assert_eq!(
            reader
                .read_matrix_cell::<Cell>(varve::MatrixKey::new(0, 0))?
                .value,
            40
        );
        reader.follow()?;
        assert_eq!(
            reader.matrix_cell_status::<Cell>(varve::MatrixKey::new(0, 0))?,
            MatrixCellStatus::NotCommitted
        );
    }
    Ok(())
}

#[test]
fn reopened_writer_discards_unpublished_matrix_changes() -> varve::Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("recovery.varve");
    let key = CellKey { scan: 0, ch: 0 };
    let mut writer =
        Generations::create_writer_with_dims(&path, GenerationsDims { scan: 1, ch: 1 })?;
    writer.write_cell(key, &Cell { value: 17 })?;
    writer.commit_cell(key)?;
    writer.sync()?;
    writer.write_cell(key, &Cell { value: 99 })?;
    writer.commit_cell(key)?;
    writer.flush()?;
    drop(writer);
    let writer = Generations::open_writer(&path)?;
    assert_eq!(
        writer
            .inner()
            .read_matrix_cell::<Cell>(varve::MatrixKey::new(0, 0))?
            .value,
        17
    );
    Ok(())
}

#[test]
fn compaction_preserves_old_readers_and_follow_crosses_the_replacement() -> varve::Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("compact.varve");
    let key = CellKey { scan: 0, ch: 0 };
    let mut writer =
        Generations::create_writer_with_dims(&path, GenerationsDims { scan: 1, ch: 1 })?;
    writer.write_cell(key, &Cell { value: 1 })?;
    writer.commit_cell(key)?;
    writer.sync()?;
    let mut old = Generations::open_reader(&path)?;
    for value in 2..30 {
        writer.write_cell(key, &Cell { value })?;
        writer.commit_cell(key)?;
        writer.sync()?;
    }
    let log = std::fs::read_dir(dir.path())?
        .map(|e| e.unwrap().path())
        .find(|p| p.extension().is_some_and(|ext| ext == "vmg"))
        .unwrap();
    let before = log.metadata()?.len();
    writer.compact_matrix()?;
    let after = log.metadata()?.len();
    assert!(after < before / 2);
    assert_eq!(old.cell(key)?.value, 1);
    old.follow()?;
    assert_eq!(old.cell(key)?.value, 29);
    writer.write_cell(key, &Cell { value: 30 })?;
    writer.commit_cell(key)?;
    writer.sync()?;
    old.follow()?;
    assert_eq!(old.cell(key)?.value, 30);
    Ok(())
}

#[cfg(feature = "scalable-fault-injection")]
#[test]
fn publication_abort_child() -> varve::Result<()> {
    let Some(path) = std::env::var_os("VARVE_MATRIX_GENERATION_CHILD") else {
        return Ok(());
    };
    let mut writer = Generations::open_writer(path)?;
    for ch in 0..2 {
        let key = CellKey { scan: 0, ch };
        writer.write_cell(key, &Cell { value: 2 })?;
        writer.commit_cell(key)?;
    }
    writer.write_note_aux(0, &[2])?;
    writer.sync()?;
    panic!("the selected publication boundary did not abort");
}
#[cfg(feature = "scalable-fault-injection")]
#[test]
fn crashes_at_every_publication_boundary_keep_a_complete_generation() -> varve::Result<()> {
    for stage in [
        "data",
        "index",
        "data_sync",
        "head_prefix",
        "head",
        "head_sync",
    ] {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("crash.varve");
        let mut writer =
            Generations::create_writer_with_dims(&path, GenerationsDims { scan: 1, ch: 2 })?;
        for ch in 0..2 {
            let key = CellKey { scan: 0, ch };
            writer.write_cell(key, &Cell { value: 1 })?;
            writer.commit_cell(key)?;
        }
        writer.write_note_aux(0, &[1])?;
        writer.sync()?;
        drop(writer);
        let trace = dir.path().join("abort-witness");
        let status = std::process::Command::new(std::env::current_exe()?)
            .args(["--exact", "publication_abort_child", "--nocapture"])
            .env("VARVE_MATRIX_GENERATION_TRACE", &trace)
            .env("VARVE_MATRIX_GENERATION_CHILD", &path)
            .env("VARVE_MATRIX_GENERATION_ABORT", stage)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()?;
        assert!(
            !status.success(),
            "{stage}: child must terminate at injected boundary"
        );
        assert_eq!(std::fs::read_to_string(trace)?, stage);
        let reader = Generations::open_reader(&path)?;
        let a = reader.cell(CellKey { scan: 0, ch: 0 })?.value;
        let b = reader.cell(CellKey { scan: 0, ch: 1 })?.value;
        assert_eq!(a, b, "{stage}: mixed cell generations");
        assert_eq!(reader.read_note_aux(0, 1)?, [a as u8]);
        if ["data", "index", "data_sync", "head_prefix"].contains(&stage) {
            assert_eq!(a, 1, "{stage}");
        } else {
            assert_eq!(a, 2, "{stage}");
        }
        varve::clear_stale_writer_lock(&path, varve::WriterLockBreakPolicy::BreakIfProcessAbsent)?;
        let mut recovered = Generations::open_writer(&path)?;
        recovered.write_cell(CellKey { scan: 0, ch: 0 }, &Cell { value: 3 })?;
        recovered.commit_cell(CellKey { scan: 0, ch: 0 })?;
        recovered.sync()?;
        assert_eq!(reader.cell(CellKey { scan: 0, ch: 0 })?.value, a);
    }
    Ok(())
}

#[test]
fn explicit_immediate_publishes_and_cache_capacity_is_configurable() -> varve::Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("immediate.varve");
    let key = CellKey { scan: 0, ch: 0 };
    let mut writer =
        Generations::create_writer_with_dims(&path, GenerationsDims { scan: 1, ch: 1 })?;
    let mut reader = Generations::open_reader(&path)?;
    writer.write_cell(key, &Cell { value: 77 })?;
    writer.commit_cell(key)?;
    writer.immediate()?;
    assert_eq!(reader.cell_status(key)?, MatrixCellStatus::NotCommitted);
    reader.follow()?;
    assert_eq!(reader.cell(key)?.value, 77);
    for cache in [0, 4096, 65536] {
        let limits = varve::ReadLimits::STANDARD.with_matrix_generation_cache_bytes(cache);
        let reader = Generations::spec().open_reader_with_resource_limits(&path, limits)?;
        assert_eq!(
            reader
                .read_matrix_cell::<Cell>(varve::MatrixKey::new(0, 0))?
                .value,
            77
        );
        assert_eq!(
            reader.matrix_generation_path(),
            writer.matrix_generation_path()
        );
    }
    Ok(())
}

#[cfg(feature = "scalable-fault-injection")]
#[test]
fn publication_error_child() -> varve::Result<()> {
    let Some(path) = std::env::var_os("VARVE_MATRIX_GENERATION_ERROR_CHILD") else {
        return Ok(());
    };
    let stage = std::env::var("VARVE_MATRIX_GENERATION_FAIL").unwrap();
    let mut writer = Generations::open_writer(&path)?;
    let mut notified = false;
    let error = writer
        .inner_mut()
        .write_matrix_cell_durable(varve::MatrixKey::new(0, 0), &Cell { value: 2 }, |_| {
            notified = true;
            Ok(())
        })
        .unwrap_err();
    assert!(!notified);
    if stage == "head_sync" {
        assert!(matches!(
            error,
            varve::Error::MatrixCommittedButDurabilityUnproven { .. }
        ));
    } else {
        assert!(matches!(
            error,
            varve::Error::MatrixPublicationUncertain { .. }
        ));
    }
    assert!(matches!(
        writer.write_cell(CellKey { scan: 0, ch: 0 }, &Cell { value: 3 }),
        Err(varve::Error::WriterPoisoned(_))
    ));
    let reader = Generations::open_reader(&path)?;
    assert_eq!(
        reader.cell(CellKey { scan: 0, ch: 0 })?.value,
        if ["head", "head_sync"].contains(&stage.as_str()) {
            2
        } else {
            1
        }
    );
    Ok(())
}
#[cfg(feature = "scalable-fault-injection")]
#[test]
fn publication_io_errors_report_uncertainty_and_do_not_run_the_hook() -> varve::Result<()> {
    for stage in [
        "data",
        "index",
        "data_sync",
        "head_prefix",
        "head",
        "head_sync",
    ] {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("error.varve");
        let mut writer =
            Generations::create_writer_with_dims(&path, GenerationsDims { scan: 1, ch: 1 })?;
        writer.write_cell(CellKey { scan: 0, ch: 0 }, &Cell { value: 1 })?;
        writer.commit_cell(CellKey { scan: 0, ch: 0 })?;
        writer.sync()?;
        drop(writer);
        let output = std::process::Command::new(std::env::current_exe()?)
            .args(["--exact", "publication_error_child", "--nocapture"])
            .env("VARVE_MATRIX_GENERATION_ERROR_CHILD", &path)
            .env("VARVE_MATRIX_GENERATION_FAIL", stage)
            .output()?;
        assert!(
            output.status.success(),
            "{stage}: {} {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(())
}

#[test]
fn matrix_only_sync_never_publishes_a_native_tail_that_recovery_will_truncate() -> varve::Result<()>
{
    for mode in [
        varve::TransactionMarkerMode::Explicit,
        varve::TransactionMarkerMode::OnFlush,
    ] {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("marker.varve");
        let spec =
            Generations::spec().with_commit_policy(varve::CommitPolicy::TransactionMarker(mode));
        let dims = varve::MatrixDimensions::from_pairs([("scan", 1), ("ch", 1)]);
        let key = varve::MatrixKey::new(0, 0);
        let mut writer = spec.create_writer_with_dims(&path, dims)?;
        writer.write_matrix_cell(key, &Cell { value: 1 })?;
        writer.commit_matrix_cell::<Cell>(key)?;
        writer.push(&Log { value: 1 })?;
        writer.commit_durable()?;
        let mut old = spec.open_reader(&path)?;
        writer.write_matrix_cell(key, &Cell { value: 2 })?;
        writer.commit_matrix_cell::<Cell>(key)?;
        writer.push(&Log { value: 2 })?;
        writer.sync()?;
        old.follow()?;
        assert_eq!(old.read_matrix_cell::<Cell>(key)?.value, 2);
        assert_eq!(old.blocks::<Log>()?.len(), 1);
        drop(writer);
        let mut recovered = spec.open_writer(&path)?;
        assert_eq!(recovered.read_matrix_cell::<Cell>(key)?.value, 2);
        let reopened = spec.open_reader(&path)?;
        assert_eq!(reopened.blocks::<Log>()?.len(), 1);
        recovered.push(&Log { value: 3 })?;
        recovered.commit_durable()?;
        old.follow()?;
        assert_eq!(old.blocks::<Log>()?.len(), 2);
    }
    Ok(())
}

#[test]
fn skipped_generations_update_growing_shrinking_and_reordered_bitmap_indexes() -> varve::Result<()>
{
    for integrity in [
        varve::IntegrityPolicy::None,
        #[cfg(feature = "integrity")]
        varve::IntegrityPolicy::Crc32,
    ] {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("delta-model.varve");
        let spec = Generations::spec().with_integrity_policy(integrity);
        let mut writer = spec.create_writer_with_dims(
            &path,
            varve::MatrixDimensions::from_pairs([("scan", 32 * 32768), ("ch", 1)]),
        )?;
        let mut reader = spec.open_reader(&path)?;
        let mut state = 0x361794dbu64;
        let mut model = [None; 32];
        for batch in 0..36 {
            for _ in 0..9 {
                state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
                let page = ((state >> 32) % 32) as usize;
                let key = varve::MatrixKey::new(page as u64 * 32768, 0);
                if state & 4 == 0 {
                    writer.clear_matrix_cell::<Cell>(key)?;
                    model[page] = None;
                } else {
                    writer.write_matrix_cell(key, &Cell { value: state })?;
                    writer.commit_matrix_cell::<Cell>(key)?;
                    model[page] = Some(state);
                }
            }
            if batch % 11 == 10 {
                writer.clear_matrix_category("data")?;
                model.fill(None);
            }
            writer.sync()?;
            // Skip the compaction's own generation, then ordinary publications.
            if batch == 12 || batch == 13 {
                writer.compact_matrix()?;
            }
            if batch % 3 == 2 {
                reader.follow()?;
                let fresh = spec.open_reader(&path)?;
                for (page, expected) in model.iter().enumerate() {
                    let key = varve::MatrixKey::new(page as u64 * 32768, 0);
                    assert_eq!(
                        reader.matrix_cell_status::<Cell>(key)?,
                        fresh.matrix_cell_status::<Cell>(key)?
                    );
                    if let Some(value) = expected {
                        assert_eq!(reader.read_matrix_cell::<Cell>(key)?.value, *value);
                    } else {
                        assert_eq!(
                            reader.matrix_cell_status::<Cell>(key)?,
                            MatrixCellStatus::NotCommitted
                        );
                    }
                }
            }
        }
    }
    Ok(())
}

#[cfg(all(feature = "integrity", feature = "scalable-fault-injection"))]
#[test]
fn follow_verifies_only_changed_pages_and_rebuilds_only_after_compaction() -> varve::Result<()> {
    use varve::MatrixRecoveryReport as Counters;
    for live_pages in [8, 256] {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("delta-cost.varve");
        let spec = Generations::spec().with_integrity_policy(varve::IntegrityPolicy::Crc32);
        let mut writer = spec.create_writer_with_dims(
            &path,
            varve::MatrixDimensions::from_pairs([("scan", live_pages * 32768), ("ch", 1)]),
        )?;
        for page in 0..live_pages {
            let key = varve::MatrixKey::new(page * 32768, 0);
            writer.write_matrix_cell(key, &Cell { value: page })?;
            writer.commit_matrix_cell::<Cell>(key)?;
        }
        writer.sync()?;
        let mut reader = spec.open_reader(&path)?;
        let warm = varve::MatrixKey::new((live_pages - 1) * 32768, 0);
        reader.read_matrix_cell::<Cell>(warm)?;
        let mut follow_us = Vec::new();
        let mut open_us = Vec::new();
        for value in 1..=12 {
            // A new bit within the first existing bitmap page; no index growth.
            let key = varve::MatrixKey::new(value, 0);
            writer.write_matrix_cell(key, &Cell { value })?;
            writer.commit_matrix_cell::<Cell>(key)?;
            writer.sync()?;
            Counters::reset_matrix_integrity_counters();
            let start = std::time::Instant::now();
            reader.follow()?;
            follow_us.push(start.elapsed().as_micros());
            assert_eq!(
                Counters::matrix_open_bitmap_pages_visited(),
                1,
                "follow must not verify {live_pages} unchanged live pages"
            );
            Counters::reset_matrix_integrity_counters();
            reader.read_matrix_cell::<Cell>(warm)?;
            assert_eq!(
                Counters::matrix_lazy_fault_bytes_read(),
                0,
                "unchanged bitmap cache pages must survive follow"
            );
            let start = std::time::Instant::now();
            let fresh = spec.open_reader(&path)?;
            open_us.push(start.elapsed().as_micros());
            assert_eq!(fresh.read_matrix_cell::<Cell>(key)?.value, value);
        }
        follow_us.sort_unstable();
        open_us.sort_unstable();
        eprintln!(
            "matrix_follow_cost live_pages={live_pages} follow_median_us={} fresh_open_median_us={}",
            follow_us[6], open_us[6]
        );
        writer.compact_matrix()?;
        // Do not let a subsequent normal publication erase the marker.
        writer.write_matrix_aux("note", 0, &[42])?;
        writer.sync()?;
        Counters::reset_matrix_integrity_counters();
        reader.follow()?;
        assert!(Counters::matrix_open_bitmap_pages_visited() >= live_pages);
        writer.write_matrix_aux("note", 0, &[43])?;
        writer.sync()?;
        Counters::reset_matrix_integrity_counters();
        reader.follow()?;
        assert_eq!(Counters::matrix_open_bitmap_pages_visited(), 0);
    }
    Ok(())
}

#[test]
fn refused_delta_keeps_old_generation_and_can_retry_after_a_smaller_publication()
-> varve::Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("delta-limit.varve");
    let spec = Generations::spec().with_integrity_policy(varve::IntegrityPolicy::None);
    let mut writer = spec.create_writer_with_dims(
        &path,
        varve::MatrixDimensions::from_pairs([("scan", 2 * 32768), ("ch", 1)]),
    )?;
    let first = varve::MatrixKey::new(0, 0);
    let second = varve::MatrixKey::new(32768, 0);
    writer.write_matrix_cell(first, &Cell { value: 1 })?;
    writer.commit_matrix_cell::<Cell>(first)?;
    writer.sync()?;
    let limits = varve::ReadLimits::STANDARD
        .with_max_matrix_bitmap_bytes(48)
        .with_matrix_metadata_residency(varve::MatrixMetadataResidency::Lazy { cache_bytes: 0 })
        .with_matrix_generation_cache_bytes(0);
    let mut reader = spec.open_reader_with_resource_limits(&path, limits)?;
    let generation = reader.matrix_generation();
    writer.write_matrix_cell(first, &Cell { value: 2 })?;
    writer.commit_matrix_cell::<Cell>(first)?;
    writer.write_matrix_cell(second, &Cell { value: 3 })?;
    writer.commit_matrix_cell::<Cell>(second)?;
    writer.sync()?;
    for _ in 0..2 {
        assert!(matches!(
            reader.follow(),
            Err(varve::Error::LimitExceeded { .. })
        ));
        assert_eq!(reader.matrix_generation(), generation);
        assert_eq!(reader.read_matrix_cell::<Cell>(first)?.value, 1);
        assert_eq!(
            reader.matrix_cell_status::<Cell>(second)?,
            MatrixCellStatus::NotCommitted
        );
    }
    // The reader's refusal does not affect or wait for the writer.
    writer.clear_matrix_cell::<Cell>(second)?;
    writer.sync()?;
    reader.follow()?;
    assert_eq!(reader.read_matrix_cell::<Cell>(first)?.value, 2);
    assert_eq!(
        reader.matrix_cell_status::<Cell>(second)?,
        MatrixCellStatus::NotCommitted
    );
    Ok(())
}

#[test]
fn damaged_new_root_does_not_partially_advance_the_reader() -> varve::Result<()> {
    use std::io::{Read, Seek, SeekFrom, Write};
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("delta-damage.varve");
    let key = CellKey { scan: 0, ch: 0 };
    let mut writer =
        Generations::create_writer_with_dims(&path, GenerationsDims { scan: 1, ch: 1 })?;
    writer.write_cell(key, &Cell { value: 1 })?;
    writer.commit_cell(key)?;
    writer.sync()?;
    let mut reader = Generations::open_reader(&path)?;
    let generation = reader.matrix_generation();
    writer.write_cell(key, &Cell { value: 2 })?;
    writer.commit_cell(key)?;
    writer.sync()?;
    let mut log = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(writer.matrix_generation_path().unwrap())?;
    // The final appended page is the new root. Corrupt its checksum, then restore
    // it, so the retry exercises precisely the same candidate generation.
    let checksum = log.metadata()?.len() - 4;
    log.seek(SeekFrom::Start(checksum))?;
    let mut saved = [0; 4];
    log.read_exact(&mut saved)?;
    log.seek(SeekFrom::Start(checksum))?;
    log.write_all(&[saved[0] ^ 1, saved[1], saved[2], saved[3]])?;
    assert!(reader.follow().is_err());
    assert_eq!(reader.matrix_generation(), generation);
    assert_eq!(reader.cell(key)?.value, 1);
    log.seek(SeekFrom::Start(checksum))?;
    log.write_all(&saved)?;
    reader.follow()?;
    assert_eq!(reader.cell(key)?.value, 2);
    Ok(())
}
