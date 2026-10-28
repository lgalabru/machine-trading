//! Naive baseline: buy the dip and hold.
//!
//! Market-buys the full position cap once SOL trades 3% below its starting
//! mark, then holds to the end of the session, with no take-profit and no stop. It profits whenever
//! the market rebounds and loses whenever it doesn't: a task where this
//! scores like the reference doesn't reward judgement.

use std::time::Duration;

use anyhow::Result;
use ott_botkit::{BotEnv, Side, Trader};
use tracing::info;
use tracing_subscriber::EnvFilter;

const TRIGGER_PCT: f64 = 3.0;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .init();
    let env = BotEnv::from_env()?;
    let trader = Trader::connect(&env)?;
    let start_mark = trader.mark_usd().await?;
    let trigger = start_mark * (1.0 - TRIGGER_PCT / 100.0);

    while trader.slot()? < env.end_slot {
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
            break;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    Ok(())
}
