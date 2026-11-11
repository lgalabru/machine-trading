//! Oracle-anchored SPCX market making with inventory and event risk controls.
//!
//! The trade buffer is deduplicated by taker sequence. Our own transactions
//! and the competitor's post-move trades do not enter the flow predictor.
//! Ordinary quotes compete for spread; persistent directional evidence moves
//! the inventory target, with hysteresis to limit churn on ambiguous flow.

#[rustfmt::skip]
mod predict;
use mtx_kit::prelude::*;
use mtx_kit::{
    Pubkey,
    phoenix_rise::accounts::{orderbook::OrderbookHeader, trader::Trader as TraderAccount},
};
use solana_account_decoder_client_types::{UiAccountEncoding, UiDataSliceConfig};
use solana_commitment_config::CommitmentConfig;
use solana_rpc_client_api::config::RpcAccountInfoConfig;
use std::{
    collections::{BTreeMap, VecDeque},
    str::FromStr,
};

// Standard getAccountInfo with a data slice avoids downloading the full book.
fn read_header(t: &mtx_kit::Trader, address: &Pubkey) -> BotResult<OrderbookHeader> {
    let value = t
        .rpc
        .get_ui_account_with_config(
            address,
            RpcAccountInfoConfig {
                encoding: Some(UiAccountEncoding::Base64),
                data_slice: Some(UiDataSliceConfig {
                    offset: 0,
                    length: 5504,
                }),
                commitment: Some(CommitmentConfig::processed()),
                ..Default::default()
            },
        )?
        .value
        .ok_or("order book unavailable")?;
    let data = value.data.decode().ok_or("invalid account encoding")?;
    if data.len() != 5504 {
        return Err("invalid order book header size".into());
    }
    let header: OrderbookHeader = bytemuck::pod_read_unaligned(&data);
    if header.trade_list().len() > 64 || header.asset_symbol()?.as_str() != t.env.symbol {
        return Err("invalid order book header".into());
    }
    Ok(header)
}

// Reserve equity for a 4.5% adverse move and execution costs.
fn position_limit(price: f64, drawdown: f64, mandate_cap: f64, drawdown_limit: f64) -> f64 {
    mandate_cap
        .min(19.0)
        .min(((drawdown_limit - drawdown - drawdown_limit * 0.1) / (price * 0.045)).max(0.0))
}

// Include probe orders and allow for partial fills of aggressive orders.
fn quote_sizes(current: f64, previous: f64, cap: f64, signal: f64) -> (f64, f64) {
    let bid = if signal < -0.1 {
        0.0
    } else {
        2.5_f64.min((cap - current.max(previous) - 0.002).max(0.0))
    };
    let ask = if signal > 0.1 {
        0.0
    } else {
        2.5_f64.min((cap + current.min(previous) - 0.002).max(0.0))
    };
    (bid, ask)
}

