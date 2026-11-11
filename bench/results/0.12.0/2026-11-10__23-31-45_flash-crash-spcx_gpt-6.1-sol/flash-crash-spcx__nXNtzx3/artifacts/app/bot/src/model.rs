//! Small native evaluator for the embedded regression trees. No runtime dependencies.
#[derive(Clone)]
struct Node {
    feature: i32,
    left: i32,
    right: i32,
    threshold: f64,
    value: f64,
}
#[derive(Default)]
pub struct Model {
    trees: Vec<Vec<Node>>,
}
impl Model {
    pub fn load(data: &[u8]) -> Self {
        let mut at = 0;
        fn i32_at(d: &[u8], p: &mut usize) -> i32 {
            let v = i32::from_le_bytes(d[*p..*p + 4].try_into().unwrap());
            *p += 4;
            v
        }
        fn f64_at(d: &[u8], p: &mut usize) -> f64 {
            let v = f64::from_le_bytes(d[*p..*p + 8].try_into().unwrap());
            *p += 8;
            v
        }
        let count = i32_at(data, &mut at);
        let mut trees = Vec::new();
        for _ in 0..count {
            let n = i32_at(data, &mut at);
            let mut nodes = Vec::new();
            for _ in 0..n {
                nodes.push(Node {
                    feature: i32_at(data, &mut at),
                    left: i32_at(data, &mut at),
                    right: i32_at(data, &mut at),
                    threshold: f64_at(data, &mut at),
                    value: f64_at(data, &mut at),
                });
            }
            trees.push(nodes);
        }
        assert_eq!(at, data.len());
        Self { trees }
    }
    pub fn predict(&self, x: &[f64]) -> f64 {
        let mut result = 0.0;
        for tree in &self.trees {
            let mut i = 0;
            loop {
                let n = &tree[i];
                if n.feature < 0 {
                    result += n.value;
                    break;
                }
                i = if x[n.feature as usize] <= n.threshold {
                    n.left as usize
                } else {
                    n.right as usize
                };
            }
        }
        result
    }
}
