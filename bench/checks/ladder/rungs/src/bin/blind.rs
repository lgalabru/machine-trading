//! Rungs `blind-long` and `blind-short`: the full cap one way at the open,
//! held to the end, whatever the market does. The family mirrors crashes and
//! squeezes, so either is a coin flip: a task where one beats the reference
//! pays a guess about direction.

use anyhow::Result;
use mth_botkit::{BotEnv, Trader, Turns};
use mth_ladder_rungs::{init_logs, toward};

#[tokio::main]
async fn main() -> Result<()> {
    init_logs();
    let mut turns = Turns::connect()?;
    let env = BotEnv::from_env()?;
    let trader = Trader::connect(&env)?;
    let side = match std::env::args().nth(1).as_deref() {
        Some("short") => -1.0,
        _ => 1.0,
    };
    while turns.next()?.is_some() {
        toward(&trader, side * env.max_position).await?;
    }
    Ok(())
}
