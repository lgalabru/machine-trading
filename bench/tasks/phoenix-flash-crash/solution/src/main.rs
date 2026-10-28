//! Reference: scale into the crash, hold for the rebound.
//!
//! The family's crashes go 5-12% deep and usually rebound, so the reference
//! buys in tranches as the drop deepens, each a share of the position cap,
//! and holds to the end of the session to keep the whole rebound. A crash
//! that keeps falling past anything the family's crashes reach is a slide:
//! the reference cuts the position there and stops trading.
//!
//! Its total PnL over a scenario set defines a reward of 1. It reads the
//! disclosed regime well but not optimally: it doesn't tell a stall from a
//! rebound, and its stop only catches deep slides.

use std::time::{Duration, Instant};

use anyhow::Result;
use ott_botkit::{BotEnv, Side, Trader};
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

/// Drop below the starting mark, in percent, at which each tranche is bought,
/// and its share of the position cap.
const TRANCHES: [(f64, f64); 2] = [(4.0, 0.5), (8.0, 0.5)];
/// Drop below the starting mark, in percent, past the deepest crash the
/// family draws, at which the position is cut: the market is sliding.
const SLIDE_STOP_PCT: Option<f64> = Some(13.5);
/// Largest market order, in base units. One order that crosses much of the
/// book matches too many resting orders to fit a transaction's compute
/// budget, so tranches are bought in slices.
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
        ?SLIDE_STOP_PCT,
        "watching for the crash"
    );

    let mut last_order: Option<Instant> = None;
    while trader.slot()? < env.end_slot {
        tokio::time::sleep(POLL).await;
        let Ok(mark) = trader.mark_usd().await else {
            continue;
        };
        let drop_pct = (start_mark - mark) / start_mark * 100.0;
        if last_order.is_some_and(|at| at.elapsed() < SETTLE) {
            continue;
        }
        let position = trader.position().await?;

        if let Some(stop) = SLIDE_STOP_PCT
            && drop_pct >= stop
        {
            if position.size <= 0.0 {
                info!("stopped; holding nothing to the end");
                return Ok(());
            }
            let size = position.size.min(SLICE);
            warn!(mark, drop_pct, size, "sliding past the stop; closing");
            if let Err(e) = trader
                .place_market(Side::Ask, size, round_cents(mark * 0.98))
                .await
            {
                warn!(%e, "close failed; retrying");
            }
            last_order = Some(Instant::now());
            continue;
        }

        let target: f64 = TRANCHES
            .iter()
            .filter(|(at_pct, _)| drop_pct >= *at_pct)
            .map(|(_, share)| share * env.max_position)
            .sum();
        let missing = (target - position.size).floor().min(SLICE);
        if missing >= 1.0 {
            info!(mark, drop_pct, size = missing, "buying a slice");
            if let Err(e) = trader
                .place_market(Side::Bid, missing, round_cents(mark * 1.01))
                .await
            {
                warn!(%e, "buy failed; retrying");
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
