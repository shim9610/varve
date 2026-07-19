//! DUR-02 regression: the authoritative single-writer lock is held on the
//! native file object itself, so hard-link aliases cannot bypass it the way
//! they bypass the path-derived ".lock" marker.

use std::fs;

use varve::{
    BlockDescriptor, BlockKind, Decoder, Encoder, Endian, Error, FormatSpec, IndexPolicy,
    IntegrityPolicy, LayoutFieldValue, LayoutValue, ManifestPolicy, ReadLimits, RecoveryPolicy,
    Result, SegmentWrite, VarveBlock, VarveDecode, VarveEncode, VarveFile, WireType, varve_format,
};

#[cfg(feature = "high-cardinality-dev")]
use varve::{CommitPolicy, StreamOptions, VarveStreamReader, VarveStreamWriter};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct LockRecord(u64);

impl VarveEncode for LockRecord {
    const WIRE_TYPE: WireType = WireType::U64;

    fn encode_varve(&self, encoder: &mut Encoder) -> Result<()> {
        self.0.encode_varve(encoder)
    }
}

impl VarveDecode for LockRecord {
    const WIRE_TYPE: WireType = WireType::U64;

    fn decode_varve(decoder: &mut Decoder<'_>) -> Result<Self> {
        Ok(Self(u64::decode_varve(decoder)?))
    }
}

impl VarveBlock for LockRecord {
    const ID: u32 = 61;
    const VERSION: u16 = 1;
    const KIND: BlockKind = BlockKind::Fixed;
    const ENDIAN: Option<Endian> = None;
    const IS_KEYED: bool = false;
    const SCHEMA_FINGERPRINT: u64 = 0x4C4F_434B_0000_003D;
}

static BLOCKS: &[BlockDescriptor] = &[BlockDescriptor {
    id: LockRecord::ID,
    name: "SingleWriterLockRecord",
    version: LockRecord::VERSION,
    kind: LockRecord::KIND,
    fields: &[],
}];

fn spec() -> FormatSpec {
    FormatSpec::new(
        b"VSWLK",
        1,
        Endian::Little,
        0,
        IndexPolicy::ScanOnOpen,
        IntegrityPolicy::None,
        RecoveryPolicy::Strict,
        ManifestPolicy::None,
        BLOCKS,
    )
    .with_read_limits(ReadLimits::STANDARD)
}

#[test]
fn hard_link_alias_cannot_acquire_second_writer() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let original = directory.path().join("original.varve");
    let alias = directory.path().join("alias.varve");
    {
        let mut writer = VarveFile::create(spec(), &original)?;
        writer.push(&LockRecord(1))?;
        writer.sync()?;
    }
    fs::hard_link(&original, &alias)?;

    let writer = VarveFile::open(spec(), &original)?;

    // The alias resolves to the same file object, so the second writer must
    // be refused even though its path-derived ".lock" marker differs.
    match VarveFile::open(spec(), &alias) {
        Err(Error::WriterLockHeld(_)) => {}
        other => panic!("alias writer was not refused: {other:?}"),
    }

    // Readers never take the single-writer lock and stay unaffected while the
    // writer holds it, through either pathname.
    let reader = VarveFile::open_readonly(spec(), &original)?;
    assert_eq!(reader.blocks::<LockRecord>()?.get(0)?, Some(LockRecord(1)));
    let alias_reader = VarveFile::open_readonly(spec(), &alias)?;
    assert_eq!(
        alias_reader.blocks::<LockRecord>()?.get(0)?,
        Some(LockRecord(1))
    );
    drop(reader);
    drop(alias_reader);

    drop(writer);

    // Closing the writer's handles releases the object lock, after which the
    // alias may acquire the writer role and its appends stay reachable from
    // the original pathname.
    let mut alias_writer = VarveFile::open(spec(), &alias)?;
    alias_writer.push(&LockRecord(2))?;
    alias_writer.sync()?;
    drop(alias_writer);

    let reopened = VarveFile::open_readonly(spec(), &original)?;
    let blocks = reopened.blocks::<LockRecord>()?;
    assert_eq!(blocks.get(0)?, Some(LockRecord(1)));
    assert_eq!(blocks.get(1)?, Some(LockRecord(2)));
    Ok(())
}

