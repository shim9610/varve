#![cfg(feature = "high-cardinality-dev")]

use std::path::{Path, PathBuf};

use varve::{
    BatchOptions, BlockDescriptor, BlockKind, CommitPolicy, Decoder, DiskIndexBatchOptions,
    DiskIndexError, DiskIndexOptions, Encoder, Endian, Error, FormatSpec, IndexPolicy,
    IntegrityPolicy, ManifestPolicy, ReadLimits, RecoveryPolicy, Result, ScanOptions,
    ScanProgressPhase, StreamOptions, VarveBlock, VarveDecode, VarveEncode, VarveStreamReader,
    VarveStreamWriter, WireType, bootstrap_stream_checkpoint_with_progress,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Value(u64);

impl VarveEncode for Value {
    const WIRE_TYPE: WireType = WireType::U64;

    fn encode_varve(&self, encoder: &mut Encoder) -> Result<()> {
        self.0.encode_varve(encoder)
    }
}

impl VarveDecode for Value {
    const WIRE_TYPE: WireType = WireType::U64;

    fn decode_varve(decoder: &mut Decoder<'_>) -> Result<Self> {
        Ok(Self(u64::decode_varve(decoder)?))
    }
}

impl VarveBlock for Value {
    const ID: u32 = 61;
    const VERSION: u16 = 1;
    const KIND: BlockKind = BlockKind::Fixed;
    const ENDIAN: Option<Endian> = None;
    const IS_KEYED: bool = false;
    const SCHEMA_FINGERPRINT: u64 = 0x5045_5246_4354_0061;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Extra(u64);

impl VarveEncode for Extra {
    const WIRE_TYPE: WireType = WireType::U64;

    fn encode_varve(&self, encoder: &mut Encoder) -> Result<()> {
        self.0.encode_varve(encoder)
    }
}

impl VarveDecode for Extra {
    const WIRE_TYPE: WireType = WireType::U64;

    fn decode_varve(decoder: &mut Decoder<'_>) -> Result<Self> {
        Ok(Self(u64::decode_varve(decoder)?))
    }
}

impl VarveBlock for Extra {
    const ID: u32 = 62;
    const VERSION: u16 = 1;
    const KIND: BlockKind = BlockKind::Fixed;
    const ENDIAN: Option<Endian> = None;
    const IS_KEYED: bool = false;
    const SCHEMA_FINGERPRINT: u64 = 0x5045_5246_4354_0062;
}

static BLOCKS: &[BlockDescriptor] = &[
    BlockDescriptor {
        id: Value::ID,
        name: "PerfContractValue",
        version: Value::VERSION,
        kind: Value::KIND,
        fields: &[],
    },
    BlockDescriptor {
        id: Extra::ID,
        name: "PerfContractExtra",
        version: Extra::VERSION,
        kind: Extra::KIND,
        fields: &[],
    },
];

fn chain_spec(integrity: IntegrityPolicy) -> FormatSpec {
    FormatSpec::new(
        b"VPERFCT",
        1,
        Endian::Little,
        0,
        IndexPolicy::BlockOffsetChain,
        integrity,
        RecoveryPolicy::Strict,
        ManifestPolicy::None,
        BLOCKS,
    )
    .with_commit_policy(CommitPolicy::RecordFooter)
    .with_read_limits(ReadLimits::finite_all(u64::MAX))
}

/// Bounds the writer's sidecar transaction at two coverage items so a small
/// number of scalar pushes crosses several `chunk_records` boundaries.
fn small_chunk_options() -> StreamOptions {
    StreamOptions {
        state: DiskIndexOptions {
            max_key_bytes: 1024,
            batch: DiskIndexBatchOptions {
                max_records: 2,
                max_bytes: 4096,
            },
            ..DiskIndexOptions::default()
        },
        ..StreamOptions::default()
    }
}

fn state_sidecar(path: &Path) -> PathBuf {
    let mut value = path.as_os_str().to_owned();
    value.push(".vks");
    PathBuf::from(value)
}

fn last_offset_of_block(reader: &VarveStreamReader, block_id: u32) -> Result<Option<u64>> {
    let mut last = None;
    for event in reader.events()? {
        let event = event?;
        if event.block_id == block_id {
            last = Some(event.record_offset);
        }
    }
    Ok(last)
}

#[test]
fn scalar_pushes_survive_sync_and_reopen() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("scalar-batched.varve");
    let spec = chain_spec(IntegrityPolicy::None);
    let mut previous = None;
    {
        let mut writer = VarveStreamWriter::create(spec, &path, small_chunk_options())?;
        for value in 0..5 {
            let info = writer.push_info(&Value(value))?;
            assert_eq!(info.prev_same_block_offset, previous);
            previous = Some(info.record_offset);
        }
        writer.sync()?;
        for value in 5..7 {
            let info = writer.push_info(&Value(value))?;
            assert_eq!(info.prev_same_block_offset, previous);
            previous = Some(info.record_offset);
        }
        writer.sync()?;
    }
    {
        let reader = VarveStreamReader::open(spec, &path, small_chunk_options())?;
        let values = reader.blocks::<Value>()?.collect::<Result<Vec<_>>>()?;
        assert_eq!(values, (0..7).map(Value).collect::<Vec<_>>());
        assert_eq!(last_offset_of_block(&reader, Value::ID)?, previous);
    }
    {
        // The reopened writer rebuilds its block tails from the sidecar, so the
        // collapsed per-chunk tails must still name the true final record.
        let mut writer = VarveStreamWriter::open(spec, &path, small_chunk_options())?;
        let info = writer.push_info(&Value(7))?;
        assert_eq!(info.prev_same_block_offset, previous);
        writer.sync()?;
    }
    let reader = VarveStreamReader::open(spec, &path, small_chunk_options())?;
    let values = reader.blocks::<Value>()?.collect::<Result<Vec<_>>>()?;
    assert_eq!(values, (0..8).map(Value).collect::<Vec<_>>());
    Ok(())
}

#[test]
fn interleaved_scalar_and_plural_appends_stay_consistent() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("interleaved.varve");
    let spec = chain_spec(IntegrityPolicy::None);
    {
        let mut writer = VarveStreamWriter::create(spec, &path, small_chunk_options())?;
        writer.push_info(&Value(0))?;
        // Five records with a state bound of two coverage items force the
        // native batch to split into multiple prepared chunks.
        let report = writer
            .push_iter::<Value, _>(
                (1..=5).map(Value).collect::<Vec<_>>(),
                BatchOptions {
                    max_records: 100,
                    max_bytes: usize::MAX,
                },
            )
            .map_err(|error| error.source)?;
        assert_eq!(report.records, 5);
        assert!(report.write_calls >= 3);
        writer.push_info(&Extra(100))?;
        writer.push_info(&Value(6))?;
        writer.sync()?;
    }
    let (last_value, last_extra) = {
        let reader = VarveStreamReader::open(spec, &path, small_chunk_options())?;
        let values = reader.blocks::<Value>()?.collect::<Result<Vec<_>>>()?;
        assert_eq!(values, (0..7).map(Value).collect::<Vec<_>>());
        let extras = reader.blocks::<Extra>()?.collect::<Result<Vec<_>>>()?;
        assert_eq!(extras, [Extra(100)]);
        (
            last_offset_of_block(&reader, Value::ID)?,
            last_offset_of_block(&reader, Extra::ID)?,
        )
    };
    let mut writer = VarveStreamWriter::open(spec, &path, small_chunk_options())?;
    assert_eq!(
        writer.push_info(&Value(7))?.prev_same_block_offset,
        last_value
    );
    assert_eq!(
        writer.push_info(&Extra(101))?.prev_same_block_offset,
        last_extra
    );
    writer.sync()?;
    Ok(())
}

