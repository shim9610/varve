//! Real-byte load study; driven by scripts/load_scalable.py. No library changes.
//! Every returned payload is compared in full against an independent oracle.
use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use varve::{
    BatchOptions, BlockDescriptor, BlockKind, DiskIndexBatchOptions, DiskIndexOptions,
    DiskIndexPlan, DiskIndexedBlock, Endian, FormatSpec, IndexPolicy, IntegrityPolicy,
    ManifestPolicy, ReadLimits, RecoveryPolicy, StreamOptions, VarveBlock, VarveIndexedReader,
    VarveIndexedWriter, VarveStreamReader, VarveStreamWriter,
};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

#[derive(Clone, Debug, VarveBlock)]
#[varve(id = 1, version = 1, kind = "variable", key = "key")]
struct Record {
    key: u64,
    generation: u64,
    payload: Vec<u8>,
}

#[derive(Clone, Debug, VarveBlock)]
#[varve(id = 2, version = 1, kind = "variable")]
struct StreamRecord {
    key: u64,
    generation: u64,
    payload: Vec<u8>,
}

static BLOCKS: &[BlockDescriptor] = &[
    BlockDescriptor {
        id: 1,
        name: "Record",
        version: 1,
        kind: BlockKind::Variable,
        fields: &[],
    },
    BlockDescriptor {
        id: 2,
        name: "StreamRecord",
        version: 1,
        kind: BlockKind::Variable,
        fields: &[],
    },
];
static INDEXED: &[DiskIndexedBlock] = &[DiskIndexedBlock::of::<Record>()];

#[derive(Clone)]
struct Config {
    phase: String,
    mode: String,
    root: PathBuf,
    total: u64,
    payload: usize,
    epoch_bytes: u64,
    batch_bytes: usize,
    index_batch: usize,
    cache: usize,
    readers: usize,
    pinned_readers: usize,
    crc: bool,
    pause_us: u64,
    seconds: u64,
}

