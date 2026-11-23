//! A ladder rung for liquidation-flash: the reference, reading Scope's own
//! price instead of the upstream. It liquidates only what Scope already
//! shows under water, so it never pushes, and it is a push too late for
//! every race a liquidator that reads upstream enters (RESEARCH.md: a bot
//! that only reads Scope on chain is too late for anything over $10). It
//! tips the reference's share ([`liquidator::TIP_SHARE`]; it tipped 0.65,
//! the first reference's, after the reference moved to 0.36). It must
//! score under the reference.

#[path = "../../../../reference/src/liquidator.rs"]
pub mod liquidator;

use liquidator::{Bidding, Liquidator, MIN_TIP_LAMPORTS, Opportunity, PriceSource, TIP_SHARE};
use mtx_kit::prelude::*;

struct FixedShare;

impl Bidding for FixedShare {
    fn tip(&self, found: &Opportunity) -> Option<u64> {
        assert!(found.sol_usd > 0.0);
        Some(((TIP_SHARE * found.profit_usd / found.sol_usd * 1e9) as u64).max(MIN_TIP_LAMPORTS))
    }
}

pub struct ScopeReader {
    liquidator: Liquidator,
}

impl Default for ScopeReader {
    fn default() -> Self {
        Self {
            liquidator: Liquidator::reading(PriceSource::Scope),
        }
    }
}

impl Strategy for ScopeReader {
    fn start(&mut self, session: &Session) -> BotResult {
        self.liquidator.start(session)
    }

    fn step(&mut self, market: &Market, orders: &mut Orders) -> BotResult {
        self.liquidator.step(market, orders, &FixedShare)
    }
}

#[cfg(target_arch = "wasm32")]
mtx_kit::export!(ScopeReader);
