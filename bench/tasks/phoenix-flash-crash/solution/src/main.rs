//! Reference flash-crash liquidity provider. Its PnL on each scenario defines
//! a score of 1.
//!
//! Liquidation cascades overshoot. Rest a ladder of bids below the market to
//! buy the forced selling at a discount, then offer the position back with a
//! take-profit ask above the average entry. A stop-loss caps a dip that keeps
//! going. Shortly before the session ends, pull resting orders and hold
//! whatever is left; it is marked to market. The ladder adds up to the
//! position cap, so the reference trades at the size the task allows.

use std::time::Duration;

use anyhow::Result;
use ott_botkit::{BotEnv, Position, Side, Trader};
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

/// Bid levels, as percent below the starting mark, and each one's share of
/// the position cap.
const LADDER: [(f64, f64); 4] = [(3.0, 0.25), (5.0, 0.25), (7.0, 0.25), (9.0, 0.25)];
const TAKE_PROFIT_PCT: f64 = 3.0;
const STOP_LOSS_PCT: f64 = 15.0;
/// Stop quoting this many slots before the session ends.
const WIND_DOWN_SLOTS: u64 = 40;
const POLL: Duration = Duration::from_millis(500);

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .init();
    let env = BotEnv::from_env()?;
    let trader = Trader::connect(&env)?;
    let start_mark = trader.mark_usd().await?;
    let levels: Vec<(f64, f64)> = LADDER
        .iter()
        .map(|&(offset_pct, share)| {
            (
                round_cents(start_mark * (1.0 - offset_pct / 100.0)),
                share * env.max_position,
            )
        })
        .collect();
    info!(start_mark, ?levels, "quoting ladder");
    for &(price, size) in &levels {
        trader.place_limit(Side::Bid, price, size).await?;
    }

    let mut quoted = Position {
        size: 0.0,
        entry_usd: 0.0,
    };
    let mut low = start_mark;
    while trader.slot()? + WIND_DOWN_SLOTS < env.end_slot {
        tokio::time::sleep(POLL).await;
        let mark = trader.mark_usd().await?;
        low = low.min(mark);
        let position = trader.position().await?;
        if position.size <= 0.0 {
            continue;
        }
        if mark <= position.entry_usd * (1.0 - STOP_LOSS_PCT / 100.0) {
            warn!(mark, entry = position.entry_usd, "stop hit; closing");
            trader.cancel_all()?;
            trader
                .place_market(Side::Ask, position.size, round_cents(mark * 0.98))
                .await?;
            return wind_down(&trader).await;
        }
        if position != quoted {
            // A new fill moved the entry: re-quote the take-profit for the
            // whole position, and restore the bids the market hasn't reached.
            let take_profit = round_cents(position.entry_usd * (1.0 + TAKE_PROFIT_PCT / 100.0));
            info!(
                size = position.size,
                entry = position.entry_usd,
                take_profit,
                "re-quoting"
            );
            trader.cancel_all()?;
            trader
                .place_limit(Side::Ask, take_profit, position.size)
                .await?;
            for &(price, size) in levels.iter().filter(|(price, _)| *price < low) {
                trader.place_limit(Side::Bid, price, size).await?;
            }
            quoted = position;
        }
    }
    wind_down(&trader).await
}

async fn wind_down(trader: &Trader) -> Result<()> {
    trader.cancel_all()?;
    let position = trader.position().await?;
    info!(
        size = position.size,
        entry = position.entry_usd,
        "done; holding the rest to the mark"
    );
    Ok(())
}

fn round_cents(usd: f64) -> f64 {
    (usd * 100.0).round() / 100.0
}
