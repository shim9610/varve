//! What a `[T; N]` field costs to decode.
//!
//! The blanket `impl<T, const N: usize> VarveDecode for [T; N]` used to build
//! every fixed array through a heap `Vec` and then `try_into` it, so a block
//! declaring `quad: [u32; 4]` and `grid: [[u8; 2]; 3]` paid five allocations
//! per record — one per array, and nesting multiplies rather than adds because
//! the inner arrays recurse into the same impl. The matching *encode*
//! (`for value in self { value.encode_varve(encoder)?; }`) has never allocated
//! at all, which is what made the asymmetry a defect rather than a cost.
//!
//! These tests measure the allocations, not the intent.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use varve::{Endian, Error, decode_from_slice, encode_to_vec};

/// Thread-attributed allocation counter.
///
/// The window is opened and closed on one thread and the harness gives every
/// test its own thread, so a concurrently running sibling cannot pollute it.
/// All state is `const`-initialised thread-local `Cell`s, so the hook itself
/// never allocates and cannot recurse.
mod counting_allocations {
    use super::{Cell, GlobalAlloc, Layout, System};

    std::thread_local! {
        /// `(measuring, allocation calls, live bytes)`.
        static STATE: Cell<(bool, u64, i64)> = const { Cell::new((false, 0, 0)) };
    }

    fn note(calls: u64, delta: i64) {
        let _ = STATE.try_with(|state| {
            let (measuring, seen, live) = state.get();
            if !measuring {
                return;
            }
            state.set((true, seen + calls, live + delta));
        });
    }

    pub struct Counting;

    // SAFETY: every method forwards to `System` unchanged and only records
    // counts around it, so the allocator contract is exactly `System`'s.
    unsafe impl GlobalAlloc for Counting {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            let pointer = unsafe { System.alloc(layout) };
            if !pointer.is_null() {
                note(1, layout.size() as i64);
            }
            pointer
        }

        unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
            let pointer = unsafe { System.alloc_zeroed(layout) };
            if !pointer.is_null() {
                note(1, layout.size() as i64);
            }
            pointer
        }

        unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
            note(0, -(layout.size() as i64));
            unsafe { System.dealloc(pointer, layout) }
        }

        unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
            let moved = unsafe { System.realloc(pointer, layout, new_size) };
            if !moved.is_null() {
                note(1, new_size as i64 - layout.size() as i64);
            }
            moved
        }
    }

    /// What `body` allocated: `(result, allocation calls, net live-byte delta)`.
    pub fn measure<R>(body: impl FnOnce() -> R) -> (R, u64, i64) {
        STATE.with(|state| state.set((true, 0, 0)));
        let result = body();
        let (_, calls, live) = STATE.with(|state| state.get());
        STATE.with(|state| state.set((false, 0, 0)));
        (result, calls, live)
    }
}

#[global_allocator]
static COUNTING_ALLOCATOR: counting_allocations::Counting = counting_allocations::Counting;

/// The shape of the public fixture block's array fields
/// (`tools/public-api-fixture/src/main.rs`): one flat array and one nested one.
type Quad = [u32; 4];
type Grid = [[u8; 2]; 3];

/// Above `ARRAY_STACK_DECODE_BUDGET` (16 KiB): `Option<u64>` is 16 bytes with
/// no niche to pack into, so 2048 of them is 32 KiB.
type AboveBudget = [u64; 2048];

fn quad_bytes() -> Vec<u8> {
    encode_to_vec(&[1u32, 2, 3, 4], Endian::Little).expect("quad encodes")
}

fn grid_bytes() -> Vec<u8> {
    encode_to_vec(&[[1u8, 2], [3, 4], [5, 6]], Endian::Little).expect("grid encodes")
}

fn above_budget_bytes() -> Vec<u8> {
    let values: [u64; 2048] = std::array::from_fn(|index| index as u64);
    encode_to_vec(&values, Endian::Little).expect("above-budget array encodes")
}

