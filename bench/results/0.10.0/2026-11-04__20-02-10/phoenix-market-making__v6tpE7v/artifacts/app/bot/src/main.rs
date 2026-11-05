//! Market maker: quotes both sides of SPCX around the oracle, skews against
//! inventory, and widens or pulls the side that informed flow is hitting.

use anyhow::Result;
use mtx_botkit::{BotEnv, Side, Trader, Turns};
use tracing::info;
use tracing_subscriber::EnvFilter;

fn param(name: &str, default: f64) -> f64 {
    std::fs::read_to_string("/tmp/mm_params")
        .ok()
        .and_then(|t| {
            t.lines().find_map(|l| {
                let (k, v) = l.split_once('=')?;
                (k.trim() == name).then(|| v.trim().parse().ok())?
            })
        })
        .unwrap_or(default)
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .init();
    let mut turns = Turns::connect()?;
    let env = BotEnv::from_env()?;
    let trader = Trader::connect(&env)?;

    let frac = param("Q_FRAC", 0.6); // quote offset as a fraction of competitor half-spread
    let min_bp = param("Q_MIN_BP", 3.0);
    let size = param("Q_SIZE", 1.0);
    let soft = param("Q_SOFT", 8.0).min(env.max_position * 0.5);
    let skew_bp = param("Q_SKEW", 1.0); // bp shift per unit of inventory
    let pen_bp = param("Q_PEN", 6.0); // bp widening per unit of recent one-sided fill
    let decay = param("Q_DECAY", 0.85);
    let pull = param("Q_PULL", 3.0); // flow score at which a side is pulled

    let lean = param("Q_LEAN", 0.0);
    let alpha = param("Q_ALPHA", 0.05);
    let trig = param("Q_TRIG", 2.5);
    let follow = param("Q_FOLLOW", 0.0);
    let hold = param("Q_HOLD", 10.0) as u64;
    let mut alert_dir = 0.0f64; // +1: expecting a jump up
    let mut alert_until = 0u64;
    let mut last_oracle: Option<f64> = None;
    let mut drift_bp = 0.0f64;
    let mut half_bp: Option<f64> = None;
    let mut prev_pos = 0.0f64;
    // Recent signed fill flow: positive = I've been buying (sellers hitting me).
    let mut flow = 0.0f64;

    while let Some(step) = turns.next()? {
        let pos = trader.position().await?.size;
        let oracle = trader.oracle_usd().await?;
        if let Some(lo) = last_oracle {
            drift_bp += alpha * ((oracle / lo - 1.0) * 1e4 - drift_bp);
        }
        last_oracle = Some(oracle);
        let d = pos - prev_pos;
        flow = flow * decay + d;
        prev_pos = pos;

        if half_bp.is_none() {
            // First turn: the book holds only competitors.
            let m = trader.phoenix.market(&env.symbol)?;
            let bbo = trader.luce.bbo().await?;
            let h = match (bbo.best_bid_ticks(), bbo.best_ask_ticks()) {
                (Some(b), Some(a)) => {
                    let (b, a) = (trader.phoenix.ticks_to_usd(m, b), trader.phoenix.ticks_to_usd(m, a));
                    (a - b) / 2.0 / oracle * 1e4
                }
                _ => 15.0,
            };
            half_bp = Some(h.clamp(6.0, 35.0));
            info!(half_bp = h, "competitor half-spread");
        }
        let h = half_bp.unwrap();
        let base = (h * frac).max(min_bp);

        let _ = trader.cancel_all();
        if step.last {
            break;
        }
        let mut pos = pos;
        if flow.abs() >= trig {
            // One-sided fills: informed flow is likely ahead of a jump.
            let dir = -flow.signum();
            alert_dir = dir;
            alert_until = step.k + hold;
            let target = dir * follow;
            let need = target - pos;
            if need.abs() >= 0.3 {
                let (side, sgn) = if need > 0.0 { (Side::Bid, 1.0) } else { (Side::Ask, -1.0) };
                let lim = oracle * (1.0 + sgn * 3.0 * h / 1e4);
                if let Err(e) = trader.place_market(side, need.abs().min(4.0), lim).await {
                    info!(?e, "hedge failed");
                }
                pos = trader.position().await?.size;
                info!(step = step.k, pos, "hedged");
            }
            flow = 0.0;
            prev_pos = pos;
        }
        let alert = step.k < alert_until;
        let center = oracle * (1.0 - skew_bp * pos / 1e4 + lean * drift_bp / 1e4);
        for side in [Side::Bid, Side::Ask] {
            // Bid side is hit by sellers (flow > 0 means bids got filled).
            let (adverse, would_add) = match side {
                Side::Bid => (flow, pos + size > soft),
                Side::Ask => (-flow, -pos + size > soft),
            };
            let blocked = alert
                && match side {
                    Side::Bid => alert_dir < 0.0,
                    Side::Ask => alert_dir > 0.0,
                };
            if would_add || adverse >= pull || blocked {
                continue;
            }
            let off = base + pen_bp * adverse.max(0.0);
            let px = match side {
                Side::Bid => center * (1.0 - off / 1e4),
                Side::Ask => center * (1.0 + off / 1e4),
            };
            if let Err(e) = trader.place_limit(side, px, size).await {
                info!(?e, "place failed");
            }
        }
        info!(step = step.k, pos, flow, oracle, "turn");
    }
    let _ = trader.cancel_all();
    Ok(())
}
