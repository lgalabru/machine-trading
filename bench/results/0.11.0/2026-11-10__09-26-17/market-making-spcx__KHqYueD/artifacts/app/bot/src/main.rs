//! SPCX market maker: quotes inside the competing makers around the oracle,
//! skews against inventory, and reads one-sided taker flow against its own
//! quotes as a warning of informed orders ahead of a jump.

use mtx_kit::prelude::*;
use std::collections::VecDeque;
use tracing::info;

#[derive(Clone, Copy)]
struct Params {
    min_edge_bp: f64,   // never quote closer than this to the oracle
    size: f64,          // quote size per side
    skew_bp: f64,       // bp of skew at full cap inventory
    flow_window: u64,   // slots of taker flow to sum
    flow_trigger: f64,  // SPCX of net taker flow that triggers a lean
    lean_slots: u64,    // how long a lean lasts
    lean_pos: f64,      // position to lean to
    jump_bp: f64,       // oracle move in one step that counts as a jump
    drift_lean: f64,    // position per bp/step of drift
    size_skew: f64,     // how much quote sizes shrink on the inventory side
    max_inv: f64,       // stop adding inventory past this
}

impl Default for Params {
    fn default() -> Self {
        Params {
            min_edge_bp: 4.0,
            size: 0.7,
            skew_bp: 10.0,
            flow_window: 24,
            flow_trigger: 1000.0,
            lean_slots: 50,
            lean_pos: 19.0,
            jump_bp: 80.0,
            drift_lean: 0.0,
            size_skew: 1.0,
            max_inv: 1.5,
        }
    }
}

impl Params {
    fn load() -> Self {
        let mut p = Params::default();
        if let Ok(s) = std::fs::read_to_string("/tmp/mmparams") {
            for line in s.lines() {
                let mut it = line.split('=');
                let (Some(k), Some(v)) = (it.next(), it.next()) else { continue };
                let Ok(v) = v.trim().parse::<f64>() else { continue };
                match k.trim() {
                    "min_edge_bp" => p.min_edge_bp = v,
                    "size" => p.size = v,
                    "skew_bp" => p.skew_bp = v,
                    "flow_window" => p.flow_window = v as u64,
                    "flow_trigger" => p.flow_trigger = v,
                    "lean_slots" => p.lean_slots = v as u64,
                    "lean_pos" => p.lean_pos = v,
                    "jump_bp" => p.jump_bp = v,
                    "drift_lean" => p.drift_lean = v,
                    "size_skew" => p.size_skew = v,
                    "max_inv" => p.max_inv = v,
                    _ => {}
                }
            }
        }
        p
    }
}

#[derive(Default)]
struct Maker {
    p: Params,
    tick: f64,
    // Our resting quotes from the last turn (rounded prices), if any.
    my_bid: Option<f64>,
    my_ask: Option<f64>,
    last_pos: Option<f64>,
    expected_delta: f64,
    last_oracle: Option<f64>,
    // (slot, signed taker volume: + = takers bought)
    flow: VecDeque<(u64, f64)>,
    lean: f64,
    lean_until: u64,
    // Competitor half-spread estimate in bp.
    comp_bp: Option<f64>,
    drift: f64,
    start_oracle: f64,
}

impl Strategy for Maker {
    async fn start(&mut self, session: &Session) -> BotResult {
        self.p = Params::load();
        let t = &session.trader;
        let m = t.phoenix.market(&session.env.symbol)?;
        self.tick = t.phoenix.ticks_to_usd(m, 1);
        info!(tick = self.tick, "started");
        Ok(())
    }

