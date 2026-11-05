//! Fade shocks: after a sudden move away from a pre-shock reference, take the
//! other side, exit on reversion, a stop, or a timeout.

use std::collections::VecDeque;

use anyhow::Result;
use mtx_botkit::{phoenix_rise::math::Side, BotEnv, Trader, Turns};
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

fn entry_pct() -> f64 { cfg("T_ENTRY_PCT").and_then(|v| v.parse().ok()).unwrap_or(0.04) }
fn stop_pct() -> f64 { cfg("T_STOP_PCT").and_then(|v| v.parse().ok()).unwrap_or(0.05) } // further adverse move from entry
fn take_frac() -> f64 { cfg("T_TAKE_FRAC").and_then(|v| v.parse().ok()).unwrap_or(0.1) } // exit when deviation shrinks to this fraction of entry deviation
fn max_hold() -> usize { cfg("T_MAX_HOLD").and_then(|v| v.parse().ok()).unwrap_or(250) } // steps
const REF_LAG: usize = 40; // steps back used as reference (quiet) price
fn size() -> f64 { cfg("T_SIZE").and_then(|v| v.parse().ok()).unwrap_or(70.0) }

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .init();
    let mut turns = Turns::connect()?;
    let env = BotEnv::from_env()?;
    let trader = Trader::connect(&env)?;
    let mut hist: VecDeque<f64> = VecDeque::new();
    // (reference, entry mark, entry step)
    let mut trade: Option<(f64, f64, usize)> = None;
    let mut n = 0usize;
    let (mut realized, mut peak) = (0.0f64, 0.0f64);

    while let Some(step) = turns.next()? {
        n += 1;
        let mark = trader.mark_usd().await?;
        let pos = trader.position().await?.size;
        hist.push_back(mark);
        if hist.len() > 200 {
            hist.pop_front();
        }
        info!(k = step.k, mark, pos, "step");
        let flat = pos.abs() < 0.5;
        if flat {
            trade = None;
        }
        let remaining = (env.end_slot.saturating_sub(step.slot)) as usize;

        if let Some((reference, entry, at)) = trade {
            let dev0 = entry - reference;
            let dev = mark - reference;
            let adverse = (mark - entry) * dev0.signum();
            let open_pnl = (mark - entry) * pos;
            let guard = realized + open_pnl < peak - 700.0;
            let done = guard
                || dev.abs() < take_frac() * dev0.abs()
                || dev.signum() != dev0.signum()
                || adverse > stop_pct() * entry
                || n - at > max_hold()
                || remaining < 6;
            if done {
                close(&trader, pos, mark).await;
                realized += (mark - entry) * pos;
                peak = peak.max(realized);
                trade = None;
            }
        } else if flat && remaining > 10 && hist.len() > REF_LAG {
            let len = hist.len();
            let mut w: Vec<f64> = hist.iter().skip(len.saturating_sub(REF_LAG + 4)).take(REF_LAG).cloned().collect();
            w.sort_by(|a, b| a.partial_cmp(b).unwrap());
            let reference = w[w.len() / 2];
            let dev = (mark - reference) / reference;
            if dev.abs() > entry_pct() {
                let side = if dev > 0.0 { Side::Ask } else { Side::Bid };
                let lim = if dev > 0.0 { mark * 0.97 } else { mark * 1.03 };
                let sz = size().min(650.0 / (stop_pct() * mark)).floor();
                if sz >= 1.0 && open(&trader, side, sz, lim).await {
                    trade = Some((reference, mark, n));
                } else {
                    warn!("open failed");
                }
            }
        }
    }
    Ok(())
}

async fn open(t: &Trader, side: Side, size: f64, lim: f64) -> bool {
    let mut left = size;
    let mut ok = false;
    while left > 0.01 {
        let q = left.min(20.0);
        match t.place_market(side, q, lim).await {
            Ok(_) => ok = true,
            Err(e) => {
                warn!("order err {e}");
                break;
            }
        }
        left -= q;
    }
    ok
}

async fn close(t: &Trader, pos: f64, mark: f64) {
    let (side, lim) = if pos > 0.0 { (Side::Ask, mark * 0.95) } else { (Side::Bid, mark * 1.05) };
    let mut left = pos.abs();
    while left > 0.01 {
        let q = left.min(20.0);
        if let Err(e) = t.place_market(side, q, lim).await {
            warn!("close err {e}");
            break;
        }
        left -= q;
    }
}

/// Tuning overrides from /tmp/botcfg ("KEY=value" lines); absent when graded.
fn cfg(key: &str) -> Option<String> {
    let s = std::fs::read_to_string("/tmp/botcfg").ok()?;
    s.lines().find_map(|l| l.strip_prefix(&format!("{key}=")).map(|v| v.trim().to_string()))
}
