//! The informed bot, scoring v2's 1: told which wallets are the informed
//! fund's, and nothing of the session's draws.
//!
//! It reads `MTX_REGIME`, what the verifier's `--informed` hands it: the
//! informed fund's wallets (`wallets.informed`, authorities the session
//! resolves from the latent's roster). It isn't told when the headline
//! prints, which way it jumps, when the verdict comes or which way, or how
//! far ahead the fund sees; both kinds of session seat the same funds. It
//! trades the same market under the same mandate, costs and latency as any
//! bot.
//!
//! Phoenix names traders by their account, a PDA of the wallet, so it maps
//! each wallet to its account and reads their TSLA positions every step.
//! Flat through the headline; once the jump is done, the fund's net buying
//! over the last 30 slots past 1.2 TSLA either way sets the side, with the
//! cap, until six slots after the verdict.

use mtx_bot_event_follow_through::{
    FLOW_SLOTS, Headline, Hold, Watch, flow_side, go_to, mid,
};
use mtx_kit::{log, prelude::*, venue::PhoenixVenue};

/// The strategy.
#[derive(Default)]
pub struct Informed {
    start_slot: u64,
    watch: Watch,
    headline: Headline,
    hold: Hold,
}

impl Strategy for Informed {
    fn start(&mut self, session: &Session) -> BotResult {
        self.start_slot = session.start_slot;
        let told = session
            .var("MTX_REGIME")
            .ok_or("no MTX_REGIME: the informed bot is told the session's latent state")?;
        let told: serde_json::Value = serde_json::from_str(told)?;
        let informed: Vec<String> = told["wallets"]["informed"]
            .as_array()
            .ok_or("MTX_REGIME names no informed wallets")?
            .iter()
            .filter_map(|w| w.as_str().map(str::to_string))
            .collect();
        let venue = PhoenixVenue::from_json(&session.venue)?;
        let accounts = Watch::of_authorities(&venue, &informed)?;
        log(&format!("informed {informed:?}, accounts {accounts:?}"));
        self.watch = Watch::new(&venue, &session.symbol, accounts, FLOW_SLOTS + 20)?;
        Ok(())
    }

    fn step(&mut self, market: &Market, orders: &mut Orders) -> BotResult {
        let at = market.slot.saturating_sub(self.start_slot);
        orders.cancel_all();
        self.watch.read(at)?;
        self.headline.update(at, market.oracle);
        if self.headline.settled.is_some() && self.headline.verdict.is_none() {
            let net: f64 = self
                .watch
                .changes(self.headline.flow_slots(at))
                .iter()
                .sum();
            if let Some(dir) = flow_side(net) {
                self.hold.want(dir, at);
            }
        }
        self.hold.manage(at, &self.headline);
        if mid(market).is_some() {
            go_to(self.hold.target(market), market, orders);
        }
        Ok(())
    }
}
