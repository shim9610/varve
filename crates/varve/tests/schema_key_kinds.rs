//! Finite keys identify declared value combinations; offsets identify records.
use varve::{
    Decoder, Encoder, Endian, VarveBlock, VarveDecode, VarveEncode, VarveKeyedBlock, varve_format,
};

varve_format! {
    pub format Finite {
        magic: b"FINITE"; version: 1; schema_hash: computed;
        index: keyed_offset_chain;
        blocks {
            fixed Frame(id = 1, key = [scan, frame],
                key_values = [Scan1Frame2 = (1, 2), Scan2Frame4 = (2, 4), Scan9Frame8 = (9, 8)]) {
                scan: u32, frame: u32, value: u64,
            }
            variable Channel(id = 2, key = [name, active],
                key_domain = [name = ["red", "blue"], active = [false, true]]) {
                name: String, active: bool, value: u64,
            }
            fixed Sample(id = 3, key = [scan, frame],
                key_domain = [scan = -2..=1, frame = 0..4]) {
                scan: i8, frame: u16, value: u64,
            }
            fixed Mixed(id = 4, key = [second, first], key_values = [Pair = (9, 7)]) {
                prefix: u8, first: u16, middle: u32, second: u16, suffix: u8,
            }
        }
    }
}

#[test]
fn actual_combinations_have_fixed_codes_and_two_byte_encoding() -> varve::Result<()> {
    assert_eq!(FrameKey::COUNT, 3);
    assert_eq!(std::mem::size_of::<FrameKey>(), 2);
    assert_eq!(FrameKey::from_values((1, 2))?, FrameKey::Scan1Frame2);
    assert_eq!(FrameKey::Scan2Frame4.values(), (2, 4));
    assert!(FrameKey::from_values((1, 4)).is_err());
    assert!(FrameKey::from_code(3).is_err());
    assert!(FrameKey::from_code(u16::MAX).is_err());
    for endian in [Endian::Little, Endian::Big] {
        for key in FrameKey::ALL {
            let mut encoder = Encoder::new(endian);
            key.encode_varve(&mut encoder)?;
            let bytes = encoder.into_inner();
            assert_eq!(bytes.len(), 2);
            assert_eq!(
                FrameKey::decode_varve(&mut Decoder::new(&bytes, endian))?,
                *key
            );
        }
        assert!(FrameKey::decode_varve(&mut Decoder::new(&[255, 255], endian)).is_err());
        assert!(FrameKey::decode_varve(&mut Decoder::new(&[0], endian)).is_err());
    }
    let frame = Frame {
        key: FrameKey::Scan1Frame2,
        value: 17,
    };
    assert_eq!(frame.key(), FrameKey::Scan1Frame2);
    let mut encoder = Encoder::new(Endian::Little);
    frame.encode_varve(&mut encoder)?;
    assert_eq!(encoder.into_inner().len(), 10);
    Ok(())
}

#[test]
fn cartesian_domains_include_each_value_combination_once() -> varve::Result<()> {
    assert_eq!(ChannelKey::COUNT, 4);
    for key in ChannelKey::ALL {
        assert_eq!(ChannelKey::from_values(key.values())?, *key);
    }
    assert!(ChannelKey::from_values(("green", true)).is_err());
    assert_eq!(SampleKey::COUNT, 16);
    for scan in -2..=1 {
        for frame in 0..4 {
            let key = SampleKey::from_values((scan, frame))?;
            assert_eq!(key.values(), (scan, frame));
            assert_eq!(key.code(), ((scan + 2) as u16) * 4 + frame);
        }
    }
    assert!(SampleKey::from_values((2, 0)).is_err());
    Ok(())
}

mod changed {
    use varve::varve_format;
    varve_format! {
        pub format Changed {
            magic: b"FINITE"; version: 1; schema_hash: computed;
            blocks {
                fixed Frame(id = 1, key = [scan, frame],
                    key_values = [Scan1Frame2 = (1, 3), Scan2Frame4 = (2, 4), Scan9Frame8 = (9, 8)]) {
                    scan: u32, frame: u32, value: u64,
                }
            }
        }
    }
}

