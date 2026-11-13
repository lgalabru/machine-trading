//! Oracle-anchored SPCX market maker.
//!
//! Reads the public Phoenix order book and its trade ring, removes its own
//! aggressive flow, and predicts short-horizon adverse selection. Quotes lean
//! against inventory and toward the forecast. Strong signals permit a bounded
//! directional position; expired signals unwind through the book.
//!
//! Risk is checked at the oracle using the trader account's cash and position.
//! A shock reserve shrinks the exposure budget before the hard drawdown stop.

mod model;
use mtx_kit::{
    log,
    prelude::*,
    rpc_account,
    venue::{
        PhoenixVenue,
        phoenix_rise::accounts::owned::{Orderbook, Trader},
    },
};
use std::collections::VecDeque;

#[derive(Default)]
struct Maker {
    tick: f64,
    lots: f64,
    address: String,
    prev_price: f64,
    prev_pos: f64,
    prev_slot: u64,
    start_slot: u64,
    flow: f64,
    drift: f64,
    half: f64,
    trades: VecDeque<(u64, f64)>,
    burst: f64,
    burst_until: u64,
    own_id: Option<u32>,
    last_bid: f64,
    last_ask: f64,
    last_size: f64,
    last_seq: u64,
    suppress: bool,
    pending_take: f64,
    own_key: String,
    had_burst: bool,
    last_jump: u64,
    peak_equity: f64,
    stopped: bool,
    unwind_until: u64,
}
impl Strategy for Maker {
    fn start(&mut self, s: &Session) -> BotResult {
        let venue = PhoenixVenue::from_json(&s.venue)?;
        let market = venue.market(&s.symbol)?;
        self.tick = venue.ticks_to_usd(market, 1);
        self.lots = 10f64.powi(market.base_lots_decimals as i32);
        self.address = market.market_pubkey.clone();
        self.own_key = venue
            .trader_account(&mtx_kit::venue::key(&s.authority)?)
            .to_string();
        if let Ok(Some(data)) = rpc_account(&self.address) {
            if let Ok(book) = Orderbook::try_from_account_bytes(&data) {
                self.last_seq = book
                    .header
                    .trade_list
                    .iter()
                    .map(|t| t.trade_sequence_number)
                    .max()
                    .unwrap_or(0);
            }
        }
        self.peak_equity = 10000.0;
        self.prev_price = s.start_oracle;
        self.prev_slot = s.start_slot;
        self.start_slot = s.start_slot;
        log(&format!(
            "start oracle={} tick={} lots={}",
            s.start_oracle, self.tick, self.lots
        ));
        Ok(())
    }
    fn step(&mut self, m: &Market, o: &mut Orders) -> BotResult {
        let p = m.oracle;
        if !p.is_finite() || p <= 0.0 {
            o.cancel_all();
            return Ok(());
        }
        let pos = m.position.size;
        let mut dd = m.drawdown;
        if let Ok(Some(data)) = rpc_account(&self.own_key) {
            if let Ok(trader) = Trader::try_from_account_bytes(&data) {
                let mut equity = trader.state.quote_lot_collateral.as_inner() as f64 / 1e6;
                for entry in trader.position_entries() {
                    equity += entry.position.virtual_quote_lot_position.as_inner() as f64 / 1e6
                        + entry.position.base_lot_position.as_inner() as f64 / self.lots * p;
                }
                self.peak_equity = self.peak_equity.max(equity);
                dd = dd.max(self.peak_equity - equity);
                if m.k % 20 == 0 {
                    log(&format!("equity={:.4} true_dd={:.4}", equity, dd));
                }
            }
        }
        if dd > 145.0 {
            self.stopped = true;
        }

        let change = p / self.prev_price - 1.0;
        if change.abs() < 0.005 {
            self.drift = 0.98 * self.drift + 0.02 * change;
        }
        self.flow *= 0.88;
        let mut cb = m.best_bid.unwrap_or(p * 0.998);
        let mut ca = m.best_ask.unwrap_or(p * 1.002);
        if let Ok(Some(data)) = rpc_account(&self.address) {
            if let Ok(book) = Orderbook::try_from_account_bytes(&data) {
                if self.own_id.is_none() && self.prev_slot > self.start_slot {
                    for e in book.bids.iter().chain(book.asks.iter()) {
                        let price = e.order_id.price_in_ticks.as_inner() as f64 * self.tick;
                        if e.order.initial_slot == self.prev_slot
                            && (price - self.last_bid).abs() < self.tick * 0.1
                            && (e.order.initial_trade_size as f64 / self.lots - self.last_size)
                                .abs()
                                < 0.001
                        {
                            self.own_id = e.order.trader_position_id.trader_id;
                        }
                    }
                }
                cb = book
                    .bids
                    .iter()
                    .find(|e| {
                        self.own_id.is_none() || e.order.trader_position_id.trader_id != self.own_id
                    })
                    .map(|e| e.order_id.price_in_ticks.as_inner() as f64 * self.tick)
                    .unwrap_or(p * (1.0 - self.half.max(0.0008)));
                ca = book
                    .asks
                    .iter()
                    .find(|e| {
                        self.own_id.is_none() || e.order.trader_position_id.trader_id != self.own_id
                    })
                    .map(|e| e.order_id.price_in_ticks.as_inner() as f64 * self.tick)
                    .unwrap_or(p * (1.0 + self.half.max(0.0008)));
                // Deduct our aggressive executions: they are not independent information.
                let passive: f64 = book
                    .header
                    .trade_list
                    .iter()
                    .filter(|t| {
                        t.trade_sequence_number > self.last_seq
                            && t.maker.to_string() == self.own_key
                    })
                    .map(|t| t.maker_fill.quantity as f64 / self.lots)
                    .sum();
                let mut own_take = if self.pending_take != 0.0 {
                    pos - self.prev_pos - passive
                } else {
                    0.0
                };
                for t in &book.header.trade_list {
                    if t.trade_sequence_number > self.last_seq {
                        let mut qty = -(t.maker_fill.quantity as f64) / self.lots;
                        if own_take * qty > 0.0 {
                            let remove = own_take.abs().min(qty.abs()) * qty.signum();
                            own_take -= remove;
                            qty -= remove;
                        }
                        if qty.abs() < 0.002 {
                            continue;
                        }
                        self.flow += qty;
                        self.trades.push_back((m.slot, qty));
                        log(&format!(
                            "trade slot={} qty={:.3} price={:.2}",
                            t.slot - self.start_slot,
                            qty,
                            t.maker_fill.price as f64 * self.tick
                        ));
                    }
                }
                self.last_seq = book
                    .header
                    .trade_list
                    .iter()
                    .map(|t| t.trade_sequence_number)
                    .max()
                    .unwrap_or(self.last_seq);
            } else if m.k == 0 {
                log("book decode failed");
            }
        }
        self.suppress = false;
        self.pending_take = 0.0;
        while self
            .trades
            .front()
            .is_some_and(|t| m.slot.saturating_sub(t.0) > 120)
        {
            self.trades.pop_front();
        }
        if self.half == 0.0 {
            self.half = ((ca - cb) / p / 2.0).clamp(0.0008, 0.003);
        }
        if change.abs() > 0.005 {
            self.flow = 0.0;
            self.trades.clear();
            self.last_jump = m.slot;
        }
        let mut features = [0.0; 15];
        for (i, window) in [4_u64, 8, 16, 30, 60, 120].iter().enumerate() {
            for &(slot, q) in &self.trades {
                if m.slot.saturating_sub(slot) < *window {
                    features[2 * i + usize::from(q < 0.0)] += (q.abs() / 0.6).clamp(0.5, 1.5);
                }
            }
        }
        features[12] = m.slot.saturating_sub(self.last_jump).min(100) as f64;
        features[13] = m.slot.saturating_sub(self.start_slot).min(100) as f64;
        features[14] = m.slots_left.min(100) as f64;
        let alpha = model::predict(&features);
        let entry_threshold = 0.002 + 2.0 * self.half;
        if alpha.abs() > entry_threshold
            && m.slot.saturating_sub(self.last_jump) > 30
            && m.slots_left > 40
        {
            self.burst = alpha.signum();
            self.burst_until = m.slot + 24;
        }
        if m.slot > self.burst_until || change.abs() > 0.005 {
            self.burst = 0.0;
        }
        let expired = self.had_burst && self.burst == 0.0;
        if expired {
            self.unwind_until = m.slot + 32;
        }
        self.had_burst = self.burst != 0.0;
        // Reserve for a 4.5% adverse move and $35 of execution/model error.
        let risk_cap = ((m.max_drawdown - dd - 35.0).max(0.0) / (p * 0.045))
            .min(18.0)
            .min(m.cap - 0.01);
        let trend = (self.drift * 60000.0).clamp(-6.0, 6.0);
        let target = if self.burst != 0.0 {
            self.burst * risk_cap
        } else if m.slot <= self.unwind_until {
            0.0
        } else {
            (trend + alpha * 4000.0).clamp(-8.0, 8.0)
        };
        o.cancel_all();
        let finish_unwind = self.unwind_until != 0
            && m.slot > self.unwind_until
            && self.burst == 0.0
            && pos.abs() > 5.0;
        if self.stopped || finish_unwind || pos.abs() > risk_cap + 0.1 {
            let target = if self.stopped {
                0.0
            } else if finish_unwind {
                self.unwind_until = 0;
                pos.signum() * 5.0
            } else {
                pos.signum() * risk_cap
            };
            let amount = target - pos;
            let mut left = amount.abs();
            while left > 0.01 {
                let q = left.min(10.0);
                if amount > 0.0 {
                    o.buy(q, p * 1.004);
                } else {
                    o.sell(q, p * 0.996);
                }
                left -= q;
            }
            self.pending_take = amount;
        } else {
            // Burst signals warrant paying the spread to avoid being trapped on the wrong side.
            if self.burst.abs() > 0.2 && (target - pos) * self.burst.signum() > 2.0 {
                self.pending_take = target - pos;
                let mut left = self.pending_take.abs();
                while left > 0.01 {
                    let size = left.min(10.0);
                    if target > pos {
                        o.buy(size, p * 1.004);
                    } else {
                        o.sell(size, p * 0.996);
                    }
                    left -= size;
                }
            }
            let skew = (target - pos) * 0.00015 + alpha * 0.6;
            cb = cb.max(p * (1.0 - self.half));
            ca = ca.min(p * (1.0 + self.half));
            let min_width = 0.00015;
            let bid = (cb + self.tick + p * skew).min(p * (1.0 - min_width));
            let ask = (ca - self.tick + p * skew).max(p * (1.0 + min_width));
            let bid = (bid / self.tick).floor() * self.tick;
            let ask = (ask / self.tick).ceil() * self.tick;
            let projected = pos + self.pending_take;
            let bs = 2.0_f64.min((risk_cap - pos.max(projected)).max(0.0));
            let az = 2.0_f64.min((risk_cap + pos.min(projected)).max(0.0));
            // Keep competitive liquidity near its floor with minimum-lot inner quotes.
            let probe = 1.0 / self.lots;
            let probe_bid = ((p / self.tick).floor() - 1.0) * self.tick;
            let probe_ask = ((p / self.tick).ceil() + 1.0) * self.tick;
            if pos.max(projected) + probe < m.cap {
                o.bid(probe_bid, probe);
            }
            if pos.min(projected) - probe > -m.cap {
                o.ask(probe_ask, probe);
            }
            if bs >= 0.01 {
                o.bid(bid, bs);
            }
            if az >= 0.01 {
                o.ask(ask, az);
            }
            self.last_bid = bid;
            self.last_ask = ask;
            self.last_size = bs;
        }
        self.prev_price = p;
        self.prev_pos = pos;
        self.prev_slot = m.slot;
        if m.k % 10 == 0 || self.burst != 0.0 {
            log(&format!(
                "k={} slot={} p={:.4} pos={:.3} pnl={:.2} dd={:.2} flow={:.2} signal={:.2} target={:.2} cb={:.2} ca={:.2} id={:?}",
                m.k,
                m.slot - self.start_slot,
                p,
                pos,
                m.pnl,
                m.drawdown,
                self.flow,
                alpha * 100.0,
                target,
                cb,
                ca,
                self.own_id
            ));
        }
        Ok(())
    }
}
mtx_kit::export!(Maker);

#[cfg(test)]
mod tests;
