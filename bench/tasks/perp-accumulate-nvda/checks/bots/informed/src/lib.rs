//! The informed bot, scoring v2's 1: the reference's order handling, told
//! who the informed trader is and the book's process, and nothing of the
//! session's draws.
//!
//! It reads `MTX_REGIME`, what the verifier's `--informed` hands it: the
//! informed trader's wallet (`wallets.informed`, an authority, resolved by
//! the session from the latent's roster), and the grader's maker's
//! half-spread, quote size and look delay. It isn't told whether the
//! session moves, which way, when or how far, and it trades the same market
//! under the same mandate, costs and latency as any bot.
//!
//! - **The informed trader** sends noise like every account, and a burst
//!   ahead of a move. It reads the orders that wallet sends, filled or not
//!   (`getSignaturesForAddress`, then each transaction's Phoenix market
//!   order: its side and size): its noise orders come now and then, either
//!   way; its burst comes often and one way. `BURST_TRADES` or more orders
//!   within `BURST_SLOTS`, at least `BURST_UNITS` net and nearly all of it
//!   the same way, is the burst, and its sign is the move's. Orders, not
//!   fills: a burst order the book doesn't fill says as much as one it
//!   does.
//! - **The oracle** too: a break of 0.25% from its range over 150 slots,
//!   held 30 slots, is a move under way; a burst read later overrules it.
//! - **Up:** it buys the rest of the order at once, at the maker when it can
//!   and on the backstop when it must, paying up to 60 bp over the broker.
//! - **Down:** it buys nothing, and rests nothing, until the fall has
//!   landed (the oracle 0.5% under the session's high, and no new low for
//!   80 slots), then works the rest as in a calm session.
//! - **Until a burst, or in a calm session:** it rests a bid a tick inside
//!   the maker and catches up behind a back-loaded floor, crossing only
//!   while the ask beats the broker.

use std::collections::VecDeque;

use serde_json::{Value, json};

use mtx_bot_perp_accumulate::{Order, number_after, round_down_cents, round_lots};
use mtx_kit::{
    log,
    prelude::*,
    venue::{PhoenixVenue, key},
};

/// The informed trader's net change, in NVDA, within a window of slots,
/// that is its burst rather than its noise.
const BURST_UNITS: f64 = 0.5;
const BURST_SLOTS: u64 = 120;
const BURST_TRADES: usize = 4;
/// The share of its traded size the burst's net must be.
const BURST_ONE_WAY: f64 = 0.75;
/// What it pays over the broker ahead of a rise, in basis points.
const UP_EDGE_BPS: f64 = 60.0;
/// Its largest buy a step ahead of a rise.
const UP_SLICE: f64 = 4.0;
/// A fall this far under the session's high, as a share, has landed once
/// it makes no new low for `SETTLE_SLOTS`.
const LANDED: f64 = 0.005;
const SETTLE_SLOTS: u64 = 80;
/// From here on, in share of the session, it catches up whatever it saw.
const LATE: f64 = 0.85;
/// The oracle's break from its range over `BREAK_SLOTS`, held for
/// `BREAK_HOLD` slots, that marks a move whatever the tape showed: well
/// inside the smallest move (0.71%) and outside the walk's noise there.
const BREAK: f64 = 0.0025;
const BREAK_SLOTS: u64 = 150;
const BREAK_HOLD: u64 = 30;

/// The strategy.
#[derive(Default)]
pub struct Informed {
    order: Order,
    maker_bps: f64,
    pace: f64,
    /// The informed trader's wallet, Phoenix's program and a base lot in
    /// NVDA.
    wallet: String,
    program: String,
    lot: f64,
    /// The newest of its transactions read.
    last_signature: Option<String>,
    /// Its recent orders: slot, and size signed by side (buy positive).
    orders: VecDeque<(u64, f64)>,
    /// The move's sign once its burst showed.
    sign: f64,
    /// Whether the burst was read (the oracle's break is overruled by it).
    read: bool,
    /// The oracle at recent steps, and the way it has been breaking since when.
    oracles: VecDeque<(u64, f64)>,
    breaking: Option<(f64, u64)>,
    high: f64,
    low: f64,
    low_at: u64,
    /// The share of the session at which a fall had landed.
    landed: Option<f64>,
}

