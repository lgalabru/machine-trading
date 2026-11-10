use mtx_kit::prelude::*;
use std::collections::VecDeque;
use tracing::{info, warn};

const TICK: f64 = 0.01;
const H_MIN_BPS: f64 = 3.0; // tightest half-spread we quote, bps
const QUOTE_SIZE: f64 = 3.0;
const FLOW_WINDOW: usize = 15; // steps
const SHORT_WINDOW: usize = 6;
const SHORT_THR: f64 = 4.0;
const H_MAX_BPS: f64 = 12.0;
const JUMP_BPS: f64 = 80.0;
const JUMP_COOLDOWN: u64 = 12;
const RET_WINDOW: usize = 80; // steps
const SKEW_BPS_PER_SHARE: f64 = 1.0;
const MARKET_THRESHOLD: f64 = 5.0; // shares off target before we cross
const BURST_THR: f64 = 4.5; // net shares over the window before we lean
const BURST_RAMP: f64 = 3.0;
const BURST_HOLD: u64 = 20; // steps to hold a burst lean
const TREND_DEAD_BPS: f64 = 35.0;
const TREND_FULL_BPS: f64 = 90.0; // clipped drift over window for a full lean
const JUMP_CLIP_BPS: f64 = 30.0;
const DD_SOFT: f64 = 110.0;
const DD_HARD: f64 = 170.0;

#[derive(Default)]
struct Mm {
    prev_pos: Option<f64>,
    last_bid: f64,
    last_ask: f64,
    pending_market: f64, // signed size of market orders sent last step
    flow: VecDeque<f64>, // signed fills we received (positive: takers sold to us)
    rets: VecDeque<f64>, // clipped per-step oracle returns, bps
    last_oracle: Option<f64>,
    cash: f64,
    peak_value: f64,
    fees: f64,
    burst_target: f64,
    burst_until: u64,
    cooldown_until: u64,
}

impl Mm {
    fn value(&self, pos: f64, oracle: f64) -> f64 {
        self.cash + pos * oracle - self.fees
    }
}

impl Strategy for Mm {
    async fn start(&mut self, session: &Session) -> BotResult {
        info!(symbol = session.env.symbol, steps = session.hello.steps, "started");
        Ok(())
    }