/// The measurement this change exists for.
///
/// Five allocations per record on the old impl — one for `[u32; 4]`, one for
/// the outer `[[u8; 2]; 3]` and one for each of its three inner arrays — and
/// none after it.
#[test]
fn small_fixed_arrays_decode_without_allocating() {
    let quad = quad_bytes();
    let grid = grid_bytes();

    let (values, calls, _live) =
        counting_allocations::measure(|| decode_from_slice::<Quad>(&quad, Endian::Little));
    assert_eq!(values.expect("quad decodes"), [1u32, 2, 3, 4]);
    assert_eq!(calls, 0, "a `[u32; 4]` decode must not touch the allocator");

    let (values, calls, _live) =
        counting_allocations::measure(|| decode_from_slice::<Grid>(&grid, Endian::Little));
    assert_eq!(values.expect("grid decodes"), [[1u8, 2], [3, 4], [5, 6]]);
    assert_eq!(
        calls, 0,
        "a `[[u8; 2]; 3]` decode must not touch the allocator, outer or inner"
    );

    // The same two fields decoded a thousand times over, which is how a record
    // loop meets them: still zero, so nothing amortised is hiding in the count.
    let (result, calls, _live) = counting_allocations::measure(|| {
        for _ in 0..1000 {
            decode_from_slice::<Quad>(&quad, Endian::Little)?;
            decode_from_slice::<Grid>(&grid, Endian::Little)?;
        }
        Ok::<(), Error>(())
    });
    result.expect("a thousand array decodes succeed");
    assert_eq!(
        calls, 0,
        "1000 records of two array fields allocated {calls}"
    );
}

/// The guard on the size ceiling, which is the part a wrong fix drops.
///
/// An implementation that builds *every* array through `[Option<T>; N]` passes
/// the test above and fails this one: an above-budget array must still go
/// through the fallible heap reservation, because a declared array length must
/// never become an unbounded stack frame.
///
/// This asserts the branch is taken. It does not measure stack consumption.
#[test]
fn an_above_budget_array_still_reserves_on_the_heap() {
    assert!(
        size_of::<Option<u64>>() * 2048 > 16 * 1024,
        "the fixture must sit above the 16 KiB stack budget"
    );
    let bytes = above_budget_bytes();

    let (values, calls, _live) =
        counting_allocations::measure(|| decode_from_slice::<AboveBudget>(&bytes, Endian::Little));
    let values = values.expect("above-budget array decodes");
    assert_eq!(values[0], 0);
    assert_eq!(values[2047], 2047);
    assert!(
        calls >= 1,
        "an array above the stack budget must keep its fallible heap reservation"
    );
}

/// The error path, which is why the slots are `Option<T>` and not
/// `MaybeUninit<T>`.
///
/// A payload that runs out mid-array leaves the stack array partly filled; the
/// `?` must drop it through its ordinary `Drop` and give back a typed error,
/// with no panic and nothing leaked. `String` elements make a leak visible:
/// each decoded element owns a heap buffer.
#[test]
fn a_truncated_array_errors_without_panicking_or_leaking() {
    let values: [String; 4] = std::array::from_fn(|index| "x".repeat(64 + index));
    let bytes = encode_to_vec(&values, Endian::Little).expect("string array encodes");
    // Far enough in that two elements decode and own heap buffers before the
    // third runs off the end.
    let truncated = &bytes[..150];

    let (result, _calls, live) = counting_allocations::measure(|| {
        std::panic::catch_unwind(|| decode_from_slice::<[String; 4]>(truncated, Endian::Little))
    });
    let result = result.expect("a truncated array must not panic");
    assert!(
        matches!(result, Err(Error::UnexpectedEof)),
        "expected a typed UnexpectedEof, got {result:?}"
    );
    assert_eq!(
        live, 0,
        "the partially filled array must drop its decoded elements ({live} bytes still live)"
    );
}

/// Values are unchanged: the arrays that decode without allocating hold exactly
/// what encode wrote, at every nesting depth.
#[test]
fn fixed_arrays_round_trip_unchanged() {
    let quad: Quad = [7, 0, u32::MAX, 42];
    let grid: Grid = [[0, 255], [17, 18], [200, 1]];
    let strings: [String; 3] = ["a".into(), String::new(), "cc".into()];

    for endian in [Endian::Little, Endian::Big] {
        let bytes = encode_to_vec(&quad, endian).expect("quad encodes");
        assert_eq!(
            decode_from_slice::<Quad>(&bytes, endian).expect("quad"),
            quad
        );

        let bytes = encode_to_vec(&grid, endian).expect("grid encodes");
        assert_eq!(
            decode_from_slice::<Grid>(&bytes, endian).expect("grid"),
            grid
        );

        let bytes = encode_to_vec(&strings, endian).expect("strings encode");
        assert_eq!(
            decode_from_slice::<[String; 3]>(&bytes, endian).expect("strings"),
            strings
        );
    }
}
