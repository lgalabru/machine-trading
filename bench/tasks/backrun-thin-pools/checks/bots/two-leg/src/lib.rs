//! A ladder rung for backrun-thin-pools: the reference, looking only at a
//! pair's two pools, as some of the session's searchers do. It catches the
//! triggers that open a two-leg cycle and misses every one whose gap only
//! a cycle through a third token closes: searching the graph is the skill,
//! and a bot that doesn't must score under the reference.

#[path = "../../../../reference/src/cycles.rs"]
pub mod cycles;

use cycles::{Backrunner, Bidding, MIN_TIP_LAMPORTS, Opportunity};
use mtx_kit::prelude::*;

/// The reference's tip share.
const TIP_SHARE: f64 = 0.5;
/// A pair's two pools: two legs.
const MAX_LEGS: usize = 2;

struct Half;

impl Bidding for Half {
    fn tip(&self, found: &Opportunity) -> Option<u64> {
        let profit_sol = found.profit_sol();
        assert!(profit_sol > 0.0, "a cycle that profits");
        Some(((TIP_SHARE * profit_sol * 1e9) as u64).max(MIN_TIP_LAMPORTS))
    }
}

pub struct TwoLeg {
    backrunner: Backrunner,
}

impl Default for TwoLeg {
    fn default() -> Self {
        Self {
            backrunner: Backrunner::new(MAX_LEGS),
        }
    }
}

impl Strategy for TwoLeg {
    fn start(&mut self, session: &Session) -> BotResult {
        self.backrunner.start(session)
    }

    fn step(&mut self, market: &Market, orders: &mut Orders) -> BotResult {
        self.backrunner.step(market, orders, &Half)
    }
}

#[cfg(target_arch = "wasm32")]
mtx_kit::export!(TwoLeg);
