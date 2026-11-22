//! The backrun-sol-usdc reference: sound and simple. On each trigger step
//! (a large swap has just landed, its slot still open) it reads both pools,
//! sizes the SOL→USDC→SOL cycle that profits most on exact quotes, and
//! sends it as one atomic group whose last limit is its least profit
//! ([`backrunner`]). It tips a fixed share of the profit it expects,
//! whoever it races: it knows nothing of the session's searchers.

pub mod backrunner;

use backrunner::{Backrunner, Bidding, MIN_TIP_LAMPORTS, Opportunity};
use mtx_kit::prelude::*;

/// The share of the profit it expects that it tips. Mainnet's winners paid
/// 0 to 93% of their gross, a third of them about 34% (NOTES.md §1); half
/// outbids every searcher that tips less, whoever reaches the slot, and
/// keeps half of every race it wins. It put the reference at 0.55 of the
/// informed bot on 72 hidden sessions, inside v2's strength band
/// (DESIGN.md, "Calibration").
pub const TIP_SHARE: f64 = 0.5;

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
