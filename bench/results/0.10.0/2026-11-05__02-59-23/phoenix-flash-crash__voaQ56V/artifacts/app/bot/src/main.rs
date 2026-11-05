//! A shock-fading bot for Phoenix's SPCX perpetual.
//!
//! Each turn it reads the oracle price (the level the scenario drives), its
//! position and equity, runs the policy in `policy.rs`, and moves to the
//! target position with market orders. Sizing keeps the account inside the
//! session's drawdown limit even through the largest single-step move the
//! scenario family produces.
//!
//! `bot replay <trace.json>` runs the policy over recorded level series to
//! check the port against the Python simulator it was tuned in.

mod policy;

use anyhow::{Context, Result};
use mtx_botkit::{BotEnv, Side, Trader, Turns};
use policy::{Params, State};
use std::time::Instant;
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

/// Hard cap the grader enforces; kept a hair below for lot rounding.
const POSITION_CAP: f64 = 69.9;
/// Largest market order sent in one transaction.
const CHUNK: f64 = 35.0;
/// Smallest position change worth a transaction.
const MIN_TRADE: f64 = 0.5;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env().add_directive("bot=info".parse()?))
        .init();
    let args: Vec<String> = std::env::args().collect();
    if args.len() >= 3 && args[1] == "replay" {
        return replay(&args[2]);
    }
    // Answer the grader's hello first: the world waits for it.
    let mut turns = Turns::connect()?;
    let env = BotEnv::from_env()?;
    let trader = Trader::connect(&env)?;
    let max_dd = env.max_drawdown_usd;
    let max_pos = env.max_position.min(POSITION_CAP);
    info!(authority = %trader.authority(), end_slot = env.end_slot, hello = ?turns.hello(), max_pos, max_dd, "connected");

    let params = Params::default();
    let mut state = State::new();
    let mut anchor: Option<f64> = None;
    let mut peak: Option<f64> = None;

    while let Some(step) = turns.next()? {
        let t0 = Instant::now();
        // Market data: the oracle is what the scenario drives; the mark lags it.
        let oracle = match trader.oracle_usd().await {
            Ok(v) if v.is_finite() && v > 1.0 => v,
            Ok(v) => {
                warn!(step = step.k, oracle = v, "implausible oracle; skipping step");
                continue;
            }
            Err(e) => {
                warn!(step = step.k, error = %e, "oracle read failed; skipping step");
                continue;
            }
        };
        let anchor_px = match anchor {
            Some(a) => a,
            None => {
                // The scenario's levels are relative to the mark at the start.
                let a = trader.mark_usd().await.unwrap_or(oracle);
                anchor = Some(a);
                a
            }
        };
        let level = (oracle / anchor_px - 1.0) * 100.0;

        // Account: position, and equity as the grader values it (collateral + unrealized PnL at the mark).
        let (pos, equity) = match read_account(&trader).await {
            Ok(v) => v,
            Err(e) => {
                warn!(step = step.k, error = %e, "account read failed; skipping step");
                continue;
            }
        };
        let pk = peak.map_or(equity, |p: f64| p.max(equity));
        peak = Some(pk);
        let dd = pk - equity;
        let budget = max_dd - dd;

        let mut target = policy::step(&mut state, &params, step.k, level, pos, budget, oracle);
        target = target.clamp(-max_pos, max_pos);
        let delta = target - pos;
        info!(
            step = step.k, slot = step.slot, level = format!("{level:+.3}"), oracle, pos, equity = format!("{equity:.2}"),
            dd = format!("{dd:.1}"), target, phase = ?state.phase, active = state.active, legs = state.legs, ext = format!("{:+.3}", state.ext),
            "step"
        );
        if step.last {
            // Valued right after this step: a trade now only costs fees. Still
            // answer the step (the next `turns.next()` does) so the grader
            // doesn't wait for us.
            continue;
        }
        if delta.abs() >= MIN_TRADE {
            execute(&trader, pos, target, oracle, max_pos).await;
        }
        let ms = t0.elapsed().as_millis();
        if ms > 1000 {
            warn!(step = step.k, ms, "slow turn");
        }
    }
    info!("session over");
    Ok(())
}