#[test]
fn hard_link_alias_create_is_refused_before_truncation() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let original = directory.path().join("create-original.varve");
    let alias = directory.path().join("create-alias.varve");
    {
        let mut writer = VarveFile::create(spec(), &original)?;
        writer.push(&LockRecord(7))?;
        writer.sync()?;
    }
    fs::hard_link(&original, &alias)?;

    let writer = VarveFile::open(spec(), &original)?;

    // Creating over the alias must fail on the object lock before the
    // create-mode truncation can destroy the live writer's data.
    match VarveFile::create(spec(), &alias) {
        Err(Error::WriterLockHeld(_)) => {}
        other => panic!("alias create was not refused: {other:?}"),
    }
    drop(writer);

    let reopened = VarveFile::open_readonly(spec(), &original)?;
    assert_eq!(
        reopened.blocks::<LockRecord>()?.get(0)?,
        Some(LockRecord(7))
    );
    Ok(())
}

#[test]
fn same_path_second_writer_is_still_refused() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("same-path.varve");
    let writer = VarveFile::create(spec(), &path)?;

    match VarveFile::open(spec(), &path) {
        Err(Error::WriterLockHeld(_)) => {}
        other => panic!("second same-path writer was not refused: {other:?}"),
    }
    drop(writer);

    let _reopened = VarveFile::open(spec(), &path)?;
    Ok(())
}

#[test]
fn create_new_refuses_existing_path() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("exclusive.varve");
    {
        let mut writer = VarveFile::create(spec(), &path)?;
        writer.push(&LockRecord(9))?;
        writer.sync()?;
    }

    match VarveFile::create_new(spec(), &path) {
        Err(Error::Io(error)) => {
            assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists);
        }
        other => panic!("create_new did not refuse an existing file: {other:?}"),
    }

    // The refused exclusive create must leave the existing file untouched.
    let reopened = VarveFile::open_readonly(spec(), &path)?;
    assert_eq!(
        reopened.blocks::<LockRecord>()?.get(0)?,
        Some(LockRecord(9))
    );

    let fresh = directory.path().join("exclusive-fresh.varve");
    let mut writer = VarveFile::create_new(spec(), &fresh)?;
    writer.push(&LockRecord(10))?;
    writer.sync()?;
    Ok(())
}

/// Streaming spec: `BlockOffsetChain` never checkpoints on flush and uses a
/// record footer, both of which the scalable stream writer accepts.
#[cfg(feature = "high-cardinality-dev")]
fn stream_spec() -> FormatSpec {
    FormatSpec::new(
        b"VSWSK",
        1,
        Endian::Little,
        0,
        IndexPolicy::BlockOffsetChain,
        IntegrityPolicy::None,
        RecoveryPolicy::Strict,
        ManifestPolicy::None,
        BLOCKS,
    )
    .with_commit_policy(CommitPolicy::RecordFooter)
    .with_read_limits(ReadLimits::STANDARD)
}

/// Path of the stream writer's state sidecar (`.vks`), so the alias can be a
/// faithful second name for both the native file and its sidecar.
#[cfg(feature = "high-cardinality-dev")]
fn stream_sidecar(path: &std::path::Path) -> std::path::PathBuf {
    let mut value = path.as_os_str().to_owned();
    value.push(".vks");
    std::path::PathBuf::from(value)
}

