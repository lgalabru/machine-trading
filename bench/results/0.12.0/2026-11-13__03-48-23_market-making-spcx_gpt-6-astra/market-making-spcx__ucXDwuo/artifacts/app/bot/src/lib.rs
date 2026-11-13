//! Oracle-anchored SPCX market maker. All forecasts use session data and
//! contemporaneous, read-only Phoenix observations; no external feeds or files.

use base64::Engine;
use mtx_kit::{
    prelude::*,
    venue::{PhoenixVenue, phoenix_rise},
};
use phoenix_rise::ix::hawkeye::{
    self, HawkeyeTraderViewAccounts, ViewAssetReturn, ViewMarginReturn,
};
use serde_json::{Value, json};
use std::collections::VecDeque;

#[derive(Default)]
struct Maker {
    tick: f64,
    lot_scale: f64,
    authority: String,
    trader: String,
    address: String,
    asset_id: u64,
    previous_oracle: f64,
    previous_position: f64,
    sum_returns: f64,
    sum_squares: f64,
    moves: f64,
    flow: f64,
    tape: VecDeque<(u64, f64)>,
    half: f64,
    peak_equity: f64,
    burst_direction: f64,
    burst_until: u64,
    last_jump: u64,
    total_orders: f64,
    total_signed: f64,
    stopped: bool,
    view_accounts: Option<HawkeyeTraderViewAccounts>,
    live_traders: Vec<(String, f64, bool)>,
}

