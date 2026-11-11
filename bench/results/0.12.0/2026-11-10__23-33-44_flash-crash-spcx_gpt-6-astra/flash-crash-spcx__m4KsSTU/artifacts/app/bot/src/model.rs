//! Small native evaluator for the compiled regression-tree tables. There is no
//! runtime model loading, training, or dependency on a machine-learning library.
use super::features::NF;
#[derive(Clone)]
pub struct Node {
    pub f: i32,
    pub l: usize,
    pub r: usize,
    pub cut: f32,
    pub val: f32,
}
pub struct Model {
    pub trees: Vec<Vec<Node>>,
}
impl Model {
    pub fn parse(s: &str) -> Self {
        let mut it = s.split_whitespace();
        let n: usize = it.next().unwrap().parse().unwrap();
        let mut trees = Vec::new();
        for _ in 0..n {
            let count: usize = it.next().unwrap().parse().unwrap();
            let mut nodes = Vec::new();
            for _ in 0..count {
                let f = it.next().unwrap().parse().unwrap();
                let l: i32 = it.next().unwrap().parse().unwrap();
                let r: i32 = it.next().unwrap().parse().unwrap();
                let cut = it.next().unwrap().parse().unwrap();
                let val = it.next().unwrap().parse().unwrap();
                nodes.push(Node {
                    f,
                    l: l.max(0) as usize,
                    r: r.max(0) as usize,
                    cut,
                    val,
                });
            }
            trees.push(nodes);
        }
        Self { trees }
    }
    pub fn predict(&self, x: &[f32; NF]) -> f64 {
        let mut sum = 0.0;
        for tree in &self.trees {
            let mut n = 0;
            loop {
                let node = &tree[n];
                if node.f < 0 {
                    sum += node.val as f64;
                    break;
                }
                n = if x[node.f as usize] <= node.cut {
                    node.l
                } else {
                    node.r
                };
            }
        }
        sum
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn embedded_models_have_finite_acyclic_trees() {
        for text in [include_str!("model0.txt"), include_str!("model4.txt")] {
            let model = Model::parse(text);
            assert_eq!(model.trees.len(), 160);
            for tree in &model.trees {
                assert!(!tree.is_empty());
                for (i, n) in tree.iter().enumerate() {
                    assert!(n.cut.is_finite() && n.val.is_finite());
                    if n.f >= 0 {
                        assert!((n.f as usize) < NF);
                        assert!(n.l > i && n.l < tree.len());
                        assert!(n.r > i && n.r < tree.len());
                    }
                }
            }
            assert!(model.predict(&[0.0; NF]).is_finite());
        }
    }
}
