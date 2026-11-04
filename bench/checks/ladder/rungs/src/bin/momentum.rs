//! Rung `momentum`: once the mark has moved 4% from the open, the full cap
//! with the move, held to the end. It wins shocks that extend and loses the
//! ones that revert: the reference's opposite bet.

use anyhow::Result;
use mtx_botkit::{BotEnv, Trader, Turns};
use mtx_ladder_rungs::{init_logs, toward};

const TRIGGER_PCT: f64 = 4.0;

#[tokio::main]
async fn main() -> Result<()> {
    init_logs();
    let mut turns = Turns::connect()?;
    let env = BotEnv::from_env()?;
    let trader = Trader::connect(&env)?;
    let start = trader.mark_usd().await?;
    let mut side: Option<f64> = None;
    while turns.next()?.is_some() {
        if side.is_none() {
            let move_pct = (trader.mark_usd().await? / start - 1.0) * 100.0;
            if move_pct.abs() >= TRIGGER_PCT {
                side = Some(move_pct.signum());
            }
        }
        if let Some(side) = side {
            toward(&trader, side * env.max_position).await?;
        }
    }
    Ok(())
}
