//! Rung `flow-oracle` (market-making): a perfect reader of the tape. The
//! grader hands it, as `MTX_REGIME` (ladder runs only, `mtx grade
//! --tell-regime`), the session's informed taker orders by slot and side; it
//! looks only at those that have already traded, so it learns of a burst the
//! moment its first order hits the book, as a trader who could tell informed
//! flow from noise and fake-outs perfectly would. It isn't told the jump, its
//! size or its slot.
//!
//! Between bursts it quotes as the skewed quoter does. Once an informed order
//! has traded, a jump its way is certain: it pulls its quotes, takes the cap
//! the burst's way, holds until the oracle has jumped, and flattens.
//!
//! Its lead over the reference is what reading the flow perfectly is worth.
//! An exploit bot above it reads something no honest trader could.

use anyhow::Result;
use mtx_botkit::{BotEnv, Side, Trader, Turns};
use mtx_ladder_rungs::{SLICE, init_logs, inside_competitor_bps};
use serde::Deserialize;
use tracing::{info, warn};

const SIZE: f64 = 1.0;
const SKEW_BPS_PER_UNIT: f64 = 2.5;
const REQUOTE_BPS: f64 = 1.5;
const INVENTORY_LIMIT: f64 = 0.25;
/// A move of this much from where it entered, the burst's way, is the jump.
const JUMPED_PCT: f64 = 1.0;
/// Slots past the burst's last informed order after which it gives up on the
/// jump: informed orders lead it by at most 24.
const GIVE_UP_SLOTS: u64 = 32;
/// Most it trades in one turn, in base units.
const MAX_STEP_TRADE: f64 = 40.0;

#[derive(Deserialize)]
struct Informed {
    slot: u64,
    side: String,
}

struct Lean {
    dir: f64,
    entry: f64,
    last_order: u64,
}

#[tokio::main]
async fn main() -> Result<()> {
    init_logs();
    let mut turns = Turns::connect()?;
    let env = BotEnv::from_env()?;
    let informed: Vec<Informed> = serde_json::from_str(&std::env::var("MTX_REGIME")?)?;
    let start_slot = turns.hello().start_slot;
    let slots_per_step = turns.hello().slots_per_step;
    let trader = Trader::connect(&env)?;
    let cap = env.max_position;
    let half = inside_competitor_bps(&trader).await?;
    info!(informed = informed.len(), "told the informed orders");

    // Informed orders it has seen trade, by index.
    let mut seen = 0usize;
    let mut lean: Option<Lean> = None;
    let mut quoted: Option<(f64, f64)> = None;
    while let Some(step) = turns.next()? {
        let (Ok(oracle), Ok(position)) = (trader.oracle_usd().await, trader.position().await)
        else {
            continue;
        };
        let held = position.size;
        // Orders due by the last step's end traded before this turn.
        let traded_by = (step.slot - start_slot).saturating_sub(slots_per_step);
        while let Some(order) = informed.get(seen).filter(|o| o.slot <= traded_by) {
            seen += 1;
            let dir = if order.side == "buy" { 1.0 } else { -1.0 };
            match &mut lean {
                Some(l) if l.dir == dir => l.last_order = order.slot,
                _ => {
                    info!(slot = order.slot, dir, "an informed burst");
                    lean = Some(Lean {
                        dir,
                        entry: oracle,
                        last_order: order.slot,
                    });
                }
            }
        }
        if let Some(l) = &lean {
            let moved = l.dir * (oracle / l.entry - 1.0) * 100.0;
            if moved >= JUMPED_PCT || traded_by > l.last_order + GIVE_UP_SLOTS {
                info!(moved, "the jump landed; flattening");
                lean = None;
            }
        }

        let target = match &lean {
            Some(l) => Some(l.dir * cap),
            // Flatten what a lean left before quoting again.
            None if held.abs() > INVENTORY_LIMIT * cap => Some(0.0),
            None => None,
        };
        if let Some(target) = target {
            if quoted.take().is_some()
                && let Err(e) = trader.cancel_all()
            {
                warn!(%e, "cancel failed");
            }
            trade_toward(&trader, held, target, oracle).await;
            continue;
        }

        let mid = oracle * (1.0 - SKEW_BPS_PER_UNIT * held / 10_000.0);
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

/// Market orders of up to [`SLICE`] from `held` toward `target`, at most
/// [`MAX_STEP_TRADE`] this turn, limited 2% through the oracle.
async fn trade_toward(trader: &Trader, held: f64, target: f64, oracle: f64) {
    let mut missing = (target - held).clamp(-MAX_STEP_TRADE, MAX_STEP_TRADE);
    while missing.abs() >= 0.01 {
        let size = (missing.abs().min(SLICE) * 1000.0).floor() / 1000.0;
        let (side, limit) = if missing > 0.0 {
            (Side::Bid, oracle * 1.02)
        } else {
            (Side::Ask, oracle * 0.98)
        };
        if let Err(e) = trader
            .place_market(side, size, (limit * 100.0).round() / 100.0)
            .await
        {
            warn!(%e, "order failed; retrying next turn");
            return;
        }
        missing -= size.copysign(missing);
    }
}
