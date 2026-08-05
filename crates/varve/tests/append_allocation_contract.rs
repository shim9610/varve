// F-52/F-04 — the payload copy every append paid, on every default spec.
//
// `prepare_user_record_payload` returned an owned `StoredPayload { bytes:
// Vec<u8> }`, and `uncompressed_user_payload` filled it with
// `logical_payload.to_vec()`. Four routes reach that function and all four are
// uncompressed: no compression declared for the block (the default), a
// non-`Variable` block kind, a payload below `min_uncompressed_len`, and
// `only_if_smaller` losing. Every one of them allocated a payload-sized buffer,
// memcpy'd into it, handed `&payload.bytes` straight to the record writer and
// freed it — one allocation, one copy and one free per record, proportional to
// the record.
//
// The fix makes `StoredPayload.bytes` a `Cow<'_, [u8]>` so those four routes
// borrow. What is measured here is the allocator, not the clock: a counting
// `#[global_allocator]` in this test binary (the library is untouched — a test
// binary owns its own).
//
// The discriminator is the TWO payload sizes. A "fix" that staged into a reused
// scratch buffer would hold the allocation count flat and keep the memcpy, so
// the payload-sized allocation count would not move; a fix that only borrowed
// in the no-compression-declared branch would still allocate on the second
// spec. Both are caught by counting allocations at least as large as the
// payload, per push, at each size and on each spec.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::path::{Path, PathBuf};

use varve::varve_format;

struct CountingAllocator;

std::thread_local! {
    /// Allocations on this thread inside the measurement window.
    static ALLOCATIONS: Cell<u64> = const { Cell::new(0) };
    /// Bytes requested by those allocations.
    static ALLOCATED_BYTES: Cell<u64> = const { Cell::new(0) };
    /// Of those, the ones at least `LARGE_THRESHOLD` bytes wide — the ones that
    /// can only be payload-proportional.
    static LARGE_ALLOCATIONS: Cell<u64> = const { Cell::new(0) };
    /// Whether this thread is inside a measurement window at all. `cargo test`
    /// runs test functions in parallel and they all share this allocator, so
    /// the counters are thread-local and armed explicitly.
    static MEASURING: Cell<bool> = const { Cell::new(false) };
}

/// Wide enough that no fixed-size bookkeeping allocation reaches it, and well
/// under the large payload size below.
const LARGE_THRESHOLD: usize = 64 * 1024;

unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let pointer = unsafe { System.alloc(layout) };
        if !pointer.is_null() {
            note(layout.size());
        }
        pointer
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        unsafe { System.dealloc(pointer, layout) };
    }

    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let replacement = unsafe { System.realloc(pointer, layout, new_size) };
        if !replacement.is_null() && new_size > layout.size() {
            note(new_size - layout.size());
        }
        replacement
    }
}

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

/// Records one allocation, if this thread asked to be measured.
///
/// `try_with` rather than `with`: a thread whose TLS is already being destroyed
/// still allocates, and panicking in the allocator would abort the process.
fn note(size: usize) {
    if MEASURING.try_with(Cell::get) != Ok(true) {
        return;
    }
    let _ = ALLOCATIONS.try_with(|count| count.set(count.get() + 1));
    let _ = ALLOCATED_BYTES.try_with(|bytes| bytes.set(bytes.get() + size as u64));
    if size >= LARGE_THRESHOLD {
        let _ = LARGE_ALLOCATIONS.try_with(|count| count.set(count.get() + 1));
    }
}

#[derive(Debug, Clone, Copy)]
struct Counts {
    allocations: u64,
    bytes: u64,
    large: u64,
}

/// Runs `body` with this thread's allocation counters armed.
fn measure(body: impl FnOnce()) -> Counts {
    ALLOCATIONS.with(|count| count.set(0));
    ALLOCATED_BYTES.with(|bytes| bytes.set(0));
    LARGE_ALLOCATIONS.with(|count| count.set(0));
    MEASURING.with(|flag| flag.set(true));
    body();
    MEASURING.with(|flag| flag.set(false));
    Counts {
        allocations: ALLOCATIONS.with(Cell::get),
        bytes: ALLOCATED_BYTES.with(Cell::get),
        large: LARGE_ALLOCATIONS.with(Cell::get),
    }
}

