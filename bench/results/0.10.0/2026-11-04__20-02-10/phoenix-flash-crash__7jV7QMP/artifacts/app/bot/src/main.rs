//! Fade shocks once they stop moving; cut losses if the shock extends.

use anyhow::Result;
use mtx_botkit::phoenix_rise::math::Side;
use mtx_botkit::{BotEnv, Trader, Turns};
use tracing::info;
use tracing_subscriber::EnvFilter;

fn param(name: &str, default: f64) -> f64 {
    // Tuning overrides, read from a scratch file when present.
    std::fs::read_to_string("/tmp/botparams.txt")
        .ok()
        .and_then(|t| {
            t.lines()
                .filter_map(|l| l.split_once('='))
                .find(|(k, _)| k.trim() == name)
                .and_then(|(_, v)| v.trim().parse().ok())
        })
        .unwrap_or(default)
}

async fn trade(t: &Trader, delta: f64, mark: f64) {
    // delta > 0 buys. Split into chunks to stay inside the compute budget.
    let side = if delta > 0.0 { Side::Bid } else { Side::Ask };
    let mut left = delta.abs();
    while left > 0.05 {
        let q = left.min(25.0);
        let limit = if delta > 0.0 { mark * 1.05 } else { mark * 0.95 };
        if let Err(e) = t.place_market(side, q, limit).await {
            info!(error = %e, "order failed");
            break;
        }
        left -= q;
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt().with_env_filter(EnvFilter::from_default_env()).init();
    let mut turns = Turns::connect()?;
    let env = BotEnv::from_env()?;
    let trader = Trader::connect(&env)?;
    let cap = env.max_position;

    let entry_thr = param("ENTRY", 0.012);
    let settle = param("SETTLE", 0.0015);
    let stop_pct = param("STOP", 0.012);
    let take_frac = param("TAKE", 0.8);
    let max_hold = param("HOLD", 60.0);
    let size = param("SIZE", 1.0) * cap;
    let lookback = param("LOOK", 24.0) as usize;

    let mut hist: Vec<f64> = Vec::new();
    // Active fade: (anchor, entry mark, direction of shock, entered_step)
    let mut fade: Option<(f64, f64, f64, usize)> = None;
    let mut cooldown = 0usize;

    while let Some(step) = turns.next()? {
        let mark = trader.mark_usd().await?;
        let pos = trader.position().await?.size;
        let k = hist.len();
        hist.push(mark);

        if let Some((anchor, entry, dir, ek)) = fade {
            // dir = +1 if shock was up (we're short)
            let adverse = (mark - entry) * dir / entry;
            let target = anchor + (entry - anchor) * (1.0 - take_frac);
            let reached = (mark - target) * dir <= 0.0;
            let timed = (k - ek) as f64 * 0.5 > max_hold;
            if adverse > stop_pct || reached || timed || step.last {
                trade(&trader, -pos, mark).await;
                info!(step = step.k, mark, adverse, reached, timed, "exit");
                fade = None;
                cooldown = 6;
            }
        } else if pos.abs() > 0.05 {
            trade(&trader, -pos, mark).await;
        } else if cooldown > 0 {
            cooldown -= 1;
        } else if k > lookback + 3 && !step.last {
            // Baseline: median of marks from before the recent window.
            let mut base: Vec<f64> = hist[k - lookback - 3..k - 3].to_vec();
            base.sort_by(|a, b| a.partial_cmp(b).unwrap());
            let anchor = base[base.len() / 2];
            let dev = (mark - anchor) / anchor;
            let settled = hist[k - 2..=k]
                .windows(2)
                .all(|w| ((w[1] - w[0]) / w[0]).abs() < settle);
            if dev.abs() >= entry_thr && settled {
                let dir = dev.signum();
                trade(&trader, -dir * size, mark).await;
                fade = Some((anchor, mark, dir, k));
                info!(step = step.k, mark, anchor, dev, "enter fade");
            }
        }
    }
    Ok(())
}
