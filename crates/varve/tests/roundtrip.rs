use std::fs::{read_dir, remove_file};
use std::path::PathBuf;

use varve::{
    OP_BLOCK_ID, ReplaceStrategy, TOMBSTONE_BLOCK_ID, VarveBlock, VarveMerge, compact_keyed_file,
    compact_keyed_files, merge_keyed_files, varve_format,
};

#[derive(Clone, Debug, PartialEq, VarveBlock)]
#[varve(id = 1, version = 1, kind = "fixed")]
struct Point {
    x: u32,
    y: u32,
}

#[derive(Clone, Debug, PartialEq, VarveBlock)]
#[varve(id = 2, version = 1, kind = "variable", key = "user_id, region")]
struct User {
    #[varve(field_id = 1)]
    user_id: u64,
    #[varve(field_id = 2)]
    region: u16,
    #[varve(field_id = 3)]
    name: String,
}

#[derive(Clone, Debug, PartialEq, VarveBlock)]
#[varve(id = 2, version = 2, kind = "variable", key = "user_id, region")]
struct UserV2 {
    #[varve(field_id = 1)]
    user_id: u64,
    #[varve(field_id = 2)]
    region: u16,
    #[varve(field_id = 3)]
    name: String,
}

#[derive(Clone, Debug, PartialEq, VarveBlock)]
#[varve(id = 3, version = 1, kind = "variable")]
struct UserOp {
    #[varve(field_id = 1)]
    rename_to: String,
}

impl VarveMerge for User {
    type Op = UserOp;

    fn apply_op(&mut self, op: Self::Op) -> varve::Result<()> {
        self.name = op.rename_to;
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, VarveBlock)]
#[varve(id = 4, version = 1, kind = "fixed")]
struct Nested {
    point: Point,
    score: i32,
}

varve_format! {
    pub struct TestFormat {
        magic: b"TVARVE";
        version: 1;
        endian: little;
        blocks: [Point, User, UserOp, Nested];
    }
}

varve_format! {
    pub struct TestFormatV2 {
        magic: b"TVARVE";
        version: 1;
        endian: little;
        blocks: [UserV2];
    }
}

#[test]
fn fixed_variable_nested_roundtrip_and_lazy_collections() -> varve::Result<()> {
    let path = temp_path("roundtrip");
    cleanup(&path);

    {
        let mut file = TestFormat::create(&path)?;
        file.push(&Point { x: 10, y: 20 })?;
        file.push(&User {
            user_id: 7,
            region: 82,
            name: "first".to_string(),
        })?;
        file.push(&Nested {
            point: Point { x: 1, y: 2 },
            score: -5,
        })?;
        file.flush()?;
    }

    let file = TestFormat::open_readonly(&path)?;
    let points = file.blocks::<Point>()?;
    assert_eq!(points.len(), 1);
    assert_eq!(points.get(0)?, Some(Point { x: 10, y: 20 }));

    let users = file.keyed_blocks::<User>()?;
    assert_eq!(
        users.get(&(7, 82))?,
        Some(User {
            user_id: 7,
            region: 82,
            name: "first".to_string(),
        })
    );

    let nested = file.blocks::<Nested>()?;
    assert_eq!(
        nested.get(0)?,
        Some(Nested {
            point: Point { x: 1, y: 2 },
            score: -5,
        })
    );

    cleanup(&path);
    Ok(())
}

#[test]
fn reader_writer_handles_cover_common_workflow() -> varve::Result<()> {
    let path = temp_path("reader_writer_handles");
    cleanup(&path);

    {
        let mut writer = TestFormat::create_writer(&path)?;
        assert_eq!(writer.mode(), varve::OpenMode::ReadWrite);
        writer.push(&Point { x: 44, y: 55 })?;
        writer.push(&User {
            user_id: 8,
            region: 1,
            name: "handle".to_string(),
        })?;
        writer.flush()?;
    }

    let reader = TestFormat::open_reader(&path)?;
    assert_eq!(reader.mode(), varve::OpenMode::ReadOnly);
    assert_eq!(
        reader.blocks::<Point>()?.get(0)?,
        Some(Point { x: 44, y: 55 })
    );
    assert_eq!(
        reader.keyed_blocks::<User>()?.get(&(8, 1))?,
        Some(User {
            user_id: 8,
            region: 1,
            name: "handle".to_string(),
        })
    );

    cleanup(&path);
    Ok(())
}