impl Config {
    fn parse() -> Result<Self> {
        let args: Vec<_> = std::env::args().skip(1).collect();
        if !args.len().is_multiple_of(2) {
            return Err("arguments must be --name value pairs".into());
        }
        let mut options: BTreeMap<_, _> = args
            .chunks(2)
            .map(|a| (a[0].clone(), a[1].clone()))
            .collect();
        let mut get =
            |name: &str, default: &str| options.remove(name).unwrap_or_else(|| default.into());
        let config = Self {
            phase: get("--phase", "write"),
            mode: get("--mode", "indexed"),
            root: PathBuf::from(get("--root", "")),
            total: get("--total-bytes", "21474836480").parse()?,
            payload: get("--record-bytes", "65536").parse()?,
            epoch_bytes: get("--epoch-bytes", "268435456").parse()?,
            batch_bytes: get("--batch-bytes", "4194304").parse()?,
            index_batch: get("--index-batch-records", "16384").parse()?,
            cache: get("--cache-bytes", "8388608").parse()?,
            readers: get("--readers", "0").parse()?,
            pinned_readers: get("--pinned-readers", "1").parse()?,
            crc: get("--crc", "true").parse()?,
            pause_us: get("--reader-pause-us", "0").parse()?,
            seconds: get("--seconds", "10").parse()?,
        };
        if !options.is_empty() {
            return Err(format!("unknown options: {options:?}").into());
        }
        if config.root.as_os_str().is_empty()
            || config.payload < 16
            || config.payload > 8 * 1024 * 1024
            || config.epoch_bytes == 0
            || !config.epoch_bytes.is_multiple_of(config.payload as u64)
            || !config.total.is_multiple_of(config.epoch_bytes)
            || config.epochs() < 2
            || !config.epochs().is_multiple_of(2)
            || config.batch_bytes == 0
            || config.readers > 64
            || config.pinned_readers > 64
        {
            return Err(
                "invalid size/topology (need even epochs and exact record alignment)".into(),
            );
        }
        if !["raw", "stream", "indexed"].contains(&config.mode.as_str()) {
            return Err("unknown mode".into());
        }
        Ok(config)
    }
    fn path(&self) -> PathBuf {
        self.root.join("data.varve")
    }
    fn epochs(&self) -> u64 {
        self.total / self.epoch_bytes
    }
    fn per_epoch(&self) -> u64 {
        self.epoch_bytes / self.payload as u64
    }
    fn base_epochs(&self) -> u64 {
        self.epochs() / 2
    }
    fn keys(&self) -> u64 {
        self.base_epochs() * self.per_epoch()
    }
    fn visible_keys(&self, epoch: u64) -> u64 {
        ((epoch + 1) * self.per_epoch()).min(self.keys())
    }
    fn key_at(&self, ordinal: u64) -> u64 {
        ordinal % self.keys()
    }
    fn expected(&self, key: u64, epoch: u64) -> Option<u64> {
        if key >= self.visible_keys(epoch) {
            return None;
        }
        let initial = key / self.per_epoch();
        let updated = initial + self.base_epochs();
        if updated <= epoch {
            if key.is_multiple_of(97) {
                None
            } else {
                Some(updated)
            }
        } else {
            Some(initial)
        }
    }
    fn spec(&self) -> FormatSpec {
        FormatSpec::new(
            b"VLOAD01",
            1,
            Endian::Little,
            0,
            if self.mode == "stream" {
                IndexPolicy::BlockOffsetChain
            } else {
                IndexPolicy::KeyedOffsetChain
            },
            if self.crc {
                IntegrityPolicy::Crc32
            } else {
                IntegrityPolicy::None
            },
            RecoveryPolicy::Strict,
            ManifestPolicy::None,
            BLOCKS,
        )
        .with_read_limits(ReadLimits::STANDARD.with_max_sidecar_len(4 * 1024 * 1024 * 1024))
    }
    fn plan(&self) -> DiskIndexPlan {
        DiskIndexPlan::canonical(self.spec(), INDEXED).unwrap()
    }
    fn options(&self) -> DiskIndexOptions {
        DiskIndexOptions {
            cache_bytes: self.cache,
            batch: DiskIndexBatchOptions {
                max_records: self.index_batch,
                max_bytes: 4 * 1024 * 1024,
            },
            ..DiskIndexOptions::default()
        }
    }
    fn stream_options(&self) -> StreamOptions {
        StreamOptions {
            state: self.options(),
            ..StreamOptions::default()
        }
    }
    fn batch(&self) -> BatchOptions {
        BatchOptions {
            max_bytes: self.batch_bytes,
            max_records: 16_384,
        }
    }
}