/// DUR-02 on the scalable path the review specifically called out: a freshly
/// created stream writer must bind the object lock onto the file it just
/// created, or a hard-link alias opened afterwards wins a second writer role.
#[cfg(feature = "high-cardinality-dev")]
#[test]
fn stream_create_binds_object_lock_against_hard_link_alias() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let original = directory.path().join("stream-original.varve");
    let alias = directory.path().join("stream-alias.varve");

    let mut writer = VarveStreamWriter::create(stream_spec(), &original, StreamOptions::default())?;
    writer.push_info(&LockRecord(1))?;
    writer.sync()?;

    // Alias both the native file and its state sidecar so the only thing that
    // can refuse a second writer is the object lock itself, not a missing
    // sidecar. Without the create-path native bind, this alias open would
    // succeed and admit a second live writer on the same file object.
    fs::hard_link(&original, &alias)?;
    fs::hard_link(stream_sidecar(&original), stream_sidecar(&alias))?;
    match VarveStreamWriter::open(stream_spec(), &alias, StreamOptions::default()) {
        Err(Error::WriterLockHeld(_)) => {}
        Ok(_) => panic!("stream alias writer was admitted as a second live writer"),
        Err(other) => panic!("stream alias writer failed with the wrong error: {other:?}"),
    }

    // Closing the live writer releases the object lock, after which the alias
    // may take the writer role and its appends stay reachable from the original
    // pathname.
    drop(writer);

    let mut alias_writer =
        VarveStreamWriter::open(stream_spec(), &alias, StreamOptions::default())?;
    alias_writer.push_info(&LockRecord(2))?;
    alias_writer.sync()?;
    drop(alias_writer);

    let reader = VarveStreamReader::open(stream_spec(), &original, StreamOptions::default())?;
    let values = reader.blocks::<LockRecord>()?.collect::<Result<Vec<_>>>()?;
    assert_eq!(values, vec![LockRecord(1), LockRecord(2)]);
    Ok(())
}

/// DUR2-05 on the scalable stream create path: creating over a hard-link
/// alias of a live writer's file must be refused by the object lock without
/// ever truncating the live file object. Before the fix,
/// `VarveStreamWriter::create_unmanaged` opened with `.truncate(true)` ahead
/// of `bind_native`, so a losing creator could clear the winner's freshly
/// initialized object before its own bind failed.
#[cfg(feature = "high-cardinality-dev")]
#[test]
fn stream_create_over_live_alias_is_refused_before_truncation() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let original = directory.path().join("stream-create-original.varve");
    let alias = directory.path().join("stream-create-alias.varve");

    let mut writer = VarveStreamWriter::create(stream_spec(), &original, StreamOptions::default())?;
    writer.push_info(&LockRecord(41))?;
    writer.sync()?;
    fs::hard_link(&original, &alias)?;

    match VarveStreamWriter::create(stream_spec(), &alias, StreamOptions::default()) {
        Err(Error::WriterLockHeld(_)) => {}
        Ok(_) => panic!("stream create over a live alias was admitted as a second writer"),
        Err(other) => {
            panic!("stream create over a live alias failed with the wrong error: {other:?}")
        }
    }

    // The live writer's file object must be untouched: it can keep appending,
    // and everything stays readable through the original pathname.
    writer.push_info(&LockRecord(42))?;
    writer.sync()?;
    drop(writer);

    let reader = VarveStreamReader::open(stream_spec(), &original, StreamOptions::default())?;
    let values = reader.blocks::<LockRecord>()?.collect::<Result<Vec<_>>>()?;
    assert_eq!(values, vec![LockRecord(41), LockRecord(42)]);
    Ok(())
}

/// DUR2-05 companion for the stream create path: with the truncating open
/// replaced by an explicit `set_len(0)` after the lock bind, creating over a
/// stale unlocked file must still clear every stale byte. The recreated file
/// must be byte-length identical to a control file created fresh.
#[cfg(feature = "high-cardinality-dev")]
#[test]
fn stream_create_over_stale_file_truncates_through_bound_handle() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let stale = directory.path().join("stream-stale.varve");
    let control = directory.path().join("stream-control.varve");
    fs::write(&stale, vec![0xAB; 4096])?;

    for path in [&stale, &control] {
        let mut writer = VarveStreamWriter::create(stream_spec(), path, StreamOptions::default())?;
        writer.push_info(&LockRecord(5))?;
        writer.sync()?;
    }

    assert_eq!(
        fs::metadata(&stale)?.len(),
        fs::metadata(&control)?.len(),
        "stale bytes survived the create-path truncation"
    );
    let reader = VarveStreamReader::open(stream_spec(), &stale, StreamOptions::default())?;
    let values = reader.blocks::<LockRecord>()?.collect::<Result<Vec<_>>>()?;
    assert_eq!(values, vec![LockRecord(5)]);
    Ok(())
}

