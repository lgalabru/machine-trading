//! SPCX shock fader: fades displacements from a settled anchor once the move
//! pauses, takes profit after a retrace, and guards the drawdown limit.

use mtx_kit::prelude::*;
use tracing::info;

/// Parameters, read from the environment where present (for sweeps).
struct Params {
    enter_pct: f64,      // minimum displacement to fade
    full_pct: f64,       // displacement at which the fade is full size
    pause_pct: f64,      // a step moving less than this counts as a pause
    retrace_frac: f64,   // retrace fraction (of the extreme) to take profit
    retrace_done: f64,   // retrace fraction to take profit regardless of pause
    worst_step_pct: f64, // worst adverse move assumed in one step
    dd_budget: f64,      // drawdown we keep under (with the worst step)
    cost_bps: f64,       // fee + slippage estimate per traded notional
    anchor_alpha: f64,   // anchor drift toward mark while flat and quiet
    pause_steps: u32,    // consecutive quiet steps before entering/adding
    stop_pct: f64,       // adverse move beyond the fade's entry that stops it out
}

fn envf(name: &str, default: f64) -> f64 {
    std::env::var(name).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}

impl Params {
    fn load() -> Self {
        Params {
            enter_pct: envf("BOT_ENTER_PCT", 0.5),
            full_pct: envf("BOT_FULL_PCT", 1.4),
            pause_pct: envf("BOT_PAUSE_PCT", 0.2),
            retrace_frac: envf("BOT_RETRACE_FRAC", 0.5),
            retrace_done: envf("BOT_RETRACE_DONE", 0.85),
            worst_step_pct: envf("BOT_WORST_STEP_PCT", 4.5),
            dd_budget: envf("BOT_DD_BUDGET", 830.0),
            cost_bps: envf("BOT_COST_BPS", 25.0),
            anchor_alpha: envf("BOT_ANCHOR_ALPHA", 0.05),
            pause_steps: envf("BOT_PAUSE_STEPS", 3.0) as u32,
            stop_pct: envf("BOT_STOP_PCT", 1.0),
        }
    }
}

fn size_for(p: &Params, cap: f64, disp: f64) -> f64 {
    let d = disp * 100.0;
    let x = ((d - p.enter_pct) / (p.full_pct - p.enter_pct)).clamp(0.0, 1.0);
    // Start at a third of the cap at the entry threshold, full at full_pct.
    let frac = 0.33 + 0.67 * x;
    (cap * frac).floor()
}

#[derive(Default)]
struct Fade {
    dir: f64,     // +1: price displaced up (we are short), -1: down (we are long)
    extreme: f64, // largest |displacement| (fraction) seen since entry
    base: f64,    // |displacement| at entry (or last add): the stop is measured from it
    size: f64,    // size we hold / aim for (positive)
}

struct Bot {
    p: Params,
    anchor: Option<f64>,
    prev_mark: Option<f64>,
    fade: Option<Fade>,
    // Own account-value estimate: mark-to-market PnL minus trading costs.
    value: f64,
    peak: f64,
    prev_pos: f64,
    cap: f64,
    quiet: u32, // consecutive steps with a small move
    base_value: Option<f64>,
}

impl Bot {
    fn new() -> Self {
        Bot {
            p: Params::load(),
            anchor: None,
            prev_mark: None,
            fade: None,
            value: 0.0,
            peak: 0.0,
            prev_pos: 0.0,
            cap: 70.0,
            quiet: 0,
            base_value: None,
        }
    }

    /// Largest |position| the drawdown guard allows right now.
    fn guard_limit(&self, mark: f64) -> f64 {
        let dd = (self.peak - self.value).max(0.0);
        let room = (self.p.dd_budget - dd).max(0.0);
        (room / (mark * self.p.worst_step_pct / 100.0)).floor().min(self.cap)
    }
}

impl Strategy for Bot {
    async fn start(&mut self, session: &Session) -> BotResult {
        self.cap = session.env.max_position;
        info!(steps = session.hello.steps, cap = self.cap, "started");
        Ok(())
    }