fn random(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9e37_79b9_7f4a_7c15);
    let mut x = *state;
    x = (x ^ (x >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    x ^ (x >> 31)
}

struct Payloads {
    templates: Vec<Vec<u8>>,
}
impl Payloads {
    fn new(length: usize) -> Self {
        let mut state = 17;
        let templates = (0..64)
            .map(|_| {
                let mut bytes = vec![0u8; length];
                for chunk in bytes.chunks_mut(8) {
                    let value = random(&mut state).to_le_bytes();
                    chunk.copy_from_slice(&value[..chunk.len()]);
                }
                bytes
            })
            .collect();
        Self { templates }
    }
    fn template(&self, key: u64, generation: u64) -> &[u8] {
        &self.templates[((key.wrapping_mul(17) ^ generation) % 64) as usize]
    }
    fn record(&self, key: u64, generation: u64) -> Record {
        let mut payload = self.template(key, generation).to_vec();
        payload[..8].copy_from_slice(&key.to_le_bytes());
        payload[8..16].copy_from_slice(&generation.to_le_bytes());
        Record {
            key,
            generation,
            payload,
        }
    }
    fn check(&self, key: u64, generation: u64, payload: &[u8]) -> Result<()> {
        let template = self.template(key, generation);
        if payload.len() != template.len()
            || payload[..8] != key.to_le_bytes()
            || payload[8..16] != generation.to_le_bytes()
            || payload[16..] != template[16..]
        {
            return Err(format!("payload mismatch: key={key} generation={generation}").into());
        }
        Ok(())
    }
}

// Bounded logarithmic latency histogram: reported quantiles are bucket upper
// bounds (<= 12.5% width), not falsely precise sampled percentiles.
struct Latencies {
    buckets: [u64; 512],
    count: u64,
    total: u128,
    max: u64,
}
impl Default for Latencies {
    fn default() -> Self {
        Self {
            buckets: [0; 512],
            count: 0,
            total: 0,
            max: 0,
        }
    }
}
impl Latencies {
    fn observe(&mut self, elapsed: Duration) {
        let ns = elapsed.as_nanos().min(u128::from(u64::MAX)) as u64;
        let power = 63 - ns.max(1).leading_zeros() as usize;
        let sub = ((ns.max(1) - (1u64 << power)) >> power.saturating_sub(3)).min(7) as usize;
        self.buckets[power * 8 + sub] += 1;
        self.count += 1;
        self.total += u128::from(ns);
        self.max = self.max.max(ns);
    }
    fn percentile(&self, percent: u64) -> u64 {
        if self.count == 0 {
            return 0;
        }
        let target = (self.count * percent).div_ceil(100);
        let mut seen = 0;
        for (index, count) in self.buckets.iter().enumerate() {
            seen += count;
            if seen >= target {
                let power = index / 8;
                return (1u64 << power)
                    .saturating_add(((index % 8 + 1) as u64) << power.saturating_sub(3));
            }
        }
        self.max
    }
    fn json(&self) -> String {
        format!(
            "{{\"count\":{},\"total_ns\":{},\"p50_upper_ns\":{},\"p95_upper_ns\":{},\"p99_upper_ns\":{},\"max_ns\":{},\"histogram_buckets\":{:?}}}",
            self.count,
            self.total,
            self.percentile(50),
            self.percentile(95),
            self.percentile(99),
            self.max,
            self.buckets
        )
    }
}

fn check_point(
    reader: &VarveIndexedReader,
    config: &Config,
    bank: &Payloads,
    key: u64,
    epoch: u64,
) -> Result<Duration> {
    let started = Instant::now();
    let actual = reader.get::<Record>(&key)?;
    let elapsed = started.elapsed();
    match (config.expected(key, epoch), actual) {
        (None, None) => Ok(()),
        (Some(generation), Some(record))
            if record.key == key && record.generation == generation =>
        {
            bank.check(key, generation, &record.payload)
        }
        (expected, actual) => Err(format!(
            "snapshot mismatch key={key} snapshot={epoch} expected={expected:?} actual={:?}",
            actual.map(|r| (r.key, r.generation))
        )
        .into()),
    }?;
    Ok(elapsed)
}

struct Job {
    reader: VarveIndexedReader,
    epoch: u64,
}

fn indexed_worker(
    id: usize,
    mut job: Job,
    receive: mpsc::Receiver<Job>,
    stop: &AtomicBool,
    config: &Config,
    bank: &Payloads,
) -> Result<()> {
    let mut rng = id as u64 + 123;
    let mut times = Latencies::default();
    let mut snapshots = 1;
    let started = Instant::now();
    while !stop.load(Ordering::Acquire) {
        while let Ok(next) = receive.try_recv() {
            job = next;
            snapshots += 1;
        }
        let key = random(&mut rng) % (config.visible_keys(job.epoch) + 32);
        let elapsed = check_point(&job.reader, config, bank, key, job.epoch)?;
        times.observe(elapsed);
        if config.pause_us > 0 {
            std::thread::sleep(Duration::from_micros(config.pause_us));
        }
    }
    println!(
        "{{\"event\":\"reader\",\"id\":{id},\"snapshots\":{snapshots},\"seconds\":{},\"latency\":{}}}",
        started.elapsed().as_secs_f64(),
        times.json()
    );
    Ok(())
}

fn stream_worker(
    id: usize,
    reader: VarveStreamReader,
    stop: &AtomicBool,
    config: &Config,
    bank: &Payloads,
) -> Result<()> {
    let mut times = Latencies::default();
    let mut scans = 0;
    while !stop.load(Ordering::Acquire) {
        let mut ordinal = 0;
        for value in reader.blocks::<StreamRecord>()? {
            if stop.load(Ordering::Acquire) {
                break;
            }
            let now = Instant::now();
            let record = value?;
            if record.key != u64::MAX {
                if record.key != config.key_at(ordinal)
                    || record.generation != ordinal / config.per_epoch()
                {
                    return Err("stream snapshot order mismatch".into());
                }
                bank.check(record.key, record.generation, &record.payload)?;
                ordinal += 1;
                times.observe(now.elapsed());
            }
        }
        scans += 1;
    }
    println!(
        "{{\"event\":\"stream_reader\",\"id\":{id},\"scans_started\":{scans},\"validation_latency\":{}}}",
        times.json()
    );
    Ok(())
}

struct StopOnDrop<'a>(&'a AtomicBool);
impl Drop for StopOnDrop<'_> {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

