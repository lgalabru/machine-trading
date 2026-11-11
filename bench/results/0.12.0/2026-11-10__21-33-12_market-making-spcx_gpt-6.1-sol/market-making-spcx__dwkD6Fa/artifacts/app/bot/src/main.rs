//! Oracle anchored market maker with a public order-flow burst detector.
use mtx_kit::prelude::*;
use mtx_kit::{Trader, phoenix_rise::ix::PhoenixInstruction};
use serde_json::{Value, json};
use solana_rpc_client_api::request::RpcRequest;
use std::collections::{HashSet, VecDeque};
use tracing::{info, warn};

/// Decode the SDK's Borsh IOC packet, including its optional price field.
fn taker_packet(data: &[u8], lot: f64) -> Option<(f64, f64)> {
    let disc = PhoenixInstruction::PlaceMarketOrder.discriminant();
    if data.len() < 20 || data[..8] != disc || data[8] != 2 || data[9] > 1 {
        return None;
    }
    let offset = 11 + if data[10] == 1 { 8 } else { 0 };
    let bytes = data.get(offset..offset + 8)?;
    let size = u64::from_le_bytes(bytes.try_into().ok()?) as f64 * lot;
    Some((if data[9] == 0 { 1.0 } else { -1.0 }, size))
}

/// Reserve a cushion for a 5% move before the next opportunity to act.
fn exposure_limit(cap: f64, price: f64, drawdown: f64) -> f64 {
    ((220.0 - drawdown).max(0.0) / (price * 0.05))
        .min(cap - 0.01)
        .min(19.8)
}