varve_format! {
    pub format PlainFormat {
        magic: b"ALLOCPLN";
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
        blocks {
            variable Payload(id = 1) {
                data: Vec<u8>,
            }
        }
    }
}

#[cfg(feature = "compression-zstd")]
varve_format! {
    pub format DeclaredCompressionFormat {
        magic: b"ALLOCZST";
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
        manifest: embedded;
        // Every payload below stays under `min_len`, so this spec declares
        // compression and takes the third uncompressed route. A fix that only
        // borrowed when no compression is declared still copies here.
        compression: variable_blocks(
            zstd,
            level = default,
            header = record_explicit,
            min_len = 4194304,
            only_if_smaller = true,
            max_len = 67108864,
        );
        blocks {
            variable CompressedPayload(id = 1) {
                data: Vec<u8>,
            }
        }
    }
}

// A spec that writes a record footer, which `PlainFormat` does not.
//
// `spec_needs_record_footer` is `commit_policy.requires_record_footer() ||
// index_policy.requires_record_footer()`, and a bare `varve_format!` asks for
// neither — which is why the footer allocation was invisible to every
// measurement in this file until this format existed. `block_offset_chain` is
// the cheapest clause that turns the footer on: it adds the back-pointer the
// footer carries and nothing else per record.
varve_format! {
    pub format FooterFormat {
        magic: b"ALLOCFTR";
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
        index: block_offset_chain;
        blocks {
            variable FooterPayload(id = 1) {
                data: Vec<u8>,
            }
        }
    }
}

const PUSHES: u64 = 1_000;
const SMALL: usize = 256;
const LARGE: usize = 256 * 1024;

/// Measured, on this tree, at 1,000 pushes of a 256 KiB payload:
///
/// |                      | allocations/push | bytes/push | >=64 KiB allocations/push |
/// | ---                  | ---              | ---        | ---                       |
/// | before (`to_vec`)    | 6.01             | 786,594    | 3.000                     |
/// | after (`Cow`)        | 5.01             | 524,426    | 2.000                     |
///
/// So: exactly one allocation per push disappears, and it takes exactly one
/// payload with it. Two payload-proportional allocations remain and are NOT
/// this fix — the macro's per-field `Vec` and `encode_to_vec_limited`'s buffer,
/// findings [5, 33, 54] and [45, 53], neither of which is touched here.
const MAX_PAYLOAD_COPIES: f64 = 2.0;

/// How many payloads' worth of bytes one push allocates, derived from the two
/// sizes rather than from either alone.
///
/// This is the discriminator the single-size form does not give. A "fix" that
/// staged into a reused scratch buffer holds `allocations/push` flat while
/// keeping the memcpy, and a fix that only borrowed in the
/// no-compression-declared branch still copies on the second spec; both leave
/// this ratio at 3.0.
fn payload_copies_per_push(small: Counts, large: Counts) -> f64 {
    let extra_bytes = (large.bytes - small.bytes) as f64 / PUSHES as f64;
    extra_bytes / (LARGE - SMALL) as f64
}

/// The default configuration: no compression declared anywhere. This is route
/// one of four, and it is what every `varve_format!` without a `compression:`
/// clause gets.
#[test]
fn a_default_append_copies_the_payload_no_more_than_the_encoder_does() {
    let small = plain_window(SMALL);
    let large = plain_window(LARGE);
    report("plain/small", SMALL, small);
    report("plain/large", LARGE, large);

    let copies = payload_copies_per_push(small, large);
    assert!(
        copies <= MAX_PAYLOAD_COPIES + 0.05,
        "each push allocated {copies:.3} payloads' worth of bytes; before this fix it was 3.0",
    );
    assert!(
        large.large <= MAX_PAYLOAD_COPIES as u64 * PUSHES,
        "each push made {} allocations of at least {LARGE_THRESHOLD} bytes; before this fix it \
         was 3 per push",
        large.large as f64 / PUSHES as f64,
    );
    // 5.01 after, 6.01 before. The ceiling is loose because the +0.01 is the
    // resident index's amortised growth, not a per-record cost.
    assert!(
        large.allocations <= 5 * PUSHES + PUSHES / 2,
        "allocations/push was {:.2}; before this fix it was 6.01",
        large.allocations as f64 / PUSHES as f64,
    );
    assert_eq!(
        small.large, 0,
        "a 256-byte payload must produce no large allocation at all",
    );
}

