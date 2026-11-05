use anyhow::Result;
use mtx_botkit::{BotEnv, Side, Trader, Turns};
use tracing::info;
use tracing_subscriber::EnvFilter;

fn envf(k: &str, d: f64) -> f64 {
    std::env::var(k).ok().and_then(|v| v.parse().ok()).unwrap_or(d)
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .init();
    let mut turns = Turns::connect()?;
    let env = BotEnv::from_env()?;
    let trader = Trader::connect(&env)?;
    let cap = env.max_position.min(20.0);
    let max_inv = envf("MAXINV", 8.0);
    let qsize = envf("QSIZE", 1.0);
    let skew_bp = envf("SKEW", 1.0); // bp per SPCX of inventory
    let min_bp = envf("MINBP", 4.0);
    let flow_th = envf("FLOWTH", 0.8);
    let decay = envf("DECAY", 0.9);
    let frac = envf("FRAC", 0.9);
    let mut last_pos = 0.0f64;
    let mut flow = 0.0f64; // signed: positive = I've been buying (takers selling)

    while let Some(step) = turns.next()? {
        let pos = trader.position().await?.size;
        flow = flow * decay + (pos - last_pos);
        last_pos = pos;
        if step.last {
            let _ = trader.cancel_all();
            continue;
        }
        let o = trader.oracle_usd().await?;
        let _ = trader.cancel_all();
        // competitor's best prices after my cancel
        let book = trader.luce.bbo().await?;
        let m = trader.phoenix.market(&env.symbol)?;
        let bid_c = trader.phoenix.ticks_to_usd(m, book.best_bid_ticks().unwrap_or(0));
        let ask_c = trader.phoenix.ticks_to_usd(m, book.best_ask_ticks().unwrap_or(0));
        let center = o * (1.0 - skew_bp * 1e-4 * pos);
        let minoff = o * min_bp * 1e-4;
        let doff = if bid_c > 0.0 { (o - bid_c).max(0.0) } else { 0.0 };
        let aoff = if ask_c > 0.0 { (ask_c - o).max(0.0) } else { 0.0 };
        let bid = center - minoff.max(frac * doff);
        let ask = center + minoff.max(frac * aoff);
        let mut bq = qsize.min(cap - pos).min(max_inv - pos).max(0.0);
        let mut aq = qsize.min(cap + pos).min(max_inv + pos).max(0.0);
        if flow > flow_th { bq = 0.0; }
        if flow < -flow_th { aq = 0.0; }
        info!(k = step.k, o, pos, flow, bid, ask, bid_c, ask_c, "q");
        if bq >= 0.1 { if let Err(e) = trader.place_limit(Side::Bid, bid, bq).await { info!("bid err {e}"); } }
        if aq >= 0.1 { if let Err(e) = trader.place_limit(Side::Ask, ask, aq).await { info!("ask err {e}"); } }
    }
    Ok(())
}