enum Writer {
    Raw(File),
    Stream(Box<VarveStreamWriter>),
    Indexed(Box<VarveIndexedWriter>),
}
impl Writer {
    fn new(config: &Config) -> Result<Self> {
        if config.path().exists() {
            return Err("refusing to overwrite an existing load file".into());
        }
        Ok(match config.mode.as_str() {
            "raw" => Self::Raw(
                OpenOptions::new()
                    .create_new(true)
                    .write(true)
                    .open(config.path())?,
            ),
            "stream" => Self::Stream(Box::new(VarveStreamWriter::create(
                config.spec(),
                config.path(),
                config.stream_options(),
            )?)),
            _ => Self::Indexed(Box::new(VarveIndexedWriter::create(
                config.spec(),
                config.path(),
                config.options(),
                config.plan(),
            )?)),
        })
    }
    fn append(&mut self, config: &Config, bank: &Payloads, epoch: u64) -> Result<u64> {
        let records = (0..config.per_epoch()).map(|n| {
            bank.record(
                (epoch % config.base_epochs()) * config.per_epoch() + n,
                epoch,
            )
        });
        Ok(match self {
            Self::Raw(file) => {
                let mut buffer = Vec::with_capacity(config.batch_bytes.max(config.payload));
                let mut writes = 0;
                for record in records {
                    buffer.extend_from_slice(&record.payload);
                    if buffer.len() >= config.batch_bytes {
                        file.write_all(&buffer)?;
                        buffer.clear();
                        writes += 1;
                    }
                }
                if !buffer.is_empty() {
                    file.write_all(&buffer)?;
                    writes += 1;
                }
                writes
            }
            Self::Stream(writer) => {
                writer
                    .push_iter::<StreamRecord, _>(
                        records.map(|r| StreamRecord {
                            key: r.key,
                            generation: r.generation,
                            payload: r.payload,
                        }),
                        config.batch(),
                    )
                    .map_err(|e| e.source)?
                    .write_calls
            }
            Self::Indexed(writer) => {
                writer
                    .push_iter::<Record, _>(records, config.batch())
                    .map_err(|e| e.source)?
                    .write_calls
            }
        })
    }
    fn finish_epoch(&mut self, config: &Config, epoch: u64) -> Result<u64> {
        let mut deletes = 0;
        match self {
            Self::Indexed(writer) => {
                if epoch >= config.base_epochs() {
                    let start = (epoch % config.base_epochs()) * config.per_epoch();
                    for key in start..start + config.per_epoch() {
                        if key.is_multiple_of(97) {
                            writer.delete_info::<Record>(&key)?;
                            deletes += 1;
                        }
                    }
                }
                writer.push_info(&Record {
                    key: u64::MAX,
                    generation: epoch,
                    payload: Vec::new(),
                })?;
            }
            Self::Stream(writer) => {
                writer.push_info(&StreamRecord {
                    key: u64::MAX,
                    generation: epoch,
                    payload: Vec::new(),
                })?;
            }
            Self::Raw(_) => {}
        }
        Ok(deletes)
    }
    fn sync(&mut self) -> Result<()> {
        match self {
            Self::Raw(file) => file.sync_all()?,
            Self::Stream(w) => w.sync()?,
            Self::Indexed(w) => w.sync()?,
        }
        Ok(())
    }
}

