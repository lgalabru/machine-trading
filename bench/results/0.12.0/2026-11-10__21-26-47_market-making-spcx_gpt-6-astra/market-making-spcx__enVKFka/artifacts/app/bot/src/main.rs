//! Oracle-anchored SPCX market maker for the synchronous turn protocol.
//! Reads only standard Solana RPC and compiles all signal models into the binary.
//! The order sheet provides a second position-cap check before submission.

mod model;
mod trend;
use mtx_kit::{Trader, phoenix_rise, prelude::*};
use serde_json::{Value, json};
use solana_rpc_client_api::request::RpcRequest;
use std::collections::VecDeque;
use tracing::info;

#[derive(Default)]
struct Maker {
    tick: f64,
    base_half: f64,
    lot: f64,
    market_key: String,
    last_slot: u64,
    start_slot: u64,
    last_price: f64,
    last_pos: f64,
    drift: f64,
    flow: f64,
    tape: VecDeque<(u64, f64)>,
    burst_dir: f64,
    burst_until: u64,
    last_jump: u64,
    peak: f64,
    stopped: bool,
    total_orders: f64,
    total_buy: f64,
    total_sell: f64,
    nr: f64,
    rsum: f64,
    rsq: f64,
    last_buy: u64,
    last_sell: u64,
    jumps: u32,
    traded: bool,
}
impl Maker {
    // Decode public Phoenix IOC instructions from newly finalized blocks.
    // Identity and order size do not distinguish informed traders in this market;
    // the signals use order direction and arrival times, excluding our own trades.
    fn read_tape(&mut self, t: &Trader, slot: u64) -> (f64, f64) {
        let mut buys = 0.0;
        let mut sells = 0.0;
        let tag = phoenix_rise::ix::PhoenixInstruction::PlaceMarketOrder.discriminant();
        let authority = t.authority().to_string();
        for s in self.last_slot.max(slot.saturating_sub(4))..slot {
            let result=t.rpc.send::<Value>(RpcRequest::GetBlock,json!([s,{"encoding":"json","transactionDetails":"full","rewards":false,"maxSupportedTransactionVersion":0,"commitment":"confirmed"}]));
            if let Ok(b) = result {
                if let Some(txs) = b["transactions"].as_array() {
                    for tx in txs {
                        let msg = &tx["transaction"]["message"];
                        let Some(keys) = msg["accountKeys"].as_array() else {
                            continue;
                        };
                        if keys.first().and_then(Value::as_str) == Some(&authority) {
                            continue;
                        }
                        if !keys.iter().any(|k| k.as_str() == Some(&self.market_key)) {
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
                            if data.len() < 19 || data[..8] != tag || data[8] != 2 || data[9] > 1 {
                                continue;
                            }
                            let sign = if data[9] == 0 { 1.0 } else { -1.0 };
                            let off = 11 + if data[10] == 1 { 8 } else { 0 };
                            if data.len() < off + 8 {
                                continue;
                            }
                            let qty = u64::from_le_bytes(data[off..off + 8].try_into().unwrap())
                                as f64
                                * self.lot;
                            self.tape.push_back((s, sign));
                            self.total_orders += 1.0;
                            if sign > 0.0 {
                                self.last_buy = s;
                                self.total_buy += 1.0;
                            } else {
                                self.last_sell = s;
                                self.total_sell += 1.0;
                            }
                            if sign > 0.0 {
                                buys += 1.0;
                            } else {
                                sells += 1.0;
                            }
                            info!(slot = s, sign, qty, "tape");
                        }
                    }
                }
            }
        }
        self.last_slot = slot;
        while self.tape.front().is_some_and(|x| x.0 + 40 < slot) {
            self.tape.pop_front();
        }
        (buys, sells)
    }
    // A separate persistent-regime forecast avoids interpreting every burst as
    // a trend. Once a jump has been observed, this family has no trend component.
    fn trend_forecast(&self, slot: u64, left: u64) -> f64 {
        if self.jumps > 0 || self.nr < 3.0 {
            return 0.0;
        }
        let mean = self.rsum / self.nr;
        let sd = (self.rsq / self.nr - mean * mean).max(0.01).sqrt();
        let b = self
            .tape
            .iter()
            .filter(|&&(t, d)| t + 40 >= slot && d > 0.0)
            .count() as f64;
        let s = self
            .tape
            .iter()
            .filter(|&&(t, d)| t + 40 >= slot && d < 0.0)
            .count() as f64;
        let predict = |direction: f64| {
            trend::predict(&[
                direction * self.rsum,
                mean * direction,
                sd,
                self.nr,
                mean * direction * self.nr.sqrt() / sd,
                direction * (self.total_buy - self.total_sell),
                self.total_orders,
                direction * (self.total_buy - self.total_sell - b + s),
                self.total_orders - b - s,
                slot.saturating_sub(self.start_slot) as f64,
                left as f64,
            ])
        };
        (predict(1.0) - predict(-1.0)).clamp(-1.0, 1.0)
    }
    // Direction-symmetric probability of a jump in the next 24 slots. All
    // features are trailing observations; cooldowns use the announced family.
    fn forecast(&self, slot: u64, left: u64) -> (f64, f64) {
        let time = slot.saturating_sub(self.start_slot);
        let since = if self.last_jump > 0 {
            slot - self.last_jump
        } else {
            1000
        };
        if time < 30 || left < 60 || since < 30 || self.jumps >= 3 {
            return (0.0, 0.0);
        }
        let mut b = Vec::new();
        let mut s = Vec::new();
        for w in [4, 8, 12, 20, 30, 40] {
            b.push(
                self.tape
                    .iter()
                    .filter(|&&(t, d)| t + w >= slot && d > 0.0)
                    .count() as f64,
            );
            s.push(
                self.tape
                    .iter()
                    .filter(|&&(t, d)| t + w >= slot && d < 0.0)
                    .count() as f64,
            );
        }
        let predict = |up: bool| {
            let mut x = if up { b.clone() } else { s.clone() };
            x.extend(if up { &s } else { &b });
            let (last_same, last_opp) = if up {
                (self.last_buy, self.last_sell)
            } else {
                (self.last_sell, self.last_buy)
            };
            x.extend([
                self.total_orders / time.max(1) as f64,
                slot.saturating_sub(last_same + 2).min(40) as f64,
                slot.saturating_sub(last_opp + 2).min(40) as f64,
                time.min(100) as f64,
                left.min(100) as f64,
                since.min(100) as f64,
                self.jumps as f64,
            ]);
            model::predict(&x)
        };
        (predict(true), predict(false))
    }
}
impl Strategy for Maker {
    async fn start(&mut self, s: &Session) -> BotResult {
        let m = s.trader.phoenix.market(&s.env.symbol)?;
        self.tick = s.trader.phoenix.ticks_to_usd(m, 1);
        self.lot = 10f64.powi(-(m.base_lots_decimals as i32));
        self.market_key = m.market_pubkey.clone();
        self.last_slot = s.hello.start_slot;
        self.start_slot = s.hello.start_slot;
        Ok(())
    }
    async fn step(&mut self, m: &Market, o: &mut Orders<'_>) -> BotResult {
        let p = m.oracle;
        let pos = m.position.size;
        let fill = if self.traded {
            0.0
        } else {
            pos - self.last_pos
        };
        self.traded = false;
        self.flow = self.flow * 0.88 - fill;
        let jumped = self.last_price > 0.0 && (p / self.last_price - 1.0).abs() > 0.005;
        if self.last_price > 0.0 && p != self.last_price {
            let r = p / self.last_price - 1.0;
            if !jumped {
                self.drift = 0.94 * self.drift + 0.06 * r;
                self.nr += 1.0;
                self.rsum += r * 10000.0;
                self.rsq += (r * 10000.0).powi(2);
            } else {
                self.flow = 0.0;
                self.burst_dir = 0.0;
                self.burst_until = 0;
                if m.slot.saturating_sub(self.last_jump) > 20 {
                    self.jumps += 1;
                }
                self.last_jump = m.slot;
            }
        }
        self.last_price = p;
        self.last_pos = pos;
        let trader = o.trader().unwrap();
        self.read_tape(trader, m.slot);
        let (pu, pd) = self.forecast(m.slot, m.slots_left);
        let alpha = (pu - pd) * 0.0275;
        let trend_score = self.trend_forecast(m.slot, m.slots_left);
        let margin = trader
            .phoenix
            .hawkeye(trader.authority())
            .view_margin(trader.authority(), 0, 0)
            .await?
            .value;
        // Use settled collateral and oracle-valued inventory, not Phoenix's
        // book-sensitive mark or the kit's mark-to-market-only PnL estimate.
        let equity = margin.collateral_quote_lots as f64 * 1e-6 + pos * (p - m.position.entry_usd);
        if self.peak == 0.0 {
            self.peak = equity;
        }
        self.peak = self.peak.max(equity);
        let dd = (self.peak - equity).max(0.0);
        if dd > m.max_drawdown * 0.65 {
            self.stopped = true;
        }
        trader.cancel_all()?;
        if m.last {
            return Ok(());
        }
        // Keep 35 USDC for costs/estimation error and enough remaining equity
        // to absorb a 4.5% adverse move at the proposed inventory limit.
        let cap = m
            .cap
            .min(((m.max_drawdown - dd - 35.0) / (p * 0.045)).max(0.0))
            .min(19.5);
        let bbo = trader.inner.bbo().await?;
        let market = trader.phoenix.market(&trader.env.symbol)?;
        let bb = bbo
            .best_bid_ticks()
            .map(|v| trader.phoenix.ticks_to_usd(market, v))
            .unwrap_or(p * 0.998);
        let ba = bbo
            .best_ask_ticks()
            .map(|v| trader.phoenix.ticks_to_usd(market, v))
            .unwrap_or(p * 1.002);
        // The front maker has only a few shares. Estimate the deeper spread
        // before competing, then charge that cost when considering a large hedge.
        if self.base_half == 0.0 {
            self.base_half = ((ba - bb) / (2.0 * p)).clamp(0.0005, 0.004);
        }
        let cost = (if alpha > 0.0 {
            (ba / p - 1.0).max(0.0)
        } else {
            (1.0 - bb / p).max(0.0)
        })
        .max(self.base_half * 0.85)
            + 0.00035;
        let detected = if alpha.abs() > 3.0 * cost + 0.0005 {
            alpha.signum()
        } else {
            0.0
        };
        if detected != 0.0 {
            if detected != self.burst_dir {
                info!(k = m.k, detected, pu, pd, cost, "burst");
            }
            self.burst_dir = detected;
            self.burst_until = m.slot + 24;
        }
        if m.slot > self.burst_until {
            self.burst_dir = 0.0;
        }
        let mut target = (19.5 * trend_score + (pu - pd) * 100.0).clamp(-cap, cap);
        if self.burst_dir != 0.0 {
            target = self.burst_dir * cap;
        }
        if self.stopped {
            target = 0.0;
        }
        let trend_cost = if trend_score > 0.0 {
            (ba / p - 1.0).max(0.0)
        } else {
            (1.0 - bb / p).max(0.0)
        } + 0.00035;
        let trend_take = self.burst_dir == 0.0
            && trend_score.abs() > 0.55
            && (pos - target).abs() > 6.0
            && trend_score.abs() * 0.00012 * (m.slots_left as f64 / 6.0) > 2.0 * trend_cost + 0.001;
        let cover = pos * alpha < 0.0 && alpha.abs() > cost + 0.0003 && pos.abs() > 2.0;
        if ((self.burst_dir != 0.0 || self.stopped) && (pos - target).abs() > 2.0)
            || pos.abs() > cap + 0.01
            || cover
            || trend_take
        {
            let aggressive = if cover && self.burst_dir == 0.0 {
                0.0
            } else {
                target.clamp(-cap, cap)
            };
            target_at_oracle(o, pos, aggressive, p);
            self.traded = true;
            info!(
                k = m.k,
                p,
                pos,
                target = aggressive,
                dd,
                pu,
                pd,
                trend_score,
                "hedge"
            );
            return Ok(());
        }
        if self.stopped {
            return Ok(());
        }
        // Small IOC trades can take stale quotes only at favorable oracle
        // prices; never assume a displayed crossed book offers two executable legs.
        let buy_edge = 0.0006 + ((pos - target).max(0.0)) * 0.00005;
        let sell_edge = 0.0006 + ((target - pos).max(0.0)) * 0.00005;
        if ba < p * (1.0 - buy_edge) && cap - pos >= 0.10 {
            o.buy((cap - pos).min(5.0), p * (1.0 - buy_edge));
            self.traded = true;
            info!(k = m.k, p, pos, ba, bb, "buy stale offer");
            return Ok(());
        }
        if bb > p * (1.0 + sell_edge) && cap + pos >= 0.10 {
            o.sell((cap + pos).min(5.0), p * (1.0 + sell_edge));
            self.traded = true;
            info!(k = m.k, p, pos, ba, bb, "sell stale bid");
            return Ok(());
        }
        let skew = (pos - target) * p * 0.00015;
        let flow_skew = self.flow.clamp(-4.0, 4.0) * p * 0.00008;
        let bid = (bb + self.tick)
            .min(p * (1.0 - 0.0003) - skew + flow_skew)
            .min(ba - self.tick);
        let ask = (ba - self.tick)
            .max(p * (1.0 + 0.0003) - skew + flow_skew)
            .max(bb + self.tick);
        let buy = 2.0_f64.min((cap - pos).max(0.0));
        let sell = 2.0_f64.min((cap + pos).max(0.0));
        if buy >= 0.05 {
            o.bid(
                if self.burst_dir < 0.0 {
                    bid.min(p * (1.0 - alpha.abs().max(0.002)))
                } else {
                    bid
                },
                buy,
            );
        }
        if sell >= 0.05 {
            o.ask(
                if self.burst_dir > 0.0 {
                    ask.max(p * (1.0 + alpha.abs().max(0.002)))
                } else {
                    ask
                },
                sell,
            );
        }
        info!(
            k = m.k,
            p,
            pos,
            fill,
            target,
            dd,
            pu,
            pd,
            trend_score,
            drift = self.drift,
            flow = self.flow,
            bid,
            ask,
            "quote"
        );
        Ok(())
    }
}
// IOC slices use the independent oracle as their price reference.
fn target_at_oracle(o: &mut Orders<'_>, position: f64, target: f64, oracle: f64) {
    let delta = target - position;
    let mut left = (delta.abs() * 1000.0).floor() / 1000.0;
    while left >= 0.001 {
        let q = left.min(8.0);
        if delta > 0.0 {
            o.buy(q, oracle * 1.008);
        } else {
            o.sell(q, oracle * 0.992);
        }
        left = (left - q).max(0.0);
    }
}
fn main() -> BotResult {
    mtx_kit::run(Maker::default())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exported_models_match_training_runtime() {
        let jump = [
            2., 4., 6., 7., 8., 10., 0., 1., 2., 3., 5., 6., 0.3, 0., 4., 100., 100., 100., 1.,
        ];
        let drift = [
            4.,
            0.2,
            3.,
            20.,
            0.29814239699997197,
            8.,
            40.,
            5.,
            26.,
            130.,
            500.,
        ];
        assert!((model::predict(&jump) - 0.3218966384835737).abs() < 1e-12);
        assert!((trend::predict(&drift) - 0.08008190876765466).abs() < 1e-12);
    }

    #[test]
    fn reversal_orders_obey_cap_and_oracle_limit() {
        for position in [-19.5, -7.123, 0.0, 4.321, 19.5] {
            for target in [-19.5, -1.234, 0.0, 9.876, 19.5] {
                let mut sheet = Orders::recording();
                target_at_oracle(&mut sheet, position, target, 143.11);
                let market = Market {
                    mark: 999.0,
                    oracle: 143.11,
                    cap: 20.0,
                    position: Position {
                        size: position,
                        entry_usd: 143.11,
                    },
                    ..Market::default()
                };
                let (actions, notes) = mtx_kit::plan(sheet.intents(), &market);
                assert!(notes.is_empty());
                let mut held = position;
                for action in actions {
                    let Action::Market { side, size, limit } = action else {
                        panic!("expected IOC")
                    };
                    assert!(size > 0.0 && size <= 8.0);
                    if side == Side::Bid {
                        held += size;
                        assert!((limit - 143.11 * 1.008).abs() < 1e-9);
                    } else {
                        held -= size;
                        assert!((limit - 143.11 * 0.992).abs() < 1e-9);
                    }
                    assert!(held.abs() <= 19.500001);
                }
                assert!((held - target).abs() < 0.00101);
            }
        }
    }

    #[test]
    fn order_flow_is_direction_symmetric_and_respects_jump_cooldown() {
        let mut maker = Maker {
            start_slot: 1000,
            total_orders: 6.0,
            last_buy: 1098,
            last_sell: 1090,
            tape: VecDeque::from([
                (1082, 1.0),
                (1090, -1.0),
                (1092, 1.0),
                (1094, 1.0),
                (1096, 1.0),
                (1098, 1.0),
            ]),
            ..Maker::default()
        };
        let up = maker.forecast(1100, 300);
        for (_, side) in &mut maker.tape {
            *side = -*side;
        }
        std::mem::swap(&mut maker.last_buy, &mut maker.last_sell);
        let down = maker.forecast(1100, 300);
        assert!((up.0 - down.1).abs() < 1e-12);
        assert!((up.1 - down.0).abs() < 1e-12);
        maker.last_jump = 1090;
        assert_eq!(maker.forecast(1100, 300), (0.0, 0.0));
        maker.last_jump = 1000;
        maker.jumps = 3;
        assert_eq!(maker.forecast(1100, 300), (0.0, 0.0));
        maker.jumps = 0;
        assert_eq!(maker.forecast(1100, 50), (0.0, 0.0));
    }
}