#[test]
fn changed_value_mapping_changes_schema_identity() {
    assert_ne!(
        <FrameKey as VarveEncode>::SCHEMA_ID,
        <changed::FrameKey as VarveEncode>::SCHEMA_ID
    );
    assert_ne!(
        Frame::SCHEMA_FINGERPRINT,
        changed::Frame::SCHEMA_FINGERPRINT
    );
}

#[test]
fn combined_field_preserves_non_key_field_order_and_ids() -> varve::Result<()> {
    let key = MixedKey::from_values((9, 7))?;
    let value = Mixed {
        prefix: 1,
        key,
        middle: 0x04030201,
        suffix: 2,
    };
    let mut encoder = Encoder::new(Endian::Little);
    value.encode_varve(&mut encoder)?;
    let bytes = encoder.into_inner();
    assert_eq!(bytes, [1, 0, 0, 1, 2, 3, 4, 2]);
    assert_eq!(
        Mixed::decode_varve(&mut Decoder::new(&bytes, Endian::Little))?,
        value
    );
    assert_eq!(
        Mixed::FIELDS.iter().map(|f| f.id).collect::<Vec<_>>(),
        [1, 2, 3, 5]
    );
    Ok(())
}

mod indexed {
    use super::*;
    use varve::DiskIndexOptions;
    varve_format! {
        pub format Disk {
            magic: b"FINDISK"; version: 1; schema_hash: computed;
            index: keyed_offset_chain;
            blocks {
                fixed Entry(id = 1, key = [scan, frame], key_index = disk,
                    key_domain = [scan = 0..4, frame = 0..8]) {
                    scan: u32, frame: u32, value: u64,
                }
                fixed LogRecord(id = 2) { sequence: u64 }
            }
        }
    }

