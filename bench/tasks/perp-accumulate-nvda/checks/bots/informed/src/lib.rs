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
//!   ahead of a move. Phoenix names traders by their account, a PDA of the
//!   wallet, so it reads that account's NVDA position every step: a net
//!   change of `BURST_UNITS` within `BURST_SLOTS` is the burst, and its sign
//!   is the move's.
//! - **Up:** it buys the rest of the order at once, at the maker when it can
//!   and on the backstop when it must, paying up to 40 bp over the broker.
//! - **Down:** it buys nothing, and rests nothing, until the fall has
//!   landed (the oracle 0.35% under the session's high, and no new low for
//!   80 slots), then works the rest as in a calm session.
//! - **Until a burst, or in a calm session:** it rests a bid a tick inside
//!   the maker and catches up behind a back-loaded floor, crossing only
//!   while the ask beats the broker.

use std::collections::VecDeque;

use mtx_bot_perp_accumulate::{Order, number_after, round_down_cents, round_lots};
use mtx_kit::{
    log,
    prelude::*,
    rpc_account,
    venue::{PhoenixVenue, key, phoenix_rise::accounts::trader::Trader},
};

/// The informed trader's net change, in NVDA, within a window of slots,
/// that is its burst rather than its noise.
const BURST_UNITS: f64 = 0.6;
const BURST_SLOTS: u64 = 120;
/// What it pays over the broker ahead of a rise, in basis points.
const UP_EDGE_BPS: f64 = 40.0;
/// Its largest buy a step ahead of a rise.
const UP_SLICE: f64 = 4.0;
/// A fall this far under the session's high, as a share, has landed once
/// it makes no new low for `SETTLE_SLOTS`.
const LANDED: f64 = 0.0035;
const SETTLE_SLOTS: u64 = 80;
/// From here on, in share of the session, it catches up whatever it saw.
const LATE: f64 = 0.85;

/// The strategy.
#[derive(Default)]
pub struct Informed {
    order: Order,
    maker_bps: f64,
    pace: f64,
    /// The informed trader's Phoenix account, NVDA's asset id and a base
    /// lot in NVDA.
    trader: String,
    asset_id: u64,
    lot: f64,
    /// Its position at recent steps.
    seen: VecDeque<(u64, f64)>,
    /// The move's sign once its burst showed.
    sign: f64,
    high: f64,
    low: f64,
    low_at: u64,
    /// The share of the session at which a fall had landed.
    landed: Option<f64>,
}

impl Informed {
    /// The informed trader's NVDA position, in NVDA.
    fn informed_position(&self) -> f64 {
        let Ok(Some(data)) = rpc_account(&self.trader) else {
            return 0.0;
        };
        let Ok(trader) = Trader::try_from_account_bytes(&data) else {
            return 0.0;
        };
        trader
            .positions()
            .find(|(asset, _)| *asset == self.asset_id)
            .map_or(0.0, |(_, p)| {
                p.to_position().base_lot_position.as_inner() as f64 * self.lot
            })
    }
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
        self.trader = venue.trader_account(&key(wallet)?).to_string();
        self.asset_id = u64::from(market.asset_id);
        self.lot = venue.lots_to_base_units(market, 1);
        log(&format!(
            "informed trader {wallet}, account {}",
            self.trader
        ));
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

        // The informed trader's burst, read off its position.
        if self.sign == 0.0 {
            let now = self.informed_position();
            self.seen.push_back((slot, now));
            while self
                .seen
                .front()
                .is_some_and(|(s, _)| s + BURST_SLOTS < slot)
            {
                self.seen.pop_front();
            }
            let moved = now - self.seen.front().map_or(now, |(_, p)| *p);
            if moved.abs() >= BURST_UNITS {
                self.sign = moved.signum();
                log(&format!(
                    "slot {slot}: the informed trader moved {moved:+.3}"
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
