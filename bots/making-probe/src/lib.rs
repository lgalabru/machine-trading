//! Probes for a market-making session's mandate: quoting obligations and
//! reduce-only taking (`bench/checks/making-mandate`). Each quotes around
//! the oracle, skewed against what it holds; the feature it is built with
//! says what else it does.
//!
//! - default: a two-sided maker, post-only on both sides inside the band
//!   at the minimum size. It meets its obligations and never takes.
//! - `one-sided`: the same, but one side at a time: bids until it holds a
//!   few units, then asks.
//! - `taker`: the maker, plus a market order every few steps the way that
//!   adds to its position (a buy when flat). The grader refuses each one
//!   that would open or add.
//! - `crosser`: the maker, but every few steps it cancels and sends a
//!   plain bid through the best ask while its own ask was resting. The
//!   grader can't tell before the send that the touch isn't the bot's own,
//!   so the order goes, opens a position as a taker, and the audit
//!   catches it.

use mtx_kit::prelude::*;

/// How far from the oracle each side sits, in basis points.
const HALF_SPREAD_BPS: f64 = 10.0;
/// How far the quotes shift per unit held, in basis points.
const SKEW_BPS_PER_UNIT: f64 = 1.0;
/// Each side's size, in base units.
const SIZE: f64 = 1.0;
/// What the one-sided probe holds before it turns from bidding to asking.
const ONE_SIDED_TURN: f64 = 5.0;
/// Steps between the probe's takes.
const EVERY: u64 = 10;

#[derive(Default)]
struct Probe;

impl Strategy for Probe {
    fn step(&mut self, market: &Market, orders: &mut Orders) -> BotResult {
        let held = market.position.size;
        if cfg!(feature = "taker") && market.k % EVERY == EVERY - 1 {
            if held >= 0.0 {
                orders.buy(SIZE, market.oracle * 1.01);
            } else {
                orders.sell(SIZE, market.oracle * 0.99);
            }
        }
        if cfg!(feature = "crosser")
            && market.k % EVERY == EVERY - 1
            && market.resting_asks > 0.0
            && let Some(ask) = market.best_ask
        {
            orders.cancel_all();
            orders.bid(ask * 1.005, SIZE);
            return Ok(());
        }
        let reservation = market.oracle * (1.0 - SKEW_BPS_PER_UNIT * held / 10_000.0);
        let bid = reservation * (1.0 - HALF_SPREAD_BPS / 10_000.0);
        let ask = reservation * (1.0 + HALF_SPREAD_BPS / 10_000.0);
        if cfg!(feature = "one-sided") {
            orders.cancel_all();
            if held < ONE_SIDED_TURN {
                orders.bid(bid, SIZE).post_only();
            } else {
                orders.ask(ask, SIZE).post_only();
            }
        } else {
            orders.quote(bid, ask, SIZE).post_only();
        }
        Ok(())
    }
}

mtx_kit::export!(Probe);
