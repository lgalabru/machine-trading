//! Weekend SPCX flow forecasting strategy. All inputs are current chain reads.
use mtx_kit::{log, prelude::*, rpc};
use serde_json::{Value, json};
mod features;
mod model;
use features::State;
use model::Model;
#[derive(Default)]
struct Bot {
    start: f64,
    start_slot: u64,
    reopen: u64,
    address: String,
    authority: String,
    cursor: String,
    state: State,
    models: Vec<Model>,
    classifiers: Vec<Model>,
    last_mid: f64,
    spread: f64,
    target: f64,
    initial_anchor: f64,
    anchor_moved: bool,
    spline: String,
    trader: String,
    peak_equity: f64,
    last_mark: f64,
    halted: bool,
}
impl Bot {
    fn history(&mut self, slot: u64) -> Vec<(String, f64, f64)> {
        let mut trades = Vec::new();
        let mut cfg = json!({"commitment":"confirmed","limit":64});
        if !self.cursor.is_empty() {
            cfg["until"] = json!(self.cursor);
        }
        let Ok(s) = rpc(
            "getSignaturesForAddress",
            &json!([self.address, cfg]).to_string(),
        ) else {
            return trades;
        };
        let Ok(v) = serde_json::from_str::<Value>(&s) else {
            return trades;
        };
        let Some(a) = v.as_array() else { return trades };
        if let Some(sig) = a.first().and_then(|x| x["signature"].as_str()) {
            self.cursor = sig.to_string();
        }
        for x in a.iter().rev() {
            if !x["err"].is_null() || x["slot"].as_u64().unwrap_or(0) < self.start_slot {
                continue;
            }
            let Some(sig) = x["signature"].as_str() else {
                continue;
            };
            let Ok(t)=rpc("getTransaction",&json!([sig,{"commitment":"confirmed","encoding":"json","maxSupportedTransactionVersion":0}]).to_string())else{continue};
            let Ok(t) = serde_json::from_str::<Value>(&t) else {
                continue;
            };
            if !t["meta"]["err"].is_null() {
                continue;
            }
            let msg = &t["transaction"]["message"];
            let who = msg["accountKeys"][0].as_str().unwrap_or("");
            if who == self.authority || who.is_empty() {
                continue;
            }
            let Some(ixs) = msg["instructions"].as_array() else {
                continue;
            };
            for ix in ixs {
                let Some(enc) = ix["data"].as_str() else {
                    continue;
                };
                let Ok(d) = bs58::decode(enc).into_vec() else {
                    continue;
                };
                let disc = mtx_kit::venue::phoenix_rise::ix::PhoenixInstruction::PlaceMarketOrder
                    .discriminant();
                if d.len() < 20 || d[..8] != disc || d[8] != 2 {
                    continue;
                }
                let side = if d[9] == 0 { 1. } else { -1. };
                let p = if d[10] == 1 { 19 } else { 11 };
                if d.len() < p + 8 {
                    continue;
                }
                let sz = u64::from_le_bytes(d[p..p + 8].try_into().unwrap()) as f64 / 1000.;
                let limit = if d[10] == 1 {
                    u64::from_le_bytes(d[11..19].try_into().unwrap()) as f64 * 0.01
                } else {
                    self.last_mid
                };
                let aggression =
                    (side * (limit / self.last_mid - 1.) * 10000. / self.spread).clamp(0., 12.);
                trades.push((who.to_string(), side * sz, aggression));
                log(&format!(
                    "TRADE {} {} {} {} {:.5}",
                    slot - self.start_slot,
                    who,
                    side,
                    sz,
                    aggression
                ));
            }
        }
        trades
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
        self.address = mtx_kit::venue::PhoenixVenue::from_json(&s.venue)?
            .market(&s.symbol)?
            .market_pubkey
            .clone();
        self.state = State::new(self.reopen - self.start_slot);
        let venue = mtx_kit::venue::PhoenixVenue::from_json(&s.venue)?;
        self.spline = venue.market(&s.symbol)?.spline_pubkey.clone();
        self.trader = venue
            .trader_account(&mtx_kit::venue::key(&s.authority)?)
            .to_string();
        self.peak_equity = 10000.;
        self.models = vec![
            Model::load(include_bytes!("h30.bin")),
            Model::load(include_bytes!("late30.bin")),
            Model::load(include_bytes!("terminal2.bin")),
            Model::load(include_bytes!("termclass2.bin")),
            Model::load(include_bytes!("closed30.bin")),
        ];
        self.classifiers = vec![
            Model::load(include_bytes!("class2-0.bin")),
            Model::load(include_bytes!("class2-1.bin")),
            Model::load(include_bytes!("class2-2.bin")),
            Model::load(include_bytes!("class2-3.bin")),
            Model::load(include_bytes!("class2-4.bin")),
        ];
        log(&format!(
            "START {} {} {} {}",
            self.start, self.start_slot, self.reopen, s.end_slot
        ));
        Ok(())
    }
    fn step(&mut self, m: &Market, o: &mut Orders) -> BotResult {
        let mut mid = m
            .best_bid
            .zip(m.best_ask)
            .map(|(b, a)| (a + b) / 2.)
            .unwrap_or(m.mark);
        if !mid.is_finite() || mid <= 0. {
            return Ok(());
        }
        if let Ok(Some(data)) = mtx_kit::rpc_account(&self.spline) {
            if let Ok(collection)=mtx_kit::venue::phoenix_rise::accounts::spline_collection::SplineCollection::try_from_account_bytes(&data) {
    let mut mids:Vec<f64>=collection.iter().filter_map(Result::ok).filter(|s|s.is_enabled()).map(|s|s.mid_price().as_inner() as f64*0.01).collect();mids.sort_by(f64::total_cmp);
    if !mids.is_empty(){
     let anchor=mids[mids.len()/2];if self.initial_anchor==0.{self.initial_anchor=anchor;}
     if (anchor-self.initial_anchor).abs()>0.001{self.anchor_moved=true;}
     let book_mid=mid;mid=if self.anchor_moved{anchor+0.005}else{self.start};
     log(&format!("SPLINE {} {:.6} {:.6}",m.slot-self.start_slot,book_mid,mid));
    }
   }
        }
        if self.last_mid == 0. {
            self.last_mid = mid;
            self.spread = m
                .best_bid
                .zip(m.best_ask)
                .map(|(b, a)| ((a - b) / 2. - 0.015) / self.start * 10000.)
                .unwrap_or(12.)
                .clamp(7., 18.);
        }
        if m.slot >= self.reopen {
            return Ok(());
        }
        let trades = self.history(m.slot);
        let t = m.slot - self.start_slot;
        let y = mid / self.start - 1.;
        self.state.update(t, y, trades);
        let (mut x, orient) = self.state.features(t, y, self.spread);
        let old = (0.25 * self.models[0].predict(&x) + 0.75 * self.models[4].predict(&x)) * orient;
        let left = (self.reopen - m.slot) as f64;
        let mut signal = old;
        let mut late = old;
        let mut fundamental = old;
        let mut confidence = 0.;
        if left < 100. {
            late = self.models[1].predict(&x) * orient;
            let logits: Vec<f64> = self.classifiers.iter().map(|c| c.predict(&x)).collect();
            let max = logits.iter().copied().fold(f64::NEG_INFINITY, f64::max);
            let exp: Vec<f64> = logits.iter().map(|z| (z - max).exp()).collect();
            let total = exp.iter().sum::<f64>();
            let probs: Vec<f32> = exp.iter().map(|z| (z / total) as f32).collect();
            confidence = probs.iter().copied().fold(0., f32::max) as f64;
            x.extend(probs);
            fundamental = self.models[3].predict(&x) * orient;
            let weight = ((100. - left) / 40.).clamp(0., 1.) * 0.5;
            signal = (1. - weight) * old + weight * late;
            let terminal_weight = ((40. - left) / 30.).clamp(0., 1.) * 0.5;
            signal = (1. - terminal_weight) * signal + terminal_weight * fundamental;
        }
        let cap = m.cap.min(69.);
        if signal > 0.12 {
            self.target = cap;
        } else if signal < -0.12 {
            self.target = -cap;
        } else if self.target > 0. && signal < -0.08 || self.target < 0. && signal > 0.08 {
            self.target = 0.;
        }
        // Reserve room for losses already incurred, including a prudent buffer.

        if m.drawdown > 600. {
            self.target = 0.;
        }
        let mut dd = m.drawdown;
        if let Ok(Some(data)) = mtx_kit::rpc_account(&self.trader) {
            if let Ok(trader) =
                mtx_kit::venue::phoenix_rise::accounts::trader::Trader::try_from_account_bytes(
                    &data,
                )
            {
                let collateral =
                    trader.header.trader_state.quote_lot_collateral.as_inner() as f64 / 1e6;
                let equity = collateral + m.position.size * (m.mark - m.position.entry_usd);
                if self.last_mark > 0. {
                    self.peak_equity = self.peak_equity.max(
                        collateral + m.position.size * (self.last_mark - m.position.entry_usd),
                    );
                }
                self.peak_equity = self.peak_equity.max(equity);
                dd = (self.peak_equity - equity).max(0.);
                if m.slot + 8 >= self.reopen {
                    let book_equity = equity + m.position.size * (mid - m.mark);
                    let effective_peak = self.peak_equity.max(book_equity);
                    let budget = (900. - (effective_peak - book_equity)).max(0.);
                    let opposite_flow = self.state.accounts.values().any(|a| {
                        let net = a.iter().map(|x| x.z).sum::<f64>();
                        let gross = a.iter().map(|x| x.z.abs()).sum::<f64>();
                        a.len() >= 3 && net.abs() > 0.95 * gross && net * self.target < 0.
                    });
                    let bound = if opposite_flow { 0.14 } else { 0.07 };
                    let adverse = if self.target >= 0. {
                        mid - self.start * (1. - bound)
                    } else {
                        self.start * (1. + bound) - mid
                    };
                    let max_position =
                        (budget / adverse.max(self.start * 0.01)).min(m.cap.min(60.));
                    self.target = self.target.clamp(-max_position, max_position);
                }
                log(&format!(
                    "RISK {:.4} {:.4} {:.4}",
                    equity, self.peak_equity, dd
                ));
            }
        }
        self.last_mark = m.mark;
        if dd > 600. {
            self.halted = true;
        }
        if self.halted {
            self.target = 0.;
        }
        let diff = self.target - m.position.size;
        if diff.abs() > 2. {
            o.cancel_all();
            let mut remaining = diff.abs();
            for _ in 0..((remaining / 5.).ceil() as usize).min(28) {
                let size = remaining.min(5.);
                remaining -= size;
                if diff > 0. {
                    o.buy(size, mid * 1.008);
                } else {
                    o.sell(size, mid * 0.992);
                }
            }
        }
        log(&format!(
            "STATE {} {:.6} {:.6} {:.3} {:.3} {:.3} {:.3} PRED {:.5} {:.5} {:.5} {:.5}",
            t,
            mid,
            m.mark,
            m.position.size,
            self.target,
            m.pnl,
            m.drawdown,
            signal,
            late,
            fundamental,
            confidence
        ));
        self.last_mid = mid;
        Ok(())
    }
}
mtx_kit::export!(Bot);

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn features_and_forests_match_training_runtime() {
        let fixture: Value = serde_json::from_str(include_str!("fixture.json")).unwrap();
        let mut state = State::new(500);
        for r in fixture["records"].as_array().unwrap() {
            let t = r[0].as_u64().unwrap();
            let y = r[1].as_f64().unwrap();
            let trades = r[2]
                .as_array()
                .unwrap()
                .iter()
                .map(|x| {
                    (
                        x[0].as_str().unwrap().to_string(),
                        x[1].as_f64().unwrap(),
                        x[2].as_f64().unwrap(),
                    )
                })
                .collect();
            state.update(t, y, trades);
        }
        let last = fixture["records"].as_array().unwrap().last().unwrap();
        let (x, o) = state.features(400, last[1].as_f64().unwrap(), 12.);
        assert_eq!(o, fixture["orient"].as_f64().unwrap());
        assert_eq!(x.len(), 162);
        for (i, (&got, want)) in x
            .iter()
            .zip(fixture["features"].as_array().unwrap())
            .enumerate()
        {
            assert!(
                (got as f64 - want.as_f64().unwrap()).abs() < 0.0001,
                "feature {i}: {got} vs {want}"
            );
        }
        for (name, b) in [
            ("h30", include_bytes!("h30.bin").as_slice()),
            ("late30", include_bytes!("late30.bin").as_slice()),
        ] {
            let got = Model::load(b).predict(&x);
            assert!(
                (got - fixture[name].as_f64().unwrap()).abs() < 0.00001,
                "forest {name}: {got}"
            );
        }
        let mut xp = x.clone();
        let classifiers = [
            include_bytes!("class2-0.bin").as_slice(),
            include_bytes!("class2-1.bin").as_slice(),
            include_bytes!("class2-2.bin").as_slice(),
            include_bytes!("class2-3.bin").as_slice(),
            include_bytes!("class2-4.bin").as_slice(),
        ];
        let logits: Vec<f64> = classifiers
            .iter()
            .map(|b| Model::load(b).predict(&x))
            .collect();
        let max = logits.iter().copied().fold(f64::NEG_INFINITY, f64::max);
        let exp: Vec<_> = logits.iter().map(|v| (v - max).exp()).collect();
        let total = exp.iter().sum::<f64>();
        for (i, e) in exp.iter().enumerate() {
            let prob = (e / total) as f32;
            assert!((prob as f64 - fixture["classes"][i].as_f64().unwrap()).abs() < 0.00001);
            xp.push(prob);
        }
        let pred = Model::load(include_bytes!("termclass2.bin")).predict(&xp);
        assert!((pred - fixture["termclass2"].as_f64().unwrap()).abs() < 0.00001);
    }
}
