use std::cmp::Ordering;
use std::collections::{BTreeMap, HashMap};
use std::panic::catch_unwind;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use varve::{
    Decoder, Encoder, Endian, Error, FieldHeader, VarveDecode, VarveEncode, WireType,
    decode_from_slice, encode_to_vec, read_field_header, write_field,
};

const DUPLICATE_MAP_BYTES: [u8; 11] = [2, 0, 0, 0, 0, 0, 0, 0, 7, 9, 7];
const VALID_MAP_BYTES: [u8; 20] = [2, 0, 0, 0, 0, 0, 0, 0, 1, 0, 10, 0, 0, 0, 2, 0, 20, 0, 0, 0];

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

    assert_eq!(
        encode_to_vec(&values, Endian::Little)?,
        [3, 0, 0, 0, 0, 0, 0, 0, 3, 30, 2, 20, 1, 10]
    );
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
fn field_headers_accept_future_flags_while_writers_emit_zero() -> varve::Result<()> {
    let future_header = [
        0x44, 0x33, 0x22, 0x11, 0x0f, 0x00, 0x5a, 0xa5, 0x03, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00,
    ];
    let mut decoder = Decoder::new(&future_header, Endian::Little);
    assert_eq!(
        read_field_header(&mut decoder)?,
        FieldHeader {
            field_id: 0x1122_3344,
            wire_type: WireType::String,
            payload_len: 3,
        }
    );
    assert_eq!(decoder.remaining(), 0);

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
