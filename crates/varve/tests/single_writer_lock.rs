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

// ---------------------------------------------------------------------------
// A *failed* open must give back everything it took.
//
// `WriterLock::acquire` takes two things before an open can do any work: the
// authoritative object lock on the native file (`probe_native_target_lock` and
// `WriterLock::bind_native`), and the diagnostic `<target>.lock` marker, whose
// non-empty content is itself a refusal under `WriterLockBreakPolicy::Refuse`.
// A live writer keeps both on purpose. An open that *fails* keeps neither: it
// never became a writer, so the next open of any kind, including a recovering
// one, must see a file nobody holds.
//
// The leak these cover was found by the first Linux CI run, in
// `strict_open_rejects_and_recover_truncates_partial_tail`: a strict open
// rejected a corrupt tail, and the recovering open that followed was refused
// with `WriterLockHeld`. The shape is platform independent - a failed open
// followed by another open - so these assert it directly rather than through a
// recovery scenario, and they cover the sibling refusal paths too, because
// whatever leaks on one of them leaks on all of them.

fn lock_marker(path: &std::path::Path) -> std::path::PathBuf {
    let mut value = path.as_os_str().to_owned();
    value.push(".lock");
    std::path::PathBuf::from(value)
}

/// Asserts that a failed open left no writer claim behind at `path`.
///
/// Two independent witnesses, one per thing an acquisition takes:
///
/// * the marker describes no writer. A zero-length marker file may remain - the
///   object is deliberately not deleted - but `inspect_writer_lock` must report
///   `None`, because a marker that still parses as writer metadata is exactly
///   what `acquire_with_policy` refuses under `Refuse`.
/// * the object lock is free, which `probe` proves: it performs an operation
///   that acquires the writer lock, and must then fail or succeed on its own
///   terms, never with a lock refusal.
fn assert_no_writer_claim_left<T>(
    path: &std::path::Path,
    what: &str,
    probe: impl FnOnce() -> Result<T>,
) {
    match VarveFile::inspect_writer_lock(path) {
        Ok(None) => {}
        Ok(Some(info)) => panic!(
            "{what}: the failed open left live writer metadata in {}: {info:?}",
            lock_marker(path).display()
        ),
        Err(error) => panic!("{what}: the failed open left an unreadable lock marker: {error:?}"),
    }
    // The same statement in the terms `acquire_with_policy` actually decides on:
    // it reads the marker's *length* and refuses any non-zero one under
    // `Refuse` without ever parsing the content. Absent is fine; empty is fine;
    // anything else is a refusal waiting to happen.
    let marker = lock_marker(path);
    match fs::metadata(&marker) {
        Ok(metadata) => assert_eq!(
            metadata.len(),
            0,
            "{what}: the failed open left {} bytes in {}, which the next \
             acquisition refuses without reading them",
            metadata.len(),
            marker.display()
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => panic!("{what}: could not inspect {}: {error:?}", marker.display()),
    }
    match probe() {
        Err(Error::WriterLockHeld(held)) => panic!(
            "{what}: the failed open kept the writer lock on {held}, so the next open was refused"
        ),
        Err(Error::WriterLockBreakRefused(held)) => panic!(
            "{what}: the failed open kept a writer claim on {held}, so the next open was refused"
        ),
        _ => {}
    }
}

/// The CI failure itself, reduced to its shape: a strict open rejects a corrupt
/// tail, and the recovering open that follows must be able to take the writer
/// role. Before the fix this passed on Windows and failed on Linux with
/// `WriterLockHeld`, because the two platforms release an object lock on
/// different events.
#[test]
fn a_failed_open_on_a_corrupt_tail_releases_the_writer_lock() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("failed-open-corrupt-tail.varve");
    {
        let mut writer = VarveFile::create(spec(), &path)?;
        writer.push(&LockRecord(1))?;
        writer.push(&LockRecord(2))?;
        writer.sync()?;
    }
    let intact_len = fs::metadata(&path)?.len();
    // A record header that stops short: enough to be seen, too little to be a
    // record.
    append_bytes(&path, &[1, 2, 3, 4])?;

    match VarveFile::open(spec(), &path) {
        Err(Error::CorruptTail { .. }) => {}
        other => panic!("strict open did not reject the partial tail: {other:?}"),
    }

    // The same failure twice: the second attempt must fail on the tail again,
    // not on a lock the first attempt kept.
    assert_no_writer_claim_left(&path, "corrupt tail", || VarveFile::open(spec(), &path));

    // And the recovering open must be able to take the writer role, which is
    // the exact step the Linux run failed on.
    let recovering = spec().with_recovery_policy(RecoveryPolicy::TruncateTail);
    let (recovered, report) = VarveFile::open_recover_with_report(recovering, &path)?;
    assert_eq!(report.recovered_len, intact_len);
    assert_eq!(recovered.blocks::<LockRecord>()?.len(), 2);
    Ok(())
}

