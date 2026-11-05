//! SPCX shock trader. Each step: read Phoenix's index and mark, the account's
//! value and position, ask the strategy (`strategy.rs`) for a target
//! position, and trade toward it with market orders. The strategy is pure;
//! everything chain-related lives here.

mod strategy;

use anyhow::Result;
use std::str::FromStr;

use mtx_botkit::phoenix_rise::accounts::owned::Orderbook;
use mtx_botkit::{BotEnv, Pubkey, Side, Trader, Turns};
use strategy::{Obs, Params, Strategy};
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

/// Largest single market order, in shares: keeps each transaction well
/// inside its compute budget.
const MAX_ORDER: f64 = 140.0;
/// Worst price a market order accepts, as a fraction past the index.
const LIMIT_SLIP: f64 = 0.015;
/// Take resting orders at least this far through the index.
const STALE_EDGE: f64 = 0.003;

struct Account {
    /// Collateral plus unrealized PnL, in USD.
    value: f64,
    pos: f64,
}

async fn account(trader: &Trader) -> Result<Account> {
    let hawkeye = trader.phoenix.hawkeye(trader.authority());
    let margin = hawkeye.view_margin(trader.authority(), 0, 0).await?.value;
    let pos = trader.position().await?.size;
    Ok(Account {
        value: (margin.collateral_quote_lots + margin.unrealized_pnl_quote_lots) as f64 / 1e6,
        pos,
    })
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .with_ansi(false)
        .init();
    // Answer the grader's hello first: the world waits for it.
    let mut turns = Turns::connect()?;
    let env = BotEnv::from_env()?;
    let trader = Trader::connect(&env)?;
    let hello = turns.hello().clone();
    let slots_per_step = hello.slots_per_step.max(1);
    let max_pos = env.max_position.min(70.0);
    info!(authority = %trader.authority(), end_slot = env.end_slot, max_pos, "connected");

    let market_account = Pubkey::from_str(&trader.phoenix.market(&env.symbol)?.market_pubkey)?;
    let mut strat: Option<Strategy> = None;
    let mut start_value: Option<f64> = None;
    let mut halted = false;

    while let Some(step) = turns.next()? {
        let bbo = match trader.luce.bbo().await {
            Ok(b) => b,
            Err(e) => {
                warn!(step = step.k, "bbo failed: {e}");
                continue;
            }
        };
        let index = bbo.index_price_ticks as f64 * 0.01;
        let mark = bbo.mark_price_ticks as f64 * 0.01;
        let acct = match account(&trader).await {
            Ok(a) => a,
            Err(e) => {
                warn!(step = step.k, "account read failed: {e}");
                continue;
            }
        };
        let start = *start_value.get_or_insert(acct.value);
        let s = strat.get_or_insert_with(|| {
            let mut p = Params::default();
            p.max_pos = max_pos;
            p.max_dd = env.max_drawdown_usd;
            // The grader values at the mark it reads at the start.
            Strategy::new(p, mark)
        });
        let steps_left = env.end_slot.saturating_sub(step.slot) / slots_per_step;
        let pnl = acct.value - start;
        let mut target = s.step(Obs {
            step: step.k,
            price: index,
            steps_left,
            pos: acct.pos,
            pnl,
        });
        if halted {
            target = 0.0;
        }
        // Hard backstop, independent of the strategy's own sizing.
        if s.drawdown() > env.max_drawdown_usd * 0.75 {
            halted = true;
            target = 0.0;
        }
        let target = target.clamp(-max_pos, max_pos);
        info!(
            k = step.k,
            index,
            mark,
            pos = acct.pos,
            pnl = format!("{pnl:.2}"),
            target = format!("{target:.2}"),
            why = s.last_reason,
            "step"
        );
        if step.last {
            continue;
        }
        // Work in whole lots (0.001 SPCX), rounding the target toward zero
        // so no fill can take the position past the cap.
        let target_lots = (target * 1000.0).trunc() as i64;
        let pos_lots = (acct.pos * 1000.0).round() as i64;
        let mut delta = target_lots - pos_lots;
        while delta != 0 {
            let q = delta.abs().min((MAX_ORDER * 1000.0) as i64);
            let (side, limit) = if delta > 0 {
                (Side::Bid, index * (1.0 + LIMIT_SLIP))
            } else {
                (Side::Ask, index * (1.0 - LIMIT_SLIP))
            };
            if let Err(e) = trader.place_market(side, q as f64 / 1000.0, limit).await {
                warn!(step = step.k, "order failed: {e}");
                break;
            }
            delta -= q * delta.signum();
        }
        // Stale resting orders the price has moved through are worth taking:
        // they pay more than the market makers do. Take them up to the cap;
        // the strategy trades back to its target next step.
        let room_sell = target_lots + (max_pos * 1000.0) as i64;
        let room_buy = (max_pos * 1000.0) as i64 - target_lots;
        let best_bid = bbo.best_bid_ticks().map(|t| t as f64 * 0.01);
        let best_ask = bbo.best_ask_ticks().map(|t| t as f64 * 0.01);
        if best_bid.is_some_and(|b| b > index * (1.0 + STALE_EDGE)) && room_sell > 0 {
            sweep(&trader, &market_account, Side::Ask, index * (1.0 + STALE_EDGE), room_sell, step.k).await;
        } else if best_ask.is_some_and(|a| a < index * (1.0 - STALE_EDGE)) && room_buy > 0 {
            sweep(&trader, &market_account, Side::Bid, index * (1.0 - STALE_EDGE), room_buy, step.k).await;
        }
    }
    Ok(())
}

/// Take resting orders priced past `limit` (bids above it when selling,
/// asks below it when buying), at most `room` lots.
async fn sweep(trader: &Trader, market: &Pubkey, side: Side, limit: f64, room: i64, k: u64) {
    let book = match trader.rpc.get_account(market).map(|a| Orderbook::try_from_account_bytes(&a.data)) {
        Ok(Ok(book)) => book,
        _ => return,
    };
    let limit_ticks = (limit * 100.0) as u64;
    let lots: u64 = match side {
        Side::Ask => book.bids.iter().filter(|e| e.order_id.price_in_ticks.as_inner() > limit_ticks)
            .map(|e| e.order.num_base_lots_remaining).sum(),
        Side::Bid => book.asks.iter().filter(|e| e.order_id.price_in_ticks.as_inner() < limit_ticks)
            .map(|e| e.order.num_base_lots_remaining).sum(),
    };
    let q = (lots as i64).min(room).min((MAX_ORDER * 1000.0) as i64);
    if q <= 0 {
        return;
    }
    match trader.place_market(side, q as f64 / 1000.0, limit).await {
        Ok(_) => info!(k, ?side, lots = q, limit, "took stale orders"),
        Err(e) => warn!(k, "stale sweep failed: {e}"),
    }
}
