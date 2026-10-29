//! Reference: fade the shock in tranches, hold for the reversion.
//!
//! The family's shocks are crashes or squeezes of 5-12%, and usually revert.
//! The first 4% move tells the reference which one this is. It then takes
//! the other side of the forced flow: long into a crash, short into a
//! squeeze, in tranches as the shock deepens, each a share of the position
//! cap. It holds to the end of the session to keep the whole reversion. A
//! shock that keeps going past anything the family's shocks reach is
//! extending: the reference cuts the position there and stops trading.
//!
//! Its PnL is a published baseline, not the reward's scale (that's the
//! hindsight bound). It reads the regime reasonably but not well: it doesn't
//! tell a stall from a reversion, and its stop only catches deep extensions.

use std::time::{Duration, Instant};

use anyhow::Result;
use mt_botkit::{BotEnv, Side, Trader};
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

/// Size of the shock, in percent of the starting mark, at which each tranche
/// is taken, and its share of the position cap. The first tranche's size also
/// decides which way the shock goes.
const TRANCHES: [(f64, f64); 2] = [(4.0, 0.5), (8.0, 0.5)];
/// Size of the shock, past the family's largest, at which the position is
/// cut: the shock is extending.
const EXTEND_STOP_PCT: Option<f64> = Some(13.5);
/// Largest market order, in base units. One order that crosses much of the
/// book matches too many resting orders to fit a transaction's compute
/// budget, so tranches are taken in slices.
const SLICE: f64 = 10.0;
/// After an order, wait this long for the position to reflect it before
/// sizing the next one.
const SETTLE: Duration = Duration::from_millis(500);
const POLL: Duration = Duration::from_millis(250);

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .init();
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
    let mut last_order: Option<Instant> = None;
    while trader.slot()? < env.end_slot {
        tokio::time::sleep(POLL).await;
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
        if last_order.is_some_and(|at| at.elapsed() < SETTLE) {
            continue;
        }
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
            last_order = Some(Instant::now());
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
            last_order = Some(Instant::now());
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
