//! SPCX weekend strategy. All signals are learned from current on-chain observations.
mod model;
use mtx_kit::{log, rpc, rpc_account, prelude::*, venue::{PhoenixVenue, phoenix_rise}};
use serde_json::{json, Value};
use std::collections::HashSet;
#[derive(Default)]
struct Bot {
    start: u64,
    reopen: u64,
    initial: f64,
    address: String,
    authority: String,
    trader: String,
    tick: f64,
    lot: f64,
    asset: u64,
    seen: HashSet<String>,
    actors: Vec<String>,
    filter: model::Filter,
    previous_mid: f64,
    half_spread: f64,
    peak: f64,
    stopped: bool,
}
/// Maximum exposure whose stressed reopening loss fits within the remaining budget.
/// Both mark equity and liquidation equity matter when the index lags the book.
fn reopen_room(
    initial: f64,
    mark: f64,
    mid: f64,
    position: f64,
    pnl: f64,
    peak: f64,
    sign: f64,
    cap: f64,
) -> f64 {
    let risk_price = if sign > 0. { mid.max(mark) } else { mid.min(mark) };
    let adverse = initial * 0.16 + sign * (risk_price - initial);
    let book_pnl = pnl + position * (mid - mark);
    let stress_dd = (peak - pnl).max(0.).max((peak - book_pnl).max(0.));
    ((850. - stress_dd).max(0.) / adverse.max(initial * 0.09)).min(cap).floor()
}
impl Bot {
    fn read_flow(&mut self, slot: u64, own_orders: bool) -> [f64; 8] {
        let mut flow = [0.; 8];
        let Ok(raw) = rpc(
            "getSignaturesForAddress",
            &json!([self.address, { "commitment" : "confirmed", "limit" : 64 }])
                .to_string(),
        ) else {
            return flow;
        };
        let Ok(v) = serde_json::from_str::<Value>(&raw) else {
            return flow;
        };
        let Some(a) = v.as_array() else {
            return flow;
        };
        for x in a.iter().rev() {
            let Some(sig) = x["signature"].as_str() else {
                continue;
            };
            if x["slot"].as_u64().unwrap_or(0) < self.start
                || !self.seen.insert(sig.to_string())
            {
                continue;
            }
            if !x["err"].is_null() {
                continue;
            }
            let Ok(raw) = rpc(
                "getTransaction",
                &json!(
                    [sig, { "commitment" : "confirmed", "encoding" : "json",
                    "maxSupportedTransactionVersion" : 0 }]
                )
                    .to_string(),
            ) else {
                continue;
            };
            let Ok(tx) = serde_json::from_str::<Value>(&raw) else {
                continue;
            };
            if !tx["meta"]["err"].is_null() {
                continue;
            }
            let msg = &tx["transaction"]["message"];
            let Some(keys) = msg["accountKeys"].as_array() else {
                continue;
            };
            let signer = keys.first().and_then(|x| x.as_str()).unwrap_or("");
            if signer.is_empty() || signer == self.authority {
                continue;
            }
            let Some(ixs) = msg["instructions"].as_array() else {
                continue;
            };
            for ix in ixs {
                let Some(encoded) = ix["data"].as_str() else {
                    continue;
                };
                let Ok(b) = bs58::decode(encoded).into_vec() else {
                    continue;
                };
                let disc = phoenix_rise::ix::PhoenixInstruction::PlaceMarketOrder
                    .discriminant();
                if b.len() < 19 || b[..8] != disc || b[8] != 2 || b[9] > 1 {
                    continue;
                }
                let offset = if b[10] == 1 {
                    19
                } else if b[10] == 0 {
                    11
                } else {
                    continue;
                };
                if b.len() < offset + 8 {
                    continue;
                }
                let sign = if b[9] == 0 { 1. } else { -1. };
                let size = u64::from_le_bytes(
                    b[offset..offset + 8].try_into().unwrap_or([0; 8]),
                ) as f64 * self.lot * sign;
                if size.abs() < 0.001 || size.abs() > 5. {
                    continue;
                }
                let limit = if offset == 19 {
                    u64::from_le_bytes(b[11..19].try_into().unwrap_or([0; 8])) as f64
                        * self.tick
                } else {
                    self.previous_mid
                };
                let aggression = if own_orders {
                    f64::NAN
                } else {
                    (sign * (limit / self.previous_mid - 1.) / self.half_spread).max(0.)
                };
                let c = if let Some(c) = self.actors.iter().position(|s| s == signer) {
                    c
                } else {
                    if self.actors.len() >= 8 {
                        continue;
                    }
                    self.actors.push(signer.to_string());
                    self.actors.len() - 1
                };
                flow[c] += size;
                self.filter.flows[c].add((slot - self.start) as f64, size, aggression);
                log(
                    &format!(
                        "FLOW {} {} {:.3} {:.3}", slot - self.start, c, size, aggression
                    ),
                );
            }
        }
        flow
    }
    fn equity(&self, m: &Market) -> Option<f64> {
        let data = rpc_account(&self.trader).ok()??;
        let t = phoenix_rise::accounts::trader::Trader::try_from_account_bytes(&data)
            .ok()?;
        let mut value = t.header.trader_state.quote_lot_collateral.as_inner() as f64
            / 1e6;
        for (id, p) in t.positions() {
            if id == self.asset {
                value
                    += p.virtual_quote_lot_position().as_inner() as f64 / 1e6
                        + p.base_lot_position().as_inner() as f64 * self.lot * m.mark;
            }
        }
        Some(value - 10000.)
    }
}
impl Strategy for Bot {
    fn start(&mut self, s: &Session) -> BotResult {
        self.start = s.start_slot;
        self.reopen = s
            .var("MTX_REOPEN_SLOT")
            .and_then(|s| s.parse().ok())
            .unwrap_or(s.end_slot);
        self.initial = s.start_mark;
        self.previous_mid = s.start_mark;
        self.authority = s.authority.clone();
        self.half_spread = 0.002;
        let venue = PhoenixVenue::from_json(&s.venue)?;
        let cfg = venue.market(&s.symbol)?;
        self.address = cfg.market_pubkey.clone();
        self.tick = mtx_kit::venue::ticks_to_usd(cfg, 1);
        self.lot = 10_f64.powi(-(cfg.base_lots_decimals as i32));
        self.asset = cfg.asset_id as u64;
        self.trader = venue
            .trader_account(&mtx_kit::venue::key(&s.authority)?)
            .to_string();
        log(&format!("START {} {} {}", self.start, self.reopen, self.initial));
        Ok(())
    }
    fn step(&mut self, m: &Market, o: &mut Orders) -> BotResult {
        let Some((bid, ask)) = m.best_bid.zip(m.best_ask) else {
            return Ok(());
        };
        let mid = (bid + ask) * 0.5;
        if !(mid > 0. && mid.is_finite()) {
            return Ok(());
        }
        self.half_spread = self.half_spread.min(((ask - bid) * 0.5 / mid).max(0.0008));
        let actual = self.equity(m).unwrap_or(m.pnl);
        self.peak = self.peak.max(actual);
        let dd = (self.peak - actual).max(0.);
        let exit_price = if m.position.size >= 0. { bid } else { ask };
        let exit_pnl = actual + m.position.size * (exit_price - m.mark)
            - m.position.size.abs() * mid * 0.0005;
        if dd > 550. || self.peak - exit_pnl > 550. {
            self.stopped = true;
        }
        if m.slot >= self.reopen {
            if self.stopped {
                o.target_position_with(
                    0.,
                    Execution {
                        max_per_step: 70.,
                        slice: 5.,
                        slippage_bps: 1500.,
                        min_trade: 1.,
                    },
                );
            }
            if m.k % 10 == 0 {
                log(
                    &format!(
                        "OPEN {} pnl {:.2} dd {:.2} pos {}", m.slot - self.start, actual,
                        dd, m.position.size
                    ),
                );
            }
            return Ok(());
        }
        let flow = self.read_flow(m.slot, !m.sends.is_empty());
        let y = mid / self.initial - 1.;
        let s = self.filter.step(m.k as usize, y, flow);
        let remain = self.reopen.saturating_sub(m.slot) as f64;
        let h = remain.min(80.);
        let mut alpha = s.rate * h
            - s.temporary * (1. - (-std::f64::consts::LN_2 * h / s.half.max(60.)).exp())
            - s.drift * (1. - (-std::f64::consts::LN_2 * h / 300.).exp());
        let mut cap = m.cap.min(70.);
        if remain <= 20. {
            alpha = s.open - y;
            cap = m.cap.min(70.);
        }
        let entry = 0.002;
        let scale = 0.001;
        let mut target = if alpha > entry {
            ((alpha - entry) / scale * cap).min(cap)
        } else if alpha < -entry {
            ((alpha + entry) / scale * cap).max(-cap)
        } else {
            0.
        };
        let pos = m.position.size;
        if remain > 20. {
            if pos > target && pos > 0. && alpha > 0. {
                target = pos.min(cap).min(((alpha + 0.0008) / scale * cap).max(0.));
            }
            if pos < target && pos < 0. && alpha < 0. {
                target = pos.max(-cap).max(((alpha - 0.0008) / scale * cap).min(0.));
            }
        } else {
            let room = reopen_room(
                self.initial,
                m.mark,
                mid,
                pos,
                actual,
                self.peak,
                target.signum(),
                cap,
            );
            target = target.clamp(-room, room);
        }
        if self.stopped {
            target = 0.;
        }
        target = if remain > 20. { (target / 5.).round() * 5. } else { target.trunc() };
        target = target.clamp(-m.cap, m.cap);
        o.target_position_with(
            target,
            Execution {
                max_per_step: if self.stopped { 70. } else { 20. },
                slice: 5.,
                slippage_bps: 1500.,
                min_trade: 1.,
            },
        );
        log(
            &format!(
                "OBS {} mid {:.5} pos {:.3} target {:.1} pnl {:.2} dd {:.2} alpha {:.5} open {:.5} temp {:.5} rate {:.6} half {:.1} prob {:?}",
                m.slot - self.start, mid, pos, target, actual, dd, alpha, s.open, s
                .temporary, s.rate, s.half, s.probs
            ),
        );
        self.previous_mid = mid;
        Ok(())
    }
}
mtx_kit::export!(Bot);
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn reopening_stress_includes_execution_and_a_new_peak() {
        let initial = 143.11;
        for level in [-0.066, -0.03, 0., 0.03, 0.066] {
            let mid = initial * (1. + level);
            for lag in [-0.03, 0., 0.03] {
                let mark = mid * (1. + lag);
                for old in [-70., -35., 0., 35., 70.] {
                    for drawdown in [0., 100., 300., 550.] {
                        for sign in [-1., 1.] {
                            let peak = 100.;
                            let pnl = peak - drawdown;
                            let q = sign
                                * reopen_room(
                                    initial,
                                    mark,
                                    mid,
                                    old,
                                    pnl,
                                    peak,
                                    sign,
                                    70.,
                                );
                            let change = q - old;
                            let costs = change.abs() * initial * 0.006;
                            let after = pnl - change * mid + change * mark - costs;
                            let reopened = pnl - old * mark - change * mid
                                + q * initial * (1. - sign * 0.16) - costs;
                            assert!(
                                peak.max(after) - reopened <= 1000.,
                                "old {old}, q {q}, mid {mid}, mark {mark}"
                            );
                            assert!(q.abs() <= 70.);
                        }
                    }
                }
            }
        }
    }
    #[test]
    fn no_flow_does_not_create_directional_conviction() {
        let mut filter = model::Filter::default();
        for k in 0..400 {
            let s = filter.step(k, 0., [0.; 8]);
            assert_eq!(s.open, 0.);
            assert_eq!(s.rate, 0.);
            assert!(s.temporary.is_finite() && s.half.is_finite());
        }
    }
}