fn indexed_reader(config: &Config) -> Result<VarveIndexedReader> {
    Ok(VarveIndexedReader::open(
        config.spec(),
        config.path(),
        config.options(),
        config.plan(),
    )?)
}

fn write(config: &Config, bank: &Payloads) -> Result<()> {
    let mut writer = Writer::new(config)?;
    let stop = AtomicBool::new(false);
    let started = Instant::now();
    std::thread::scope(|scope| -> Result<()> {
        let _stop_on_error = StopOnDrop(&stop);
        let mut workers = Vec::new();
        let mut senders = Vec::new();
        let mut append_latency = Latencies::default();
        let mut sync_latency = Latencies::default();
        for epoch in 0..config.epochs() {
            let append = Instant::now();
            let calls = writer.append(config, bank, epoch)?;
            let append_time = append.elapsed();
            append_latency.observe(append_time);
            let mutation = Instant::now();
            let deletes = writer.finish_epoch(config, epoch)?;
            let mutation_time = mutation.elapsed();
            let sync = Instant::now();
            writer.sync()?;
            let sync_time = sync.elapsed();
            sync_latency.observe(sync_time);
            let handoff = Instant::now();
            if epoch + 1 == config.base_epochs() && config.readers > 0 {
                if config.mode == "indexed" {
                    // One pinned reader plus N readers refreshed at publication
                    // boundaries. A separate churn phase tests unsynchronized opens.
                    for id in 0..config.readers + config.pinned_readers {
                        let job = Job {
                            reader: indexed_reader(config)?,
                            epoch,
                        };
                        let (send, receive) = mpsc::sync_channel(1);
                        let stop = &stop;
                        workers.push(
                            scope.spawn(move || {
                                indexed_worker(id, job, receive, stop, config, bank)
                            }),
                        );
                        if id >= config.pinned_readers {
                            senders.push(send);
                        }
                    }
                } else if config.mode == "stream" {
                    for id in 0..config.readers {
                        let reader = VarveStreamReader::open(
                            config.spec(),
                            config.path(),
                            config.stream_options(),
                        )?;
                        let stop = &stop;
                        workers.push(
                            scope.spawn(move || stream_worker(id, reader, stop, config, bank)),
                        );
                    }
                }
            } else if epoch >= config.base_epochs() {
                for send in &senders {
                    match send.try_send(Job {
                        reader: indexed_reader(config)?,
                        epoch,
                    }) {
                        Ok(()) | Err(mpsc::TrySendError::Full(_)) => {}
                        Err(mpsc::TrySendError::Disconnected(_)) => {
                            return Err("reader worker failed".into());
                        }
                    }
                }
            }
            let native = fs::metadata(config.path())?.len();
            let sidecar = [".vki", ".vks"]
                .into_iter()
                .map(|suffix| {
                    fs::metadata(companion(&config.path(), suffix)).map_or(0, |m| m.len())
                })
                .sum::<u64>();
            println!(
                "{{\"event\":\"epoch\",\"epoch\":{epoch},\"elapsed_seconds\":{},\"payload_bytes\":{},\"native_bytes\":{native},\"sidecar_bytes\":{sidecar},\"append_ns\":{},\"mutation_ns\":{},\"sync_ns\":{},\"handoff_ns\":{},\"write_calls\":{calls},\"deletes\":{deletes}}}",
                started.elapsed().as_secs_f64(),
                config.epoch_bytes,
                append_time.as_nanos(),
                mutation_time.as_nanos(),
                sync_time.as_nanos(),
                handoff.elapsed().as_nanos()
            );
        }
        stop.store(true, Ordering::Release);
        drop(senders);
        for worker in workers {
            worker.join().map_err(|_| "reader thread panicked")??;
        }
        println!(
            "{{\"event\":\"write_complete\",\"payload_bytes\":{},\"seconds\":{},\"append_latency\":{},\"sync_latency\":{}}}",
            config.total,
            started.elapsed().as_secs_f64(),
            append_latency.json(),
            sync_latency.json()
        );
        Ok(())
    })
}

