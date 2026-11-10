//! SPCX shock trader: ride a move while it's moving, fade it once it stops,
//! and stand aside once the aftermath has given it back.

use mtx_kit::prelude::*;
use std::collections::VecDeque;
use tracing::info;

fn envf(name: &str, default: f64) -> f64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

struct Params {
    /// A step move at least this big (fraction) counts as the market moving.
    mom_move: f64,
    /// Steps a move keeps momentum alive.
    mom_hold: u64,
    /// Deviation from anchor below which we don't fade.
    fade_min: f64,
    /// Deviation at which the fade reaches full size.
    fade_full: f64,
    /// Fraction of the extreme given back that ends the shock (aftermath seen).
    done_retrace: f64,
    /// Worst gap assumed for sizing against the drawdown limit.
    gap: f64,
    /// Drawdown (USD) past which we go flat for good.
    halt_dd: f64,
    /// Use momentum at all.
    mom_frac: f64,
}

struct Bot {
    p: Params,
    anchor: f64,
    prev: f64,
    hist: VecDeque<f64>,
    last_move_k: u64,
    last_move_dir: f64,
    extreme: f64, // signed deviation at the extreme of the current shock
    halted: bool,
    started: bool,
}

impl Bot {
    fn new() -> Self {
        Bot {
            p: Params {
                mom_move: envf("B_MOM_MOVE", 0.0025),
                mom_hold: envf("B_MOM_HOLD", 3.0) as u64,
                fade_min: envf("B_FADE_MIN", 0.006),
                fade_full: envf("B_FADE_FULL", 0.015),
                done_retrace: envf("B_DONE", 0.45),
                gap: envf("B_GAP", 0.08),
                halt_dd: envf("B_HALT", 650.0),
                mom_frac: envf("B_MOM_FRAC", 1.0),
            },
            anchor: 0.0,
            prev: 0.0,
            hist: VecDeque::new(),
            last_move_k: 0,
            last_move_dir: 0.0,
            extreme: 0.0,
            halted: false,
            started: false,
        }
    }
}

impl Strategy for Bot {
    async fn step(&mut self, market: &Market, orders: &mut Orders<'_>) -> BotResult {
        let mark = market.mark;
        if !(mark > 0.0) {
            return Ok(());
        }
        if !self.started {
            self.started = true;
            self.anchor = mark;
            self.prev = mark;
        }
        let k = market.k;
        let step_ret = mark / self.prev - 1.0;
        self.prev = mark;
        self.hist.push_back(mark);
        if self.hist.len() > 6 {
            self.hist.pop_front();
        }
        // Moves over the last couple of steps (a jump may land in parts).
        let ret2 = mark / self.hist[self.hist.len().saturating_sub(3)] - 1.0;
        if step_ret.abs() >= self.p.mom_move {
            self.last_move_k = k;
            self.last_move_dir = step_ret.signum();
        } else if ret2.abs() >= self.p.mom_move * 1.5 && step_ret * ret2 > 0.0 {
            self.last_move_k = k;
            self.last_move_dir = ret2.signum();
        }
        let moving = self.last_move_k > 0 && k - self.last_move_k < self.p.mom_hold;

        let dev = mark / self.anchor - 1.0;
        // Track the shock's extreme.
        if self.extreme == 0.0 || dev * self.extreme < 0.0 {
            if dev.abs() >= self.p.fade_min {
                self.extreme = dev;
            }
        } else if dev.abs() > self.extreme.abs() {
            self.extreme = dev;
        }
        // Aftermath seen: the move has given back enough; re-anchor here.
        if self.extreme != 0.0
            && !moving
            && (self.extreme - dev).abs() >= self.p.done_retrace * self.extreme.abs()
            && dev * self.extreme >= -0.002
        {
            info!(k, mark, dev, extreme = self.extreme, "aftermath: re-anchor");
            self.anchor = mark;
            self.extreme = 0.0;
        }
        let dev = mark / self.anchor - 1.0;

        if market.drawdown > self.p.halt_dd {
            self.halted = true;
        }
        let cap = market.cap;
        // Size so a gap against us can't take us past the drawdown limit.
        let room = (market.max_drawdown - market.drawdown - 150.0).max(0.0);
        let safe = (room / (mark * self.p.gap)).min(cap);

        let target = if self.halted {
            0.0
        } else if moving && self.p.mom_frac > 0.0 {
            self.last_move_dir * safe * self.p.mom_frac
        } else if dev.abs() >= self.p.fade_min {
            let f = ((dev.abs() - self.p.fade_min * 0.5) / (self.p.fade_full - self.p.fade_min * 0.5))
                .clamp(0.0, 1.0);
            -dev.signum() * safe * f
        } else {
            0.0
        };
        let target = target.round();
        if (target - market.position.size).abs() >= 1.0 {
            info!(k, mark, dev, moving, target, pos = market.position.size, dd = market.drawdown, "trade");
        }
        orders.target_position_with(
            target,
            Execution {
                slice: 10.0,
                slippage_bps: envf("B_SLIP", 300.0),
                min_trade: 1.0,
                max_per_step: f64::INFINITY,
            },
        );
        Ok(())
    }
}

fn main() -> BotResult {
    mtx_kit::run(Bot::new())
}
