//! SPCX market maker: quotes both sides around the oracle, skews against
//! inventory, detects one-sided taker bursts (informed flow before jumps) and
//! leans into them, and leans with a detected drift.

use mtx_kit::prelude::*;
use std::collections::VecDeque;
use tracing::info;

fn envf(name: &str, default: f64) -> f64 {
    std::env::var(name).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}

#[derive(Clone, Copy)]
struct Params {
    // quoting
    spread_frac: f64,   // our half-spread as a fraction of the competitor's
    min_half_bps: f64,  // never tighter than this
    quote_size: f64,
    skew_per_unit: f64, // bps of quote shift per unit of inventory (relative to target)
    // burst detection
    flow_decay: f64,    // per-step EWMA factor
    flow_trigger: f64,  // trigger on |flow| (SPCX)
    lean_size: f64,     // target position when leaning into a burst
    flow_full: f64,     // |flow| for a full-size lean
    flow_pull: f64,     // |flow| at which the hit side is pulled
    lean_hold: u64,     // steps to hold the lean
    // trend
    trend_min_moves: usize,
    trend_z: f64,
    trend_size: f64,
    // risk
    dd_guard: f64,
}

impl Params {
    fn from_env() -> Self {
        Params {
            spread_frac: envf("BOT_SPREAD_FRAC", 0.55),
            min_half_bps: envf("BOT_MIN_HALF_BPS", 2.5),
            quote_size: envf("BOT_QUOTE_SIZE", 1.5),
            skew_per_unit: envf("BOT_SKEW", 0.6),
            flow_decay: envf("BOT_FLOW_DECAY", 0.75),
            flow_trigger: envf("BOT_FLOW_TRIGGER", 1.8),
            lean_size: envf("BOT_LEAN_SIZE", 20.0),
            flow_full: envf("BOT_FLOW_FULL", 0.0),
            flow_pull: envf("BOT_FLOW_PULL", 1e9),
            lean_hold: envf("BOT_LEAN_HOLD", 22.0) as u64,
            trend_min_moves: envf("BOT_TREND_MIN", 25.0) as usize,
            trend_z: envf("BOT_TREND_Z", 2.5),
            trend_size: envf("BOT_TREND_SIZE", 12.0),
            dd_guard: envf("BOT_DD_GUARD", 200.0),
        }
    }
}

#[derive(Default)]
struct Bot {
    p: Option<Params>,
    comp_bps: f64,
    last_pos: Option<f64>,
    flow: f64,
    // own pnl estimate: cash + pos * oracle
    cash: f64,
    peak_value: f64,
    last_bid: f64,
    last_ask: f64,
    // lean state
    lean_dir: f64,
    lean_until: u64,
    // trend
    last_oracle: Option<f64>,
    moves: VecDeque<f64>,
    trend_dir: f64,
    start_oracle: f64,
    sent_market: f64,
    last_target: f64,
    flatten_pending: bool,
    lean_full: bool,
}

impl Bot {
    fn p(&self) -> Params {
        self.p.unwrap()
    }
}

impl Strategy for Bot {
    async fn start(&mut self, session: &Session) -> BotResult {
        self.p = Some(Params::from_env());
        info!(symbol = session.env.symbol, steps = session.hello.steps, "started");
        Ok(())
    }

