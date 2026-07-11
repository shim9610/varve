#![no_main]

use std::collections::{BTreeMap, HashMap};

use libfuzzer_sys::fuzz_target;
use varve::{Endian, decode_from_slice};
use varve_fuzz::{FuzzPoint, FuzzRecord};

fuzz_target!(|data: &[u8]| {
    let Some((&selector, payload)) = data.split_first() else {
        return;
    };

    match selector % 12 {
        0 => drop(decode_from_slice::<FuzzPoint>(payload, Endian::Little)),
        1 => drop(decode_from_slice::<FuzzRecord>(payload, Endian::Little)),
        2 => drop(decode_from_slice::<Vec<u8>>(payload, Endian::Little)),
        3 => drop(decode_from_slice::<String>(payload, Endian::Little)),
        4 => drop(decode_from_slice::<Vec<i64>>(payload, Endian::Little)),
        5 => drop(decode_from_slice::<Vec<String>>(payload, Endian::Little)),
        6 => drop(decode_from_slice::<BTreeMap<u64, Vec<u8>>>(
            payload,
            Endian::Little,
        )),
        7 => drop(decode_from_slice::<HashMap<String, u64>>(
            payload,
            Endian::Little,
        )),
        8 => drop(decode_from_slice::<BTreeMap<(), ()>>(
            payload,
            Endian::Little,
        )),
        9 => drop(decode_from_slice::<HashMap<(), ()>>(
            payload,
            Endian::Little,
        )),
        10 => drop(decode_from_slice::<Option<(u64, String, Vec<u8>)>>(
            payload,
            Endian::Little,
        )),
        _ => drop(decode_from_slice::<(u128, i128, f64, bool)>(
            payload,
            Endian::Little,
        )),
    }
});
