#![cfg(feature = "high-cardinality-dev")]

//! Scalable primary/sidecar identity contracts (STO-01, API-03).
//!
//! A stream or indexed sidecar must belong to the exact logical generation of
//! the primary it was published against. Schema hash, OS object identity and
//! the deterministic file header are all preserved by an in-place rewrite of a
//! primary with an equal-length primary of the same format, so those alone
//! cannot separate two generations of one file object; the sidecar must refuse
//! such a primary at open and force a rebuild.
//!
//! A disk-index descriptor must likewise carry the block schema fingerprint of
//! the concrete type whose decoder it captured, and that identity must be
//! validated at plan construction — before any captured codec can be handed
//! primary bytes.

use std::fs;
use std::path::Path;

use varve::{
    BlockDescriptor, BlockKind, Decoder, DiskIndexOptions, DiskIndexPlan, DiskIndexedBlock,
    Encoder, Endian, Error, FormatSpec, IndexPolicy, IntegrityPolicy, ManifestPolicy, ReadLimits,
    RecoveryPolicy, Result, VarveBlock, VarveDecode, VarveEncode, VarveIndexedReader,
    VarveIndexedWriter, VarveKeyedBlock, VarveStreamWriter, WireType, rebuild_disk_index,
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
    const ID: u32 = 71;
    const VERSION: u16 = 1;
    const KIND: BlockKind = BlockKind::Variable;
    const ENDIAN: Option<Endian> = None;
    const SCHEMA_FINGERPRINT: u64 = 0x4944_454E_5449_5459;
    const IS_KEYED: bool = true;
}

impl VarveKeyedBlock for Item {
    type Key = u64;

    fn key(&self) -> Self::Key {
        self.key
    }
}

/// Same block id and version as [`Item`], but a different schema fingerprint:
/// the impostor a descriptor could previously carry into a format that never
/// declared it.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Impostor {
    key: u64,
    value: String,
}

impl VarveEncode for Impostor {
    const WIRE_TYPE: WireType = <(u64, String) as VarveEncode>::WIRE_TYPE;

    fn encode_varve(&self, encoder: &mut Encoder) -> Result<()> {
        (self.key, self.value.clone()).encode_varve(encoder)
    }
}

impl VarveDecode for Impostor {
    const WIRE_TYPE: WireType = <(u64, String) as VarveDecode>::WIRE_TYPE;

    fn decode_varve(decoder: &mut Decoder<'_>) -> Result<Self> {
        // Reaching this proves a captured codec was handed file bytes before
        // the descriptor's schema identity was validated.
        let _ = decoder;
        panic!("impostor decoder must never run")
    }
}

impl VarveBlock for Impostor {
    const ID: u32 = Item::ID;
    const VERSION: u16 = Item::VERSION;
    const KIND: BlockKind = BlockKind::Variable;
    const ENDIAN: Option<Endian> = None;
    const SCHEMA_FINGERPRINT: u64 = 0x494D_504F_5354_4F52;
    const IS_KEYED: bool = true;
}

impl VarveKeyedBlock for Impostor {
    type Key = u64;

    fn key(&self) -> Self::Key {
        self.key
    }
}

static BLOCKS: &[BlockDescriptor] = &[BlockDescriptor {
    id: Item::ID,
    name: "IdentityItem",
    version: Item::VERSION,
    kind: Item::KIND,
    fields: &[],
}];

static INDEXED: &[DiskIndexedBlock] = &[DiskIndexedBlock::of::<Item>()];
static INDEXED_IMPOSTOR: &[DiskIndexedBlock] = &[DiskIndexedBlock::of::<Impostor>()];

fn spec() -> FormatSpec {
    FormatSpec::new(
        b"VIDN",
        1,
        Endian::Little,
        0,
        IndexPolicy::KeyedOffsetChain,
        IntegrityPolicy::None,
        RecoveryPolicy::Strict,
        ManifestPolicy::None,
        BLOCKS,
    )
    .with_read_limits(ReadLimits::STANDARD)
    .with_block_identities(IDENTITIES)
}

static IDENTITIES: &[(u32, Option<Endian>, bool, u64)] = &[(
    Item::ID,
    Item::ENDIAN,
    Item::IS_KEYED,
    Item::SCHEMA_FINGERPRINT,
)];

