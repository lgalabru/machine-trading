//! Oracle-anchored SPCX market making with public order-flow and drift signals.
//! The execution path stays within an internal share cap and an oracle-valued risk budget.

use mtx_kit::prelude::*;
use mtx_kit::{Pubkey, phoenix_rise::accounts::owned::Trader as AccountTrader};
use std::collections::{HashSet, VecDeque};
use tracing::info;

#[derive(Default)]
struct Maker {
    book: Option<Pubkey>,
    tick: f64,
    lot_scale: f64,
    asset_id: u64,
    last_price: f64,
    moves: u64,
    sum_return: f64,
    sum_square: f64,
    flow: VecDeque<(u64, f64)>,
    peak_equity: f64,
    stopped: bool,
    jump_slot: u64,
    first_slot: u64,
    observed: f64,
    seen: HashSet<String>,
}
impl Strategy for Maker {
    async fn start(&mut self, s: &Session) -> BotResult {
        if s.env.symbol != "SPCX" {
            return Err("Only SPCX is supported".into());
        }
        let cfg = s.trader.phoenix.market(&s.env.symbol)?;
        self.book = Some(cfg.market_pubkey.parse()?);
        self.tick = s.trader.phoenix.ticks_to_usd(cfg, 1);
        self.lot_scale = 10f64.powi(cfg.base_lots_decimals as i32);
        self.asset_id = cfg.asset_id as u64;
        Ok(())
    }
    async fn step(&mut self, m: &Market, orders: &mut Orders<'_>) -> BotResult {
        let t = orders.trader().unwrap();
        // Cancel before reading competitors. A failed cancellation prevents new orders.
        t.cancel_all()?;
        let p = m.oracle;
        let pos = m.position.size;
        if !p.is_finite() || p <= 0.0 {
            return Ok(());
        }
        let raw = t.rpc.get_account(&t.account)?;
        let a = AccountTrader::try_from_account_bytes(&raw.data)?;
        let mut equity = a.state.quote_lot_collateral.as_inner() as f64 / 1e6;
        for (id, v) in &a.positions.entries {
            if *id == self.asset_id {
                equity += v.virtual_quote_lot_position.as_inner() as f64 / 1e6
                    + v.base_lot_position.as_inner() as f64 / self.lot_scale * p;
            }
        }
        if self.peak_equity == 0.0 {
            self.peak_equity = equity;
        }
        self.peak_equity = self.peak_equity.max(equity);
        let dd = self.peak_equity - equity;
        if dd > m.max_drawdown * 0.55 {
            self.stopped = true;
        }
        if self.stopped {
            if pos.abs() > 0.01 {
                orders.target_position_with(
                    0.0,
                    Execution {
                        min_trade: 0.001,
                        slice: 5.0,
                        slippage_bps: 100.0,
                        ..Execution::default()
                    },
                );
            }
            return Ok(());
        }
        let safe_cap = risk_capacity(p, dd, m.max_drawdown, m.cap);
        // Shrink existing exposure as well as new orders when the risk budget contracts.
        if pos.abs() > safe_cap + 0.001 {
            orders.target_position_with(
                pos.signum() * safe_cap,
                Execution {
                    min_trade: 0.001,
                    slice: 5.0,
                    slippage_bps: 100.0,
                    ..Execution::default()
                },
            );
            return Ok(());
        }
        if self.last_price > 0.0 && p != self.last_price {
            let r = (p / self.last_price - 1.0) * 10000.0;
            if r.abs() > 60.0 {
                self.flow.clear();
                self.jump_slot = m.slot;
            } else {
                self.moves += 1;
                self.sum_return += r;
                self.sum_square += r * r;
            }
        }
        self.last_price = p;
        if self.first_slot == 0 {
            self.first_slot = m.slot;
        }
        // Observe submissions rather than fills, so withdrawing a quote does not hide flow.
        let history: serde_json::Value=t.rpc.send(solana_rpc_client_api::request::RpcRequest::GetSignaturesForAddress,serde_json::json!([self.book.unwrap().to_string(),{"limit":128,"commitment":"confirmed"}]))?;
        let mut fresh = Vec::new();
        let authority = t.authority().to_string();
        let book_address = self.book.unwrap().to_string();
        let program = mtx_kit::phoenix_rise::ix::PHOENIX_PROGRAM_ID.to_string();
        if let Some(history) = history.as_array() {
            for entry in history.iter().rev() {
                let Some(signature) = entry["signature"].as_str() else {
                    continue;
                };
                if self.seen.contains(signature) {
                    continue;
                }
                let slot = entry["slot"].as_u64().unwrap_or(0);
                if slot < self.first_slot {
                    self.seen.insert(signature.to_owned());
                    continue;
                }
                let tx: serde_json::Value=t.rpc.send(solana_rpc_client_api::request::RpcRequest::GetTransaction,serde_json::json!([signature,{"encoding":"json","maxSupportedTransactionVersion":0,"commitment":"confirmed"}]))?;
                if tx.is_null() {
                    continue;
                }
                self.seen.insert(signature.to_owned());
                if !tx["meta"]["err"].is_null() {
                    continue;
                }
                let message = &tx["transaction"]["message"];
                let Some(keys) = message["accountKeys"].as_array() else {
                    continue;
                };
                if keys.first().and_then(|x| x.as_str()) == Some(authority.as_str()) {
                    continue;
                }
                let Some(instructions) = message["instructions"].as_array() else {
                    continue;
                };
                for ix in instructions {
                    let Some(pi) = ix["programIdIndex"].as_u64() else {
                        continue;
                    };
                    if keys.get(pi as usize).and_then(|v| v.as_str()) != Some(program.as_str()) {
                        continue;
                    }
                    let in_market = ix["accounts"].as_array().is_some_and(|a| {
                        a.iter().any(|i| {
                            i.as_u64()
                                .and_then(|i| keys.get(i as usize))
                                .and_then(|v| v.as_str())
                                == Some(book_address.as_str())
                        })
                    });
                    if !in_market {
                        continue;
                    }
                    let Some(encoded) = ix["data"].as_str() else {
                        continue;
                    };
                    let Ok(data) = bs58::decode(encoded).into_vec() else {
                        continue;
                    };
                    if let Some(side) = market_order_side(&data) {
                        fresh.push(side);
                    }
                }
            }
        }
        for side in fresh {
            self.flow.push_back((m.slot, side));
            self.observed += 1.0;
        }
        if self.jump_slot > 0 && m.slot.saturating_sub(self.jump_slot) <= 28 {
            self.flow.clear();
        }
        while self
            .flow
            .front()
            .is_some_and(|(slot, _)| m.slot.saturating_sub(*slot) > 40)
        {
            self.flow.pop_front();
        }
        let elapsed = m.slot.saturating_sub(self.first_slot) as f64;
        // One-sided Poisson arrival likelihoods, regularized by a noise-flow prior.
        let lambda = ((self.observed + 60.0) / (elapsed + 200.0) / 2.0).clamp(0.08, 0.25);
        let logratio = (1.0 + 0.35 / lambda).ln();
        let mut buy_score: f64 = 0.0;
        let mut sell_score: f64 = 0.0;
        for window in [4, 6, 10, 16, 24] {
            let mut nb = 0.0;
            let mut ns = 0.0;
            for (slot, q) in &self.flow {
                if m.slot.saturating_sub(*slot) <= window {
                    if *q > 0.0 {
                        nb += 1.0;
                    } else {
                        ns += 1.0;
                    }
                }
            }
            buy_score = buy_score.max(nb * logratio - 0.35 * window as f64);
            sell_score = sell_score.max(ns * logratio - 0.35 * window as f64);
        }
        let burst = buy_score.max(sell_score);
        let direction = if buy_score > sell_score { 1.0 } else { -1.0 };
        let flow_target = if elapsed < 28.0 || m.slots_left <= 60 {
            0.0
        } else {
            direction * ((burst - 1.0).max(0.0) * 20.0).min(18.0)
        };
        // Bayesian mixture: a stationary walk, upward drift, or downward drift.
        let mut trend_target = 0.0;
        if self.moves > 12 {
            let n = self.moves as f64;
            let variance = ((self.sum_square - self.sum_return * self.sum_return / n + 100.0)
                / (n + 4.0))
                .max(1.0);
            let mut denom = 0.75;
            let mut numer = 0.0;
            for mu in [0.6, 1.2, 1.8] {
                let up = ((mu * self.sum_return - 0.5 * n * mu * mu) / variance)
                    .clamp(-40.0, 40.0)
                    .exp()
                    * 0.125
                    / 3.0;
                let down = ((-mu * self.sum_return - 0.5 * n * mu * mu) / variance)
                    .clamp(-40.0, 40.0)
                    .exp()
                    * 0.125
                    / 3.0;
                denom += up + down;
                numer += mu * (up - down);
            }
            trend_target = (numer / denom * 20.0).clamp(-14.0, 14.0);
        }
        let net = buy_score - sell_score;
        let gross = buy_score + sell_score;
        let mut target = (trend_target + flow_target).clamp(-18.0, 18.0);
        if m.slots_left < 20 {
            target *= m.slots_left as f64 / 20.0;
        }
        target = target.clamp(-safe_cap, safe_cap);
        let mut projected = pos;
        if elapsed >= 28.0 && m.slots_left > 60 && burst > 4.0 && (target - pos).abs() > 4.0 {
            let amount = (target - pos).clamp(-10.0, 10.0);
            if amount > 0.0 {
                t.place_market(Side::Bid, amount, p * 1.004).await?;
            } else {
                t.place_market(Side::Ask, -amount, p * 0.996).await?;
            }
            projected = t.position().await?.size;
        }
        // Re-read the competing book after any aggressive execution.
        let bbo = t.inner.bbo().await?;
        let cfg = t.phoenix.market(&t.env.symbol)?;
        let best_bid = bbo
            .best_bid_ticks()
            .map(|x| t.phoenix.ticks_to_usd(cfg, x))
            .unwrap_or(p * 0.997);
        let best_ask = bbo
            .best_ask_ticks()
            .map(|x| t.phoenix.ticks_to_usd(cfg, x))
            .unwrap_or(p * 1.003);
        let skew = (target - projected) * 0.4;
        let floor = 3.0;
        let bid = ((best_bid + self.tick) + p * skew / 10000.0)
            .min(p * (1.0 - (floor - skew).max(0.7) / 10000.0));
        let ask = ((best_ask - self.tick) + p * skew / 10000.0)
            .max(p * (1.0 + (floor + skew).max(0.7) / 10000.0));
        let (mut bid_size, mut ask_size) = quote_sizes(projected, target, safe_cap);

        if burst > 2.5 && flow_target != 0.0 {
            if direction > 0.0 {
                ask_size = 0.0;
            } else {
                bid_size = 0.0;
            }
        }
        if bid_size >= 0.01 {
            orders.bid(bid, bid_size);
        }
        if ask_size >= 0.01 {
            orders.ask(ask, ask_size);
        }
        if m.k % 20 == 0 || burst > 1.0 {
            info!(
                k = m.k,
                p,
                pos,
                equity,
                dd,
                net,
                gross,
                burst,
                target,
                observed = self.observed,
                bid,
                ask,
                "quote"
            );
        }
        Ok(())
    }
}
/// Share capacity that reserves cash for execution and a 4.5% adverse oracle move.
fn risk_capacity(price: f64, drawdown: f64, limit: f64, mandate_cap: f64) -> f64 {
    ((limit - drawdown - 25.0) / (price * 0.045))
        .clamp(0.0, 18.0)
        .min((mandate_cap - 0.01).max(0.0))
}