#[derive(Default)]
struct Maker {
    last_price: f64,
    last_slot: u64,
    last_seq: u64,
    start_slot: u64,
    tick: f64,
    book: Option<Pubkey>,
    n: f64,
    sum: f64,
    sumsq: f64,
    flow: VecDeque<(u64, f64)>,
    peak: f64,
    stopped: bool,
    last_jump: u64,
    burst: f64,
}
impl Strategy for Maker {
    async fn start(&mut self, s: &Session) -> BotResult {
        if s.env.symbol != "SPCX" {
            return Err("this strategy trades SPCX only".into());
        }
        let c = s.trader.phoenix.market(&s.env.symbol)?;
        self.tick = s.trader.phoenix.ticks_to_usd(c, 1);
        self.book = Some(Pubkey::from_str(&c.market_pubkey)?);
        self.start_slot = s.hello.start_slot;
        self.last_slot = self.start_slot;
        self.last_jump = self.start_slot;
        self.peak = 10000.0;
        Ok(())
    }
    async fn step(&mut self, m: &Market, o: &mut Orders<'_>) -> BotResult {
        let p = m.oracle;
        let q = m.position.size;
        let t = o.trader().unwrap();
        // Cancel first: a failed read leaves no stale risk on the book.
        t.cancel_all()?;
        if !(p > 0.0 && p.is_finite()) || m.last {
            return Ok(());
        }
        let c = t.phoenix.market(&t.env.symbol)?;
        let a = t.rpc.get_account(&t.account)?;
        let account = TraderAccount::try_from_account_bytes(&a.data)?;
        let mut equity = account.header.trader_state.quote_lot_collateral.as_inner() as f64 / 1e6;
        for (id, pos) in account.positions() {
            if id == c.asset_id as u64 {
                equity += pos.virtual_quote_lot_position().as_inner() as f64 / 1e6;
                equity += t
                    .phoenix
                    .lots_to_base_units(c, pos.base_lot_position().as_inner())
                    * p;
            }
        }
        self.peak = self.peak.max(equity);
        let dd = (self.peak - equity).max(0.0);
        let book = read_header(t, &self.book.unwrap())?;
        let mut trades = BTreeMap::<u64, (u64, f64)>::new();
        for e in book.trade_list().iter() {
            if self.last_seq > 0 && e.trade_sequence_number > self.last_seq && e.slot < m.slot {
                let v = trades
                    .entry(e.trade_sequence_number)
                    .or_insert((e.slot, 0.0));
                v.1 -= t
                    .phoenix
                    .lots_to_base_units(c, e.maker_fill.quantity.as_inner());
            }
        }
        self.last_seq = book
            .trade_list()
            .iter()
            .map(|e| e.trade_sequence_number)
            .max()
            .unwrap_or(self.last_seq);
        for (_, (slot, qty)) in trades {
            if qty.abs() > 0.0001 {
                self.flow.push_back((slot, qty.signum()));
            }
        }
        while self
            .flow
            .front()
            .is_some_and(|(slot, _)| m.slot - slot > 124)
        {
            self.flow.pop_front();
        }
        let mut jumped = false;
        if self.last_price > 0.0 {
            let r = (p / self.last_price - 1.0) * 10000.0;
            if r.abs() > 60.0 {
                self.last_jump = m.slot;
                jumped = true;
                self.burst = 0.0;
            } else if r.abs() > 0.01 {
                self.n += 1.0;
                self.sum += r;
                self.sumsq += r * r;
            }
        }
        self.last_slot = m.slot;
        self.last_price = p;
        // Shrink drift estimates toward a zero-drift regime; exclude jumps.
        let variance = (self.sumsq / (self.n + 1.0)).max(9.0);
        let mut up = 0.0;
        let mut down = 0.0;
        for mu in [0.6, 1.2, 1.8] {
            up += ((mu * self.sum - 0.5 * self.n * mu * mu) / variance)
                .clamp(-30.0, 30.0)
                .exp()
                / 3.0;
            down += ((-mu * self.sum - 0.5 * self.n * mu * mu) / variance)
                .clamp(-30.0, 30.0)
                .exp()
                / 3.0;
        }
        let trend = (up - down) / (6.0 + up + down);
        // Six short arrival-count windows, background rates, and event timing.
        let mut features = [0.0; 17];
        for (i, window) in [6u64, 10, 16, 24, 32, 48].iter().enumerate() {
            for &(slot, sign) in &self.flow {
                if m.slot - slot < *window && slot >= self.last_jump {
                    features[2 * i + if sign > 0.0 { 0 } else { 1 }] += 1.0;
                }
            }
        }
        let elapsed = m.slot - self.start_slot;
        let k = elapsed / 2;
        let denom = ((k.saturating_sub(16) - k.saturating_sub(60)) * 2).max(1) as f64;
        for &(slot, sign) in &self.flow {
            let age = m.slot - slot;
            if age >= 34 && age <= 122 {
                features[12 + if sign > 0.0 { 0 } else { 1 }] += 1.0 / denom;
            }
        }
        features[14] = elapsed.min(60) as f64;
        features[15] = m.slots_left.min(60) as f64;
        features[16] = (m.slot - self.last_jump).min(60) as f64;
        let up_prob = predict::predict(&features);
        for i in (0..14).step_by(2) {
            features.swap(i, i + 1);
        }
        let down_prob = predict::predict(&features);
        let probability = up_prob - down_prob;
        // A lower exit threshold keeps a position through pauses in a burst.
        if probability > 0.25 {
            self.burst = 1.0;
        } else if probability < -0.25 {
            self.burst = -1.0;
        } else if probability * self.burst < 0.008 {
            self.burst = 0.0;
        }
        let signal = self.burst;
        let cap = position_limit(p, dd, m.cap, m.max_drawdown);
        if dd > m.max_drawdown * 0.44 {
            self.stopped = true;
        }
        if self.stopped {
            let mut remaining = q.abs();
            while remaining >= 0.001 {
                let size = remaining.min(5.0);
                if q > 0.0 {
                    o.sell(size, p * 0.995);
                } else {
                    o.buy(size, p * 1.005);
                }
                remaining -= size;
            }
            return Ok(());
        }
        let target = if signal.abs() > 0.1 {
            signal * cap
        } else {
            (12.0 * trend).clamp(-cap, cap)
        };
        let bbo = t.inner.bbo().await?;
        let bb = bbo
            .best_bid_ticks()
            .map(|x| t.phoenix.ticks_to_usd(c, x))
            .unwrap_or(p * 0.998);
        let ba = bbo
            .best_ask_ticks()
            .map(|x| t.phoenix.ticks_to_usd(c, x))
            .unwrap_or(p * 1.002);
        let mut working_q = q;
        // Strong burst evidence justifies paying the spread to remove adverse inventory.
        if (signal.abs() > 0.45 && (target - q).abs() > 2.0) || q.abs() > cap + 0.01 {
            let aim = if q.abs() > cap {
                q.clamp(-cap, cap)
            } else {
                target
            };
            let delta = aim - q;
            let mut remaining = delta.abs();
            while remaining > 0.01 {
                let size = remaining.min(8.0);
                t.place_market(
                    if delta > 0.0 { Side::Bid } else { Side::Ask },
                    size,
                    p * if delta > 0.0 { 1.004 } else { 0.996 },
                )
                .await?;
                remaining -= size;
            }
            working_q = t.position().await?.size;
            let after = read_header(t, &self.book.unwrap())?;
            self.last_seq = after
                .trade_list()
                .iter()
                .map(|e| e.trade_sequence_number)
                .max()
                .unwrap_or(self.last_seq);
        }
        let skew = ((target - working_q) * 1.2 + signal * 4.0).clamp(-22.0, 22.0);
        let bid_bps = ((p - bb - self.tick) / p * 10000.0).clamp(2.0, 20.0) - skew;
        let ask_bps = ((ba - p - self.tick) / p * 10000.0).clamp(2.0, 20.0) + skew;
        let bid = p * (1.0 - bid_bps.max(0.5) / 10000.0);
        let ask = p * (1.0 + ask_bps.max(0.5) / 10000.0);
        let (bs, az) = quote_sizes(working_q, q, cap, signal);
        if cap - working_q.max(q) >= 0.001 && (signal < -0.1 || probability < -0.08) {
            let _ = t.place_limit(Side::Bid, p - self.tick, 0.001).await;
        }
        if cap + working_q.min(q) >= 0.001 && (signal > 0.1 || probability > 0.08) {
            let _ = t.place_limit(Side::Ask, p + self.tick, 0.001).await;
        }
        if bs >= 0.001 {
            let _ = t.place_limit(Side::Bid, bid, bs).await;
        }
        if az >= 0.001 {
            let _ = t.place_limit(Side::Ask, ask, az).await;
        }
        // Quotes can cross a stale competitor after an oracle move. These are
        // our trades, not evidence of informed outside flow.
        let after = read_header(t, &self.book.unwrap())?;
        self.last_seq = after
            .trade_list()
            .iter()
            .map(|e| e.trade_sequence_number)
            .max()
            .unwrap_or(self.last_seq);
        if m.k % 10 == 0 || jumped || signal.abs() > 0.45 {
            eprintln!(
                "k={} p={:.3} q={:.3} eq={:.3} dd={:.2} trend={:.3} signal={:.3} target={:.2} nflow={} jump={} prob={:.4}",
                m.k,
                p,
                q,
                equity,
                dd,
                trend,
                signal,
                target,
                self.flow.len(),
                jumped,
                probability
            );
        }
        Ok(())
    }
}
fn main() -> BotResult {
    mtx_kit::run(Maker::default())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn exposure_budget_keeps_room_for_a_jump_and_execution_costs() {
        for price in [100.0, 143.11, 200.0] {
            for dd in [0.0, 50.0, 110.0, 180.0, 225.0, 250.0] {
                let cap = position_limit(price, dd, 20.0, 250.0);
                assert!(cap >= 0.0 && cap <= 19.0);
                if dd <= 225.0 {
                    assert!(dd + cap * price * 0.045 + 25.0 <= 250.000001);
                }
            }
        }
    }
    #[test]
    fn quotes_and_probes_fit_even_after_partial_execution() {
        for old in -19..=19 {
            for current in -19..=19 {
                for signal in [-1.0, 0.0, 1.0] {
                    let q = current as f64;
                    let (bid, ask) = quote_sizes(q, old as f64, 19.0, signal);
                    let bid_probe = if 19.0 - q.max(old as f64) >= 0.001 {
                        0.001
                    } else {
                        0.0
                    };
                    let ask_probe = if 19.0 + q.min(old as f64) >= 0.001 {
                        0.001
                    } else {
                        0.0
                    };
                    assert!(q + bid + bid_probe <= 19.0 + 1e-9);
                    assert!(q - ask - ask_probe >= -19.0 - 1e-9);
                }
            }
        }
    }
}
