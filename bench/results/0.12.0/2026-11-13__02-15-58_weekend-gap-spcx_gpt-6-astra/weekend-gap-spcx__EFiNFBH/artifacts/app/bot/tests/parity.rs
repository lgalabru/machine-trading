// Expected predictions were generated independently by the Python training
// implementation from a public, generated scenario, before export to Rust.
#[allow(dead_code)]
#[path = "../src/features.rs"]
mod features;

#[test]
fn causal_features_and_exported_models_match_training_replay() {
    let mut state = features::Features::default();
    let models: [&[u8]; 3] = [
        include_bytes!("../src/model1.bin"),
        include_bytes!("../src/model2.bin"),
        include_bytes!("../src/model3.bin"),
    ];
    let mut checked = 0;
    for line in include_str!("replay.tsv").lines() {
        let row: Vec<f64> = line
            .split_whitespace()
            .map(|v| v.parse().unwrap())
            .collect();
        let flow: Vec<_> = row[6..]
            .chunks_exact(3)
            .map(|v| (v[0] as usize, v[1], v[2]))
            .collect();
        state.update(row[0], row[1], &flow);
        if row[3] == -9999.0 {
            continue;
        }
        let (vector, _) = state.vector(row[0], row[2]);
        assert_eq!(vector.len(), 256);
        for j in 0..3 {
            let actual = features::predict(models[j], &vector);
            assert!(
                (actual - row[j + 3]).abs() < 1e-9,
                "slot {}, model {}: {} vs {}",
                row[0],
                j,
                actual,
                row[j + 3]
            );
        }
        checked += 1;
    }
    assert!(checked > 40);
}
