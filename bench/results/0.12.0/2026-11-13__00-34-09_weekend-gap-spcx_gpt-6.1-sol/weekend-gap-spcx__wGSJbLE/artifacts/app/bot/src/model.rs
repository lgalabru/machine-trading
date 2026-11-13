#[derive(Default)]
pub struct Model {
    base: f32,
    roots: Vec<usize>,
    nodes: Vec<(i32, i32, i32, f32)>,
}
impl Model {
    pub fn load(b: &[u8]) -> Self {
        fn u(b: &[u8], p: usize) -> u32 {
            u32::from_le_bytes(b[p..p + 4].try_into().unwrap())
        }
        let base = f32::from_bits(u(b, 0));
        let n = u(b, 4) as usize;
        let count = u(b, 8) as usize;
        let roots = (0..n).map(|j| u(b, 12 + j * 4) as usize).collect();
        let p = 12 + 4 * n;
        let nodes = (0..count)
            .map(|j| {
                let k = p + 16 * j;
                (
                    u(b, k) as i32,
                    u(b, k + 4) as i32,
                    u(b, k + 8) as i32,
                    f32::from_bits(u(b, k + 12)),
                )
            })
            .collect();
        Self { base, roots, nodes }
    }
    pub fn predict(&self, x: &[f32]) -> f64 {
        let mut y = self.base;
        for &root in &self.roots {
            let mut i = root;
            loop {
                let (f, l, r, v) = self.nodes[i];
                if f < 0 {
                    y += v;
                    break;
                }
                i = if x[f as usize] < v {
                    l as usize
                } else {
                    r as usize
                };
            }
        }
        y as f64
    }
}