    async fn step(&mut self, m: &Market, orders: &mut Orders<'_>) -> BotResult {
        let px = m.oracle;
        let pos = m.position.size;
        if self.start_oracle == 0.0 {
            self.start_oracle = px;
        }

        // --- fills since last step (approximate fill price by our quotes) ---
        if let Some(lp) = self.last_pos {
            let mut d = pos - lp;
            // our own market orders: not taker flow; assume they filled at about the competitor's price
            if self.sent_market.abs() > 1e-9 {
                let own = if d.signum() == self.sent_market.signum() { d.abs().min(self.sent_market.abs()) * d.signum() } else { 0.0 };
                let own_px = if own > 0.0 { px * (1.0 + (self.comp_bps + 3.5) / 1e4) } else { px * (1.0 - (self.comp_bps + 3.5) / 1e4) };
                self.cash -= own * own_px;
                d -= own;
            }
            if d.abs() > 1e-9 {
                let fill_px = if d > 0.0 { self.last_bid } else { self.last_ask };
                let fill_px = if fill_px > 0.0 { fill_px } else { px };
                self.cash -= d * fill_px * (1.0 + 0.5e-4 * d.signum());
                // taker side: they sold to us if d>0 → negative flow (takers selling)
                // fills our inventory skew invited are less informative
                let inv = lp - self.last_target;
                let invited = (d > 0.0 && inv < -2.0) || (d < 0.0 && inv > 2.0);
                let w = if invited { 0.4 } else { 1.0 };
                self.flow += -d * w;
            }
        }
        self.flow *= self.p().flow_decay;
        self.last_pos = Some(pos);
        self.sent_market = 0.0;
        let value = self.cash + pos * px;
        self.peak_value = self.peak_value.max(value);
        let dd = self.peak_value - value;

        // --- competitor half-spread estimate from the first step's BBO ---
        let mut best_bid = 0.0;
        let mut best_ask = 0.0;
        if let Some(t) = orders.trader() {
            if let Ok(bbo) = t.inner.bbo().await {
                if let (Some(b), Some(a)) = (bbo.best_bid_ticks(), bbo.best_ask_ticks()) {
                    let cfg = t.phoenix.market(&t.env.symbol)?;
                    best_bid = t.phoenix.ticks_to_usd(cfg, b);
                    best_ask = t.phoenix.ticks_to_usd(cfg, a);
                    if m.k == 0 || self.comp_bps == 0.0 {
                        let half = (best_ask - best_bid) / 2.0 / px * 1e4;
                        if half > 1.0 && half < 100.0 {
                            self.comp_bps = half;
                        }
                    }
                }
            }
        }
        let mut jumped = false;
        if let Some(lo) = self.last_oracle {
            if ((px - lo) / lo).abs() > 0.008 {
                jumped = true;
            }
        }
        if self.comp_bps == 0.0 {
            self.comp_bps = 15.0;
        }

        // --- trend detection from oracle moves ---
        if let Some(lo) = self.last_oracle {
            let mv = (px - lo) / self.start_oracle * 1e4;
            if mv.abs() > 1e-6 {
                // ignore jumps (robustness)
                if mv.abs() < 60.0 {
                    self.moves.push_back(mv);
                    if self.moves.len() > 80 {
                        self.moves.pop_front();
                    }
                }
            }
        }
        self.last_oracle = Some(px);
        let p = self.p();
        if self.moves.len() >= p.trend_min_moves {
            let n = self.moves.len() as f64;
            let mean = self.moves.iter().sum::<f64>() / n;
            let var = self.moves.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / (n - 1.0);
            let z = mean / (var.sqrt() / n.sqrt()).max(1e-9);
            let ups = self.moves.iter().filter(|x| **x > 0.0).count() as f64 / n;
            if z > p.trend_z && ups > 0.62 {
                self.trend_dir = 1.0;
            } else if z < -p.trend_z && ups < 0.38 {
                self.trend_dir = -1.0;
            } else if z.abs() < p.trend_z * 0.5 {
                self.trend_dir = 0.0;
            }
        }

        // --- burst detection ---
        if self.flow.abs() > p.flow_trigger && m.k >= self.lean_until {
            self.lean_dir = self.flow.signum();
            self.lean_until = m.k + p.lean_hold;
            info!(step = m.k, flow = self.flow, dir = self.lean_dir, "burst: leaning");
        } else if self.flow.abs() > p.flow_trigger && self.flow.signum() == self.lean_dir {
            // extend the hold while the burst continues
            self.lean_until = m.k + p.lean_hold;
        }
        if jumped && self.lean_dir != 0.0 {
            info!(step = m.k, "jump landed: ending lean");
            self.lean_until = m.k;
            self.flow = 0.0;
        }
        if m.k >= self.lean_until && self.lean_dir != 0.0 {
            self.lean_dir = 0.0;
            self.flatten_pending = true;
        }
        if jumped {
            // after a jump the burst signal is stale
            self.flow = 0.0;
        }

        // --- target position ---
        let mut target = if self.lean_dir != 0.0 {
            let strong = self.flow * self.lean_dir > p.flow_full;
            if strong { self.lean_full = true; }
            self.lean_dir * if self.lean_full { p.lean_size } else { p.lean_size * 0.5 }
        } else {
            self.lean_full = false;
            self.trend_dir * p.trend_size
        };
        // drawdown guard: shrink the allowed position as drawdown grows
        let room = (m.max_drawdown - dd - 30.0).max(0.0);
        let max_pos = (room / (0.045 * px)).min(m.cap);
        target = target.clamp(-max_pos, max_pos);
        let steps_left = m.steps_left as f64;
        if steps_left < 3.0 {
            // nothing to gain in the last steps; avoid new leans
        }

        // --- quoting ---
        let half_bps = (self.comp_bps * p.spread_frac).max(p.min_half_bps);
        let half = px * half_bps / 1e4;
        let min_half = px * p.min_half_bps / 1e4;
        let inv = pos - target;
        let skew = -inv * p.skew_per_unit * px / 1e4;
        let mid = px + skew;
        let mut bid = mid - half;
        let mut ask = mid + half;
        // step one tick inside anyone quoting inside us, down to the minimum half-spread
        if best_bid > 0.0 && best_bid > self.last_bid + 1e-9 && best_bid + 0.01 > bid {
            bid = bid.max((best_bid + 0.01).min(mid - min_half));
        }
        if best_ask > 0.0 && best_ask < self.last_ask - 1e-9 && best_ask - 0.01 < ask {
            ask = ask.min((best_ask - 0.01).max(mid + min_half));
        }
        bid = bid.min(px - min_half);
        ask = ask.max(px + min_half);
        self.last_target = target;
        let bid = (bid * 100.0).floor() / 100.0;
        let ask = (ask * 100.0).ceil() / 100.0;
        self.last_bid = bid;
        self.last_ask = ask;

        orders.cancel_all();
        // move toward the target with market orders when far off
        let gap = target - pos;
        let active = self.lean_dir != 0.0 || self.trend_dir != 0.0 || self.flatten_pending || pos.abs() > max_pos + 0.5;
        if gap.abs() >= 1.0 && active {
            let limit_bps = self.comp_bps + 15.0;
            let size = (gap.abs() * 1000.0).floor() / 1000.0;
            if gap > 0.0 {
                orders.buy(size.min(20.0), px * (1.0 + limit_bps / 1e4));
                self.sent_market = size.min(20.0);
            } else {
                orders.sell(size.min(20.0), px * (1.0 - limit_bps / 1e4));
                self.sent_market = -size.min(20.0);
            }
        }
        if gap.abs() < 1.0 {
            self.flatten_pending = false;
        }
        let size = p.quote_size;
        // when leaning into a burst, don't quote the side the informed are hitting
        let quote_bid = !(self.lean_dir < 0.0) && !(self.flow < -p.flow_pull);
        let quote_ask = !(self.lean_dir > 0.0) && !(self.flow > p.flow_pull);
        let bid_room = (max_pos - pos).max(0.0);
        let ask_room = (max_pos + pos).max(0.0);
        if quote_bid && bid_room > 0.1 {
            orders.bid(bid, size.min(bid_room));
        }
        if quote_ask && ask_room > 0.1 {
            orders.ask(ask, size.min(ask_room));
        }

        if m.k % 10 == 0 || self.lean_dir != 0.0 {
            info!(
                step = m.k,
                px,
                pos,
                target,
                flow = format!("{:.2}", self.flow),
                trend = self.trend_dir,
                value = format!("{:.2}", value),
                dd = format!("{:.1}", dd),
                comp = format!("{:.1}", self.comp_bps),
                half = format!("{:.1}", half_bps),
                qb = format!("{:.1}", (px - bid) / px * 1e4),
                qa = format!("{:.1}", (ask - px) / px * 1e4),
                bb = format!("{:.1}", (px - best_bid) / px * 1e4),
                ba = format!("{:.1}", (best_ask - px) / px * 1e4),
                "step"
            );
        }
        Ok(())
    }

    fn end(&mut self, last: Option<&Market>) {
        if let Some(m) = last {
            info!(value = self.cash + m.position.size * m.oracle, pos = m.position.size, "end");
        }
    }
}

fn main() -> BotResult {
    mtx_kit::run(Bot::default())
}
