//! Small, dependency-free evaluator for the fitted regression trees.
pub struct Model {
    roots: Vec<usize>,
    nodes: Vec<Node>,
}
struct Node {
    feature: i32,
    left: usize,
    right: usize,
    value: f64,
}
impl Model {
    pub fn from_bytes(data: &[u8]) -> Self {
        let mut at = 0;
        fn u32_at(data: &[u8], at: &mut usize) -> u32 {
            let v = u32::from_le_bytes(data[*at..*at + 4].try_into().unwrap());
            *at += 4;
            v
        }
        let nt = u32_at(data, &mut at) as usize;
        let nn = u32_at(data, &mut at) as usize;
        let roots = (0..nt).map(|_| u32_at(data, &mut at) as usize).collect();
        let nodes = (0..nn)
            .map(|_| {
                let feature = u32_at(data, &mut at) as i32;
                let left = u32_at(data, &mut at) as usize;
                let right = u32_at(data, &mut at) as usize;
                let value = f64::from_le_bytes(data[at..at + 8].try_into().unwrap());
                at += 8;
                Node {
                    feature,
                    left,
                    right,
                    value,
                }
            })
            .collect();
        Self { roots, nodes }
    }
    pub fn predict(&self, x: &[f32; 73]) -> f64 {
        self.roots
            .iter()
            .map(|&root| {
                let mut index = root;
                loop {
                    let n = &self.nodes[index];
                    if n.feature < 0 {
                        break n.value;
                    }
                    index = if x[n.feature as usize] as f64 <= n.value {
                        n.left
                    } else {
                        n.right
                    };
                }
            })
            .sum()
    }
}
