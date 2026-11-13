//! Account-attributed SPCX weekend trading.
//!
//! All decisions use this turn's read-only chain state. Model features use
//! percentage price changes; quoted spreads and transaction limits use bps.

use mtx_kit::venue::phoenix_rise::accounts::{spline_collection::SplineCollection, trader::Trader};
use mtx_kit::{log, prelude::*, rpc};
use serde_json::{Value, json};
use std::collections::HashSet;
mod features;
mod model;
use features::State;
use model::Models;
#[derive(Default)]
struct Bot {
    start: f64,
    start_slot: u64,
    reopen: u64,
    address: String,
    authority: String,
    seen: HashSet<String>,
    state: State,
    models: Models,
    previous: f64,
    halted: bool,
    spline: String,
    trader: String,
    peak: f64,
}
impl Bot {
    // The spline midpoint does not move when our orders consume the best quote.
    // Collateral plus virtual quote balance plus marked base position gives equity.
    fn chain_state(&self, mark: f64) -> (Option<f64>, Option<f64>, Option<f64>) {
        let Ok(a) = mtx_kit::rpc_accounts(&[&self.spline, &self.trader]) else {
            return (None, None, None);
        };
        let mut mid = None;
        let mut spread = None;
        let mut eq = None;
        if let Some(Some(b)) = a.first() {
            if let Ok(c) = SplineCollection::try_from_account_bytes(b) {
                let mut centers = Vec::new();
                let mut minb = f64::INFINITY;
                let mut mina = f64::INFINITY;
                for e in c
                    .splines()
                    .filter_map(Result::ok)
                    .filter(|e| e.is_enabled())
                {
                    centers.push(e.mid_price().as_inner() as f64 * 0.01);
                    for r in e.active_bid_regions() {
                        if !r.is_empty() {
                            minb = minb.min(r.start_offset().as_inner() as f64);
                        }
                    }
                    for r in e.active_ask_regions() {
                        if !r.is_empty() {
                            mina = mina.min(r.start_offset().as_inner() as f64);
                        }
                    }
                }
                if !centers.is_empty() {
                    centers.sort_by(f64::total_cmp);
                    mid = Some(centers[centers.len() / 2]);
                }
                if minb.is_finite() && mina.is_finite() {
                    spread = Some((minb + mina) * 0.005 / self.start * 10000.);
                }
            }
        }
        if let Some(Some(b)) = a.get(1) {
            if let Ok(t) = Trader::try_from_account_bytes(b) {
                let mut e = t.header.trader_state.quote_lot_collateral.as_inner() as f64 / 1e6;
                for (id, p) in t.positions() {
                    if id as u32 == 41 {
                        e += p.virtual_quote_lot_position().as_inner() as f64 / 1e6
                            + p.base_lot_position().as_inner() as f64 * 0.001 * mark;
                    }
                }
                eq = Some(e - 10000.);
            }
        }
        (mid, spread, eq)
    }
    // Confirmed commitment exposes the takers immediately, without the 32-slot
    // delay of finalized signature history. Own transactions are excluded.
    fn read_flows(&mut self, m: &Market) -> Vec<(String, f64, f64)> {
        let mut flows = Vec::new();
        let Ok(s) = rpc(
            "getSignaturesForAddress",
            &json!([self.address,{"commitment":"confirmed","limit":64}]).to_string(),
        ) else {
            return flows;
        };
        let Ok(v) = serde_json::from_str::<Value>(&s) else {
            return flows;
        };
        let Some(rows) = v.as_array() else {
            return flows;
        };
        for row in rows.iter().rev() {
            let sig = row["signature"].as_str().unwrap_or("");
            if row["slot"].as_u64().unwrap_or(0) < self.start_slot
                || !self.seen.insert(sig.to_string())
                || !row["err"].is_null()
            {
                continue;
            }
            let params = json!([sig, {"commitment": "confirmed", "encoding": "json", "maxSupportedTransactionVersion": 0}]);
            let Ok(s) = rpc("getTransaction", &params.to_string()) else {
                continue;
            };
            let Ok(tx) = serde_json::from_str::<Value>(&s) else {
                continue;
            };
            let msg = &tx["transaction"]["message"];
            let Some(owner) = msg["accountKeys"][0].as_str() else {
                continue;
            };
            if owner == self.authority {
                continue;
            }
            let Some(ixs) = msg["instructions"].as_array() else {
                continue;
            };
            for ix in ixs {
                let Some(data) = ix["data"]
                    .as_str()
                    .and_then(|s| bs58::decode(s).into_vec().ok())
                else {
                    continue;
                };
                // Phoenix IOC packet: discriminator, kind, side, optional price, base lots.
                if data.len() < 27
                    || data[..8] != [90, 118, 192, 252, 192, 99, 39, 145]
                    || data[8] != 2
                    || data[10] != 1
                {
                    continue;
                }
                let side = if data[9] == 0 { 1. } else { -1. };
                let price = u64::from_le_bytes(data[11..19].try_into().unwrap()) as f64 * 0.01;
                let size = u64::from_le_bytes(data[19..27].try_into().unwrap()) as f64 * 0.001;
                let ratio = (price / self.previous - 1.) * side * 10000. / self.state.spread;
                if size > 0. && size <= 4.01 {
                    flows.push((owner.to_string(), side * size, ratio.clamp(0., 10.)));
                    log(&format!(
                        "F {} {} {} {:.3} {:.3}",
                        m.slot - self.start_slot,
                        owner,
                        side,
                        size,
                        ratio
                    ));
                }
            }
        }
        flows
    }
}
impl Strategy for Bot {
    fn start(&mut self, s: &Session) -> BotResult {
        self.start = s.start_mark;
        self.start_slot = s.start_slot;
        self.reopen = s
            .var("MTX_REOPEN_SLOT")
            .and_then(|x| x.parse().ok())
            .unwrap_or(s.end_slot);
        self.authority = s.authority.clone();
        self.address = mtx_kit::phoenix_market_address(&s.venue, &s.symbol).unwrap_or_default();
        self.spline = serde_json::from_str::<Value>(&s.venue)
            .ok()
            .and_then(|v| {
                v["markets"]
                    .as_array()
                    .and_then(|a| a.iter().find(|m| m["symbol"] == s.symbol))
                    .and_then(|m| m["splinePubkey"].as_str())
                    .map(str::to_string)
            })
            .unwrap_or_default();
        if let Ok(v) = mtx_kit::venue::PhoenixVenue::from_json(&s.venue) {
            if let Ok(k) = mtx_kit::venue::key(&s.authority) {
                self.trader = v.trader_account(&k).to_string();
            }
        }
        self.models = Models::load();
        self.state = State::new((self.reopen - self.start_slot) as f64, 12.);
        self.previous = s.start_mark;
        Ok(())
    }
    fn step(&mut self, m: &Market, orders: &mut Orders) -> BotResult {
        let Some((bid, ask)) = m.best_bid.zip(m.best_ask) else {
            return Ok(());
        };
        let (center, spread, eq) = self.chain_state(m.mark);
        let mid = center.unwrap_or((bid + ask) * 0.5);
        let equity = eq.unwrap_or(m.pnl);
        self.peak = self.peak.max(equity);
        let dd = self.peak - equity;
        if m.slot >= self.reopen {
            return Ok(());
        }
        if self.state.y.is_none() {
            self.state.spread = spread
                .unwrap_or((ask - bid) / self.start * 5000. - 1.)
                .clamp(8., 18.);
        }
        let flows = self.read_flows(m);
        let t = (m.slot - self.start_slot) as f64;
        let y = (mid / self.start - 1.) * 100.;
        self.state.update(t, y, &flows);
        self.previous = mid;
        let f = self.state.features();
        let p = self.models.predict(&f);
        // Blend two horizons to follow continuing flow and anticipate its unwind.
        let signal = 0.875 * p[1] + 0.125 * p[2];
        let mut cap = m.cap.min(69.);
        let current = m.position.size / cap;
        let scale = 0.20;
        let cost = (self.state.spread * 0.01 + 0.045) * 2.0;
        // A cost-aware no-trade region: retain inventory until the forecast pays
        // for changing it, rather than crossing the spread on every small update.
        let delta = signal - scale * current;
        let mut target = if delta.abs() <= cost {
            m.position.size
        } else {
            ((signal - cost * delta.signum()) / scale).clamp(-1., 1.) * cap
        };
        // Leave room for costs and a reopening jump before the hard desk limit.
        if dd > 600. {
            self.halted = true;
        }
        if self.halted {
            target = 0.;
        }
        // Bound the desired position's opening-gap loss using a 14% envelope
        // around the external close and reserve $100 for execution and rounding.
        // Apply this after the cost filter so retained inventory is also reduced.
        if self.reopen - m.slot <= 10 && target.abs() > 0.01 {
            let worst = (self.start * 0.14 + target.signum() * (m.mark - self.start))
                .max(self.start * 0.07);
            cap = cap.min(((900. - dd).max(0.) / worst - 1.).max(0.));
        }
        target = target.clamp(-cap, cap);
        if (target - m.position.size).abs() > 15.
            || m.position.size.abs() > cap + 0.5
            || self.halted && m.position.size.abs() > 0.5
        {
            // Small independent transactions stay within Phoenix's compute budget.
            orders.target_position_with(
                target,
                Execution {
                    max_per_step: 70.,
                    slice: 5.,
                    slippage_bps: 1200.,
                    min_trade: 1.,
                },
            );
        }
        log(&format!(
            "S {:.0} {:.5} {:.3} {:.2} {:.3} {:.3} {:.3} {:.3} target {:.2}",
            t, mid, m.position.size, m.pnl, p[0], p[1], p[2], p[3], target
        ));
        if m.k % 40 == 0 {
            log(&format!(
                "FEATURE {} eq {:.3} dd {:.3} spread {:.3}",
                json!(f),
                equity,
                dd,
                self.state.spread
            ));
        }
        Ok(())
    }
}
mtx_kit::export!(Bot);
