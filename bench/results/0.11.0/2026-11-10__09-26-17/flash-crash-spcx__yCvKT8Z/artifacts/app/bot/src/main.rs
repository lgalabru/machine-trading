//! Shock trader for SPCX: ride a move while it runs, fade it once it stops,
//! with stops and a drawdown guard.

use mtx_kit::prelude::*;
use std::collections::HashMap;
use tracing::info;

#[derive(Clone, Debug)]
struct Params {
    /// Move over `look` steps (fraction) that starts a ride.
    trigger: f64,
    look: usize,
    /// Ride ends when no new extreme for this many steps...
    stall_steps: u64,
    /// ...or the mark retraces this far (fraction) from the extreme.
    ride_retrace: f64,
    /// Fraction of cap used for rides.
    ride_frac: f64,
    /// Rides take a position only once the move so far is this big.
    ride_min: f64,
    /// Smallest shock (fraction of anchor) worth fading.
    fade_min: f64,
    /// Fraction of cap used for fades.
    fade_frac: f64,
    /// Fade exits once this fraction of the shock is given back.
    fade_take: f64,
    /// Fade stops out (and rides) when the mark passes the extreme by this.
    fade_stop: f64,
    /// Don't open fades with fewer steps left than this.
    min_steps_left: u64,
    /// Drawdown (USD) at which we go flat for good.
    dd_halt: f64,
    slippage_bps: f64,
    /// Steps of history for the anchor (quiet level).
    anchor_steps: usize,
}

impl Default for Params {
    fn default() -> Self {
        Params {
            trigger: 0.006,
            look: 3,
            stall_steps: 4,
            ride_retrace: 0.004,
            ride_frac: 1.0,
            ride_min: 0.02,
            fade_min: 0.005,
            fade_frac: 1.0,
            fade_take: 0.85,
            fade_stop: 0.012,
            min_steps_left: 20,
            dd_halt: 400.0,
            slippage_bps: 150.0,
            anchor_steps: 10,
        }
    }
}

impl Params {
    fn load() -> Params {
        let mut p = Params::default();
        let path = std::env::current_exe()
            .ok()
            .and_then(|e| e.parent().map(|d| d.join("../params.txt")));
        let Some(path) = path else { return p };
        let Ok(text) = std::fs::read_to_string(path) else { return p };
        let kv: HashMap<String, f64> = text
            .lines()
            .filter_map(|l| {
                let (k, v) = l.split_once('=')?;
                Some((k.trim().to_string(), v.trim().parse().ok()?))
            })
            .collect();
        let g = |k: &str, d: f64| kv.get(k).copied().unwrap_or(d);
        p.trigger = g("trigger", p.trigger);
        p.look = g("look", p.look as f64) as usize;
        p.stall_steps = g("stall_steps", p.stall_steps as f64) as u64;
        p.ride_retrace = g("ride_retrace", p.ride_retrace);
        p.ride_frac = g("ride_frac", p.ride_frac);
        p.ride_min = g("ride_min", p.ride_min);
        p.fade_min = g("fade_min", p.fade_min);
        p.fade_frac = g("fade_frac", p.fade_frac);
        p.fade_take = g("fade_take", p.fade_take);
        p.fade_stop = g("fade_stop", p.fade_stop);
        p.min_steps_left = g("min_steps_left", p.min_steps_left as f64) as u64;
        p.dd_halt = g("dd_halt", p.dd_halt);
        p.slippage_bps = g("slippage_bps", p.slippage_bps);
        p.anchor_steps = g("anchor_steps", p.anchor_steps as f64) as usize;
        p
    }
}

#[derive(Clone, Debug)]
enum Mode {
    Flat,
    /// Riding a move in `dir` that started from `anchor`.
    Ride { dir: f64, anchor: f64, extreme: f64, extreme_k: u64 },
    /// Fading a shock in `dir` (the shock's direction) from `anchor` to `extreme`.
    Fade { dir: f64, anchor: f64, extreme: f64 },
    Halted,
}

struct Shock {
    p: Params,
    marks: Vec<f64>,
    mode: Mode,
    target: f64,
}

impl Default for Shock {
    fn default() -> Self {
        Shock { p: Params::load(), marks: Vec::new(), mode: Mode::Flat, target: 0.0 }
    }
}

