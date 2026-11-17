//! The informed bot, scoring v2's 1: the reference's feed reading and
//! trading, told which gateway is which, and nothing of the session's draws.
//!
//! It reads `MTX_REGIME`, what the verifier's `--informed` hands it: the
//! wallet of the gateway that reports the house model's own side
//! (`wallets.gateway-taker-side`) and of the one that reports the opposite
//! (`wallets.gateway-maker-side`), authorities the session resolves from
//! the latent's roster. It isn't told when a call comes, which way, whether
//! the model is right this time, or the path, and it trades the same market
//! under the same mandate, costs and latency as any bot.
//!
//! From its first call it follows each call from the model's own gateway
//! and fades each from the other, at the mandate's cap, until the call's horizon
//! has passed.

use mtx_bot_flipped_feed::{Feed, Hold, direction, trade_to};
use mtx_kit::prelude::*;

/// The strategy.
#[derive(Default)]
pub struct Informed {
    size: f64,
    feed: Feed,
    taker_side: usize,
    hold: Hold,
}

/// The first wallet listed under `kind` in the told state.
fn wallet(told: &str, kind: &str) -> Option<String> {
    let told: serde_json::Value = serde_json::from_str(told).ok()?;
    told["wallets"][kind][0].as_str().map(str::to_string)
}

impl Strategy for Informed {
    fn start(&mut self, session: &Session) -> BotResult {
        self.size = session.cap;
        self.feed = Feed::from_session(session)?;
        let told = session
            .var("MTX_REGIME")
            .ok_or("no MTX_REGIME: the informed bot is told the session's latent state")?;
        let own = wallet(told, "gateway-taker-side").ok_or("MTX_REGIME names no taker-side gateway")?;
        self.taker_side = self
            .feed
            .wallets
            .iter()
            .position(|w| *w == own)
            .ok_or("the taker-side gateway isn't one of MTX_FEED's")?;
        Ok(())
    }

    fn step(&mut self, market: &Market, orders: &mut Orders) -> BotResult {
        for i in self.feed.poll(market) {
            let call = &self.feed.calls[i];
            self.hold
                .take(direction(call, self.taker_side), call.slot + call.horizon);
        }
        trade_to(market, orders, self.hold.target(market.slot, self.size));
        Ok(())
    }
}

#[cfg(target_arch = "wasm32")]
mtx_kit::export!(Informed);