fn companion(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(suffix);
    name.into()
}

fn verify(config: &Config, bank: &Payloads) -> Result<()> {
    let started = Instant::now();
    let mut records = 0;
    let mut markers = 0;
    let mut check = |record: Record| -> Result<()> {
        if record.key == u64::MAX {
            if records != (markers + 1) * config.per_epoch()
                || record.generation != markers
                || !record.payload.is_empty()
            {
                return Err(
                    format!("invalid publication marker {markers} at record {records}").into(),
                );
            }
            markers += 1;
        } else {
            if record.key != config.key_at(records)
                || record.generation != records / config.per_epoch()
            {
                return Err(format!("out-of-order physical record {records}").into());
            }
            bank.check(record.key, record.generation, &record.payload)?;
            records += 1;
        }
        Ok(())
    };
    match config.mode.as_str() {
        "raw" => {
            let mut file = File::open(config.path())?;
            let mut buffer = vec![0; config.payload];
            for ordinal in 0..config.total / config.payload as u64 {
                file.read_exact(&mut buffer)?;
                bank.check(
                    config.key_at(ordinal),
                    ordinal / config.per_epoch(),
                    &buffer,
                )?;
                records += 1;
            }
            if file.read(&mut [0])? != 0 {
                return Err("unexpected raw tail".into());
            }
        }
        "stream" => {
            let reader =
                VarveStreamReader::open(config.spec(), config.path(), config.stream_options())?;
            for value in reader.blocks::<StreamRecord>()? {
                let r = value?;
                check(Record {
                    key: r.key,
                    generation: r.generation,
                    payload: r.payload,
                })?;
            }
        }
        _ => {
            for value in indexed_reader(config)?.blocks::<Record>()? {
                check(value?)?;
            }
        }
    }
    if records != config.total / config.payload as u64
        || (config.mode != "raw" && markers != config.epochs())
    {
        return Err(format!("scan count mismatch records={records} markers={markers}").into());
    }
    println!(
        "{{\"event\":\"verify_complete\",\"records\":{records},\"markers\":{markers},\"payload_bytes\":{},\"seconds\":{}}}",
        config.total,
        started.elapsed().as_secs_f64()
    );
    Ok(())
}

fn point(config: &Config, bank: &Payloads) -> Result<()> {
    let open = Instant::now();
    let reader = indexed_reader(config)?;
    let open_ns = open.elapsed().as_nanos();
    let started = Instant::now();
    let mut times = Latencies::default();
    for key in 0..=config.keys() {
        let elapsed = check_point(&reader, config, bank, key, config.epochs() - 1)?;
        times.observe(elapsed);
    }
    println!(
        "{{\"event\":\"point_complete\",\"keys_checked\":{},\"open_ns\":{open_ns},\"seconds\":{},\"latency\":{}}}",
        config.keys() + 1,
        started.elapsed().as_secs_f64(),
        times.json()
    );
    Ok(())
}

