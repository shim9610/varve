use std::fs;
use std::path::PathBuf;

use varve::varve_format;

varve_format! {
    pub format FuzzNativeFormat {
        magic: b"FZNV";
        version: 1;
        limits {
            file_len: 2_097_152;
            records: 4_096;
            index_bytes: 1_048_576;
            scan_bytes: 2_097_152;
            record_payload: 1_048_576;
            logical_payload: 2_097_152;
            materialized_bytes: 4_194_304;
            segments: 4_096;
            matrix_dimension: 1_024;
            matrix_cells: 65_536;
            matrix_bitmap: 1_048_576;
            matrix_crc: 1_048_576;
            matrix_metadata: 1_048_576;
            matrix_slot_region: 2_097_152;
            sidecar: 2_097_152;
            mmap: 2_097_152;
        }
        endian: little;
        schema_hash: computed;
        index: [scan_on_open, checkpoint_on_flush, block_offset_chain, keyed_offset_chain];
        commit: transaction_marker(on_flush);
        integrity: crc32_with_header;
        recovery: truncate_tail;
        manifest: embedded;
        compression: variable_blocks(
            zstd,
            level = fast,
            header = record_explicit,
            min_len = 0,
            only_if_smaller = false,
            max_len = 2_097_152,
        );
        blocks {
            fixed FuzzPoint(id = 1) {
                x: u32,
                y: i64,
            }

            variable FuzzRecord(id = 2, key = [id]) {
                id: u64,
                name: String,
                payload: Vec<u8>,
                values: Vec<i64> = default,
            }
        }
    }
}

varve_format! {
    pub format FuzzLayoutFormat {
        magic: b"FZLY";
        version: 1;
        limits {
            file_len: 2_097_152;
            records: 4_096;
            index_bytes: 1_048_576;
            scan_bytes: 2_097_152;
            record_payload: 1_048_576;
            logical_payload: 2_097_152;
            materialized_bytes: 4_194_304;
            segments: 4_096;
            matrix_dimension: 1_024;
            matrix_cells: 65_536;
            matrix_bitmap: 1_048_576;
            matrix_crc: 1_048_576;
            matrix_metadata: 1_048_576;
            matrix_slot_region: 2_097_152;
            sidecar: 2_097_152;
            mmap: 2_097_152;
        }
        endian: little;
        schema_hash: computed;
        preset: none;

        layout {
            segment Data repeat until_eof {
                lead_in Lead {
                    bytes tag = b"FZSG";
                    u32 kind;
                    u64 next = finalize(target = segment_end, relative_to = after_lead_in);
                    u64 raw = finalize(target = raw_region_start, relative_to = after_lead_in);
                }

                metadata Metadata;
                raw_region Raw;

                footer Footer {
                    bytes seal = b"FZDN";
                    u64 segment_len = finalize(target = segment_end, relative_to = segment_start);
                }
            }
        }
    }
}

varve_format! {
    pub format FuzzMatrixFormat {
        magic: b"FZMX";
        version: 1;
        limits {
            file_len: 2_097_152;
            records: 4_096;
            index_bytes: 1_048_576;
            scan_bytes: 2_097_152;
            record_payload: 1_048_576;
            logical_payload: 2_097_152;
            materialized_bytes: 4_194_304;
            segments: 4_096;
            matrix_dimension: 1_024;
            matrix_cells: 65_536;
            matrix_bitmap: 1_048_576;
            matrix_crc: 1_048_576;
            matrix_metadata: 1_048_576;
            matrix_slot_region: 2_097_152;
            sidecar: 2_097_152;
            mmap: 2_097_152;
        }
        endian: little;
        schema_hash: computed;
        integrity: crc32_with_header;
        dims {
            row: u32,
            column: u32,
        }
        commit: cell_bitmap {
            keyspace = [row, column];
            categories = [fuzz];
        };
        aux {
            scratch: 64,
        }
        blocks {
            matrix FuzzCell(id = 10, dims = [row, column], category = fuzz) {
                value: u64,
            }
        }
    }
}

pub fn write_fuzz_file(kind: &str, bytes: &[u8]) -> Option<PathBuf> {
    let path = std::env::temp_dir().join(format!("varve-{kind}-fuzz-{}.bin", std::process::id()));
    let mut lock_name = path.as_os_str().to_os_string();
    lock_name.push(".lock");
    let _ = fs::remove_file(PathBuf::from(lock_name));
    fs::write(&path, bytes).ok()?;
    Some(path)
}
