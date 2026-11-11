//! Oracle-centered SPCX market making with public order-flow inference.
//!
//! Completed blocks provide the full order stream. Changes in known taker
//! accounts provide an earlier, partial observation, discounted for uncertainty.
//! Orders and inventory are bounded independently of Phoenix's movable mark.

use mtx_kit::prelude::*;
use mtx_kit::{Trader, phoenix_rise};
use phoenix_rise::accounts::trader::Trader as AccountTrader;
use std::collections::VecDeque;
use std::str::FromStr;
use mtx_kit::Pubkey;

struct TakerState { key: Pubkey, last: Option<f64> }

// Leave enough equity for an unannounced jump and the cost of reducing risk.
fn inventory_cap(cap: f64, limit: f64, drawdown: f64, oracle: f64) -> f64 {
    ((limit - drawdown - 35.0).max(0.0) / (oracle * 0.047))
        .min(19.8)
        .min(cap)
}

#[derive(Default)]
struct Maker {
    last_oracle: f64,
    start_slot: u64,
    jumps: u8,
    last_jump_slot: u64,
    eligible_slot: u64,
    takers: Vec<TakerState>,
    flow_reset_slot: u64,
    tick: f64,
    lot_scale: f64,
    market_key: String,
    authority: String,
    flow: VecDeque<(u64, f64)>,
    returns: VecDeque<f64>,
    alarm_dir: f64,
    alarm_until: u64,
    peak: f64,
    halted: bool,
}

impl Maker {
    fn observe_jump(&mut self, slot: u64) {
        // The oracle can report one large scenario move in two successive
        // updates. True scenario jumps are separated by at least 60 slots.
        if slot.saturating_sub(self.last_jump_slot) > 10 {
            self.jumps += 1;
        }
        self.last_jump_slot = slot;
        self.alarm_until = 0;
        self.eligible_slot = slot + 30;
        self.flow.clear();
        self.flow_reset_slot = slot;
    }

    fn equity(&self, trader: &Trader, oracle: f64) -> Option<f64> {
        let account = trader.rpc.get_account(&trader.account).ok()?;
        let view = AccountTrader::try_from_account_bytes(&account.data).ok()?;
        let mut value = view.header.trader_state.quote_lot_collateral.as_inner() as f64 / 1e6;
        for (id, position) in view.positions() {
            if id as u32 == trader.inner.market().asset_id {
                value += position.base_lot_position().as_inner() as f64 / self.lot_scale * oracle;
                value += position.virtual_quote_lot_position().as_inner() as f64 / 1e6;
            }
        }
        Some(value)
    }

    fn read_flow(&mut self, trader: &Trader, slot: u64) {
        // A block is complete only after the next two-slot turn begins.
        let disc = phoenix_rise::ix::PhoenixInstruction::PlaceMarketOrder.discriminant();
        for s in slot.saturating_sub(2)..=slot.saturating_sub(2) {
            if s <= self.flow_reset_slot { continue; }
            let block: serde_json::Value = trader.rpc.send(solana_rpc_client_api::request::RpcRequest::GetBlock, serde_json::json!([s, {"encoding":"json","commitment":"confirmed","maxSupportedTransactionVersion":0,"rewards":false}])).unwrap_or_default();
            let Some(txs) = block["transactions"].as_array() else { continue };
            for tx in txs {
                let msg = &tx["transaction"]["message"];
                let Some(keys) = msg["accountKeys"].as_array() else { continue };
                if keys.iter().any(|v| v.as_str() == Some(self.authority.as_str())) { continue }
                if !keys.iter().any(|v| v.as_str() == Some(self.market_key.as_str())) { continue }
                let Some(ixs) = msg["instructions"].as_array() else { continue };
                for ix in ixs {
                    let Some(encoded) = ix["data"].as_str() else { continue };
                    let Ok(data) = bs58::decode(encoded).into_vec() else { continue };
                    if data.len() > 11 && data[..8] == disc && data[8] == 2 {
                        let direction = if data[9] == 0 { 1.0 } else { -1.0 };
                        self.flow.push_back((s, direction));
                        if let Some(idx) = ix["accounts"][4].as_u64() {
                            if let Some(key) = keys.get(idx as usize).and_then(|v| v.as_str()).and_then(|v| Pubkey::from_str(v).ok()) {
                                if !self.takers.iter().any(|t| t.key == key) { self.takers.push(TakerState{key,last:None}); }
                            }
                        }
                    }
                }
            }
        }
        while self.flow.front().is_some_and(|(s,_)| slot.saturating_sub(*s) > 80) { self.flow.pop_front(); }
    }

