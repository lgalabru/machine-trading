//! What the ladder's rungs share: taking a position in slices, and quoting
//! one tick inside the competing makers.

use anyhow::Result;
use mtx_botkit::{Side, Trader};
use tracing::warn;

/// Largest market order, in base units: one order that crosses much of the
/// book doesn't fit a transaction's compute budget.
pub const SLICE: f64 = 10.0;

pub fn init_logs() {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();
}

/// Move the position one slice toward `target` (base units, signed), at a
/// limit 2% through the mark.
pub async fn toward(trader: &Trader, target: f64) -> Result<()> {
    let held = trader.position().await?.size;
    let missing = target - held;
    if missing.abs() < 1.0 {
        return Ok(());
    }
    let mark = trader.mark_usd().await?;
    let size = missing.abs().floor().min(SLICE);
    let (side, limit) = if missing > 0.0 {
        (Side::Bid, mark * 1.02)
    } else {
        (Side::Ask, mark * 0.98)
    };
    if let Err(e) = trader
        .place_market(side, size, (limit * 100.0).round() / 100.0)
        .await
    {
        warn!(%e, "order failed; retrying next step");
    }
    Ok(())
}

/// The competing makers' half-spread less one tick, in basis points of the
/// oracle: the closest any enabled spline quotes to its mid.
pub async fn inside_competitor_bps(trader: &Trader) -> Result<f64> {
    use mtx_botkit::phoenix_rise::accounts::spline_collection::SplineCollection;
    let market = trader.phoenix.market(&trader.env.symbol)?;
    let address: mtx_botkit::Pubkey = market.spline_pubkey.parse()?;
    let account = trader.rpc.get_account(&address)?;
    let collection = SplineCollection::try_from_account_bytes(&account.data)
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let inner = collection
        .splines()
        .filter_map(|s| s.ok())
        .filter(|s| s.is_enabled())
        .flat_map(|s| {
            s.active_bid_regions()
                .iter()
                .chain(s.active_ask_regions())
                .map(|r| r.start_offset().as_inner())
                .collect::<Vec<_>>()
        })
        .min()
        .ok_or_else(|| anyhow::anyhow!("no competing quotes"))?;
    let tick_usd = trader.phoenix.ticks_to_usd(market, 1);
    let oracle = trader.oracle_usd().await?;
    Ok(((inner.saturating_sub(1)) as f64 * tick_usd / oracle * 10_000.0).max(1.0))
}
