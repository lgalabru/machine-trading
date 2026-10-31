//! A bot that connects, watches the market, and never trades.
//!
//! Start from here: the plumbing works, the strategy is yours. The world
//! moves in steps and waits for the bot between them: `Turns::next` hands
//! over each step, and anything the bot sends before asking for the next one
//! lands before the world moves again. See `mth_botkit::Trader` for placing
//! and cancelling orders and reading your position.

use anyhow::Result;
use mth_botkit::{BotEnv, Trader, Turns};
use tracing::info;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .init();
    // Answer the grader's hello first: the world waits for it.
    let mut turns = Turns::connect()?;
    let env = BotEnv::from_env()?;
    let trader = Trader::connect(&env)?;
    info!(authority = %trader.authority(), end_slot = env.end_slot, "connected");

    while let Some(step) = turns.next()? {
        let mark = trader.mark_usd().await?;
        let position = trader.position().await?;
        info!(
            step = step.k,
            slot = step.slot,
            mark,
            position = position.size,
            "step"
        );
    }
    Ok(())
}