    #[test]
    fn bounded_keys_follow_delete_compact_and_preserve_record_offsets() -> varve::Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("finite.varve");
        let opts = DiskIndexOptions::default();
        let mut writer = Disk::create_indexed_writer(&path, opts)?;
        let key = EntryKey::from_values((0, 0))?;
        let first = writer.push_entry(&Entry { key, value: 0 })?;
        let mut previous = [None; 32];
        previous[0] = Some(first.record_offset);
        let mut last_offset = first.record_offset;
        writer.sync()?;
        let mut reader = Disk::open_indexed_reader(&path, opts)?;
        for value in 1..=2048 {
            let code = (value % 32) as u16;
            let info = writer.push_entry(&Entry {
                key: EntryKey::from_code(code)?,
                value,
            })?;
            assert_eq!(info.prev_same_key_offset, previous[usize::from(code)]);
            assert!(info.record_offset > last_offset);
            previous[usize::from(code)] = Some(info.record_offset);
            last_offset = info.record_offset;
            writer.push_log_record(&LogRecord { sequence: value })?;
        }
        assert_eq!(reader.get_entry(&key)?.unwrap().value, 0);
        writer.sync()?;
        reader.follow()?;
        assert_eq!(EntryKey::COUNT, 32);
        assert_eq!(reader.get_entry(&key)?.unwrap().value, 2048);
        let records: Vec<_> = reader.entries()?.collect::<varve::Result<_>>()?;
        assert_eq!(records.len(), 2049);
        let events: Vec<_> = reader.log_records()?.collect::<varve::Result<_>>()?;
        assert_eq!(events.len(), 2048);
        writer.delete_entry(&key)?;
        writer.sync()?;
        assert_eq!(reader.get_entry(&key)?.unwrap().value, 2048);
        reader.follow()?;
        assert!(reader.get_entry(&key)?.is_none());
        writer.compact_index()?;
        reader.follow()?;
        let reopened = Disk::open_indexed_reader(&path, opts)?;
        assert!(reopened.get_entry(&key)?.is_none());
        for code in 1..32 {
            assert_eq!(
                reopened
                    .get_entry(&EntryKey::from_code(code)?)?
                    .unwrap()
                    .value,
                2016 + u64::from(code)
            );
        }
        assert_eq!(reopened.into_inner().historical_distinct_keys()?, 32);
        Ok(())
    }

    proptest::proptest! {
        #![proptest_config(proptest::test_runner::Config::with_cases(32))]
        #[test]
        fn finite_operations_match_redb_and_model(
            operations in proptest::collection::vec((0u16..32, proptest::option::of(proptest::num::u64::ANY)), 1..160)
        ) {
            use redb::{ReadableDatabase, TableDefinition};
            const TABLE: TableDefinition<u16, u64> = TableDefinition::new("finite");
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("finite.varve");
            let opts = DiskIndexOptions::default();
            let oracle = redb::Database::create(dir.path().join("oracle.redb")).unwrap();
            let mut writer = Disk::create_indexed_writer(&path, opts).unwrap();
            writer.sync().unwrap();
            let mut reader = Disk::open_indexed_reader(&path, opts).unwrap();
            let mut model = [None; 32];
            for (generation, batch) in operations.chunks(16).enumerate() {
                let old = model;
                let transaction = oracle.begin_write().unwrap();
                {
                    let mut table = transaction.open_table(TABLE).unwrap();
                    for &(code, value) in batch {
                        let key = EntryKey::from_code(code).unwrap();
                        match value {
                            Some(value) => {
                                writer.push_entry(&Entry { key, value }).unwrap();
                                table.insert(code, value).unwrap();
                            }
                            None => {
                                writer.delete_entry(&key).unwrap();
                                table.remove(code).unwrap();
                            }
                        }
                        model[usize::from(code)] = value;
                    }
                }
                transaction.commit().unwrap();
                writer.sync().unwrap();
                for (code, expected) in old.into_iter().enumerate() {
                    assert_eq!(reader.get_entry(&EntryKey::from_code(code as u16).unwrap()).unwrap().map(|v| v.value), expected);
                }
                if generation % 3 == 2 { writer.compact_index().unwrap(); }
                reader.follow().unwrap();
                let transaction = oracle.begin_read().unwrap();
                let table = transaction.open_table(TABLE).unwrap();
                for (code, expected) in model.into_iter().enumerate() {
                    let actual = reader.get_entry(&EntryKey::from_code(code as u16).unwrap()).unwrap().map(|v| v.value);
                    assert_eq!(actual, expected);
                    assert_eq!(actual, table.get(code as u16).unwrap().map(|v| v.value()));
                }
            }
            assert!(reader.into_inner().historical_distinct_keys().unwrap() <= 32);
            let reader = Disk::open_indexed_reader(&path, opts).unwrap();
            for (code, expected) in model.into_iter().enumerate() {
                assert_eq!(reader.get_entry(&EntryKey::from_code(code as u16).unwrap()).unwrap().map(|v| v.value), expected);
            }
        }
    }

    mod changed_mapping {
        use varve::varve_format;
        varve_format! {
            pub format Disk {
                magic: b"FINDISK"; version: 1; schema_hash: computed;
                index: keyed_offset_chain;
                blocks {
                    fixed Entry(id = 1, key = [scan, frame], key_index = disk,
                        key_domain = [scan = [1, 0, 2, 3], frame = 0..8]) {
                        scan: u32, frame: u32, value: u64,
                    }
                    fixed LogRecord(id = 2) { sequence: u64 }
                }
            }
        }
    }

    #[test]
    fn opening_with_reassigned_codes_is_rejected() -> varve::Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("finite.varve");
        let opts = DiskIndexOptions::default();
        let mut writer = Disk::create_indexed_writer(&path, opts)?;
        writer.push_entry(&Entry {
            key: EntryKey::from_code(0)?,
            value: 7,
        })?;
        writer.sync()?;
        assert!(changed_mapping::Disk::open_indexed_reader(&path, opts).is_err());
        Ok(())
    }
}
