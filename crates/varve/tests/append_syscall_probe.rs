// An append loop with nothing else in it, so an external tool can count the
// kernel crossings a record costs.
//
// The in-tree counters (`VarveFile::take_record_file_metadata_calls`,
// `take_snapshot_bounds_fstats`) are the deterministic proof that the two
// per-record metadata syscalls are gone. This is the corroborating measurement
// from outside the process, because a counter can be wrong about what the
// kernel actually saw and `strace` cannot be.
//
// `#[ignore]`d: it is a measurement harness, not an assertion. Run it as
//
//     N=10000  cargo test -p varve --test append_syscall_probe --all-features \
//         -- --ignored --exact appends_records_for_an_external_syscall_count
//
// under `strace -f -c -e trace=newfstatat,statx,fstat,lseek,write`, twice with
// two values of `N`. The difference divided by the difference in `N` is the
// per-record cost with every fixed cost — process start, dynamic linking, the
// test harness, create, flush — cancelled out. A single run cannot be divided
// by `N` honestly, which is why this takes `N` from the environment.

use varve::varve_format;

varve_format! {
    pub format SyscallProbeFormat {
        magic: b"SYSCPROB";
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
            fixed Sample(id = 1) {
                a: u64,
                b: u64,
            }
        }
    }
}

#[test]
#[ignore = "measurement harness; run under strace with two values of N"]
fn appends_records_for_an_external_syscall_count() -> varve::Result<()> {
    let records: u64 = std::env::var("N")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(10_000);
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("syscall-probe.vrv");

    let mut file = SyscallProbeFormat::create(&path)?;
    for index in 0..records {
        file.push(&Sample {
            a: index,
            b: index.wrapping_mul(3),
        })?;
    }
    file.flush()?;
    eprintln!("appended {records}");
    Ok(())
}
