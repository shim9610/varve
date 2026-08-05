//! What one `metadata(key)` lookup allocates.
//!
//! The walk over every metadata record is a correctness requirement — the
//! newest write for a key wins, so there is no early break — but decoding every
//! record is not. `read_payload_snapshot` already allocates one payload-sized
//! buffer per record; decoding it into `(String, Vec<u8>)` allocates a *second*
//! copy of the value, and for every record whose key does not match, that copy
//! is dropped unread.
//!
//! This measures allocation COUNT, not cumulative bytes: the payload buffer per
//! record stays, so the saving is one payload-sized allocation per non-matching
//! record (two → one), not a flat total.
//!
//! One test in its own binary, because the global allocator here counts for the
//! whole process.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::path::{Path, PathBuf};

use varve::{VarveBlock, varve_format};

/// Only allocations at least this large are counted, so the measurement sees
/// payload buffers and decoded values and not the noise around them.
const LARGE: usize = 2048;
const VALUE_BYTES: usize = 4096;
const ENTRIES: u8 = 16;

struct CountingAllocator;

std::thread_local! {
    static COUNTING: Cell<bool> = const { Cell::new(false) };
    static LARGE_ALLOCATIONS: Cell<usize> = const { Cell::new(0) };
    static LARGE_BYTES: Cell<usize> = const { Cell::new(0) };
}

/// Thread-local, so nothing another thread in this binary does can land in the
/// window. `Cell` with a `const` initializer has no destructor and allocates
/// nothing, which is what makes it safe to touch from inside the allocator.
fn note(size: usize) {
    let _ = COUNTING.try_with(|counting| {
        if !counting.get() {
            return;
        }
        let _ = LARGE_ALLOCATIONS.try_with(|count| count.set(count.get() + 1));
        let _ = LARGE_BYTES.try_with(|bytes| bytes.set(bytes.get() + size));
    });
}

unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let pointer = unsafe { System.alloc(layout) };
        if !pointer.is_null() && layout.size() >= LARGE {
            note(layout.size());
        }
        pointer
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        unsafe { System.dealloc(pointer, layout) };
    }

    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let replacement = unsafe { System.realloc(pointer, layout, new_size) };
        if !replacement.is_null() && new_size > layout.size() && new_size >= LARGE {
            note(new_size - layout.size());
        }
        replacement
    }
}

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

#[derive(Clone, Debug, PartialEq, VarveBlock)]
#[varve(id = 1, version = 1, kind = "fixed")]
struct Marker {
    value: u32,
}

varve_format! {
    pub struct MetadataAllocFormat {
        magic: b"MDALLC";
        version: 1;
        limits {
            file_len: 1_073_741_824;
            records: 1_000_000;
            index_bytes: 134_217_728;
            scan_bytes: 1_073_741_824;
            record_payload: 16_777_216;
            logical_payload: 16_777_216;
            materialized_bytes: 268_435_456;
            segments: 1_000_000;
            sidecar: 16_777_216;
            mmap: 1_073_741_824;
        }
        endian: little;
        blocks: [Marker];
    }
}

fn cleanup(path: &Path) {
    let _ = std::fs::remove_file(path);
    let mut lock = path.as_os_str().to_os_string();
    lock.push(".lock");
    let _ = std::fs::remove_file(PathBuf::from(lock));
}

fn measure<T>(body: impl FnOnce() -> T) -> (T, usize, usize) {
    LARGE_ALLOCATIONS.with(|count| count.set(0));
    LARGE_BYTES.with(|bytes| bytes.set(0));
    COUNTING.with(|counting| counting.set(true));
    let value = body();
    COUNTING.with(|counting| counting.set(false));
    let count = LARGE_ALLOCATIONS.with(|count| count.get());
    let bytes = LARGE_BYTES.with(|bytes| bytes.get());
    (value, count, bytes)
}

#[test]
fn a_metadata_lookup_decodes_only_the_entry_it_returns() -> varve::Result<()> {
    let path = std::env::temp_dir().join(format!("varve-mdallc-{}.varve", std::process::id()));
    cleanup(&path);
    {
        let mut file = MetadataAllocFormat::spec().create(&path)?;
        for index in 0..ENTRIES {
            file.write_metadata(&format!("k{index}"), &vec![index; VALUE_BYTES])?;
        }
        file.flush()?;
    }

    let file = MetadataAllocFormat::spec().open_readonly(&path)?;
    // Outside the window: the first call settles anything lazily initialized.
    assert_eq!(
        file.metadata("k15")?,
        Some(vec![15u8; VALUE_BYTES]),
        "the fixture must be readable before it is measured",
    );

    let (value, allocations, bytes) = measure(|| file.metadata("k15"));
    assert_eq!(value?, Some(vec![15u8; VALUE_BYTES]));

    // 16 payload buffers plus the one value that is returned = 17. Before the
    // fix every record was decoded as well: 32, two payload-sized allocations
    // per record.
    assert!(
        allocations < 2 * usize::from(ENTRIES),
        "a lookup must not decode every entry: {allocations} allocations of \
         >= {LARGE} bytes ({bytes} bytes) for {ENTRIES} entries",
    );
    assert!(
        allocations <= usize::from(ENTRIES) + 4,
        "expected about one payload buffer per record plus the returned value, \
         got {allocations} allocations ({bytes} bytes)",
    );

    cleanup(&path);
    Ok(())
}
