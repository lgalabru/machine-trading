//! The informed bot, scoring v2's 1: the reference's order handling, told
//! the session's latent state and nothing of its future.
//!
//! It reads `MTX_REGIME`, what the verifier's `--informed` hands it: the
//! session's kind (calm, or an informed move up or down) and its book (the
//! grader's maker's half-spread, quote size and look delay, and the makers'
//! backstop). It isn't told when the move starts or lands, how far it goes,
//! the walk, the noise flow or the fake-outs, and it trades the same market
//! under the same mandate, costs and latency as any bot.
//!
//! - **Up:** the move is coming, at least 0.43% and at most 0.85%: it buys
//!   the order in the first tenth of the session, at the maker when it can
//!   and on the backstop when it must, paying up to 40 bp over the broker.
//! - **Down:** it buys nothing, and rests nothing, until the move has
//!   landed: the oracle 0.35% under the session's high, and no new low for
//!   80 slots. Then it works the rest as in a calm session.
//! - **Calm:** it rests a bid a tick inside the maker and catches up behind
//!   a back-loaded floor, crossing only while the ask beats the broker.

use mtx_bot_perp_accumulate::{Order, number_after, round_down_cents, round_lots};
use mtx_kit::{log, prelude::*};

/// What it pays over the broker ahead of a rise, in basis points.
const UP_EDGE_BPS: f64 = 40.0;
/// The share of the session it front-loads an up session in.
const UP_SHARE: f64 = 0.1;
/// A fall this far under the session's high, as a share, has landed once
/// it makes no new low for `SETTLE_SLOTS`.
const LANDED: f64 = 0.0035;
const SETTLE_SLOTS: u64 = 80;
/// From here on, in share of the session, it catches up whatever it saw.
const LATE: f64 = 0.85;

/// The session's latent state, from `MTX_REGIME`.
#[derive(Debug, Clone, Copy, Default)]
pub struct Latent {
    /// +1 up, -1 down, 0 calm.
    pub sign: f64,
    pub maker_bps: f64,
    pub maker_size: f64,
    pub react_slots: f64,
}

impl Latent {
    pub fn parse(json: &str) -> Self {
        let sign = if json.contains("\"regime\":\"up\"") {
            1.0
        } else if json.contains("\"regime\":\"down\"") {
            -1.0
        } else {
            0.0
        };
        let n = |key: &str| number_after(json, &format!("\"{key}\""));
        Self {
            sign,
            maker_bps: n("maker_bps").unwrap_or(4.0),
            maker_size: n("maker_size").unwrap_or(2.0),
            react_slots: n("react_slots").unwrap_or(6.0),
        }
    }
}

/// The strategy.
#[derive(Default)]
pub struct Informed {
    order: Order,
    latent: Latent,
    high: f64,
    low: f64,
    low_at: u64,
    /// The share of the session at which a down move had landed.
    landed: Option<f64>,
}

impl Strategy for Informed {
    fn start(&mut self, session: &Session) -> BotResult {
        self.order = Order::from_session(session)?;
        let told = session
            .var("MTX_REGIME")
            .ok_or("no MTX_REGIME: the informed bot is told the session's latent state")?;
        self.latent = Latent::parse(told);
        log(&format!("told {told}"));
        Ok(())
    }

    fn step(&mut self, market: &Market, orders: &mut Orders) -> BotResult {
        let latent = self.latent;
        let slot = market.slot;
        let held = market.position.size;
        let oracle = market.oracle;
        let q = self.order.quantity;
        let left = (q - held).max(0.0);
        let t = self.order.progress(slot);
        orders.cancel_all();

        // Where a fall stands: the session's high, and its low since then.
        if oracle > self.high {
            (self.high, self.low, self.low_at) = (oracle, oracle, slot);
        } else if oracle < self.low {
            (self.low, self.low_at) = (oracle, slot);
        }
        if self.landed.is_none()
            && self.low < self.high * (1.0 - LANDED)
            && slot >= self.low_at + SETTLE_SLOTS
        {
            self.landed = Some(t);
        }
        if left < 1e-3 {
            return Ok(());
        }

        // What the maker refills a step, at its own price.
        let pace = latent.maker_size * 2.0 / latent.react_slots.max(2.0);
        let calm_floor = |from: f64| {
            let x = ((t - from) / (0.95 - from).max(0.05)).clamp(0.0, 1.0);
            q * x.powi(3)
        };
        let (floor, edge, size, rest) = if latent.sign > 0.0 {
            let steps = ((UP_SHARE - t).max(0.0) * self.order.span_steps()).max(1.0);
            (q, UP_EDGE_BPS, pace.max(left / steps), false)
        } else if latent.sign < 0.0 {
            match self.landed {
                Some(from) => (calm_floor(from), 0.0, pace.max(0.5), true),
                None if t > LATE => (calm_floor(0.0), 0.0, pace.max(0.5), true),
                None => return Ok(()),
            }
        } else {
            (calm_floor(0.0), 0.0, pace.max(0.5), true)
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
            let bid = oracle * (1.0 - latent.maker_bps / 10_000.0);
            let price = round_down_cents((bid + 0.01).min(self.order.max_buy_price));
            let size = round_lots(1.0f64.min(left - sent));
            if size > 0.0 && market.best_ask.is_none_or(|ask| price < ask) {
                orders.bid(price, size).post_only();
            }
        }
        Ok(())
    }
}
