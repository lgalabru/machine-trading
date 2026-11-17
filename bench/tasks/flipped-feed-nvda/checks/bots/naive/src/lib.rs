//! The ladder's rungs that trade without reading which gateway is which. A
//! component has no environment, so each rung is its own build, its rule
//! set by `MTX_NAIVE` at build time:
//!
//! - `follow-all`: takes every call as published.
//! - `fade-all`: takes every call the other way.
//! - `follow-first`: trusts the first gateway `MTX_FEED` lists, as a desk
//!   that never checks the convention trusts its usual one.
//! - `post-move`: no convention at all: [`POST_MOVE_WAIT`] slots after a
//!   call, it follows the index's move since the call, if it moved over
//!   [`POST_MOVE_BPS`], for [`POST_MOVE_HOLD`] slots.
//! - `twap`: buys evenly to the cap over the first half of the session and
//!   sells evenly back to flat over the second.
//!
//! Each trades at the reference's size and costs.

use mtx_bot_flipped_feed::{Feed, Hold, SIZE, direction, trade_to};
use mtx_kit::prelude::*;

/// The post-move rung's wait after a call, the least move it follows, in
/// basis points, and how long it holds, in slots: the feasibility model's
/// best on its training set (40, 5 and 150 of its 0.4 s slots).
pub const POST_MOVE_WAIT: u64 = 64;
pub const POST_MOVE_BPS: f64 = 5.0;
pub const POST_MOVE_HOLD: u64 = 240;

#[derive(Default)]
pub struct Naive {
    mode: String,
    feed: Feed,
    hold: Hold,
    start: u64,
    end: u64,
    /// The calls the post-move rung has looked at.
    looked: usize,
}

impl Strategy for Naive {
    fn start(&mut self, session: &Session) -> BotResult {
        self.mode = option_env!("MTX_NAIVE")
            .ok_or("built without MTX_NAIVE")?
            .to_string();
        if self.mode != "twap" {
            self.feed = Feed::from_session(session)?;
        }
        self.start = session.start_slot;
        self.end = session.end_slot;
        Ok(())
    }

    fn step(&mut self, market: &Market, orders: &mut Orders) -> BotResult {
        if self.mode == "twap" {
            let span = self.end.saturating_sub(self.start).max(1) as f64;
            let progress = (market.slot.saturating_sub(self.start) as f64 / span).min(1.0);
            let ramp = if progress < 0.5 {
                2.0 * progress
            } else {
                2.0 * (1.0 - progress)
            };
            let target = (SIZE * ramp * 4.0).round() / 4.0;
            trade_to(market, orders, target);
            return Ok(());
        }
        let new = self.feed.poll(market);
        match self.mode.as_str() {
            "follow-all" | "fade-all" | "follow-first" => {
                for i in new {
                    let call = &self.feed.calls[i];
                    let dir = match self.mode.as_str() {
                        "follow-all" => call.sign,
                        "fade-all" => -call.sign,
                        _ => direction(call, 0),
                    };
                    self.hold.take(dir, call.slot + call.horizon);
                }
            }
            "post-move" => {
                while let Some(call) = self.feed.calls.get(self.looked) {
                    if market.slot < call.slot + POST_MOVE_WAIT {
                        break;
                    }
                    self.looked += 1;
                    let moved = (market.index / call.index_at - 1.0) * 10_000.0;
                    if moved.abs() >= POST_MOVE_BPS {
                        self.hold.take(moved.signum(), market.slot + POST_MOVE_HOLD);
                    }
                }
            }
            other => return Err(format!("no rung {other}").into()),
        }
        trade_to(market, orders, self.hold.target(market.slot, SIZE));
        Ok(())
    }
}

#[cfg(target_arch = "wasm32")]
mtx_kit::export!(Naive);
