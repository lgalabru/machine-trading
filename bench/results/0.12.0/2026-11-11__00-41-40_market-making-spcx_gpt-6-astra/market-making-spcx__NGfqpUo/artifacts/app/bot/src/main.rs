use mtx_kit::{Pubkey, phoenix_rise::accounts::trader::Trader as AccountTrader, prelude::*};
use std::{collections::VecDeque, str::FromStr};
use tracing::info;

#[derive(Clone)]
struct Config {
    half: f64,
    floor: f64,
    inv: f64,
    flow: f64,
    drift: f64,
    cap: f64,
    size: f64,
    competitive: bool,
    passive_exit: bool,
    trend_model: bool,
    burst_threshold: f64,
    burst_size: f64,
    hold: f64,
    burst_lean: f64,
}
impl Default for Config {
    fn default() -> Self {
        Self {
            half: 12.0,
            floor: 2.5,
            inv: 1.5,
            flow: 1.5,
            drift: 8.0,
            cap: 5.0,
            size: 2.0,
            competitive: true,
            passive_exit: true,
            trend_model: false,
            burst_threshold: 4.0,
            burst_size: 19.0,
            hold: 28.0,
            burst_lean: 150.0,
        }
    }
}
impl Config {
    fn load() -> Self {
        let mut c = Self::default();
        if let Ok(s) = std::fs::read_to_string("config.json") {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&s) {
                macro_rules! set {($($f:ident),*)=>{$(if let Some(x)=v[stringify!($f)].as_f64(){c.$f=x;})*}}
                set!(
                    half,
                    floor,
                    inv,
                    flow,
                    drift,
                    cap,
                    size,
                    burst_threshold,
                    burst_size,
                    hold,
                    burst_lean
                );
                if let Some(x) = v["competitive"].as_bool() {
                    c.competitive = x;
                }
                if let Some(x) = v["passive_exit"].as_bool() {
                    c.passive_exit = x;
                }
                if let Some(x) = v["trend_model"].as_bool() {
                    c.trend_model = x;
                }
            }
        }
        c
    }
}
struct Maker {
    c: Config,
    book: Pubkey,
    tick: f64,
    lot: f64,
    asset: u32,
    block_slot: u64,
    flow: VecDeque<(u64, f64)>,
    last_oracle: f64,
    drift: f64,
    equity_peak: f64,
    halt: bool,
    start_slot: u64,
    burst_dir: f64,
    burst_until: u64,
    last_jump: u64,
    jumps: u32,
    samples: f64,
    sum_return: f64,
    sum_squared: f64,
    buys: f64,
    sells: f64,
}
impl Default for Maker {
    fn default() -> Self {
        Self {
            c: Config::load(),
            book: Pubkey::default(),
            tick: 0.01,
            lot: 0.001,
            asset: 0,
            block_slot: 0,
            flow: VecDeque::new(),
            last_oracle: 0.0,
            drift: 0.0,
            equity_peak: 10000.0,
            halt: false,
            start_slot: 0,
            burst_dir: 0.0,
            burst_until: 0,
            last_jump: 0,
            jumps: 0,
            samples: 0.0,
            sum_return: 0.0,
            sum_squared: 0.0,
            buys: 0.0,
            sells: 0.0,
        }
    }
}
impl Strategy for Maker {
    async fn start(&mut self, s: &Session) -> BotResult {
        if s.env.symbol != "SPCX" {
            return Err("this strategy is restricted to SPCX".into());
        }
        let m = s.trader.phoenix.market(&s.env.symbol)?;
        self.book = Pubkey::from_str(&m.market_pubkey)?;
        self.tick = s.trader.phoenix.ticks_to_usd(m, 1);
        self.lot = 10.0_f64.powi(-(m.base_lots_decimals as i32));
        self.asset = m.asset_id;
        self.start_slot = s.hello.start_slot;
        self.block_slot = s.hello.start_slot.saturating_sub(1);
        Ok(())
    }
    async fn step(&mut self, m: &Market, o: &mut Orders<'_>) -> BotResult {
        let Some(t) = o.trader() else { return Ok(()) };
        let p = m.oracle;
        let mut q = m.position.size;
        t.cancel_all()?;
        let ac = t.rpc.get_account(&t.account)?;
        let at = AccountTrader::try_from_account_bytes(&ac.data)?;
        let mut equity = at.header.trader_state.quote_lot_collateral.as_inner() as f64 / 1e6;
        for (id, pos) in at.positions() {
            if id == self.asset as u64 {
                equity += pos.base_lot_position().as_inner() as f64 * self.lot * p
                    + pos.virtual_quote_lot_position().as_inner() as f64 / 1e6;
            }
        }
        self.equity_peak = self.equity_peak.max(equity);
        let dd = self.equity_peak - equity;
        if dd > 150.0 {
            self.halt = true;
        }
        if m.last {
            return Ok(());
        }
        if self.halt {
            o.target_position_with(
                0.0,
                Execution {
                    min_trade: 0.001,
                    slice: 4.0,
                    ..Execution::default()
                },
            );
            return Ok(());
        }
        let mut step_flow = 0.0;
        let disc = mtx_kit::phoenix_rise::ix::PhoenixInstruction::PlaceMarketOrder.discriminant();
        let authority = t.authority().to_string();
        let book_key = self.book.to_string();
        let program_key = mtx_kit::phoenix_rise::ix::PHOENIX_PROGRAM_ID.to_string();
        for slot in self.block_slot..m.slot {
            let v:serde_json::Value=t.rpc.send(solana_rpc_client_api::request::RpcRequest::GetBlock,
    serde_json::json!([slot,{"commitment":"processed","encoding":"json","transactionDetails":"full","rewards":false,"maxSupportedTransactionVersion":0}]))?;
            if v.is_null() {
                break;
            }
            self.block_slot = slot + 1;
            if let Some(txs) = v["transactions"].as_array() {
                for tx in txs {
                    if !tx["meta"]["err"].is_null() {
                        continue;
                    }
                    let msg = &tx["transaction"]["message"];
                    let Some(keys) = msg["accountKeys"].as_array() else {
                        continue;
                    };
                    if keys.first().and_then(|k| k.as_str()) == Some(authority.as_str()) {
                        continue;
                    }
                    if !keys.iter().any(|k| k.as_str() == Some(book_key.as_str())) {
                        continue;
                    }
                    let Some(ixs) = msg["instructions"].as_array() else {
                        continue;
                    };
                    for ix in ixs {
                        let Some(program_index) = ix["programIdIndex"].as_u64() else {
                            continue;
                        };
                        if keys.get(program_index as usize).and_then(|v| v.as_str())
                            != Some(program_key.as_str())
                        {
                            continue;
                        }
                        if !ix["accounts"].as_array().is_some_and(|a| {
                            a.iter().any(|i| {
                                i.as_u64()
                                    .and_then(|n| keys.get(n as usize))
                                    .and_then(|v| v.as_str())
                                    == Some(book_key.as_str())
                            })
                        }) {
                            continue;
                        }
                        let Some(data) = ix["data"].as_str() else {
                            continue;
                        };
                        let Ok(raw) = bs58::decode(data).into_vec() else {
                            continue;
                        };
                        if raw.len() < 20 || raw[..8] != disc || raw[8] != 2 || raw[9] > 1 {
                            continue;
                        }
                        let sign = if raw[9] == 0 { 1.0 } else { -1.0 };
                        self.flow.push_back((slot, sign));
                        step_flow += sign;
                        if sign > 0.0 {
                            self.buys += 1.0;
                        } else {
                            self.sells += 1.0;
                        }
                    }
                }
            }
        }
        while self
            .flow
            .front()
            .is_some_and(|(s, _)| m.slot.saturating_sub(*s) > 32)
        {
            self.flow.pop_front();
        }
        let flow: f64 = self
            .flow
            .iter()
            .map(|(s, v)| v * (-(m.slot.saturating_sub(*s) as f64) / 12.0).exp())
            .sum();
        if self.last_oracle > 0.0 {
            let r = (p / self.last_oracle - 1.0) * 10000.0;
            if r.abs() < 80.0 {
                self.drift = 0.975 * self.drift + 0.025 * r;
                self.samples += 1.0;
                self.sum_return += r;
                self.sum_squared += r * r;
            } else {
                self.flow.clear();
                self.burst_dir = 0.0;
                self.last_jump = m.slot;
                self.jumps += 1;
            }
        }
        self.last_oracle = p;
        let mut drift = self.drift;
        if self.c.trend_model && self.jumps == 0 {
            let variance = ((self.sum_squared + 80.0) / (self.samples + 20.0)
                - (self.sum_return / self.samples.max(1.0)).powi(2))
            .max(0.5);
            let (mut weight, mut estimate) = (0.75, 0.0);
            for mu in [-0.6_f64, -0.4, -0.2, 0.2, 0.4, 0.6] {
                let prob = if mu > 0.0 { 0.58_f64 } else { 0.42_f64 };
                let ll = (mu * self.sum_return - 0.5 * self.samples * mu * mu) / variance
                    + 0.5
                        * (self.buys * (prob / 0.5).ln() + self.sells * ((1.0 - prob) / 0.5).ln());
                let w = ll.clamp(-50.0, 50.0).exp() * 0.25 / 6.0;
                weight += w;
                estimate += w * mu;
            }
            drift = estimate / weight;
        }
        if self.jumps > 0 {
            drift = 0.0;
        }
        let bbo = t.inner.bbo().await?;
        let bb = bbo
            .best_bid_ticks()
            .map(|v| v as f64 * self.tick)
            .unwrap_or(p * 0.995);
        let ba = bbo
            .best_ask_ticks()
            .map(|v| v as f64 * self.tick)
            .unwrap_or(p * 1.005);
        let scores = burst_scores(&self.flow, m.slot, self.last_jump);
        if m.slot >= self.burst_until || m.slots_left <= 60 || self.jumps >= 3 {
            self.burst_dir = 0.0;
        }
        let score = scores[0].max(scores[1]);
        if score >= self.c.burst_threshold
            && m.slots_left > 60
            && m.slot.saturating_sub(self.start_slot) >= 30
            && self.jumps < 3
            && m.slot.saturating_sub(self.last_jump) >= 32
        {
            self.burst_dir = if scores[0] > scores[1] { 1.0 } else { -1.0 };
            self.burst_until = m.slot + self.c.hold as u64;
        }
        let riskcap = risk_cap(m.cap, m.max_drawdown, dd, p);
        let active = self.burst_dir != 0.0;
        let cap = (if active {
            self.c.burst_size + 2.0
        } else {
            self.c.cap
        })
        .min(riskcap);
        let target = if active {
            self.burst_dir * self.c.burst_size.min(cap)
        } else {
            (drift * self.c.drift).clamp(-self.c.cap * 0.7, self.c.cap * 0.7)
        };
        let need = q.abs() > riskcap + 0.001
            || if active {
                (q - target).abs() > 3.0
            } else {
                q.abs() > (if self.c.passive_exit { riskcap } else { cap }) + 0.001
            };
        if need {
            let goal = target.clamp(-cap, cap);
            for _ in 0..12 {
                let delta = goal - q;
                if delta.abs() < 0.01 {
                    break;
                }
                let side = if delta > 0.0 { Side::Bid } else { Side::Ask };
                let size = (delta.abs().min(4.0) / self.lot).floor() * self.lot;
                if size < self.lot {
                    break;
                }
                t.place_market(side, size, p * (1.0 + delta.signum() * 0.005))
                    .await?;
                let now = t.position().await?.size;
                if (now - q).abs() < self.lot * 0.5 {
                    break;
                }
                q = now;
            }
        }
        let lean = if active {
            self.burst_dir * self.c.burst_lean + (target - q) * self.c.inv
        } else {
            (target - q) * self.c.inv + flow * self.c.flow
        };
        let bid_half = if self.c.competitive {
            ((p - bb - self.tick) / p * 1e4).min(self.c.half)
        } else {
            self.c.half
        };
        let ask_half = if self.c.competitive {
            ((ba - p - self.tick) / p * 1e4).min(self.c.half)
        } else {
            self.c.half
        };
        let mut bid = grid_price(
            p * (1.0 - (bid_half - lean).max(self.c.floor) / 1e4),
            self.tick,
            false,
        );
        let mut ask = grid_price(
            p * (1.0 + (ask_half + lean).max(self.c.floor) / 1e4),
            self.tick,
            true,
        );
        if self.c.passive_exit && !active && q.abs() > self.c.cap {
            if q > 0.0 {
                ask = grid_price(p + self.tick * 0.5, self.tick, true);
            } else {
                bid = grid_price(p - self.tick * 0.5, self.tick, false);
            }
        }
        let buy_size = (self.c.size.min(cap - q) * 1000.0).floor() / 1000.0;
        let sell_size = (self.c.size.min(cap + q) * 1000.0).floor() / 1000.0;
        if buy_size >= 0.001 {
            o.bid(bid, buy_size);
        }
        if sell_size >= 0.001 {
            o.ask(ask, sell_size);
        }
        info!(
            k = m.k,
            slot = m.slot,
            block_slot = self.block_slot,
            p,
            q,
            equity,
            dd,
            flow,
            step_flow,
            score,
            active,
            target,
            drift,
            bid,
            ask,
            bb,
            ba,
            "quote"
        );
        Ok(())
    }
}
fn main() -> BotResult {
    mtx_kit::run(Maker::default())
}