    async fn step(&mut self, m: &Market, orders: &mut Orders<'_>) -> BotResult {
        let mark = m.mark;
        let pos = m.position.size;
        // Account value estimate: PnL on the position held over the step,
        // minus costs of what was traded (position change since last step).
        if let Some(pm) = self.prev_mark {
            self.value += self.prev_pos * (mark - pm);
            let traded = (pos - self.prev_pos).abs();
            self.value -= traded * mark * self.p.cost_bps / 10_000.0;
        }
        // Prefer the account's true value from the chain: collateral (which
        // carries fees and realised PnL) plus unrealised PnL at the mark.
        if let Some(t) = orders.trader() {
            match t.phoenix.hawkeye(t.authority()).view_margin(t.authority(), 0, 0).await {
                Ok(v) => {
                    let collateral = v.value.collateral_quote_lots as f64 / 1e6;
                    let real = collateral + pos * (mark - m.position.entry_usd);
                    if self.base_value.is_none() {
                        self.base_value = Some(real);
                    }
                    let real_pnl = real - self.base_value.unwrap();
                    if m.k % 20 == 0 {
                        info!(step = m.k, collateral, real_pnl, estimate = self.value, "value check");
                    }
                    self.value = real_pnl;
                }
                Err(e) => info!(step = m.k, error = %e, "view_margin failed; using estimate"),
            }
        }
        self.peak = self.peak.max(self.value);
        let dd = self.peak - self.value;
        let step_move = self.prev_mark.map(|pm| (mark - pm) / pm).unwrap_or(0.0);
        self.prev_mark = Some(mark);
        self.prev_pos = pos;

        let anchor = *self.anchor.get_or_insert(mark);
        let disp = (mark - anchor) / anchor; // signed displacement fraction
        if step_move.abs() * 100.0 < self.p.pause_pct {
            self.quiet += 1;
        } else {
            self.quiet = 0;
        }
        let paused = self.quiet >= self.p.pause_steps;
        let limit = self.guard_limit(mark);

        let mut target = pos;
        let (p, cap) = (&self.p, self.cap);
        match &mut self.fade {
            None => {
                if disp.abs() * 100.0 >= self.p.enter_pct && paused {
                    let dir = disp.signum();
                    let size = size_for(p, cap, disp.abs()).min(limit);
                    if size >= 1.0 {
                        self.fade = Some(Fade { dir, extreme: disp.abs(), base: disp.abs(), size });
                        target = -dir * size;
                        info!(step = m.k, mark, anchor, disp_pct = disp * 100.0, size, "enter fade");
                    }
                } else if disp.abs() * 100.0 < self.p.enter_pct {
                    // Quiet: let the anchor drift toward the mark.
                    self.anchor = Some(anchor + p.anchor_alpha * (mark - anchor));
                }
            }
            Some(f) => {
                let along = disp * f.dir; // displacement along the fade's direction
                let retrace = 1.0 - along / f.extreme;
                let retrace_paused = (step_move * f.dir) * 100.0 > -self.p.pause_pct;
                if (along - f.base) * 100.0 >= p.stop_pct {
                    // A new leg (or an extension) against the fade: stop out,
                    // keep the anchor, and re-enter from the new extreme once
                    // the move pauses.
                    info!(step = m.k, mark, along_pct = along * 100.0, base_pct = f.base * 100.0, "stop out");
                    target = 0.0;
                    self.fade = None;
                } else if retrace >= self.p.retrace_done || (retrace >= self.p.retrace_frac && retrace_paused) {
                    info!(step = m.k, mark, retrace, "exit fade, re-anchor");
                    target = 0.0;
                    self.anchor = Some(mark);
                    self.fade = None;
                } else {
                    if along > f.extreme {
                        f.extreme = along;
                    }
                    if paused {
                        let want = size_for(p, cap, f.extreme);
                        if want > f.size {
                            f.size = want;
                            f.base = along;
                        }
                    }
                    let allowed = f.size.min(limit);
                    if allowed < pos.abs() - 2.5 {
                        // Guard cut: realise and keep the smaller size.
                        f.size = allowed;
                        target = -f.dir * allowed;
                        info!(step = m.k, dd, allowed, "guard cut");
                    } else if f.size.min(limit) > pos.abs() + 2.5 && paused {
                        target = -f.dir * f.size.min(limit);
                    }
                }
            }
        }

        info!(
            step = m.k, mark, oracle = m.oracle, pos, disp_pct = disp * 100.0,
            value = self.value, dd, limit, target, "step"
        );
        if (target - pos).abs() >= 1.0 {
            orders.target_position(target);
        }
        Ok(())
    }
}

fn main() -> BotResult {
    mtx_kit::run(Bot::new())
}
