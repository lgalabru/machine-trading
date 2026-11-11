//! Oracle-anchored SPCX market maker with bounded inventory and a short-lived
//! hedge against unusually dense directional taker flow. All observations are
//! ordinary on-chain RPC reads; no scenario files are read at runtime.

use mtx_kit::prelude::*;
use serde_json::{Value, json};
use solana_rpc_client_api::request::RpcRequest;
use std::collections::VecDeque;

#[derive(Default)]
struct Maker {
    tick: f64,
    start_slot: u64,
    traded: bool,
    lot: f64,
    book_address: String,
    flow_orders: VecDeque<(u64, f64)>,
    last_price: f64,
    last_position: f64,
    drift: f64,
    flow: f64,
    peak: f64,
    stopped: bool,
    burst: f64,
    expires: u64,
    cooldown: u64,
}
impl Maker {
    // Sealed blocks expose completed market orders, including orders filled
    // against competitors. Count arrivals: trade size alone is not informative.
    fn observe(&mut self, m: &Market, t: &mtx_kit::Trader) {
        for slot in m.slot.saturating_sub(2)..m.slot {
            let block: Result<Value, _> = t.rpc.send(
                RpcRequest::GetBlock,
                json!([slot, {
                    "encoding":"json", "transactionDetails":"full", "rewards":false,
                    "maxSupportedTransactionVersion":0, "commitment":"confirmed"
                }]),
            );
            if let Ok(b) = block {
                if let Some(txs) = b["transactions"].as_array() {
                    for tx in txs {
                        let msg = &tx["transaction"]["message"];
                        if msg["accountKeys"][0].as_str() == Some(&t.authority().to_string()) {
                            continue;
                        }
                        if let Some(ixs) = msg["instructions"].as_array() {
                            for ix in ixs {
                                if let Some(encoded) = ix["data"].as_str() {
                                    if let Ok(data) = bs58::decode(encoded).into_vec() {
                                        let tag = mtx_kit::phoenix_rise::ix::PhoenixInstruction::PlaceMarketOrder.discriminant();
                                        if data.len() >= 19 && data[..8] == tag && data[8] == 2 {
                                            let off = 11 + if data[10] == 1 { 8 } else { 0 };
                                            if let Some(bytes) = data.get(off..off + 8) {
                                                let size =
                                                    u64::from_le_bytes(bytes.try_into().unwrap())
                                                        as f64
                                                        * self.lot;
                                                let signed =
                                                    if data[9] == 0 { size } else { -size };
                                                self.flow_orders.push_back((slot, signed));
                                                tracing::debug!(slot, signed, "observed taker");
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }

        while self.flow_orders.front().is_some_and(|x| m.slot - x.0 > 40) {
            self.flow_orders.pop_front();
        }
    }
    fn target(o: &mut Orders<'_>, target: f64) {
        o.target_position_with(
            target,
            Execution {
                min_trade: 0.001,
                slice: 5.0,
                slippage_bps: 100.0,
                ..Execution::default()
            },
        );
    }
}
impl Strategy for Maker {
    async fn start(&mut self, s: &Session) -> BotResult {
        if s.env.symbol != "SPCX" {
            return Err("this strategy only trades SPCX".into());
        }
        let m = s.trader.phoenix.market(&s.env.symbol)?;
        self.start_slot = s.hello.start_slot;
        self.book_address = m.market_pubkey.to_string();
        self.lot = 10f64.powi(-(m.base_lots_decimals as i32));
        self.tick = s.trader.phoenix.ticks_to_usd(m, 1);
        Ok(())
    }
    async fn step(&mut self, m: &Market, o: &mut Orders<'_>) -> BotResult {
        let p = m.oracle;
        let q = m.position.size;
        if !(p > 0.0 && p.is_finite()) {
            o.cancel_all();
            return Ok(());
        }
        self.flow *= 0.86;
        if !self.traded {
            self.flow -= q - self.last_position;
        }
        self.traded = false;
        let mut jumped = false;
        if self.last_price > 0.0 && p != self.last_price {
            let ret = (p / self.last_price - 1.0) * 10000.0;
            if ret.abs() < 50.0 {
                self.drift = self.drift * 0.94 + ret * 0.06;
            } else {
                self.flow = 0.0;
                self.burst = 0.0;
                self.expires = 0;
                self.cooldown = m.slot + 36;
                self.flow_orders.clear();
                jumped = true;
            }
        }
        self.last_price = p;
        self.last_position = q;
        let mut bb = p * 0.999;
        let mut ba = p * 1.001;
        let mut dd = 0.0;
        if let Some(t) = o.trader() {
            self.observe(m, t);
            // Remove our quotes before observing the competing book. A failed
            // cancellation aborts the turn, so replacement orders cannot stack.
            t.cancel_all()?;
            let bbo = t.inner.bbo().await?;
            let cfg = t.phoenix.market(&t.env.symbol)?;
            if let Some(b) = bbo.best_bid_ticks() {
                bb = t.phoenix.ticks_to_usd(cfg, b);
            }
            if let Some(a) = bbo.best_ask_ticks() {
                ba = t.phoenix.ticks_to_usd(cfg, a);
            }
            let margin = t
                .phoenix
                .hawkeye(t.authority())
                .view_margin_for_trader(t.account)
                .await?
                .value;
            // Real collateral includes realized PnL and fees. Revalue the open
            // position at the oracle rather than the book-influenced mark.
            let eq = margin.collateral_quote_lots as f64 / 1e6 + q * (p - m.position.entry_usd);
            self.peak = self.peak.max(eq);
            dd = self.peak - eq;
            if dd > m.max_drawdown * 0.56 {
                self.stopped = true;
            }
        } else {
            o.cancel_all();
        }
        if self.stopped {
            self.traded = true;
            Self::target(o, 0.0);
            return Ok(());
        }
        // Reserve $25 for execution and limit exposure to a hypothetical 5%
        // adverse move within the remaining drawdown allowance.
        let risk_cap = ((m.max_drawdown - dd - 25.0).max(0.0) / (p * 0.05))
            .min(m.cap - self.lot)
            .min(19.0);
        // Exponential arrival intensity: the newest sealed block has weight 1.
        let weight =
            |slot: u64| (-(m.slot.saturating_sub(slot).saturating_sub(2) as f64) / 5.0).exp();
        let nb: f64 = self
            .flow_orders
            .iter()
            .filter(|x| x.1 > 0.0)
            .map(|x| weight(x.0))
            .sum();
        let ns: f64 = self
            .flow_orders
            .iter()
            .filter(|x| x.1 < 0.0)
            .map(|x| weight(x.0))
            .sum();
        let signal = if nb > 4.0 && nb > ns {
            1.0
        } else if ns > 4.0 && ns > nb {
            -1.0
        } else {
            0.0
        };
        if self.burst != 0.0 && m.slot > self.expires {
            self.burst = 0.0;
        }
        if signal != 0.0
            && m.slot > self.start_slot + 32
            && m.slots_left > 60
            && m.slot >= self.cooldown
            && signal != self.burst
        {
            self.burst = signal;
            self.expires = m.slot + 36;
            eprintln!(
                "BURST slot={} direction={} buys={} sells={}",
                m.slot, signal, nb, ns
            );
        }
        tracing::debug!(
            step = m.k,
            oracle = p,
            position = q,
            burst = self.burst,
            drawdown = dd,
            "quote state"
        );
        if self.burst != 0.0 {
            let target = self.burst * risk_cap;
            if (target - q).abs() > 0.5 {
                self.traded = true;
                Self::target(o, target);
                return Ok(());
            }
            // During an unusually one-sided burst, preserve the directional hedge.
            if self.burst > 0.0 {
                if risk_cap - q >= self.lot {
                    o.bid(p * 0.9995, (risk_cap - q).min(1.0));
                }
                o.ask(p * 1.008, 0.1);
            } else {
                o.bid(p * 0.992, 0.1);
                if risk_cap + q >= self.lot {
                    o.ask(p * 1.0005, (risk_cap + q).min(1.0));
                }
            }
            return Ok(());
        }
        if q.abs() > 8.0 || (jumped && q.abs() > 2.0) {
            self.traded = true;
            Self::target(o, 0.0);
            self.flow = 0.0;
            return Ok(());
        }
        let target = (self.drift * 2.5 + self.flow * 1.0).clamp(-5.0, 5.0);
        let skew = (q - target) * 1.5;
        let half_b = ((p - bb) / p * 10000.0 - self.tick / p * 10000.0).clamp(3.0, 25.0);
        let half_a = ((ba - p) / p * 10000.0 - self.tick / p * 10000.0).clamp(3.0, 25.0);
        let bid =
            p * (1.0 - (half_b + skew.max(0.0) - (-skew).max(0.0).min(half_b - 1.0)) / 10000.0);
        let ask =
            p * (1.0 + (half_a + (-skew).max(0.0) - skew.max(0.0).min(half_a - 1.0)) / 10000.0);
        let cap = 8.0_f64.min(risk_cap);
        let bs = 2.0_f64.min((cap - q).max(0.0));
        let az = 2.0_f64.min((cap + q).max(0.0));
        if bs >= self.lot {
            o.bid((bid / self.tick).floor() * self.tick, bs);
        }
        if az >= self.lot {
            o.ask((ask / self.tick).ceil() * self.tick, az);
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

    fn maker() -> Maker {
        Maker {
            tick: 0.01,
            lot: 0.001,
            ..Maker::default()
        }
    }
    fn market(q: f64) -> Market {
        Market {
            k: 50,
            slot: 100,
            slots_left: 500,
            steps_left: 250,
            oracle: 143.11,
            mark: 143.11,
            cap: 20.0,
            max_drawdown: 250.0,
            position: Position {
                size: q,
                entry_usd: 143.11,
            },
            ..Market::default()
        }
    }
    #[test]
    fn order_sheet_respects_position_cap_on_both_sides() {
        for q in [-20.0, -19.0, -8.0, -2.0, 0.0, 2.0, 8.0, 19.0, 20.0] {
            let mut s = maker();
            let m = market(q);
            let intents = mtx_kit::step_offline(&mut s, &m).unwrap();
            let (actions, _) = plan(&intents, &m);
            let mut position = q;
            let mut bids = 0.0;
            let mut asks = 0.0;
            for a in actions {
                match a {
                    Action::CancelAll => {
                        bids = 0.0;
                        asks = 0.0;
                    }
                    Action::Market { side, size, .. } => {
                        position += if side == Side::Bid { size } else { -size };
                        assert!(position.abs() <= 20.0 + 1e-8);
                    }
                    Action::Limit { side, price, size } => {
                        assert!(price.is_finite() && price > 0.0 && size > 0.0);
                        if side == Side::Bid {
                            bids += size;
                        } else {
                            asks += size;
                        }
                    }
                }
            }
            assert!(position + bids <= 20.0 + 1e-8);
            assert!(position - asks >= -20.0 - 1e-8);
        }
    }
    #[test]
    fn burst_hedges_in_direction_of_taker_flow() {
        for direction in [-1.0, 1.0] {
            let mut s = maker();
            let m = market(0.0);
            for _ in 0..6 {
                s.flow_orders.push_back((98, direction));
            }
            let orders = mtx_kit::step_offline(&mut s, &m).unwrap();
            assert!(orders.iter().any(|o| matches!(o,Intent::Target{position,..} if *position*direction > 17.0 && position.abs()<20.0)));
        }
    }
    #[test]
    fn observed_jump_clears_burst_and_reduces_inventory() {
        let mut s = maker();
        s.last_price = 139.0;
        s.burst = 1.0;
        s.expires = 130;
        let orders = mtx_kit::step_offline(&mut s, &market(19.0)).unwrap();
        assert_eq!(s.burst, 0.0);
        assert!(
            orders
                .iter()
                .any(|o| matches!(o,Intent::Target{position,..} if *position==0.0))
        );
    }
    #[test]
    fn drawdown_stop_is_latched_and_only_flattens() {
        let mut s = maker();
        s.stopped = true;
        let orders = mtx_kit::step_offline(&mut s, &market(-8.0)).unwrap();
        assert!(s.stopped);
        assert!(
            orders
                .iter()
                .any(|o| matches!(o,Intent::Target{position,..} if *position==0.0))
        );
        assert!(
            !orders
                .iter()
                .any(|o| matches!(o, Intent::Limit { .. } | Intent::Quote { .. }))
        );
    }
    #[test]
    fn expired_burst_does_not_leave_large_inventory() {
        let mut s = maker();
        s.burst = -1.0;
        s.expires = 99;
        let orders = mtx_kit::step_offline(&mut s, &market(-19.0)).unwrap();
        assert_eq!(s.burst, 0.0);
        assert!(
            orders
                .iter()
                .any(|o| matches!(o,Intent::Target{position,..} if *position==0.0))
        );
    }
}
