//! Naive baseline: buy the dip and hold.
//!
//! Market-buys the full position cap once the market trades 3% below its starting
//! mark, then holds to the end of the session, with no take-profit and no stop. It profits whenever
//! the market rebounds and loses whenever it doesn't: a task where this
//! scores like the reference doesn't reward judgement.

use anyhow::Result;
use mth_botkit::{BotEnv, Side, Trader, Turns};
use tracing::info;
use tracing_subscriber::EnvFilter;

const TRIGGER_PCT: f64 = 3.0;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .init();
    // Answer the grader's hello first: the world waits for it.
    let mut turns = Turns::connect()?;
    let env = BotEnv::from_env()?;
    let trader = Trader::connect(&env)?;
    let start_mark = trader.mark_usd().await?;
    let trigger = start_mark * (1.0 - TRIGGER_PCT / 100.0);

    let mut bought = false;
    while turns.next()?.is_some() {
        if bought {
            continue;
        }
        let mark = trader.mark_usd().await?;
        if mark <= trigger {
            info!(mark, start_mark, "dip; buying");
            trader
                .place_market(
                    Side::Bid,
                    env.max_position,
                    (mark * 1.02 * 100.0).round() / 100.0,
                )
                .await?;
            bought = true;
        }
    }
    Ok(())
}
