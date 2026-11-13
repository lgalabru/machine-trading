//! Compact tree inference with no runtime I/O.
//!
//! The trusted, compiled-in blob contains a model count, then each model's
//! baseline, root indices, and nodes (feature, children, threshold, leaf value).

#[derive(Default)]
pub struct Models {
    models: Vec<Model>,
}
struct Model {
    base: f64,
    roots: Vec<usize>,
    nodes: Vec<Node>,
}
struct Node {
    feature: i32,
    left: usize,
    right: usize,
    threshold: f64,
    value: f64,
}
fn u32at(b: &[u8], p: &mut usize) -> u32 {
    let x = u32::from_le_bytes(b[*p..*p + 4].try_into().unwrap());
    *p += 4;
    x
}
fn f64at(b: &[u8], p: &mut usize) -> f64 {
    let x = f64::from_le_bytes(b[*p..*p + 8].try_into().unwrap());
    *p += 8;
    x
}
impl Models {
    pub fn load() -> Self {
        let b = include_bytes!("models.bin");
        let mut p = 0;
        let count = u32at(b, &mut p);
        let mut models = Vec::new();
        for _ in 0..count {
            let base = f64at(b, &mut p);
            let nt = u32at(b, &mut p);
            let nn = u32at(b, &mut p);
            let roots = (0..nt).map(|_| u32at(b, &mut p) as usize).collect();
            let nodes = (0..nn)
                .map(|_| Node {
                    feature: u32at(b, &mut p) as i32,
                    left: u32at(b, &mut p) as usize,
                    right: u32at(b, &mut p) as usize,
                    threshold: f64at(b, &mut p),
                    value: f64at(b, &mut p),
                })
                .collect();
            models.push(Model { base, roots, nodes });
        }
        Self { models }
    }
    pub fn predict(&self, f: &[f64]) -> Vec<f64> {
        self.models
            .iter()
            .map(|m| {
                let mut y = m.base;
                for &root in &m.roots {
                    let mut j = root;
                    loop {
                        let n = &m.nodes[j];
                        if n.feature < 0 {
                            y += n.value;
                            break;
                        }
                        j = if f[n.feature as usize] <= n.threshold {
                            n.left
                        } else {
                            n.right
                        };
                    }
                }
                y
            })
            .collect()
    }
}