fn random_read(config: &Config, bank: &Payloads) -> Result<()> {
    let stop = AtomicBool::new(false);
    std::thread::scope(|scope| -> Result<()> {
        let _stop_on_error = StopOnDrop(&stop);
        let mut workers = Vec::new();
        for id in 0..config.readers.max(1) {
            let job = Job {
                reader: indexed_reader(config)?,
                epoch: config.epochs() - 1,
            };
            let (_, receive) = mpsc::channel();
            let stop = &stop;
            workers.push(scope.spawn(move || indexed_worker(id, job, receive, stop, config, bank)));
        }
        std::thread::sleep(Duration::from_secs(config.seconds));
        stop.store(true, Ordering::Release);
        for worker in workers {
            worker.join().map_err(|_| "random reader panicked")??;
        }
        println!(
            "{{\"event\":\"random_complete\",\"seconds\":{}}}",
            config.seconds
        );
        Ok(())
    })
}

fn probe(config: &Config) -> Result<()> {
    match indexed_reader(config) {
        Ok(reader) => {
            let marker = reader
                .get::<Record>(&u64::MAX)?
                .ok_or("missing publication marker")?;
            println!(
                "{{\"event\":\"process_probe\",\"status\":\"opened\",\"epoch\":{}}}",
                marker.generation
            );
        }
        Err(error) => {
            let message = error.to_string();
            println!("{{\"event\":\"process_probe\",\"status\":\"refused\",\"error\":{message:?}}}")
        }
    }
    Ok(())
}

fn probe_child(holder: &str) -> Result<()> {
    // Child process exercises the actual cross-process lock boundary; no lock
    // is removed or bypassed. It has the same command arguments except phase.
    let mut args: Vec<_> = std::env::args().skip(1).collect();
    if let Some(index) = args.iter().position(|a| a == "--phase") {
        args[index + 1] = "probe".into();
    }
    let output = std::process::Command::new(std::env::current_exe()?)
        .args(args)
        .output()?;
    println!("{{\"event\":\"process_probe_context\",\"holder\":{holder:?}}}");
    print!("{}", String::from_utf8_lossy(&output.stdout));
    if !output.status.success() {
        return Err("cross-process probe child failed".into());
    }
    Ok(())
}

fn churn(config: &Config, bank: &Payloads) -> Result<()> {
    let mut writer = VarveIndexedWriter::open(
        config.spec(),
        config.path(),
        config.options(),
        config.plan(),
    )?;
    probe_child("writer")?;
    let base_generation = indexed_reader(config)?
        .get::<Record>(&u64::MAX)?
        .ok_or("load file has no confirmed marker")?
        .generation;
    let final_generation = base_generation
        .checked_add(1000)
        .ok_or("marker generation overflow")?;
    let stop = AtomicBool::new(false);
    let mut writer_error = None;
    let mut failed_operation = "none";
    let mut acknowledged = 0u64;
    let original_eof = fs::metadata(config.path())?.len();
    std::thread::scope(|scope| -> Result<()> {
        let _stop_on_error = StopOnDrop(&stop);
        let workers: Vec<_> = (0..config.readers.max(8))
            .map(|_| {
                let stop = &stop;
                scope.spawn(move || -> Result<BTreeMap<String, u64>> {
                    let mut counts = BTreeMap::<String, u64>::new();
                    let mut retained = indexed_reader(config)?;
                    let mut previous = base_generation;
                    loop {
                        let done = stop.load(Ordering::Acquire);
                        let fresh = indexed_reader(config)?;
                        *counts.entry("opened".into()).or_default() += 1;
                        check_point(&fresh, config, bank, 1, config.epochs() - 1)?;
                        retained.follow()?;
                        *counts.entry("followed".into()).or_default() += 1;
                        let generation = retained
                            .get::<Record>(&u64::MAX)?
                            .map_or(0, |r| r.generation);
                        if generation < previous || generation > final_generation {
                            return Err(
                                "following reader observed invalid marker generation".into()
                            );
                        }
                        check_point(
                            &retained,
                            config,
                            bank,
                            config.keys() - 1,
                            config.epochs() - 1,
                        )?;
                        previous = generation;
                        if done {
                            if generation != final_generation {
                                return Err(
                                    "following reader missed the final confirmed marker".into()
                                );
                            }
                            break;
                        }
                    }
                    Ok(counts)
                })
            })
            .collect();
        for attempt in 0..1000 {
            let result = match writer.push_info(&Record {
                key: u64::MAX,
                generation: base_generation + attempt + 1,
                payload: Vec::new(),
            }) {
                Ok(_) => {
                    failed_operation = "sync";
                    writer.sync()
                }
                Err(error) => {
                    failed_operation = "push";
                    Err(error)
                }
            };
            if let Err(error) = result {
                writer_error = Some(format!("attempt={attempt}: {error}"));
                break;
            }
            acknowledged += 1;
        }
        stop.store(true, Ordering::Release);
        for worker in workers {
            for (status, count) in worker.join().map_err(|_| "churn reader panicked")?? {
                println!("{{\"event\":\"open_result\",\"status\":{status:?},\"count\":{count}}}");
            }
        }
        Ok(())
    })?;
    drop(writer);
    if let Some(error) = writer_error {
        let native_eof = fs::metadata(config.path())?.len();
        println!(
            "{{\"event\":\"contract_failure\",\"operation\":{failed_operation:?},\"acknowledged_markers\":{acknowledged},\"native_before\":{original_eof},\"native_after\":{native_eof},\"error\":{error:?}}}"
        );
        // Preserve the failed state. The driver runs recovery explicitly.
        return Err(error.into());
    }
    println!("{{\"event\":\"churn_complete\"}}");
    Ok(())
}