#[test]
fn fixed_replace_updates_existing_record_in_place() -> varve::Result<()> {
    let path = temp_path("replace");
    cleanup(&path);

    {
        let mut file = TestFormat::create(&path)?;
        file.push(&Point { x: 1, y: 2 })?;
        file.replace(0, &Point { x: 3, y: 4 }, ReplaceStrategy::FixedInPlace)?;
        file.flush()?;
    }

    let file = TestFormat::open_readonly(&path)?;
    let points = file.blocks::<Point>()?;
    assert_eq!(points.get(0)?, Some(Point { x: 3, y: 4 }));

    cleanup(&path);
    Ok(())
}

#[test]
fn variable_replace_rewrite_preserves_records_and_cleans_temp() -> varve::Result<()> {
    let path = temp_path("rewrite_variable");
    cleanup(&path);

    {
        let mut file = TestFormat::create(&path)?;
        file.push(&Point { x: 1, y: 2 })?;
        file.push(&User {
            user_id: 7,
            region: 82,
            name: "old".to_string(),
        })?;
        file.push(&Nested {
            point: Point { x: 3, y: 4 },
            score: 5,
        })?;
        file.replace(
            0,
            &User {
                user_id: 7,
                region: 82,
                name: "replacement with a different payload size".to_string(),
            },
            ReplaceStrategy::RewriteFile,
        )?;
        file.flush()?;
    }

    let file = TestFormat::open_readonly(&path)?;
    assert_eq!(file.blocks::<Point>()?.get(0)?, Some(Point { x: 1, y: 2 }));
    assert_eq!(
        file.blocks::<Nested>()?.get(0)?,
        Some(Nested {
            point: Point { x: 3, y: 4 },
            score: 5,
        })
    );
    assert_eq!(
        file.keyed_blocks::<User>()?.get(&(7, 82))?,
        Some(User {
            user_id: 7,
            region: 82,
            name: "replacement with a different payload size".to_string(),
        })
    );

    let parent = path.parent().unwrap_or_else(|| std::path::Path::new("."));
    let leaked_temp = read_dir(parent)?
        .filter_map(|entry| entry.ok())
        .any(|entry| {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            name.contains("rewrite_variable") && name.contains(".rewrite.")
        });
    assert!(!leaked_temp);

    cleanup(&path);
    Ok(())
}

#[test]
fn keyed_merge_applies_put_op_and_tombstone_by_sequence() -> varve::Result<()> {
    let base = temp_path("base");
    let delta = temp_path("delta");
    let output = temp_path("merged");
    cleanup(&base);
    cleanup(&delta);
    cleanup(&output);

    {
        let mut file = TestFormat::create(&base)?;
        file.push(&User {
            user_id: 1,
            region: 82,
            name: "base".to_string(),
        })?;
        file.push(&User {
            user_id: 2,
            region: 82,
            name: "delete-me".to_string(),
        })?;
        file.flush()?;
    }

    {
        let mut file = TestFormat::create(&delta)?;
        file.push_op::<User>(
            &(1, 82),
            &UserOp {
                rename_to: "renamed".to_string(),
            },
        )?;
        file.delete::<User>(&(2, 82))?;
        file.push(&User {
            user_id: 3,
            region: 82,
            name: "new".to_string(),
        })?;
        file.flush()?;
    }

    merge_keyed_files::<User, _>(TestFormat::spec(), &base, &[&delta], &output)?;
    let merged = TestFormat::open_readonly(&output)?;
    let users = merged.keyed_blocks::<User>()?;
    assert_eq!(users.len(), 2);
    assert_eq!(
        users.get(&(1, 82))?,
        Some(User {
            user_id: 1,
            region: 82,
            name: "renamed".to_string(),
        })
    );
    assert_eq!(users.get(&(2, 82))?, None);
    assert_eq!(
        users.get(&(3, 82))?,
        Some(User {
            user_id: 3,
            region: 82,
            name: "new".to_string(),
        })
    );

    cleanup(&base);
    cleanup(&delta);
    cleanup(&output);
    Ok(())
}

