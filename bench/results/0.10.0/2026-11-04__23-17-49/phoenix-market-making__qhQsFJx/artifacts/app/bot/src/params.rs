//! Strategy parameters. The defaults are what the bot trades with; a
//! `params.json` next to `run.sh` overrides them, for tuning.

use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Params {
    /// An order is informed-looking if its limit is at least this many
    /// competitor half-spreads from the price...
    pub k_lim: f64,
    /// ...and it is at least this big, in SPCX.
    pub min_size: f64,
    /// This many informed-looking orders one way within `window` slots
    /// trigger a burst.
    pub n_trig: usize,
    pub window: u64,
    /// A burst with no jump this many slots after its first order is a
    /// fake-out.
    pub deadline: u64,
    /// A one-step oracle move this big is a jump.
    pub jump_thr: f64,
    /// Jumps land at least this far from the session's ends and each other.
    pub edge_slots: u64,
    pub filters: bool,
    /// Position taken with a burst, in SPCX.
    pub qmax: f64,
    /// Stay this far inside the position cap.
    pub cap_margin: f64,
    /// Don't bother taking less than this.
    pub min_take: f64,
    /// Taking limit: this far past the competitor half-spread (fraction).
    pub take_extra: f64,
    /// Market making: quote size, closest distance to the oracle (bps),
    /// inventory skew (bps per SPCX), minimum edge after skew (bps).
    pub mm_size: f64,
    pub mm_min_bps: f64,
    pub skew_bps: f64,
    pub min_edge_bps: f64,
    pub unwind_size: f64,
    pub min_quote: f64,
    /// Drawdown guard: keep the worst jump (fraction of the price) within
    /// this share of the drawdown limit.
    pub worst_jump: f64,
    pub dd_budget: f64,
    /// Before any jump, skip bursts when the trend z-score is beyond this.
    pub trend_skip_z: f64,
    /// Inventory lean with the trend: lean_gain * (|z| - lean_z0), capped.
    pub lean_z0: f64,
    pub lean_gain: f64,
    pub lean_max: f64,
}

impl Default for Params {
    fn default() -> Self {
        Self {
            k_lim: 2.2,
            min_size: 0.9,
            n_trig: 3,
            window: 24,
            deadline: 30,
            jump_thr: 0.009,
            edge_slots: 58,
            filters: true,
            qmax: 19.0,
            cap_margin: 0.6,
            min_take: 0.3,
            take_extra: 0.0010,
            mm_size: 1.0,
            mm_min_bps: 3.0,
            skew_bps: 0.5,
            min_edge_bps: 0.6,
            unwind_size: 3.0,
            min_quote: 0.05,
            worst_jump: 0.045,
            dd_budget: 0.9,
            trend_skip_z: 2.0,
            lean_z0: 1.0,
            lean_gain: 4.0,
            lean_max: 10.0,
        }
    }
}

impl Params {
    pub fn load() -> Self {
        let dir = std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().and_then(|d| d.parent()).map(|d| d.to_path_buf()));
        if let Some(dir) = dir {
            let path = dir.join("params.json");
            if let Ok(text) = std::fs::read_to_string(&path) {
                match serde_json::from_str(&text) {
                    Ok(p) => return p,
                    Err(e) => eprintln!("params.json: {e}"),
                }
            }
        }
        Self::default()
    }
}
