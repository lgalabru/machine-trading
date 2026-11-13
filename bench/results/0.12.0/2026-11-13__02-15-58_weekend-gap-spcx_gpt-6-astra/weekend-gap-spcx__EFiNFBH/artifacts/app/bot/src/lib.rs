//! SPCX weekend strategy. All observations come from the current session's chain.
use mtx_kit::venue::{PhoenixVenue, phoenix_rise};
use mtx_kit::{log, prelude::*, rpc, rpc_account};
use serde_json::{Value, json};
use std::collections::HashSet;
mod features;
use features::{Features, predict};
// Forecasts: return over 100 slots (including a possible reopen), return
// to the reopen, and return over 40 slots capped at the closed period's end.
const MODELS: [&[u8]; 3] = [
    include_bytes!("model1.bin"),
    include_bytes!("model2.bin"),
    include_bytes!("model3.bin"),
];
#[derive(Default)]
struct Bot {
    start: u64,
    reopen: u64,
    start_price: f64,
    address: String,
    authority: String,
    trader: String,
    lots: f64,
    tick: f64,
    seen: HashSet<String>,
    features: Features,
    target: f64,
    peak: f64,
    stopped: bool,
    halfspread: f64,
    smooth: f64,
}
impl Bot {
    // Confirmed history is visible without the 32-slot finalized delay.
    fn flow(&mut self, m: &Market) -> Vec<(usize, f64, f64)> {
        let mut flow = vec![];
        let Ok(raw) = rpc(
            "getSignaturesForAddress",
            &json!([self.address,{"commitment":"confirmed","limit":64}]).to_string(),
        ) else {
            return flow;
        };
        let Ok(list) = serde_json::from_str::<Value>(&raw) else {
            return flow;
        };
        let Some(list) = list.as_array() else {
            return flow;
        };
        for item in list.iter().rev() {
            let slot = item["slot"].as_u64().unwrap_or(0);
            if slot < self.start {
                continue;
            }
            let Some(sig) = item["signature"].as_str() else {
                continue;
            };
            if self.seen.contains(sig) {
                continue;
            }
            let Ok(raw)=rpc("getTransaction",&json!([sig,{"encoding":"json","commitment":"confirmed","maxSupportedTransactionVersion":0}]).to_string()) else {continue};
            let Ok(tx) = serde_json::from_str::<Value>(&raw) else {
                continue;
            };
            if tx.is_null() {
                continue;
            }
            self.seen.insert(sig.to_string());
            if !tx["meta"]["err"].is_null() {
                continue;
            }
            let msg = &tx["transaction"]["message"];
            let Some(keys) = msg["accountKeys"].as_array() else {
                continue;
            };
            let who = keys.first().and_then(|x| x.as_str()).unwrap_or("");
            if who.is_empty() || who == self.authority {
                continue;
            }
            let Some(ixs) = msg["instructions"].as_array() else {
                continue;
            };
            for ix in ixs {
                let Some(encoded) = ix["data"].as_str() else {
                    continue;
                };
                let Ok(data) = bs58::decode(encoded).into_vec() else {
                    continue;
                };
                // Phoenix IOC packet: discriminator, kind, side, optional limit, lots.
                let disc = phoenix_rise::ix::PhoenixInstruction::PlaceMarketOrder.discriminant();
                if data.len() < 27 || data[..8] != disc || data[8] != 2 || data[10] != 1 {
                    continue;
                }
                let side = if data[9] == 0 {
                    1.
                } else if data[9] == 1 {
                    -1.
                } else {
                    continue;
                };
                let price = u64::from_le_bytes(data[11..19].try_into().unwrap()) as f64 * self.tick;
                let size = u64::from_le_bytes(data[19..27].try_into().unwrap()) as f64 / self.lots;
                if size <= 0. || size > 4.001 {
                    continue;
                }
                let a = if let Some(a) = self.features.actors.iter().position(|a| a.key == who) {
                    a
                } else if let Some(a) = self.features.actors.iter().position(|a| a.key.is_empty()) {
                    self.features.actors[a].key = who.to_string();
                    a
                } else {
                    continue;
                };
                let i = ((slot - self.start) / 2) as usize;
                let p = self
                    .features
                    .prices
                    .get(i)
                    .copied()
                    .unwrap_or_else(|| *self.features.prices.last().unwrap_or(&0.));
                let prior = self.start_price * (1. + p / 100.);
                let ratio = (side * (price - prior) / prior * 10000. / self.halfspread.max(8.))
                    .clamp(1., 7.);
                flow.push((a, side * size, ratio));
                log(&format!(
                    "FLOW {} {} {:0.3} {:0.3}",
                    m.slot - self.start,
                    a,
                    side * size,
                    ratio
                ));
            }
        }
        flow
    }
    // Collateral plus the signed virtual quote balance and marked base lots.
    // Wallet USDC stays constant and therefore cancels out of drawdown.
    fn equity(&self, m: &Market) -> Option<f64> {
        let data = rpc_account(&self.trader).ok()??;
        let t = phoenix_rise::accounts::trader::Trader::try_from_account_bytes(&data).ok()?;
        let mut eq = t.header.trader_state.quote_lot_collateral.as_inner() as f64 / 1e6;
        for (_, p) in t.positions() {
            eq += p.virtual_quote_lot_position().as_inner() as f64 / 1e6
                + p.base_lot_position().as_inner() as f64 / self.lots * m.mark;
        }
        Some(eq)
    }
}
impl Strategy for Bot {
    fn start(&mut self, s: &Session) -> BotResult {
        self.start = s.start_slot;
        self.reopen = s
            .var("MTX_REOPEN_SLOT")
            .and_then(|s| s.parse().ok())
            .unwrap_or(s.end_slot);
        self.start_price = s.start_mark;
        self.authority = s.authority.clone();
        self.peak = 10000.;
        self.halfspread = 100.;
        let venue = PhoenixVenue::from_json(&s.venue)?;
        let m = venue.market(&s.symbol)?;
        self.address = m.market_pubkey.clone();
        self.lots = 10f64.powi(m.base_lots_decimals as i32);
        self.tick = m.tick_size as f64 * 10f64.powi(m.base_lots_decimals as i32 - 6);
        self.trader = venue
            .trader_account(&mtx_kit::venue::key(&s.authority)?)
            .to_string();
        log(&format!(
            "START {} {} {:0.4}",
            self.start, self.reopen, self.start_price
        ));
        Ok(())
    }
    fn step(&mut self, m: &Market, o: &mut Orders) -> BotResult {
        let bid = m.best_bid.unwrap_or(m.mark);
        let ask = m.best_ask.unwrap_or(m.mark);
        let mid = (bid + ask) * 0.5;
        if !(mid > 0. && m.mark > 0.) {
            return Ok(());
        }
        self.halfspread = self.halfspread.min(
            ((ask - bid - self.tick).max(self.tick) * 0.5 / self.start_price * 10000.).max(8.),
        );
        let equity = self.equity(m).unwrap_or(10000. + m.pnl - 50.);
        self.peak = self.peak.max(equity);
        let dd = self.peak - equity;
        let liquidation_equity =
            equity + m.position.size * (mid - m.mark) - m.position.size.abs() * mid * 0.002;
        if dd > m.max_drawdown * 0.6 || self.peak - liquidation_equity > m.max_drawdown * 0.7 {
            self.stopped = true;
        }
        let t = (m.slot - self.start) as f64;
        let left = self.reopen.saturating_sub(m.slot) as f64;
        let mut forecasts = [0.; 3];
        if m.slot < self.reopen {
            let flow = self.flow(m);
            self.features
                .update(t, 100. * (mid / self.start_price - 1.), &flow);
            if t >= 16. && m.k % 2 == 1 {
                let (f, sign) = self.features.vector(t, (self.reopen - self.start) as f64);
                for j in 0..3 {
                    forecasts[j] = predict(MODELS[j], &f) * sign;
                }
                self.smooth += 0.5 * (forecasts[2] - self.smooth);
                let signal = if left <= 20. {
                    0.5 * (forecasts[0] + forecasts[1])
                } else {
                    self.smooth
                };
                if signal.abs() > 0.15 {
                    self.target = signal.signum() * m.cap.min(70.);
                } else if signal * self.target.signum() < -0.1 {
                    self.target = 0.;
                }
                log(&format!(
                    "PRED {} {:0.4} {:0.4} {:0.4} target {:0.1} equity {:0.2} dd {:0.2}",
                    t, forecasts[2], forecasts[0], forecasts[1], self.target, equity, dd
                ));
            }
            // Reduce before the discontinuous index reset, allowing several execution turns.
            if left < 24. {
                let worst_move =
                    self.start_price * 0.145 + self.target.signum() * (m.mark - self.start_price);
                let safe = ((m.max_drawdown * 0.8 - dd).max(0.)
                    / worst_move.max(self.start_price * 0.05))
                .min(m.cap);
                self.target = self.target.clamp(-safe, safe);
            }
        } else {
            self.target = m.position.size;
        }
        if self.stopped {
            self.target = 0.;
        }
        if (self.target - m.position.size).abs() > if left < 24. { 0.5 } else { 7. } {
            o.target_position_with(
                self.target,
                Execution {
                    max_per_step: 30.,
                    slice: 5.,
                    slippage_bps: ((mid / m.mark - 1.).abs() * 10000. + 100.).max(100.),
                    min_trade: 0.1,
                },
            );
        }
        log(&format!(
            "M {} {:0.5} {:0.5} {:0.2} {:0.2} {:0.2}",
            t,
            mid,
            m.mark,
            m.position.size,
            equity - 10000.,
            self.target
        ));
        Ok(())
    }
}
mtx_kit::export!(Bot);
