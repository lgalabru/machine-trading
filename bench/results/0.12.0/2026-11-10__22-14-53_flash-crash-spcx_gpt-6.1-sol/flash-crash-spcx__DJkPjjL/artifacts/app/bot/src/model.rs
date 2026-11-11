struct Node {
    feature: i32,
    threshold: f64,
    left: usize,
    right: usize,
    value: f64,
}
pub struct Model {
    roots: Vec<usize>,
    nodes: Vec<Node>,
}
impl Model {
    pub fn load(data: &[u8]) -> Self {
        fn u(d: &[u8], p: &mut usize) -> u32 {
            let v = u32::from_le_bytes(d[*p..*p + 4].try_into().unwrap());
            *p += 4;
            v
        }
        fn f(d: &[u8], p: &mut usize) -> f64 {
            let v = f64::from_le_bytes(d[*p..*p + 8].try_into().unwrap());
            *p += 8;
            v
        }
        let mut p = 0;
        let nr = u(data, &mut p);
        let nn = u(data, &mut p);
        let roots = (0..nr).map(|_| u(data, &mut p) as usize).collect();
        let mut nodes = Vec::new();
        for _ in 0..nn {
            nodes.push(Node {
                feature: u(data, &mut p) as i32,
                threshold: f(data, &mut p),
                left: u(data, &mut p) as usize,
                right: u(data, &mut p) as usize,
                value: f(data, &mut p),
            });
        }
        Self { roots, nodes }
    }
    pub fn predict(&self, features: &[f64]) -> f64 {
        let mut result = 0.;
        for &root in &self.roots {
            let mut i = root;
            loop {
                let n = &self.nodes[i];
                if n.feature < 0 {
                    result += n.value;
                    break;
                }
                i = if (features[n.feature as usize] as f32 as f64) <= n.threshold {
                    n.left
                } else {
                    n.right
                };
            }
        }
        result
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn exported_predictions_match_training_library() {
        let models = [
            Model::load(include_bytes!("model6.bin")),
            Model::load(include_bytes!("model12.bin")),
            Model::load(include_bytes!("urgent.bin")),
        ];
        let bytes = include_bytes!("fixture.bin");
        for row in bytes.chunks_exact(93 * 8) {
            let v: Vec<f64> = row
                .chunks_exact(8)
                .map(|x| f64::from_le_bytes(x.try_into().unwrap()))
                .collect();
            for i in 0..3 {
                assert!((models[i].predict(&v[..90]) - v[90 + i]).abs() < 1e-10);
            }
        }
    }
}
