//! `mtx_kit::trees` against models from outside it: the funding-carry
//! reference's `model.bin`, and the fixtures `tools/export_trees.py` wrote
//! from fitted scikit-learn and LightGBM models.
//!
//! The fixtures in `tests/fixtures/trees` were made with scikit-learn 1.9.1
//! and LightGBM 4.7.0: each `.bin` is `export_trees.export(model)`, and each
//! `.json` holds rows of raw float64 features (as bit patterns, so a value
//! one step from a threshold survives the file) with
//! `model.predict(rows.astype(np.float32))` for each.

use std::path::{Path, PathBuf};

use mtx_kit::trees::Trees;
use serde::Deserialize;

/// How close a prediction must be to the Python model's.
const TOLERANCE: f64 = 1e-9;

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/trees")
        .join(name)
}

#[derive(Deserialize)]
struct Fixture {
    feature_count: usize,
    rows: Vec<Row>,
}

#[derive(Deserialize)]
struct Row {
    /// Each feature's `f64` bits, big-endian hex.
    features: Vec<String>,
    prediction: f64,
}

impl Row {
    fn features(&self) -> Vec<f64> {
        self.features
            .iter()
            .map(|bits| f64::from_bits(u64::from_str_radix(bits, 16).unwrap()))
            .collect()
    }
}

/// Loads `<name>.bin` and checks it against every row of `<name>.json`.
fn matches_python(name: &str) {
    let bytes = std::fs::read(fixture(&format!("{name}.bin"))).unwrap();
    let trees = Trees::from_bytes(&bytes).unwrap();
    assert_eq!(trees.to_bytes(), bytes);
    let json = std::fs::read_to_string(fixture(&format!("{name}.json"))).unwrap();
    let fixture: Fixture = serde_json::from_str(&json).unwrap();
    assert!(trees.feature_count() <= fixture.feature_count);
    assert!(fixture.rows.len() >= 30, "the fixture holds its rows");
    for (index, row) in fixture.rows.iter().enumerate() {
        let features = row.features();
        assert_eq!(features.len(), fixture.feature_count);
        let prediction = trees.predict(&features).unwrap();
        let error = (prediction - row.prediction).abs();
        assert!(
            error <= TOLERANCE,
            "{name} row {index}: {prediction} against Python's {}",
            row.prediction
        );
    }
}

#[test]
fn matches_scikit_learn_hist_gradient_boosting() {
    matches_python("sklearn_hgb");
}

#[test]
fn matches_lightgbm() {
    matches_python("lightgbm");
}

/// The fixtures hold rows where rounding to f32 changes the leaf: without
/// the rounding, the kit would disagree with Python on them.
#[test]
fn the_fixtures_exercise_the_f32_rounding() {
    for name in ["sklearn_hgb", "lightgbm"] {
        let bytes = std::fs::read(fixture(&format!("{name}.bin"))).unwrap();
        let trees = Trees::from_bytes(&bytes).unwrap();
        let json = std::fs::read_to_string(fixture(&format!("{name}.json"))).unwrap();
        let fixture: Fixture = serde_json::from_str(&json).unwrap();
        let unrounded_model = reference::Model::load(&bytes);
        let moved = fixture.rows.iter().filter(|row| {
            let features = row.features();
            let unrounded = unrounded_model.predict_unrounded(&features);
            let kit = trees.predict(&features).unwrap();
            (kit - unrounded).abs() > TOLERANCE
        });
        assert!(moved.count() >= 1, "{name} has a row at a threshold");
    }
}

/// The 0.14 funding-carry-spcx reference's model (its `model.bin`, kept as
/// `tests/fixtures/carry-trees.bin` since the task was retired) loads, writes back the same bytes,
/// and predicts what the reference's own evaluator does, to the bit.
#[test]
fn matches_the_funding_carry_reference() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/carry-trees.bin");
    let bytes = std::fs::read(&path).unwrap_or_else(|error| panic!("{}: {error}", path.display()));
    let trees = Trees::from_bytes(&bytes).unwrap();
    assert_eq!(trees.tree_count(), 1900);
    assert_eq!(trees.node_count(), 188_082);
    assert_eq!(trees.feature_count(), 46);
    assert_eq!(trees.to_bytes(), bytes);

    let model = reference::Model::load(&bytes);
    // A fixed-seed generator: features across the scales the model's
    // thresholds sit at, and a few exact corners.
    let mut state = 0x2545_F491_4F6C_DD1D_u64;
    let mut next = move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        (state >> 11) as f64 / (1u64 << 53) as f64
    };
    let mut rows: Vec<Vec<f64>> = vec![vec![0.0; 46], vec![1.0; 46], vec![-1.0; 46]];
    for scale in [0.01, 1.0, 30.0, 1000.0] {
        for _ in 0..8 {
            rows.push((0..46).map(|_| (next() * 2.0 - 1.0) * scale).collect());
        }
    }
    let mut distinct = std::collections::BTreeSet::new();
    for row in &rows {
        let prediction = trees.predict(row).unwrap();
        assert_eq!(prediction.to_bits(), model.predict(row).to_bits());
        distinct.insert(prediction.to_bits());
    }
    assert!(distinct.len() > 10, "the rows reach different leaves");
}

/// The 0.14 funding-carry-spcx reference's evaluator (its `reference/
/// src/model.rs`), copied so this test doesn't depend on the reference.
mod reference {
    pub struct Node {
        feature: usize,
        leaf: bool,
        threshold: f64,
        value: f64,
        left: usize,
        right: usize,
    }

    pub struct Model {
        base: f64,
        trees: Vec<Vec<Node>>,
    }

    impl Model {
        pub fn load(b: &[u8]) -> Self {
            let mut p = 0;
            fn u(b: &[u8], p: &mut usize) -> u32 {
                let x = u32::from_le_bytes(b[*p..*p + 4].try_into().unwrap());
                *p += 4;
                x
            }
            fn d(b: &[u8], p: &mut usize) -> f64 {
                let x = f64::from_le_bytes(b[*p..*p + 8].try_into().unwrap());
                *p += 8;
                x
            }
            let n = u(b, &mut p);
            let base = d(b, &mut p);
            let mut trees = Vec::new();
            for _ in 0..n {
                let n = u(b, &mut p);
                let mut tree = Vec::new();
                for _ in 0..n {
                    tree.push(Node {
                        feature: u(b, &mut p) as usize,
                        leaf: u(b, &mut p) != 0,
                        threshold: d(b, &mut p),
                        value: d(b, &mut p),
                        left: u(b, &mut p) as usize,
                        right: u(b, &mut p) as usize,
                    });
                }
                trees.push(tree);
            }
            Self { base, trees }
        }

        pub fn predict(&self, x: &[f64]) -> f64 {
            self.walk(x, |value| value as f32 as f64)
        }

        /// The same walk comparing the feature as given: what the kit
        /// would do if it skipped the rounding.
        pub fn predict_unrounded(&self, x: &[f64]) -> f64 {
            self.walk(x, |value| value)
        }

        fn walk(&self, x: &[f64], seen: impl Fn(f64) -> f64) -> f64 {
            let mut y = self.base;
            for tree in &self.trees {
                let mut n = 0;
                loop {
                    let node = &tree[n];
                    if node.leaf {
                        y += node.value;
                        break;
                    }
                    n = if seen(x[node.feature]) <= node.threshold {
                        node.left
                    } else {
                        node.right
                    };
                }
            }
            y
        }
    }
}