/// Reserve $35 for fees/slippage and size against a 5% adverse gap.
fn risk_cap(cap: f64, limit: f64, drawdown: f64, price: f64) -> f64 {
    if !price.is_finite() || price <= 0.0 || !drawdown.is_finite() {
        return 0.0;
    }
    (cap - 0.01)
        .max(0.0)
        .min((limit - drawdown - 35.0).max(0.0) / (price * 0.05))
}

/// A directional Poisson likelihood ratio, at several short horizons.
fn burst_scores(flow: &VecDeque<(u64, f64)>, slot: u64, last_jump: u64) -> [f64; 2] {
    let mut scores = [0.0_f64; 2];
    for window in [6, 10, 16, 24, 32] {
        if slot.saturating_sub(last_jump) < window + 6 {
            continue;
        }
        let mut counts = [0.0_f64; 2];
        for (s, sign) in flow {
            if *s < slot && slot.saturating_sub(*s) <= window {
                counts[if *sign > 0.0 { 0 } else { 1 }] += 1.0;
            }
        }
        let base = 0.17 * window as f64;
        for i in 0..2 {
            let n = counts[i];
            if n >= 3.0 && n > base {
                scores[i] = scores[i].max(n * (n / base).ln() - n + base);
            }
        }
    }
    scores
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn prices_stay_on_the_spcx_tick_grid() {
        assert_eq!(
            grid_price(143.14000000000001, 0.01, true).to_string(),
            "143.14"
        );
        assert_eq!(
            grid_price(143.13999999999999, 0.01, false).to_string(),
            "143.14"
        );
        assert_eq!(grid_price(143.145, 0.01, true), 143.15);
        assert_eq!(grid_price(143.145, 0.01, false), 143.14);
    }
    #[test]
    fn caps_leave_a_gap_and_execution_reserve() {
        for price in [100.0, 143.11, 180.0] {
            for dd in [0.0, 50.0, 100.0, 150.0, 200.0, 240.0] {
                let cap = risk_cap(20.0, 250.0, dd, price);
                assert!((0.0..=19.99).contains(&cap));
                if dd <= 215.0 {
                    assert!(dd + cap * price * 0.05 <= 215.0 + 1e-8);
                }
            }
        }
        assert_eq!(risk_cap(20.0, 250.0, 0.0, f64::NAN), 0.0);
    }
    #[test]
    fn detects_directional_bursts_and_clears_after_jumps() {
        let burst = (0..7).map(|i| (100 + i / 2, 1.0)).collect();
        let s = burst_scores(&burst, 106, 0);
        assert!(s[0] > 4.0 && s[1] == 0.0);
        assert_eq!(burst_scores(&burst, 106, 105), [0.0, 0.0]);
        assert_eq!(burst_scores(&burst, 150, 0), [0.0, 0.0]);
    }
    #[test]
    fn future_or_open_block_events_are_not_signals() {
        let flow = (0..10).map(|i| (110 + i, 1.0)).collect();
        assert_eq!(burst_scores(&flow, 110, 0), [0.0, 0.0]);
    }
}

/// Canonical decimal output avoids a second, accidental tick of SDK rounding.
fn grid_price(price: f64, tick: f64, up: bool) -> f64 {
    let ticks = if up {
        (price / tick - 1e-9).ceil()
    } else {
        (price / tick + 1e-9).floor()
    };
    (ticks * tick * 1e6).round() / 1e6
}
