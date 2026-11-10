//! SPCX market maker: quotes at the front of the book, reads its own fills
//! for one-sided (informed) bursts, and leans with them into the jump.

use mtx_kit::prelude::*;
use mtx_kit::rust_decimal::prelude::ToPrimitive;
use std::collections::VecDeque;
use tracing::info;

// Tunables.
const QUOTE_SIZE: f64 = 3.0;
const MIN_HALF_BPS: f64 = 2.0; // never quote closer than this to the center
const SKEW_BPS_AT_CAP: f64 = 25.0; // center shift at full inventory
const BURST_WINDOW: u64 = 16; // slots
const BURST_NET: f64 = 5.5; // net taker flow (SPCX) to call a burst
const BURST_COUNT: usize = 4; // same-side fills in the window
const LEAN_FRAC: f64 = 1.0; // of cap
const LEAN_TIMEOUT: u64 = 35; // slots after the last burst fill without a jump
const JUMP_FRAC: f64 = 0.008;
const UNWIND_HARD: u64 = 80; // slots after which a remaining unwind is crossed

#[derive(PartialEq, Clone, Copy, Debug)]
enum Mode {
    Make,
    Lean { dir: f64, last_burst: u64 },
    Unwind { since: u64 },
}

struct Bot {
    tick: f64,
    comp_h: Option<f64>,
    my_bid: Option<f64>,
    my_ask: Option<f64>,
    expected_pos: Option<f64>,
    fills: VecDeque<(u64, f64)>, // (slot, signed taker flow: + = takers bought)
    mode: Mode,
    last_oracle: Option<f64>,
    start_oracle: Option<f64>,
}

impl Default for Bot {
    fn default() -> Self {
        Bot {
            tick: 0.01,
            comp_h: None,
            my_bid: None,
            my_ask: None,
            expected_pos: None,
            fills: VecDeque::new(),
            mode: Mode::Make,
            last_oracle: None,
            start_oracle: None,
        }
    }
}

fn floor_tick(p: f64, t: f64) -> f64 {
    ((p / t) + 1e-9).floor() * t
}
fn ceil_tick(p: f64, t: f64) -> f64 {
    ((p / t) - 1e-9).ceil() * t
}

impl Strategy for Bot {
    async fn start(&mut self, session: &Session) -> BotResult {
        let m = session.trader.inner.market();
        self.tick = m.ticks_to_price(1).to_f64().unwrap_or(0.01);
        info!(tick = self.tick, "started");
        Ok(())
    }

