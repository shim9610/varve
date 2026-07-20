use std::cmp::Ordering;
use std::collections::{BTreeMap, HashMap};
use std::panic::catch_unwind;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

#[cfg(all(feature = "compression-zstd", feature = "integrity"))]
use varve::CompressionLevel;
use varve::{
    ChunkedBytes, Decoder, Encoder, Endian, Error, FieldHeader, VarveBlock, VarveDecode,
    VarveEncode, WireType, decode_from_slice, encode_to_vec, read_field_header, write_field,
};

const DUPLICATE_MAP_BYTES: [u8; 12] = [2, 0, 0, 0, 0, 0, 0, 0, 7, 9, 7, 99];
const DESCENDING_MAP_BYTES: [u8; 12] = [2, 0, 0, 0, 0, 0, 0, 0, 2, 20, 1, 99];
const CANONICAL_U8_MAP_BYTES: [u8; 12] = [2, 0, 0, 0, 0, 0, 0, 0, 1, 10, 2, 20];
const VALID_MAP_BYTES: [u8; 20] = [2, 0, 0, 0, 0, 0, 0, 0, 1, 0, 10, 0, 0, 0, 2, 0, 20, 0, 0, 0];

#[derive(Debug, PartialEq, VarveBlock)]
#[varve(id = 900, version = 1, kind = "variable")]
struct HardenedVariable {
    #[varve(field_id = 1)]
    value: u8,
}

fn assert_invalid_canonical<T>(result: varve::Result<T>) {
    match result {
        Err(Error::InvalidCanonicalEncoding(_)) => {}
        Err(error) => panic!("expected invalid canonical encoding, got {error:?}"),
        Ok(_) => panic!("expected invalid canonical encoding, got success"),
    }
}

fn assert_truncations_error_without_panicking<T: VarveDecode>(bytes: &[u8]) {
    assert!(decode_from_slice::<T>(bytes, Endian::Little).is_ok());

    for end in 0..bytes.len() {
        match catch_unwind(|| decode_from_slice::<T>(&bytes[..end], Endian::Little)) {
            Ok(Err(_)) => {}
            Ok(Ok(_)) => panic!("truncated payload of {end} bytes decoded successfully"),
            Err(_) => panic!("truncated payload of {end} bytes panicked"),
        }
    }
}

fn append_field(
    output: &mut Vec<u8>,
    field_id: u32,
    wire_type: WireType,
    flags: u16,
    payload_len: u64,
    payload: &[u8],
) {
    output.extend_from_slice(&field_id.to_le_bytes());
    output.extend_from_slice(&(wire_type as u16).to_le_bytes());
    output.extend_from_slice(&flags.to_le_bytes());
    output.extend_from_slice(&payload_len.to_le_bytes());
    output.extend_from_slice(payload);
}

#[derive(Debug, Eq, Hash, PartialEq)]
struct NonCloneKey(u8);

impl Ord for NonCloneKey {
    fn cmp(&self, other: &Self) -> Ordering {
        other.0.cmp(&self.0)
    }
}

impl PartialOrd for NonCloneKey {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl VarveEncode for NonCloneKey {
    const WIRE_TYPE: WireType = WireType::U8;

    fn encode_varve(&self, encoder: &mut Encoder) -> varve::Result<()> {
        self.0.encode_varve(encoder)
    }
}

impl VarveDecode for NonCloneKey {
    const WIRE_TYPE: WireType = WireType::U8;