#[test]
fn merge_uses_shard_order_before_local_sequence() -> varve::Result<()> {
    let base = temp_path("base_order");
    let delta = temp_path("delta_order");
    let output = temp_path("merged_order");
    cleanup(&base);
    cleanup(&delta);
    cleanup(&output);

    {
        let mut file = TestFormat::create(&base)?;
        for index in 0..5 {
            file.push(&User {
                user_id: 9,
                region: 82,
                name: format!("base-{index}"),
            })?;
        }
        file.flush()?;
    }

    {
        let mut file = TestFormat::create(&delta)?;
        file.delete::<User>(&(9, 82))?;
        file.flush()?;
    }

    merge_keyed_files::<User, _>(TestFormat::spec(), &base, &[&delta], &output)?;
    let merged = TestFormat::open_readonly(&output)?;
    assert_eq!(merged.keyed_blocks::<User>()?.get(&(9, 82))?, None);

    cleanup(&base);
    cleanup(&delta);
    cleanup(&output);
    Ok(())
}

#[test]
fn materialized_keyed_blocks_apply_same_file_ops_and_tombstones() -> varve::Result<()> {
    let path = temp_path("materialized_state");
    cleanup(&path);

    {
        let mut file = TestFormat::create(&path)?;
        file.push(&User {
            user_id: 10,
            region: 82,
            name: "start".to_string(),
        })?;
        file.push_op::<User>(
            &(10, 82),
            &UserOp {
                rename_to: "after-op".to_string(),
            },
        )?;
        file.push(&User {
            user_id: 11,
            region: 82,
            name: "remove".to_string(),
        })?;
        file.delete::<User>(&(11, 82))?;
        file.flush()?;
    }

    let file = TestFormat::open_readonly(&path)?;
    let state = file.materialized_keyed_blocks::<User>()?;
    assert_eq!(state.len(), 1);
    assert_eq!(
        state.get(&(10, 82)).map(|user| user.name.as_str()),
        Some("after-op")
    );
    assert!(!state.contains_key(&(11, 82)));

    cleanup(&path);
    Ok(())
}

#[test]
fn compact_keyed_file_writes_only_final_keyed_state() -> varve::Result<()> {
    let input = temp_path("compact_input");
    let output = temp_path("compact_output");
    cleanup(&input);
    cleanup(&output);

    {
        let mut file = TestFormat::create(&input)?;
        file.push(&Point { x: 99, y: 100 })?;
        file.push(&User {
            user_id: 20,
            region: 82,
            name: "start".to_string(),
        })?;
        file.push_op::<User>(
            &(20, 82),
            &UserOp {
                rename_to: "after-op".to_string(),
            },
        )?;
        file.push(&User {
            user_id: 21,
            region: 82,
            name: "remove".to_string(),
        })?;
        file.delete::<User>(&(21, 82))?;
        file.push(&User {
            user_id: 22,
            region: 82,
            name: "final".to_string(),
        })?;
        file.flush()?;
    }
    {
        let mut stale = TestFormat::create(&output)?;
        stale.push(&Point { x: 1, y: 1 })?;
        stale.flush()?;
    }

    compact_keyed_file::<User, _>(TestFormat::spec(), &input, &output)?;

    let compacted = TestFormat::open_readonly(&output)?;
    let record_ids: Vec<_> = compacted.scan().map(|event| event.block_id).collect();
    assert_eq!(record_ids, vec![User::ID, User::ID]);
    assert!(!record_ids.contains(&Point::ID));
    assert!(!record_ids.contains(&OP_BLOCK_ID));
    assert!(!record_ids.contains(&TOMBSTONE_BLOCK_ID));

    let users = compacted.keyed_blocks::<User>()?;
    assert_eq!(users.len(), 2);
    assert_eq!(
        users.get(&(20, 82))?,
        Some(User {
            user_id: 20,
            region: 82,
            name: "after-op".to_string(),
        })
    );
    assert_eq!(users.get(&(21, 82))?, None);
    assert_eq!(
        users.get(&(22, 82))?,
        Some(User {
            user_id: 22,
            region: 82,
            name: "final".to_string(),
        })
    );

    cleanup(&input);
    cleanup(&output);
    Ok(())
}

