//! SPCX market maker: quotes around the oracle, chases the competing maker
//! down to a floor, skews against inventory, and turns one-sided taker flow
//! (informed bursts before jumps) into a directional position.

use std::collections::VecDeque;
use std::env;

use mtx_kit::prelude::*;
use tracing::info;

fn envf(name: &str, default: f64) -> f64 {
    env::var(name).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}

struct P {
    size: f64,        // quote size per side
    min_bp: f64,      // never quote closer than this
    target_frac: f64, // target distance as a fraction of the competitor half-spread
    skew_bp: f64,     // skew per unit of inventory / cap, in bp
    window: usize,    // flow window, steps
    thresh: f64,      // flow threshold, SPCX
    burst_pos: f64,   // position taken on a burst
    hold_steps: u64,  // how long to wait for the jump
    jump_frac: f64,   // move that counts as the jump
    unwind_steps: u64,
}

impl P {
    fn load() -> Self {
        P {
            size: envf("B_SIZE", 2.0),
            min_bp: envf("B_MIN_BP", 1.5),
            target_frac: envf("B_TFRAC", 0.6),
            skew_bp: envf("B_SKEW", 6.0),
            window: envf("B_WIN", 8.0) as usize,
            thresh: envf("B_THRESH", 3.5),
            burst_pos: envf("B_BPOS", 8.0),
            hold_steps: envf("B_HOLD", 22.0) as u64,
            jump_frac: envf("B_JUMP", 0.009),
            unwind_steps: envf("B_UNW", 5.0) as u64,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Mode {
    Make,
    // direction, step it started, oracle then
    Burst(f64, u64, f64),
    Unwind,
}

struct Bot {
    p: P,
    tick: f64,
    half: Option<f64>, // competitor half-spread, fraction
    last_bid: Option<f64>,
    last_ask: Option<f64>,
    bid_d: f64, // current distances, fraction of oracle
    ask_d: f64,
    expected_pos: Option<f64>,
    flow: VecDeque<f64>, // taker net buying per step
    mode: Mode,
    cooldown: u64,
}

impl Bot {
    fn new() -> Self {
        Bot {
            p: P::load(),
            tick: 0.01,
            half: None,
            last_bid: None,
            last_ask: None,
            bid_d: 0.0,
            ask_d: 0.0,
            expected_pos: None,
            flow: VecDeque::new(),
            mode: Mode::Make,
            cooldown: 0,
        }
    }
}

impl Strategy for Bot {
    async fn step(&mut self, m: &Market, orders: &mut Orders<'_>) -> BotResult {
        let o = m.oracle;
        let pos = m.position.size;
        let cap = m.cap;
        let p = &self.p;

        // Top of book (includes our own quotes).
        let (mut bb, mut ba) = (None, None);
        if let Some(t) = orders.trader() {
            if let (Ok(bbo), Ok(cfg)) = (t.inner.bbo().await, t.phoenix.market(&t.env.symbol)) {
                bb = bbo.best_bid_ticks().map(|x| t.phoenix.ticks_to_usd(cfg, x));
                ba = bbo.best_ask_ticks().map(|x| t.phoenix.ticks_to_usd(cfg, x));
                if self.half.is_none() {
                    self.tick = t.phoenix.ticks_to_usd(cfg, 1).max(1e-6);
                }
            }
        }
        if self.half.is_none() {
            let h = match (bb, ba) {
                (Some(b), Some(a)) if a > b => ((a - b) / 2.0 / o).clamp(0.0006, 0.0035),
                _ => 0.0015,
            };
            self.half = Some(h);
            self.bid_d = h * p.target_frac;
            self.ask_d = h * p.target_frac;
            info!(half_bp = h * 1e4, tick = self.tick, oracle = o, "competitor");
        }
        let h = self.half.unwrap();

        // Taker flow: position change not explained by our own market orders.
        let passive = self.expected_pos.map(|e| pos - e).unwrap_or(0.0);
        self.flow.push_back(-passive);
        while self.flow.len() > p.window {
            self.flow.pop_front();
        }
        let flow: f64 = self.flow.iter().sum();

        let k = m.k;
        let mut sent = 0.0;
        let slip = o * (h + 0.0008);

        // Mode transitions.
        match self.mode {
            Mode::Make => {
                if self.cooldown == 0 && flow.abs() >= p.thresh && m.slots_left > 12 {
                    let dir = flow.signum();
                    self.mode = Mode::Burst(dir, k, o);
                    info!(k, flow, dir, "burst");
                }
            }
            Mode::Burst(dir, start, o0) => {
                let moved = (o - o0) / o0 * dir;
                if moved > p.jump_frac {
                    info!(k, moved, "jump caught");
                    self.mode = Mode::Unwind;
                } else if k - start > p.hold_steps || moved < -p.jump_frac {
                    info!(k, moved, "burst timeout");
                    self.mode = Mode::Unwind;
                }
            }
            Mode::Unwind => {}
        }
        if self.cooldown > 0 {
            self.cooldown -= 1;
        }

        match self.mode {
            Mode::Burst(dir, _, _) => {
                let target = (dir * p.burst_pos).clamp(-cap, cap);
                orders.cancel_all();
                let diff = target - pos;
                if diff.abs() >= 0.5 {
                    let sz = diff.abs().min(10.0);
                    if diff > 0.0 {
                        orders.buy(sz, o + slip);
                        sent = sz;
                    } else {
                        orders.sell(sz, o - slip);
                        sent = -sz;
                    }
                }
                self.last_bid = None;
                self.last_ask = None;
            }
            Mode::Unwind => {
                orders.cancel_all();
                if pos.abs() < 0.05 {
                    self.mode = Mode::Make;
                    self.flow.clear();
                    self.cooldown = 4;
                } else {
                    let sz = (cap / p.unwind_steps as f64).max(1.0).min(pos.abs()).min(10.0);
                    if pos > 0.0 {
                        orders.sell(sz, o - slip);
                        sent = -sz;
                    } else {
                        orders.buy(sz, o + slip);
                        sent = sz;
                    }
                }
                self.last_bid = None;
                self.last_ask = None;
            }
            Mode::Make => {
                let target_d = h * p.target_frac;
                let min_d = (p.min_bp / 1e4).max(self.tick / o);
                // Chase: if someone is strictly inside our last quote, step a tick inside them.
                match (self.last_bid, bb) {
                    (Some(lb), Some(b)) if b > lb + self.tick * 0.5 => {
                        self.bid_d = ((o - b - self.tick) / o).max(min_d);
                    }
                    _ => self.bid_d = (self.bid_d + self.tick / o).min(target_d),
                }
                match (self.last_ask, ba) {
                    (Some(la), Some(a)) if a < la - self.tick * 0.5 => {
                        self.ask_d = ((a - self.tick - o) / o).max(min_d);
                    }
                    _ => self.ask_d = (self.ask_d + self.tick / o).min(target_d),
                }
                // Skew against inventory.
                let inv = pos / cap;
                let skew = inv * p.skew_bp / 1e4;
                let bid_d = (self.bid_d + skew.max(0.0) * 1.0 + skew).max(min_d);
                let ask_d = (self.ask_d - skew.min(0.0) * 1.0 - skew).max(min_d);
                let bid = ((o * (1.0 - bid_d)) / self.tick).floor() * self.tick;
                let ask = ((o * (1.0 + ask_d)) / self.tick).ceil() * self.tick;
                let mut bsz = p.size;
                let mut asz = p.size;
                if pos > cap * 0.5 {
                    bsz *= 0.5;
                }
                if pos < -cap * 0.5 {
                    asz *= 0.5;
                }
                orders.cancel_all();
                orders.bid(bid, bsz);
                orders.ask(ask, asz);
                self.last_bid = Some(bid);
                self.last_ask = Some(ask);
            }
        }
        let sent = sent.clamp(-cap - pos, cap - pos);
        self.expected_pos = Some(pos + sent);
        if k % 20 == 0 {
            info!(k, o, pos, flow, pnl = m.pnl, mode = ?self.mode, bid_bp = self.bid_d * 1e4, ask_bp = self.ask_d * 1e4, "state");
        }
        Ok(())
    }
}

fn main() -> BotResult {
    mtx_kit::run(Bot::new())
}
