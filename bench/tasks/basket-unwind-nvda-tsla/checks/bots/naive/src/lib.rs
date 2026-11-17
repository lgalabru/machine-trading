//! The ladder's rungs other than the anchors, each the reference's desk
//! with one rule, set at build time (`MTX_B5_RUNG`, and `MTX_B5_PRO_RATA`
//! to hold each name to its share of the cap):
//!
//! - `dump`: both names as fast as the cap lets it, pro rata;
//! - `twap-each`: equal crossing slices of each name over the session;
//! - `pro-rata`: asks resting in both names, crossing behind a straight
//!   schedule while the bid beats the broker;
//! - `bigger-first`: the bigger notional over the first half, the other
//!   over the second;
//! - `reference`, with `MTX_B5_PRO_RATA`: the reference's reads, each name
//!   held to its share of the cap.

use mtx_bot_basket_unwind_nvda::{Mode, Unwind};

/// The rung.
pub struct Rung(pub Unwind);

impl Default for Rung {
    fn default() -> Self {
        let mode = option_env!("MTX_B5_RUNG")
            .and_then(Mode::parse)
            .unwrap_or(Mode::ProRata);
        let allocate = !matches!(mode, Mode::Dump | Mode::TwapEach | Mode::ProRata)
            && option_env!("MTX_B5_PRO_RATA").is_none();
        Self(Unwind::new(mode, allocate))
    }
}

impl mtx_kit::Strategy for Rung {
    fn start(&mut self, session: &mtx_kit::Session) -> mtx_kit::BotResult {
        self.0.start(session)
    }

    fn step(&mut self, market: &mtx_kit::Market, orders: &mut mtx_kit::Orders) -> mtx_kit::BotResult {
        self.0.step(market, orders)
    }
}

#[cfg(target_arch = "wasm32")]
mtx_kit::export!(Rung);
