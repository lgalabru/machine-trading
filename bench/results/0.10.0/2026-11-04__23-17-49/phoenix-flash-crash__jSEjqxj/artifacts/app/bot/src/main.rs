//! Trades SPCX through shocks: rides moves while they build and fades
//! fizzles and big shocks, sized so a worst-case step can't breach the
//! drawdown limit. The logic lives in `strategy`; this file is the plumbing
//! between it and the chain.

mod strategy;

use anyhow::Result;
use mtx_botkit::{BotEnv, QUOTE_LOTS_PER_USD, Side, Trader, Turns};
use strategy::{Params, Strategy};
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

/// Largest single market order: one that crosses much of the book can run
/// out of compute.
const CHUNK: f64 = 35.0;
/// Trades smaller than this aren't worth their fees.
const MIN_TRADE: f64 = 0.5;

struct Reading {
    index: f64,
    mark: f64,
    position: f64,
    value: f64,
}

async fn read(trader: &Trader, tick_usd: f64) -> Result<Reading> {
    let bbo = trader.luce.bbo().await?;
    let margin = trader
        .phoenix
        .hawkeye(trader.authority())
        .view_margin(trader.authority(), 0, 0)
        .await?
        .value;
    let position = trader.position().await?.size;
    Ok(Reading {
        index: bbo.index_price_ticks as f64 * tick_usd,
        mark: bbo.mark_price_ticks as f64 * tick_usd,
        position,
        value: (margin.collateral_quote_lots + margin.unrealized_pnl_quote_lots) as f64
            / QUOTE_LOTS_PER_USD,
    })
}

async fn trade(trader: &Trader, delta: f64, ref_px: f64) {
    let side = if delta > 0.0 { Side::Bid } else { Side::Ask };
    // Fill within 1% of the index, which moves at most 4% a step anyway.
    let limit = if delta > 0.0 { ref_px * 1.01 } else { ref_px * 0.99 };
    let mut left = delta.abs();
    while left >= MIN_TRADE * 0.5 {
        let q = left.min(CHUNK);
        if let Err(e) = trader.place_market(side, q, limit).await {
            warn!(error = %e, q, "order failed");
            return;
        }
        left -= q;
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();
    // Answer the grader's hello first: the world waits for it.
    let mut turns = Turns::connect()?;
    let env = BotEnv::from_env()?;
    let trader = Trader::connect(&env)?;
    let market = trader.phoenix.market(&env.symbol)?;
    let tick_usd = trader.phoenix.ticks_to_usd(market, 1);
    // Stay a hair inside the cap: lots round, and the cap is a hard line.
    let max_pos = (env.max_position - 0.1).max(0.0);
    let params = Params {
        max_pos,
        dd_limit: (env.max_drawdown_usd * 0.85).min(env.max_drawdown_usd - 100.0),
        ..Params::default()
    };
    info!(authority = %trader.authority(), end_slot = env.end_slot, ?params, "connected");
    let mut strat = Strategy::new(params);

    while let Some(step) = turns.next()? {
        let r = match read(&trader, tick_usd).await {
            Ok(r) => r,
            Err(e) => {
                warn!(error = %e, "read failed");
                continue;
            }
        };
        let target = strat.step(r.index, r.value).clamp(-max_pos, max_pos);
        let delta = target - r.position;
        info!(
            k = step.k,
            index = r.index,
            mark = r.mark,
            pos = r.position,
            value = r.value,
            target,
            "step"
        );
        if delta.abs() >= MIN_TRADE || (target == 0.0 && r.position != 0.0) {
            trade(&trader, delta, r.index).await;
        }
    }
    Ok(())
}