/// Stream state sidecars index no keys, so the stream probe uses a spec whose
/// index policy the streaming writer supports for this block.
fn stream_spec() -> FormatSpec {
    FormatSpec::new(
        b"VIDS",
        1,
        Endian::Little,
        0,
        IndexPolicy::BlockOffsetChain,
        IntegrityPolicy::None,
        RecoveryPolicy::Strict,
        ManifestPolicy::None,
        BLOCKS,
    )
    .with_read_limits(ReadLimits::STANDARD)
    .with_block_identities(IDENTITIES)
}

fn plan() -> DiskIndexPlan {
    DiskIndexPlan::canonical(spec(), INDEXED).unwrap()
}

fn item(key: u64, value: &str) -> Item {
    Item {
        key,
        value: value.into(),
    }
}

/// Builds an indexed primary with two records whose payload lengths are equal
/// for every key, so two primaries built with different keys have byte-equal
/// lengths.
fn build_primary(path: &Path, keys: [u64; 2]) -> Result<()> {
    build_primary_with_filler(path, 0, keys)
}

/// Same as [`build_primary`], but writes `filler` leading records that are
/// byte-identical across every call first.
///
/// With enough filler the two primaries agree on far more than the bounded
/// primary-generation window, so a witness computed over a leading prefix
/// cannot separate them; only a per-create nonce can.
fn build_primary_with_filler(path: &Path, filler: u64, keys: [u64; 2]) -> Result<()> {
    let options = DiskIndexOptions::default();
    let mut writer = VarveIndexedWriter::create(spec(), path, options, plan())?;
    for index in 0..filler {
        writer.push_info(&item(1_000_000 + index, "fixed-width-value"))?;
    }
    for key in keys {
        writer.push_info(&item(key, "fixed-width-value"))?;
    }
    writer.sync()?;
    drop(writer);
    Ok(())
}

/// Records enough leading filler that the two primaries share far more than
/// [`PRIMARY_GENERATION_WINDOW`](varve) bytes (4096) of common content.
const WINDOW_DEFEATING_FILLER: u64 = 400;

fn rewrite_in_place(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::{Seek, SeekFrom, Write};

    let mut file = fs::OpenOptions::new().write(true).open(path)?;
    file.seek(SeekFrom::Start(0))?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}

#[test]
fn same_object_equal_length_rewrite_is_refused_and_rebuild_recovers() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let first = directory.path().join("first.varve");
    let second = directory.path().join("second.varve");
    let options = DiskIndexOptions::default();

    build_primary(&first, [10, 11])?;
    build_primary(&second, [20, 21])?;

    // Normal create/reopen still works before anything is tampered with.
    {
        let reader = VarveIndexedReader::open(spec(), &first, options, plan())?;
        assert_eq!(
            reader.get::<Item>(&10)?,
            Some(item(10, "fixed-width-value"))
        );
        assert_eq!(reader.historical_distinct_keys()?, 2);
    }

    let first_bytes = fs::read(&first)?;
    let second_bytes = fs::read(&second)?;
    assert_eq!(
        first_bytes.len(),
        second_bytes.len(),
        "probe requires equal-length primaries"
    );
    assert_ne!(first_bytes, second_bytes);

    // Rewrite the first primary in place with the second's bytes. The OS file
    // object, the file header and the schema hash are all unchanged; only the
    // logical generation of the contents differs.
    {
        use std::io::{Seek, SeekFrom, Write};
        let mut file = fs::OpenOptions::new().write(true).open(&first)?;
        file.seek(SeekFrom::Start(0))?;
        file.write_all(&second_bytes)?;
        file.sync_all()?;
    }
    assert_eq!(fs::read(&first)?, second_bytes);

    // The retained sidecar belongs to the previous generation and must be
    // refused rather than accepted with stale contents.
    let reader = VarveIndexedReader::open(spec(), &first, options, plan());
    match reader {
        Err(Error::DiskIndex(error))
            if matches!(*error, varve::DiskIndexError::PrimaryGenerationMismatch) => {}
        other => panic!("stale sidecar accepted by the reader: {:?}", other.err()),
    }
    let writer = VarveIndexedWriter::open(spec(), &first, options, plan());
    match writer {
        Err(Error::DiskIndex(error))
            if matches!(*error, varve::DiskIndexError::PrimaryGenerationMismatch) => {}
        other => panic!("stale sidecar accepted by the writer: {:?}", other.err()),
    }

    // Rebuild is the documented recovery: it republishes a sidecar for the
    // generation actually on disk.
    let report = rebuild_disk_index(spec(), &first, options, plan())?;
    // Two user records plus the leading internal creation-nonce record.
    assert_eq!(report.records, 3);
    let reader = VarveIndexedReader::open(spec(), &first, options, plan())?;
    assert_eq!(
        reader.get::<Item>(&20)?,
        Some(item(20, "fixed-width-value"))
    );
    assert_eq!(reader.get::<Item>(&10)?, None);
    assert_eq!(reader.historical_distinct_keys()?, 2);
    Ok(())
}