    fn live_flow(&mut self, trader: &Trader) -> Vec<f64> {
        let keys: Vec<_> = self.takers.iter().map(|t| t.key).collect();
        if keys.is_empty() { return Vec::new(); }
        let Ok(accounts) = trader.rpc.get_multiple_accounts(&keys) else { return Vec::new() };
        let mut live = Vec::new();
        for (state, account) in self.takers.iter_mut().zip(accounts) {
            let Some(account) = account else { continue };
            let Ok(view) = AccountTrader::try_from_account_bytes(&account.data) else { continue };
            let q = view.positions().find(|(id,_)| *id as u32 == trader.inner.market().asset_id).map(|(_,p)| p.base_lot_position().as_inner() as f64 / self.lot_scale).unwrap_or(0.0);
            if let Some(last) = state.last {
                if (q-last).abs() > 0.01 { live.push((q-last).signum()); }
            }
            state.last = Some(q);
        }
        live
    }

    fn signal(&self, slot: u64) -> (f64, f64) {
        let mut best = 0.0;
        let mut dir = 0.0;
        for window in [6_u64, 10, 16, 24, 32] {
            let mut buy: f64 = 0.0;
            let mut sell: f64 = 0.0;
            for (s, d) in &self.flow {
                if slot.saturating_sub(*s) < window {
                    if *d > 0.0 { buy += 1.0; } else { sell += 1.0; }
                }
            }
            for rate in [0.2_f64, 0.4, 0.7] {
                for (n, opp, sign) in [(buy,sell,1.0), (sell,buy,-1.0)] {
                    let score = n * ((0.15 + rate)/0.15).ln() - rate * window as f64;
                    if n - opp >= 3.0 && score > best { best = score; dir = sign; }
                }
            }
        }
        (dir, best)
    }
}

impl Strategy for Maker {
    async fn start(&mut self, session: &Session) -> BotResult {
        self.start_slot = session.hello.start_slot;
        self.eligible_slot = self.start_slot + 30;
        let config = session.trader.phoenix.market(&session.env.symbol)?;
        self.tick = session.trader.phoenix.ticks_to_usd(config, 1);
        self.lot_scale = 10_f64.powi(config.base_lots_decimals as i32);
        self.market_key = config.market_pubkey.to_string();
        self.authority = session.trader.authority().to_string();
        Ok(())
    }

    async fn step(&mut self, market: &Market, orders: &mut Orders<'_>) -> BotResult {
        let Some(trader) = orders.trader() else { return Ok(()) };
        let oracle = market.oracle;
        let mut position = market.position.size;
        if !(oracle > 0.0 && oracle.is_finite()) { orders.cancel_all(); return Ok(()) }
        let change = if self.last_oracle > 0.0 { oracle / self.last_oracle - 1.0 } else { 0.0 };
        if change.abs() > 0.005 {
            self.observe_jump(market.slot);
        } else if change != 0.0 {
            self.returns.push_back(change);
            if self.returns.len() > 80 { self.returns.pop_front(); }
        }
        self.last_oracle = oracle;
        self.read_flow(trader, market.slot);
        let (past_dir, past_score) = self.signal(market.slot.saturating_sub(2));
        let live = self.live_flow(trader);
        let count = if change.abs() < 0.005 { live.len() } else { 0 };
        for d in live.into_iter().take(count) { self.flow.push_back((market.slot,d)); }
        let (live_dir, live_score) = self.signal(market.slot);
        let (dir,score) = if live_score - 1.5 > past_score { (live_dir,live_score-1.5) } else { (past_dir,past_score) };
        for _ in 0..count { self.flow.pop_back(); }
        // The announced scenario family excludes jumps near the ends and
        // spaces jumps by at least sixty slots.
        if score > 4.5 && change.abs() < 0.005 && market.slot >= self.eligible_slot && market.slots_left >= 60 && self.jumps < 3 {
            self.alarm_dir = dir;
            self.alarm_until = market.slot + 24;
        }
        let alarm = if market.slot < self.alarm_until && market.slots_left >= 60 && self.jumps < 3 { self.alarm_dir } else { 0.0 };
        let drift = if self.returns.len() >= 12 && self.jumps == 0 {
            let n = self.returns.len() as f64;
            let sum: f64 = self.returns.iter().sum();
            let variance: f64 = self.returns.iter().map(|r| r*r).sum::<f64>() / n;
            let z = sum / (n * variance + 1e-9).sqrt();
            (z * 2.5).clamp(-7.0, 7.0)
        } else { 0.0 };
        let mut target = if alarm != 0.0 { alarm * 19.8 } else { drift };
        let equity = self.equity(trader, oracle).unwrap_or(10000.0 + market.pnl);
        self.peak = self.peak.max(equity);
        let drawdown = self.peak - equity;
        if drawdown > market.max_drawdown * 0.62 { self.halted = true; }
        if self.halted { target = 0.0; }
        let risk_cap = inventory_cap(market.cap, market.max_drawdown, drawdown, oracle);
        target = target.clamp(-risk_cap, risk_cap);

        // Remove old inventory risk before placing the next pair of quotes.
        trader.cancel_all()?;
        if market.last { return Ok(()) }
        let bbo = trader.inner.bbo().await?;
        let config = trader.phoenix.market(&trader.env.symbol)?;
        let best_bid = trader.phoenix.ticks_to_usd(config, bbo.best_bid_ticks);
        let best_ask = trader.phoenix.ticks_to_usd(config, bbo.best_ask_ticks);
        if self.halted || position.abs() > risk_cap + 0.1 || (alarm != 0.0 && (target-position).abs() > 3.0) {
            for _ in 0..4 {
                let difference = target - position;
                if difference.abs() < 0.05 { break; }
                let qty = difference.abs().min(10.0);
                let side = if difference > 0.0 { Side::Bid } else { Side::Ask };
                let limit = if side == Side::Bid { oracle*1.004 } else { oracle*0.996 };
                let before = position;
                if trader.place_market(side, qty, limit).await.is_ok() { position = trader.position().await?.size; }
                if (position-before).abs() < 0.001 { break; }
            }
        }
        if self.halted || market.last { return Ok(()) }
        let skew = (target-position) * 0.00020 * oracle;
        let base_bid = (best_bid + self.tick).min(oracle * 0.9998);
        let base_ask = (best_ask - self.tick).max(oracle * 1.0002);
        let mut bid = (base_bid + skew).max(oracle*0.98).min(oracle-self.tick);
        let mut ask = (base_ask + skew).min(oracle*1.02).max(oracle+self.tick);
        if alarm > 0.0 { ask = oracle * 1.02; }
        if alarm < 0.0 { bid = oracle * 0.98; }
        let size: f64 = 2.5;
        let bid_size = size.min((risk_cap-position).max(0.0));
        let ask_size = size.min((risk_cap+position).max(0.0));
        if bid_size >= 0.01 { orders.bid(bid, bid_size); }
        if ask_size >= 0.01 { orders.ask(ask, ask_size); }
        if market.k % 20 == 0 || alarm != 0.0 {
            eprintln!("STAT k={} oracle={:.4} q={:.3} eq={:.3} dd={:.3} target={:.2} flow={:.2} alarm={} b={:.4} a={:.4}", market.k,oracle,position,equity,drawdown,target,score,alarm,bid,ask);
        }
        Ok(())
    }
}

