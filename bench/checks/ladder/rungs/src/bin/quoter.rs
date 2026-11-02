//! Rungs `naive-quoter` and `skew-quoter` (market-making): one tick inside
//! the competing makers around the oracle, one unit a side, requoted when
//! the quotes would move. `skew` also leans the quotes against the
//! inventory, as the reference does; neither reads the flow or backs off.
//! Both stop adding past a quarter of the cap, so neither breaches it.

use anyhow::Result;
use mth_botkit::{BotEnv, Side, Trader, Turns};
use mth_ladder_rungs::{init_logs, inside_competitor_bps};
use tracing::warn;

const SIZE: f64 = 1.0;
const SKEW_BPS_PER_UNIT: f64 = 2.5;
const REQUOTE_BPS: f64 = 1.5;
const INVENTORY_LIMIT: f64 = 0.25;

#[tokio::main]
async fn main() -> Result<()> {
    init_logs();
    let mut turns = Turns::connect()?;
    let env = BotEnv::from_env()?;
    let skew = std::env::args().nth(1).as_deref() == Some("skew");
    let trader = Trader::connect(&env)?;
    let cap = env.max_position;
    let half = inside_competitor_bps(&trader).await?;
    let mut quoted: Option<(f64, f64)> = None;
    while turns.next()?.is_some() {
        let (Ok(oracle), Ok(position)) = (trader.oracle_usd().await, trader.position().await)
        else {
            continue;
        };
        let held = position.size;
        let lean = if skew { SKEW_BPS_PER_UNIT * held } else { 0.0 };
        let mid = oracle * (1.0 - lean / 10_000.0);
        let (bid, ask) = (mid * (1.0 - half / 10_000.0), mid * (1.0 + half / 10_000.0));
        if quoted.is_some_and(|(b, a)| {
            ((bid - b) / oracle).abs() * 10_000.0 < REQUOTE_BPS
                && ((ask - a) / oracle).abs() * 10_000.0 < REQUOTE_BPS
        }) {
            continue;
        }
        if let Err(e) = trader.cancel_all() {
            warn!(%e, "cancel failed");
            continue;
        }
        if held < INVENTORY_LIMIT * cap
            && let Err(e) = trader.place_limit(Side::Bid, bid, SIZE.min(cap - held)).await
        {
            warn!(%e, "bid failed");
        }
        if held > -INVENTORY_LIMIT * cap
            && let Err(e) = trader.place_limit(Side::Ask, ask, SIZE.min(cap + held)).await
        {
            warn!(%e, "ask failed");
        }
        quoted = Some((bid, ask));
    }
    Ok(())
}
