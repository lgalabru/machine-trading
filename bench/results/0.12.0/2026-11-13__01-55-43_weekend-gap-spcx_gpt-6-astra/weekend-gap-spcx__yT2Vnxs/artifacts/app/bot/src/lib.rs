//! SPCX weekend order-flow strategy. Uses only confirmed public transactions.
mod model;
use model::{Model, Trade};
use mtx_kit::venue::{PhoenixVenue, phoenix_rise};
use mtx_kit::{log, prelude::*, rpc, rpc_account};
use serde_json::{Value, json};
use std::collections::HashSet;
#[derive(Default)]
struct Bot {
    model: Option<Model>,
    address: String,
    authority: String,
    trader: String,
    actors: Vec<String>,
    seen: HashSet<String>,
    start: u64,
    base: f64,
    lot: f64,
    tick: f64,
    peak: f64,
    last_p: f64,
    stopped: bool,
}
impl Bot {
    fn read_flow(&mut self, t: u64) {
        let Ok(a) = rpc(
            "getSignaturesForAddress",
            &json!([self.address,{"commitment":"confirmed","limit":80}]).to_string(),
        ) else {
            return;
        };
        let Ok(v) = serde_json::from_str::<Value>(&a) else {
            return;
        };
        let Some(arr) = v.as_array() else { return };
        for s in arr.iter().rev() {
            let sig = s["signature"].as_str().unwrap_or("");
            let slot = s["slot"].as_u64().unwrap_or(0);
            if slot < self.start || !s["err"].is_null() || self.seen.contains(sig) {
                continue;
            }
            let Ok(tx)=rpc("getTransaction",&json!([sig,{"encoding":"json","commitment":"confirmed","maxSupportedTransactionVersion":0}]).to_string())else{continue};
            let Ok(tx) = serde_json::from_str::<Value>(&tx) else {
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
            let authority = keys.first().and_then(Value::as_str).unwrap_or("");
            if authority == self.authority {
                continue;
            }
            let Some(ixs) = msg["instructions"].as_array() else {
                continue;
            };
            for ix in ixs {
                let Some(pid) = ix["programIdIndex"].as_u64() else {
                    continue;
                };
                if keys.get(pid as usize).and_then(Value::as_str)
                    != Some("EtrnLzgbS7nMMy5fbD42kXiUzGg8XQzJ972Xtk1cjWih")
                {
                    continue;
                }
                let Some(data) = ix["data"].as_str() else {
                    continue;
                };
                let Ok(d) = bs58::decode(data).into_vec() else {
                    continue;
                };
                let disc = phoenix_rise::ix::PhoenixInstruction::PlaceMarketOrder.discriminant();
                if d.len() < 27 || d[..8] != disc || d[8] != 2 || d[9] > 1 || d[10] > 1 {
                    continue;
                }
                let off = if d[10] == 1 { 19 } else { 11 };
                let Some(bytes) = d.get(off..off + 8) else {
                    continue;
                };
                let Ok(bytes) = <[u8; 8]>::try_from(bytes) else {
                    continue;
                };
                let size =
                    u64::from_le_bytes(bytes) as f64 / self.lot * if d[9] == 0 { 1. } else { -1. };
                if size.abs() > 4.1 || size.abs() < 0.05 {
                    continue;
                }
                let actor = if let Some(i) = self.actors.iter().position(|k| k == authority) {
                    i
                } else if self.actors.len() < 8 {
                    self.actors.push(authority.to_string());
                    self.actors.len() - 1
                } else {
                    continue;
                };
                let tr = Trade {
                    t: slot.saturating_sub(self.start).min(t),
                    actor,
                    size,
                    price: self.last_p,
                };
                if let Some(m) = self.model.as_mut() {
                    m.trade(tr);
                }
                log(&format!("flow {} {} {:.3}", t, actor, size));
            }
        }
    }
    fn equity(&self, m: &Market) -> Option<f64> {
        let data = rpc_account(&self.trader).ok()??;
        let tr = phoenix_rise::accounts::trader::Trader::try_from_account_bytes(&data).ok()?;
        let collateral = tr.header().state().quote_lot_collateral.as_inner() as f64 / 1e6;
        Some(collateral + m.position.size * (m.mark - m.position.entry_usd))
    }
}
impl Strategy for Bot {
    fn start(&mut self, s: &Session) -> BotResult {
        self.address = s
            .venues
            .first()
            .and_then(|v| v.address.clone())
            .unwrap_or_default();
        self.authority = s.authority.clone();
        self.start = s.start_slot;
        self.base = s.start_mark;
        self.peak = 10000.;
        let venue = PhoenixVenue::from_json(&s.venue)?;
        let cfg = venue.market(&s.symbol)?;
        self.lot = 10f64.powi(cfg.base_lots_decimals as i32);
        self.tick = venue.ticks_to_usd(cfg, 1);
        self.trader = venue.trader_account(&s.authority.parse()?).to_string();
        let reopen = s
            .var("MTX_REOPEN_SLOT")
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(s.end_slot)
            .saturating_sub(s.start_slot);
        self.model = Some(Model::new(reopen));
        log(&format!("start {:.4} reopen {}", self.base, reopen));
        Ok(())
    }
    fn step(&mut self, m: &Market, o: &mut Orders) -> BotResult {
        let t = m.slot.saturating_sub(self.start);
        let Some((bid, ask)) = m.best_bid.zip(m.best_ask) else {
            return Ok(());
        };
        if !bid.is_finite() || !ask.is_finite() || bid <= 0. || ask < bid {
            return Ok(());
        }
        let mid = (bid + ask) * 0.5;
        let p = mid / self.base - 1.;
        let eq = self.equity(m).unwrap_or(10000. + m.pnl);
        self.peak = self.peak.max(eq);
        let dd = self.peak - eq;
        if dd > 680. {
            self.stopped = true;
        }
        if self.model.as_ref().is_some_and(|model| t >= model.reopen) {
            return Ok(());
        }
        self.read_flow(t);
        let model = self.model.as_mut().unwrap();
        model.update(t, p);
        self.last_p = p;
        let mut target = model.target(m.position.size, dd, self.base, m.mark);
        if self.stopped {
            target = 0.;
        }
        target = target.clamp(-m.cap + 0.01, m.cap - 0.01);
        let delta = target - m.position.size;
        let threshold = if model.reopen.saturating_sub(t) <= 8 {
            1.
        } else {
            8.
        };
        if delta.abs() > threshold {
            let mut remaining = delta.abs().min(140.);
            let limit = if delta > 0. { ask * 1.004 } else { bid * 0.996 };
            for _ in 0..28 {
                let amount = remaining.min(5.);
                if amount < 0.1 {
                    break;
                }
                if delta > 0. {
                    o.buy(amount, limit);
                } else {
                    o.sell(amount, limit);
                }
                remaining -= amount;
            }
        }
        if m.k % 8 == 0 || delta.abs() > threshold {
            log(&format!(
                "state {} mid {:.4} pos {:.1} target {:.1} eq {:.2} dd {:.1} edge {:.4} fair {:.4} temp {:.4} info {:?}",
                t,
                mid,
                m.position.size,
                target,
                eq,
                dd,
                model.edge,
                model.fundamental,
                model.temporary,
                model.info
            ));
        }
        Ok(())
    }
}
mtx_kit::export!(Bot);