#[derive(Default)]
struct Maker {
    last_price: f64,
    last_pos: f64,
    drift: f64,
    fill_flow: f64,
    start_slot: u64,
    tick: f64,
    lot: f64,
    book_key: String,
    authority: String,
    seen: HashSet<String>,
    tape: VecDeque<(u64, f64, f64)>,
    head: Option<String>,
    peak: f64,
    stopped: bool,
    event_side: f64,
    event_until: u64,
    last_jump: u64,
}
impl Maker {
    fn read_flow(&mut self, t: &Trader, slot: u64) -> BotResult {
        let mut cfg = json!({"limit":100,"commitment":"confirmed"});
        if let Some(h) = &self.head {
            cfg["until"] = json!(h);
        }
        let list: Value = t.rpc.send(
            RpcRequest::GetSignaturesForAddress,
            json!([self.book_key, cfg]),
        )?;
        let Some(sigs) = list.as_array() else {
            return Ok(());
        };
        for item in sigs.iter().rev() {
            let Some(sig) = item["signature"].as_str() else {
                continue;
            };
            let s = item["slot"].as_u64().unwrap_or(0);
            if s < self.start_slot || self.seen.contains(sig) {
                continue;
            }
            let tx:Value=t.rpc.send(RpcRequest::GetTransaction,json!([sig,{"encoding":"json","commitment":"confirmed","maxSupportedTransactionVersion":0}]))?;
            if tx.is_null() {
                continue;
            }
            self.seen.insert(sig.to_owned());
            let msg = &tx["transaction"]["message"];
            let Some(keys) = msg["accountKeys"].as_array() else {
                continue;
            };
            if keys.first().and_then(Value::as_str) == Some(self.authority.as_str()) {
                continue;
            }
            if !keys
                .iter()
                .any(|k| k.as_str() == Some(self.book_key.as_str()))
            {
                continue;
            }
            let Some(ixs) = msg["instructions"].as_array() else {
                continue;
            };
            for ix in ixs {
                let Some(enc) = ix["data"].as_str() else {
                    continue;
                };
                let Ok(data) = bs58::decode(enc).into_vec() else {
                    continue;
                };
                let Some((side, size)) = taker_packet(&data, self.lot) else {
                    continue;
                };
                self.tape.push_back((s, side, size));
            }
        }
        if let Some(h) = sigs.first().and_then(|s| s["signature"].as_str()) {
            self.head = Some(h.to_owned());
        }
        self.tape.retain(|(s, _, _)| slot.saturating_sub(*s) <= 160);
        Ok(())
    }
    fn book(&self, t: &Trader) -> BotResult<(f64, f64)> {
        use mtx_kit::phoenix_rise::accounts::orderbook::Orderbook;
        let a = t.rpc.get_account(&self.book_key.parse()?)?;
        let b = Orderbook::try_from_account_bytes(&a.data)?;
        Ok((
            b.best_bid()
                .map(|e| e.price_in_ticks().as_inner() as f64 * self.tick)
                .unwrap_or(0.0),
            b.best_ask()
                .map(|e| e.price_in_ticks().as_inner() as f64 * self.tick)
                .unwrap_or(f64::INFINITY),
        ))
    }
    async fn hedge(
        &self,
        t: &Trader,
        mut pos: f64,
        target: f64,
        p: f64,
        fast: bool,
    ) -> BotResult<f64> {
        for _ in 0..if fast { 8 } else { 1 } {
            let delta = target - pos;
            let qty = (delta.abs().min(5.0) / self.lot + 1e-8).floor() * self.lot;
            if qty < 0.02 {
                break;
            }
            let side = if delta > 0.0 { Side::Bid } else { Side::Ask };
            let limit = if delta > 0.0 { p * 1.006 } else { p * 0.994 };
            if let Err(e) = t.place_market(side, qty, limit).await {
                warn!(%e,"hedge failed");
                break;
            }
            let next = t.position().await?.size;
            if (next - pos).abs() < self.lot * 0.5 {
                break;
            }
            pos = next;
        }
        Ok(t.position().await?.size)
    }
}
impl Strategy for Maker {
    async fn start(&mut self, s: &Session) -> BotResult {
        let m = s.trader.phoenix.market(&s.env.symbol)?;
        self.tick = m.tick_size as f64 * 10f64.powi(m.base_lots_decimals as i32 - 6);
        self.lot = 10f64.powi(-(m.base_lots_decimals as i32));
        self.book_key = m.market_pubkey.to_string();
        self.authority = s.trader.authority().to_string();
        self.start_slot = s.hello.start_slot;
        Ok(())
    }
    async fn step(&mut self, m: &Market, o: &mut Orders<'_>) -> BotResult {
        let p = m.oracle;
        let mut pos = m.position.size;
        if !p.is_finite() || p <= 0.0 {
            o.cancel_all();
            return Ok(());
        }
        let t = o.trader().unwrap();
        if let Err(e) = self.read_flow(t, m.slot) {
            warn!(%e,"flow read failed");
        }
        t.cancel_all()?;
        if m.last {
            return Ok(());
        }
        let (bb, ba) = self.book(t).unwrap_or((p * 0.998, p * 1.002));
        let margin = t
            .phoenix
            .hawkeye(t.authority())
            .view_margin(t.authority(), 0, 0)
            .await;
        let mut dd = m.drawdown;
        if let Ok(view) = margin {
            let a = view.value;
            let equity = (a.collateral_quote_lots + a.unrealized_pnl_quote_lots
                - a.unsettled_funding_quote_lots) as f64
                / 1e6
                + pos * (p - m.mark);
            self.peak = self.peak.max(equity);
            dd = (self.peak - equity).max(0.0);
            if m.k % 50 == 0 {
                info!(equity, dd, "equity");
            }
        }
        if dd > 130.0 {
            self.stopped = true;
        }
        let risk_cap = exposure_limit(m.cap, p, dd);
        let ret = if self.last_price > 0.0 {
            (p / self.last_price - 1.0) * 10000.0
        } else {
            0.0
        };
        if ret.abs() > 50.0 {
            self.last_jump = m.slot;
            self.event_side = 0.0;
            self.event_until = 0;
            self.tape.clear();
        } else {
            self.drift = self.drift * 0.98 + ret * 0.02;
        }
        self.fill_flow = self.fill_flow * 0.88 - (pos - self.last_pos);
        let mut count = 0.0;
        let mut n = 0.0;
        let mut volume = 0.0;
        let mut fast = 0.0;
        for &(slot, side, size) in &self.tape {
            let age = m.slot.saturating_sub(slot);
            if age <= 10 {
                count += side;
                n += 1.0;
                volume += side * size;
            }
            if age <= 8 {
                fast += side;
            }
        }
        if ((count.abs() >= 4.0 && n >= 8.0) || fast.abs() >= 6.0)
            && m.slot.saturating_sub(self.last_jump) > 22
            && m.slots_left > 60
            && m.slot.saturating_sub(self.start_slot) >= 30
        {
            self.event_side = if fast.abs() >= 6.0 {
                fast.signum()
            } else {
                count.signum()
            };
            self.event_until = m.slot + 32;
        }
        if m.slot > self.event_until || m.slots_left <= 60 {
            self.event_side = 0.0;
        }
        let target = if self.event_side != 0.0 {
            self.event_side * 19.0f64.min(risk_cap)
        } else {
            (self.drift * 10.0 + self.fill_flow * 0.5).clamp(-3.0, 3.0)
        };
        if self.stopped {
            self.hedge(t, pos, 0.0, p, true).await?;
            return Ok(());
        }
        // A detected burst is worth crossing for; after it ends, shed the tactical inventory.
        if (self.event_side != 0.0 && pos * self.event_side < target.abs() - 0.6)
            || (self.event_side == 0.0 && pos.abs() > 4.0)
            || pos.abs() > risk_cap
        {
            pos = self
                .hedge(
                    t,
                    pos,
                    target,
                    p,
                    self.event_side != 0.0 || pos.abs() > risk_cap,
                )
                .await?;
        }
        let skew = (pos - target) * 2.0;
        let bdist = ((p - bb - self.tick) / p * 10000.0).clamp(2.0, 9.0);
        let adist = ((ba - p - self.tick) / p * 10000.0).clamp(2.0, 9.0);
        let mut bid = p * (1.0 - (bdist + skew).max(0.8) / 10000.0);
        let mut ask = p * (1.0 + (adist - skew).max(0.8) / 10000.0);
        if self.event_side > 0.0 {
            bid = p * 0.9998;
            ask = p * 1.0035;
        }
        if self.event_side < 0.0 {
            bid = p * 0.9965;
            ask = p * 1.0002;
        }
        let cap = if self.event_side == 0.0 {
            3.5f64.min(risk_cap)
        } else {
            risk_cap
        };
        let bs = (cap - pos).clamp(0.0, 1.5).min(m.room_to_buy());
        let az = (cap + pos).clamp(0.0, 1.5).min(m.room_to_sell());
        if bs >= self.lot {
            o.bid(bid, bs);
        }
        if az >= self.lot {
            o.ask(ask, az);
        }
        info!(
            k = m.k,
            p, pos, ret, target, count, n, volume, dd, bb, ba, "quote"
        );
        self.last_price = p;
        self.last_pos = pos;
        Ok(())
    }
}
fn main() -> BotResult {
    mtx_kit::run(Maker::default())
}

