//! A ladder rung for backrun-thin-pools: the reference's search and sizing,
//! always tipping the least the layer takes (1,000 lamports), whoever it
//! races. It keeps all of every race it wins, and wins only those no
//! tipping rival reaches. Blind cheap bidding is a habit that must not
//! pay: knowing when a low bid is safe is the informed bot's edge, so this
//! rung must score under the reference.

#[path = "../../../../reference/src/cycles.rs"]
pub mod cycles;

use cycles::{Backrunner, Bidding, MIN_TIP_LAMPORTS, Opportunity};
use mtx_kit::prelude::*;

/// It looks as deep as the reference.
const MAX_LEGS: usize = 3;

struct Least;

impl Bidding for Least {
    fn tip(&self, found: &Opportunity) -> Option<u64> {
        assert!(found.profit_sol() > 0.0, "a cycle that profits");
        Some(MIN_TIP_LAMPORTS)
    }
}

pub struct MinTip {
    backrunner: Backrunner,
}

impl Default for MinTip {
    fn default() -> Self {
        Self {
            backrunner: Backrunner::new(MAX_LEGS),
        }
    }
}

impl Strategy for MinTip {
    fn start(&mut self, session: &Session) -> BotResult {
        self.backrunner.start(session)
    }

    fn step(&mut self, market: &Market, orders: &mut Orders) -> BotResult {
        self.backrunner.step(market, orders, &Least)
    }
}

#[cfg(target_arch = "wasm32")]
mtx_kit::export!(MinTip);
