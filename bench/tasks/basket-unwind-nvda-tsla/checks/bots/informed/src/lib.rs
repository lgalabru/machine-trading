//! The informed bot, scoring v2's 1: the reference's desk, told each
//! market's informed wallet and nothing of the session's draws.
//!
//! It reads `MTX_REGIME`, what the verifier's `--informed` hands it:
//! `wallets.informed-nvda` and `wallets.informed-tsla`, resolved by the
//! session from the latent's roster. Each market's informed trader is
//! seated in every session, calm or not, and trades noise like any account.
//! It isn't told which market moves, which way, when, or who sends the
//! fake-outs.
//!
//! - It watches each told wallet's takes on its market. Four or more one
//!   way within 100 slots is its burst, and its sign is the move's.
//! - **Selling:** that name takes the capacity first until it is gone,
//!   sweeping to the backstop if it must.
//! - **Buying:** it sells nothing there until the rise has landed.
//! - **Until then, or calm:** it works the name as the reference does.
//!
//! Without allocation (`pro_rata`), each name gets its share of the cap:
//! the ladder's diagnostic for whether the cap binds.

use mtx_bot_basket_unwind_nvda::{Mode, Unwind};

/// The informed bot.
pub struct Informed(pub Unwind);

impl Default for Informed {
    fn default() -> Self {
        let allocate = option_env!("MTX_B5_PRO_RATA").is_none();
        Self(Unwind::new(Mode::Informed, allocate))
    }
}

impl mtx_kit::Strategy for Informed {
    fn start(&mut self, session: &mtx_kit::Session) -> mtx_kit::BotResult {
        self.0.start(session)
    }

    fn step(
        &mut self,
        market: &mtx_kit::Market,
        orders: &mut mtx_kit::Orders,
    ) -> mtx_kit::BotResult {
        self.0.step(market, orders)
    }
}

#[cfg(target_arch = "wasm32")]
mtx_kit::export!(Informed);
