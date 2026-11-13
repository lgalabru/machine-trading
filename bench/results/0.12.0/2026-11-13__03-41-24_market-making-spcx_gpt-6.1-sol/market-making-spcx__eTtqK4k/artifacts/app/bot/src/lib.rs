//! Oracle-anchored SPCX market making with an aggregate order-flow signal.
//!
//! The strategy discovers newly active takers from public Phoenix accounts,
//! reads their aggregate inventory changes, and distinguishes ordinary flow
//! from dense one-sided bursts. All chain access uses approved read RPCs.

use base64::Engine;
use mtx_kit::{
    log,
    prelude::*,
    rpc_accounts,
    venue::{
        PhoenixVenue,
        phoenix_rise::accounts::{PhoenixAccount, owned::Trader, trader::TraderHeader},
    },
};
use serde_json::json;
use std::collections::{BTreeMap, VecDeque};

// Counts measure arrivals, rather than size: informed and noise sizes share a distribution.
const WINDOW: u64 = 12;
const COUNT: f64 = 7.0;
const NET: f64 = 4.0;
const HOLD: u64 = 24;
const SIGNAL_SIZE: f64 = 19.0;
const NORMAL_CAP: f64 = 6.0;

#[derive(Default)]
struct Flow {
    slot: u64,
    buy: f64,
    sell: f64,
    volume: f64,
}
#[derive(Default)]
struct Maker {
    own: String,
    pool: Vec<String>,
    pool_pos: Vec<f64>,
    pool_seq: Vec<u64>,
    asset: u64,
    factor: f64,
    last_oracle: f64,
    last_pos: f64,
    tick: f64,
    last_bid: f64,
    last_ask: f64,
    history: VecDeque<Flow>,
    direction: f64,
    until: u64,
    first_signal: u64,
    cooldown: u64,
    drift: f64,
    flow_ema: f64,
    peak: f64,
    stopped: bool,
}

