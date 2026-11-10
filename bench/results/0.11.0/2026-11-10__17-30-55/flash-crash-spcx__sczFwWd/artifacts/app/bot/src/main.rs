//! SPCX shock bot: ride a move while it is still landing, fade it once it
//! stops, take profit as it reverts, and keep the position inside a
//! drawdown budget so no single adverse leg can breach the mandate.

use mtx_kit::prelude::*;
use tracing::info;

#[derive(Debug, Clone, Copy, PartialEq)]
enum Mode {
    Flat,
    Momentum,
    Fade,
}

struct ShockBot {
    // Parameters (fractions of the baseline price unless noted).
    entry: f64,
    full: f64,
    tp_frac: f64,
    tp_abs: f64,
    quiet_steps: u64,
    mom_thresh: f64,
    mom_size: f64,
    mom_max: f64,
    base_ema: f64,
    base_band: f64,
    worst_leg: f64,
    buffer_usd: f64,
    cost_bps: f64,
    // State.
    base: Option<f64>,
    prev_mark: Option<f64>,
    last_move_k: i64,
    mode: Mode,
    entry_disp: f64,
    est_costs: f64,
    traded: f64,
}

impl Default for ShockBot {
    fn default() -> Self {
        ShockBot {
            entry: 0.010,
            full: 0.020,
            tp_frac: 0.8,
            tp_abs: 0.0025,
            quiet_steps: 3,
            mom_thresh: 0.004,
            mom_size: 1.0,
            mom_max: 0.03,
            base_ema: 40.0,
            base_band: 0.004,
            worst_leg: 0.085,
            buffer_usd: 150.0,
            cost_bps: 10.0,
            base: None,
            prev_mark: None,
            last_move_k: -1000,
            mode: Mode::Flat,
            entry_disp: 0.0,
            est_costs: 0.0,
            traded: 0.0,
        }
    }
}

impl ShockBot {
    fn fade_size(&self, cap: f64, d: f64) -> f64 {
        let x = ((d.abs() - self.entry) / (self.full - self.entry)).clamp(0.0, 1.0);
        cap * (0.3 + 0.7 * x)
    }
}

impl Strategy for ShockBot {
    async fn start(&mut self, session: &Session) -> BotResult {
        info!(
            symbol = session.env.symbol,
            steps = session.hello.steps,
            cap = session.env.max_position,
            "started"
        );
        Ok(())
    }

    async fn step(&mut self, market: &Market, orders: &mut Orders<'_>) -> BotResult {
        let mark = market.mark;
        if !(mark > 0.0) {
            return Ok(());
        }
        let cap = market.cap;
        let pos = market.position.size;
        let base = *self.base.get_or_insert(mark);
        let d = (mark - base) / base;
        let step_move = match self.prev_mark {
            Some(p) => (mark - p) / base,
            None => 0.0,
        };
        self.prev_mark = Some(mark);
        let k = market.k as i64;
        if step_move.abs() > self.mom_thresh {
            self.last_move_k = k;
        }
        let stopped = (k - self.last_move_k) as u64 >= self.quiet_steps;

        // Baseline drifts with the mark only while flat and near it.
        if d.abs() < self.base_band && pos.abs() < 0.5 {
            self.base = Some(base + (mark - base) / self.base_ema);
        }

        // Drawdown budget: the grader's drawdown counts fees and slippage,
        // which `market.drawdown` leaves out, so add an estimate.
        let pnl_net = market.pnl - self.est_costs;
        let peak_net = market.peak_pnl.max(0.0);
        let dd = (peak_net - pnl_net).max(0.0);
        let room = market.max_drawdown - dd;
        let cap_eff = ((room - self.buffer_usd) / (self.worst_leg * mark))
            .clamp(0.0, cap);

        let mut target = pos;
        let mut mode = self.mode;
        if d.abs() > self.entry && !stopped && self.mom_size > 0.0 && d.abs() < self.mom_max {
            target = (cap * self.mom_size).copysign(d);
            mode = Mode::Momentum;
        } else if d.abs() > self.entry
            && stopped
            && (self.mode != Mode::Fade || d.abs() > self.entry_disp.abs())
        {
            target = -self.fade_size(cap, d).copysign(d);
            mode = Mode::Fade;
            self.entry_disp = d;
        } else if self.mode == Mode::Fade
            && (d.abs() < self.tp_abs
                || (self.entry_disp != 0.0 && (self.entry_disp - d) / self.entry_disp > self.tp_frac))
        {
            target = 0.0;
            mode = Mode::Flat;
            self.base = Some(mark);
            self.entry_disp = 0.0;
        } else if self.mode == Mode::Momentum && stopped {
            target = 0.0;
            mode = Mode::Flat;
        }
        if target.abs() > cap_eff {
            target = cap_eff.copysign(target);
        }
        self.mode = mode;

        let delta = target - pos;
        if delta.abs() >= 1.0 {
            self.traded += delta.abs();
            self.est_costs += delta.abs() * mark * self.cost_bps / 1e4;
            orders.target_position(target);
        }
        info!(
            step = market.k,
            mark,
            oracle = market.oracle,
            base,
            d = format!("{:.4}", d),
            pos,
            target,
            cap_eff = format!("{:.1}", cap_eff),
            mode = ?self.mode,
            pnl = format!("{:.1}", market.pnl),
            dd = format!("{:.1}", dd),
            "step"
        );
        Ok(())
    }

    fn end(&mut self, last: Option<&Market>) {
        if let Some(m) = last {
            info!(pnl = m.pnl, traded = self.traded, est_costs = self.est_costs, "end");
        }
    }
}

fn main() -> BotResult {
    mtx_kit::run(ShockBot::default())
}
