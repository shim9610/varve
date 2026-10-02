#![cfg(feature = "integrity")]
//! Controlled representation benchmark. Run with scripts/compare_finite_keys.py.
//! Ignored by default: creates and fully validates actual temporary payload files.
use std::{fs, hint::black_box, time::Instant};
use varve::{
    BatchOptions, DiskIndexOptions, DiskIndexPlan, FormatSpec, VarveDiskKey, VarveIndexedReader,
    VarveIndexedWriter, VarveKeyedBlock, varve_format,
};
mod raw_numeric {
    use super::*;
    varve_format! {
        pub format Capture {
            magic: b"FINPERF"; version: 1; schema_hash: computed;
            index: keyed_offset_chain; integrity: crc32;
            blocks {
                variable Record(id=1, key=[scan, frame], key_index=disk) {
                    scan: u32, frame: u32, value: u64, payload: Vec<u8>,
                }
            }
        }
    }
}
mod finite_numeric {
    use super::*;
    varve_format! {
        pub format Capture {
            magic: b"FINPERF"; version: 1; schema_hash: computed;
            index: keyed_offset_chain; integrity: crc32;
            blocks {
                variable Record(id=1, key=[scan, frame], key_index=disk, key_domain = [scan = 0..64, frame = 0..64]) {
                    scan: u32, frame: u32, value: u64, payload: Vec<u8>,
                }
            }
        }
    }
}
mod raw_label {
    use super::*;
    varve_format! {
        pub format Capture {
            magic: b"FINPERF"; version: 1; schema_hash: computed;
            index: keyed_offset_chain; integrity: crc32;
            blocks {
                variable Record(id=1, key=[label, channel], key_index=disk) {
                    label: String, channel: u32, value: u64, payload: Vec<u8>,
                }
            }
        }
    }
}
mod finite_label {
    use super::*;
    varve_format! {
        pub format Capture {
            magic: b"FINPERF"; version: 1; schema_hash: computed;
            index: keyed_offset_chain; integrity: crc32;
            blocks {
                variable Record(id=1, key=[label, channel], key_index=disk, key_domain = [label = ["schema/channel/xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx/000", "schema/channel/xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx/001", "schema/channel/xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx/002", "schema/channel/xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx/003"], channel = 0..16]) {
                    label: String, channel: u32, value: u64, payload: Vec<u8>,
                }
            }
        }
    }
}
const LABELS: [&str; 4] = [
    "schema/channel/xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx/000",
    "schema/channel/xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx/001",
    "schema/channel/xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx/002",
    "schema/channel/xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx/003",
];
trait Bench: VarveKeyedBlock {
    fn spec() -> FormatSpec;
    fn plan() -> DiskIndexPlan;
    fn input_key(code: u64) -> Self::Key;
    fn make(code: u64, value: u64, bytes: usize) -> Self;
    fn check(&self, code: u64, value: u64, bytes: usize);
}
impl Bench for raw_numeric::Record {
    fn spec() -> FormatSpec {
        raw_numeric::Capture::spec()
    }
    fn plan() -> DiskIndexPlan {
        raw_numeric::Capture::disk_index_plan().unwrap()
    }
    fn input_key(code: u64) -> Self::Key {
        ((code / 64) as u32, (code % 64) as u32)
    }
    fn make(code: u64, value: u64, bytes: usize) -> Self {
        Self {
            scan: (code / 64) as u32,
            frame: (code % 64) as u32,
            value,
            payload: vec![code as u8; bytes],
        }
    }
    fn check(&self, code: u64, value: u64, bytes: usize) {
        assert_eq!((self.scan, self.frame), Self::input_key(code));
        assert_eq!(self.value, value);
        assert_eq!(self.payload.len(), bytes);
        assert!(self.payload.iter().all(|v| *v == code as u8));
    }
}
impl Bench for finite_numeric::Record {
    fn spec() -> FormatSpec {
        finite_numeric::Capture::spec()
    }
    fn plan() -> DiskIndexPlan {
        finite_numeric::Capture::disk_index_plan().unwrap()
    }
    fn input_key(code: u64) -> Self::Key {
        finite_numeric::RecordKey::from_values(((code / 64) as u32, (code % 64) as u32)).unwrap()
    }
    fn make(code: u64, value: u64, bytes: usize) -> Self {
        Self {
            key: Self::input_key(code),
            value,
            payload: vec![code as u8; bytes],
        }
    }
    fn check(&self, code: u64, value: u64, bytes: usize) {
        assert_eq!(self.key, Self::input_key(code));
        assert_eq!(self.value, value);
        assert_eq!(self.payload.len(), bytes);
        assert!(self.payload.iter().all(|v| *v == code as u8));
    }
}
impl Bench for raw_label::Record {
    fn spec() -> FormatSpec {
        raw_label::Capture::spec()
    }
    fn plan() -> DiskIndexPlan {
        raw_label::Capture::disk_index_plan().unwrap()
    }
    fn input_key(code: u64) -> Self::Key {
        (LABELS[(code / 16) as usize].to_owned(), (code % 16) as u32)
    }
    fn make(code: u64, value: u64, bytes: usize) -> Self {
        Self {
            label: LABELS[(code / 16) as usize].to_owned(),
            channel: (code % 16) as u32,
            value,
            payload: vec![code as u8; bytes],
        }
    }
    fn check(&self, code: u64, value: u64, bytes: usize) {
        assert_eq!(self.label, LABELS[(code / 16) as usize]);
        assert_eq!(self.channel, (code % 16) as u32);
        assert_eq!(self.value, value);
        assert_eq!(self.payload.len(), bytes);
        assert!(self.payload.iter().all(|v| *v == code as u8));
    }
}
impl Bench for finite_label::Record {
    fn spec() -> FormatSpec {
        finite_label::Capture::spec()
    }
    fn plan() -> DiskIndexPlan {
        finite_label::Capture::disk_index_plan().unwrap()
    }
    fn input_key(code: u64) -> Self::Key {
        finite_label::RecordKey::from_values((LABELS[(code / 16) as usize], (code % 16) as u32))
            .unwrap()
    }
    fn make(code: u64, value: u64, bytes: usize) -> Self {
        Self {
            key: Self::input_key(code),
            value,
            payload: vec![code as u8; bytes],
        }
    }
    fn check(&self, code: u64, value: u64, bytes: usize) {
        assert_eq!(self.key, Self::input_key(code));
        assert_eq!(self.value, value);
        assert_eq!(self.payload.len(), bytes);
        assert!(self.payload.iter().all(|v| *v == code as u8));
    }
}
fn number(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .map(|s| s.parse().unwrap())
        .unwrap_or(default)
}
fn measure<T: Bench>(mode: &str)
where
    T::Key: VarveDiskKey,
{
    let keys = number("VARVE_BENCH_KEYS", 4096);
    let records = number("VARVE_BENCH_RECORDS", 1_000_000);
    let payload = number("VARVE_BENCH_PAYLOAD", 0) as usize;
    let sync_records = number("VARVE_BENCH_SYNC", 16384);
    let queries = number("VARVE_BENCH_QUERIES", 200_000);
    let batch_records = number("VARVE_BENCH_BATCH_RECORDS", 16384) as usize;
    let key_order = std::env::var("VARVE_BENCH_KEY_ORDER").unwrap_or_else(|_| "schema".into());
    assert!(key_order == "schema" || (key_order == "wire" && keys == 4096));
    let code_at = |ordinal: u64| {
        let ordinal = ordinal % keys;
        if key_order == "wire" {
            (ordinal % 16) * 256 + ordinal / 16
        } else {
            ordinal
        }
    };
    let latest = |code: u64| {
        let ordinal = if key_order == "wire" {
            (code % 256) * 16 + code / 256
        } else {
            code
        };
        records - 1 - (records - 1 - ordinal) % keys
    };
    assert!(keys > 0 && records >= keys && sync_records > 0);
    let root = std::env::var("VARVE_BENCH_DATA").expect("set an owned benchmark data directory");
    let dir = tempfile::tempdir_in(root).unwrap();
    let path = dir.path().join("records.varve");
    let options = DiskIndexOptions::default();
    let spec = T::spec();
    let plan = T::plan();
    let mut writer = VarveIndexedWriter::create(spec, &path, options, plan).unwrap();
    let mut sync_ms = 0.0;
    let started = Instant::now();
    let mut position = 0;
    while position < records {
        let end = (position + sync_records).min(records);
        writer
            .push_iter::<T, _>(
                (position..end).map(|value| T::make(code_at(value), value, payload)),
                BatchOptions {
                    max_records: batch_records,
                    ..BatchOptions::default()
                },
            )
            .unwrap();
        let sync = Instant::now();
        writer.sync().unwrap();
        sync_ms += sync.elapsed().as_secs_f64() * 1000.0;
        position = end;
    }
    let write_ms = started.elapsed().as_secs_f64() * 1000.0;
    let native_bytes = fs::metadata(&path).unwrap().len();
    let sidecar = path.with_file_name("records.varve.vki");
    let index_bytes = fs::metadata(&sidecar).unwrap().len();
    #[cfg(unix)]
    let allocated_bytes = {
        use std::os::unix::fs::MetadataExt;
        fs::metadata(&path).unwrap().blocks() * 512
    };
    #[cfg(not(unix))]
    let allocated_bytes = native_bytes;
    let mut opens = Vec::new();
    for _ in 0..31 {
        let start = Instant::now();
        let reader = VarveIndexedReader::open(spec, &path, options, plan).unwrap();
        opens.push(start.elapsed().as_secs_f64() * 1000.0);
        drop(reader);
    }
    opens.sort_by(f64::total_cmp);
    let input_keys: Vec<_> = (0..keys).map(T::input_key).collect();
    let mut reader = VarveIndexedReader::open(spec, &path, options, plan).unwrap();
    assert_eq!(reader.historical_distinct_keys().unwrap(), keys);
    let pass = |count: u64| {
        let start = Instant::now();
        for i in 0..count {
            let code = (i * 4051) % keys;
            let value = reader
                .get::<T>(&input_keys[code as usize])
                .unwrap()
                .unwrap();
            let expected = latest(code);
            value.check(code, expected, payload);
            black_box(value);
        }
        start.elapsed().as_secs_f64() * 1000.0
    };
    let first_pass_ms = pass(keys);
    let warm_query_ms = pass(queries);
    let start = Instant::now();
    let mut count = 0;
    for (i, value) in reader.blocks::<T>().unwrap().enumerate() {
        value.unwrap().check(code_at(i as u64), i as u64, payload);
        count += 1;
    }
    assert_eq!(count, records);
    let scan_ms = start.elapsed().as_secs_f64() * 1000.0;
    let start = Instant::now();
    let compact = writer.compact_index().unwrap();
    let compact_ms = start.elapsed().as_secs_f64() * 1000.0;
    reader.follow().unwrap();
    assert_eq!(reader.historical_distinct_keys().unwrap(), keys);
    for code in 0..keys {
        reader
            .get::<T>(&input_keys[code as usize])
            .unwrap()
            .unwrap()
            .check(code, latest(code), payload);
    }
    let start = Instant::now();
    for i in 0..1_000_000 {
        black_box(T::input_key(black_box(i % keys)));
    }
    let input_conversion_ms = start.elapsed().as_secs_f64() * 1000.0;
    println!(
        "FINITE_BENCH {{\"mode\":\"{mode}\",\"keys\":{keys},\"records\":{records},\"payload\":{payload},\"sync_records\":{sync_records},\"queries\":{queries},\"write_ms\":{write_ms},\"sync_ms\":{sync_ms},\"open_median_ms\":{},\"first_pass_ms\":{first_pass_ms},\"warm_query_ms\":{warm_query_ms},\"scan_ms\":{scan_ms},\"compact_ms\":{compact_ms},\"input_conversion_ms\":{input_conversion_ms},\"native_bytes\":{native_bytes},\"allocated_native_bytes\":{allocated_bytes},\"index_bytes\":{index_bytes},\"compacted_index_bytes\":{}}}",
        opens[15], compact.after_bytes
    );
    drop(reader);
    drop(writer);
    dir.close().unwrap();
}
#[test]
#[ignore = "controlled I/O benchmark; use scripts/compare_finite_keys.py"]
fn compare_finite_representation() {
    let mode = std::env::var("VARVE_BENCH_MODE").unwrap();
    match mode.as_str() {
        "raw_numeric" => measure::<raw_numeric::Record>(&mode),
        "finite_numeric" => measure::<finite_numeric::Record>(&mode),
        "raw_label" => measure::<raw_label::Record>(&mode),
        "finite_label" => measure::<finite_label::Record>(&mode),
        _ => panic!("unknown mode"),
    }
}