#[cfg(feature = "integrity")]
#[test]
fn crc_typed_scan_decodes_all_values() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("crc-scan.varve");
    let spec = chain_spec(IntegrityPolicy::Crc32WithHeader);
    {
        let mut writer = VarveStreamWriter::create(spec, &path, small_chunk_options())?;
        for value in 0..4 {
            writer.push_info(&Value(value))?;
        }
        writer
            .push_iter::<Value, _>(
                (4..20).map(Value).collect::<Vec<_>>(),
                BatchOptions::default(),
            )
            .map_err(|error| error.source)?;
        writer.sync()?;
    }
    let reader = VarveStreamReader::open(spec, &path, small_chunk_options())?;
    assert_eq!(reader.verify_all()?, 20);
    let values = reader.blocks::<Value>()?.collect::<Result<Vec<_>>>()?;
    assert_eq!(values, (0..20).map(Value).collect::<Vec<_>>());
    Ok(())
}

/// The CRC typed scan must read each decoded payload exactly once: the frame
/// scan performs no payload checksum pre-pass, and the one payload read that
/// feeds typed decode is the read that verifies the record checksum.
#[cfg(feature = "integrity")]
#[test]
fn crc_typed_scan_skips_foreign_payloads_and_gates_decode() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("crc-single-read.varve");
    let spec = chain_spec(IntegrityPolicy::Crc32WithHeader);
    {
        let mut writer = VarveStreamWriter::create(spec, &path, small_chunk_options())?;
        for value in 0..3 {
            writer.push_info(&Value(value))?;
        }
        writer.push_info(&Extra(100))?;
        for value in 3..6 {
            writer.push_info(&Value(value))?;
        }
        writer.sync()?;
    }
    let extra_payload_offset = {
        let reader = VarveStreamReader::open(spec, &path, small_chunk_options())?;
        let mut offset = None;
        for event in reader.events()? {
            let event = event?;
            if event.block_id == Extra::ID {
                offset = Some(event.payload_offset);
            }
        }
        offset.expect("extra record present")
    };

    // Corrupt one payload byte of the Extra record in place. Frames stay
    // intact, so only a payload checksum computation can observe the damage.
    {
        use std::io::{Read, Seek, SeekFrom, Write};

        let mut file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)?;
        let mut byte = [0u8; 1];
        file.seek(SeekFrom::Start(extra_payload_offset))?;
        file.read_exact(&mut byte)?;
        byte[0] ^= 0xFF;
        file.seek(SeekFrom::Start(extra_payload_offset))?;
        file.write_all(&byte)?;
        file.sync_all()?;
    }

    let reader = VarveStreamReader::open(spec, &path, small_chunk_options())?;
    // A typed scan of another block must not read (or checksum) the corrupt
    // foreign payload; before the single-read contract this scan failed with
    // a checksum mismatch on the skipped record.
    let values = reader.blocks::<Value>()?.collect::<Result<Vec<_>>>()?;
    assert_eq!(values, (0..6).map(Value).collect::<Vec<_>>());
    // Decoding the corrupt record itself must fail: the one payload read that
    // feeds typed decode is also the checksum verification.
    let extra_error = reader
        .blocks::<Extra>()?
        .collect::<Result<Vec<_>>>()
        .expect_err("decoding the corrupted record must fail");
    assert!(matches!(extra_error, Error::ChecksumMismatch { .. }));
    // Whole-file verification keeps its payload checksum pre-pass.
    let verify_error = reader
        .verify_all()
        .expect_err("verify_all must observe the corrupted payload");
    assert!(matches!(verify_error, Error::ChecksumMismatch { .. }));
    Ok(())
}