/// Sibling refusal path: the header is rejected before any record is scanned.
/// The lock is already held at that point, so this leaks in exactly the same
/// way if the release depends on anything but the failure itself.
#[test]
fn a_failed_open_on_an_invalid_header_releases_the_writer_lock() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("failed-open-bad-magic.varve");
    {
        let mut writer = VarveFile::create(spec(), &path)?;
        writer.push(&LockRecord(3))?;
        writer.sync()?;
    }
    let original = fs::read(&path)?;

    // Corrupt the magic in place: same length, everything else untouched.
    let mut damaged = original.clone();
    damaged[0] ^= 0xFF;
    fs::write(&path, &damaged)?;

    match VarveFile::open(spec(), &path) {
        Err(Error::InvalidMagic) => {}
        other => panic!("open did not reject the damaged magic: {other:?}"),
    }
    assert_no_writer_claim_left(&path, "invalid magic", || VarveFile::open(spec(), &path));

    // Restoring the header must be enough to open the file as a writer again:
    // nothing about the refusal may outlive it.
    fs::write(&path, &original)?;
    let reopened = VarveFile::open(spec(), &path)?;
    assert_eq!(
        reopened.blocks::<LockRecord>()?.get(0)?,
        Some(LockRecord(3))
    );
    Ok(())
}

/// Sibling refusal path: a format-version mismatch, decided from the header of
/// a file that is otherwise completely intact. The proof that nothing was kept
/// is that the *matching* spec can then open it.
#[test]
fn a_failed_open_on_a_version_mismatch_releases_the_writer_lock() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("failed-open-version.varve");
    {
        let mut writer = VarveFile::create(spec(), &path)?;
        writer.push(&LockRecord(4))?;
        writer.sync()?;
    }

    let future = FormatSpec::new(
        b"VSWLK",
        2,
        Endian::Little,
        0,
        IndexPolicy::ScanOnOpen,
        IntegrityPolicy::None,
        RecoveryPolicy::Strict,
        ManifestPolicy::None,
        BLOCKS,
    )
    .with_read_limits(ReadLimits::STANDARD);

    match VarveFile::open(future, &path) {
        Err(Error::FormatVersionMismatch { .. }) => {}
        other => panic!("open did not reject the version mismatch: {other:?}"),
    }
    assert_no_writer_claim_left(&path, "version mismatch", || VarveFile::open(future, &path));

    let reopened = VarveFile::open(spec(), &path)?;
    assert_eq!(
        reopened.blocks::<LockRecord>()?.get(0)?,
        Some(LockRecord(4))
    );
    Ok(())
}

/// Sibling refusal path: a refused read limit. This one fails at
/// `check_open_file_len`, the first thing after the lock is bound, so it is the
/// narrowest window in which a leak can happen at all.
#[test]
fn a_failed_open_on_a_refused_limit_releases_the_writer_lock() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("failed-open-limit.varve");
    {
        let mut writer = VarveFile::create(spec(), &path)?;
        writer.push(&LockRecord(5))?;
        writer.sync()?;
    }

    let refusing = spec().with_read_limits(ReadLimits::finite_all(1));
    match VarveFile::open(refusing, &path) {
        Err(Error::LimitExceeded { .. }) => {}
        other => panic!("open did not refuse the file under a one-byte limit: {other:?}"),
    }
    assert_no_writer_claim_left(&path, "refused limit", || VarveFile::open(refusing, &path));

    let reopened = VarveFile::open(spec(), &path)?;
    assert_eq!(
        reopened.blocks::<LockRecord>()?.get(0)?,
        Some(LockRecord(5))
    );
    Ok(())
}

/// Sibling refusal path: a schema-hash mismatch. Same header, same version,
/// same records - only the caller's declared schema differs, and the refusal
/// still happens with the claim already taken.
#[test]
fn a_failed_open_on_a_schema_hash_mismatch_releases_the_writer_lock() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("failed-open-schema.varve");
    {
        let mut writer = VarveFile::create(spec(), &path)?;
        writer.push(&LockRecord(9))?;
        writer.sync()?;
    }

    let other_schema = FormatSpec::new(
        b"VSWLK",
        1,
        Endian::Little,
        0x0BAD_5C4E_0000_0001,
        IndexPolicy::ScanOnOpen,
        IntegrityPolicy::None,
        RecoveryPolicy::Strict,
        ManifestPolicy::None,
        BLOCKS,
    )
    .with_read_limits(ReadLimits::STANDARD);

    match VarveFile::open(other_schema, &path) {
        Err(Error::SchemaHashMismatch { .. }) => {}
        other => panic!("open did not reject the schema-hash mismatch: {other:?}"),
    }
    assert_no_writer_claim_left(&path, "schema-hash mismatch", || {
        VarveFile::open(other_schema, &path)
    });

    let reopened = VarveFile::open(spec(), &path)?;
    assert_eq!(
        reopened.blocks::<LockRecord>()?.get(0)?,
        Some(LockRecord(9))
    );
    Ok(())
}

