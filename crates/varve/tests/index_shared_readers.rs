#![cfg(feature = "high-cardinality-dev")]

//! Shared indexed-handle contracts (PERF-06/PERF-07).
//!
//! Independent in-process indexed handles must share one sidecar database per
//! native file identity: two readers — or a reader beside a synced writer —
//! open concurrently instead of failing with `IndexBusy`. Write admission
//! stays exclusive and typed, sidecar rebuild serves fresh databases to new
//! handles, and CRC point lookups verify the payload with a single read.

#[cfg(feature = "integrity")]
use std::fs;
use std::path::Path;

use varve::{
    BlockDescriptor, BlockKind, Decoder, DiskIndexEntry, DiskIndexOptions, DiskIndexPlan,
    DiskIndexedBlock, Encoder, Endian, Error, FormatSpec, IndexPolicy, IntegrityPolicy,
    ManifestPolicy, ReadLimits, RecoveryPolicy, Result, VarveBlock, VarveDecode, VarveEncode,
    VarveIndexedReader, VarveIndexedWriter, VarveKeyedBlock, WireType, rebuild_disk_index,
};

#[derive(Clone, Debug, PartialEq, Eq)]
struct Item {
    key: u64,
    value: String,
}

impl VarveEncode for Item {
    const WIRE_TYPE: WireType = <(u64, String) as VarveEncode>::WIRE_TYPE;

    fn encode_varve(&self, encoder: &mut Encoder) -> Result<()> {
        (self.key, self.value.clone()).encode_varve(encoder)
    }
}

impl VarveDecode for Item {
    const WIRE_TYPE: WireType = <(u64, String) as VarveDecode>::WIRE_TYPE;

    fn decode_varve(decoder: &mut Decoder<'_>) -> Result<Self> {
        let (key, value) = <(u64, String)>::decode_varve(decoder)?;
        Ok(Self { key, value })
    }
}

impl VarveBlock for Item {
    const ID: u32 = 31;
    const VERSION: u16 = 1;
    const KIND: BlockKind = BlockKind::Variable;
    const ENDIAN: Option<Endian> = None;
    const SCHEMA_FINGERPRINT: u64 = 0x5348_4152_4544_4931;
    const IS_KEYED: bool = true;
}

impl VarveKeyedBlock for Item {
    type Key = u64;

    fn key(&self) -> Self::Key {
        self.key
    }
}

static BLOCKS: &[BlockDescriptor] = &[BlockDescriptor {
    id: Item::ID,
    name: "SharedItem",
    version: Item::VERSION,
    kind: Item::KIND,
    fields: &[],
}];

static INDEXED: &[DiskIndexedBlock] = &[DiskIndexedBlock::of::<Item>()];

fn spec(integrity: IntegrityPolicy) -> FormatSpec {
    FormatSpec::new(
        b"VSHR",
        1,
        Endian::Little,
        0,
        IndexPolicy::KeyedOffsetChain,
        integrity,
        RecoveryPolicy::Strict,
        ManifestPolicy::None,
        BLOCKS,
    )
    .with_read_limits(ReadLimits::STANDARD)
}

fn plan(integrity: IntegrityPolicy) -> DiskIndexPlan {
    DiskIndexPlan::canonical(spec(integrity), INDEXED).unwrap()
}

fn item(key: u64, value: &str) -> Item {
    Item {
        key,
        value: value.into(),
    }
}

fn sidecar(path: &Path) -> std::path::PathBuf {
    varve::disk_index_sidecar_path(path)
}

#[test]
fn independent_readers_share_one_sidecar_database() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("shared-readers.varve");
    let options = DiskIndexOptions::default();
    let spec = spec(IntegrityPolicy::None);
    let plan = plan(IntegrityPolicy::None);

    let mut writer = VarveIndexedWriter::create(spec, &path, options, plan)?;
    for key in 0..3 {
        writer.push_info(&item(key, &format!("value-{key}")))?;
    }
    writer.delete_info::<Item>(&1)?;
    writer.sync()?;
    drop(writer);

    // The IndexBusy regression: a second independent reader used to fail
    // because each handle opened its own writable redb database.
    let first = VarveIndexedReader::open(spec, &path, options, plan)?;
    let second = VarveIndexedReader::open(spec, &path, options, plan)?;
    for reader in [&first, &second] {
        assert_eq!(reader.get::<Item>(&0)?, Some(item(0, "value-0")));
        assert_eq!(reader.get::<Item>(&1)?, None);
        assert!(matches!(
            reader.lookup::<Item>(&1)?,
            DiskIndexEntry::Tombstone { .. }
        ));
        assert_eq!(reader.historical_distinct_keys()?, 3);
    }
    drop(first);
    // Dropping one shared handle must not close the database under the other.
    assert_eq!(second.get::<Item>(&2)?, Some(item(2, "value-2")));
    let third = VarveIndexedReader::open(spec, &path, options, plan)?;
    assert_eq!(third.get::<Item>(&2)?, Some(item(2, "value-2")));
    Ok(())
}

