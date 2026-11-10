//! SPCX market maker: quote one tick inside the visible competitor around the
//! oracle, skew against inventory, detect informed one-sided bursts from our
//! own fills and position ahead of the jump, lean with a detected trend.

use mtx_kit::prelude::*;
use tracing::{info, warn};

const QUOTE_SIZE: f64 = 3.0;
const MIN_D_BPS: f64 = 1.0; // closest we quote to the oracle
const SKEW_BPS_PER_UNIT: f64 = 0.6;
const MAX_INV_NO_LEAN: f64 = 8.0;

// Burst detection
const BURST_WINDOW_STEPS: usize = 10;
const BURST_IMBALANCE: i32 = 3;
const BURST_NET_SIZE: f64 = 3.5;
const BURST_HOLD_SLOTS: u64 = 60;
const BURST_COOLDOWN_SLOTS: u64 = 30;
const JUMP_BPS: f64 = 90.0;

// Trend lean
const TREND_MIN_STEPS: usize = 60;
const TREND_Z: f64 = 3.0;
const LEAN_BASE: f64 = 8.0;
const LEAN_MAX: f64 = 14.0;

const DD_GUARD_USD: f64 = 140.0;

#[derive(Debug, Clone)]
struct Burst {
    dir: f64,
    start_slot: u64,
    last_fill_slot: u64,
    entry_oracle: f64,
}

struct Bot {
    symbol: String,
    tick: f64,
    c_bps: f64,
    bid_d: f64,
    ask_d: f64,
    my_bid: Option<f64>,
    my_ask: Option<f64>,
    last_pos: Option<f64>,
    expected_delta: f64,
    // signed fill per step (positive: takers sold to us), most recent last
    fills: Vec<f64>,
    // oracle path
    last_oracle: Option<f64>,
    diffs_bps: Vec<f64>,
    cum_bps: f64,
    burst: Option<Burst>,
    cooldown_until: u64,
    lean: f64,
    guard: bool,
    tot_buys: i32,
    tot_sells: i32,
}

impl Default for Bot {
    fn default() -> Self {
        Bot {
            symbol: "SPCX".into(),
            tick: 0.01,
            c_bps: 15.0,
            bid_d: 15.0,
            ask_d: 15.0,
            my_bid: None,
            my_ask: None,
            last_pos: None,
            expected_delta: 0.0,
            fills: Vec::new(),
            last_oracle: None,
            diffs_bps: Vec::new(),
            cum_bps: 0.0,
            burst: None,
            cooldown_until: 0,
            lean: 0.0,
            guard: false,
            tot_buys: 0,
            tot_sells: 0,
        }
    }
}

fn median(v: &mut Vec<f64>) -> f64 {
    if v.is_empty() {
        return 0.0;
    }
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    v[v.len() / 2]
}

impl Bot {
    fn tick_bps(&self, oracle: f64) -> f64 {
        self.tick / oracle * 1e4
    }

    fn sigma_bps(&self) -> f64 {
        let mut v: Vec<f64> = self.diffs_bps.iter().map(|d| d.abs()).collect();
        (median(&mut v) * 1.4826).max(0.3)
    }
}

impl Strategy for Bot {
    async fn start(&mut self, session: &Session) -> BotResult {
        self.symbol = session.env.symbol.clone();
        let m = session.trader.phoenix.market(&self.symbol)?;
        self.tick = m.tick_size as f64 * 10f64.powi(m.base_lots_decimals as i32 - 6);
        info!(tick = self.tick, steps = session.hello.steps, "started");
        Ok(())
    }

