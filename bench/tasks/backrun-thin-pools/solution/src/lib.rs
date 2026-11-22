//! The backrun-thin-pools reference: sound and simple. On each trigger step
//! (a large swap has just landed on one of the ten pools, its slot still
//! open) it reads every pool, lists the cycles from SOL back to SOL of two
//! and three legs, skips those whose fees eat the gap, sizes the rest on
//! exact quotes, and sends the best as one atomic group whose last limit is
//! its least profit ([`cycles`]). It tips a fixed share of the profit it
//! expects, whoever it races: it knows nothing of the session's searchers.

pub mod cycles;

use cycles::{Backrunner, Bidding, MIN_TIP_LAMPORTS, Opportunity};
use mtx_kit::prelude::*;

/// The share of the profit it expects that it tips. Mainnet's backrun
/// winners paid 0 to 93% of their gross (`backrun-sol-usdc`'s research);
/// half outbids every searcher that tips less, whoever reaches the slot,
/// and keeps half of every race it wins (DESIGN.md, "Calibration").
pub const TIP_SHARE: f64 = 0.5;
/// The most legs it looks at: the snapshot's cycles are of two and three.
pub const MAX_LEGS: usize = 3;

/// Tips [`TIP_SHARE`] of the expected profit.
pub struct FixedShare;

impl Bidding for FixedShare {
    fn tip(&self, found: &Opportunity) -> Option<u64> {
        let profit_sol = found.profit_sol();
        assert!(profit_sol > 0.0, "a cycle that profits");
        Some(((TIP_SHARE * profit_sol * 1e9) as u64).max(MIN_TIP_LAMPORTS))
    }
}

pub struct Reference {
    backrunner: Backrunner,
}

impl Default for Reference {
    fn default() -> Self {
        Self {
            backrunner: Backrunner::new(MAX_LEGS),
        }
    }
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
