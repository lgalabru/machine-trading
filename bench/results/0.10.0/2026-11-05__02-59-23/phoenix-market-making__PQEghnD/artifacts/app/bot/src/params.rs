//! Tunables. Defaults are the graded values; `$HOME/bot-params.json` or
//! `/tmp/bot-params.json` may override them for local experiments.

use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Params {
    /// Target half-spread as a fraction of the competitor half-spread.
    pub rho: f64,
    /// Quote one tick inside the competitor when it is tighter than rho*C.
    pub inside: bool,
    /// Never quote tighter than this many bps from the oracle.
    pub h_floor_bps: f64,
    /// Never quote wider than this multiple of C.
    pub h_max_mult: f64,
    /// Base quote size per side, in base units.
    pub size: f64,
    /// Inventory skew, bps of quote shift per base unit of inventory.
    pub skew_bps_per_unit: f64,
    /// Inventory beyond which the adding side's size shrinks linearly to 0.
    pub q_soft: f64,
    /// Requote when the resting price is off target by more than this many ticks.
    pub requote_ticks: i64,
    /// Requote when the resting size is below this fraction of target.
    pub requote_size_frac: f64,
    /// Burst detector: window in slots, min same-direction orders, min size.
    pub det_window: u64,
    pub det_n: usize,
    pub det_size: f64,
    pub det_vol: f64,
    /// Volume alone (with at least two orders) that also triggers.
    pub det_vol_alone: f64,
    /// Pull the quote facing one-sided flow at this many orders / volume.
    pub pull_n: usize,
    pub pull_vol: f64,
    /// Fraction of the position cap taken on a trigger.
    pub bet_frac: f64,
    /// Slots after the trigger without a jump before calling it a fake-out.
    pub bet_timeout: u64,
    /// After a fake-out, ignore same-direction triggers for this many slots.
    pub cooldown_slots: u64,
    /// Oracle move (fraction) in one step that counts as a jump.
    pub jump_frac: f64,
    /// Take a competitor quote crossed against the oracle by this many bps.
    pub pickoff_bps: f64,
    /// Trend lean: max target inventory and gains.
    pub lean_max: f64,
    pub lean_gain_flow: f64,
    pub lean_gain_drift: f64,
    /// Drawdown (USD from peak) past which risk is cut, then stopped.
    pub dd_soft: f64,
    pub dd_hard: f64,
    /// Fraction of a market order that must still fit under the position cap.
    pub enabled_bets: bool,
    pub enabled_pickoff: bool,
    pub enabled_lean: bool,
}

impl Default for Params {
    fn default() -> Self {
        Self {
            rho: 0.8,
            inside: true,
            h_floor_bps: 2.5,
            h_max_mult: 0.93,
            size: 2.0,
            skew_bps_per_unit: 0.6,
            q_soft: 8.0,
            requote_ticks: 3,
            requote_size_frac: 0.5,
            det_window: 10,
            det_n: 4,
            det_size: 0.3,
            det_vol: 5.0,
            det_vol_alone: 7.0,
            pull_n: 2,
            pull_vol: 2.5,
            bet_frac: 1.0,
            bet_timeout: 30,
            cooldown_slots: 30,
            jump_frac: 0.008,
            pickoff_bps: 12.0,
            lean_max: 10.0,
            lean_gain_flow: 40.0,
            lean_gain_drift: 5.0,
            dd_soft: 120.0,
            dd_hard: 180.0,
            enabled_bets: true,
            enabled_pickoff: true,
            enabled_lean: true,
        }
    }
}

impl Params {
    pub fn load() -> Self {
        let mut candidates = vec![std::path::PathBuf::from("/tmp/bot-params.json")];
        if let Some(home) = std::env::var_os("HOME") {
            candidates.insert(0, std::path::PathBuf::from(home).join("bot-params.json"));
        }
        for path in candidates {
            if let Ok(text) = std::fs::read_to_string(&path) {
                match serde_json::from_str::<Params>(&text) {
                    Ok(p) => {
                        tracing::info!(path = %path.display(), "loaded params");
                        return p;
                    }
                    Err(e) => tracing::warn!(path = %path.display(), error = %e, "bad params file"),
                }
            }
        }
        Self::default()
    }
}
