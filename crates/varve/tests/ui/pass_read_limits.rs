use std::path::Path;

use varve::{ReadLimit, ReadLimits, varve_format};

varve_format! {
    pub format BoundedFormat {
        magic: b"BOUNDED";
        version: 1;
        limits {
            file_len: 1024;
            records: 16;
            index_bytes: 4096;
            scan_bytes: 1024;
            record_payload: 256;
            logical_payload: 512;
            materialized_bytes: 2048;
        }
        blocks {
            fixed Item(id = 1) {
                value: u32,
            }
        }
    }
}

varve_format! {
    pub format TrustedFormat {
        magic: b"TRUSTED";
        version: 1;
        limits: trusted_unbounded;
        blocks {
            fixed TrustedItem(id = 2) {
                value: u32,
            }
        }
    }
}

fn typecheck_generated_entrypoints(path: &Path) {
    let runtime = ReadLimits::finite_all(512);
    let _ = BoundedFormat::open_with_limits(path, runtime);
    let _ = BoundedFormat::open_reader_with_limits(path, runtime);
    let _ = BoundedFormat::open_writer_with_limits(path, runtime);
    let _ = BoundedFormat::open_recover_with_limits(path, runtime);
    let _ = BoundedFormat::open_recover_writer_with_limits(path, runtime);
    let _ = TrustedFormat::open_trusted_unbounded(path);
    let _ = TrustedFormat::open_reader_trusted_unbounded(path);
    let _ = TrustedFormat::open_writer_trusted_unbounded(path);
    let _ = TrustedFormat::open_recover_trusted_unbounded(path);
    let _ = TrustedFormat::open_recover_writer_trusted_unbounded(path);
}

fn main() {
    assert_eq!(
        BoundedFormat::spec().read_limits.max_file_len,
        ReadLimit::Finite(1024)
    );
    assert_eq!(
        TrustedFormat::spec().read_limits.max_file_len,
        ReadLimit::TrustedUnbounded
    );
    let _ = typecheck_generated_entrypoints;
}
