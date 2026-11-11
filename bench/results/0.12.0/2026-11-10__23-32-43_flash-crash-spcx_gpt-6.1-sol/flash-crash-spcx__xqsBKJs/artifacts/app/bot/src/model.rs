//! Compact, embedded gradient-boosted regressors. No runtime files or Python.
#[derive(Debug)]
struct Node {
    feature: usize,
    leaf: bool,
    threshold: f64,
    value: f64,
    left: usize,
    right: usize,
}
pub struct Regressor {
    baseline: f64,
    trees: Vec<Vec<Node>>,
}
pub struct Forecast {
    models: Vec<Regressor>,
}
struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
}
impl Reader<'_> {
    fn u8(&mut self) -> u8 {
        let v = self.bytes[self.at];
        self.at += 1;
        v
    }
    fn u16(&mut self) -> usize {
        let a = self.at;
        self.at += 2;
        u16::from_le_bytes(self.bytes[a..a + 2].try_into().unwrap()) as usize
    }
    fn u32(&mut self) -> usize {
        let a = self.at;
        self.at += 4;
        u32::from_le_bytes(self.bytes[a..a + 4].try_into().unwrap()) as usize
    }
    fn f64(&mut self) -> f64 {
        let a = self.at;
        self.at += 8;
        f64::from_le_bytes(self.bytes[a..a + 8].try_into().unwrap())
    }
}
impl Default for Forecast {
    fn default() -> Self {
        let mut r = Reader {
            bytes: include_bytes!("model.bin"),
            at: 0,
        };
        let n = r.u32();
        let mut models = Vec::with_capacity(n);
        for _ in 0..n {
            let baseline = r.f64();
            let nt = r.u32();
            let mut trees = Vec::with_capacity(nt);
            for _ in 0..nt {
                let nn = r.u32();
                let mut nodes = Vec::with_capacity(nn);
                for _ in 0..nn {
                    nodes.push(Node {
                        feature: r.u8() as usize,
                        leaf: r.u8() != 0,
                        threshold: r.f64(),
                        value: r.f64(),
                        left: r.u16(),
                        right: r.u16(),
                    });
                }
                trees.push(nodes);
            }
            models.push(Regressor { baseline, trees });
        }
        assert_eq!(r.at, r.bytes.len());
        Self { models }
    }
}
impl Forecast {
    pub fn predict(&self, x: &[f64; 33]) -> [f64; 3] {
        let mut out = [0.; 3];
        for (i, m) in self.models.iter().enumerate() {
            let mut value = m.baseline;
            for tree in &m.trees {
                let mut at = 0;
                loop {
                    let n = &tree[at];
                    if n.leaf {
                        value += n.value;
                        break;
                    }
                    at = if x[n.feature] <= n.threshold {
                        n.left
                    } else {
                        n.right
                    };
                }
            }
            out[i] = value;
        }
        out
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    include!("model_fixtures.rs");
    #[test]
    fn predictions_match_training_export() {
        let m = Forecast::default();
        for (x, expected) in FIXTURES {
            let got = m.predict(x);
            for i in 0..3 {
                assert!(
                    (got[i] - expected[i]).abs() < 1e-10,
                    "{got:?} vs {expected:?}"
                );
            }
        }
    }
}
