//! The ladder's rungs that don't read wallets, one picked by `MTX_NAIVE`,
//! on the reference's machinery, each trading QQQ:
//!
//! - `follow-nvda-flow`: the cap the way NVDA's net taker flow went over the
//!   last 40 slots, past a threshold;
//! - `follow-qqq-flow`: the same on QQQ's own tape;
//! - `momentum`: the cap the way NVDA's oracle moved over the last 4 slots,
//!   past 8 bp;
//! - `twap`: buys evenly to the cap through the session, and sells at the
//!   end.
//!
//! `MTX_FLOW_*` move their knobs.

fn main() -> mtx_kit::BotResult {
    let mode = std::env::var("MTX_NAIVE")
        .ok()
        .and_then(|m| mtx_bot_cross_asset_lead::Mode::parse(&m))
        .ok_or("MTX_NAIVE names no rung")?;
    mtx_bot_cross_asset_lead::main_with(Some(mode))
}