impl Informed {
    /// The orders the informed wallet sent since the last read: each
    /// Phoenix market order's slot and size, signed by side.
    fn new_orders(&mut self) -> Vec<(u64, f64)> {
        let mut options = json!({ "limit": 100, "commitment": "confirmed" });
        if let Some(last) = &self.last_signature {
            options["until"] = json!(last);
        }
        let Ok(text) = rpc(
            "getSignaturesForAddress",
            &json!([self.wallet, options]).to_string(),
        ) else {
            return Vec::new();
        };
        let Ok(Value::Array(list)) = serde_json::from_str::<Value>(&text) else {
            return Vec::new();
        };
        if let Some(newest) = list.first().and_then(|e| e["signature"].as_str()) {
            self.last_signature = Some(newest.to_string());
        }
        let mut out = Vec::new();
        for entry in list.iter().rev() {
            let Some(signature) = entry["signature"].as_str() else {
                continue;
            };
            let params = json!([
                signature,
                { "encoding": "json", "maxSupportedTransactionVersion": 0, "commitment": "confirmed" }
            ]);
            let Ok(text) = rpc("getTransaction", &params.to_string()) else {
                continue;
            };
            let Ok(tx) = serde_json::from_str::<Value>(&text) else {
                continue;
            };
            let slot = tx["slot"].as_u64().unwrap_or(0);
            let message = &tx["transaction"]["message"];
            let keys = message["accountKeys"]
                .as_array()
                .cloned()
                .unwrap_or_default();
            for ix in message["instructions"].as_array().into_iter().flatten() {
                let program = ix["programIdIndex"]
                    .as_u64()
                    .and_then(|i| keys.get(i as usize))
                    .and_then(Value::as_str);
                if program != Some(self.program.as_str()) {
                    continue;
                }
                if let Some(order) = ix["data"].as_str().and_then(|d| market_order(d, self.lot)) {
                    out.push((slot, order));
                }
            }
        }
        out
    }
}

/// A Phoenix market order's size, signed by side (buy positive), from its
/// instruction data: an 8-byte discriminant, then the order packet, an
/// immediate-or-cancel (2), its side (0 bid, 1 ask), an optional price in
/// ticks, and the base lots.
fn market_order(data: &str, lot: f64) -> Option<f64> {
    let bytes = bs58::decode(data).into_vec().ok()?;
    if bytes.len() < 11 || bytes[8] != 2 {
        return None;
    }
    let sign = match bytes[9] {
        0 => 1.0,
        1 => -1.0,
        _ => return None,
    };
    let at = if bytes[10] == 1 { 19 } else { 11 };
    let lots = u64::from_le_bytes(bytes.get(at..at + 8)?.try_into().ok()?);
    Some(sign * lots as f64 * lot)
}

impl Strategy for Informed {
    fn start(&mut self, session: &Session) -> BotResult {
        self.order = Order::from_session(session)?;
        let told = session
            .var("MTX_REGIME")
            .ok_or("no MTX_REGIME: the informed bot is told the session's latent state")?;
        let n = |k: &str| number_after(told, &format!("\"{k}\""));
        self.maker_bps = n("maker_bps").unwrap_or(4.0);
        self.pace = n("maker_size").unwrap_or(2.0) * 2.0 / n("react_slots").unwrap_or(6.0).max(2.0);
        let wallet = told
            .split("\"informed\":[\"")
            .nth(1)
            .and_then(|rest| rest.split('"').next())
            .ok_or("MTX_REGIME names no informed wallet")?;
        let venue = PhoenixVenue::from_json(&session.venue)?;
        let market = venue.market(&session.symbol)?;
        key(wallet)?;
        self.wallet = wallet.to_string();
        self.program = mtx_kit::venue::phoenix_rise::ix::PHOENIX_PROGRAM_ID.to_string();
        self.lot = venue.lots_to_base_units(market, 1);
        // Everything it sent before the session is history, not flow.
        self.new_orders();
        log(&format!("informed trader {wallet}"));
        Ok(())
    }

