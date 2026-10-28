//! A bot that connects, watches the market, and never trades.
//!
//! Start from here: the plumbing works, the strategy is yours. See
//! `ott_botkit::Trader` for placing and cancelling orders and reading your
//! position.

use std::time::Duration;

use anyhow::Result;
use ott_botkit::{BotEnv, Trader};
use tracing::info;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .init();
    let env = BotEnv::from_env()?;
    let trader = Trader::connect(&env)?;
    info!(authority = %trader.authority(), end_slot = env.end_slot, "connected");

    while trader.slot()? < env.end_slot {
        let mark = trader.mark_usd().await?;
        let position = trader.position().await?;
        info!(mark, position = position.size, "tick");
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    Ok(())
}