impl Shock {
    fn anchor(&self) -> f64 {
        // Median of the marks before the last `look` steps.
        let n = self.marks.len();
        let end = n.saturating_sub(self.p.look);
        let start = end.saturating_sub(self.p.anchor_steps);
        let mut v: Vec<f64> = self.marks[start..end.max(start)].to_vec();
        if v.is_empty() {
            return self.marks[0];
        }
        v.sort_by(|a, b| a.partial_cmp(b).unwrap());
        v[v.len() / 2]
    }

    fn decide(&mut self, m: &Market) {
        let p = self.p.clone();
        let mark = m.mark;
        let k = m.k;
        let n = self.marks.len();
        let past = self.marks[n.saturating_sub(p.look + 1)];
        let chg = mark / past - 1.0;
        let cap = m.cap;
        match self.mode.clone() {
            Mode::Halted => {
                self.target = 0.0;
            }
            Mode::Flat => {
                if chg.abs() >= p.trigger {
                    let dir = chg.signum();
                    let anchor = self.anchor();
                    self.mode = Mode::Ride { dir, anchor, extreme: mark, extreme_k: k };
                    let sofar = (mark - anchor) * dir / anchor;
                    self.target = if sofar >= p.ride_min { dir * cap * p.ride_frac } else { 0.0 };
                    info!(k, mark, dir, anchor, "ride start");
                } else {
                    self.target = 0.0;
                }
            }
            Mode::Ride { dir, anchor, extreme, extreme_k } => {
                let (extreme, extreme_k) = if (mark - extreme) * dir > 0.0 {
                    (mark, k)
                } else {
                    (extreme, extreme_k)
                };
                let retr = (extreme - mark) * dir / extreme;
                if k - extreme_k >= p.stall_steps || retr >= p.ride_retrace {
                    let shock = (extreme - anchor) * dir / anchor;
                    if shock >= p.fade_min && m.steps_left >= p.min_steps_left {
                        self.mode = Mode::Fade { dir, anchor, extreme };
                        self.target = -dir * cap * p.fade_frac;
                        info!(k, mark, shock, "fade start");
                    } else {
                        self.mode = Mode::Flat;
                        self.target = 0.0;
                        info!(k, mark, shock, "ride end");
                    }
                } else {
                    self.mode = Mode::Ride { dir, anchor, extreme, extreme_k };
                    let sofar = (mark - anchor) * dir / anchor;
                    self.target = if sofar >= p.ride_min { dir * cap * p.ride_frac } else { 0.0 };
                }
            }
            Mode::Fade { dir, anchor, extreme } => {
                let given = (extreme - mark) * dir / ((extreme - anchor) * dir);
                let past_ext = (mark - extreme) * dir / extreme;
                if past_ext >= p.fade_stop {
                    // A new leg or an extension: ride it, keeping the old anchor.
                    self.mode = Mode::Ride { dir, anchor, extreme: mark, extreme_k: k };
                    self.target = dir * cap * p.ride_frac;
                    info!(k, mark, "fade stopped; riding");
                } else if given >= p.fade_take {
                    self.mode = Mode::Flat;
                    self.target = 0.0;
                    info!(k, mark, given, "fade done");
                }
            }
        }
        if m.drawdown >= p.dd_halt {
            self.mode = Mode::Halted;
            self.target = 0.0;
        }
    }
}

impl Strategy for Shock {
    async fn start(&mut self, session: &Session) -> BotResult {
        info!(steps = session.hello.steps, params = ?self.p, "started");
        Ok(())
    }

    async fn step(&mut self, market: &Market, orders: &mut Orders<'_>) -> BotResult {
        self.marks.push(market.mark);
        if self.marks.len() > 1 {
            self.decide(market);
        }
        info!(k = market.k, mark = market.mark, pos = market.position.size, pnl = market.pnl, dd = market.drawdown, target = self.target, "step");
        if (self.target - market.position.size).abs() >= 1.0 {
            orders.target_position_with(
                self.target,
                Execution { slippage_bps: self.p.slippage_bps, ..Execution::default() },
            );
        }
        Ok(())
    }
}

fn main() -> BotResult {
    mtx_kit::run(Shock::default())
}