impl Maker {
    // Group fresh, non-spline accounts by funding slot. The pool is discovered
    // anew each session; no trader addresses or scenario data are embedded.
    fn discover(&mut self, s: &Session) {
        let tag = PhoenixAccount::Trader.discriminant();
        let params = json!([
            mtx_kit::venue::phoenix_rise::ix::PHOENIX_PROGRAM_ID.to_string(),
            {
                "encoding": "base64",
                "filters": [{"memcmp": {"offset": 0, "bytes": bs58::encode(tag).into_string()}}],
                "dataSlice": {"offset": 0, "length": 224}
            }
        ]);
        let Ok(z) = rpc("getProgramAccounts", &params.to_string()) else {
            return;
        };
        let Ok(z) = serde_json::from_str::<serde_json::Value>(&z) else {
            return;
        };
        let Some(xs) = z.as_array() else { return };
        let mut groups: BTreeMap<u64, Vec<String>> = BTreeMap::new();
        for x in xs {
            let Some(address) = x["pubkey"].as_str() else {
                continue;
            };
            if address == self.own {
                continue;
            }
            let Some(b) = x["account"]["data"][0].as_str() else {
                continue;
            };
            let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(b) else {
                continue;
            };
            let Ok(h) = TraderHeader::try_read_from_account_bytes(&bytes) else {
                continue;
            };
            if h.num_markets_with_splines == 0
                && h.last_deposit_slot >= s.start_slot.saturating_sub(256)
            {
                groups
                    .entry(h.last_deposit_slot)
                    .or_default()
                    .push(address.into());
            }
        }
        if let Some((_, g)) = groups.into_iter().max_by_key(|(_, g)| g.len()) {
            if g.len() >= 4 && g.len() <= 16 {
                self.pool = g;
            }
        }
        self.pool_pos = vec![0.0; self.pool.len()];
        self.pool_seq = vec![2; self.pool.len()];
        log(&format!(
            "observing {} active taker accounts",
            self.pool.len()
        ));
    }
    fn read(&mut self, m: &Market) -> (Flow, f64) {
        let mut keys = vec![self.own.as_str()];
        keys.extend(self.pool.iter().map(String::as_str));
        let mut flow = Flow {
            slot: m.slot,
            ..Flow::default()
        };
        let mut equity = 10000.0 + m.pnl;
        if let Ok(accounts) = rpc_accounts(&keys) {
            for (i, a) in accounts.iter().enumerate() {
                let Some(a) = a else { continue };
                let Ok(t) = Trader::try_from_account_bytes(a) else {
                    continue;
                };
                let pos = t
                    .positions
                    .entries
                    .iter()
                    .find(|(id, _)| *id == self.asset)
                    .map(|(_, p)| p);
                let units = pos
                    .map(|p| p.base_lot_position.as_inner() as f64 / self.factor)
                    .unwrap_or(0.0);
                if i == 0 {
                    equity = t.state.quote_lot_collateral.as_inner() as f64 / 1e6;
                    if let Some(p) = pos {
                        equity +=
                            p.virtual_quote_lot_position.as_inner() as f64 / 1e6 + units * m.oracle;
                    }
                    // Phoenix settles maker fills lazily. The SDK position
                    // includes them; the raw trader position can still lag.
                    let pending = m.position.size - units;
                    if pending > 0.0001 && self.last_bid > 0.0 {
                        equity += pending * (m.oracle - self.last_bid);
                    }
                    if pending < -0.0001 && self.last_ask > 0.0 {
                        equity += pending * (m.oracle - self.last_ask);
                    }
                } else {
                    let delta = units - self.pool_pos[i - 1];
                    let seq = t.sequence_number.sequence_number;
                    if delta.abs() > 0.0001 {
                        let count = seq.saturating_sub(self.pool_seq[i - 1]).clamp(1, 3) as f64;
                        if delta > 0.0 {
                            flow.buy += count;
                        } else {
                            flow.sell += count;
                        }
                        flow.volume += delta;
                    }
                    self.pool_pos[i - 1] = units;
                    self.pool_seq[i - 1] = seq;
                }
            }
        }
        if self.pool.is_empty() {
            let dp = m.position.size - self.last_pos;
            flow.volume = -dp;
            if dp < 0.0 {
                flow.buy = 1.0;
            } else if dp > 0.0 {
                flow.sell = 1.0;
            }
        }
        (flow, equity)
    }
}
impl Strategy for Maker {
    fn start(&mut self, s: &Session) -> BotResult {
        self.last_oracle = s.start_oracle;
        self.tick = 0.01;
        self.factor = 1000.0;
        self.asset = 41;
        self.peak = 10000.0;
        if let Ok(v) = PhoenixVenue::from_json(&s.venue) {
            if let Ok(m) = v.market(&s.symbol) {
                self.tick = v.ticks_to_usd(m, 1);
                self.factor = 10f64.powi(m.base_lots_decimals as i32);
                self.asset = m.asset_id as u64;
            }
            if let Ok(a) = mtx_kit::venue::key(&s.authority) {
                self.own = v.trader_account(&a).to_string();
            }
        }
        self.discover(s);
        Ok(())
    }
    fn step(&mut self, m: &Market, o: &mut Orders) -> BotResult {
        let p = m.oracle;
        if p <= 0.0 || !p.is_finite() {
            o.cancel_all();
            return Ok(());
        }
        let (flow, equity) = self.read(m);
        self.peak = self.peak.max(equity);
        let dd = (self.peak - equity).max(0.0);
        let r = (p / self.last_oracle - 1.0) * 10000.0;
        if r.abs() < 60.0 && r.abs() > 0.0001 {
            self.drift = 0.96 * self.drift + 0.04 * r;
        }
        self.flow_ema = 0.97 * self.flow_ema + flow.buy - flow.sell;
        self.history.push_back(flow);
        while self
            .history
            .front()
            .is_some_and(|x| x.slot + WINDOW <= m.slot)
        {
            self.history.pop_front();
        }
        let buy: f64 = self.history.iter().map(|x| x.buy).sum();
        let sell: f64 = self.history.iter().map(|x| x.sell).sum();
        let volume: f64 = self.history.iter().map(|x| x.volume).sum();
        // Clear the episode after a jump. Further jumps are separated by at
        // least 60 slots; informed flow can begin 30 slots before the next.
        if r.abs() > 100.0 {
            log(&format!(
                "jump k={} ret={:.1} held={:.3} equity={:.2}",
                m.k, r, m.position.size, equity
            ));
            self.direction = 0.0;
            self.until = 0;
            self.cooldown = m.slot + 30;
            self.history.clear();
        }
        let short_buy: f64 = self
            .history
            .iter()
            .filter(|x| x.slot + 6 > m.slot)
            .map(|x| x.buy)
            .sum();
        let short_sell: f64 = self
            .history
            .iter()
            .filter(|x| x.slot + 6 > m.slot)
            .map(|x| x.sell)
            .sum();
        if m.slot >= self.cooldown && m.slots_left > 60 {
            let signal = if (buy >= COUNT && buy - sell >= NET)
                || (short_buy >= 5.0 && short_buy - short_sell >= 3.0)
            {
                1.0
            } else if (sell >= COUNT && sell - buy >= NET)
                || (short_sell >= 5.0 && short_sell - short_buy >= 3.0)
            {
                -1.0
            } else {
                0.0
            };
            if signal != 0.0 && (self.direction == 0.0 || signal == self.direction) {
                if self.direction == 0.0 {
                    self.first_signal = m.slot;
                    log(&format!(
                        "signal k={} dir={} buy={} sell={} volume={:.2} equity={:.2}",
                        m.k, signal, buy, sell, volume, equity
                    ));
                }
                self.direction = signal;
                self.until = (m.slot + HOLD).min(self.first_signal + 44);
            }
        }
        if m.slot >= self.until {
            self.direction = 0.0;
        }
        // Leave room below the $250 mandate for a full jump and execution
        // costs. This latch can only turn on, and liquidates through the sheet.
        if dd > 100.0 {
            self.stopped = true;
        }
        let pos = m.position.size;
        let target = if self.stopped {
            0.0
        } else {
            self.direction * SIGNAL_SIZE.min(m.cap - 0.5)
        };
        o.cancel_all();
        if (target != 0.0 && (target - pos).abs() > 1.0) || (self.stopped && pos.abs() > 0.05) {
            o.target_position_with(
                target,
                Execution {
                    max_per_step: 40.0,
                    slice: 4.0,
                    slippage_bps: 80.0,
                    min_trade: 0.05,
                },
            );
        } else if !self.stopped {
            let b = m.best_bid.unwrap_or(p * 0.998);
            let a = m.best_ask.unwrap_or(p * 1.002);
            let inv_target = if target != 0.0 {
                target
            } else {
                (self.drift * 1.5 + self.flow_ema * 0.08).clamp(-3.0, 3.0)
            };
            let shift = ((inv_target - pos) * 0.8).clamp(-15.0, 15.0) * p / 10000.0;
            let half = 3.5 * p / 10000.0;
            let mut bid = ((p - half + shift)
                .min(p * (1.0 - 0.0001))
                .min(a - self.tick)
                / self.tick)
                .floor()
                * self.tick;
            let mut ask = ((p + half + shift)
                .max(p * (1.0 + 0.0001))
                .max(b + self.tick)
                / self.tick)
                .ceil()
                * self.tick;
            // Retain directional inventory while the episode is active;
            // otherwise unwind through passive quotes instead of paying a
            // round trip immediately after every signal.
            if target > 0.0 {
                ask = ask.max(p * 1.004);
            }
            if target < 0.0 {
                bid = bid.min(p * 0.996);
            }
            let limit = if target != 0.0 {
                m.cap - 0.5
            } else {
                NORMAL_CAP
            };
            let bs = (limit - pos).clamp(0.0, 3.0);
            let az = (limit + pos).clamp(0.0, 3.0);
            if bs >= 0.01 {
                o.bid(bid, bs).post_only();
            }
            if az >= 0.01 {
                o.ask(ask, az).post_only();
            }
            // Tiny touch orders preserve visibility of low-limit takers
            // while the inventory-skewed main quote rests further out.
            if target == 0.0 {
                let pb = ((p * (1.0 - 0.0001)).min(a - self.tick) / self.tick).floor() * self.tick;
                let pa = ((p * (1.0 + 0.0001)).max(b + self.tick) / self.tick).ceil() * self.tick;
                if pos < m.cap - 0.1 && (pb - bid).abs() > self.tick * 0.5 {
                    o.bid(pb, 0.02).post_only();
                }
                if pos > -m.cap + 0.1 && (pa - ask).abs() > self.tick * 0.5 {
                    o.ask(pa, 0.02).post_only();
                }
            }
            self.last_bid = bid;
            self.last_ask = ask;
        }
        if m.k % 20 == 0 {
            log(&format!(
                "k={} p={:.3} pos={:.3} eq={:.3} dd={:.3} flow={:.1}/{:.1} dir={}",
                m.k, p, pos, equity, dd, buy, sell, self.direction
            ));
        }
        self.last_oracle = p;
        self.last_pos = pos;
        Ok(())
    }
}
mtx_kit::export!(Maker);
