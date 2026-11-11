use crate::Features;
struct Node {
    feature: i32,
    value: f64,
    left: usize,
    right: usize,
}
pub struct Model {
    trees: Vec<Vec<Node>>,
}
impl Model {
    pub fn load(bytes: &[u8]) -> Self {
        let mut at = 0;
        fn u32at(b: &[u8], at: &mut usize) -> u32 {
            let v = u32::from_le_bytes(b[*at..*at + 4].try_into().unwrap());
            *at += 4;
            v
        }
        let n = u32at(bytes, &mut at);
        let mut trees = vec![];
        for _ in 0..n {
            let count = u32at(bytes, &mut at);
            let mut nodes = vec![];
            for _ in 0..count {
                let feature = u32at(bytes, &mut at) as i32;
                let value = f64::from_le_bytes(bytes[at..at + 8].try_into().unwrap());
                at += 8;
                let left = u32at(bytes, &mut at) as usize;
                let right = u32at(bytes, &mut at) as usize;
                nodes.push(Node {
                    feature,
                    value,
                    left,
                    right,
                });
            }
            trees.push(nodes);
        }
        assert_eq!(at, bytes.len());
        Self { trees }
    }
    pub fn predict(&self, features: &[f32; 64]) -> f64 {
        let mut result = 0.0;
        for tree in &self.trees {
            let mut i = 0;
            loop {
                let node = &tree[i];
                if node.feature < 0 {
                    result += node.value;
                    break;
                }
                i = if features[node.feature as usize] as f64 <= node.value {
                    node.left
                } else {
                    node.right
                };
            }
        }
        result
    }
}
pub struct Forecaster {
    pub features: Features,
    primary: Model,
    secondary: Model,
    pub score: f64,
}
impl Default for Forecaster {
    fn default() -> Self {
        Self {
            features: Features::default(),
            primary: Model::load(include_bytes!("../models/forecast-primary.bin")),
            secondary: Model::load(include_bytes!("../models/forecast-secondary.bin")),
            score: 0.0,
        }
    }
}
impl Forecaster {
    /// Expected return in percent, relative to the direction of the latest move.
    fn expected_return(&self, features: &[f32; 64]) -> f64 {
        let primary = self.primary.predict(features);
        let secondary = self.secondary.predict(features);
        if primary.abs() > 0.2 {
            primary
        } else {
            0.5 * (primary + secondary)
        }
    }
    pub fn step(&mut self, p: f64, left: usize, position: f64) -> f64 {
        let f = self.features.step(p, left);
        let expected = self.expected_return(&f);
        self.score = self.features.direction * expected;
        if self.score > 0.2 {
            1.0
        } else if self.score < -0.2 {
            -1.0
        } else if self.score > -0.15 && position > 0.0 {
            1.0
        } else if self.score < 0.15 && position < 0.0 {
            -1.0
        } else {
            0.0
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn embedded_forests_and_policy_match_independent_predictions() {
        let bot = Forecaster::default();
        let mut strong = false;
        let mut weak = false;
        for fixture in include_bytes!("../tests/forecast-fixtures.bin").chunks_exact(280) {
            let mut features = [0.0f32; 64];
            for (i, b) in fixture[..256].chunks_exact(4).enumerate() {
                features[i] = f32::from_le_bytes(b.try_into().unwrap());
            }
            let primary = f64::from_le_bytes(fixture[256..264].try_into().unwrap());
            let secondary = f64::from_le_bytes(fixture[264..272].try_into().unwrap());
            let expected = f64::from_le_bytes(fixture[272..280].try_into().unwrap());
            assert!((bot.primary.predict(&features) - primary).abs() < 1e-9);
            assert!((bot.secondary.predict(&features) - secondary).abs() < 1e-9);
            assert!((bot.expected_return(&features) - expected).abs() < 1e-9);
            if primary.abs() > 0.2 {
                strong = true;
            } else {
                weak = true;
            }
        }
        assert!(strong && weak);
    }
}
