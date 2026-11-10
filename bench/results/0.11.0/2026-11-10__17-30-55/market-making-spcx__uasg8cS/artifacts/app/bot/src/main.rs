//! SPCX market maker: tight two-sided quotes inside the competitor, inventory
//! skew, and a burst detector that leans into informed flow ahead of jumps.

use std::collections::VecDeque;

use mtx_kit::prelude::*;
use tracing::{info, warn};

const TICK: f64 = 0.01;

#[derive(Debug, Clone, Copy, PartialEq)]
enum Mode {
    Make,
    /// Leaning with detected informed flow: +1 long, -1 short, since step `since`.
    Lean { dir: f64, since: u64 },
    /// Flattening after a jump or fake-out.
    Flatten,
}

struct Fill {
    step: u64,
    /// Taker direction: +1 takers bought (hit my ask), -1 takers sold.
    taker_dir: f64,
    size: f64,
}

struct Mm {
    /// Current quote distance from oracle, in fraction of price, per side.
    d_bid: f64,
    d_ask: f64,
    d_min: f64,
    /// Where my resting quotes are (price), if any.
    my_bid: Option<f64>,
    my_ask: Option<f64>,
    last_pos: Option<f64>,
    /// Position change I expect from my own market orders sent last step.
    expected_take: f64,
    fills: VecDeque<Fill>,
    mode: Mode,
    last_oracle: Option<f64>,
    oracle_hist: VecDeque<f64>,
    flow_hist: VecDeque<f64>,
    quote_size: f64,
    comp_half: f64,
    start_oracle: f64,
}

impl Default for Mm {
    fn default() -> Self {
        Mm {
            d_bid: 0.0006,
            d_ask: 0.0006,
            d_min: 0.00015,
            my_bid: None,
            my_ask: None,
            last_pos: None,
            expected_take: 0.0,
            fills: VecDeque::new(),
            mode: Mode::Make,
            last_oracle: None,
            oracle_hist: VecDeque::new(),
            flow_hist: VecDeque::new(),
            quote_size: 2.5,
            comp_half: 0.0015,
            start_oracle: 0.0,
        }
    }
}

fn round_to_tick(p: f64) -> f64 {
    (p / TICK).round() * TICK
}

impl Strategy for Mm {
    async fn start(&mut self, session: &Session) -> BotResult {
        info!(steps = session.hello.steps, cap = session.env.max_position, "started");
        Ok(())
    }