/// Position in shares and equity in dollars (collateral plus unrealized PnL).
async fn read_account(trader: &Trader) -> Result<(f64, f64)> {
    let market = trader.phoenix.market(&trader.env.symbol)?;
    let asset = trader.luce.margin().await.context("margin for asset")?;
    let pos = trader.phoenix.lots_to_base_units(market, asset.base_lots);
    let margin = trader
        .phoenix
        .hawkeye(trader.authority())
        .view_margin(trader.authority(), 0, 0)
        .await
        .map_err(|e| anyhow::anyhow!("view_margin: {e}"))?
        .value;
    let equity = (margin.collateral_quote_lots as f64 + margin.unrealized_pnl_quote_lots as f64)
        / mtx_botkit::QUOTE_LOTS_PER_USD;
    Ok((pos, equity))
}

/// Move from `pos` to `target` with market orders, in chunks, never past the cap.
async fn execute(trader: &Trader, pos: f64, target: f64, oracle: f64, max_pos: f64) {
    let mut cur = pos;
    let mut remaining = target - cur;
    let mut sent = 0;
    while remaining.abs() >= MIN_TRADE && sent < 6 {
        let qty = remaining.abs().min(CHUNK);
        let side = if remaining > 0.0 { Side::Bid } else { Side::Ask };
        let next = if remaining > 0.0 { cur + qty } else { cur - qty };
        if next.abs() > max_pos + 1e-6 {
            warn!(cur, qty, next, "chunk would breach the position cap; stopping");
            break;
        }
        let limit = if remaining > 0.0 { oracle * 1.01 } else { oracle * 0.99 };
        let t = Instant::now();
        match trader.place_market(side, qty, limit).await {
            Ok(sig) => {
                info!(?side, qty, cur, target, ms = t.elapsed().as_millis(), %sig, "market order");
            }
            Err(e) => {
                warn!(?side, qty, error = %e, "market order failed");
                break;
            }
        }
        sent += 1;
        // Re-read the position: a partial fill must not be compounded.
        match trader.position().await {
            Ok(p) => cur = p.size,
            Err(e) => {
                warn!(error = %e, "position read failed after order");
                break;
            }
        }
        remaining = target - cur;
    }
}

/// Replay recorded level series through the policy and compare with the simulator's targets.
fn replay(path: &str) -> Result<()> {
    #[derive(serde::Deserialize)]
    struct Trace {
        name: String,
        levels: Vec<f64>,
        targets: Vec<f64>,
        pnl: f64,
    }
    let traces: Vec<Trace> = serde_json::from_str(&std::fs::read_to_string(path)?)?;
    let params = Params::default();
    let start_mark = 143.11;
    let mut mismatches = 0;
    for tr in &traces {
        let mut st = State::new();
        let (mut cash, mut pos, mut peak) = (10000.0_f64, 0.0_f64, 10000.0_f64);
        let mut bad = 0;
        for (k, &lv) in tr.levels.iter().enumerate() {
            let mark = start_mark * (1.0 + lv / 100.0);
            let eq = cash + pos * mark;
            peak = peak.max(eq);
            if k == tr.levels.len() - 1 {
                break;
            }
            let budget = 1000.0 - (peak - eq);
            let t = policy::step(&mut st, &params, k as u64, lv, pos, budget, mark).clamp(-70.0, 70.0);
            if (t - tr.targets[k]).abs() > 1e-3 {
                bad += 1;
                if bad <= 3 {
                    println!("{} step {k}: rust {t} python {}", tr.name, tr.targets[k]);
                }
            }
            let q = t - pos;
            if q.abs() > 1e-9 {
                let slip = (0.5 + 2.5 * q.abs() / 70.0) / 1e4;
                let px = mark * (1.0 + if q > 0.0 { slip } else { -slip });
                cash -= q * px;
                cash -= q.abs() * px * 0.00035;
                pos += q;
            }
        }
        let last = start_mark * (1.0 + tr.levels.last().unwrap() / 100.0);
        let pnl = cash + pos * last - 10000.0;
        if bad > 0 || (pnl - tr.pnl).abs() > 0.05 {
            mismatches += 1;
            println!("{}: {bad} target mismatches, pnl rust {pnl:.2} python {:.2}", tr.name, tr.pnl);
        }
    }
    println!("{} traces, {} with mismatches", traces.len(), mismatches);
    Ok(())
}