/// The release must not depend on a handle *closing*, because closing is not
/// when a lock is given back.
///
/// On Unix an advisory lock belongs to the open file description, and any
/// `fork` duplicates every descriptor - so every description - into the child.
/// `std::process::Command::spawn` is a `fork`, which makes this the ordinary
/// case rather than an exotic one: an application that spawns *any* child
/// process while a varve writer is open has handed a duplicate of that writer's
/// lock descriptions to it. The duplicates are `O_CLOEXEC` and close at `exec`,
/// but until then the lock stays held on the child's copies, and a writer that
/// released by closing its own descriptor has released nothing yet. The next
/// open is refused with `WriterLockHeld` for a claim no live writer holds, and
/// `flock` reports `EAGAIN` with no owner visible anywhere in this process.
///
/// The window is fork-to-`exec`, and it has to be entered from another thread:
/// `Command::spawn` does not return to its *own* caller until the child has
/// `exec`ed, so the thread that spawns can never observe it. Every thread
/// beside it can, which is exactly how this reached CI - one test spawning a
/// child while a different test, on a different test-harness thread, released a
/// writer and reopened it.
///
/// So: a spawner thread holds the window open with a `pre_exec` sleep, and this
/// thread does the release and the reopen inside it. The sleep only makes the
/// timing reliable; the bug needs a child spawned at the wrong microsecond and
/// nothing more. Unix only, because `fork` is the whole mechanism - Windows
/// creates processes without duplicating handles that are not marked
/// inheritable.
#[cfg(unix)]
#[test]
fn a_release_beside_a_forked_child_does_not_strand_the_writer_claim() -> Result<()> {
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};
    use std::sync::{Arc, Barrier};
    use std::time::Duration;

    const CHILD_HOLDS_THE_WINDOW: Duration = Duration::from_millis(400);

    let directory = tempfile::tempdir()?;
    let path = directory.path().join("forked-child.varve");
    {
        let mut writer = VarveFile::create(spec(), &path)?;
        writer.push(&LockRecord(10))?;
        writer.sync()?;
    }

    // Open the writer first: the fork must duplicate descriptors that the claim
    // is held on, which is only true while a writer is live.
    let writer = VarveFile::open(spec(), &path)?;

    let executable = std::env::current_exe()?;
    let at_the_fork = Arc::new(Barrier::new(2));
    let spawner = {
        let at_the_fork = Arc::clone(&at_the_fork);
        std::thread::spawn(move || -> std::io::Result<()> {
            // `--list` makes the child a listing run that exits on its own.
            let mut command = Command::new(executable);
            command
                .arg("--list")
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            // SAFETY: this runs in the forked child before `exec`, where only
            // async-signal-safe work is permitted. A sleep is `nanosleep` and
            // nothing else: no allocation, no locks, no inherited state read or
            // written.
            unsafe {
                command.pre_exec(|| {
                    std::thread::sleep(CHILD_HOLDS_THE_WINDOW);
                    Ok(())
                });
            }
            at_the_fork.wait();
            // Forks within microseconds of here, then blocks until the child
            // `exec`s - which is why the release below cannot live on this
            // thread.
            command.spawn()?.wait()?;
            Ok(())
        })
    };

    at_the_fork.wait();
    // Comfortably inside the window: the fork is microseconds away, the child
    // then holds the duplicates for 400ms.
    std::thread::sleep(Duration::from_millis(80));

    // The release has to be complete when it returns, not when some other
    // process happens to `exec`.
    drop(writer);
    let reopened = VarveFile::open(spec(), &path);
    let refusal = match reopened {
        Ok(file) => {
            assert_eq!(file.blocks::<LockRecord>()?.get(0)?, Some(LockRecord(10)));
            None
        }
        Err(error) => Some(error),
    };
    spawner.join().expect("spawner thread")?;

    match refusal {
        None => Ok(()),
        Some(Error::WriterLockHeld(held)) => panic!(
            "releasing a writer beside a forked child stranded the claim on {held}: \
             the release waited on a descriptor closing in another process"
        ),
        Some(other) => Err(other),
    }
}

/// Windows-only forensic companion to the four tests above.
///
/// They prove that no *lock* survives a failed open. This proves the stronger
/// property they rest on - that no *handle* to the target file survives one -
/// by reopening with `share_mode(0)`, which the OS refuses while any other
/// handle to the object is open anywhere in the process. Unix has no equivalent
/// (an fd cannot exclude other fds), but the handle lifecycle is written in
/// platform-independent Rust, so a leak observable here is a leak on Linux too,
/// where it additionally strands the advisory lock that fd holds.
#[cfg(windows)]
#[test]
fn a_failed_open_leaves_no_handle_to_the_target() -> Result<()> {
    use std::os::windows::fs::OpenOptionsExt;

    let directory = tempfile::tempdir()?;
    let path = directory.path().join("failed-open-handles.varve");
    {
        let mut writer = VarveFile::create(spec(), &path)?;
        writer.push(&LockRecord(6))?;
        writer.sync()?;
    }
    append_bytes(&path, &[1, 2, 3, 4])?;

    assert!(VarveFile::open(spec(), &path).is_err());

    fs::OpenOptions::new()
        .read(true)
        .write(true)
        .share_mode(0)
        .open(&path)
        .expect("a handle to the target survived the failed open");
    Ok(())
}

fn append_bytes(path: &std::path::Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write;

    let mut file = fs::OpenOptions::new().append(true).open(path)?;
    file.write_all(bytes)?;
    Ok(())
}