    async fn step(&mut self, market: &Market, orders: &mut Orders<'_>) -> BotResult {
        let p = self.p;
        let fair = market.oracle;
        let pos = market.position.size;
        let slot = market.slot;
        if self.start_oracle == 0.0 {
            self.start_oracle = fair;
        }

        // Taker flow against us since the last turn.
        if let Some(lp) = self.last_pos {
            let fills = pos - lp - self.expected_delta;
            if fills.abs() > 1e-9 {
                self.flow.push_back((slot, -fills));
            }
        }
        self.expected_delta = 0.0;
        self.last_pos = Some(pos);
        while let Some(&(s, _)) = self.flow.front() {
            if s + p.flow_window < slot {
                self.flow.pop_front();
            } else {
                break;
            }
        }

        // Oracle moves: jumps end a lean; drift estimate.
        if let Some(lo) = self.last_oracle {
            let mv_bp = (fair - lo) / lo * 1e4;
            if mv_bp.abs() >= p.jump_bp {
                self.lean_until = 0;
                self.flow.clear();
                info!(k = market.k, mv_bp, pos, "jump");
            } else {
                self.drift = 0.97 * self.drift + 0.03 * mv_bp;
            }
        }
        self.last_oracle = Some(fair);
        let net: f64 = self.flow.iter().map(|x| x.1).sum();

        if net.abs() >= p.flow_trigger && slot >= self.lean_until.saturating_sub(p.lean_slots / 2) {
            self.lean = net.signum() * p.lean_pos;
            self.lean_until = slot + p.lean_slots;
            self.flow.clear();
            info!(k = market.k, net, "lean");
        }
        let leaning = slot < self.lean_until;
        let mut target = if leaning { self.lean } else { 0.0 };
        if !leaning {
            target = (self.drift * p.drift_lean).clamp(-p.lean_pos, p.lean_pos);
        }

        // Book: best competing prices.
        let mut best_bid = None;
        let mut best_ask = None;
        if let Some(t) = orders.trader() {
            if let Ok(bbo) = t.inner.bbo().await {
                let m = t.phoenix.market(&t.env.symbol)?;
                best_bid = bbo.best_bid_ticks().map(|x| t.phoenix.ticks_to_usd(m, x));
                best_ask = bbo.best_ask_ticks().map(|x| t.phoenix.ticks_to_usd(m, x));
            }
        }
        let near = |a: f64, b: Option<f64>| b.is_some_and(|b| (a - b).abs() < self.tick * 0.5);
        let bid_is_mine = best_bid.is_some_and(|b| near(b, self.my_bid));
        let ask_is_mine = best_ask.is_some_and(|a| near(a, self.my_ask));
        if let (Some(b), Some(a)) = (best_bid, best_ask) {
            if !bid_is_mine && !ask_is_mine {
                let h = (a - b) / 2.0 / fair * 1e4;
                self.comp_bp = Some(match self.comp_bp {
                    None => h,
                    Some(c) => c.min(h).max(0.0),
                });
            }
        }

        let min_edge = fair * p.min_edge_bp / 1e4;
        // Inventory skew relative to target.
        let skew = -(pos - target) / market.cap * p.skew_bp * fair / 1e4;
        let center = fair + skew;
        // Bid: one tick inside the best competing bid, but no closer than min_edge.
        let comp_bid = if bid_is_mine { None } else { best_bid };
        let comp_ask = if ask_is_mine { None } else { best_ask };
        let default_half = fair * self.comp_bp.unwrap_or(10.0) / 1e4;
        let mut bid = match comp_bid {
            Some(b) => b + self.tick,
            None => self.my_bid.unwrap_or(fair - default_half),
        };
        let mut ask = match comp_ask {
            Some(a) => a - self.tick,
            None => self.my_ask.unwrap_or(fair + default_half),
        };
        bid = bid.min(center - min_edge).max(fair - default_half * 2.0);
        ask = ask.max(center + min_edge).min(fair + default_half * 2.0);
        bid = bid.min(fair - fair * 0.5 / 1e4);
        ask = ask.max(fair + fair * 0.5 / 1e4);
        let bid = (bid / self.tick).floor() * self.tick;
        let ask = (ask / self.tick).ceil() * self.tick;

        // Move toward the target position with market orders when leaning.
        let gap = target - pos;
        let inv = ((pos - target) / market.cap).clamp(-1.0, 1.0);
        let mut bid_size = p.size * (1.0 - p.size_skew * inv).clamp(0.0, 2.0);
        let mut ask_size = p.size * (1.0 + p.size_skew * inv).clamp(0.0, 2.0);
        orders.cancel_all();
        if leaning && gap.abs() > 0.5 {
            let q = gap.abs().min(10.0);
            let q = (q * 10.0).floor() / 10.0;
            if gap > 0.0 {
                orders.buy(q, fair * 1.004);
                self.expected_delta += q;
            } else {
                orders.sell(q, fair * 0.996);
                self.expected_delta -= q;
            }
        }
        if leaning {
            // Don't quote against the lean.
            if self.lean > 0.0 {
                ask_size = 0.0;
            } else {
                bid_size = 0.0;
            }
        }
        if pos > p.max_inv && !leaning {
            bid_size = 0.0;
        }
        if pos < -p.max_inv && !leaning {
            ask_size = 0.0;
        }
        let room_buy = (market.cap - 0.5 - (pos + self.expected_delta)).max(0.0);
        let room_sell = (market.cap - 0.5 + (pos + self.expected_delta)).max(0.0);
        bid_size = (bid_size.min(room_buy) * 10.0).floor() / 10.0;
        ask_size = (ask_size.min(room_sell) * 10.0).floor() / 10.0;
        self.my_bid = None;
        self.my_ask = None;
        if bid_size >= 0.1 && !market.last {
            orders.bid(bid, bid_size);
            self.my_bid = Some(bid);
        }
        if ask_size >= 0.1 && !market.last {
            orders.ask(ask, ask_size);
            self.my_ask = Some(ask);
        }
        info!(
            k = market.k,
            fair,
            pos,
            net,
            target,
            bid,
            ask,
            bb = best_bid.unwrap_or(0.0),
            ba = best_ask.unwrap_or(0.0),
            comp = self.comp_bp.unwrap_or(0.0),
            pnl = market.pnl,
            "step"
        );
        Ok(())
    }
}

fn main() -> BotResult {
    mtx_kit::run(Maker::default())
}
