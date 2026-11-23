//! A ladder rung for backrun-sol-usdc: the reference, tipping nearly all
//! the profit it expects, as mainnet's highest bidders paid 93% of their
//! gross. It wins what the reference wins and a little more, and keeps
//! almost none of it: overbidding every race is a habit that loses. It
//! must score under the reference.

#[path = "../../../../reference/src/backrunner.rs"]
pub mod backrunner;

use backrunner::{Backrunner, Bidding, MIN_TIP_LAMPORTS, Opportunity};
use mtx_kit::prelude::*;

/// The share of the expected profit it tips.
const TIP_SHARE: f64 = 0.95;

struct NearlyAll;

impl Bidding for NearlyAll {
    fn tip(&self, found: &Opportunity) -> Option<u64> {
        let profit_sol = found.profit_sol();
        assert!(profit_sol > 0.0, "a cycle that profits");
        Some(((TIP_SHARE * profit_sol * 1e9) as u64).max(MIN_TIP_LAMPORTS))
    }
}

#[derive(Default)]
pub struct Overbidder {
    backrunner: Backrunner,
}

impl Strategy for Overbidder {
    fn start(&mut self, session: &Session) -> BotResult {
        self.backrunner.start(session)
    }

    fn step(&mut self, market: &Market, orders: &mut Orders) -> BotResult {
        self.backrunner.step(market, orders, &NearlyAll)
    }
}

#[cfg(target_arch = "wasm32")]
mtx_kit::export!(Overbidder);
