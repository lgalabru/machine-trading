//! A ladder rung for backrun-sol-usdc: the reference, trading a fixed clip
//! of 200 SOL every time rather than the size that profits most, as one
//! mainnet bot did (it took 29% of the optimum, NOTES.md §1). It tips the
//! reference's share ([`backrunner::TIP_SHARE`]), so it wins the races the
//! reference wins and leaves most of each gap: sizing through both curves
//! is a skill. It must score under the reference. (It tipped 0.5, the first
//! reference's share, after the reference moved to 0.34, and won races the
//! reference lost: 1.08 of it on one hidden set.)

#[path = "../../../../reference/src/backrunner.rs"]
pub mod backrunner;

use backrunner::{Backrunner, Bidding, MIN_TIP_LAMPORTS, Opportunity, TIP_SHARE};
use mtx_kit::prelude::*;

/// The clip, in SOL.
const CLIP_SOL: f64 = 200.0;

struct FixedShare;

impl Bidding for FixedShare {
    fn tip(&self, found: &Opportunity) -> Option<u64> {
        let profit_sol = found.profit_sol();
        assert!(profit_sol > 0.0, "a cycle that profits");
        Some(((TIP_SHARE * profit_sol * 1e9) as u64).max(MIN_TIP_LAMPORTS))
    }
}

pub struct FixedClip {
    backrunner: Backrunner,
}

impl Default for FixedClip {
    fn default() -> Self {
        let mut backrunner = Backrunner::default();
        backrunner.clip_sol = Some(CLIP_SOL);
        Self { backrunner }
    }
}

impl Strategy for FixedClip {
    fn start(&mut self, session: &Session) -> BotResult {
        self.backrunner.start(session)
    }

    fn step(&mut self, market: &Market, orders: &mut Orders) -> BotResult {
        self.backrunner.step(market, orders, &FixedShare)
    }
}

#[cfg(target_arch = "wasm32")]
mtx_kit::export!(FixedClip);
