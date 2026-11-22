//! A ladder rung for liquidation-flash: the reference, tipping nearly all
//! the profit it expects. It wins what the reference wins and a little
//! more, and keeps almost none of it: overbidding every race is a habit
//! that loses. It must score under the reference.

#[path = "../../../../reference/src/liquidator.rs"]
pub mod liquidator;

use liquidator::{Bidding, Liquidator, MIN_TIP_LAMPORTS, Opportunity};
use mtx_kit::prelude::*;

/// The share of the expected profit it tips.
const TIP_SHARE: f64 = 0.95;

struct NearlyAll;

impl Bidding for NearlyAll {
    fn tip(&self, found: &Opportunity) -> Option<u64> {
        assert!(found.sol_usd > 0.0);
        Some(((TIP_SHARE * found.profit_usd / found.sol_usd * 1e9) as u64).max(MIN_TIP_LAMPORTS))
    }
}

#[derive(Default)]
pub struct Overbidder {
    liquidator: Liquidator,
}

impl Strategy for Overbidder {
    fn start(&mut self, session: &Session) -> BotResult {
        self.liquidator.start(session)
    }

    fn step(&mut self, market: &Market, orders: &mut Orders) -> BotResult {
        self.liquidator.step(market, orders, &NearlyAll)
    }
}

#[cfg(target_arch = "wasm32")]
mtx_kit::export!(Overbidder);
