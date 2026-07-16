use varve::{ReadLimit, varve_format};

varve_format! {
    pub format OmittedLimits {
        magic: b"OMITTED_LIMITS";
        version: 1;
        blocks {
            fixed OmittedItem(id = 1) {
                value: u32,
            }
        }
    }
}

varve_format! {
    pub format PartialLimits {
        magic: b"PARTIAL_LIMITS";
        version: 1;
        limits {
            record_payload: 1024;
        }
        blocks {
            variable PartialItem(id = 2) {
                value: String,
            }
        }
    }
}

fn main() {
    assert_eq!(
        OmittedLimits::spec().read_limits.max_file_len,
        ReadLimit::Missing,
    );
    assert_eq!(
        PartialLimits::spec().read_limits.max_file_len,
        ReadLimit::Missing,
    );
    assert_eq!(
        PartialLimits::spec().read_limits.max_record_payload_len,
        ReadLimit::Finite(1024),
    );
}
