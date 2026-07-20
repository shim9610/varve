//! F-10: a matrix field spelled as a type ALIAS of a supported scalar must
//! compile.
//!
//! Matrix field eligibility is decided by the generated `SLOT_STRIDE`, which
//! resolves `<T as VarveEncode>::WIRE_TYPE`. A proc macro sees spellings, not
//! types, so it must not pre-reject a field just because the identifier is not
//! one of the literal primitive names — `docs/api-reference.md` documents alias
//! support, and the previous spelling whitelist contradicted it.
//!
//! Covered here: a plain alias, an alias-of-an-alias, an alias reached through
//! a module path, and a fixed array whose element type is an alias.

use varve::{VarveMatrixBlock, varve_format};

/// Alias of a supported scalar. Resolves to `u32`, so the slot stride is 4.
pub type Word = u32;
/// Alias of an alias. Still `u32`.
pub type Sample = Word;

pub mod units {
    /// Alias reached through a module path, so the macro sees a multi-segment
    /// path whose last segment is not a primitive spelling either.
    pub type Millivolts = f64;
}

varve_format! {
    pub format AliasMatrixFormat {
        magic: b"AMTX";
        version: 1;
        limits {
            file_len: 8_589_934_592;
            record_payload: 67_108_864;
            materialized_bytes: 1_073_741_824;
            matrix_dimension: 16_000_000;
            matrix_cells: 16_000_000;
            matrix_bitmap: 64_000_000;
            matrix_crc: 128_000_000;
            matrix_metadata: 268_435_456;
            matrix_slot_region: 8_589_934_592;
            sidecar: 268_435_456;
        }
        endian: little;
        schema_hash: computed;

        dims {
            scan: u32,
            ch: u32,
        }

        commit: cell_bitmap {
            keyspace = [scan, ch];
            categories = [analysis];
        };

        blocks {
            matrix AliasCell(id = 30, dims = [scan, ch], category = analysis) {
                word: Word,
                sample: Sample,
                millivolts: units::Millivolts,
                window: [Word; 4],
            }
        }
    }
}

fn main() {
    // The whole point of the fix: the stride is derived from the RESOLVED
    // types, so aliases contribute exactly what their targets contribute.
    // 4 (`u32`) + 4 (`u32`) + 8 (`f64`) + 4 * 4 = 32.
    assert_eq!(AliasCell::SLOT_STRIDE, 32);

    let cell = AliasCell {
        word: 1,
        sample: 2,
        millivolts: 3.5,
        window: [4, 5, 6, 7],
    };
    assert_eq!(cell.word, 1);
    assert_eq!(cell.sample, 2);
    assert_eq!(cell.millivolts, 3.5);
    assert_eq!(cell.window, [4, 5, 6, 7]);

    let spec = AliasMatrixFormat::spec();
    assert_ne!(spec.schema_hash, 0);
}
