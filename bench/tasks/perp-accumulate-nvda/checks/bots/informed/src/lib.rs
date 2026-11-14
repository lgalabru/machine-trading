//! The informed bot, scoring v2's 1: the reference's order handling, told
//! what the session's data could tell in principle and nothing of its
//! future randomness.
//!
//! It reads `MTX_REGIME`, the session's shape as the generator drew it:
//! the regime (calm, up or down), the move's onset, last step and size, and
//! the grader's maker (its half-spread, quote size and look delay) and the
//! makers' backstop. It doesn't know the walk, the noise flow, or where the
//! fake-outs fall, and it trades the same market under the same mandate,
//! costs and latency as any bot.
//!
//! - **Up:** buys the order before the move is half done, at the pace the
//!   maker refills, paying up to 80% of the move over the broker's haircut
//!   when time runs short.
//! - **Down:** buys nothing, and rests nothing, until the move is done;
//!   then works the order as in a calm session.
//! - **Calm:** rests a bid a tick inside the maker and catches up behind a
//!   back-loaded floor, crossing only while the ask beats the broker.

use mtx_bot_perp_accumulate::{Order, number_after, round_down_cents, round_lots};
use mtx_kit::{log, prelude::*};

/// The session's shape, from `MTX_REGIME`.
#[derive(Debug, Clone, Copy, Default)]
pub struct Shape {
    /// +1 up, -1 down, 0 calm.
    pub sign: f64,
    pub onset: u64,
    pub end: u64,
    pub move_bps: f64,
    pub maker_bps: f64,
    pub maker_size: f64,
    pub react_slots: f64,
}

impl Shape {
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
            onset: n("onset").unwrap_or(0.0) as u64,
            end: n("end").unwrap_or(0.0) as u64,
            move_bps: n("move_pct").unwrap_or(0.0).abs() * 100.0,
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
    shape: Shape,
}

impl Strategy for Informed {
    fn start(&mut self, session: &Session) -> BotResult {
        self.order = Order::from_session(session)?;
        let told = session.var("MTX_REGIME").ok_or("no MTX_REGIME: the informed bot is told the session")?;
        self.shape = Shape::parse(told);
        log(&format!("told {told}"));
        Ok(())
    }

    fn step(&mut self, market: &Market, orders: &mut Orders) -> BotResult {
        let shape = self.shape;
        let at = market.slot.saturating_sub(self.order.start_slot);
        let held = market.position.size;
        let oracle = market.oracle;
        let q = self.order.quantity;
        let left = (q - held).max(0.0);
        orders.cancel_all();
        if left < 1e-3 {
            return Ok(());
        }
        let t = self.order.progress(market.slot);
        // What the maker refills a step, at its own price.
        let pace = shape.maker_size * 2.0 / shape.react_slots.max(2.0);
        let (floor, edge, size, rest) = if shape.sign > 0.0 && at < (shape.onset + shape.end) / 2 {
            let target = (shape.onset + shape.end) / 2;
            let steps = (target.saturating_sub(at) / 2).max(1) as f64;
            (q, 0.8 * shape.move_bps, pace.max(left / steps), false)
        } else if shape.sign < 0.0 && at <= shape.end {
            return Ok(());
        } else {
            (q * (t / 0.95).min(1.0).powi(3), 0.0, pace.max(0.5), true)
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
            let bid = oracle * (1.0 - shape.maker_bps / 10_000.0);
            let price = round_down_cents((bid + 0.01).min(self.order.max_buy_price));
            let size = round_lots(1.0f64.min(left - sent));
            if size > 0.0 && market.best_ask.is_none_or(|ask| price < ask) {
                orders.bid(price, size).post_only();
            }
        }
        Ok(())
    }
}
