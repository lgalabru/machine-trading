//! The informed bot, scoring v2's 1: told which wallets are the fund's,
//! and nothing of the session's draws.
//!
//! It reads `MTX_REGIME`, what the verifier's `--informed` hands it: the
//! fund's wallets (`wallets.informed`, authorities the session resolves
//! from the latent's roster). It isn't told whether the morning is toxic or
//! quiet (how busy the fund is), the path or its jumps, or how far ahead
//! the fund sees; every session seats the fund, with as many wallets either
//! way. It trades the same market under the same mandate, costs and
//! latency as any bot.
//!
//! Phoenix names traders by their account, a PDA of the wallet, so it maps
//! each wallet to its account and reads their TSLA positions every step.
//! Any net buying or selling by the fund over the last 10 slots sets the
//! side, with the cap, held 40 slots after the fund's last take; between
//! the fund's trades it stays flat.

use mtx_bot_toxic_day_make_or_follow::{Lean, Watch, closing, go_to, make};
use mtx_kit::{log, prelude::*, venue::PhoenixVenue};

/// Slots the fund's flow is summed over, and the least net that counts.
const FUND_SLOTS: u64 = 10;
const FUND_UNITS: f64 = 0.001;

/// The strategy.
pub struct Informed {
    start_slot: u64,
    watch: Watch,
    lean: Lean,
    /// Whether it makes between the fund's trades.
    makes: bool,
}

impl Default for Informed {
    fn default() -> Self {
        Self {
            start_slot: 0,
            watch: Watch::default(),
            lean: Lean::default(),
            makes: option_env!("MTX_INFORMED_MAKE") == Some("1"),
        }
    }
}

impl Strategy for Informed {
    fn start(&mut self, session: &Session) -> BotResult {
        self.start_slot = session.start_slot;
        let told = session
            .var("MTX_REGIME")
            .ok_or("no MTX_REGIME: the informed bot is told the session's latent state")?;
        let told: serde_json::Value = serde_json::from_str(told)?;
        let fund: Vec<String> = told["wallets"]["informed"]
            .as_array()
            .ok_or("MTX_REGIME names no informed wallets")?
            .iter()
            .filter_map(|w| w.as_str().map(str::to_string))
            .collect();
        let venue = PhoenixVenue::from_json(&session.venue)?;
        let accounts = Watch::of_authorities(&venue, &fund)?;
        log(&format!(
            "fund {} wallets, accounts {accounts:?}, makes {}",
            fund.len(),
            self.makes
        ));
        self.watch = Watch::new(&venue, &session.symbol, accounts, FUND_SLOTS + 20)?;
        Ok(())
    }

    fn step(&mut self, market: &Market, orders: &mut Orders) -> BotResult {
        let at = market.slot.saturating_sub(self.start_slot);
        orders.cancel_all();
        self.watch.read(at)?;
        let net: f64 = self.watch.changes(FUND_SLOTS).iter().sum();
        if net.abs() >= FUND_UNITS {
            self.lean.signal(net.signum(), at);
        }
        self.lean.manage(at);
        if closing(market) {
            go_to(0.0, market, orders);
        } else if self.lean.dir != 0.0 {
            go_to(self.lean.dir * market.cap, market, orders);
        } else if self.makes && market.position.size.abs() <= market.cap * 0.75 {
            make(market, orders);
        } else {
            go_to(0.0, market, orders);
        }
        Ok(())
    }
}

#[cfg(target_arch = "wasm32")]
mtx_kit::export!(Informed);