    fn decode_varve(decoder: &mut Decoder<'_>) -> varve::Result<Self> {
        Ok(Self(u8::decode_varve(decoder)?))
    }
}

#[test]
fn bool_codec_accepts_only_literal_zero_and_one() -> varve::Result<()> {
    assert_eq!(encode_to_vec(&false, Endian::Little)?, [0]);
    assert_eq!(encode_to_vec(&true, Endian::Little)?, [1]);
    assert!(!decode_from_slice::<bool>(&[0], Endian::Little)?);
    assert!(decode_from_slice::<bool>(&[1], Endian::Little)?);

    for byte in 2..=u8::MAX {
        assert_invalid_canonical(decode_from_slice::<bool>(&[byte], Endian::Little));
    }

    Ok(())
}

#[test]
fn nested_bool_decoders_reject_noncanonical_values() {
    assert_invalid_canonical(decode_from_slice::<Option<u16>>(&[2], Endian::Little));
    assert_invalid_canonical(decode_from_slice::<Vec<bool>>(
        &[1, 0, 0, 0, 0, 0, 0, 0, 2],
        Endian::Little,
    ));
}

#[test]
fn hash_map_encoding_sorts_borrowed_non_clone_keys_by_ord() -> varve::Result<()> {
    let values = HashMap::from([
        (NonCloneKey(2), 20u8),
        (NonCloneKey(1), 10u8),
        (NonCloneKey(3), 30u8),
    ]);

    let encoded = encode_to_vec(&values, Endian::Little)?;
    assert_eq!(encoded, [3, 0, 0, 0, 0, 0, 0, 0, 3, 30, 2, 20, 1, 10]);
    assert_eq!(
        decode_from_slice::<HashMap<NonCloneKey, u8>>(&encoded, Endian::Little)?,
        values
    );
    Ok(())
}

#[test]
fn maps_accept_only_strictly_increasing_keys() -> varve::Result<()> {
    assert_eq!(
        decode_from_slice::<BTreeMap<u8, u8>>(&CANONICAL_U8_MAP_BYTES, Endian::Little)?,
        BTreeMap::from([(1, 10), (2, 20)])
    );
    assert_eq!(
        decode_from_slice::<HashMap<u8, u8>>(&CANONICAL_U8_MAP_BYTES, Endian::Little)?,
        HashMap::from([(1, 10), (2, 20)])
    );

    assert_invalid_canonical(decode_from_slice::<BTreeMap<u8, u8>>(
        &DESCENDING_MAP_BYTES,
        Endian::Little,
    ));
    assert_invalid_canonical(decode_from_slice::<HashMap<u8, u8>>(
        &DESCENDING_MAP_BYTES,
        Endian::Little,
    ));
    Ok(())
}

#[test]
fn btree_map_rejects_duplicate_key_before_decoding_its_value() {
    assert_invalid_canonical(decode_from_slice::<BTreeMap<u8, u8>>(
        &DUPLICATE_MAP_BYTES,
        Endian::Little,
    ));
}

#[test]
fn hash_map_rejects_duplicate_key_before_decoding_its_value() {
    assert_invalid_canonical(decode_from_slice::<HashMap<u8, u8>>(
        &DUPLICATE_MAP_BYTES,
        Endian::Little,
    ));
}

#[test]
fn zero_width_maps_terminate_on_hostile_counts() {
    let hostile_count = (usize::MAX as u64).to_le_bytes();
    let (sender, receiver) = mpsc::channel();
    let worker = thread::spawn(move || {
        let btree =
            decode_from_slice::<BTreeMap<(), ()>>(&hostile_count, Endian::Little).map(|_| ());
        let hash = decode_from_slice::<HashMap<(), ()>>(&hostile_count, Endian::Little).map(|_| ());
        sender
            .send((btree, hash))
            .expect("hardening test receiver should remain available");
    });

    let (btree, hash) = receiver
        .recv_timeout(Duration::from_secs(5))
        .expect("zero-width map decoding did not terminate");
    worker.join().expect("zero-width map decoder panicked");
    assert_invalid_canonical(btree);
    assert_invalid_canonical(hash);
}

#[test]
fn map_preflight_includes_the_final_value_before_budget_or_reserve() {
    let truncated = [2, 0, 0, 0, 0, 0, 0, 0, 1, 10, 2];
    assert!(matches!(
        Decoder::decode_from_slice_limited::<BTreeMap<u8, u8>>(&truncated, Endian::Little, 0,),
        Err(Error::UnexpectedEof)
    ));
    assert!(matches!(
        Decoder::decode_from_slice_limited::<HashMap<u8, u8>>(&truncated, Endian::Little, 0,),
        Err(Error::UnexpectedEof)
    ));
}

#[test]
fn truncated_and_oversized_payloads_return_errors_without_panicking() {
    assert_truncations_error_without_panicking::<BTreeMap<u16, u32>>(&VALID_MAP_BYTES);
    assert_truncations_error_without_panicking::<HashMap<u16, u32>>(&VALID_MAP_BYTES);

    let hostile_len = u64::MAX.to_le_bytes();
    match catch_unwind(|| decode_from_slice::<Vec<u8>>(&hostile_len, Endian::Little)) {
        Ok(Err(_)) => {}
        Ok(Ok(_)) => panic!("oversized byte payload decoded successfully"),
        Err(_) => panic!("oversized byte payload panicked"),
    }
}

#[test]
fn variable_field_lengths_are_checked_before_payload_access() {
    let mut encoded = Vec::new();
    append_field(&mut encoded, 1, WireType::U8, 0, u64::MAX, &[]);

    match catch_unwind(|| decode_from_slice::<HardenedVariable>(&encoded, Endian::Little)) {
        Ok(Err(Error::LengthOverflow { value })) if usize::BITS < u64::BITS => {
            assert_eq!(value, u64::MAX);
        }
        Ok(Err(Error::UnexpectedEof)) if usize::BITS >= u64::BITS => {}
        Ok(Err(error)) => panic!("unexpected oversized field error: {error:?}"),
        Ok(Ok(_)) => panic!("oversized variable field decoded successfully"),
        Err(_) => panic!("oversized variable field panicked"),
    }
}

#[test]
fn truncated_duplicate_variable_field_is_rejected_before_field_id_allocation() {
    let mut encoded = Vec::new();
    append_field(&mut encoded, 1, WireType::U8, 0, 1, &[7]);
    append_field(&mut encoded, 1, WireType::U8, 0, 1, &[]);

    assert!(matches!(
        decode_from_slice::<HardenedVariable>(&encoded, Endian::Little),
        Err(Error::UnexpectedEof)
    ));
}

#[test]
fn built_in_allocations_share_a_finite_decoder_budget() {
    let encoded = encode_to_vec(&(vec![1u8, 2, 3], "four".to_string()), Endian::Little)
        .expect("encode fixture");
    assert!(matches!(
        Decoder::decode_from_slice_limited::<(Vec<u8>, String)>(&encoded, Endian::Little, 6),
        Err(Error::LimitExceeded {
            resource: "string",
            actual: 7,
            limit: 6
        })
    ));
    assert_eq!(
        Decoder::decode_from_slice_limited::<(Vec<u8>, String)>(&encoded, Endian::Little, 7)
            .expect("exact budget"),
        (vec![1, 2, 3], "four".to_string())
    );

    let first = encode_to_vec(&vec![1u8, 2, 3], Endian::Little).expect("encode first value");
    let second = encode_to_vec(&"four".to_string(), Endian::Little).expect("encode second value");
    let mut remaining = 7;
    assert_eq!(
        Decoder::decode_from_slice_accounted::<Vec<u8>>(&first, Endian::Little, &mut remaining)
            .expect("accounted first value"),
        vec![1, 2, 3]
    );
    assert_eq!(remaining, 4);
    assert_eq!(
        Decoder::decode_from_slice_accounted::<String>(&second, Endian::Little, &mut remaining)
            .expect("accounted second value"),
        "four"
    );
    assert_eq!(remaining, 0);
}

#[test]
fn byte_derived_counts_are_rejected_before_loops_or_allocations() {
    let hostile = u64::MAX.to_le_bytes();
    for result in [
        decode_from_slice::<Vec<u64>>(&hostile, Endian::Little).map(|_| ()),
        decode_from_slice::<Vec<String>>(&hostile, Endian::Little).map(|_| ()),
        decode_from_slice::<HashMap<u64, u64>>(&hostile, Endian::Little).map(|_| ()),
    ] {
        assert!(result.is_err());
    }
}

#[test]
fn field_extent_is_checked_before_large_field_id_bookkeeping() {
    let mut encoded = Vec::new();
    append_field(&mut encoded, u32::MAX, WireType::U8, 0, 1, &[]);
    let mut decoder = Decoder::new(&encoded, Endian::Little);
    assert!(matches!(
        read_field_header(&mut decoder),
        Err(Error::UnexpectedEof)
    ));
}

#[test]
fn nonzero_field_flags_are_rejected_for_known_and_unknown_fields() {
    for field_id in [1, 99] {
        for flags in [1, 2, 0x8000, u16::MAX] {
            let mut encoded = Vec::new();
            append_field(&mut encoded, field_id, WireType::U8, flags, 0, &[]);
            assert_invalid_canonical(decode_from_slice::<HardenedVariable>(
                &encoded,
                Endian::Little,
            ));
        }
    }
}

#[test]
fn zero_flag_unknown_fields_remain_skippable() -> varve::Result<()> {
    let mut encoded = Vec::new();
    append_field(&mut encoded, 99, WireType::U16, 0, 2, &[0xaa, 0xbb]);
    append_field(&mut encoded, 1, WireType::U8, 0, 1, &[7]);

    assert_eq!(
        decode_from_slice::<HardenedVariable>(&encoded, Endian::Little)?,
        HardenedVariable { value: 7 }
    );
    Ok(())
}

#[test]
fn nested_field_and_top_level_map_decoders_report_exact_trailing_bytes() {
    let mut field = Vec::new();
    append_field(&mut field, 1, WireType::U8, 0, 2, &[7, 8]);
    assert!(matches!(
        decode_from_slice::<HardenedVariable>(&field, Endian::Little),
        Err(Error::TrailingBytes { remaining: 1 })
    ));

    let mut map = CANONICAL_U8_MAP_BYTES.to_vec();
    map.extend_from_slice(&[0xaa, 0xbb]);
    assert!(matches!(
        decode_from_slice::<BTreeMap<u8, u8>>(&map, Endian::Little),
        Err(Error::TrailingBytes { remaining: 2 })
    ));
    assert!(matches!(
        decode_from_slice::<HashMap<u8, u8>>(&map, Endian::Little),
        Err(Error::TrailingBytes { remaining: 2 })
    ));
}

#[test]
fn field_headers_decode_checked_lengths_while_writers_emit_zero_flags() -> varve::Result<()> {
    let canonical_header = [
        0x44, 0x33, 0x22, 0x11, 0x0f, 0x00, 0x00, 0x00, 0x03, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0xaa, 0xbb, 0xcc,
    ];
    let mut decoder = Decoder::new(&canonical_header, Endian::Little);
    assert_eq!(
        read_field_header(&mut decoder)?,
        FieldHeader {
            field_id: 0x1122_3344,
            wire_type: WireType::String,
            payload_len: 3,
        }
    );
    assert_eq!(decoder.remaining(), 3);

    let mut encoder = Encoder::new(Endian::Little);
    write_field(
        &mut encoder,
        0x1122_3344,
        WireType::String,
        &[0xaa, 0xbb, 0xcc],
    )?;
    assert_eq!(
        encoder.into_inner(),
        [
            0x44, 0x33, 0x22, 0x11, 0x0f, 0x00, 0x00, 0x00, 0x03, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0xaa, 0xbb, 0xcc,
        ]
    );

    Ok(())
}

/// RES-01: distinct variable field ids above the small-id mask are the one
/// decoder-owned set an attacker sizes directly from the header stream, so
/// each one must be charged to the materialization budget before the set
/// reserves for it — like every other decoder-owned container.
#[test]
fn large_field_id_bookkeeping_is_charged_to_the_materialization_budget() {
    const HIGH_FIELD_IDS: u32 = 8;
    const CHARGE_PER_ID: u64 = 8;

    let mut encoded = Vec::new();
    append_field(&mut encoded, 1, WireType::U8, 0, 1, &[7]);
    for offset in 0..HIGH_FIELD_IDS {
        append_field(&mut encoded, 64 + offset, WireType::U8, 0, 1, &[0]);
    }

    // A zero budget cannot admit even the first high field id.
    assert!(matches!(
        Decoder::decode_from_slice_limited::<HardenedVariable>(&encoded, Endian::Little, 0),
        Err(Error::LimitExceeded {
            resource: "variable field ids",
            actual: CHARGE_PER_ID,
            limit: 0,
        })
    ));

    // One byte short of the full set still fails, and it fails on the last
    // id rather than silently over-allocating.
    let exact = CHARGE_PER_ID * u64::from(HIGH_FIELD_IDS);
    assert!(matches!(
        Decoder::decode_from_slice_limited::<HardenedVariable>(
            &encoded,
            Endian::Little,
            exact - 1
        ),
        Err(Error::LimitExceeded {
            resource: "variable field ids",
            limit,
            ..
        }) if limit == exact - 1
    ));

    // The exact budget admits the record, and small ids stay free: they are
    // tracked by the fixed 64-bit mask and own no allocation.
    assert_eq!(
        Decoder::decode_from_slice_limited::<HardenedVariable>(&encoded, Endian::Little, exact)
            .expect("exact large-field-id budget"),
        HardenedVariable { value: 7 }
    );

    let mut small_only = Vec::new();
    append_field(&mut small_only, 1, WireType::U8, 0, 1, &[7]);
    append_field(&mut small_only, 63, WireType::U8, 0, 1, &[0]);
    assert_eq!(
        Decoder::decode_from_slice_limited::<HardenedVariable>(&small_only, Endian::Little, 0)
            .expect("small field ids need no budget"),
        HardenedVariable { value: 7 }
    );
}

/// Duplicate detection still precedes the charge, so a hostile duplicate
/// cannot drain the budget on an id the set already holds.
#[test]
fn duplicate_large_field_ids_are_rejected_without_extra_charge() {
    let mut encoded = Vec::new();
    append_field(&mut encoded, 1, WireType::U8, 0, 1, &[7]);
    append_field(&mut encoded, 4096, WireType::U8, 0, 1, &[0]);
    append_field(&mut encoded, 4096, WireType::U8, 0, 1, &[0]);

    assert_invalid_canonical(Decoder::decode_from_slice_limited::<HardenedVariable>(
        &encoded,
        Endian::Little,
        8,
    ));
}

/// SAFE-01. `HashMap::try_reserve(len)` allocates a real open-addressed table:
/// buckets are rounded up to a power of two at a 7/8 load factor and *every*
/// bucket owns a control byte, so a zero-sized entry costs far more than the
/// one byte the old preflight floor charged it. With that floor the standard
/// 1 GiB budget admitted close to one billion declared entries for
/// `HashMap<(), ()>` and the reservation crossed a 1 GiB RSS ceiling from an
/// 11-byte input.
///
/// The count here sits *below* the budget in entry terms — one million entries
/// against a one-million-byte budget, which the old one-byte-per-entry model
/// accepted — so this covers the case the pre-existing
/// `zero_width_maps_terminate_on_hostile_counts` (a count above the budget,
/// rejected as non-progressing) does not. It must fail with the typed budget
/// error before any table is allocated, so its own footprint stays negligible.
#[test]
fn zero_sized_hash_map_entries_are_charged_their_real_table_cost() {
    const DECLARED: u64 = 1_000_000;
    const BUDGET: u64 = 1_000_000;

    let encoded = DECLARED.to_le_bytes();
    // Sanity: the count is not rejected as non-progressing; it is genuinely
    // within the budget under the discredited one-byte-per-entry model.
    const { assert!(DECLARED <= BUDGET) };

    match Decoder::decode_from_slice_limited::<HashMap<(), ()>>(&encoded, Endian::Little, BUDGET) {
        Err(Error::LimitExceeded {
            resource,
            actual,
            limit,
        }) => {
            assert_eq!(resource, "HashMap entries");
            assert_eq!(limit, BUDGET);
            // 2^21 buckets * 1 control byte each, the smallest power-of-two
            // table that holds 1_000_000 entries at a 7/8 load factor.
            assert!(
                actual >= 2_097_152,
                "charge {actual} must model the real bucket array"
            );
        }
        Err(error) => panic!("expected a budget rejection, got {error:?}"),
        Ok(_) => panic!("a zero-sized-entry map outran its budget"),
    }

    // The same input under the standard 1 GiB budget: this is the fuzz
    // artifact's shape, and it must be a typed rejection rather than a
    // gigabyte-scale reservation.
    const FUZZ_DECLARED: u64 = 1_000_000_000;
    let encoded = FUZZ_DECLARED.to_le_bytes();
    const { assert!(FUZZ_DECLARED < Decoder::STANDARD_MATERIALIZATION_LIMIT) };
    assert!(matches!(
        decode_from_slice::<HashMap<(), ()>>(&encoded, Endian::Little),
        Err(Error::LimitExceeded {
            resource: "HashMap entries",
            ..
        })
    ));
}

/// The tightened charge must bound the reservation without rejecting maps that
/// genuinely fit: honest small maps still decode, and a sized-entry map is
/// charged more than its raw `size_of` footprint because the table is.
#[test]
fn hash_map_budget_still_admits_honest_maps() {
    assert_eq!(
        decode_from_slice::<HashMap<u16, u32>>(&VALID_MAP_BYTES, Endian::Little)
            .expect("honest map decodes"),
        HashMap::from([(1u16, 10u32), (2, 20)])
    );

    // Two entries of `(u16, u32)` occupy 16 bytes of `size_of`, but the table
    // that holds them is larger; a budget equal to the raw entry bytes must not
    // be treated as sufficient.
    assert!(matches!(
        Decoder::decode_from_slice_limited::<HashMap<u16, u32>>(
            &VALID_MAP_BYTES,
            Endian::Little,
            16,
        ),
        Err(Error::LimitExceeded {
            resource: "HashMap entries",
            ..
        })
    ));
}

/// API-03. `ChunkedBytes` is a documented variable-field codec, so it must
/// declare a stable non-zero identity (the derive rejects zero) that is
/// distinct from the `Vec<u8>` codec whose bytes it rides on. The literals are
/// pinned so an accidental retag or format-version bump is a visible test
/// change rather than a silent identity drift.
#[test]
fn chunked_bytes_declares_a_stable_non_zero_schema_identity() {
    const ENCODE_ID: u64 = <ChunkedBytes as VarveEncode>::SCHEMA_ID;
    const DECODE_ID: u64 = <ChunkedBytes as VarveDecode>::SCHEMA_ID;

    assert_ne!(ENCODE_ID, 0);
    assert_eq!(ENCODE_ID, DECODE_ID);
    assert_eq!(ENCODE_ID, 0x0b01_19b7_650d_1366);
    assert_ne!(ENCODE_ID, <Vec<u8> as VarveEncode>::SCHEMA_ID);
    assert_eq!(<ChunkedBytes as VarveEncode>::WIRE_TYPE, WireType::Bytes);
}

/// The identity change must not disturb the bytes: a chunked payload still
/// roundtrips through the ordinary codec entry points.
/// Requires both features: `from_zstd_chunks` needs the zstd encoder and the
/// chunk framing carries an integrity digest, so without them the constructor
/// returns `CompressionFeatureDisabled` / `IntegrityFeatureDisabled` rather
/// than exercising the codec. The identity assertions above stay ungated.
#[cfg(all(feature = "compression-zstd", feature = "integrity"))]
#[test]
fn chunked_bytes_roundtrips_through_the_codec() -> varve::Result<()> {
    let payload: Vec<u8> = (0..1024u32).map(|index| (index % 251) as u8).collect();
    let chunked = ChunkedBytes::from_zstd_chunks(&payload, 128, CompressionLevel::Fast)?;
    let encoded = encode_to_vec(&chunked, Endian::Little)?;
    let decoded: ChunkedBytes = decode_from_slice(&encoded, Endian::Little)?;
    assert_eq!(decoded, chunked);
    assert_eq!(
        decoded.decode_to_vec()?,
        payload,
        "chunked payload must survive the roundtrip"
    );
    Ok(())
}

/// API-03, the actual regression: a variable block with a documented
/// `ChunkedBytes` field (docs/spec.md:52) must compile. `#[derive(VarveBlock)]`
/// rejects at compile time every field whose codec identity is zero, so before
/// the identity above existed this declaration did not build at all.
///
/// The declaration itself is the compile-time half of the regression and stays
/// ungated so every feature configuration builds it; only the runtime
/// roundtrip below needs zstd + integrity to construct a `ChunkedBytes`.
#[allow(dead_code)]
#[derive(Debug, PartialEq, VarveBlock)]
#[varve(id = 901, version = 1, kind = "variable")]
struct ChunkedVariable {
    #[varve(field_id = 1)]
    blob: ChunkedBytes,
}

#[cfg(all(feature = "compression-zstd", feature = "integrity"))]
#[test]
fn chunked_bytes_is_usable_as_a_derived_variable_field() -> varve::Result<()> {
    let payload: Vec<u8> = (0..600u32).map(|index| (index % 97) as u8).collect();
    let record = ChunkedVariable {
        blob: ChunkedBytes::from_zstd_chunks(&payload, 64, CompressionLevel::Fast)?,
    };
    let encoded = encode_to_vec(&record, Endian::Little)?;
    let decoded: ChunkedVariable = decode_from_slice(&encoded, Endian::Little)?;
    assert_eq!(decoded, record);
    assert_eq!(decoded.blob.decode_to_vec()?, payload);
    assert_ne!(ChunkedVariable::SCHEMA_FINGERPRINT, 0);
    Ok(())
}