/// A spec that DOES declare compression, with every payload below
/// `min_uncompressed_len` — route three of four, and the one that catches a fix
/// applied only to the "no compression declared" branch.
#[cfg(feature = "compression-zstd")]
#[test]
fn a_declared_compression_spec_below_the_minimum_copies_no_more_either() {
    let small = declared_window(SMALL);
    let large = declared_window(LARGE);
    report("declared/small", SMALL, small);
    report("declared/large", LARGE, large);

    let copies = payload_copies_per_push(small, large);
    assert!(
        copies <= MAX_PAYLOAD_COPIES + 0.05,
        "declaring compression added a payload copy to a payload that is never compressed: \
         {copies:.3} payloads' worth per push",
    );
    assert!(large.large <= MAX_PAYLOAD_COPIES as u64 * PUSHES);
    assert!(large.allocations <= 5 * PUSHES + PUSHES / 2);
    assert_eq!(small.large, 0);
}

/// Byte identity. The whole point of the change is that it moves no byte: the
/// record writer receives the same `&[u8]` it always did.
///
/// The constant was measured on the UNFIXED tree and is pinned here, so a
/// change that altered the stream — an added envelope, a different length, a
/// dropped flag — fails this rather than being argued about.
#[test]
fn the_written_bytes_are_unchanged() -> varve::Result<()> {
    let path = temp_path("identity");
    {
        let mut file = PlainFormat::create(&path)?;
        for index in 0..64u32 {
            file.push(&Payload {
                data: vec![(index % 251) as u8; 64 + index as usize],
            })?;
        }
        file.flush()?;
    }
    let bytes = std::fs::read(&*path)?;
    assert_eq!(
        (bytes.len() as u64, fnv1a(&bytes)),
        (PLAIN_IDENTITY_LEN, PLAIN_IDENTITY_HASH),
        "the default configuration's byte stream moved",
    );
    Ok(())
}

/// Measured on the tree BEFORE the `Cow` change, with this exact push
/// sequence, and unchanged after it. That equality is the byte-identity proof:
/// the same numbers came out of both trees.
const PLAIN_IDENTITY_LEN: u64 = 9_722;
const PLAIN_IDENTITY_HASH: u64 = 4_053_647_348_546_472_655;

fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

fn plain_window(payload_len: usize) -> Counts {
    let path = temp_path(&format!("plain_{payload_len}"));
    let block = Payload {
        data: vec![0x5a; payload_len],
    };
    let mut file = PlainFormat::create(&path).expect("create");
    // Warm every lazily-built structure before the window opens.
    file.push(&block).expect("warm-up push");
    let counts = measure(|| {
        for _ in 0..PUSHES {
            file.push(&block).expect("push");
        }
    });
    file.flush().expect("flush");
    counts
}

fn footer_window(payload_len: usize) -> Counts {
    let path = temp_path(&format!("footer_{payload_len}"));
    let block = FooterPayload {
        data: vec![0x5a; payload_len],
    };
    let mut file = FooterFormat::create(&path).expect("create");
    file.push(&block).expect("warm-up push");
    let counts = measure(|| {
        for _ in 0..PUSHES {
            file.push(&block).expect("push");
        }
    });
    file.flush().expect("flush");
    counts
}