    async fn step(&mut self, market: &Market, orders: &mut Orders<'_>) -> BotResult {
        let oracle = market.oracle;
        let pos = market.position.size;
        let slot = market.slot;
        let t = self.tick;
        let cap = market.cap;
        if self.start_oracle.is_none() {
            self.start_oracle = Some(oracle);
        }

        // Book top.
        let trader = orders.trader();
        let (mut bb, mut ba) = (None, None);
        if let Some(tr) = trader {
            if let Ok(bbo) = tr.inner.bbo().await {
                let m = tr.inner.market();
                bb = bbo.best_bid_ticks().map(|x| m.ticks_to_price(x).to_f64().unwrap_or(0.0));
                ba = bbo.best_ask_ticks().map(|x| m.ticks_to_price(x).to_f64().unwrap_or(0.0));
            }
        }
        if self.comp_h.is_none() {
            if let (Some(b), Some(a)) = (bb, ba) {
                self.comp_h = Some(((a - b) / 2.0 / oracle).clamp(0.0006, 0.004));
            }
        }
        let comp_h = self.comp_h.unwrap_or(0.0015);

        // Fills since last step (all position change not from our market orders).
        if let Some(exp) = self.expected_pos {
            let d = pos - exp;
            if d.abs() > 1e-6 && self.mode == Mode::Make {
                self.fills.push_back((slot, -d));
                info!(slot, flow = -d, pos, "fill");
            }
        }
        while let Some(&(s, _)) = self.fills.front() {
            if s + BURST_WINDOW < slot {
                self.fills.pop_front();
            } else {
                break;
            }
        }
        let net: f64 = self.fills.iter().map(|f| f.1).sum();
        let nbuy = self.fills.iter().filter(|f| f.1 > 0.0).count();
        let nsell = self.fills.iter().filter(|f| f.1 < 0.0).count();

        // Jump detection.
        let jumped = self
            .last_oracle
            .map(|l| ((oracle / l) - 1.0).abs() > JUMP_FRAC)
            .unwrap_or(false);
        self.last_oracle = Some(oracle);

        let burst_dir = if net >= BURST_NET && nbuy >= BURST_COUNT && nbuy > 2 * nsell {
            1.0
        } else if net <= -BURST_NET && nsell >= BURST_COUNT && nsell > 2 * nbuy {
            -1.0
        } else {
            0.0
        };

        // Mode transitions.
        self.mode = match self.mode {
            Mode::Make if burst_dir != 0.0 && market.slots_left > 20 => {
                info!(slot, dir = burst_dir, net, "burst: leaning");
                self.fills.clear();
                Mode::Lean { dir: burst_dir, last_burst: slot }
            }
            Mode::Lean { dir, last_burst } => {
                if jumped {
                    info!(slot, "jump: unwinding");
                    self.fills.clear();
                    Mode::Unwind { since: slot }
                } else if burst_dir == dir {
                    Mode::Lean { dir, last_burst: slot }
                } else if burst_dir == -dir {
                    Mode::Lean { dir: -dir, last_burst: slot }
                } else if slot > last_burst + LEAN_TIMEOUT {
                    info!(slot, "no jump: unwinding");
                    Mode::Unwind { since: slot }
                } else {
                    Mode::Lean { dir, last_burst }
                }
            }
            Mode::Unwind { .. } if pos.abs() < 0.5 => {
                self.fills.clear();
                Mode::Make
            }
            m => m,
        };
        if market.drawdown > 170.0 && pos.abs() > 0.5 {
            self.mode = Mode::Unwind { since: 0 };
        }

        let mut expected = pos;
        match self.mode {
            Mode::Lean { dir, .. } => {
                let target = dir * cap * LEAN_FRAC * 0.98;
                if (target - pos).abs() >= 0.1 {
                    orders.cancel_all();
                    self.my_bid = None;
                    self.my_ask = None;
                    let ex = Execution {
                        max_per_step: f64::INFINITY,
                        slice: 10.0,
                        slippage_bps: (comp_h * 1e4 * 3.0).max(40.0),
                        min_trade: 0.1,
                    };
                    orders.target_position_with(target, ex);
                    expected = target;
                }
                self.expected_pos = Some(expected);
                return Ok(());
            }
            Mode::Unwind { since } if slot > since + UNWIND_HARD || market.slots_left < 30 => {
                if pos.abs() >= 0.1 {
                    orders.cancel_all();
                    self.my_bid = None;
                    self.my_ask = None;
                    let ex = Execution {
                        max_per_step: f64::INFINITY,
                        slice: 10.0,
                        slippage_bps: (comp_h * 1e4 * 3.0).max(40.0),
                        min_trade: 0.1,
                    };
                    orders.target_position_with(0.0, ex);
                    expected = 0.0;
                }
                self.expected_pos = Some(expected);
                return Ok(());
            }
            _ => {}
        }

        // Making (or passive unwind).
        let unwinding = matches!(self.mode, Mode::Unwind { .. });
        let skew = if unwinding { 2.0 } else { 1.0 };
        let center = oracle * (1.0 - skew * SKEW_BPS_AT_CAP * 1e-4 * pos / cap);
        let min_h = oracle * MIN_HALF_BPS * 1e-4;
        let wide = oracle * comp_h;

        // Others' best: if the top equals our own resting price, we can't see behind it.
        let other_bid = match (bb, self.my_bid) {
            (Some(b), Some(m)) if (b - m).abs() < t * 0.5 => None,
            (b, _) => b,
        };
        let other_ask = match (ba, self.my_ask) {
            (Some(a), Some(m)) if (a - m).abs() < t * 0.5 => None,
            (a, _) => a,
        };
        // Bid: one tick inside the best other bid, within [center - wide, center - min_h].
        let bid_cap = floor_tick(center - min_h, t);
        let mut bid = match other_bid {
            Some(b) => b + t,
            None => self.my_bid.unwrap_or(floor_tick(center - wide, t)),
        };
        bid = bid.min(bid_cap).max(floor_tick(center - wide - t, t));
        let ask_floor = ceil_tick(center + min_h, t);
        let mut ask = match other_ask {
            Some(a) => a - t,
            None => self.my_ask.unwrap_or(ceil_tick(center + wide, t)),
        };
        ask = ask.max(ask_floor).min(ceil_tick(center + wide + t, t));
        // If we're best and the other side is far behind, ease toward the wide price? keep.
        if unwinding {
            // Only quote the reducing side aggressively; other side wide.
            if pos > 0.0 {
                bid = floor_tick(center - wide, t);
            } else {
                ask = ceil_tick(center + wide, t);
            }
        }
        // Never cross the book.
        if let Some(a) = other_ask {
            bid = bid.min(a - t);
        }
        if let Some(b) = other_bid {
            ask = ask.max(b + t);
        }
        bid = (bid / t).round() * t;
        ask = (ask / t).round() * t;

        let bsize = if unwinding && pos < 0.0 { pos.abs().max(QUOTE_SIZE) } else { QUOTE_SIZE };
        let asize = if unwinding && pos > 0.0 { pos.abs().max(QUOTE_SIZE) } else { QUOTE_SIZE };
        let filled = self.fills.back().map(|f| f.0 == slot).unwrap_or(false);
        let same = self.my_bid.map(|b| (b - bid).abs() < t * 0.5).unwrap_or(false)
            && self.my_ask.map(|a| (a - ask).abs() < t * 0.5).unwrap_or(false);
        if !same || filled {
            orders.cancel_all();
            orders.bid(bid, bsize);
            orders.ask(ask, asize);
            self.my_bid = Some(bid);
            self.my_ask = Some(ask);
        }
        self.expected_pos = Some(expected);
        Ok(())
    }
}

fn main() -> BotResult {
    mtx_kit::run(Bot::default())
}
