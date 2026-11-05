//! Gradient-boosted regression trees, trained offline on simulated sessions
//! to forecast the index's forward return from `Strategy::features`.

/// One tree node: split feature (-1 for a leaf), threshold (go left when
/// the feature is at most it), children, and the leaf's value.
#[derive(Clone, Copy)]
pub struct Node {
    f: i32,
    thr: f32,
    l: u32,
    r: u32,
    v: f32,
}

pub struct Gbt {
    base: f32,
    trees: std::sync::Arc<Vec<Vec<Node>>>,
}

impl Gbt {
    /// Parses the trainer's JSON: `{"base": b, "trees": [[[f, thr, l, r, v], ...], ...]}`.
    pub fn from_json(s: &str) -> Result<Self, String> {
        let v: serde_json::Value = serde_json::from_str(s).map_err(|e| e.to_string())?;
        let base = v["base"].as_f64().ok_or("no base")? as f32;
        let mut trees = Vec::new();
        for t in v["trees"].as_array().ok_or("no trees")? {
            let mut nodes = Vec::new();
            for n in t.as_array().ok_or("bad tree")? {
                let a = n.as_array().ok_or("bad node")?;
                let num = |k: usize| a.get(k).and_then(|x| x.as_f64()).ok_or("bad node field");
                nodes.push(Node {
                    f: num(0)? as i32,
                    thr: num(1)? as f32,
                    l: num(2)? as u32,
                    r: num(3)? as u32,
                    v: num(4)? as f32,
                });
            }
            trees.push(nodes);
        }
        Ok(Self { base, trees: std::sync::Arc::new(trees) })
    }

    /// Another handle on the same trees.
    pub fn clone_ref(&self) -> Self {
        Self { base: self.base, trees: self.trees.clone() }
    }

    pub fn predict(&self, x: &[f32]) -> f32 {
        let mut s = self.base;
        for t in self.trees.iter() {
            let mut k = 0usize;
            loop {
                let n = &t[k];
                if n.f < 0 {
                    s += n.v;
                    break;
                }
                k = if x[n.f as usize] <= n.thr { n.l as usize } else { n.r as usize };
            }
        }
        s
    }
}
