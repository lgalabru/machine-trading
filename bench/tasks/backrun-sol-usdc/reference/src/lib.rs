//! The backrun-sol-usdc reference: sound and simple. On each trigger step
//! (a large swap has just landed, its slot still open) it reads both pools,
//! sizes the SOL→USDC→SOL cycle that profits most on exact quotes, and
//! sends it as one atomic group whose last limit is its least profit
//! ([`backrunner`]). It tips a fixed share of the profit it expects,
//! whoever it races: it knows nothing of the session's searchers.

pub mod backrunner;

use backrunner::{Backrunner, Bidding, MIN_TIP_LAMPORTS, Opportunity};
use mtx_kit::prelude::*;

/// The share of the profit it expects that it tips: what mainnet's winners
/// paid the leader at the median, of their gross (NOTES.md §1: 0% in 7 of
/// 20, about 34% in 6, 50 to 80% in 4, 93% in 3), and what the week's top
/// winner, MriyaN, bid on every win. A principle, not a fit: it knows
/// nothing of the session's searchers. It outbids every searcher in the
/// trigger's slot that pays less per unit, a tip or a priority fee, and
/// keeps two thirds of every race it wins (DESIGN.md, "Calibration").
pub const TIP_SHARE: f64 = 0.34;

/// Tips [`TIP_SHARE`] of the expected profit.
pub struct FixedShare;

impl Bidding for FixedShare {
    fn tip(&self, found: &Opportunity) -> Option<u64> {
        let profit_sol = found.profit_sol();
        assert!(profit_sol > 0.0, "a cycle that profits");
        Some(((TIP_SHARE * profit_sol * 1e9) as u64).max(MIN_TIP_LAMPORTS))
    }
}

#[derive(Default)]
pub struct Reference {
    backrunner: Backrunner,
}

impl Strategy for Reference {
    fn start(&mut self, session: &Session) -> BotResult {
        self.backrunner.start(session)
    }

    fn step(&mut self, market: &Market, orders: &mut Orders) -> BotResult {
        self.backrunner.step(market, orders, &FixedShare)
    }
}

#[cfg(target_arch = "wasm32")]
mtx_kit::export!(Reference);
