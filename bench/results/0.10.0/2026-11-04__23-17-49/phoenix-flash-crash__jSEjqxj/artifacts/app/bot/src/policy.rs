//! Turns forecasts into a position: the rules' target blended with the
//! models' view of the forward return, within the drawdown budget.

use crate::model::Gbt;
use crate::strategy::{Params, Strategy};

#[derive(Clone, Debug)]
pub struct PolicyParams {
    /// Weight of each model's forecast (a forward return).
    pub w: Vec<f64>,
    /// Forecast that earns the full position.
    pub scale: f64,
    /// Weight of the rules' target (as a share of the cap).
    pub mix: f64,
    /// Don't trade changes smaller than this (shares).
    pub band: f64,
}

impl Default for PolicyParams {
    fn default() -> Self {
        Self { w: vec![1.0], scale: 0.002, mix: 0.0, band: 10.0 }
    }
}

pub struct Policy {
    pub strat: Strategy,
    pub models: Vec<Gbt>,
    pub pp: PolicyParams,
    pub signal: f64,
}

impl Policy {
    pub fn new(p: Params, models: Vec<Gbt>, pp: PolicyParams) -> Self {
        Self { strat: Strategy::new(p), models, pp, signal: 0.0 }
    }

    /// The position to hold after seeing index `px` with the account worth
    /// `value`, `remaining` steps before the end, holding `pos`.
    pub fn step(&mut self, px: f64, value: f64, remaining: f64, pos: f64) -> f64 {
        let rules = self.strat.step(px, value);
        let cap = self.strat.cap();
        if self.models.is_empty() {
            return rules;
        }
        let x = self.strat.features(remaining);
        let f: f64 = self
            .models
            .iter()
            .zip(self.pp.w.iter())
            .map(|(m, w)| w * m.predict(&x) as f64)
            .sum();
        self.signal = f;
        let max = self.strat.p.max_pos;
        let z = f / self.pp.scale + self.pp.mix * rules / max;
        let tgt = (z.clamp(-1.0, 1.0) * max).clamp(-cap, cap);
        if (tgt - pos).abs() < self.pp.band && pos.abs() <= cap {
            pos
        } else {
            tgt
        }
    }
}