    async fn step(&mut self, m: &Market, orders: &mut Orders<'_>) -> BotResult {
        let px = m.oracle;
        if !(px > 0.0) {
            return Ok(());
        }
        if self.start_oracle == 0.0 {
            self.start_oracle = px;
        }
        let pos = m.position.size;
        let tick_frac = TICK / px;
        self.d_min = self.d_min.max(2.0 * tick_frac);

        // --- fills since last step ---
        if let Some(lp) = self.last_pos {
            let passive = pos - lp - self.expected_take;
            if passive.abs() >= 0.05 {
                // takers bought from me if my position fell
                let taker_dir = if passive < 0.0 { 1.0 } else { -1.0 };
                self.fills.push_back(Fill { step: m.k, taker_dir, size: passive.abs() });
                info!(step = m.k, passive, pos, "fill");
            }
        }
        self.expected_take = 0.0;
        self.last_pos = Some(pos);
        while let Some(f) = self.fills.front() {
            if m.k - f.step > 12 { self.fills.pop_front(); } else { break; }
        }

        // --- oracle move / jump detection ---
        let jump = match self.last_oracle {
            Some(lo) => (px - lo) / lo,
            None => 0.0,
        };
        self.last_oracle = Some(px);
        self.oracle_hist.push_back(px);
        if self.oracle_hist.len() > 80 { self.oracle_hist.pop_front(); }

        // --- read the book ---
        let trader = orders.trader();
        let mut best_bid = None;
        let mut best_ask = None;
        if let Some(t) = trader {
            match t.inner.bbo().await {
                Ok(bbo) => {
                    let mk = t.phoenix.market(&t.env.symbol)?;
                    best_bid = bbo.best_bid_ticks().map(|x| t.phoenix.ticks_to_usd(mk, x));
                    best_ask = bbo.best_ask_ticks().map(|x| t.phoenix.ticks_to_usd(mk, x));
                }
                Err(e) => warn!(step = m.k, %e, "bbo failed"),
            }
        }
        // competitor half-spread estimate: from the first step's book (no quotes of mine yet)
        if m.k == 0 {
            if let (Some(b), Some(a)) = (best_bid, best_ask) {
                let h = ((a - b) / 2.0) / px;
                if h > 0.0003 && h < 0.006 { self.comp_half = h; }
            }
        }
        // undercut game: if someone is inside my quote, step one tick inside them
        // (only when my quotes are known to still be where I left them)
        let filled_now = self.fills.back().map(|f| f.step == m.k).unwrap_or(false);
        let stable = false && jump.abs() < 1e-9 && !filled_now;
        let base_d = (self.comp_half * 0.6).max(0.0003).max(3.0 * tick_frac);
        self.d_bid = base_d;
        self.d_ask = base_d;
        if let (Some(b), true) = (best_bid, stable) {
            let inside = match self.my_bid { Some(mb) => b > mb + TICK * 0.5, None => true };
            if inside {
                let x = (px - b) / px;
                if x >= self.d_min - 0.5 * tick_frac && x < self.d_bid {
                    self.d_bid = (x - tick_frac).max(self.d_min);
                }
            }
        }
        if let (Some(a), true) = (best_ask, stable) {
            let inside = match self.my_ask { Some(ma) => a < ma - TICK * 0.5, None => true };
            if inside {
                let x = (a - px) / px;
                if x >= self.d_min - 0.5 * tick_frac && x < self.d_ask {
                    self.d_ask = (x - tick_frac).max(self.d_min);
                }
            }
        }

        // --- burst detection ---
        let (mut buys, mut sells) = (0i32, 0i32);
        let mut net_size = 0.0;
        for f in &self.fills {
            if f.taker_dir > 0.0 { buys += 1 } else { sells += 1 }
            net_size += f.taker_dir * f.size;
        }
        let total = (buys + sells) as f64;
        let z = if total > 0.0 { (buys - sells) as f64 / total.sqrt() } else { 0.0 };
        let burst_dir = if buys >= 7 && buys - sells >= 6 && z >= 2.2 && net_size > 4.0 {
            1.0
        } else if sells >= 7 && sells - buys >= 6 && z <= -2.2 && net_size < -4.0 {
            -1.0
        } else {
            0.0
        };
        // back-off: one-sided flow -> pull the side being hit
        let hit_dir = if buys >= 4 && buys - sells >= 3 && net_size > 1.0 { 1.0 } else if sells >= 4 && sells - buys >= 3 && net_size < -1.0 { -1.0 } else { 0.0 };
        // trend lean: oracle drift over the last 60 steps plus long-window flow imbalance
        self.flow_hist.push_back(self.fills.back().filter(|f| f.step == m.k).map(|f| f.taker_dir * f.size).unwrap_or(0.0));
        if self.flow_hist.len() > 60 { self.flow_hist.pop_front(); }
        let ret60 = if self.oracle_hist.len() >= 60 { (px - self.oracle_hist[self.oracle_hist.len() - 60]) / px } else { 0.0 };
        let flow60: f64 = self.flow_hist.iter().sum();
        let trend = (ret60 / 0.0025 + flow60 / 10.0).clamp(-1.0, 1.0);
        let bias = if self.flow_hist.len() >= 60 { trend * 0.0 } else { 0.0 };

        // --- mode transitions ---
        let dd_room = m.drawdown_room();
        let lean_cap = ((dd_room - 60.0) / (px * 0.045)).clamp(0.0, m.cap).floor();
        match self.mode {
            Mode::Make => {
                if burst_dir != 0.0 && lean_cap >= 5.0 && m.steps_left > 3 {
                    info!(step = m.k, burst_dir, buys, sells, net_size, "lean start");
                    self.mode = Mode::Lean { dir: burst_dir, since: m.k };
                }
            }
            Mode::Lean { dir, since } => {
                if jump * dir > 0.008 {
                    info!(step = m.k, jump, "jump landed: flatten");
                    self.mode = Mode::Flatten;
                } else if jump * dir < -0.008 {
                    warn!(step = m.k, jump, "jump against lean: flatten");
                    self.mode = Mode::Flatten;
                } else if m.k - since > 17 {
                    info!(step = m.k, "fake-out timeout: flatten");
                    self.mode = Mode::Flatten;
                } else if burst_dir == -dir {
                    info!(step = m.k, "flow reversed: flatten");
                    self.mode = Mode::Flatten;
                }
            }
            Mode::Flatten => {
                if pos.abs() < 1.0 {
                    self.mode = Mode::Make;
                    self.fills.clear();
                }
            }
        }

        // --- end of session: go flat-ish, no new risk ---
        let ending = m.steps_left <= 2;

        let sz = self.quote_size;
        match self.mode {
            Mode::Lean { dir, since } => {
                let target = dir * lean_cap;
                let missing = target - pos;
                self.my_bid = None;
                self.my_ask = None;
                orders.cancel_all();
                if missing.abs() >= 0.5 && m.k - since < 8 {
                    let lim = if dir > 0.0 { px * 1.004 } else { px * 0.996 };
                    if dir > 0.0 { orders.buy(missing, round_to_tick(lim)); } else { orders.sell(-missing, round_to_tick(lim)); }
                    self.expected_take = missing;
                }
                // keep an adding-side quote to collect flow at the tight distance
                if dir > 0.0 && pos < target - 0.1 {
                    let b = round_to_tick(px * (1.0 - self.d_bid));
                    orders.bid(b, (target - pos).min(sz));
                    self.my_bid = Some(b);
                } else if dir < 0.0 && pos > target + 0.1 {
                    let a = round_to_tick(px * (1.0 + self.d_ask));
                    orders.ask(a, (pos - target).min(sz));
                    self.my_ask = Some(a);
                }
            }
            Mode::Flatten => {
                self.my_bid = None;
                self.my_ask = None;
                orders.cancel_all();
                if pos.abs() >= 0.1 {
                    let lim = if pos > 0.0 { px * 0.996 } else { px * 1.004 };
                    if pos > 0.0 { orders.sell(pos, round_to_tick(lim)); } else { orders.buy(-pos, round_to_tick(lim)); }
                    self.expected_take = -pos;
                }
            }
            Mode::Make => {
                if ending {
                    // last steps: flatten passively-ish: cross if sizeable
                    orders.cancel_all();
                    self.my_bid = None;
                    self.my_ask = None;
                    return Ok(());
                }
                // inventory skew: widen the side that adds, keep the reducing side tight
                let q = pos - bias;
                let skew = (q.abs() / m.cap).min(1.0); // 0..1
                let widen = skew * 0.0015 + skew * skew * 0.003;
                let tighten = 1.0 - 0.5 * skew;
                let (mut db, mut da) = if q > 0.0 {
                    (self.d_bid + widen, (self.d_ask * tighten).max(2.0 * tick_frac))
                } else {
                    ((self.d_bid * tighten).max(2.0 * tick_frac), self.d_ask + widen)
                };
                if hit_dir > 0.0 { da = (self.comp_half * 4.2).max(da); } else if hit_dir < 0.0 { db = (self.comp_half * 4.2).max(db); }
                let size_add = (sz * (1.0 - 0.8 * skew)).max(0.3);
                let (sb, sa) = if q > 0.0 { (size_add, sz) } else { (sz, size_add) };
                let b = round_to_tick(px * (1.0 - db));
                let a = round_to_tick(px * (1.0 + da));
                let same = matches!((self.my_bid, self.my_ask), (Some(mb), Some(ma)) if (mb - b).abs() < TICK * 0.5 && (ma - a).abs() < TICK * 0.5);
                if !same || self.fills.back().map(|f| f.step == m.k).unwrap_or(false) {
                    orders.cancel_all();
                    orders.bid(b, sb);
                    orders.ask(a, sa);
                    self.my_bid = Some(b);
                    self.my_ask = Some(a);
                }
            }
        }
        if m.k % 20 == 0 {
            info!(step = m.k, px, pos, bias, pnl = m.pnl, dd = m.drawdown, d_bid = self.d_bid * 1e4, d_ask = self.d_ask * 1e4, ?best_bid, ?best_ask, hit_dir, mode = ?self.mode, budget_ms = m.budget_left.as_millis() as u64, "status");
        }
        Ok(())
    }

    fn end(&mut self, last: Option<&Market>) {
        if let Some(m) = last {
            info!(pnl = m.pnl, pos = m.position.size, "end");
        }
    }
}

fn main() -> BotResult {
    mtx_kit::run(Mm::default())
}