#[test]
fn stream_state_sidecar_refuses_a_rewritten_primary() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let first = directory.path().join("stream-first.varve");
    let second = directory.path().join("stream-second.varve");
    let options = varve::StreamOptions::default();

    for (path, keys) in [(&first, [1u64, 2u64]), (&second, [3, 4])] {
        let mut writer = VarveStreamWriter::create(stream_spec(), path, options)?;
        for key in keys {
            writer.push_info(&item(key, "fixed-width-value"))?;
        }
        writer.sync()?;
    }

    let second_bytes = fs::read(&second)?;
    assert_eq!(fs::read(&first)?.len(), second_bytes.len());
    {
        use std::io::{Seek, SeekFrom, Write};
        let mut file = fs::OpenOptions::new().write(true).open(&first)?;
        file.seek(SeekFrom::Start(0))?;
        file.write_all(&second_bytes)?;
        file.sync_all()?;
    }

    let opened = VarveStreamWriter::open(stream_spec(), &first, options);
    match opened {
        Err(Error::DiskIndex(error))
            if matches!(*error, varve::DiskIndexError::PrimaryGenerationMismatch) => {}
        other => panic!("stale stream state sidecar accepted: {:?}", other.err()),
    }
    Ok(())
}

/// F-09, consumer half. Sidecar publication cannot be made atomic against a
/// process that replaces the primary *pathname* without taking the writer lock,
/// so the library narrows the interval and retires a sidecar it published for a
/// replaced primary. What keeps that a rebuild-cost problem rather than a
/// wrong-data problem is this: a sidecar whose primary was swapped wholesale for
/// a different file object is refused by consumers on identity, before any
/// indexed lookup can be answered from it.
///
/// The existing rewrite tests replace a primary's *contents* in place, keeping
/// the OS object. This one replaces the object itself, which is the shape a
/// pathname mutator produces.
#[test]
fn stream_state_sidecar_refuses_a_wholesale_primary_replacement() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let first = directory.path().join("swap-first.varve");
    let second = directory.path().join("swap-second.varve");
    let options = varve::StreamOptions::default();

    for (path, keys) in [(&first, [1u64, 2u64]), (&second, [3, 4])] {
        let mut writer = VarveStreamWriter::create(stream_spec(), path, options)?;
        for key in keys {
            writer.push_info(&item(key, "fixed-width-value"))?;
        }
        writer.sync()?;
    }

    // The whole file object behind the pathname is replaced; the state sidecar
    // published for the original object stays where it was.
    fs::rename(&second, &first)?;

    match VarveStreamWriter::open(stream_spec(), &first, options) {
        Err(Error::DiskIndex(error)) => assert!(
            matches!(*error, varve::DiskIndexError::IdentityMismatch)
                || matches!(*error, varve::DiskIndexError::PrimaryGenerationMismatch),
            "unexpected sidecar rejection: {error:?}"
        ),
        other => panic!(
            "a sidecar for a replaced primary was accepted: {:?}",
            other.err()
        ),
    }
    Ok(())
}