    async fn step(&mut self, market: &Market, orders: &mut Orders<'_>) -> BotResult {
        let oracle = market.oracle;
        if !(oracle > 0.0) {
            return Ok(());
        }
        let pos = market.position.size;
        let slot = market.slot;
        let tick_bps = self.tick_bps(oracle);

        // ---- oracle path statistics
        let mut jumped = 0.0;
        if let Some(last) = self.last_oracle {
            let d = (oracle - last) / last * 1e4;
            if d.abs() > JUMP_BPS {
                jumped = d;
            } else {
                self.diffs_bps.push(d);
                self.cum_bps += d;
            }
        }
        self.last_oracle = Some(oracle);

        // ---- fills since last step (position change not from our own market orders)
        let fill = match self.last_pos {
            Some(lp) => pos - lp - self.expected_delta,
            None => 0.0,
        };
        self.last_pos = Some(pos);
        self.expected_delta = 0.0;
        let fill = if fill.abs() < 0.05 { 0.0 } else { fill };
        self.fills.push(fill);
        if fill < 0.0 { self.tot_buys += 1; } else if fill > 0.0 { self.tot_sells += 1; }
        if self.fills.len() > BURST_WINDOW_STEPS {
            let drop = self.fills.len() - BURST_WINDOW_STEPS;
            self.fills.drain(0..drop);
        }

        // ---- read the book
        let trader = orders.trader();
        let mut bbo_bid = None;
        let mut bbo_ask = None;
        if let Some(t) = trader {
            match t.inner.bbo().await {
                Ok(b) => {
                    bbo_bid = b.best_bid_ticks().map(|x| x as f64 * self.tick);
                    bbo_ask = b.best_ask_ticks().map(|x| x as f64 * self.tick);
                }
                Err(e) => warn!(%e, "bbo read failed"),
            }
        }
        // competitor distances, excluding our own quotes
        let mine = |p: Option<f64>, b: f64| p.map(|m| (m - b).abs() < self.tick * 0.5).unwrap_or(false);
        let comp_bid_d = bbo_bid.and_then(|b| if mine(self.my_bid, b) { None } else { Some(((oracle - b) / oracle * 1e4).max(0.0)) });
        let comp_ask_d = bbo_ask.and_then(|a| if mine(self.my_ask, a) { None } else { Some(((a - oracle) / oracle * 1e4).max(0.0)) });
        if market.k == 0 {
            if let (Some(b), Some(a)) = (comp_bid_d, comp_ask_d) {
                let c = b.max(a);
                if c > 2.0 && c < 60.0 {
                    self.c_bps = c;
                }
            }
        }
        // one tick inside the competitor on each side, never closer than MIN_D
        let lo = MIN_D_BPS.max(tick_bps);
        if let Some(d) = comp_bid_d {
            self.bid_d = (d - tick_bps).clamp(lo, self.c_bps);
        }
        if let Some(d) = comp_ask_d {
            self.ask_d = (d - tick_bps).clamp(lo, self.c_bps);
        }
        if market.k == 0 {
            self.bid_d = (self.c_bps - tick_bps).max(lo);
            self.ask_d = (self.c_bps - tick_bps).max(lo);
        }

        // ---- burst detection
        let buys: i32 = self.fills.iter().filter(|f| **f < 0.0).count() as i32; // takers bought from us
        let sells: i32 = self.fills.iter().filter(|f| **f > 0.0).count() as i32;
        let imb = buys - sells; // positive: takers buying -> price likely up
        let net_size: f64 = -self.fills.iter().sum::<f64>();
        if let Some(b) = self.burst.clone() {
            if fill != 0.0 && (-fill).signum() == b.dir {
                self.burst.as_mut().unwrap().last_fill_slot = slot;
            }
            let moved = (oracle - b.entry_oracle) / b.entry_oracle * 1e4;
            let done_jump = jumped != 0.0 || moved.abs() > JUMP_BPS;
            let timeout = slot > b.start_slot + BURST_HOLD_SLOTS
                || (slot > b.start_slot + 36 && slot > b.last_fill_slot + 16);
            if done_jump || timeout {
                info!(k = market.k, dir = b.dir, moved, done_jump, timeout, "burst exit");
                self.burst = None;
                self.cooldown_until = slot + BURST_COOLDOWN_SLOTS;
                self.fills.clear();
            }
        } else if slot >= self.cooldown_until
            && !self.guard
            && imb.abs() >= BURST_IMBALANCE
            && net_size.abs() >= BURST_NET_SIZE
            && (imb as f64).signum() == net_size.signum()
        {
            let dir = net_size.signum();
            info!(k = market.k, dir, buys, sells, net_size, pos, "burst enter");
            self.burst = Some(Burst {
                dir,
                start_slot: slot,
                last_fill_slot: slot,
                entry_oracle: oracle,
            });
        }

        // ---- trend lean
        let n = self.diffs_bps.len();
        if n >= TREND_MIN_STEPS {
            let z = self.cum_bps / (self.sigma_bps() * (n as f64).sqrt());
            let nf = (self.tot_buys + self.tot_sells).max(1) as f64;
            let zf = (self.tot_buys - self.tot_sells) as f64 / nf.sqrt();
            let strong = z.abs() >= TREND_Z;
            let confirmed = z.abs() >= TREND_Z - 1.0 && zf.signum() == z.signum() && zf.abs() >= 1.5;
            if strong || confirmed {
                self.lean = z.signum() * (LEAN_BASE + 3.0 * (z.abs() - TREND_Z).max(0.0)).min(LEAN_MAX);
            } else if z.abs() < TREND_Z - 1.5 || zf.signum() != z.signum() && z.abs() < TREND_Z {
                self.lean = 0.0;
            }
        }

        // ---- drawdown guard
        if market.drawdown > DD_GUARD_USD {
            if !self.guard {
                warn!(dd = market.drawdown, "drawdown guard on");
            }
            self.guard = true;
        }

        // ---- target position
        let target = if self.guard {
            0.0
        } else if self.burst.is_some() {
            0.0
        } else {
            self.lean
        };

        orders.cancel_all();
        self.my_bid = None;
        self.my_ask = None;

        // take toward the target when far from it (burst or guard); leans are reached passively
        let take = self.burst.is_some() || self.guard;
        if take && (target - pos).abs() >= 1.5 {
            let want = target.clamp(-market.cap, market.cap);
            self.expected_delta = want - pos;
            orders.target_position_with(
                want,
                Execution {
                    max_per_step: 20.0,
                    slice: 10.0,
                    slippage_bps: 40.0,
                    min_trade: 0.5,
                },
            );
        }
        let pos_after = pos + self.expected_delta;

        // ---- quotes, skewed against inventory relative to the target
        let dev = pos_after - target;
        let skew = dev * SKEW_BPS_PER_UNIT;
        let bid_d = (self.bid_d + skew).max(lo);
        let ask_d = (self.ask_d - skew).max(lo);
        let bid = oracle * (1.0 - bid_d / 1e4);
        let ask = oracle * (1.0 + ask_d / 1e4);
        let mut bid_size = QUOTE_SIZE;
        let mut ask_size = QUOTE_SIZE;
        if dev > MAX_INV_NO_LEAN {
            bid_size = 0.0;
        }
        if dev < -MAX_INV_NO_LEAN {
            ask_size = 0.0;
        }
        if let Some(b) = &self.burst {
            if b.dir > 0.0 {
                ask_size = 0.0;
            } else {
                bid_size = 0.0;
            }
        }
        if self.guard {
            bid_size = if pos_after < -0.5 { QUOTE_SIZE.min(-pos_after) } else { 0.0 };
            ask_size = if pos_after > 0.5 { QUOTE_SIZE.min(pos_after) } else { 0.0 };
        }
        if market.last {
            bid_size = 0.0;
            ask_size = 0.0;
        }
        if bid_size > 0.0 {
            orders.bid(bid, bid_size);
            self.my_bid = Some((bid / self.tick).floor() * self.tick);
        }
        if ask_size > 0.0 {
            orders.ask(ask, ask_size);
            self.my_ask = Some((ask / self.tick).ceil() * self.tick);
        }
        info!(
            k = market.k,
            oracle,
            pos,
            fill,
            imb,
            c = self.c_bps,
            bd = format!("{:.1}", bid_d),
            ad = format!("{:.1}", ask_d),
            cb = ?comp_bid_d.map(|d| (d * 10.0).round() / 10.0),
            ca = ?comp_ask_d.map(|d| (d * 10.0).round() / 10.0),
            target,
            lean = self.lean,
            pnl = format!("{:.2}", market.pnl),
            dd = format!("{:.1}", market.drawdown),
            budget_ms = market.budget_left.as_millis() as u64,
            "step"
        );
        Ok(())
    }
}

fn main() -> BotResult {
    mtx_kit::run(Bot::default())
}
