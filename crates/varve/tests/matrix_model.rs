#![cfg(feature = "integrity")]
#[path = "support/matrix_model.rs"]
mod model;
#[test]
fn seeded_matrix_histories_match_independent_snapshots() {
    for seed in 0..64u64 {
        let mut state = seed;
        let data: Vec<u8> = (0..384)
            .map(|_| {
                state = state
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                (state >> 32) as u8
            })
            .collect();
        model::run(&data).unwrap_or_else(|error| panic!("seed={seed}: {error}"));
    }
}