fn quote_sizes(position: f64, target: f64, capacity: f64) -> (f64, f64) {
    let cap = 4.0_f64.max(target.abs() + 2.0).min(capacity);
    (
        2.0_f64.min((cap - position).max(0.0)),
        2.0_f64.min((cap + position).max(0.0)),
    )
}

/// Decode the SDK's Borsh immediate-or-cancel packet; one instruction is one observation.
fn market_order_side(data: &[u8]) -> Option<f64> {
    let discriminator =
        mtx_kit::phoenix_rise::ix::PhoenixInstruction::PlaceMarketOrder.discriminant();
    if data.len() < 11 || data[..8] != discriminator || data[8] != 2 {
        return None;
    }
    match data[9] {
        0 => Some(1.0),
        1 => Some(-1.0),
        _ => None,
    }
}

fn main() -> BotResult {
    mtx_kit::run(Maker::default())
}

#[cfg(test)]
mod tests {
    use super::*;
    use mtx_kit::phoenix_rise::ix::{
        order_packet::OrderPacket,
        types::{OrderFlags, SelfTradeBehavior},
    };
    #[test]
    fn sdk_market_packets_count_orders_independent_of_size_and_price_option() {
        for lots in [1, 10, 600, 4000, 20000] {
            for price in [None, Some(14_300)] {
                for side in [Side::Bid, Side::Ask] {
                    let packet = OrderPacket::immediate_or_cancel(
                        side,
                        price,
                        lots,
                        None,
                        0,
                        0,
                        SelfTradeBehavior::Abort,
                        None,
                        [0; 16],
                        None,
                        OrderFlags::None,
                        false,
                    );
                    let mut data = mtx_kit::phoenix_rise::ix::PhoenixInstruction::PlaceMarketOrder
                        .discriminant()
                        .to_vec();
                    data.extend(borsh::to_vec(&packet).unwrap());
                    assert_eq!(
                        market_order_side(&data),
                        Some(if side == Side::Bid { 1.0 } else { -1.0 })
                    );
                    data[0] ^= 1;
                    assert_eq!(market_order_side(&data), None);
                }
            }
        }
        assert_eq!(market_order_side(&[]), None);
    }
    #[test]
    fn quotes_and_all_fill_subsets_respect_capacity() {
        for cap in [0.5, 2.0, 4.0, 12.0, 18.0] {
            for pos in [-cap, -cap / 2.0, 0.0, cap / 2.0, cap] {
                for target in [-18.0, -4.0, 0.0, 4.0, 18.0] {
                    let (bid, ask) = quote_sizes(pos, target, cap);
                    for bf in [0.0, bid] {
                        for af in [0.0, ask] {
                            assert!((pos + bf - af).abs() <= cap + 1e-9);
                        }
                    }
                }
            }
        }
    }
    #[test]
    fn risk_capacity_preserves_reserve_through_adverse_shock() {
        for p in [50.0, 143.0, 170.0, 300.0] {
            for dd in [0.0, 60.0, 100.0, 130.0, 137.0, 200.0] {
                let cap = risk_capacity(p, dd, 250.0, 20.0);
                assert!(cap <= 18.0);
                assert!(dd + cap * p * 0.045 <= 225.0 + 1e-9);
            }
        }
    }
}
