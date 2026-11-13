//! Oracle-valued SPCX market making with inventory control and flow forecasting.
//!
//! All data comes from the session or read-only Solana RPC. Trader account
//! changes identify taker direction; recent maker identities prevent double
//! counting the competing maker. Small inside quotes observe low-limit flow.
//! Poisson likelihoods over several horizons distinguish bursts from the
//! session's estimated background rate. Strong bursts warrant directional
//! inventory; weaker evidence changes the ordinary quote reservation price.
//!
//! Positions unwind passively unless flow reverses or the equity stop fires.
//! Equity uses collateral and virtual quote balances at the oracle, including
//! trading fees, rather than the book-sensitive Phoenix mark.

use base64::Engine;
use mtx_kit::venue::phoenix_rise::accounts::owned::{OrderbookHeader, Trader};
use mtx_kit::{log, prelude::*, venue::PhoenixVenue};
use std::collections::VecDeque;
#[derive(Default)]
struct FlowAccount {
    key: String,
    pos: f64,
    maker: bool,
}
#[derive(Default)]
struct Maker {
    flow_accounts: Vec<FlowAccount>,
    start_slot: u64,
    total_b: f64,
    total_s: f64,
    n: f64,
    sum: f64,
    sum2: f64,
    jumped: bool,
    tick: f64,
    lots: f64,
    asset_id: u32,
    market_key: String,
    trader_key: String,
    events: VecDeque<(u64, f64)>,
    until: u64,
    direction: f64,
    cooldown: u64,
    last_p: f64,
    drift: f64,
    base_width: f64,
    last_bid: f64,
    last_ask: f64,
    equity_start: f64,
    peak: f64,
    dd: f64,
    stopped: bool,
}
impl Maker {
    fn read(&mut self, m: &Market) -> (f64, f64) {
        let mut count = 0.0;
        let mut volume = 0.0;
        let mut keys = vec![self.market_key.as_str(), self.trader_key.as_str()];
        keys.extend(self.flow_accounts.iter().map(|a| a.key.as_str()));
        let params =
            serde_json::json!([keys,{"encoding":"base64","dataSlice":{"offset":0,"length":5504}}]);
        if let Ok(r) = mtx_kit::rpc("getMultipleAccounts", &params.to_string()) {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&r) {
                if let Some(enc) = v["value"][0]["data"][0].as_str() {
                    if let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(enc) {
                        if let Ok(h) = OrderbookHeader::try_from_account_bytes(&bytes) {
                            for t in &h.trade_list {
                                let maker = t.maker.to_string();
                                for account in &mut self.flow_accounts {
                                    if account.key == maker && !account.maker {
                                        account.maker = true;
                                        log(&format!("exclude maker {}", maker));
                                    }
                                }
                            }
                        }
                    }
                }
                for (i, a) in self.flow_accounts.iter_mut().enumerate() {
                    if let Some(enc) = v["value"][i + 2]["data"][0].as_str() {
                        if let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(enc) {
                            if let Ok(t) = Trader::try_from_account_bytes(&bytes) {
                                let pos = t
                                    .positions
                                    .entries
                                    .iter()
                                    .filter(|(id, _)| *id as u32 == self.asset_id)
                                    .map(|(_, p)| p.base_lot_position.as_inner() as f64 / self.lots)
                                    .sum::<f64>();
                                let delta = pos - a.pos;
                                a.pos = pos;
                                if !a.maker && delta.abs() > 0.0005 {
                                    if delta > 0.0 {
                                        self.total_b += 1.0;
                                    } else {
                                        self.total_s += 1.0;
                                    }
                                    count += delta.signum();
                                    volume += delta;
                                    self.events.push_back((m.slot, delta.signum()));
                                }
                            }
                        }
                    }
                }
                if let Some(enc) = v["value"][1]["data"][0].as_str() {
                    if let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(enc) {
                        if let Ok(t) = Trader::try_from_account_bytes(&bytes) {
                            let mut equity = t.state.quote_lot_collateral.as_inner() as f64 / 1e6;
                            for (id, p) in &t.positions.entries {
                                if *id as u32 == self.asset_id {
                                    equity += p.base_lot_position.as_inner() as f64 / self.lots
                                        * m.oracle
                                        + p.virtual_quote_lot_position.as_inner() as f64 / 1e6;
                                }
                            }
                            if self.equity_start == 0.0 {
                                self.equity_start = equity;
                                self.peak = equity;
                            }
                            self.peak = self.peak.max(equity);
                            self.dd = self.peak - equity;
                        }
                    }
                }
            }
        }
        (count, volume)
    }
    fn target(&mut self, o: &mut Orders, pos: f64) {
        o.target_position_with(
            pos,
            Execution {
                max_per_step: 40.0,
                slice: 5.0,
                slippage_bps: 100.0,
                min_trade: 0.01,
            },
        );
    }
}
impl Strategy for Maker {
    fn start(&mut self, s: &Session) -> BotResult {
        *self = Self::default();
        let v = PhoenixVenue::from_json(&s.venue)?;
        let c = v.market(&s.symbol)?;
        self.asset_id = c.asset_id;
        self.tick = v.ticks_to_usd(c, 1);
        self.lots = 10f64.powi(c.base_lots_decimals as i32);
        self.market_key = c.market_pubkey.clone();
        self.trader_key = v.trader_account(&s.authority.parse()?).to_string();
        self.start_slot = s.start_slot;
        self.last_p = s.start_oracle;
        let discr = mtx_kit::venue::phoenix_rise::accounts::PhoenixAccount::Trader.discriminant();
        let params = serde_json::json!([mtx_kit::venue::phoenix_rise::ix::PHOENIX_PROGRAM_ID.to_string(),{"encoding":"base64","dataSlice":{"offset":0,"length":320},"filters":[{"memcmp":{"offset":0,"bytes":bs58::encode(discr).into_string()}}]}]);
        if let Ok(r) = mtx_kit::rpc("getProgramAccounts", &params.to_string()) {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&r) {
                if let Some(entries) = v.as_array() {
                    for e in entries {
                        if let (Some(key), Some(enc)) =
                            (e["pubkey"].as_str(), e["account"]["data"][0].as_str())
                        {
                            if let Ok(d) = base64::engine::general_purpose::STANDARD.decode(enc) {
                                if d.len() >= 200 {
                                    // TraderHeader.last_deposit_slot, at the SDK layout offset 192.
                                    let deposit = u64::from_le_bytes(
                                        d[192..200].try_into().unwrap_or_default(),
                                    );
                                    if deposit >= s.start_slot.saturating_sub(1000)
                                        && key != self.trader_key
                                    {
                                        log(&format!("flow account {}", key));
                                        self.flow_accounts.push(FlowAccount {
                                            key: key.to_string(),
                                            pos: 0.0,
                                            maker: false,
                                        });
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
        Ok(())
    }
    fn step(&mut self, m: &Market, o: &mut Orders) -> BotResult {
        let p = m.oracle;
        let pos = m.position.size;
        if p <= 0.0 || !p.is_finite() {
            o.cancel_all();
            return Ok(());
        }
        let (count, vol) = self.read(m);
        let ret = (p / self.last_p - 1.0) * 10000.0;
        if ret.abs() > 0.001 && ret.abs() < 40.0 {
            self.drift = 0.97 * self.drift + 0.03 * ret;
            self.n += 1.0;
            self.sum += ret;
            self.sum2 += ret * ret;
        }
        if self.base_width == 0.0 {
            self.base_width = match (m.best_bid, m.best_ask) {
                (Some(b), Some(a)) => ((a - b) / p * 5000.0).clamp(8.0, 30.0),
                _ => 15.0,
            };
        }
        if ret.abs() > 50.0 {
            self.jumped = true;
            self.until = 0;
            self.cooldown = m.slot + 50;
            self.events.clear();
        }
        while self.events.front().map_or(false, |e| m.slot - e.0 >= 32) {
            self.events.pop_front();
        }
        let net: f64 = self.events.iter().map(|e| e.1).sum();
        let elapsed = m.slot.saturating_sub(self.start_slot) as f64;
        let base_b = ((15.0 + self.total_b) / (100.0 + elapsed)).clamp(0.07, 0.28);
        let base_s = ((15.0 + self.total_s) / (100.0 + elapsed)).clamp(0.07, 0.28);
        // Log likelihood relative to independent background buy/sell arrivals.
        let mut score = 0.0;
        let mut dir = 0.0;
        for w in [6u64, 10, 16, 24, 32] {
            let buys = self
                .events
                .iter()
                .filter(|e| m.slot - e.0 < w && e.1 > 0.0)
                .count() as f64;
            let sells = self
                .events
                .iter()
                .filter(|e| m.slot - e.0 < w && e.1 < 0.0)
                .count() as f64;
            for extra in [0.3_f64, 0.5, 0.8] {
                let sb = buys * (1.0 + extra / base_b).ln() - extra * w as f64;
                let ss = sells * (1.0 + extra / base_s).ln() - extra * w as f64;
                if sb > score {
                    score = sb;
                    dir = 1.0;
                }
                if ss > score {
                    score = ss;
                    dir = -1.0;
                }
            }
        }
        if score > 3.5
            && dir != 0.0
            && m.slot > self.cooldown
            && elapsed > 30.0
            && m.slots_left > 60
        {
            self.direction = dir;
            self.until = m.slot + 48;
        }
        o.cancel_all();
        // Leave room for a full unexpected jump before the next risk check.
        if self.dd > 100.0 {
            self.stopped = true;
        }
        if self.stopped {
            self.target(o, 0.0);
            return Ok(());
        }
        let active = m.slot < self.until;
        // A new burst against a substantial holding invalidates the old forecast.
        // Flatten before waiting for enough evidence to establish the opposite side.
        if score > 1.8
            && pos * dir < -6.0
            && m.slot > self.cooldown
            && !(score > 3.5 && active && self.direction == dir)
        {
            self.until = 0;
            self.target(o, 0.0);
            self.last_p = p;
            log(&format!(
                "FLOW EXIT k={} pos={:.2} score={:.2}",
                m.k, pos, score
            ));
            return Ok(());
        }
        // Take only quotes offering more than the taker fee below oracle fair value.
        // The IOC can fill partially: no second leg assumes that it filled.
        if !active {
            let buy_room = (19.5 - pos).max(0.0).min(4.0);
            let sell_room = (19.5 + pos).max(0.0).min(4.0);
            if m.best_ask.map_or(false, |a| a < p * 0.999) && buy_room > 0.1 {
                o.buy(buy_room, p * 0.9995);
                self.last_p = p;
                log(&format!(
                    "ARB buy k={} size={:.3} p={:.2}",
                    m.k, buy_room, p
                ));
                return Ok(());
            }
            if m.best_bid.map_or(false, |b| b > p * 1.001) && sell_room > 0.1 {
                o.sell(sell_room, p * 1.0005);
                self.last_p = p;
                log(&format!(
                    "ARB sell k={} size={:.3} p={:.2}",
                    m.k, sell_room, p
                ));
                return Ok(());
            }
        }
        let mut drift_goal = 0.0;
        if self.n > 5.0 && !self.jumped {
            // Symmetric trend prior, integrated over plausible per-move drift.
            let variance = (self.sum2 / self.n - (self.sum / self.n).powi(2)).max(4.0);
            let mut plus = 0.0;
            let mut minus = 0.0;
            for mu in [0.6_f64, 0.9, 1.2, 1.5, 1.8] {
                plus += 0.025
                    * ((mu * self.sum - 0.5 * self.n * mu * mu) / variance)
                        .clamp(-60.0, 30.0)
                        .exp();
                minus += 0.025
                    * ((-mu * self.sum - 0.5 * self.n * mu * mu) / variance)
                        .clamp(-60.0, 30.0)
                        .exp();
            }
            drift_goal = 8.0 * (plus - minus) / (0.75 + plus + minus);
        }
        let pressure = if m.slot > self.cooldown && m.slots_left > 60 {
            dir * (score - 1.0).clamp(0.0, 2.5) * 1.6
        } else {
            0.0
        };
        let normal_goal = (drift_goal + pressure).clamp(-10.0, 10.0);
        let goal = if active {
            self.direction * 19.0
        } else {
            normal_goal
        };
        if active && pos * self.direction < 18.0 {
            self.target(o, goal);
            log(&format!(
                "TAKE k={} pos={:.3} target={:.1} net={} score={:.2} dd={:.2}",
                m.k, pos, goal, net, score, self.dd
            ));
        } else {
            let inv = pos - goal;
            let half = (self.base_width * 0.60).max(3.5);
            let mut bhalf = half;
            let mut ahalf = half;
            if let Some(b) = m.best_bid {
                if (b - self.last_bid).abs() > self.tick * 0.3 {
                    bhalf = bhalf.min(((p - b - self.tick) / p * 10000.0).max(3.0));
                }
            }
            if let Some(a) = m.best_ask {
                if (a - self.last_ask).abs() > self.tick * 0.3 {
                    ahalf = ahalf.min(((a - self.tick - p) / p * 10000.0).max(3.0));
                }
            }
            bhalf = (bhalf + inv * 1.6).max(2.0);
            ahalf = (ahalf - inv * 1.6).max(2.0);
            if active {
                bhalf = 1.0;
                ahalf = 1.0;
            }
            let mut bid = (p * (1.0 - bhalf / 10000.0) / self.tick).floor() * self.tick;
            let mut ask = (p * (1.0 + ahalf / 10000.0) / self.tick).ceil() * self.tick;
            if let Some(a) = m.best_ask {
                bid = bid.min(a - self.tick);
            }
            if let Some(b) = m.best_bid {
                ask = ask.max(b + self.tick);
            }
            let limit = if active {
                19.5
            } else {
                (goal.abs() + 3.0).min(13.0)
            };
            let size = if active { 0.01_f64 } else { 1.5_f64 };
            let bs = size.min(limit - pos).max(0.0);
            let az = size.min(limit + pos).max(0.0);
            // Small inside quotes reveal low-limit flow without carrying its size.
            if !active {
                let mut sb = (p * 0.9999 / self.tick).floor() * self.tick;
                let mut sa = (p * 1.0001 / self.tick).ceil() * self.tick;
                if let Some(a) = m.best_ask {
                    sb = sb.min(a - self.tick);
                }
                if let Some(b) = m.best_bid {
                    sa = sa.max(b + self.tick);
                }
                if pos < 19.48 {
                    o.bid(sb, 0.01).post_only();
                }
                if pos > -19.48 {
                    o.ask(sa, 0.01).post_only();
                }
            }
            if bs >= 0.01 {
                o.bid(bid, bs).post_only();
            }
            if az >= 0.01 {
                o.ask(ask, az).post_only();
            }
            self.last_bid = bid;
            self.last_ask = ask;
        }
        if m.k % 20 == 0 || ret.abs() > 50.0 {
            log(&format!(
                "k={} p={:.2} pos={:.3} net={} count={} vol={:.2} drift={:.2} dd={:.2} active={}",
                m.k, p, pos, net, count, vol, self.drift, self.dd, active
            ));
        }
        self.last_p = p;
        Ok(())
    }
}
mtx_kit::export!(Maker);
