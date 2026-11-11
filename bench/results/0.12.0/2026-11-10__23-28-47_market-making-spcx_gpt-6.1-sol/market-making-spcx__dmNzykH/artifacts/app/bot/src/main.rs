//! Oracle-centered Phoenix SPCX maker. All inputs are current chain state.
//! Resting inventory earns spread; burst and trend filters adjust reservation
//! prices and permit bounded directional exposure. The oracle-valued equity
//! reserve sizes every risky position against another sudden price move.

use mtx_kit::prelude::*;
use mtx_kit::{Trader, phoenix_rise};
use serde_json::{Value, json};
use solana_rpc_client_api::request::RpcRequest;
use std::collections::VecDeque;

// Phoenix serializes IOC packets as a Borsh enum, followed by the side,
// optional price and the base-lot quantity. Only these public fields are needed.
fn market_packet(data: &[u8]) -> Option<(f64, i64)> {
    let disc = phoenix_rise::ix::PhoenixInstruction::PlaceMarketOrder.discriminant();
    if data.len() < 11 || data[..8] != disc || data[8] != 2 || data[9] > 1 || data[10] > 1 {
        return None;
    }
    let offset = 11 + if data[10] == 1 { 8 } else { 0 };
    let bytes: [u8; 8] = data.get(offset..offset + 8)?.try_into().ok()?;
    let lots = i64::try_from(u64::from_le_bytes(bytes)).ok()?;
    Some((if data[9] == 0 { 1.0 } else { -1.0 }, lots))
}

fn oracle_equity(collateral: f64, unrealized: f64, position: f64, oracle: f64, mark: f64) -> f64 {
    collateral + unrealized + position * (oracle - mark)
}

fn position_reserve(oracle: f64, drawdown: f64, limit: f64) -> f64 {
    // Reserve another 4.5% move, plus 16% of the drawdown mandate for fees,
    // rounding and the difference between starting and current jump prices.
    (limit * 0.84 - drawdown).max(0.0) / (oracle * 0.045)
}

fn quote_sizes(position: f64, limit: f64) -> (f64, f64) {
    (
        2.5_f64.min((limit - position).max(0.0)),
        2.5_f64.min((limit + position).max(0.0)),
    )
}

#[derive(Default)]
struct Maker {
    tick: f64,
    last_oracle: f64,
    drift: f64,
    last_pos: f64,
    flow: f64,
    start_slot: u64,
    last_slot: u64,
    trades: VecDeque<(u64, f64, f64)>,
    peak: f64,
    stopped: bool,
    alarm: f64,
    alarm_until: u64,
    last_jump: u64,
    start_price: f64,
    moves: f64,
    sum_move: f64,
    sq_move: f64,
    buys: f64,
    sells: f64,
    saw_jump: bool,
    trend_direction: f64,
    jump_count: usize,
}
impl Maker {
    fn trend(&mut self, m: &Market) -> f64 {
        if self.saw_jump || self.moves < 5.0 {
            self.trend_direction = 0.0;
            return 0.0;
        }
        let n = self.moves;
        let var = ((self.sq_move - self.sum_move * self.sum_move / n + 160.0) / (n + 10.0))
            .clamp(4.0, 64.0);
        let mut ll = [0.0; 2];
        for (i, sign) in [1.0, -1.0].iter().enumerate() {
            let mut logs = Vec::new();
            for d in [0.6, 1.2, 1.8] {
                for prob in [0.55_f64, 0.585, 0.62] {
                    let (up, dn) = if *sign > 0.0 {
                        (self.buys, self.sells)
                    } else {
                        (self.sells, self.buys)
                    };
                    logs.push(
                        sign * d * self.sum_move / var - n * d * d / (2.0 * var)
                            + up * (2.0 * prob).ln()
                            + dn * (2.0 * (1.0 - prob)).ln(),
                    );
                }
            }
            let max = logs.iter().copied().fold(f64::NEG_INFINITY, f64::max);
            ll[i] = max + (logs.iter().map(|x| (x - max).exp()).sum::<f64>() / 9.0).ln();
        }
        let elapsed = (m.slot - self.start_slot) as f64;
        let prior = 0.25 + 0.25 * elapsed / (elapsed + m.slots_left as f64).max(1.0);
        let logs = [
            ll[0] + (prior / 2.0).ln(),
            ll[1] + (prior / 2.0).ln(),
            (1.0 - prior).ln(),
        ];
        let max = logs.iter().copied().fold(f64::NEG_INFINITY, f64::max);
        let weights = logs.map(|x| (x - max).exp());
        let diff = (weights[0] - weights[1]) / weights.iter().sum::<f64>();
        if diff > 0.4 {
            self.trend_direction = 1.0;
        }
        if diff < -0.4 {
            self.trend_direction = -1.0;
        }
        if diff * self.trend_direction < -0.1 {
            self.trend_direction = 0.0;
        }
        diff
    }