#[test]
fn compact_keyed_files_materializes_base_and_delta_shards() -> varve::Result<()> {
    let base = temp_path("compact_base");
    let delta = temp_path("compact_delta");
    let output = temp_path("compact_shards_output");
    cleanup(&base);
    cleanup(&delta);
    cleanup(&output);

    {
        let mut file = TestFormat::create(&base)?;
        file.push(&User {
            user_id: 30,
            region: 82,
            name: "base".to_string(),
        })?;
        file.push(&User {
            user_id: 31,
            region: 82,
            name: "delete-me".to_string(),
        })?;
        file.flush()?;
    }
    {
        let mut file = TestFormat::create(&delta)?;
        file.push_op::<User>(
            &(30, 82),
            &UserOp {
                rename_to: "delta".to_string(),
            },
        )?;
        file.delete::<User>(&(31, 82))?;
        file.push(&User {
            user_id: 32,
            region: 82,
            name: "new".to_string(),
        })?;
        file.flush()?;
    }
    {
        let mut stale = TestFormat::create(&output)?;
        stale.push(&Point { x: 1, y: 1 })?;
        stale.flush()?;
    }

    compact_keyed_files::<User, _>(TestFormat::spec(), &base, &[&delta], &output)?;

    let compacted = TestFormat::open_readonly(&output)?;
    let record_ids: Vec<_> = compacted.scan().map(|event| event.block_id).collect();
    assert_eq!(record_ids, vec![User::ID, User::ID]);
    assert!(!record_ids.contains(&Point::ID));
    assert!(!record_ids.contains(&OP_BLOCK_ID));
    assert!(!record_ids.contains(&TOMBSTONE_BLOCK_ID));

    let users = compacted.keyed_blocks::<User>()?;
    assert_eq!(users.len(), 2);
    assert_eq!(
        users.get(&(30, 82))?,
        Some(User {
            user_id: 30,
            region: 82,
            name: "delta".to_string(),
        })
    );
    assert_eq!(users.get(&(31, 82))?, None);
    assert_eq!(
        users.get(&(32, 82))?,
        Some(User {
            user_id: 32,
            region: 82,
            name: "new".to_string(),
        })
    );

    cleanup(&base);
    cleanup(&delta);
    cleanup(&output);
    Ok(())
}

#[test]
fn merge_and_materialized_reject_block_version_mismatch() -> varve::Result<()> {
    let base = temp_path("base_v2_payload");
    let output = temp_path("merged_v2_payload");
    cleanup(&base);
    cleanup(&output);

    {
        let mut file = TestFormatV2::create(&base)?;
        file.push(&UserV2 {
            user_id: 1,
            region: 82,
            name: "v2".to_string(),
        })?;
        file.flush()?;
    }

    let file = TestFormat::open_readonly(&base)?;
    assert!(matches!(
        file.materialized_keyed_blocks::<User>(),
        Err(varve::Error::BlockVersionMismatch {
            block_id: 2,
            expected: 1,
            actual: 2
        })
    ));
    assert!(matches!(
        merge_keyed_files::<User, _>(TestFormat::spec(), &base, &[], &output),
        Err(varve::Error::BlockVersionMismatch {
            block_id: 2,
            expected: 1,
            actual: 2
        })
    ));

    cleanup(&base);
    cleanup(&output);
    Ok(())
}

fn temp_path(name: &str) -> PathBuf {
    let mut path = std::env::temp_dir();
    path.push(format!("varve_{name}_{}.vrv", std::process::id()));
    path
}

fn cleanup(path: &PathBuf) {
    let _ = remove_file(path);
    let mut lock = path.as_os_str().to_os_string();
    lock.push(".lock");
    let _ = remove_file(PathBuf::from(lock));
}