    async fn step(&mut self, m: &Market, orders: &mut Orders<'_>) -> BotResult {
        let oracle = m.oracle;
        let pos = m.position.size;
        // --- account for fills since last step ---
        if let Some(prev) = self.prev_pos {
            let mut delta = pos - prev;
            // market orders we sent last step: approximate their price
            if self.pending_market != 0.0 {
                let mk = if delta.abs() < self.pending_market.abs() { delta } else { self.pending_market };
                let px = if mk > 0.0 { oracle * (1.0 + 0.0020) } else { oracle * (1.0 - 0.0020) };
                self.cash -= mk * px;
                self.fees += mk.abs() * px * 0.00035;
                delta -= mk;
            }
            if delta.abs() > 1e-9 {
                let px = if delta > 0.0 { self.last_bid } else { self.last_ask };
                let px = if px > 0.0 { px } else { oracle };
                self.cash -= delta * px;
                self.fees += delta.abs() * px * 0.00005;
            }
            self.flow.push_back(delta);
        } else {
            self.flow.push_back(0.0);
        }
        while self.flow.len() > FLOW_WINDOW { self.flow.pop_front(); }
        self.prev_pos = Some(pos);
        self.pending_market = 0.0;

        if let Some(lo) = self.last_oracle {
            let r = ((oracle / lo) - 1.0) * 1e4;
            self.rets.push_back(r.clamp(-JUMP_CLIP_BPS, JUMP_CLIP_BPS));
            if r.abs() > JUMP_BPS {
                info!(k = m.k, r, "jump landed: clearing flow");
                self.flow.clear();
                self.burst_target = 0.0;
                self.burst_until = 0;
                self.cooldown_until = m.k + JUMP_COOLDOWN;
            }
        }
        while self.rets.len() > RET_WINDOW { self.rets.pop_front(); }
        self.last_oracle = Some(oracle);

        let value = self.value(pos, oracle);
        self.peak_value = self.peak_value.max(value);
        let dd = self.peak_value - value;

        // --- signals ---
        let net_flow: f64 = self.flow.iter().sum(); // >0: we were bought from... we received
        // takers bought when we sold: net_flow < 0 => bullish flow
        let short_flow: f64 = self.flow.iter().rev().take(SHORT_WINDOW).sum();
        let s_long = net_flow.abs() / BURST_THR;
        let s_short = short_flow.abs() / SHORT_THR;
        let (ratio, sign) = if s_short >= s_long { (s_short, -short_flow.signum()) } else { (s_long, -net_flow.signum()) };
        let strength = if ratio >= 1.0 { (0.5 + (ratio - 1.0) * 1.5).min(1.0) } else { 0.0 };
        let soft_sign = if ratio >= 0.8 { sign } else { 0.0 };
        if strength > 0.0 && m.k >= self.cooldown_until {
            let lean = sign * strength * m.cap;
            if lean.abs() >= self.burst_target.abs() || lean.signum() != self.burst_target.signum() {
                self.burst_target = lean;
            }
            self.burst_until = m.k + BURST_HOLD;
        } else if m.k >= self.burst_until {
            self.burst_target = 0.0;
        }
        let burst_lean = self.burst_target;
        let drift: f64 = self.rets.iter().sum();
        let trend_lean = if drift.abs() > TREND_DEAD_BPS {
            drift.signum() * ((drift.abs() - TREND_DEAD_BPS) / (TREND_FULL_BPS - TREND_DEAD_BPS)).min(1.0) * m.cap
        } else { 0.0 };
        let mut lean_cap = m.cap;
        if dd > DD_HARD { lean_cap = 0.0 } else if dd > DD_SOFT { lean_cap = m.cap * 0.4 }
        let target = (burst_lean + trend_lean).clamp(-lean_cap, lean_cap);

        // --- cancel, read the competitor's book ---
        let trader = orders.trader();
        let (mut cbid, mut cask) = (oracle * (1.0 - 0.0015), oracle * (1.0 + 0.0015));
        if let Some(t) = trader {
            if let Err(e) = t.cancel_all() { warn!(%e, "cancel failed"); }
            match t.inner.bbo().await {
                Ok(bbo) => {
                    if let Some(b) = bbo.best_bid_ticks() { cbid = b as f64 * TICK; }
                    if let Some(a) = bbo.best_ask_ticks() { cask = a as f64 * TICK; }
                }
                Err(e) => warn!(%e, "bbo failed"),
            }
        }

        let h_min = oracle * H_MIN_BPS * 1e-4;
        let shift = (target - pos) * SKEW_BPS_PER_SHARE * 1e-4 * oracle;
        let h_max = oracle * H_MAX_BPS * 1e-4;
        let mut bid = (cbid + TICK).min(oracle - h_min).max(oracle - h_max) + shift;
        let mut ask = (cask - TICK).max(oracle + h_min).min(oracle + h_max) + shift;
        // never cross the competitor
        bid = bid.min(cask - TICK);
        ask = ask.max(cbid + TICK);
        if ask - bid < 2.0 * TICK { bid = oracle - h_min; ask = oracle + h_min; }

        let mut size = QUOTE_SIZE;
        if dd > DD_HARD { size = 0.5; }
        let bid_size = if target - pos > 8.0 { size * 1.5 } else { size };
        let ask_size = if pos - target > 8.0 { size * 1.5 } else { size };
        // when leaning hard, don't offer the side that fights the lean
        let burst_gap = burst_lean - pos;
        let mut skip_bid = false;
        let mut skip_ask = false;
        if burst_lean.abs() > 0.0 && burst_gap.abs() > MARKET_THRESHOLD && burst_gap.signum() == burst_lean.signum() && dd < DD_HARD {
            let qty = burst_gap.abs().min(m.cap);
            if burst_gap > 0.0 {
                orders.buy(qty, oracle * (1.0 + 0.0045));
                self.pending_market = qty;
            } else {
                orders.sell(qty, oracle * (1.0 - 0.0045));
                self.pending_market = -qty;
            }
        }
        if burst_lean > 0.0 || soft_sign > 0.0 { skip_ask = true; }
        if burst_lean < 0.0 || soft_sign < 0.0 { skip_bid = true; }
        if m.last {
            // nothing to gain from resting at the last step
        } else {
            if !skip_bid { orders.bid(bid, bid_size); }
            if !skip_ask { orders.ask(ask, ask_size); }
        }
        self.last_bid = bid;
        self.last_ask = ask;
        if m.k % 20 == 0 || self.pending_market != 0.0 {
            info!(k = m.k, oracle, pos, target = format!("{target:.1}"), burst = format!("{burst_lean:.1}"), net_flow = format!("{net_flow:.2}"), drift = format!("{drift:.1}"), value = format!("{value:.2}"), dd = format!("{dd:.1}"), cbid, cask, bid = format!("{bid:.2}"), ask = format!("{ask:.2}"), "step");
        }
        Ok(())
    }

    fn end(&mut self, last: Option<&Market>) {
        if let Some(m) = last {
            info!(value = self.value(m.position.size, m.oracle), fees = self.fees, pos = m.position.size, "end");
        }
    }
}

fn main() -> BotResult {
    mtx_kit::run(Mm::default())
}