fn main() -> BotResult { mtx_kit::run(Maker::default()) }


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn successive_oracle_updates_count_as_one_jump() {
        let mut maker = Maker::default();
        maker.observe_jump(1_000);
        maker.observe_jump(1_002);
        assert_eq!(maker.jumps, 1);
        maker.observe_jump(1_060);
        maker.observe_jump(1_062);
        assert_eq!(maker.jumps, 2);
        maker.observe_jump(1_120);
        assert_eq!(maker.jumps, 3);
    }

    #[test]
    fn balanced_noise_does_not_look_like_a_directional_burst() {
        let mut maker = Maker::default();
        for i in 0..12 { maker.flow.push_back((100+i, if i%2==0 {1.0} else {-1.0})); }
        assert_eq!(maker.signal(112), (0.0,0.0));
    }

    #[test]
    fn a_short_burst_is_detected_and_then_expires() {
        let mut maker = Maker::default();
        for i in 0..6 { maker.flow.push_back((100+i, 1.0)); }
        let (direction,evidence) = maker.signal(105);
        assert_eq!(direction,1.0);
        assert!(evidence > 6.0);
        assert_eq!(maker.signal(140),(0.0,0.0));
        for (_,d) in &mut maker.flow { *d = -1.0; }
        assert_eq!(maker.signal(105).0,-1.0);
    }

    #[test]
    fn early_partial_observations_need_stronger_evidence() {
        let mut maker = Maker::default();
        for i in 0..5 { maker.flow.push_back((100+i, 1.0)); }
        assert!(maker.signal(104).1 - 1.5 < 4.5);
        maker.flow.push_back((105,1.0));
        assert!(maker.signal(105).1 - 1.5 > 4.5);
    }

    #[test]
    fn jump_and_exit_cost_fit_inside_the_drawdown_reserve() {
        for oracle in [100.0,143.11,200.0] {
            for dd in 0..250 {
                let dd = dd as f64;
                let cap = inventory_cap(20.0,250.0,dd,oracle);
                assert!((0.0..20.0).contains(&cap));
                // A four percent adverse jump, followed by a 40 bp exit.
                assert!(dd + cap * oracle * 0.044 < 250.0);
            }
        }
        assert_eq!(inventory_cap(3.0,250.0,0.0,143.11),3.0);
    }
}
