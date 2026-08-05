//! What `max_materialized_bytes` bounds: one materialization, or a whole walk.
//!
//! `MaterializationBudget::consume` never refunds, so one budget threaded
//! through a loop over the index drains monotonically and bounds the SUM of
//! every record the loop touched. That is the right bound where the loop keeps
//! its results (`all_metadata`, `blocks_migrated`, `materialized_keyed_blocks`)
//! and the wrong one where each value is decoded, used and dropped — there the
//! live peak is one payload, and charging the sum refuses an undamaged file for
//! reading too much of it over time.
//!
//! Four entry points were on the wrong side of that line. Three of them are
//! read paths (`BlockVec::iter`, `VarveFile::metadata`, `VarveFile::keyed_blocks`);
//! the fourth, `key_tail_offsets`, is reached from `push_keyed`, so the drain
//! could refuse an APPEND to a healthy file.
//!
//! Every test here also asserts the ceiling still refuses a single record that
//! is genuinely too large — the discriminator against "fix" by deleting the
//! charge.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use varve::{Error, FormatSpec, ReadLimits, VarveBlock, varve_format};

#[derive(Clone, Debug, PartialEq, VarveBlock)]
#[varve(id = 1, version = 1, kind = "variable")]
struct Blob {
    #[varve(field_id = 1)]
    data: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, VarveBlock)]
#[varve(id = 2, version = 1, kind = "variable", key = "id")]
struct Entry {
    #[varve(field_id = 1)]
    id: u64,
    #[varve(field_id = 2)]
    name: String,
}

varve_format! {
    pub struct BudgetFormat {
        magic: b"BDGSCP";
        version: 1;
        limits {
            file_len: 1_073_741_824;
            records: 1_000_000;
            index_bytes: 134_217_728;
            scan_bytes: 1_073_741_824;
            record_payload: 16_777_216;
            logical_payload: 16_777_216;
            materialized_bytes: 268_435_456;
            keyed_tail: 16_777_216;
            segments: 1_000_000;
            sidecar: 16_777_216;
            mmap: 1_073_741_824;
        }
        endian: little;
        index: [scan_on_open, keyed_offset_chain];
        blocks: [Blob, Entry];
    }
}

/// One record of `RECORD_BYTES` logical payload comfortably fits; two do not.
const RECORD_BYTES: usize = 1024;

fn capped(bytes: u64) -> FormatSpec {
    BudgetFormat::spec()
        .tighten_read_limits(ReadLimits::missing().with_max_materialized_bytes(bytes))
}

static NEXT: AtomicU64 = AtomicU64::new(0);

fn temp_path(tag: &str) -> PathBuf {
    let id = NEXT.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "varve-bdgscp-{tag}-{}-{id}.varve",
        std::process::id()
    ))
}

fn cleanup(path: &Path) {
    let _ = std::fs::remove_file(path);
    let mut lock = path.as_os_str().to_os_string();
    lock.push(".lock");
    let _ = std::fs::remove_file(PathBuf::from(lock));
}

fn is_materialization_refusal(error: &Error) -> bool {
    matches!(
        error,
        Error::LimitExceeded {
            resource: "materialized bytes",
            ..
        }
    )
}

// ---------------------------------------------------------------------------
// BlockVec::iter
// ---------------------------------------------------------------------------

#[test]
fn iter_and_a_get_loop_bound_the_same_thing() -> varve::Result<()> {
    let path = temp_path("iter");
    cleanup(&path);
    {
        let mut file = BudgetFormat::spec().create(&path)?;
        for value in 0..10u8 {
            file.push(&Blob {
                data: vec![value; RECORD_BYTES],
            })?;
        }
        file.flush()?;
    }

    // 4 KiB admits one 1 KiB record (payload + decode) with room to spare and
    // refuses the sum of ten. `get(i)` in a loop always yielded all ten;
    // `iter()` yielded two and then refused, and that asymmetry over the same
    // records is the defect.
    let file = capped(4096).open_readonly(&path)?;
    let blocks = file.blocks::<Blob>()?;
    assert_eq!(blocks.len(), 10);

    let by_index = (0..blocks.len())
        .map(|index| blocks.get(index))
        .collect::<varve::Result<Vec<_>>>()?;
    assert_eq!(by_index.len(), 10);

    let by_iter = blocks.iter().collect::<varve::Result<Vec<_>>>()?;
    assert_eq!(by_iter.len(), 10, "iter() must reach the end of the block");
    assert_eq!(
        by_iter,
        by_index.into_iter().flatten().collect::<Vec<_>>(),
        "the two walks must yield the same values, not merely the same count",
    );

    cleanup(&path);
    Ok(())
}

#[test]
fn a_ceiling_below_one_record_still_refuses_the_first_item() -> varve::Result<()> {
    let path = temp_path("iter_refuses");
    cleanup(&path);
    {
        let mut file = BudgetFormat::spec().create(&path)?;
        for value in 0..10u8 {
            file.push(&Blob {
                data: vec![value; RECORD_BYTES],
            })?;
        }
        file.flush()?;
    }

    // The discriminator against deleting the budget from iteration rather than
    // resetting it per record: 512 bytes cannot materialize even one record.
    let file = capped(512).open_readonly(&path)?;
    let blocks = file.blocks::<Blob>()?;

    let from_get = blocks.get(0).expect_err("get must refuse the first record");
    assert!(is_materialization_refusal(&from_get), "{from_get:?}");

    let from_iter = blocks
        .iter()
        .next()
        .expect("the iterator must yield an item")
        .expect_err("iter must refuse the first record");
    assert!(is_materialization_refusal(&from_iter), "{from_iter:?}");

    cleanup(&path);
    Ok(())
}

