//! A ladder rung for liquidation-flash: the reference's detection and
//! group, always tipping the least the layer takes (1,000 lamports),
//! whoever it races. It keeps all of every race it wins, and wins only
//! those no rival reaches in its slot. Blind cheap bidding is a habit that
//! must not pay: knowing when a low bid is safe is the informed bot's
//! edge, so this rung must score under the reference.

#[path = "../../../../reference/src/liquidator.rs"]
pub mod liquidator;

use liquidator::{Bidding, Liquidator, MIN_TIP_LAMPORTS, Opportunity};
use mtx_kit::prelude::*;

struct Least;

impl Bidding for Least {
    fn tip(&self, found: &Opportunity) -> Option<u64> {
        assert!(found.sol_usd > 0.0, "SOL has a price");
        Some(MIN_TIP_LAMPORTS)
    }
}

#[derive(Default)]
pub struct MinTip {
    liquidator: Liquidator,
}

impl Strategy for MinTip {
    fn start(&mut self, session: &Session) -> BotResult {
        self.liquidator.start(session)
    }

    fn step(&mut self, market: &Market, orders: &mut Orders) -> BotResult {
        self.liquidator.step(market, orders, &Least)
    }
}

#[cfg(target_arch = "wasm32")]
mtx_kit::export!(MinTip);
