//! SPCX weekend flow inference, directional execution, and reopening risk limits.
//! All observations are read from the current turn; no scenario data is embedded.

use mtx_kit::{log, prelude::*, venue::PhoenixVenue};
use serde_json::{Value, json};
use std::collections::HashSet;
mod model;
use model::Filter;
use mtx_kit::venue::phoenix_rise::accounts::trader::TraderHeader;

#[derive(Default)]
struct Bot {
    market: String,
    authority: String,
    cursor: String,
    start: u64,
    reopen: u64,
    p0: f64,
    lot_scale: f64,
    tick_scale: f64,
    seen: HashSet<String>,
    names: Vec<String>,
    filter: Filter,
    flows: [f64; 8],
    qvar: [f64; 8],
    target: f64,
    trader: String,
    equity_peak: f64,
    equity_dd: f64,
    halted: bool,
    min_y: f64,
    max_y: f64,
    feed_ok: bool,
}
impl Bot {
    fn read_flow(&mut self, m: &Market) {
        let mut cfg = json!({"commitment":"confirmed","limit":100});
        if !self.cursor.is_empty() {
            cfg["until"] = json!(self.cursor);
        }
        let Ok(s) = rpc(
            "getSignaturesForAddress",
            &json!([self.market, cfg]).to_string(),
        ) else {
            self.feed_ok = false;
            return;
        };
        let Ok(v) = serde_json::from_str::<Value>(&s) else {
            self.feed_ok = false;
            return;
        };
        let Some(a) = v.as_array() else {
            self.feed_ok = false;
            log(&format!("SIGERR {s}"));
            return;
        };
        if let Some(s) = a.first().and_then(|x| x["signature"].as_str()) {
            self.cursor = s.into();
        }
        for x in a.iter().rev() {
            let Some(sig) = x["signature"].as_str() else {
                continue;
            };
            if !self.seen.insert(sig.into()) || !x["err"].is_null() {
                continue;
            }
            if m.sends
                .iter()
                .any(|s| matches!(&s.state,SendState::Landed(t) if t==sig))
            {
                continue;
            }
            let slot = x["slot"].as_u64().unwrap_or(0);
            if slot <= self.start {
                continue;
            }
            let Ok(s)=rpc("getTransaction",&json!([sig,{"encoding":"json","commitment":"confirmed","maxSupportedTransactionVersion":0}]).to_string())else{self.feed_ok=false;continue};
            let Ok(v) = serde_json::from_str::<Value>(&s) else {
                self.feed_ok = false;
                continue;
            };
            let msg = &v["transaction"]["message"];
            let Some(ixs) = msg["instructions"].as_array() else {
                self.feed_ok = false;
                log(&format!("TXERR {s}"));
                continue;
            };
            for ix in ixs {
                let Some(data) = ix["data"]
                    .as_str()
                    .and_then(|s| bs58::decode(s).into_vec().ok())
                else {
                    continue;
                };
                if data.len() < 19
                    || data[..8]
                        != mtx_kit::venue::phoenix_rise::ix::PhoenixInstruction::PlaceMarketOrder
                            .discriminant()
                    || data[8] != 2
                {
                    continue;
                }
                let actor = ix["accounts"][3]
                    .as_u64()
                    .and_then(|i| msg["accountKeys"][i as usize].as_str())
                    .unwrap_or("");
                if actor.is_empty() {
                    self.feed_ok = false;
                    continue;
                }
                if actor == self.authority {
                    continue;
                }
                let off = if data[10] == 1 { 19 } else { 11 };
                if data.len() < off + 8 {
                    continue;
                }
                let lots = u64::from_le_bytes(data[off..off + 8].try_into().unwrap());
                let size = lots as f64 / self.lot_scale * if data[9] == 0 { 1.0 } else { -1.0 };
                let limit = if data[10] == 1 {
                    u64::from_le_bytes(data[11..19].try_into().unwrap()) as f64 * self.tick_scale
                } else {
                    0.0
                };
                let id = if let Some(i) = self.names.iter().position(|s| s == actor) {
                    i
                } else {
                    self.names.push(actor.into());
                    self.names.len() - 1
                };
                if id < 8 {
                    self.filter.trade(id, (slot - self.start) as f64, size);
                    self.flows[id] += size;
                    self.qvar[id] += size * size;
                } else {
                    self.feed_ok = false;
                }
                log(&format!(
                    "FLOW {} {} {:.4} {:.6}",
                    slot - self.start,
                    id,
                    size,
                    limit
                ));
            }
        }
    }
}
impl Strategy for Bot {
    fn start(&mut self, s: &Session) -> BotResult {
        self.market = mtx_kit::phoenix_market_address(&s.venue, &s.symbol).unwrap_or_default();
        self.feed_ok = true;
        self.authority = s.authority.clone();
        self.start = s.start_slot;
        self.p0 = s.start_mark;
        self.reopen = s
            .var("MTX_REOPEN_SLOT")
            .and_then(|v| v.parse().ok())
            .unwrap_or(s.end_slot);
        let v = PhoenixVenue::from_json(&s.venue)?;
        let c = v.market(&s.symbol)?;
        self.trader = v.trader_account(&s.authority.parse()?).to_string();
        self.equity_peak = 10000.;
        self.lot_scale = 10f64.powi(c.base_lots_decimals as i32);
        self.tick_scale = v.ticks_to_usd(c, 1);
        log(&format!(
            "START {} {} {} {} {}",
            self.start, self.reopen, self.p0, self.lot_scale, self.tick_scale
        ));
        Ok(())
    }
    fn step(&mut self, m: &Market, o: &mut Orders) -> BotResult {
        let t = m.slot - self.start;
        if let Ok(Some(data)) = mtx_kit::rpc_account(&self.trader) {
            if let Ok(header) = TraderHeader::try_read_from_account_bytes(&data) {
                let collateral = header.trader_state.quote_lot_collateral.as_inner() as f64 / 1e6;
                let equity = collateral + m.position.size * (m.mark - m.position.entry_usd);
                self.equity_peak = self.equity_peak.max(equity);
                self.equity_dd = (self.equity_peak - equity).max(0.);
                // The smoothed mark can lag the book. Also reserve for the
                // loss that would actually be realized by exiting now.
                let exit_price = if m.position.size >= 0. {
                    m.best_bid.unwrap_or(m.mark)
                } else {
                    m.best_ask.unwrap_or(m.mark)
                };
                let exit_equity = collateral
                    + m.position.size * (exit_price - m.position.entry_usd)
                    - m.position.size.abs() * exit_price * 0.0005;
                let exit_drawdown = (self.equity_peak - exit_equity).max(0.);
                if self.equity_dd > 650. || exit_drawdown > 650. {
                    self.halted = true;
                }
            }
        }
        let mid = match (m.best_bid, m.best_ask) {
            (Some(b), Some(a)) => (a + b) / 2.,
            _ => m.mark,
        };
        if m.slot < self.reopen {
            self.min_y = self.min_y.min(mid / self.p0 - 1.);
            self.max_y = self.max_y.max(mid / self.p0 - 1.);
            self.read_flow(m);
            if t >= self.filter.last_t as u64 + 6 {
                self.filter.update(
                    t as f64,
                    mid / self.p0 - 1.,
                    (self.reopen - self.start) as f64,
                    self.flows,
                    self.qvar,
                );
                self.flows = [0.; 8];
                self.qvar = [0.; 8];
                let remain = (self.reopen - m.slot) as f64;
                let mut signal = self.filter.future;
                if remain < 40. {
                    signal = (self.filter.opening - (mid / self.p0 - 1.)) * (1. - remain / 70.)
                        + signal * remain / 70.;
                }
                let low = ((signal - 0.0035) / 0.0005).clamp(-1., 1.) * 69.;
                let high = ((signal + 0.0035) / 0.0005).clamp(-1., 1.) * 69.;
                self.target = m.position.size.clamp(low, high);
                log(&format!(
                    "MODEL {} {:.6} {:.6} {:.6} {:.6} {:?} target {:.3}",
                    t,
                    self.filter.permanent,
                    self.filter.temp,
                    self.filter.future,
                    self.filter.opening,
                    self.filter.probs,
                    self.target
                ));
            }
            if self.reopen - m.slot < 20 {
                // Bound adverse reopening news using every possible informed
                // account, independently of the probabilistic classifier.
                // An active opposite-side account retains the full news bound.
                // A stopped account's possible target is bounded by its volume
                // and the entire observed price range. Add a 6-sigma surprise.
                let mut bound = 0.066f64;
                for a in &self.filter.actors {
                    if a.count == 0.
                        || a.invalid
                        || a.first > (self.reopen - self.start) as f64 * 0.60
                        || a.sign * self.target >= 0.
                    {
                        continue;
                    }
                    if t as f64 - a.last < 48. {
                        bound = 0.156;
                        break;
                    }
                    let past = if self.target > 0. {
                        -self.min_y
                    } else {
                        self.max_y
                    };
                    bound =
                        bound.max((a.absvol * 0.003 + 0.01).max(past + 0.015).min(0.12) + 0.036);
                }
                if !self.feed_ok {
                    bound = 0.16;
                }
                let loss = (self.p0 * bound + self.target.signum() * (m.mark - self.p0))
                    .max(self.p0 * 0.025);
                let cap = ((800. - self.equity_dd).max(0.) / loss).min(69.);
                self.target = self.target.clamp(-cap, cap);
            }
        } else {
            self.target = m.position.size;
        }
        if self.halted {
            self.target = 0.;
        }
        let delta = self.target - m.position.size;
        if delta.abs() > 12. || ((m.slot + 20 >= self.reopen || self.halted) && delta.abs() > 0.5) {
            o.target_position_with(
                self.target,
                Execution {
                    max_per_step: 140.,
                    slice: 5.,
                    slippage_bps: 1000.,
                    min_trade: 0.5,
                },
            );
        }
        for status in &m.sends {
            if let Some(reason) = status.state.reason() {
                log(&format!("SENDERR {reason}"));
            }
        }
        log(&format!(
            "OBS {} {:.6} {:.6} {:.6} {:.6} {:.4} {:.4}",
            m.slot - self.start,
            m.mark,
            m.index,
            m.best_bid.unwrap_or(0.0),
            m.best_ask.unwrap_or(0.0),
            m.position.size,
            self.equity_dd
        ));
        Ok(())
    }
}
mtx_kit::export!(Bot);
