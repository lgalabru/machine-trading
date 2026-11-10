//! Shock trader for SPCX: rides moves while they run, fades fizzles, and
//! positions for the next leg or the aftermath once a shock leg has settled.

use mtx_kit::prelude::*;
use tracing::info;

// All levels and thresholds are in percent of the session's starting mark.
const THR_EV: f64 = 0.22; // a step move this big is an event
const QM: u64 = 3; // steps to keep riding a move after its last event
const TE: u64 = 24; // steps a small move is faded as a possible fizzle
const FZ_MAX: f64 = 2.2;
const FZ_MIN: f64 = 0.35;
const TMAX: u64 = 150; // steps after a leg by which the aftermath has come
const THR_WITH: f64 = 2.0; // below: hold with the shock, for the next leg
const THR_FADE: f64 = 2.0; // above: fade the shock, for the revert
const THR3: f64 = 0.9; // after the third leg: fade above this
const LEG_MIN: f64 = 0.5;
const W_MOM: f64 = 1.0;
const W_FZ: f64 = 1.0;
const W_WITH: f64 = 1.0;
const W_FADE: f64 = 1.0;
const DD_LIM: f64 = 750.0; // keep the drawdown under this, in USD
const STEP_RISK: f64 = 0.05; // the worst one-step move sized against

struct Move {
    l0: f64,
    dir: f64,
    last: u64,
    giveback: bool,
}

#[derive(Default)]
struct Shock {
    start: Option<f64>,
    prev: f64,
    mv: Option<Move>,
    anchor: f64,
    legs: u32,
    leg_end: u64,
    dir: f64,
    over: bool,
    last_pos: f64,
    costs: f64,
    peak: f64,
}

/// What trading `q` SPCX in one step costs, in USD: fees and the book's depth.
fn cost(q: f64) -> f64 {
    if q <= 35.0 {
        0.074 * q
    } else if q <= 70.0 {
        2.6 + 0.143 * (q - 35.0)
    } else {
        7.6 + 0.176 * (q - 70.0)
    }
}

impl Shock {
    fn classify(&mut self, mv: &Move, x: f64) {
        let m = x - mv.l0;
        if m.abs() < LEG_MIN || self.over {
            return;
        }
        let d = m.signum();
        if self.legs == 0 {
            self.anchor = mv.l0;
            self.legs = 1;
            self.leg_end = mv.last;
            self.dir = d;
        } else if d == self.dir {
            self.legs += 1;
            self.leg_end = mv.last;
        } else {
            self.over = true;
        }
    }

    fn context(&mut self, k: u64, x: f64) -> f64 {
        if self.over || self.legs == 0 {
            return 0.0;
        }
        let s = x - self.anchor;
        if k - self.leg_end > TMAX {
            self.over = true;
            return 0.0;
        }
        let sg = if s > 0.0 { 1.0 } else { -1.0 };
        if self.legs >= 3 {
            return if s.abs() >= THR3 { -sg * W_FADE } else { 0.0 };
        }
        if s.abs() < THR_WITH {
            sg * W_WITH
        } else if s.abs() > THR_FADE {
            -sg * W_FADE
        } else {
            0.0
        }
    }

    /// The position wanted at step `k` with the mark at level `x`, as a
    /// fraction of the cap.
    fn decide(&mut self, k: u64, x: f64) -> f64 {
        let px = self.prev;
        let r = x - px;
        self.prev = x;
        if r.abs() >= THR_EV {
            let d = r.signum();
            match &mut self.mv {
                Some(mv) if mv.dir == d => mv.last = k,
                Some(_) => {
                    self.mv = Some(Move { l0: px, dir: d, last: k, giveback: true });
                }
                None => {
                    self.mv = Some(Move { l0: px, dir: d, last: k, giveback: false });
                }
            }
            return d * W_MOM;
        }
        if let Some(mv) = self.mv.take() {
            let a = k - mv.last;
            let m = x - mv.l0;
            if a < QM {
                let t = mv.dir * W_MOM;
                self.mv = Some(mv);
                return t;
            }
            if mv.giveback {
                // The give-back of a fizzle: the shock's clock goes on.
            } else if a <= TE && m.abs() <= FZ_MAX {
                self.mv = Some(mv);
                return if m.abs() >= FZ_MIN { -m.signum() * W_FZ } else { 0.0 };
            } else {
                self.classify(&mv, x);
            }
        }
        self.context(k, x)
    }
}

impl Strategy for Shock {
    async fn step(&mut self, market: &Market, orders: &mut Orders<'_>) -> BotResult {
        let mark = market.mark;
        if !(mark > 0.0) {
            return Ok(());
        }
        // The oracle is the scenario's path; the mark also carries the book.
        let price = if market.oracle > 0.0 { market.oracle } else { mark };
        let pos = market.position.size;
        self.costs += cost((pos - self.last_pos).abs());
        self.last_pos = pos;
        let equity = market.pnl - self.costs;
        self.peak = self.peak.max(equity);
        let dd = self.peak - equity;
        let Some(start) = self.start else {
            self.start = Some(price);
            return Ok(());
        };
        let x = 100.0 * (price / start - 1.0);
        let want = self.decide(market.k, x) * market.cap;
        let lim = ((DD_LIM.min(market.max_drawdown * 0.75) - dd) / (STEP_RISK * mark)).max(0.0);
        let cap = market.cap.min(lim);
        let target = want.clamp(-cap, cap).round();
        if (target - pos).abs() >= 1.0 {
            info!(step = market.k, mark, x, pos, target, legs = self.legs, dd, "trade");
            // Trades that cut the position get more room to fill.
            let cutting = target.abs() < pos.abs() || target * pos < 0.0;
            orders.target_position_with(
                target,
                Execution {
                    slippage_bps: if cutting { 400.0 } else { 200.0 },
                    ..Execution::default()
                },
            );
        }
        Ok(())
    }
}

fn main() -> BotResult {
    mtx_kit::run(Shock::default())
}
