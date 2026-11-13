//! Oracle-anchored Phoenix SPCX market making with public-account flow signals.
//! All trading goes through the strategy order sheet. Runtime RPCs are reads.

use base64::Engine;
use mtx_kit::{
    prelude::*,
    venue::{PhoenixVenue, phoenix_rise::accounts::owned::Trader},
};
use serde_json::{Value, json};
use std::collections::VecDeque;

#[derive(Default)]
struct Maker {
    tick: f64,
    lot: f64,
    trader_key: String,
    asset: u32,
    prev_price: f64,
    start_price: f64,
    n_moves: f64,
    sum_moves: f64,
    sum_sq: f64,
    drift: f64,
    flow: f64,
    width: f64,
    equity: f64,
    peak: f64,
    stopped: bool,
    recent: VecDeque<(u64, f64)>,
    burst_dir: f64,
    burst_until: u64,
    was_active: bool,
    start_slot: u64,
    flow_count: f64,
    last_jump: u64,
    jumps: u32,
    taker_keys: Vec<String>,
    taker_pos: Vec<f64>,
}
impl Maker {
    // Identify a recently funded cohort, without hard-coded trader addresses.
    // The largest same-slot cohort supplies independent external-flow samples.
    fn discover(&mut self, s: &Session) {
        use mtx_kit::venue::phoenix_rise::{accounts::PhoenixAccount, ix::PHOENIX_PROGRAM_ID};
        let disc = bs58::encode(PhoenixAccount::Trader.discriminant()).into_string();
        let args = json!([PHOENIX_PROGRAM_ID.to_string(),{"encoding":"base64","filters":[{"memcmp":{"offset":0,"bytes":disc}}]}]);
        let Ok(raw) = rpc("getProgramAccounts", &args.to_string()) else {
            return;
        };
        let Ok(v) = serde_json::from_str::<Value>(&raw) else {
            return;
        };
        let Some(accounts) = v.as_array() else { return };
        let mut groups = std::collections::BTreeMap::<u64, Vec<String>>::new();
        for a in accounts {
            let Some(key) = a["pubkey"].as_str() else {
                continue;
            };
            if key == self.trader_key {
                continue;
            }
            let Some(encoded) = a["account"]["data"][0].as_str() else {
                continue;
            };
            let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(encoded) else {
                continue;
            };
            let Ok(t) = Trader::try_from_account_bytes(&bytes) else {
                continue;
            };
            if t.num_markets_with_splines == 0
                && t.last_deposit_slot > s.start_slot.saturating_sub(128)
            {
                groups
                    .entry(t.last_deposit_slot)
                    .or_default()
                    .push(key.to_string());
            }
        }
        if let Some((_, keys)) = groups.into_iter().max_by_key(|(_, v)| v.len()) {
            if keys.len() >= 4 {
                self.taker_keys = keys;
                self.taker_pos = vec![0.0; self.taker_keys.len()];
            }
        }
        mtx_kit::log(&format!(
            "monitoring {} flow accounts",
            self.taker_keys.len()
        ));
    }
    // Account changes include executions against every maker, including spline
    // liquidity. Keeping these accounts separate excludes our own taker orders.
    fn read(&mut self, m: &Market) {
        let mut keys = self.taker_keys.clone();
        keys.push(self.trader_key.clone());
        let params = json!([keys,{"encoding":"base64"}]);
        let Ok(raw) = rpc("getMultipleAccounts", &params.to_string()) else {
            return;
        };
        let Ok(v) = serde_json::from_str::<Value>(&raw) else {
            return;
        };
        self.flow *= 0.85;
        for i in 0..keys.len() {
            let Some(s) = v["value"][i]["data"][0].as_str() else {
                continue;
            };
            let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(s) else {
                continue;
            };
            let Ok(t) = Trader::try_from_account_bytes(&bytes) else {
                continue;
            };
            let mut pos = 0.0;
            let mut quote = 0.0;
            for (id, p) in t.positions.entries {
                if id as u32 == self.asset {
                    pos = i64::from(p.base_lot_position) as f64 * self.lot;
                    quote = i64::from(p.virtual_quote_lot_position) as f64 / 1e6;
                }
            }
            if i < self.taker_keys.len() {
                let delta = pos - self.taker_pos[i];
                self.taker_pos[i] = pos;
                if m.k > 0 && delta.abs() > 0.001 {
                    self.recent.push_back((m.slot, delta));
                    self.flow += delta;
                    self.flow_count += 1.0;
                }
            } else {
                let equity =
                    i64::from(t.state.quote_lot_collateral) as f64 / 1e6 + pos * m.oracle + quote;
                self.equity = equity;
                self.peak = self.peak.max(equity);
            }
        }
        while self
            .recent
            .front()
            .is_some_and(|x| m.slot.saturating_sub(x.0) > 32)
        {
            self.recent.pop_front();
        }
    }
}
impl Strategy for Maker {
    fn start(&mut self, s: &Session) -> BotResult {
        let venue = PhoenixVenue::from_json(&s.venue)?;
        let cfg = venue.market(&s.symbol)?;
        self.tick = venue.ticks_to_usd(cfg, 1);
        self.lot = 10f64.powi(-(cfg.base_lots_decimals as i32));
        self.asset = cfg.asset_id;
        self.trader_key = venue.trader_account(&s.authority.parse()?).to_string();
        self.last_jump = s.start_slot;
        self.start_slot = s.start_slot;
        self.prev_price = s.start_oracle;
        self.start_price = s.start_oracle;
        self.width = 0.0015;
        self.equity = 10000.0;
        self.peak = 10000.0;
        self.discover(s);
        Ok(())
    }
    fn step(&mut self, m: &Market, o: &mut Orders) -> BotResult {
        let p = m.oracle;
        let pos = m.position.size;
        if !p.is_finite() || p <= 0.0 {
            o.cancel_all();
            return Ok(());
        }
        self.read(m);
        let change = p / self.prev_price - 1.0;
        // Posterior mean under a mixture of stationary and drifting markets.
        // Large jumps are excluded, since they do not imply continued drift.
        if change.abs() > 0.000001 && change.abs() < 0.005 {
            self.n_moves += 1.0;
            self.sum_moves += change;
            self.sum_sq += change * change;
            let var = ((self.sum_sq - self.sum_moves * self.sum_moves / self.n_moves + 0.000001)
                / (self.n_moves + 4.0))
                .max(0.00000002);
            let mut mass = 0.75;
            let mut mean = 0.0;
            for mu in [-0.00018, -0.00012, -0.00006, 0.00006, 0.00012, 0.00018] {
                let ll = (mu * self.sum_moves - 0.5 * self.n_moves * mu * mu) / var;
                let w = (0.25 / 6.0) * ll.clamp(-50.0, 50.0).exp();
                mass += w;
                mean += w * mu;
            }
            self.drift = mean / mass;
        }
        if change.abs() > 0.005 {
            self.recent.clear();
            self.flow = 0.0;
            self.burst_until = 0;
            self.last_jump = m.slot;
            self.jumps += 1;
        }
        // Leave enough drawdown budget for another adverse move between turns.
        // The stop is latched: a subsequent price recovery does not restart risk.
        let dd = (self.peak - self.equity).max(0.0);
        if dd > 130.0 {
            self.stopped = true;
        }
        let cap = m
            .cap
            .min(19.8)
            .min(((225.0 - dd) / (self.start_price * 0.05)).max(0.0));
        o.cancel_all();
        if self.stopped {
            if pos.abs() > 0.001 {
                o.target_position(0.0);
            }
            self.prev_price = p;
            return Ok(());
        }
        let mut signal = 0.0;
        // One-sided Poisson likelihood ratio, scanned over several horizons.
        // Estimate background activity online, rather than treating a busy
        // session's ordinary order rate as an informed burst.
        let rate =
            (self.flow_count + 12.0) / (m.slot.saturating_sub(self.start_slot) as f64 + 40.0);
        let mut best_score = 4.2;
        for window in [8, 12, 18, 24, 32] {
            let mut buys = 0.0f64;
            let mut sells = 0.0f64;
            let mut net = 0.0;
            for &(slot, q) in &self.recent {
                if m.slot.saturating_sub(slot) < window {
                    net += q;
                    if q > 0.0 {
                        buys += 1.0;
                    } else {
                        sells += 1.0;
                    }
                }
            }
            let expected = (rate * window as f64 * 0.5).max(0.4);
            for (dir, n, opposite) in [(1.0, buys, sells), (-1.0, sells, buys)] {
                if n >= 4.0 && n - opposite >= 3.0 && dir * net >= 2.0 && n > expected {
                    let score = n * (n / expected).ln() - n + expected;
                    if score > best_score {
                        best_score = score;
                        signal = dir;
                    }
                }
            }
        }
        // The scenario contract bounds jump spacing and the terminal window.
        if m.slot.saturating_sub(self.last_jump) < 30 || m.slots_left < 60 || self.jumps >= 3 {
            signal = 0.0;
            self.burst_until = 0;
        }
        if signal != 0.0 {
            self.burst_dir = signal;
            self.burst_until = m.slot + 24;
        }
        let active = m.slot < self.burst_until;
        let base_target = (self.drift * 120000.0).clamp(-12.0, 12.0);
        let target = if active {
            self.burst_dir * cap.min(19.8)
        } else {
            base_target
        };
        let bb = m.best_bid.unwrap_or(p * (1.0 - self.width));
        let ba = m.best_ask.unwrap_or(p * (1.0 + self.width));
        if m.k == 0 {
            self.width = ((ba - bb) / (2.0 * p)).clamp(0.0008, 0.0030);
        }
        if self.was_active && !active && (target - pos).abs() > 2.0 {
            o.target_position(target);
        } else if active && (target - pos) * self.burst_dir > 6.0 {
            o.target_position(target);
            mtx_kit::log(&format!(
                "BURST k={} dir={} pos={:.2} target={:.2} flow={:.2}",
                m.k, self.burst_dir, pos, target, self.flow
            ));
        } else if pos.abs() > cap + 0.01 {
            o.target_position(pos.clamp(-cap, cap));
        } else if active {
            // Keep inventory through the detected burst; avoid selling it back
            // to the same flow before the potential information arrives.
            let bid = p * (1.0 - self.width * 3.0);
            let ask = p * (1.0 + self.width * 3.0);
            if pos < cap {
                o.bid(bid, 0.2f64.min(cap - pos));
            }
            if pos > -cap {
                o.ask(ask, 0.2f64.min(cap + pos));
            }
        } else {
            // Inventory rebalancing may tighten a side, but normal quotes retain
            // an oracle-relative edge and do not cross the visible book.
            let skew = (target - pos) * 0.00013 + self.flow * 0.00008;
            let half = self.width * 0.8;
            let bd = ((p - bb - self.tick) / p).clamp(0.0003, half);
            let ad = ((ba - p - self.tick) / p).clamp(0.0003, half);
            let bid =
                ((p * (1.0 - bd + skew)).min(p * (1.0 - 0.00015)) / self.tick).floor() * self.tick;
            let ask =
                ((p * (1.0 + ad + skew)).max(p * (1.0 + 0.00015)) / self.tick).ceil() * self.tick;
            if pos < cap {
                o.bid(bid.min(ba - self.tick), 3.0f64.min(cap - pos));
            }
            if pos > -cap {
                o.ask(ask.max(bb + self.tick), 3.0f64.min(cap + pos));
            }
        }
        if m.k % 25 == 0 {
            mtx_kit::log(&format!(
                "k={} p={:.3} pos={:.3} flow={:.2} drift={:.6} eq={:.2} dd={:.2}",
                m.k, p, pos, self.flow, self.drift, self.equity, dd
            ));
        }
        self.was_active = active;
        self.prev_price = p;
        Ok(())
    }
}
mtx_kit::export!(Maker);

#[cfg(test)]
mod tests;
