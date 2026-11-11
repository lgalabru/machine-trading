//! Minimal inference for the embedded LightGBM regression trees.
use crate::features::FEATURES;

struct Node {
    feature: i32,
    threshold: f64,
    left: usize,
    right: usize,
}

pub struct Model {
    roots: Vec<usize>,
    nodes: Vec<Node>,
}

impl Model {
    pub fn load(bytes: &[u8]) -> Self {
        let u32_at = |i: usize| {
            u32::from_le_bytes(bytes[i..i + 4].try_into().unwrap()) as usize
        };
        let trees = u32_at(0);
        let count = u32_at(4);
        let start = 8 + trees * 4;
        assert_eq!(bytes.len(), start + count * 20);
        let roots = (0..trees).map(|i| u32_at(8 + i * 4)).collect();
        let nodes = (0..count).map(|i| {
            let j = start + i * 20;
            Node {
                feature: i32::from_le_bytes(bytes[j..j + 4].try_into().unwrap()),
                threshold: f64::from_le_bytes(bytes[j + 4..j + 12].try_into().unwrap()),
                left: u32_at(j + 12),
                right: u32_at(j + 16),
            }
        }).collect();
        Self { roots, nodes }
    }

    pub fn predict(&self, features: &[f32; FEATURES]) -> f64 {
        let mut result = 0.0;
        for &root in &self.roots {
            let mut i = root;
            loop {
                let node = &self.nodes[i];
                if node.feature < 0 {
                    result += node.threshold;
                    break;
                }
                i = if features[node.feature as usize] as f64 <= node.threshold {
                    node.left
                } else {
                    node.right
                };
            }
        }
        result / 10000.0
    }
}
