//! Reference: fade the shock in tranches, hold for the reversion.
//!
//! The family's shocks are crashes or squeezes in one to three legs of
//! 0.6-8%, and usually revert. The reference ignores anything under 4%:
//! most of those are wobble, fizzles or shocks too small to pay for the
//! round trip. The first 4% move tells it which way the shock goes. It then
//! takes
//! the other side of the forced flow: long into a crash, short into a
//! squeeze, in tranches as the shock deepens, each a share of the position
//! cap. It holds to the end of the session to keep the whole reversion. A
//! shock that keeps going past 13.5% is more likely extending than
//! reverting: the reference cuts the position there and stops trading.
//!
//! Its PnL is a published baseline, not the reward's scale (that's the
//! hindsight bound). It reads the regime reasonably but not well: it doesn't
//! tell a stall from a reversion, sits out small shocks, and its stop only
//! catches deep extensions. Re-tuned against the 0.8.0 family offline, its
//! thresholds were already within a few dollars a session of the best.

use anyhow::Result;
use mth_botkit::{BotEnv, Side, Trader, Turns};
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

/// Size of the shock, in percent of the starting mark, at which each tranche
/// is taken, and its share of the position cap. The first tranche's size also
/// decides which way the shock goes.
const TRANCHES: [(f64, f64); 2] = [(4.0, 0.5), (8.0, 0.5)];
/// Size of the shock at which the position is cut: deeper than most of the
/// family's shocks go before they turn.
const EXTEND_STOP_PCT: Option<f64> = Some(13.5);
/// Largest market order, in base units. One order that crosses much of the
/// book matches too many resting orders to fit a transaction's compute
/// budget, so tranches are taken in slices.
const SLICE: f64 = 10.0;

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
    info!(
        start_mark,
        ?TRANCHES,
        ?EXTEND_STOP_PCT,
        "watching for the shock"
    );

    // +1 fades a crash (long), -1 fades a squeeze (short); unset until the
    // first tranche's move.
    let mut fade: Option<f64> = None;
    // One look and at most one order a step: an order lands before the next
    // step, so the position read then already reflects it.
    while turns.next()?.is_some() {
        let Ok(mark) = trader.mark_usd().await else {
            continue;
        };
        let move_pct = (mark - start_mark) / start_mark * 100.0;
        if fade.is_none() && move_pct.abs() >= TRANCHES[0].0 {
            let side = -move_pct.signum();
            info!(mark, move_pct, side, "shock under way; fading it");
            fade = Some(side);
        }
        let Some(side) = fade else {
            continue;
        };
        // How far the shock has gone, and how much of the fade is on.
        let shock_pct = -side * move_pct;
        let held = side * trader.position().await?.size;

        if let Some(stop) = EXTEND_STOP_PCT
            && shock_pct >= stop
        {
            if held <= 0.0 {
                info!("stopped; holding nothing to the end");
                return Ok(());
            }
            let size = held.min(SLICE);
            warn!(mark, shock_pct, size, "extending past the stop; closing");
            let (close, limit) = if side > 0.0 {
                (Side::Ask, mark * 0.98)
            } else {
                (Side::Bid, mark * 1.02)
            };
            if let Err(e) = trader.place_market(close, size, round_cents(limit)).await {
                warn!(%e, "close failed; retrying");
            }
            continue;
        }

        let target: f64 = TRANCHES
            .iter()
            .filter(|(at_pct, _)| shock_pct >= *at_pct)
            .map(|(_, share)| share * env.max_position)
            .sum();
        let missing = (target - held).floor().min(SLICE);
        if missing >= 1.0 {
            info!(mark, shock_pct, size = missing, "taking a slice");
            let (open, limit) = if side > 0.0 {
                (Side::Bid, mark * 1.01)
            } else {
                (Side::Ask, mark * 0.99)
            };
            if let Err(e) = trader.place_market(open, missing, round_cents(limit)).await {
                warn!(%e, "order failed; retrying");
            }
        }
    }
    let position = trader.position().await?;
    info!(
        size = position.size,
        entry = position.entry_usd,
        "done; holding to the mark"
    );
    Ok(())
}

fn round_cents(usd: f64) -> f64 {
    (usd * 100.0).round() / 100.0
}