// ---------------------------------------------------------------------------
// VarveFile::metadata
// ---------------------------------------------------------------------------

#[test]
fn metadata_lookup_is_bounded_per_record_and_still_latest_wins() -> varve::Result<()> {
    let path = temp_path("metadata");
    cleanup(&path);
    {
        let mut file = BudgetFormat::spec().create(&path)?;
        for index in 0..16u8 {
            file.write_metadata(&format!("k{index}"), &vec![index; RECORD_BYTES])?;
        }
        // k0 written a second time: latest-wins needs the whole walk, so no
        // early break on the first key match is admissible.
        file.write_metadata("k0", &vec![0xAA; RECORD_BYTES])?;
        file.flush()?;
    }

    // 16 KiB of metadata against a 4 KiB ceiling. Before the fix the walk
    // drained the budget around the second record and every lookup — including
    // one for the first key in the file — failed with LimitExceeded.
    let file = capped(4096).open_readonly(&path)?;
    assert_eq!(file.metadata("k15")?, Some(vec![15u8; RECORD_BYTES]));
    assert_eq!(file.metadata("k1")?, Some(vec![1u8; RECORD_BYTES]));
    assert_eq!(
        file.metadata("k0")?,
        Some(vec![0xAAu8; RECORD_BYTES]),
        "the newest write for a key wins; an early break would return the first",
    );
    assert_eq!(file.metadata("absent")?, None);

    cleanup(&path);
    Ok(())
}

#[test]
fn metadata_still_refuses_a_single_entry_over_the_ceiling() -> varve::Result<()> {
    let path = temp_path("metadata_refuses");
    cleanup(&path);
    {
        let mut file = BudgetFormat::spec().create(&path)?;
        file.write_metadata("big", &vec![7u8; 8192])?;
        file.flush()?;
    }

    // The discriminator against deleting the charge: one entry larger than the
    // ceiling must still be refused.
    let file = capped(4096).open_readonly(&path)?;
    let error = file
        .metadata("big")
        .expect_err("an 8 KiB entry under a 4 KiB ceiling must be refused");
    assert!(is_materialization_refusal(&error), "{error:?}");

    // And the refusal does not depend on asking for that key: reaching it at
    // all charges it.
    let error = file
        .metadata("absent")
        .expect_err("the oversized record is still framed by the walk");
    assert!(is_materialization_refusal(&error), "{error:?}");

    cleanup(&path);
    Ok(())
}

// ---------------------------------------------------------------------------
// key_tail_offsets, reached from push_keyed: the append path
// ---------------------------------------------------------------------------

/// Writes `count` keyed records and returns the physical payload length of one
/// of them. No compression is declared, so that is also the logical length the
/// budget is charged.
fn write_keyed(path: &Path, count: u64) -> varve::Result<u64> {
    let mut file = BudgetFormat::spec().create(path)?;
    for id in 0..count {
        file.push_keyed(&Entry {
            id,
            name: format!("entry-{id:04}"),
        })?;
    }
    file.flush()?;
    let payload_len = file
        .index_entries()
        .iter()
        .find(|entry| entry.block_id == 2)
        .expect("a keyed record was written")
        .payload_len;
    Ok(payload_len)
}

#[test]
fn a_healthy_keyed_file_stays_appendable_under_a_small_ceiling() -> varve::Result<()> {
    let path = temp_path("keyed_append");
    cleanup(&path);
    let payload_len = write_keyed(&path, 6)?;

    // Twice one record's payload: enough for one record's materialization
    // (payload plus the name the decode allocates, which is shorter than the
    // payload), far short of six. The tail map is built from the file on the
    // first keyed push after open, so this is the append that used to fail.
    let ceiling = payload_len * 2;
    let mut file = capped(ceiling).open(&path)?;
    file.push_keyed(&Entry {
        id: 6,
        name: "entry-0006".to_string(),
    })?;
    file.flush()?;
    drop(file);

    let reader = BudgetFormat::spec().open_readonly(&path)?;
    assert_eq!(
        reader
            .index_entries()
            .iter()
            .filter(|entry| entry.block_id == 2)
            .count(),
        7,
    );

    // The same ceiling on the resident keyed read path.
    let bounded = capped(ceiling).open_readonly(&path)?;
    assert_eq!(bounded.keyed_blocks::<Entry>()?.len(), 7);

    cleanup(&path);
    Ok(())
}

#[test]
fn a_ceiling_below_one_keyed_record_still_refuses_the_append() -> varve::Result<()> {
    let path = temp_path("keyed_append_refuses");
    cleanup(&path);
    let payload_len = write_keyed(&path, 6)?;

    // The discriminator: a per-record ceiling is still a ceiling. Removing the
    // charge from the tail build instead of resetting it per record passes the
    // test above and fails this one.
    let mut file = capped(payload_len - 1).open(&path)?;
    let error = file
        .push_keyed(&Entry {
            id: 6,
            name: "entry-0006".to_string(),
        })
        .expect_err("a ceiling below one record must refuse the tail build");
    assert!(is_materialization_refusal(&error), "{error:?}");

    cleanup(&path);
    Ok(())
}