#[test]
fn bootstrap_aborts_when_pathname_identity_changes_before_publish() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("swap.varve");
    let decoy = directory.path().join("decoy.varve");
    let moved = directory.path().join("moved.varve");
    let spec = chain_spec(IntegrityPolicy::None);
    for (target, seed) in [(&path, 0u64), (&decoy, 90u64)] {
        let mut writer = VarveStreamWriter::create(spec, target, small_chunk_options())?;
        for value in seed..seed + 3 {
            writer.push_info(&Value(value))?;
        }
        writer.sync()?;
        drop(writer);
        std::fs::remove_file(state_sidecar(target))?;
    }
    let error = bootstrap_stream_checkpoint_with_progress(
        spec,
        &path,
        small_chunk_options(),
        ScanOptions::default(),
        |progress| {
            // The scan is finished but the checkpoint is not yet published:
            // swap another same-spec generation into the scanned pathname.
            if progress.phase == ScanProgressPhase::Complete {
                std::fs::rename(&path, &moved).expect("move scanned generation aside");
                std::fs::rename(&decoy, &path).expect("swap decoy into the pathname");
            }
        },
    )
    .expect_err("bootstrap must not publish a checkpoint for a swapped pathname");
    assert!(matches!(
        &error,
        Error::DiskIndex(source) if matches!(**source, DiskIndexError::IdentityMismatch)
    ));
    assert!(
        !state_sidecar(&path).exists(),
        "no state sidecar may be published after the identity mismatch"
    );
    Ok(())
}