varve_format! {
    pub format LockLayoutFormat {
        magic: b"VSWLY";
        version: 1;
        limits {
            file_len: 8_589_934_592;
            records: 4_000_000;
            index_bytes: 536_870_912;
            scan_bytes: 8_589_934_592;
            record_payload: 67_108_864;
            logical_payload: 268_435_456;
            materialized_bytes: 1_073_741_824;
            segments: 4_000_000;
            matrix_dimension: 16_000_000;
            matrix_cells: 16_000_000;
            matrix_bitmap: 64_000_000;
            matrix_crc: 128_000_000;
            matrix_metadata: 268_435_456;
            matrix_slot_region: 8_589_934_592;
            sidecar: 268_435_456;
            mmap: 8_589_934_592;
        }
        endian: little;
        schema_hash: computed;
        extension: "swly";
        preset: none;

        layout {
            file_header LockFileHeader {
                bytes signature = b"LOCK";
                u16 header_version = 1;
            }

            segment LockSegment repeat until_eof {
                lead_in LockLeadIn {
                    bytes tag = b"SEGM";
                    u32 kind;
                    i64 next_segment_offset = finalize(target = segment_end, relative_to = after_lead_in);
                    i64 raw_data_offset = finalize(target = raw_region_start, relative_to = after_lead_in);
                }

                metadata LockMetadata;
                raw_region LockRaw;
            }
        }
    }
}

fn write_lock_layout_segment(writer: &mut LockLayoutFormatLayoutWriter, kind: u32) -> Result<()> {
    writer.write_segment(SegmentWrite {
        name: "LockSegment",
        fields: &[LayoutFieldValue {
            name: "kind",
            value: LayoutValue::U32(kind),
        }],
        footer_fields: &[],
        metadata: b"meta",
        raw: b"raw!",
    })?;
    writer.flush()
}

/// DUR2-05 on the layout create path: creating over a hard-link alias of a
/// live layout writer's file must be refused by the object lock without ever
/// truncating the live file object. Before the fix,
/// `VarveLayoutWriter::create_inner` opened with `.truncate(true)` ahead of
/// `bind_native`, so a losing creator could clear the winner's freshly
/// initialized object before its own bind failed.
#[test]
fn layout_create_over_live_alias_is_refused_before_truncation() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let original = directory.path().join("layout-create-original.swly");
    let alias = directory.path().join("layout-create-alias.swly");

    let mut writer = LockLayoutFormat::create_layout_writer(&original)?;
    write_lock_layout_segment(&mut writer, 7)?;
    fs::hard_link(&original, &alias)?;

    match LockLayoutFormat::create_layout_writer(&alias) {
        Err(Error::WriterLockHeld(_)) => {}
        Ok(_) => panic!("layout create over a live alias was admitted as a second writer"),
        Err(other) => {
            panic!("layout create over a live alias failed with the wrong error: {other:?}")
        }
    }

    // The live writer's file object must be untouched: it can keep appending,
    // and everything stays readable through the original pathname.
    write_lock_layout_segment(&mut writer, 8)?;
    drop(writer);

    let reader = LockLayoutFormat::open_layout_reader(&original)?;
    assert_eq!(reader.segments().len(), 2);
    assert_eq!(
        reader.segments()[0].field("kind"),
        Some(&LayoutValue::U32(7))
    );
    assert_eq!(
        reader.segments()[1].field("kind"),
        Some(&LayoutValue::U32(8))
    );
    Ok(())
}

/// DUR2-05 companion for the layout create path: with the truncating open
/// replaced by an explicit `set_len(0)` after the lock bind, creating over a
/// stale unlocked file must still clear every stale byte. The recreated file
/// must be byte-identical to a control file created fresh.
#[test]
fn layout_create_over_stale_file_truncates_through_bound_handle() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let stale = directory.path().join("layout-stale.swly");
    let control = directory.path().join("layout-control.swly");
    fs::write(&stale, vec![0xCD; 4096])?;

    for path in [&stale, &control] {
        let mut writer = LockLayoutFormat::create_layout_writer(path)?;
        write_lock_layout_segment(&mut writer, 9)?;
    }

    assert_eq!(
        fs::read(&stale)?,
        fs::read(&control)?,
        "stale bytes survived the create-path truncation"
    );
    let reader = LockLayoutFormat::open_layout_reader(&stale)?;
    assert_eq!(reader.segments().len(), 1);
    assert_eq!(
        reader.segments()[0].field("kind"),
        Some(&LayoutValue::U32(9))
    );
    Ok(())
}
