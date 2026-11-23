//! The liquidation-flash reference: sound and simple. Each step it reads
//! SOL's price upstream of Kamino's oracle (Scope's pushed `FixedPrice`
//! entry, in its mappings), values every obligation there, and liquidates
//! each one under water in one atomic group: a push of the price when Scope
//! still reads the obligation healthy, a flash borrow, the liquidation at
//! the close factor, a sale of the seized SOL on the pool that pays more,
//! and the repay ([`liquidator`]). It tips a fixed share of the profit it
//! expects, whoever it races: it knows nothing of the session's
//! liquidators.

pub mod liquidator;

use liquidator::{Bidding, Liquidator, MIN_TIP_LAMPORTS, Opportunity, TIP_SHARE};
use mtx_kit::prelude::*;

/// Tips [`TIP_SHARE`] of the expected profit.
pub struct FixedShare;

impl Bidding for FixedShare {
    fn tip(&self, found: &Opportunity) -> Option<u64> {
        assert!(found.sol_usd > 0.0);
        let lamports = TIP_SHARE * found.profit_usd / found.sol_usd * 1e9;
        Some((lamports as u64).max(MIN_TIP_LAMPORTS))
    }
}

#[derive(Default)]
pub struct Reference {
    liquidator: Liquidator,
}

impl Strategy for Reference {
    fn start(&mut self, session: &Session) -> BotResult {
        self.liquidator.start(session)
    }

    fn step(&mut self, market: &Market, orders: &mut Orders) -> BotResult {
        self.liquidator.step(market, orders, &FixedShare)
    }
}

#[cfg(target_arch = "wasm32")]
mtx_kit::export!(Reference);