/// The residual half of STO-01 an adversarial re-verification found: a
/// generation witness computed over a bounded leading window only separates two
/// primaries that diverge *inside* that window.
///
/// Here the two primaries carry 400 byte-identical leading records, so they
/// agree on ~38 KiB — nine times the 4096-byte window — and diverge only far
/// past it. Against a witness-only implementation this rewrite was accepted at
/// both reader and writer open, exposing a stale `historical_distinct_keys` and
/// turning lookups into `CheckpointMismatch`/`None`. The per-create nonce
/// stamped inside the primary makes the refusal independent of where the two
/// generations happen to diverge.
#[test]
fn rewrite_that_shares_the_generation_window_is_still_refused() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let first = directory.path().join("wide-first.varve");
    let second = directory.path().join("wide-second.varve");
    let options = DiskIndexOptions::default();

    build_primary_with_filler(&first, WINDOW_DEFEATING_FILLER, [10, 11])?;
    build_primary_with_filler(&second, WINDOW_DEFEATING_FILLER, [20, 21])?;

    let first_bytes = fs::read(&first)?;
    let second_bytes = fs::read(&second)?;
    assert_eq!(
        first_bytes.len(),
        second_bytes.len(),
        "probe requires equal-length primaries"
    );
    assert!(
        first_bytes.len() > 4 * 4096,
        "probe requires a primary far larger than the generation window, got {}",
        first_bytes.len()
    );
    // Everything except the leading creation-nonce record and the two trailing
    // key records is shared, so a prefix witness of any bounded size that skips
    // the nonce would match.
    let shared_suffix_start = first_bytes
        .iter()
        .zip(second_bytes.iter())
        .enumerate()
        .skip(4096)
        .find(|(_, (left, right))| left != right)
        .map_or(first_bytes.len(), |(index, _)| index);
    assert!(
        shared_suffix_start > 4 * 4096,
        "probe requires the divergence to sit far past the generation window, got {shared_suffix_start}"
    );

    {
        let reader = VarveIndexedReader::open(spec(), &first, options, plan())?;
        assert_eq!(
            reader.get::<Item>(&10)?,
            Some(item(10, "fixed-width-value"))
        );
    }
    let historical_before =
        VarveIndexedReader::open(spec(), &first, options, plan())?.historical_distinct_keys()?;

    rewrite_in_place(&first, &second_bytes)?;
    assert_eq!(fs::read(&first)?, second_bytes);

    match VarveIndexedReader::open(spec(), &first, options, plan()) {
        Err(Error::DiskIndex(error))
            if matches!(*error, varve::DiskIndexError::PrimaryGenerationMismatch) => {}
        other => panic!(
            "stale sidecar accepted by the reader past the generation window: {:?}",
            other.err()
        ),
    }
    match VarveIndexedWriter::open(spec(), &first, options, plan()) {
        Err(Error::DiskIndex(error))
            if matches!(*error, varve::DiskIndexError::PrimaryGenerationMismatch) => {}
        other => panic!(
            "stale sidecar accepted by the writer past the generation window: {:?}",
            other.err()
        ),
    }

    // Rebuild remains the documented recovery, and it reports the generation
    // actually on disk rather than the stale one.
    rebuild_disk_index(spec(), &first, options, plan())?;
    let reader = VarveIndexedReader::open(spec(), &first, options, plan())?;
    assert_eq!(
        reader.get::<Item>(&20)?,
        Some(item(20, "fixed-width-value"))
    );
    assert_eq!(reader.get::<Item>(&10)?, None);
    assert_eq!(reader.historical_distinct_keys()?, historical_before);
    Ok(())
}

/// The stream half of the same residual gap.
#[test]
fn stream_state_sidecar_refuses_a_rewrite_beyond_the_generation_window() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let first = directory.path().join("wide-stream-first.varve");
    let second = directory.path().join("wide-stream-second.varve");
    let options = varve::StreamOptions::default();

    for (path, keys) in [(&first, [1u64, 2u64]), (&second, [3, 4])] {
        let mut writer = VarveStreamWriter::create(stream_spec(), path, options)?;
        for index in 0..WINDOW_DEFEATING_FILLER {
            writer.push_info(&item(1_000_000 + index, "fixed-width-value"))?;
        }
        for key in keys {
            writer.push_info(&item(key, "fixed-width-value"))?;
        }
        writer.sync()?;
    }

    let first_bytes = fs::read(&first)?;
    let second_bytes = fs::read(&second)?;
    assert_eq!(first_bytes.len(), second_bytes.len());
    assert!(first_bytes.len() > 4 * 4096);
    let divergence = first_bytes
        .iter()
        .zip(second_bytes.iter())
        .enumerate()
        .skip(4096)
        .find(|(_, (left, right))| left != right)
        .map_or(first_bytes.len(), |(index, _)| index);
    assert!(
        divergence > 4 * 4096,
        "probe requires divergence past the generation window, got {divergence}"
    );

    rewrite_in_place(&first, &second_bytes)?;

    match VarveStreamWriter::open(stream_spec(), &first, options) {
        Err(Error::DiskIndex(error))
            if matches!(*error, varve::DiskIndexError::PrimaryGenerationMismatch) => {}
        other => panic!(
            "stale stream state sidecar accepted past the generation window: {:?}",
            other.err()
        ),
    }
    Ok(())
}