    fn step(&mut self, market: &Market, orders: &mut Orders) -> BotResult {
        let slot = market.slot;
        let held = market.position.size;
        let oracle = market.oracle;
        let q = self.order.quantity;
        let left = (q - held).max(0.0);
        let t = self.order.progress(slot);
        orders.cancel_all();

        // The informed trader's burst, read off the orders it sends.
        if !self.read {
            for order in self.new_orders() {
                self.orders.push_back(order);
            }
            while self
                .orders
                .front()
                .is_some_and(|(s, _)| s + BURST_SLOTS < slot)
            {
                self.orders.pop_front();
            }
            let net: f64 = self.orders.iter().map(|(_, q)| q).sum();
            let traded: f64 = self.orders.iter().map(|(_, q)| q.abs()).sum();
            let same_way = self
                .orders
                .iter()
                .filter(|(_, q)| q.signum() == net.signum())
                .count();
            if net.abs() >= BURST_UNITS
                && same_way >= BURST_TRADES
                && net.abs() >= BURST_ONE_WAY * traded
            {
                self.sign = net.signum();
                self.read = true;
                log(&format!(
                    "slot {slot}: the informed trader sent {net:+.3} one way"
                ));
            }
        }
        // The oracle's own break from its recent range: a move under way,
        // whether or not the burst was read.
        self.oracles.push_back((slot, oracle));
        while self
            .oracles
            .front()
            .is_some_and(|(s, _)| s + BREAK_SLOTS < slot)
        {
            self.oracles.pop_front();
        }
        if self.sign == 0.0 {
            let lo = self
                .oracles
                .iter()
                .map(|(_, p)| *p)
                .fold(f64::INFINITY, f64::min);
            let hi = self.oracles.iter().map(|(_, p)| *p).fold(0.0, f64::max);
            let way = if oracle >= lo * (1.0 + BREAK) {
                1.0
            } else if oracle <= hi * (1.0 - BREAK) {
                -1.0
            } else {
                0.0
            };
            self.breaking = match self.breaking {
                Some((w, since)) if w == way => Some((w, since)),
                _ if way != 0.0 => Some((way, slot)),
                _ => None,
            };
            if let Some((w, since)) = self.breaking
                && slot >= since + BREAK_HOLD
            {
                self.sign = w;
                log(&format!(
                    "slot {slot}: the oracle broke {}",
                    if w > 0.0 { "up" } else { "down" }
                ));
            }
        }
        // Where a fall stands: the session's high, and its low since then.
        if oracle > self.high {
            (self.high, self.low, self.low_at) = (oracle, oracle, slot);
        } else if oracle < self.low {
            (self.low, self.low_at) = (oracle, slot);
        }
        if self.sign < 0.0
            && self.landed.is_none()
            && self.low < self.high * (1.0 - LANDED)
            && slot >= self.low_at + SETTLE_SLOTS
        {
            self.landed = Some(t);
        }
        if left < 1e-3 {
            return Ok(());
        }

        let calm_floor = |from: f64| {
            let x = ((t - from) / (0.95 - from).max(0.05)).clamp(0.0, 1.0);
            q * x.powi(3)
        };
        let (floor, edge, size, rest) = if self.sign > 0.0 {
            (q, UP_EDGE_BPS, UP_SLICE.max(self.pace), false)
        } else if self.sign < 0.0 {
            match self.landed {
                Some(from) => (calm_floor(from), 0.0, self.pace.max(0.5), true),
                None if t > LATE => (calm_floor(0.0), 0.0, self.pace.max(0.5), true),
                None => return Ok(()),
            }
        } else {
            (calm_floor(0.0), 0.0, self.pace.max(0.5), true)
        };
        let limit = self.order.worth_usd(oracle, edge);
        let behind = (floor - held).max(0.0);
        let mut sent = 0.0;
        if behind > 1e-3 && market.best_ask.is_some_and(|ask| ask <= limit) {
            sent = round_lots(behind.min(size).min(left));
            if sent > 0.0 {
                orders.buy(sent, limit);
            }
        }
        if rest {
            let bid = oracle * (1.0 - self.maker_bps / 10_000.0);
            let price = round_down_cents((bid + 0.01).min(self.order.max_buy_price));
            let size = round_lots(1.0f64.min(left - sent));
            if size > 0.0 && market.best_ask.is_none_or(|ask| price < ask) {
                orders.bid(price, size).post_only();
            }
        }
        Ok(())
    }
}
