//! Oracle-anchored SPCX quoting with market-wide flow and bounded inventory.

use base64::{Engine, engine::general_purpose::STANDARD};
use mtx_kit::{
    prelude::*,
    venue::{
        PhoenixVenue,
        phoenix_rise::{
            accounts::{
                PhoenixAccount,
                owned::{OrderbookHeader, Trader},
            },
            ix::PHOENIX_PROGRAM_ID,
        },
    },
};
use serde_json::{Value, json};
use std::collections::VecDeque;

#[derive(Default)]
struct Account {
    address: String,
    maker: bool,
    own: bool,
    q: f64,
}
#[derive(Default)]
struct Maker {
    tick: f64,
    lot: f64,
    asset: u64,
    address: String,
    accounts: Vec<Account>,
    jumps: u32,
    moves: f64,
    sum_ret: f64,
    sum_sq: f64,
    flow_sum: f64,
    flow_count: f64,
    jumped: bool,
    start_slot: u64,
    last_oracle: f64,
    width: f64,
    last_seq: u64,
    flow: VecDeque<(u64, f64, f64)>,
    signal: f64,
    until: u64,
    cooldown: u64,
    peak: f64,
    stopped: bool,
}
impl Maker {
    fn data(v: &Value) -> Option<Vec<u8>> {
        STANDARD.decode(v["data"][0].as_str()?).ok()
    }
    fn read_flow(&mut self, m: &Market) -> (f64, f64, f64, f64) {
        // The market's on-chain trade ring identifies liquidity providers. Its
        // sequence number prevents double counting as the ring wraps.
        if let Ok(s) = rpc(
            "getAccountInfo",
            &json!([self.address,{"encoding":"base64","dataSlice":{"offset":0,"length":5504}}])
                .to_string(),
        ) {
            if let Ok(v) = serde_json::from_str::<Value>(&s) {
                if let Some(data) = Self::data(&v["value"]) {
                    if let Ok(h) = OrderbookHeader::try_from_account_bytes(&data) {
                        for t in h.trade_list {
                            if t.trade_sequence_number > self.last_seq {
                                let maker = t.maker.to_string();
                                for a in &mut self.accounts {
                                    if a.address == maker {
                                        a.maker = true;
                                    }
                                }
                                self.last_seq = self.last_seq.max(t.trade_sequence_number);
                            }
                        }
                    }
                }
            }
        }
        let mut net = 0.0;
        let mut count = 0.0;
        let mut events = 0.0;
        let mut equity = 0.0;
        let addresses: Vec<&str> = self.accounts.iter().map(|a| a.address.as_str()).collect();
        if let Ok(data) = mtx_kit::rpc_accounts(&addresses) {
            for (a, d) in self.accounts.iter_mut().zip(data) {
                if let Some(d) = d {
                    if let Ok(t) = Trader::try_from_account_bytes(&d) {
                        let pos = t.positions.entries.iter().find(|(id, _)| *id == self.asset);
                        let q = pos
                            .map(|(_, p)| f64::from(p.base_lot_position) * self.lot)
                            .unwrap_or(0.0);
                        // Cross-margin equity at the oracle, independent of Phoenix's mark.
                        if a.own {
                            equity = f64::from(t.state.quote_lot_collateral) / 1e6
                                + pos
                                    .map(|(_, p)| f64::from(p.virtual_quote_lot_position) / 1e6)
                                    .unwrap_or(0.0)
                                + q * m.oracle;
                        } else if !a.maker {
                            let delta = q - a.q;
                            if delta.abs() > 0.0001 {
                                net += delta;
                                count += delta.signum();
                                events += 1.0;
                            }
                        }
                        a.q = q;
                    }
                }
            }
        }
        (net, count, equity, events)
    }
}
impl Strategy for Maker {
    fn start(&mut self, s: &Session) -> BotResult {
        let venue = PhoenixVenue::from_json(&s.venue)?;
        let config = venue.market(&s.symbol)?;
        self.tick = venue.ticks_to_usd(config, 1);
        self.lot = venue.lots_to_base_units(config, 1);
        self.asset = config.asset_id as u64;
        self.address = mtx_kit::phoenix_market_address(&s.venue, &s.symbol).unwrap_or_default();
        self.start_slot = s.start_slot;
        self.last_oracle = s.start_oracle;
        let params = json!([PHOENIX_PROGRAM_ID.to_string(),{"encoding":"base64","filters":[{"memcmp":{"offset":0,"bytes":bs58::encode(PhoenixAccount::Trader.discriminant()).into_string()}}]}]);
        if let Ok(answer) = rpc("getProgramAccounts", &params.to_string()) {
            if let Ok(v) = serde_json::from_str::<Value>(&answer) {
                if let Some(xs) = v.as_array() {
                    for x in xs.iter().take(100) {
                        if let Some(data) = Self::data(&x["account"]) {
                            if let Ok(t) = Trader::try_from_account_bytes(&data) {
                                let own = t.authority.to_string() == s.authority;
                                if t.num_markets_with_splines == 0 || own {
                                    self.accounts.push(Account {
                                        address: x["pubkey"]
                                            .as_str()
                                            .unwrap_or_default()
                                            .to_string(),
                                        maker: false,
                                        own,
                                        q: t.positions
                                            .entries
                                            .iter()
                                            .find(|(id, _)| *id == self.asset)
                                            .map(|(_, p)| f64::from(p.base_lot_position) * self.lot)
                                            .unwrap_or(0.0),
                                    });
                                }
                            }
                        }
                    }
                }
            }
        }
        if !self.accounts.iter().any(|a| a.own) {
            return Err("own Phoenix trader account unavailable".into());
        }
        mtx_kit::log(&format!("tracking {} accounts", self.accounts.len()));
        Ok(())
    }
    fn step(&mut self, m: &Market, o: &mut Orders) -> BotResult {
        let p = m.oracle;
        let q = m.position.size;
        if !p.is_finite() || p <= 0.0 {
            o.cancel_all();
            return Ok(());
        }
        let (net, count, equity, events) = self.read_flow(m);
        if equity <= 0.0 {
            o.cancel_all();
            o.target_position(0.0);
            return Ok(());
        }
        if equity > 0.0 {
            self.peak = self.peak.max(equity);
            // Leave room for a full adverse jump and the cost of liquidating.
            if self.peak - equity > 110.0 {
                self.stopped = true;
            }
        }
        let ret = p / self.last_oracle - 1.0;
        self.last_oracle = p;
        if ret.abs() > 0.007 {
            self.jumped = true;
            self.jumps += 1;
            self.flow.clear();
            self.signal = 0.0;
            self.until = 0;
            self.cooldown = m.slot + 32;
        } else {
            if ret.abs() > 1e-8 {
                self.moves += 1.0;
                self.sum_ret += ret;
                self.sum_sq += ret * ret;
            }
            self.flow_sum += count;
            self.flow_count += events;
            self.flow.push_back((m.slot, net, count));
        }
        let mut trend_target = 0.0;
        // Marginalize flat/up/down drift hypotheses instead of chasing each move.
        if !self.jumped && self.moves >= 5.0 {
            let variance = ((self.sum_sq - self.sum_ret * self.sum_ret / self.moves
                + 8.0 * 0.0005_f64.powi(2))
                / (self.moves + 8.0))
                .max(0.00015_f64.powi(2));
            let buys = (self.flow_count + self.flow_sum) * 0.5;
            let sells = (self.flow_count - self.flow_sum) * 0.5;
            let mut up = 0.0;
            let mut down = 0.0;
            for mu in [0.00006, 0.00009, 0.00012, 0.00015, 0.00018] {
                let common = -0.5 * self.moves * mu * mu / variance;
                up += (common
                    + mu * self.sum_ret / variance
                    + 0.3 * (buys * 1.17_f64.ln() + sells * 0.83_f64.ln()))
                .clamp(-60.0, 60.0)
                .exp()
                    * 0.025;
                down += (common - mu * self.sum_ret / variance
                    + 0.3 * (sells * 1.17_f64.ln() + buys * 0.83_f64.ln()))
                .clamp(-60.0, 60.0)
                .exp()
                    * 0.025;
            }
            trend_target = 18.0 * (up - down) / (0.75 + up + down);
        }
        while self
            .flow
            .front()
            .is_some_and(|x| m.slot.saturating_sub(x.0) > 30)
        {
            self.flow.pop_front();
        }
        let mut n6 = 0.0;
        let mut n12 = 0.0;
        let mut n24 = 0.0;
        for &(slot, _, n) in &self.flow {
            let age = m.slot.saturating_sub(slot);
            if age < 6 {
                n6 += n;
            }
            if age < 12 {
                n12 += n;
            }
            if age < 24 {
                n24 += n;
            }
        }
        // Short windows find concentrated flow; renew only on a new matching fill.
        let strength = [(n6 / 5.0), (n12 / 6.0), (n24 / 8.0)]
            .into_iter()
            .max_by(|a, b| a.abs().total_cmp(&b.abs()))
            .unwrap_or(0.0);
        let jump_window =
            m.slot.saturating_sub(self.start_slot) >= 30 && m.slots_left > 60 && self.jumps < 3;
        if jump_window && m.slot >= self.cooldown && strength.abs() >= 1.0 && count * strength > 0.0
        {
            self.signal = strength.signum();
            self.until = m.slot + 16;
        }
        if m.slot >= self.until || !jump_window {
            self.signal = 0.0;
        }
        if self.width == 0.0 {
            self.width = ((m.best_ask.unwrap_or(p * 1.0015) - m.best_bid.unwrap_or(p * 0.9985))
                / (2.0 * p))
                .clamp(0.0008, 0.003);
        }
        o.cancel_all();
        if self.stopped {
            o.target_position(0.0);
            return Ok(());
        }
        let target = if self.signal != 0.0 {
            self.signal * 18.0
        } else {
            trend_target
        };
        let mut expected_q = q;
        if self.signal != 0.0 && (q - target).abs() > 5.0 {
            o.target_position(target);
            expected_q = target;
        }

        if self.signal == 0.0
            && (q - target).abs() > 7.0
            && m.slot >= self.until + 8
            && m.slot >= self.cooldown
        {
            let exit = target;
            o.target_position(exit);
            expected_q = exit;
        }
        let quote_target = if self.signal != 0.0 {
            self.signal * 24.0
        } else {
            target
        };
        let skew = (expected_q - quote_target) * 0.00018;
        let half = (self.width * 0.6).max(0.0004);
        let bid = p * (1.0 - (half + skew).max(0.00012));
        let ask = p * (1.0 + (half - skew).max(0.00012));
        // Keep a buffer below the 20-share mandate for lot and price rounding.
        let cap = if self.signal == 0.0 {
            (target.abs() + 4.0).min(19.5)
        } else {
            19.5
        };
        if expected_q < cap {
            o.bid(
                (bid / self.tick).floor() * self.tick,
                2.0_f64.min(cap - expected_q),
            )
            .post_only();
        }
        if expected_q > -cap {
            o.ask(
                (ask / self.tick).ceil() * self.tick,
                2.0_f64.min(cap + expected_q),
            )
            .post_only();
        }
        // Small inside quotes observe otherwise unfilled taker flow.
        if expected_q < cap - 0.02 {
            o.bid(((p - self.tick) / self.tick).floor() * self.tick, 0.01)
                .post_only();
        }
        if expected_q > -cap + 0.02 {
            o.ask(((p + self.tick) / self.tick).ceil() * self.tick, 0.01)
                .post_only();
        }
        mtx_kit::log(&format!(
            "S {} {:.2} {:.3} {:.3} {:.0} {:.0} {:.0} {:.1} {:.2}",
            m.k, p, q, net, n12, n24, self.signal, target, equity
        ));
        Ok(())
    }
}
mtx_kit::export!(Maker);