/// Pins the mechanism itself rather than only its effect: every create stamps a
/// distinct nonce inside the primary, at a fixed offset, readable through the
/// ordinary metadata API. Two primaries built from identical inputs must still
/// differ, and they must differ inside the generation window.
#[test]
fn every_create_stamps_a_distinct_nonce_inside_the_primary() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let first = directory.path().join("nonce-first.varve");
    let second = directory.path().join("nonce-second.varve");

    build_primary(&first, [7, 8])?;
    build_primary(&second, [7, 8])?;

    let first_bytes = fs::read(&first)?;
    let second_bytes = fs::read(&second)?;
    assert_eq!(
        first_bytes.len(),
        second_bytes.len(),
        "the nonce record must be fixed width so it cannot be told apart by length"
    );
    assert_ne!(
        first_bytes, second_bytes,
        "two creates with identical content produced identical primaries: no creation nonce"
    );

    let window = 4096.min(first_bytes.len());
    assert_ne!(
        first_bytes[..window],
        second_bytes[..window],
        "the creation nonce must sit inside the bounded generation window"
    );

    // This format carries no per-record checksum, so with identical content the
    // only bytes that may differ are the nonce itself: a run confined to 16
    // contiguous bytes, inside the generation window. (Two random nonces may
    // coincide on individual bytes, so the span is bounded, not the count.)
    let differing: Vec<usize> = first_bytes
        .iter()
        .zip(second_bytes.iter())
        .enumerate()
        .filter(|(_, (left, right))| left != right)
        .map(|(index, _)| index)
        .collect();
    assert!(!differing.is_empty());
    let start = differing[0];
    let end = differing[differing.len() - 1] + 1;
    assert!(
        end - start <= 16,
        "two identical creates differ outside a single 16-byte nonce, at {start}..{end}"
    );
    assert!(
        end <= window,
        "the creation nonce must sit inside the generation window, got {start}..{end}"
    );

    // Uniqueness is per create, not merely "the two samples differ": build a
    // batch and require every leading window to be distinct.
    let mut windows = Vec::new();
    for index in 0..8 {
        let path = directory.path().join(format!("nonce-{index}.varve"));
        build_primary(&path, [7, 8])?;
        windows.push(fs::read(&path)?[..window].to_vec());
    }
    let mut sorted = windows.clone();
    sorted.sort();
    sorted.dedup();
    assert_eq!(
        sorted.len(),
        windows.len(),
        "two creates produced the same creation nonce"
    );
    Ok(())
}

#[test]
fn mismatched_descriptor_plan_is_rejected_before_any_decode() {
    #[cfg(feature = "scalable-fault-injection")]
    varve::DiskIndexRebuildReport::reset_scaling_counters();

    let rejected = DiskIndexPlan::canonical(spec(), INDEXED_IMPOSTOR);
    assert!(
        matches!(
            rejected,
            Err(varve::DiskIndexError::Primary(
                Error::BlockSchemaFingerprintMismatch { .. }
            ))
        ),
        "impostor descriptor plan accepted: {rejected:?}"
    );

    // The report counted exactly one concrete decoder invocation on primary
    // bytes before any schema-identity rejection; the contract is zero.
    #[cfg(feature = "scalable-fault-injection")]
    assert_eq!(
        varve::DiskIndexRebuildReport::descriptor_decoder_calls(),
        0,
        "a captured codec ran before schema identity was validated"
    );
}

#[test]
fn matching_descriptor_plan_still_builds_and_indexes() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("valid-plan.varve");
    build_primary(&path, [5, 6])?;
    let reader = VarveIndexedReader::open(spec(), &path, DiskIndexOptions::default(), plan())?;
    assert_eq!(reader.get::<Item>(&5)?, Some(item(5, "fixed-width-value")));
    Ok(())
}
