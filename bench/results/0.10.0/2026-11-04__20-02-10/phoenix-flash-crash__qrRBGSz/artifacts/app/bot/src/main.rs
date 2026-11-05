//! Shock-fading bot: tracks a calm anchor price, fades settled deviations,
//! takes profit on the revert, stops out if the shock extends.

use anyhow::Result;
use mtx_botkit::{BotEnv, Side, Trader, Turns};
use tracing::info;
use tracing_subscriber::EnvFilter;

fn p(name: &str, default: f64) -> f64 {
    std::fs::read_to_string("/tmp/botparams")
        .ok()
        .and_then(|t| {
            t.lines()
                .filter_map(|l| l.split_once('='))
                .find(|(k, _)| *k == name)
                .and_then(|(_, v)| v.trim().parse().ok())
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
    let max_pos = env.max_position.min(70.0) * 0.95;

    let enter = p("ENTER", 0.005);
    let band = p("BAND", 0.0035);
    let tp_frac = p("TP", 0.0);
    let stop = p("STOP", 0.02);
    let hold_steps = p("HOLD", 80.0) as u64;
    let full_dev = p("FULL", 0.015);

    let mut anchor: Option<f64> = None;
    let mut hist: Vec<f64> = vec![];
    let mut entry_dev: f64 = 0.0; // signed deviation at entry
    let mut entry_px = 0.0;
    let mut entry_step = 0u64;
    let mut cooldown = 0u64;

    while let Some(step) = turns.next()? {
        let mark = trader.mark_usd().await?;
        hist.push(mark);
        let n = hist.len();
        let a = *anchor.get_or_insert(mark);
        let dev = (mark - a) / a;
        let pos = trader.position().await?.size;
        let settled = n >= 3 && ((mark - hist[n - 3]) / mark).abs() < 0.0015;

        if dev.abs() < band && pos.abs() < 0.01 {
            anchor = Some(a + (mark - a) * 0.1);
        }

        let mut target = pos;
        if pos.abs() > 0.01 {
            let dir = pos.signum();
            let adverse = (entry_px - mark) * dir / entry_px; // positive = losing
            let remaining = dev * -dir; // positive = still displaced in our favour
            let e = entry_dev.abs();
            if adverse > stop {
                target = 0.0;
                anchor = Some(mark);
                cooldown = n as u64 + 20;
            } else if remaining < e * tp_frac || remaining < 0.0 {
                target = 0.0;
            } else if n as u64 - entry_step > hold_steps {
                target = 0.0;
                anchor = Some(mark);
            }
        } else if n as u64 > cooldown && dev.abs() > enter && settled {
            let scale = (dev.abs() / full_dev).clamp(0.4, 1.0);
            target = -dev.signum() * max_pos * scale;
            entry_dev = dev;
            entry_px = mark;
            entry_step = n as u64;
        }

        let delta = target - pos;
        if delta.abs() > 0.5 {
            let (side, lim) = if delta > 0.0 {
                (Side::Bid, mark * 1.01)
            } else {
                (Side::Ask, mark * 0.99)
            };
            let mut left = delta.abs();
            while left > 0.1 {
                let q = left.min(20.0);
                if let Err(e) = trader.place_market(side, q, lim).await {
                    info!(err = %e, "order failed");
                    break;
                }
                left -= q;
            }
            info!(step = step.k, mark, anchor = a, pos, target, "trade");
        }
    }
    Ok(())
}