#[cfg(test)]
mod tests {
    use super::*;
    use mtx_kit::{
        Pubkey,
        phoenix_rise::ix::market_order::{MarketOrderParams, create_place_market_order_ix},
    };

    #[test]
    fn decodes_sdk_market_packets_in_both_directions_with_and_without_limits() {
        let key = Pubkey::new_unique();
        for side in [Side::Bid, Side::Ask] {
            for priced in [false, true] {
                let mut builder = MarketOrderParams::builder()
                    .trader(key)
                    .trader_account(key)
                    .perp_asset_map(key)
                    .orderbook(key)
                    .spline_collection(key)
                    .global_trader_index(vec![key])
                    .active_trader_buffer(vec![key])
                    .side(side)
                    .num_base_lots(1234);
                if priced {
                    builder = builder.price_in_ticks(14305);
                }
                let ix = create_place_market_order_ix(builder.build().unwrap()).unwrap();
                let (direction, size) = taker_packet(&ix.data, 0.001).unwrap();
                assert_eq!(direction, if side == Side::Bid { 1.0 } else { -1.0 });
                assert!((size - 1.234).abs() < 1e-10);
                for n in 0..20 {
                    assert!(taker_packet(&ix.data[..n], 0.001).is_none());
                }
                let mut other = ix.data.clone();
                other[..8].copy_from_slice(&PhoenixInstruction::PlaceLimitOrder.discriminant());
                assert!(taker_packet(&other, 0.001).is_none());
            }
        }
    }

    #[test]
    fn exposure_keeps_a_five_percent_gap_inside_the_reserved_drawdown_budget() {
        for cap in [1.0, 5.0, 10.0, 20.0] {
            for price in [100.0, 143.05, 200.0] {
                for dd in 0..=130 {
                    let allowed = exposure_limit(cap, price, dd as f64);
                    assert!(allowed >= 0.0 && allowed < cap);
                    assert!(dd as f64 + allowed * price * 0.05 <= 220.0 + 1e-9);
                }
            }
        }
    }
}