#[cfg(not(feature = "scalable-fault-injection"))]
#[test]
#[ignore = "sidecar commit counting requires the scalable-fault-injection trace hooks"]
fn scalar_appends_bound_sidecar_commits_requires_feature() {}

#[cfg(feature = "scalable-fault-injection")]
mod commit_contract {
    use std::env;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::process::{Command, Stdio};

    use varve::scalable_fault::{FAULT_ENV, TRACE_ENV, arm_from_env};

    use super::*;

    const CHILD_ENV: &str = "VARVE_STREAM_PERF_CONTRACT_CHILD";
    const ROOT_ENV: &str = "VARVE_STREAM_PERF_CONTRACT_ROOT";
    const CHILD_RECORDS: u64 = 10;
    const CHUNK_RECORDS: u64 = 2;

    fn native_path(root: &Path) -> PathBuf {
        root.join("commit-contract.varve")
    }

    /// N scalar pushes with a sidecar bound of K coverage items must produce at
    /// most ceil(N/K)+1 sidecar batch commits, not one commit per record.
    #[test]
    fn scalar_appends_bound_sidecar_commits() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let trace = directory.path().join("trace.tsv");
        let status = Command::new(env::current_exe().expect("locate test executable"))
            .args([
                "--ignored",
                "--exact",
                "commit_contract::commit_contract_child",
            ])
            .env(CHILD_ENV, "1")
            .env(ROOT_ENV, directory.path())
            .env(FAULT_ENV, "trace")
            .env(TRACE_ENV, &trace)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .expect("run commit contract child");
        assert!(status.success(), "commit contract child failed: {status}");

        let mut batch_commit_events = 0u64;
        let mut clean_publish_events = 0u64;
        for line in fs::read_to_string(&trace)
            .expect("read fault trace")
            .lines()
        {
            let mut fields = line.split('\t');
            assert_eq!(fields.next(), Some("v1"), "unexpected trace line {line:?}");
            match fields.next() {
                Some("append.sidecar_batch_commit") => batch_commit_events += 1,
                Some("publish.clean_commit") => clean_publish_events += 1,
                _ => {}
            }
        }
        // Every commit is bracketed by one before-hook and one after-hook.
        assert_eq!(batch_commit_events % 2, 0, "unpaired commit hooks in trace");
        let commits = batch_commit_events / 2;
        let bound = CHILD_RECORDS.div_ceil(CHUNK_RECORDS) + 1;
        assert!(
            (1..=bound).contains(&commits),
            "expected between 1 and {bound} sidecar batch commits for \
             {CHILD_RECORDS} scalar pushes with chunk bound {CHUNK_RECORDS}, got {commits}"
        );
        assert!(
            clean_publish_events >= 2,
            "sync() no longer publishes a durable clean sidecar state"
        );

        // The batched sidecar must still describe a clean, fully readable file.
        let reader = VarveStreamReader::open(
            chain_spec(IntegrityPolicy::None),
            native_path(directory.path()),
            small_chunk_options(),
        )?;
        let values = reader.blocks::<Value>()?.collect::<Result<Vec<_>>>()?;
        assert_eq!(values, (0..CHILD_RECORDS).map(Value).collect::<Vec<_>>());
        Ok(())
    }

    #[test]
    #[ignore = "invoked only as the commit-contract subprocess"]
    fn commit_contract_child() {
        if env::var_os(CHILD_ENV).is_none() {
            return;
        }
        let root = PathBuf::from(env::var_os(ROOT_ENV).expect("child root"));
        let mut writer = VarveStreamWriter::create(
            chain_spec(IntegrityPolicy::None),
            native_path(&root),
            small_chunk_options(),
        )
        .expect("create commit contract writer");
        let guard = arm_from_env().expect("arm scalable fault tracing");
        for value in 0..CHILD_RECORDS {
            writer
                .push_info(&Value(value))
                .expect("append scalar record");
        }
        writer.sync().expect("sync commit contract writer");
        drop(writer);
        drop(guard);
    }
}
