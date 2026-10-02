#![cfg(feature = "integrity")]
#[path = "support/native_redb.rs"]
mod oracle;

#[test]
fn seeded_native_redb_equivalence() {
    if let Some(path) = std::env::var_os("VARVE_ORACLE_REPLAY") {
        let trace = std::fs::read(path).unwrap();
        eprintln!("ORACLE replay {:?}", oracle::run(&trace));
        return;
    }
    let seeds = std::env::var("VARVE_ORACLE_SEEDS")
        .unwrap_or_else(|_| "0,1,42,18446744073709551615".into());
    let operations = std::env::var("VARVE_ORACLE_OPERATIONS")
        .ok()
        .map(|s| s.parse().unwrap())
        .unwrap_or(384);
    for seed in seeds.split(',').map(|s| s.parse::<u64>().unwrap()) {
        let trace = oracle::seeded(seed, operations);
        if let Some(directory) = std::env::var_os("VARVE_ORACLE_TRACE_OUT") {
            let directory = std::path::PathBuf::from(directory);
            std::fs::create_dir_all(&directory).unwrap();
            std::fs::write(directory.join(format!("seed-{seed}.bin")), &trace).unwrap();
        }
        match std::panic::catch_unwind(|| oracle::run(&trace)) {
            Ok(times) => eprintln!("ORACLE seed={seed} {times:?}"),
            Err(error) => {
                let path = std::env::temp_dir().join(format!("varve-redb-failure-{seed}.bin"));
                std::fs::write(&path, &trace).unwrap();
                eprintln!("replay input: {}", path.display());
                std::panic::resume_unwind(error);
            }
        }
    }
}