/// **The cost.** Writing a record footer allocates nothing per record.
///
/// The footer is `RECORD_FOOTER_LEN` bytes — 32, and the `const _: ()` in
/// `native_layout` makes the compiler prove it — yet `encode_native_record_footer`
/// built a `Vec` for it, wrote it, and dropped it, once per appended record.
/// The identical-size record *header* has always been encoded into a stack
/// array beside it.
///
/// **The cost.** Writing a record footer costs nothing per record — not one
/// allocation, not one byte.
///
/// The two specs run the same append path and differ only by
/// `index: block_offset_chain`, which is what turns the footer on, so the gap
/// between them *is* the footer. Measured on this tree at 1,000 pushes of a
/// 256-byte payload:
///
/// |                             | allocations/push | bytes/push |
/// | ---                         | ---              | ---        |
/// | `PlainFormat`, no footer    | 5.01             | 650.1      |
/// | `FooterFormat`, original    | 7.01             | 686.1      |
/// | after the stack-array footer| 6.01             | 654.1      |
/// | after the borrowed magic    | **5.01**         | **650.1**  |
///
/// Two allocations, in two places, for one 32-byte footer. The first was the
/// footer buffer itself, a `Vec` for a length the compiler proves is 32. The
/// second hid inside it: the 4-byte magic is a `&'static [u8]` in the field
/// table, and routing it through `LayoutValue::Bytes(Vec<u8>)` allocated a
/// heap buffer for that constant on every record.
///
/// **The middle row was published with the wrong explanation.** Its residual
/// 1.00 allocation and 4.0 bytes were recorded as "the block-offset chain's
/// own bookkeeping". They were the magic — 4.0 bytes per push is the magic's
/// width, and landing the borrowed form took the gap to zero. Equality is the
/// assertion now precisely because it is what the code earns; asserting it
/// against the middle row would have been wrong.
#[test]
fn writing_a_record_footer_allocates_nothing_per_record() {
    let plain = plain_window(SMALL);
    let footer = footer_window(SMALL);
    report("plain/small", SMALL, plain);
    report("footer/small", SMALL, footer);

    // Signed on purpose: an unsigned difference would underflow rather than
    // fail if the footer spec ever came in *under* the footerless one, and a
    // silent wrap reads as a pass.
    let extra_allocations = footer.allocations as i64 - plain.allocations as i64;
    let extra_bytes = footer.bytes as i64 - plain.bytes as i64;
    assert_eq!(
        extra_bytes, 0,
        "a footer record allocated {extra_bytes} bytes more than a footerless one over \
         {PUSHES} pushes; the footer's own buffer was 32 of them per push and the magic 4"
    );
    assert_eq!(
        extra_allocations, 0,
        "a footer record made {extra_allocations} allocations more than a footerless one over \
         {PUSHES} pushes; it was 2 per push before the footer moved to a stack array and the \
         magic stopped being copied"
    );
}

#[cfg(feature = "compression-zstd")]
fn declared_window(payload_len: usize) -> Counts {
    let path = temp_path(&format!("declared_{payload_len}"));
    let block = CompressedPayload {
        data: vec![0x5a; payload_len],
    };
    let mut file = DeclaredCompressionFormat::create(&path).expect("create");
    file.push(&block).expect("warm-up push");
    let counts = measure(|| {
        for _ in 0..PUSHES {
            file.push(&block).expect("push");
        }
    });
    file.flush().expect("flush");
    counts
}

/// Printed, not asserted, so a run records what it actually saw. `cargo test
/// -- --nocapture` shows it.
fn report(name: &str, payload_len: usize, counts: Counts) {
    println!(
        "{name}: payload={payload_len}B  allocations/push={:.2}  bytes/push={:.1}  \
         >={LARGE_THRESHOLD}B allocations/push={:.3}",
        counts.allocations as f64 / PUSHES as f64,
        counts.bytes as f64 / PUSHES as f64,
        counts.large as f64 / PUSHES as f64,
    );
}

struct TempPath {
    path: PathBuf,
    _dir: tempfile::TempDir,
}

impl std::ops::Deref for TempPath {
    type Target = Path;

    fn deref(&self) -> &Self::Target {
        &self.path
    }
}

impl AsRef<Path> for TempPath {
    fn as_ref(&self) -> &Path {
        &self.path
    }
}

fn temp_path(name: &str) -> TempPath {
    let dir = tempfile::tempdir().expect("create per-test temp directory");
    let path = dir
        .path()
        .join(format!("varve_alloc_{name}_{}.vrv", std::process::id()));
    TempPath { path, _dir: dir }
}