impl Maker {
    // Confirmed IOC instructions reveal attempted flow, including unfilled
    // orders. Live position changes supplement the most recent turn.
    fn read_flow(&mut self, m: &Market) -> (f64, f64) {
        let mut signed = 0.0;
        let mut count = 0.0;
        let params = json!([m.slot.saturating_sub(2), {"encoding":"json","transactionDetails":"full","rewards":false,"maxSupportedTransactionVersion":0,"commitment":"confirmed"}]);
        let Ok(answer) = mtx_kit::rpc("getBlock", &params.to_string()) else {
            return (signed, count);
        };
        let Ok(block) = serde_json::from_str::<Value>(&answer) else {
            return (signed, count);
        };
        let Some(txs) = block["transactions"].as_array() else {
            return (signed, count);
        };
        let disc = phoenix_rise::ix::PhoenixInstruction::PlaceMarketOrder.discriminant();
        for tx in txs {
            let msg = &tx["transaction"]["message"];
            let Some(keys) = msg["accountKeys"].as_array() else {
                continue;
            };
            if keys.first().and_then(Value::as_str) == Some(self.authority.as_str()) {
                continue;
            }
            if !keys
                .iter()
                .any(|k| k.as_str() == Some(self.address.as_str()))
            {
                continue;
            }
            let Some(ixs) = msg["instructions"].as_array() else {
                continue;
            };
            for ix in ixs {
                let Some(data) = ix["data"].as_str() else {
                    continue;
                };
                let Ok(bytes) = bs58::decode(data).into_vec() else {
                    continue;
                };
                if bytes.len() < 11 || bytes[..8] != disc || bytes[8] != 2 {
                    continue;
                }
                let sign = match bytes[9] {
                    0 => 1.0,
                    1 => -1.0,
                    _ => continue,
                };
                if let Some(idx) = ix["accounts"][4].as_u64() {
                    if let Some(key) = keys.get(idx as usize).and_then(Value::as_str) {
                        if self.live_traders.len() < 12
                            && !self.live_traders.iter().any(|t| t.0 == key)
                        {
                            self.live_traders.push((key.to_string(), 0.0, false));
                        }
                    }
                }
                self.tape.push_back((m.k.saturating_sub(1), sign));
                signed += sign;
                count += 1.0;
            }
        }
        while self
            .tape
            .front()
            .is_some_and(|x| m.k.saturating_sub(x.0) >= 8)
        {
            self.tape.pop_front();
        }
        (signed, count)
    }
    // Hawkeye merges Phoenix's active trader buffers, including maker fills
    // that have not yet been synchronized to the base trader account.
    // All instructions in this simulated transaction are read-only views.
    fn live_views(&mut self, m: &Market) -> Option<(f64, f64, f64)> {
        let base = self.view_accounts.as_ref()?;
        let mut ixs = vec![hawkeye::create_hawkeye_view_margin_ix(base.clone())];
        for (key, _, _) in &self.live_traders {
            let mut accounts = base.clone();
            accounts.trader = key.parse().ok()?;
            ixs.push(hawkeye::create_hawkeye_view_margin_for_asset_ix(
                accounts,
                self.asset_id as u32,
            ));
        }
        let mut keys = vec![self.authority.parse().ok()?];
        for ix in &ixs {
            for a in &ix.accounts {
                if !keys.contains(&a.pubkey) {
                    keys.push(a.pubkey);
                }
            }
            if !keys.contains(&ix.program_id) {
                keys.push(ix.program_id);
            }
        }
        if keys.len() > 100 || ixs.len() > 14 {
            return None;
        }
        let mut tx = vec![1u8];
        tx.extend([0u8; 64]);
        tx.extend([1, 0, (keys.len() - 1) as u8, keys.len() as u8]);
        for key in &keys {
            tx.extend(key.to_bytes());
        }
        tx.extend([0u8; 32]);
        tx.push(ixs.len() as u8);
        for ix in &ixs {
            tx.push(keys.iter().position(|k| *k == ix.program_id)? as u8);
            tx.push(ix.accounts.len() as u8);
            for a in &ix.accounts {
                tx.push(keys.iter().position(|k| *k == a.pubkey)? as u8);
            }
            tx.push(ix.data.len() as u8);
            tx.extend(&ix.data);
        }
        if tx.len() > 1232 {
            return None;
        }
        let encoded = base64::engine::general_purpose::STANDARD.encode(tx);
        let answer=mtx_kit::rpc("simulateTransaction", &json!([encoded,{"encoding":"base64","replaceRecentBlockhash":true,"sigVerify":false,"commitment":"processed"}]).to_string()).ok()?;
        let v: Value = serde_json::from_str(&answer).ok()?;
        if !v["value"]["err"].is_null() {
            if m.k % 20 == 0 {
                mtx_kit::log(&format!("view error {}", v["value"]["err"]));
            }
            return None;
        }
        let logs = v["value"]["logs"].as_array()?;
        let prefix = format!("Program return: {} ", ixs[0].program_id);
        let returns: Vec<Vec<u8>> = logs
            .iter()
            .filter_map(Value::as_str)
            .filter_map(|s| s.strip_prefix(&prefix))
            .filter_map(|s| base64::engine::general_purpose::STANDARD.decode(s).ok())
            .collect();
        if returns.len() != ixs.len() {
            return None;
        }
        let margin =
            hawkeye::decode_hawkeye_return::<ViewMarginReturn>(&returns[0], "margin").ok()?;
        let equity = (margin.collateral_quote_lots as f64
            + margin.unrealized_pnl_quote_lots as f64
            + margin.unsettled_funding_quote_lots as f64)
            / 1e6
            + m.position.size * (m.oracle - m.mark);
        let mut buys = 0.0;
        let mut sells = 0.0;
        for (i, t) in self.live_traders.iter_mut().enumerate() {
            let view =
                hawkeye::decode_hawkeye_return::<ViewAssetReturn>(&returns[i + 1], "asset").ok()?;
            let pos = view.base_lots as f64 / self.lot_scale;
            if t.2 {
                let delta = pos - t.1;
                if delta > 0.001 {
                    buys += 1.0;
                }
                if delta < -0.001 {
                    sells += 1.0;
                }
            }
            t.1 = pos;
            t.2 = true;
        }
        Some((equity, buys, sells))
    }
    fn trend_target(&self) -> f64 {
        if self.moves < 8.0 || self.last_jump > 0 {
            return 0.0;
        }
        let var = ((self.sum_squares + 10.0 * 0.0005f64.powi(2)) / (self.moves + 10.0))
            .max(0.00015f64.powi(2));
        let mut up = 0.0;
        let mut down = 0.0;
        for mu in [0.00006, 0.00012, 0.00018] {
            up += (mu * self.sum_returns / var - self.moves * mu * mu / (2.0 * var))
                .clamp(-30.0, 30.0)
                .exp()
                / 24.0;
            down += (-mu * self.sum_returns / var - self.moves * mu * mu / (2.0 * var))
                .clamp(-30.0, 30.0)
                .exp()
                / 24.0;
        }
        let mut flow_up = 0.0;
        let mut flow_down = 0.0;
        for bias in [0.10f64, 0.17, 0.24] {
            let common = 0.5 * self.total_orders * (1.0 - bias * bias).ln();
            let directional = 0.5 * self.total_signed * ((1.0 + bias) / (1.0 - bias)).ln();
            flow_up += (common + directional).clamp(-30.0, 30.0).exp() / 3.0;
            flow_down += (common - directional).clamp(-30.0, 30.0).exp() / 3.0;
        }
        up *= flow_up;
        down *= flow_down;
        19.0 * (up - down) / (0.75 + up + down)
    }
}
impl Strategy for Maker {
    fn start(&mut self, s: &Session) -> BotResult {
        let venue = PhoenixVenue::from_json(&s.venue)?;
        let market = venue.market(&s.symbol)?;
        self.tick = venue.ticks_to_usd(market, 1);
        self.lot_scale = 10f64.powi(market.base_lots_decimals as i32);
        self.address = market.market_pubkey.clone();
        self.asset_id = market.asset_id as u64;
        self.authority = s.authority.clone();
        self.trader = venue.trader_account(&s.authority.parse()?).to_string();
        let keys = &venue.exchange().keys;
        self.view_accounts = Some(HawkeyeTraderViewAccounts {
            phoenix_program_id: *phoenix_rise::ix::PHOENIX_PROGRAM_ID,
            global_config: keys.global_config.parse()?,
            global_trader_index: keys
                .global_trader_index
                .iter()
                .map(|k| k.parse())
                .collect::<Result<Vec<_>, _>>()?,
            active_trader_buffer: keys
                .active_trader_buffer
                .iter()
                .map(|k| k.parse())
                .collect::<Result<Vec<_>, _>>()?,
            perp_asset_map: keys.perp_asset_map.parse()?,
            trader: self.trader.parse()?,
        });
        self.previous_oracle = s.start_oracle;
        self.half = 0.001;
        self.peak_equity = 10000.0;
        mtx_kit::log("SPCX maker: confirmed flow + live Phoenix views");
        Ok(())
    }
    fn step(&mut self, m: &Market, o: &mut Orders) -> BotResult {
        let p = m.oracle;
        if !p.is_finite() || p <= 0.0 {
            o.cancel_all();
            return Ok(());
        }
        let pos = m.position.size;
        let ret = p / self.previous_oracle - 1.0;
        let (signed, count) = self.read_flow(m);
        self.total_orders += count;
        self.total_signed += signed;
        self.flow = self.flow * 0.86 + signed;
        if ret.abs() > 0.005 {
            self.last_jump = m.k;
            self.burst_until = 0;
            self.burst_direction = 0.0;
            self.tape.clear();
            self.flow = 0.0;
        } else if ret.abs() > 0.000001 {
            self.sum_returns += ret;
            self.sum_squares += ret * ret;
            self.moves += 1.0;
        }
        self.previous_oracle = p;
        self.previous_position = pos;
        if m.k == 0 {
            let b = m.best_bid.unwrap_or(p * 0.999);
            let a = m.best_ask.unwrap_or(p * 1.001);
            self.half = ((a - b) / p * 0.5).clamp(0.0005, 0.003);
        }
        let (equity, live_buys, live_sells) =
            self.live_views(m)
                .unwrap_or((10000.0 + m.pnl + (p - m.mark) * pos, 0.0, 0.0));
        self.peak_equity = self.peak_equity.max(equity);
        let dd = self.peak_equity - equity;
        self.stopped |= dd > 115.0;
        o.cancel_all();
        if self.stopped {
            let mut left = pos.abs();
            while left > 0.01 {
                let size = left.min(8.0);
                if pos > 0.0 {
                    o.sell(size, p * 0.99).reduce_only();
                } else {
                    o.buy(size, p * 1.01).reduce_only();
                }
                left -= size;
            }
            return Ok(());
        }
        let n = self.tape.len() as f64 + live_buys + live_sells;
        let net: f64 = self.tape.iter().map(|x| x.1).sum::<f64>() + live_buys - live_sells;
        let expected = 4.0 * (self.total_orders + 30.0) / (m.k as f64 + 50.0);
        let surprise = (n + net.abs()) * 0.5 - expected;
        let cooldown_ok = self.last_jump == 0 || m.k > self.last_jump + 20;
        if surprise >= 5.0
            && m.k > 14
            && cooldown_ok
            && m.slots_left > 60
            && (signed + live_buys - live_sells) * net > 0.0
        {
            self.burst_direction = net.signum();
            self.burst_until = m.k + 16;
        }
        let trend = self.trend_target();
        let burst = if m.k < self.burst_until {
            self.burst_direction * 19.0
        } else {
            0.0
        };
        let target = if burst != 0.0 { burst } else { trend };
        // Cross only for a strong directional signal. Passive quotes handle normal inventory.
        let aggressive = burst != 0.0 || (trend.abs() > 9.0 && m.slots_left > 50);
        if aggressive && (target - pos).abs() > 4.0 {
            let diff = target - pos;
            let mut left = diff.abs();
            while left > 0.01 {
                let size = left.min(8.0);
                if diff > 0.0 {
                    o.buy(size, p * (1.0 + self.half + 0.0005));
                } else {
                    o.sell(size, p * (1.0 - self.half - 0.0005));
                }
                left -= size;
            }
            mtx_kit::log(&format!(
                "TRADE k={} p={} pos={} target={} net={} n={} live=({},{}) eq={}",
                m.k, p, pos, target, net, n, live_buys, live_sells, equity
            ));
            return Ok(());
        }
        let max_pos = m.cap.min(19.8);
        let half = (self.half * 0.5).clamp(0.0003, 0.0008);
        let skew = ((target - pos) * 0.00013 + self.flow * 0.00006).clamp(-0.003, 0.003);
        let bid = (p * (1.0 - half + skew) / self.tick).floor() * self.tick;
        let ask = (p * (1.0 + half + skew) / self.tick).ceil() * self.tick;
        let bid = if burst < 0.0 {
            bid.min(p * 0.995)
        } else {
            bid.min(p - self.tick)
        };
        let ask = if burst > 0.0 {
            ask.max(p * 1.005)
        } else {
            ask.max(p + self.tick)
        };
        let bs = (max_pos - pos).clamp(0.0, if burst < 0.0 { 0.05 } else { 2.0 });
        let az = (max_pos + pos).clamp(0.0, if burst > 0.0 { 0.05 } else { 2.0 });
        if bs >= 0.01 {
            o.bid(bid, bs);
        }
        if az >= 0.01 {
            o.ask(ask, az);
        }
        if m.k % 10 == 0 || burst != 0.0 {
            mtx_kit::log(&format!(
                "k={} p={:.4} pos={:.3} trend={:.2} burst={} flow={:.2} net={} n={} orders={} eq={:.3} dd={:.2}",
                m.k, p, pos, trend, burst, self.flow, net, n, self.total_orders, equity, dd
            ));
        }
        Ok(())
    }
}
mtx_kit::export!(Maker);

