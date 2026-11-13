//! SPCX weekend strategy component. All live inputs come from the session,
//! market turns, and Phoenix read-only RPC; orders use the supplied order sheet.

use mtx_kit::{
    log, phoenix_market_address,
    prelude::*,
    rpc_account,
    venue::{PhoenixVenue, phoenix_rise},
};
use serde_json::{Value, json};
use std::collections::HashSet;
pub mod model;
use model::Model;
#[derive(Default)]
struct Bot {
    model: Option<Model>,
    address: String,
    authority: String,
    trader: String,
    names: Vec<String>,
    seen: HashSet<String>,
    start: u64,
    reopen: u64,
    start_price: f64,
    lot: f64,
    tick: f64,
    asset: u64,
    peak: f64,
    dd: f64,
    halfspread: f64,
    last_bid: f64,
    last_ask: f64,
    last_mid: f64,
    distorted: i32,
}
impl Bot {
    // Confirmed commitment exposes the current turn, without the finalized delay.
    fn read_flow(&mut self) {
        let Ok(r) = rpc(
            "getSignaturesForAddress",
            &json!([self.address,{"commitment":"confirmed","limit":40}]).to_string(),
        ) else {
            return;
        };
        let Ok(v) = serde_json::from_str::<Value>(&r) else {
            return;
        };
        let Some(arr) = v.as_array() else { return };
        for x in arr.iter().rev() {
            if x["slot"].as_u64().unwrap_or(0) < self.start || !x["err"].is_null() {
                continue;
            }
            let Some(sig) = x["signature"].as_str() else {
                continue;
            };
            if !self.seen.insert(sig.into()) {
                continue;
            }
            let params = json!([sig, {
                "encoding": "json", "commitment": "confirmed", "maxSupportedTransactionVersion": 0
            }]);
            let Ok(r) = rpc("getTransaction", &params.to_string()) else {
                continue;
            };
            let Ok(v) = serde_json::from_str::<Value>(&r) else {
                continue;
            };
            if !v["meta"]["err"].is_null() {
                continue;
            }
            let msg = &v["transaction"]["message"];
            let Some(keys) = msg["accountKeys"].as_array() else {
                continue;
            };
            if keys.first().and_then(Value::as_str) == Some(&self.authority) {
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
                if bytes.len() < 27
                    || bytes[..8]
                        != phoenix_rise::ix::PhoenixInstruction::PlaceMarketOrder.discriminant()
                    || bytes[8] != 2
                    || bytes[10] != 1
                {
                    continue;
                }
                let q = u64::from_le_bytes(bytes[19..27].try_into().unwrap()) as f64
                    * self.lot
                    * if bytes[9] == 0 { 1. } else { -1. };
                let limit =
                    u64::from_le_bytes(bytes[11..19].try_into().unwrap()) as f64 * self.tick;
                let Some(ai) = ix["accounts"][3].as_u64() else {
                    continue;
                };
                let Some(name) = keys.get(ai as usize).and_then(Value::as_str) else {
                    continue;
                };
                if name == self.authority {
                    continue;
                }
                let i = if let Some(i) = self.names.iter().position(|x| x == name) {
                    i
                } else {
                    self.names.push(name.into());
                    self.names.len() - 1
                };
                let t = v["slot"]
                    .as_u64()
                    .unwrap_or(self.start)
                    .saturating_sub(self.start) as f64;
                let aggression = ((limit / self.last_mid.max(1.) - 1.) * q.signum())
                    / self.halfspread.max(0.0008);
                if let Some(m) = &mut self.model {
                    m.trade(t, i, q, aggression);
                }
                log(&format!("FLOW {t} {i} {q:.3} {aggression:.2}"));
            }
        }
    }
    // Use actual collateral and virtual quote balance, so fees and slippage
    // enter the drawdown budget. The unchanged wallet USDC cancels from drawdown.
    fn risk(&mut self, mark: f64) {
        if let Ok(Some(data)) = rpc_account(&self.trader) {
            if let Ok(trader) = phoenix_rise::accounts::owned::Trader::try_from_account_bytes(&data)
            {
                let mut equity = i64::from(trader.state.quote_lot_collateral) as f64 / 1e6;
                for (asset, p) in &trader.positions.entries {
                    if *asset == self.asset {
                        equity += i64::from(p.virtual_quote_lot_position) as f64 / 1e6
                            + i64::from(p.base_lot_position) as f64 * self.lot * mark;
                    }
                }
                self.peak = self.peak.max(equity);
                self.dd = (self.peak - equity).max(0.);
            }
        }
    }
}
impl Strategy for Bot {
    fn start(&mut self, s: &Session) -> BotResult {
        self.start = s.start_slot;
        self.start_price = s.start_mark;
        self.authority = s.authority.clone();
        self.reopen = s
            .var("MTX_REOPEN_SLOT")
            .and_then(|x| x.parse().ok())
            .unwrap_or(s.end_slot);
        self.model = Some(Model::new(self.reopen.saturating_sub(self.start) as f64));
        self.address = phoenix_market_address(&s.venue, &s.symbol).unwrap_or_default();
        let venue = PhoenixVenue::from_json(&s.venue)?;
        let cfg = venue.market(&s.symbol)?;
        self.lot = 10f64.powi(-(cfg.base_lots_decimals as i32));
        self.tick = venue.ticks_to_usd(cfg, 1);
        self.asset = cfg.asset_id as u64;
        self.trader = venue
            .trader_account(&s.authority.parse().map_err(|_| "bad authority")?)
            .to_string();
        self.peak = 10000.;
        self.halfspread = 0.002;
        self.last_mid = s.start_mark;
        log(&format!(
            "START {} {} {} {}",
            self.start, self.reopen, self.start_price, self.asset
        ));
        Ok(())
    }
    fn step(&mut self, m: &Market, o: &mut Orders) -> BotResult {
        let bid = m.best_bid.unwrap_or(m.mark * 0.999);
        let ask = m.best_ask.unwrap_or(m.mark * 1.001);
        let mut mid = (bid + ask) * 0.5;
        // Do not interpret our consumption of one side as an external price move.
        if self.distorted > 0 && (bid - self.last_bid).abs() < 0.005 {
            mid = self.last_mid;
        } else if self.distorted < 0 && (ask - self.last_ask).abs() < 0.005 {
            mid = self.last_mid;
        } else {
            self.distorted = 0;
            self.halfspread = self
                .halfspread
                .min(((ask - bid) / mid / 2.).clamp(0.0008, 0.0018));
        }
        if m.slot < self.reopen {
            self.read_flow();
        }
        self.risk(m.mark);
        let model = self.model.as_mut().ok_or("not started")?;
        let t = m.slot.saturating_sub(self.start) as f64;
        model.update(
            t,
            mid / self.start_price - 1.,
            self.distorted == 0 && t >= 8.,
        );
        let target = model
            .decide(
                m.position.size,
                self.halfspread,
                self.dd,
                m.mark,
                self.start_price,
            )
            .clamp(-m.cap, m.cap);
        let diff = target - m.position.size;
        if diff.abs() > 3. && self.dd < 850.
            || (m.slot >= self.reopen && m.position.size.abs() > 0.1)
            || self.dd >= 850.
        {
            let diff = if self.dd >= 850. {
                -m.position.size
            } else {
                diff
            };
            // A late change in the inferred opening price must be executable
            // before the gap. Every individual transaction remains a five-share IOC.
            let volume = diff.abs().min(if m.slot + 10 >= self.reopen {
                140.
            } else {
                25.
            });
            let mut rest = volume;
            while rest > 0.01 {
                let size = rest.min(5.);
                if diff > 0. {
                    o.buy(size, ask + self.start_price * 0.0006);
                } else {
                    o.sell(size, bid - self.start_price * 0.0006);
                }
                rest -= size;
            }
            if volume > 0.01 {
                self.distorted = if diff > 0. { 1 } else { -1 };
            }
        }
        if m.k % 5 == 0 || diff.abs() > 3. {
            log(&format!(
                "STATE {t} mid {mid:.4} mark {:.4} pos {:.2} target {target:.2} dd {:.2} alpha {:.5} trans {:.4} fair {:.4} forecast {:.4} prob {:?}",
                m.mark,
                m.position.size,
                self.dd,
                model.alpha,
                model.transient,
                model.fair,
                model.forecast,
                model.probs
            ));
        }
        self.last_bid = bid;
        self.last_ask = ask;
        self.last_mid = mid;
        Ok(())
    }
}
mtx_kit::export!(Bot);
