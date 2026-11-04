//! Rung `regime-oracle` (flash-crash): told the session's regime, not its
//! path. The grader hands it the draw's shape as `MTX_REGIME` (ladder runs
//! only, `mtx grade --tell-regime`): the shock's direction, its extreme, and
//! where the aftermath ends. It waits for the shock to reach its extreme,
//! then takes the full cap toward where the session ends, and holds.
//!
//! Its lead over the reference is what reading the regime well could still
//! earn; what it leaves to hindsight is the path's own noise and timing.

use anyhow::Result;
use mtx_botkit::{BotEnv, Trader, Turns};
use mtx_ladder_rungs::{init_logs, toward};
use serde::Deserialize;
use tracing::info;

/// How close to the extreme, in percent of the open, counts as there.
const NEAR_PCT: f64 = 0.5;
/// Smallest move from the extreme to the end worth the round trip.
const WORTH_PCT: f64 = 1.0;

#[derive(Deserialize)]
struct Shape {
    extreme_pct: f64,
    end_pct: f64,
}

#[tokio::main]
async fn main() -> Result<()> {
    init_logs();
    let mut turns = Turns::connect()?;
    let env = BotEnv::from_env()?;
    let shape: Shape = serde_json::from_str(&std::env::var("MTX_REGIME")?)?;
    let trader = Trader::connect(&env)?;
    let start = trader.mark_usd().await?;
    let gain = shape.end_pct - shape.extreme_pct;
    let mut side: Option<f64> = None;
    info!(shape.extreme_pct, shape.end_pct, "told the regime");
    while turns.next()?.is_some() {
        if side.is_none() && gain.abs() >= WORTH_PCT {
            let level = (trader.mark_usd().await? / start - 1.0) * 100.0;
            // At the extreme: as far from the open as it gets, its way.
            if (level - shape.extreme_pct).abs() <= NEAR_PCT
                || level.signum() == shape.extreme_pct.signum()
                    && level.abs() >= shape.extreme_pct.abs()
            {
                side = Some(gain.signum());
                info!(level, "at the extreme");
            }
        }
        if let Some(side) = side {
            toward(&trader, side * env.max_position).await?;
        }
    }
    Ok(())
}