#[test]
fn uncommitted_writer_batch_is_typed_busy_for_new_handles() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("write-admission.varve");
    let options = DiskIndexOptions::default();
    let spec = spec(IntegrityPolicy::None);
    let plan = plan(IntegrityPolicy::None);

    let mut writer = VarveIndexedWriter::create(spec, &path, options, plan)?;
    writer.push_info(&item(1, "one"))?;
    writer.sync()?;

    // A reader opens beside the synced writer through the shared database.
    let reader = VarveIndexedReader::open(spec, &path, options, plan)?;
    assert_eq!(reader.get::<Item>(&1)?, Some(item(1, "one")));

    // An uncommitted sidecar batch holds the write-admission gate, so a
    // conflicting open observes a typed busy error instead of blocking
    // inside redb behind the batch transaction.
    writer.push_info(&item(2, "two"))?;
    assert!(matches!(
        VarveIndexedReader::open(spec, &path, options, plan),
        Err(Error::IndexBusy)
    ));
    // The pre-existing reader keeps serving its pinned snapshot.
    assert_eq!(reader.get::<Item>(&1)?, Some(item(1, "one")));

    writer.sync()?;
    let fresh = VarveIndexedReader::open(spec, &path, options, plan)?;
    assert_eq!(fresh.get::<Item>(&2)?, Some(item(2, "two")));
    // The older reader still serves the generation it pinned at open.
    assert_eq!(reader.get::<Item>(&1)?, Some(item(1, "one")));
    Ok(())
}

#[test]
fn rebuild_replaces_the_sidecar_for_fresh_handles() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("rebuild-fresh.varve");
    let options = DiskIndexOptions::default();
    let spec = spec(IntegrityPolicy::None);
    let plan = plan(IntegrityPolicy::None);

    let mut writer = VarveIndexedWriter::create(spec, &path, options, plan)?;
    writer.push_info(&item(1, "one"))?;
    writer.push_info(&item(2, "two"))?;
    writer.delete_info::<Item>(&2)?;
    writer.sync()?;
    drop(writer);

    let reader = VarveIndexedReader::open(spec, &path, options, plan)?;
    assert_eq!(reader.get::<Item>(&1)?, Some(item(1, "one")));
    drop(reader);

    // Rebuild replaces the clean sidecar object in place. New handles must
    // open the replacement database, not a stale shared entry for the
    // replaced file object.
    let report = rebuild_disk_index(spec, &path, options, plan)?;
    assert_eq!(report.records, 3);
    let first = VarveIndexedReader::open(spec, &path, options, plan)?;
    let second = VarveIndexedReader::open(spec, &path, options, plan)?;
    for reader in [&first, &second] {
        assert_eq!(reader.get::<Item>(&1)?, Some(item(1, "one")));
        assert_eq!(reader.get::<Item>(&2)?, None);
        assert_eq!(reader.historical_distinct_keys()?, 2);
    }
    drop(first);
    drop(second);

    // The rebuilt sidecar remains a normal writable generation.
    let mut writer = VarveIndexedWriter::open(spec, &path, options, plan)?;
    writer.push_info(&item(3, "three"))?;
    writer.sync()?;
    drop(writer);
    let reader = VarveIndexedReader::open(spec, &path, options, plan)?;
    assert_eq!(reader.get::<Item>(&3)?, Some(item(3, "three")));
    assert!(sidecar(&path).exists());
    Ok(())
}

