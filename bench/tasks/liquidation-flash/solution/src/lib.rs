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

use liquidator::{Bidding, Liquidator, MIN_TIP_LAMPORTS, Opportunity};
use mtx_kit::prelude::*;

/// The share of the profit it expects that it tips: mainnet's liquidators
/// paid 25 to 45% of the bonus they kept in fees and tips (NOTES.md §3,
/// the top of that range), and a bundle ranks by its tip per compute unit,
/// of which its group of a push, a flash loan, a liquidation and a sale
/// asks about 1.45 times a liquidator's: 0.45 × 1.45 ≈ 0.65 outbids a
/// liquidator at the research's highest share.
const TIP_SHARE: f64 = 0.65;

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