#[cfg(test)]
mod tests {
    use super::*;
    use mtx_kit::{Intent, Position, Side, step_offline};

    fn bot() -> Maker {
        Maker {
            tick: 0.01,
            lot_scale: 1000.0,
            previous_oracle: 143.0,
            half: 0.001,
            peak_equity: 10000.0,
            ..Maker::default()
        }
    }
    fn market(pos: f64) -> Market {
        Market {
            k: 100,
            slot: 1000,
            steps_left: 100,
            slots_left: 200,
            oracle: 143.0,
            mark: 143.0,
            cap: 20.0,
            max_drawdown: 250.0,
            position: Position {
                size: pos,
                entry_usd: 143.0,
            },
            ..Market::default()
        }
    }
    #[test]
    fn invalid_oracle_cancels_without_trading() {
        for p in [0.0, -1.0, f64::NAN, f64::INFINITY] {
            let mut m = market(5.0);
            m.oracle = p;
            let intents = step_offline(&mut bot(), &m).unwrap();
            assert_eq!(intents, vec![Intent::CancelAll]);
        }
    }
    #[test]
    fn passive_orders_respect_both_inventory_boundaries() {
        for pos in [-19.8, -8.0, 0.0, 8.0, 19.8] {
            let intents = step_offline(&mut bot(), &market(pos)).unwrap();
            assert!(matches!(intents.first(), Some(Intent::CancelAll)));
            let (mut bids, mut asks) = (0.0, 0.0);
            for i in intents {
                if let Intent::Limit { side, price, size } = i {
                    assert!(price.is_finite() && price > 0.0 && size > 0.0);
                    match side {
                        Side::Bid => {
                            bids += size;
                            assert!(price < 143.0);
                        }
                        Side::Ask => {
                            asks += size;
                            assert!(price > 143.0);
                        }
                    }
                }
            }
            assert!(pos + bids <= 20.0 + 1e-9);
            assert!(pos - asks >= -20.0 - 1e-9);
        }
    }
    #[test]
    fn strong_signal_reaches_target_in_bounded_slices() {
        let mut b = bot();
        b.burst_direction = 1.0;
        b.burst_until = 120;
        let intents = step_offline(&mut b, &market(-19.0)).unwrap();
        let mut bought = 0.0;
        for i in intents {
            if let Intent::Market { side, size, limit } = i {
                assert_eq!(side, Side::Bid);
                assert!(size <= 8.0);
                assert!(limit > 143.0);
                bought += size;
            }
        }
        assert!((bought - 38.0).abs() < 1e-9);
    }
    #[test]
    fn drawdown_stop_is_persistent_and_only_reduces_inventory() {
        let mut b = bot();
        let mut m = market(19.8);
        m.pnl = -130.0;
        let intents = step_offline(&mut b, &m).unwrap();
        assert!(b.stopped);
        let mut sold = 0.0;
        for i in intents {
            match i {
                Intent::CancelAll => {}
                Intent::Market { side, size, .. } => {
                    assert_eq!(side, Side::Ask);
                    assert!(size <= 8.0);
                    sold += size;
                }
                _ => panic!("stopped strategy added resting exposure"),
            }
        }
        assert!((sold - 19.8).abs() < 1e-9);
        assert_eq!(
            step_offline(&mut b, &market(0.0)).unwrap(),
            vec![Intent::CancelAll]
        );
    }
    #[test]
    fn trend_posterior_is_symmetric_and_resets_after_a_jump() {
        let mut up = bot();
        up.moves = 80.0;
        up.sum_returns = 0.012;
        up.sum_squares = 0.000020;
        up.total_orders = 200.0;
        up.total_signed = 40.0;
        let a = up.trend_target();
        assert!(a > 0.0 && a <= 19.0);
        up.sum_returns = -up.sum_returns;
        up.total_signed = -up.total_signed;
        assert!((a + up.trend_target()).abs() < 1e-9);
        up.last_jump = 50;
        assert_eq!(up.trend_target(), 0.0);
    }
}
