//! Gradient-boosted trees, as `bots/weekend/model/forecast.json` holds them:
//! the reference's forecaster, trained offline on generated sessions. Each node is `[feature,
//! threshold, left, right, value, is_leaf, missing_left]`; a prediction is the
//! baseline plus every tree's leaf. The forecast is antisymmetric: the
//! mirrored market (buys for sells) gets the mirrored forecast.

use serde_json::Value;

/// A node: feature, threshold, left child, right child, value, leaf.
type Node = (usize, f64, usize, usize, f64, bool);

pub struct Forest {
    baseline: f64,
    trees: Vec<Vec<Node>>,
}

impl Forest {
    pub fn parse(json: &str) -> Option<Self> {
        let v: Value = serde_json::from_str(json).ok()?;
        let trees = v["trees"]
            .as_array()?
            .iter()
            .map(|tree| {
                tree.as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|n| {
                        let n = n.as_array()?;
                        Some((
                            n.first()?.as_u64()? as usize,
                            n.get(1)?.as_f64()?,
                            n.get(2)?.as_u64()? as usize,
                            n.get(3)?.as_u64()? as usize,
                            n.get(4)?.as_f64()?,
                            n.get(5)?.as_u64()? != 0,
                        ))
                    })
                    .collect()
            })
            .collect();
        Some(Self {
            baseline: v["baseline"].as_f64()?,
            trees,
        })
    }

    fn raw(&self, x: &[f64]) -> f64 {
        self.baseline
            + self
                .trees
                .iter()
                .map(|tree| {
                    let mut i = 0;
                    loop {
                        let (feature, threshold, left, right, value, leaf) = tree[i];
                        if leaf {
                            break value;
                        }
                        i = if x[feature] <= threshold { left } else { right };
                    }
                })
                .sum::<f64>()
    }

    /// The forecast, made antisymmetric under the market's mirror: the
    /// features in `signed` flip sign.
    pub fn predict(&self, x: &[f64], signed: &[usize]) -> f64 {
        let mut mirrored = x.to_vec();
        for &j in signed {
            mirrored[j] = -mirrored[j];
        }
        0.5 * (self.raw(x) - self.raw(&mirrored))
    }
}