    fn read_flow(&mut self, t: &Trader, slot: u64) -> (f64, usize) {
        let own = t.authority().to_string();
        let prog = phoenix_rise::ix::constants::PHOENIX_PROGRAM_ID.to_string();
        let mut signed = 0.0;
        let mut count = 0;
        for s in self.last_slot.max(slot.saturating_sub(2))..=slot {
            let block: Result<Value, _> = t.rpc.send(RpcRequest::GetBlock, json!([s, {"encoding":"json", "transactionDetails":"full", "rewards":false, "maxSupportedTransactionVersion":0, "commitment":"processed"}]));
            if let Ok(b) = block {
                if let Some(txs) = b["transactions"].as_array() {
                    for tx in txs {
                        let msg = &tx["transaction"]["message"];
                        let Some(keys) = msg["accountKeys"].as_array() else {
                            continue;
                        };
                        if keys.first().and_then(Value::as_str) == Some(&own) {
                            continue;
                        }
                        let Some(ixs) = msg["instructions"].as_array() else {
                            continue;
                        };
                        for ix in ixs {
                            let Some(pi) = ix["programIdIndex"].as_u64() else {
                                continue;
                            };
                            if keys.get(pi as usize).and_then(Value::as_str) != Some(&prog) {
                                continue;
                            }
                            // The flow model is scoped to this market's orderbook too.
                            let cfg = t.phoenix.market(&t.env.symbol).unwrap();
                            let in_market = ix["accounts"].as_array().is_some_and(|accounts| {
                                accounts.iter().any(|a| {
                                    a.as_u64()
                                        .and_then(|i| keys.get(i as usize))
                                        .and_then(Value::as_str)
                                        == Some(cfg.market_pubkey.as_str())
                                })
                            });
                            if !in_market {
                                continue;
                            }
                            let Some(raw) = ix["data"].as_str() else {
                                continue;
                            };
                            let Ok(d) = bs58::decode(raw).into_vec() else {
                                continue;
                            };
                            let Some((sign, lots)) = market_packet(&d) else {
                                continue;
                            };
                            let cfg = t.phoenix.market(&t.env.symbol).unwrap();
                            let qty = t.phoenix.lots_to_base_units(cfg, lots);
                            signed += sign * qty;
                            count += 1;
                            if sign > 0.0 {
                                self.buys += 1.0;
                            } else {
                                self.sells += 1.0;
                            }
                            self.trades.push_back((slot, sign, qty));
                        }
                    }
                }
            } else if slot < self.start_slot + 6 {
                eprintln!("block error slot={s}: {:?}", block.err());
            }
        }
        self.last_slot = slot;
        while self.trades.front().is_some_and(|x| x.0 + 40 < slot) {
            self.trades.pop_front();
        }
        (signed, count)
    }
}
impl Strategy for Maker {
    async fn start(&mut self, s: &Session) -> BotResult {
        let cfg = s.trader.phoenix.market(&s.env.symbol)?;
        self.tick = s.trader.phoenix.ticks_to_usd(cfg, 1);
        self.start_slot = s.hello.start_slot;
        self.last_slot = s.hello.start_slot;
        self.peak = 10000.0;
        Ok(())
    }
    async fn step(&mut self, m: &Market, o: &mut Orders<'_>) -> BotResult {
        let Some(t) = o.trader() else { return Ok(()) };
        let p = m.oracle;
        if !p.is_finite() || p <= 0.0 {
            o.cancel_all();
            return Ok(());
        }
        let pos = m.position.size;
        let delta = pos - self.last_pos;
        self.flow = self.flow * 0.88 - delta;
        if self.start_price == 0.0 {
            self.start_price = p;
        }
        if self.last_oracle > 0.0 {
            let ret = (p - self.last_oracle) / self.start_price * 10000.0;
            if ret.abs() > 100.0 {
                self.saw_jump = true;
                self.jump_count += 1;
            } else if ret.abs() > 0.01 {
                self.moves += 1.0;
                self.sum_move += ret;
                self.sq_move += ret * ret;
            }
            if ret.abs() < 30.0 {
                self.drift = self.drift * 0.98 + ret * 0.02;
            } else {
                self.alarm = 0.0;
                self.alarm_until = 0;
                self.last_jump = m.slot;
                self.trades.clear();
            }
        }
        self.last_oracle = p;
        self.last_pos = pos;
        let (flow, n) = self.read_flow(t, m.slot);
        let margin = t
            .phoenix
            .hawkeye(t.authority())
            .view_margin(t.authority(), 0, 0)
            .await?
            .value;
        let equity = oracle_equity(
            margin.collateral_quote_lots as f64 / 1e6,
            margin.unrealized_pnl_quote_lots as f64 / 1e6,
            pos,
            p,
            m.mark,
        );
        self.peak = self.peak.max(equity);
        let dd = self.peak - equity;
        if dd > m.max_drawdown * 0.6 {
            self.stopped = true;
        }
        t.cancel_all()?;
        if self.stopped {
            if pos.abs() > 0.001 {
                o.target_position_with(
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
        let bbo = t.inner.bbo().await?;
        let cfg = t.phoenix.market(&t.env.symbol)?;
        let bestbid = t.phoenix.ticks_to_usd(cfg, bbo.best_bid_ticks);
        let bestask = t.phoenix.ticks_to_usd(cfg, bbo.best_ask_ticks);
        let mut score_buy = 0.0_f64;
        let mut score_sell = 0.0_f64;
        for window in [4u64, 6, 8, 10, 12, 16, 20, 24, 30] {
            let buys = self
                .trades
                .iter()
                .filter(|x| x.0 + window > m.slot && x.1 > 0.0)
                .count();
            let sells = self
                .trades
                .iter()
                .filter(|x| x.0 + window > m.slot && x.1 < 0.0)
                .count();
            score_buy = score_buy.max(buys as f64 * 1.466337 - window as f64 * 0.5);
            score_sell = score_sell.max(sells as f64 * 1.466337 - window as f64 * 0.5);
        }
        let burst = if score_buy.max(score_sell) >= 5.0 {
            if score_buy > score_sell { 1.0 } else { -1.0 }
        } else {
            0.0
        };
        if burst != 0.0
            && m.slot > self.last_jump + 30
            && m.slot > self.start_slot + 35
            && m.slots_left > 60
            && self.jump_count < 3
        {
            self.alarm = burst;
            self.alarm_until = m.slot + 30;
        }
        if m.slot > self.alarm_until {
            self.alarm = 0.0;
        }
        let trend_diff = self.trend(m);
        let base_target = if self.trend_direction != 0.0 {
            self.trend_direction * 15.0
        } else {
            (self.drift * 10.0).clamp(-3.0, 3.0)
        };
        let soft_flow = if self.jump_count >= 3 || m.slots_left <= 60 {
            0.0
        } else if score_buy > score_sell {
            (score_buy - 1.5).clamp(0.0, 4.0) * 2.0
        } else {
            -(score_sell - 1.5).clamp(0.0, 4.0) * 2.0
        };
        let target = if self.alarm != 0.0 {
            self.alarm * 19.9
        } else {
            base_target + soft_flow
        };
        let riskcap = position_reserve(p, dd, m.max_drawdown);
        let hard_cap = riskcap.min(m.cap - 0.1);
        let target = target.clamp(-hard_cap, hard_cap);
        let market_target = if self.alarm != 0.0 {
            target
        } else {
            base_target.clamp(-hard_cap, hard_cap)
        };
        let effective_pos = pos;
        if self.alarm != 0.0
            || (self.trend_direction != 0.0 && pos * self.trend_direction < 8.0)
            || (self.trend_direction == 0.0 && pos.abs() > 7.0)
            || pos.abs() > riskcap.min(19.9) + 0.1
        {
            let wanted = market_target;
            if (wanted - pos).abs() > 0.1 {
                o.target_position_with(
                    wanted,
                    Execution {
                        min_trade: 0.001,
                        slice: 5.0,
                        slippage_bps: 80.0,
                        ..Execution::default()
                    },
                );
                eprintln!(
                    "TARGET k={} p={} pos={} target={} eq={} flow={} n={}",
                    m.k, p, pos, wanted, equity, flow, n
                );
                return Ok(());
            }
        }
        let skew = (effective_pos - target) * 1.8 - self.flow.clamp(-4.0, 4.0) * 0.6;
        let bid = (bestbid + self.tick).min(p * (1.0 - (2.0 + skew).max(0.7) / 10000.0));
        let ask = (bestask - self.tick).max(p * (1.0 + (2.0 - skew).max(0.7) / 10000.0));
        let invcap = (if self.trend_direction != 0.0 {
            20.0_f64
        } else {
            6.0_f64
        })
        .min(riskcap)
        .min(m.cap - 0.01);
        let (mut bidsize, mut asksize) = quote_sizes(effective_pos, invcap);
        if self.alarm > 0.0 {
            bidsize = 0.0;
            asksize = 0.0;
        }
        if self.alarm < 0.0 {
            bidsize = 0.0;
            asksize = 0.0;
        }
        if bidsize >= 0.001 {
            o.bid((bid / self.tick + 1e-6).floor() * self.tick, bidsize);
        }
        if asksize >= 0.001 {
            o.ask((ask / self.tick - 1e-6).ceil() * self.tick, asksize);
        }
        if m.k % 20 == 0 {
            eprintln!(
                "k={} p={:.2} pos={:.3} eq={:.3} dd={:.3} drift={:.3} b={:.2} a={:.2} flow={:.3} n={} alarm={} trend={:.3} dir={}",
                m.k,
                p,
                pos,
                equity,
                dd,
                self.drift,
                bid,
                ask,
                flow,
                n,
                self.alarm,
                trend_diff,
                self.trend_direction
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
    fn reads_both_ioc_sides_and_both_optional_price_encodings() {
        for side in [0u8, 1] {
            for priced in [0u8, 1] {
                let mut packet = phoenix_rise::ix::PhoenixInstruction::PlaceMarketOrder
                    .discriminant()
                    .to_vec();
                packet.extend([2, side, priced]);
                if priced == 1 {
                    packet.extend(14305u64.to_le_bytes());
                }
                packet.extend(625u64.to_le_bytes());
                assert_eq!(
                    market_packet(&packet),
                    Some((if side == 0 { 1.0 } else { -1.0 }, 625))
                );
                packet.pop();
                assert!(market_packet(&packet).is_none());
            }
        }
        assert!(market_packet(&[]).is_none());
        assert!(market_packet(&[0; 64]).is_none());
    }

    #[test]
    fn an_inflated_mark_cannot_create_drawdown_headroom() {
        let original = oracle_equity(10000.0, 100.0, 10.0, 102.0, 110.0);
        let inflated = oracle_equity(10000.0, 500.0, 10.0, 102.0, 150.0);
        assert!((original - 10020.0).abs() < 1e-9);
        assert!((original - inflated).abs() < 1e-9);
    }

    #[test]
    fn reserve_leaves_room_for_another_jump() {
        for price in [80.0, 143.05, 200.0] {
            for dd in [0.0, 60.0, 100.0, 149.0, 180.0, 210.0, 249.0] {
                let cap = position_reserve(price, dd, 250.0).min(19.9);
                assert!(cap >= 0.0 && cap <= 20.0);
                assert!(dd + cap * price * 0.045 <= 210.000001 || dd > 210.0);
            }
        }
    }

    #[test]
    fn either_quote_can_fill_without_breaching_the_mandate() {
        for lots in (-20000..=20000).step_by(137) {
            let pos = lots as f64 / 1000.0;
            for working in [6.0, 15.0, 19.99] {
                let (bid, ask) = quote_sizes(pos, working);
                let bid = (bid * 1000.0).round() / 1000.0;
                let ask = (ask * 1000.0).round() / 1000.0;
                for b in [0.0, 0.5, 1.0] {
                    for a in [0.0, 0.5, 1.0] {
                        assert!((pos + b * bid - a * ask).abs() <= 20.000001);
                    }
                }
            }
        }
    }

    #[test]
    fn trend_filter_requires_consistent_price_and_flow_evidence() {
        let market = Market {
            slot: 300,
            slots_left: 300,
            ..Market::default()
        };
        let mut up = Maker {
            moves: 100.0,
            sum_move: 120.0,
            sq_move: 2644.0,
            buys: 170.0,
            sells: 110.0,
            ..Maker::default()
        };
        assert!(up.trend(&market) > 0.4);
        assert_eq!(up.trend_direction, 1.0);
        up.saw_jump = true;
        assert_eq!(up.trend(&market), 0.0);
        assert_eq!(up.trend_direction, 0.0);
        let mut flat = Maker {
            moves: 100.0,
            sum_move: 0.0,
            sq_move: 2500.0,
            buys: 140.0,
            sells: 140.0,
            ..Maker::default()
        };
        assert!(flat.trend(&market).abs() < 1e-10);
        assert_eq!(flat.trend_direction, 0.0);
    }
}