fn main() {
    if let Err(error) = run() {
        eprintln!("LOAD_FAILURE: {error}");
        std::process::exit(1);
    }
}
fn run() -> Result<()> {
    let config = Config::parse()?;
    let bank = Payloads::new(config.payload);
    match config.phase.as_str() {
        "write" => write(&config, &bank),
        "verify" => verify(&config, &bank),
        "point" => point(&config, &bank),
        "compact" => {
            let mut writer = VarveIndexedWriter::open(
                config.spec(),
                config.path(),
                config.options(),
                config.plan(),
            )?;
            let start = Instant::now();
            let report = writer.compact_index()?;
            println!(
                "{{\"event\":\"compact_complete\",\"before_bytes\":{},\"after_bytes\":{},\"keys\":{},\"seconds\":{}}}",
                report.before_bytes,
                report.after_bytes,
                report.historical_distinct_keys,
                start.elapsed().as_secs_f64()
            );
            Ok(())
        }
        "random" => random_read(&config, &bank),
        "churn" => churn(&config, &bank),
        "probe" => probe(&config),
        "cross-read" => {
            let _reader = indexed_reader(&config)?;
            probe_child("reader")?;
            println!("{{\"event\":\"cross_read_complete\"}}");
            Ok(())
        }
        "rebuild" => {
            let started = Instant::now();
            let result = varve::rebuild_disk_index(
                config.spec(),
                config.path(),
                config.options(),
                config.plan(),
            )?;
            println!(
                "{{\"event\":\"rebuild_complete\",\"records\":{},\"seconds\":{}}}",
                result.records,
                started.elapsed().as_secs_f64()
            );
            Ok(())
        }
        "restore" => {
            match VarveIndexedWriter::open(
                config.spec(),
                config.path(),
                config.options(),
                config.plan(),
            ) {
                Ok(_) => println!("{{\"event\":\"restore_complete\",\"needed\":false}}"),
                Err(_) => {
                    drop(VarveIndexedWriter::restore_checkpoint_and_open(
                        config.spec(),
                        config.path(),
                        config.options(),
                        config.plan(),
                    )?);
                    println!("{{\"event\":\"restore_complete\",\"needed\":true}}");
                }
            }
            Ok(())
        }
        _ => Err("unknown phase".into()),
    }
}