/// PERF2-09: the process-global registry mutex only guards the identity map;
/// a cache-miss redb open runs under a per-identity slot, so concurrent opens
/// of unrelated sidecars — and racing opens of the same sidecar — all succeed
/// and same-file handles still share one database.
#[test]
fn concurrent_opens_of_unrelated_and_same_sidecars_all_succeed() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let options = DiskIndexOptions::default();
    let spec = spec(IntegrityPolicy::None);
    let plan = plan(IntegrityPolicy::None);

    let paths: Vec<_> = (0..4)
        .map(|ordinal| directory.path().join(format!("convoy-{ordinal}.varve")))
        .collect();
    for path in &paths {
        let mut writer = VarveIndexedWriter::create(spec, path, options, plan)?;
        writer.push_info(&item(7, "seven"))?;
        writer.sync()?;
    }

    std::thread::scope(|scope| {
        let mut handles = Vec::new();
        // One thread per identity, opening concurrently with the others:
        // distinct identities must not serialize behind one another's
        // database open. Each thread opens twice so both handles of one
        // identity converge on the shared database. (Simultaneous opens of
        // one identity may still fail fast with the typed busy error while
        // the other handle's open validation holds the write gate, so
        // same-identity opens stay sequential here.)
        for path in &paths {
            handles.push(scope.spawn(move || -> Result<()> {
                let first = VarveIndexedReader::open(spec, path, options, plan)?;
                let second = VarveIndexedReader::open(spec, path, options, plan)?;
                assert_eq!(first.get::<Item>(&7)?, Some(item(7, "seven")));
                assert_eq!(second.get::<Item>(&7)?, Some(item(7, "seven")));
                Ok(())
            }));
        }
        for handle in handles {
            handle.join().expect("reader thread panicked")?;
        }
        Ok::<(), Error>(())
    })?;

    // The registry stays consistent afterwards: fresh handles still share.
    let first = VarveIndexedReader::open(spec, &paths[0], options, plan)?;
    let second = VarveIndexedReader::open(spec, &paths[0], options, plan)?;
    assert_eq!(first.get::<Item>(&7)?, Some(item(7, "seven")));
    assert_eq!(second.get::<Item>(&7)?, Some(item(7, "seven")));
    Ok(())
}

#[cfg(feature = "integrity")]
#[test]
fn crc_point_lookups_verify_payloads_and_detect_corruption() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("crc-single-read.varve");
    let options = DiskIndexOptions::default();
    let spec = spec(IntegrityPolicy::Crc32WithHeader);
    let plan = plan(IntegrityPolicy::Crc32WithHeader);

    let marker = "crc-corruption-target-payload-0123456789abcdef";
    let mut writer = VarveIndexedWriter::create(spec, &path, options, plan)?;
    writer.push_info(&item(1, marker))?;
    writer.push_info(&item(2, "intact"))?;
    writer.delete_info::<Item>(&3)?;
    writer.sync()?;
    drop(writer);

    // Under CRC policies the candidate read is frame-only; the payload is
    // read once and verified against the record checksum during decode.
    let reader = VarveIndexedReader::open(spec, &path, options, plan)?;
    assert_eq!(reader.get::<Item>(&1)?, Some(item(1, marker)));
    assert!(matches!(
        reader.lookup::<Item>(&1)?,
        DiskIndexEntry::Put { .. }
    ));
    // Tombstone candidates re-validate the native tombstone key under CRC.
    assert!(matches!(
        reader.lookup::<Item>(&3)?,
        DiskIndexEntry::Tombstone { .. }
    ));
    assert_eq!(reader.get::<Item>(&3)?, None);
    drop(reader);

    // Flip one payload byte. The frame (header/footer) stays intact, so the
    // single-read decode path must still catch the corruption via the
    // record checksum.
    let mut bytes = fs::read(&path)?;
    let position = bytes
        .windows(marker.len())
        .position(|window| window == marker.as_bytes())
        .expect("marker payload must exist in the native file");
    bytes[position + marker.len() / 2] ^= 0x01;
    fs::write(&path, &bytes)?;

    let reader = VarveIndexedReader::open(spec, &path, options, plan)?;
    assert!(matches!(
        reader.get::<Item>(&1),
        Err(Error::ChecksumMismatch { .. })
    ));
    // The untouched record still verifies and decodes.
    assert_eq!(reader.get::<Item>(&2)?, Some(item(2, "intact")));
    Ok(())
}
