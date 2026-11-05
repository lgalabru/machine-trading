//! SPCX market maker for Phoenix: quotes one tick inside the competing maker
//! (never tighter than a floor), skews against inventory, steps aside from
//! informed bursts, and leans with a detected trend.

mod chain;
mod params;
mod strategy;

use anyhow::Result;
use chain::{Chain, Snapshot};
use mtx_botkit::{BotEnv, Trader, Turns, phoenix_rise::math::Side};
use params::Params;
use strategy::Strategy;
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

fn describe(s: &Snapshot, st: &Strategy, n: usize) -> String {
    let o = s.oracle_ticks as i64;
    let fmt = |l: &chain::Level| {
        format!(
            "{}{}x{:.2}",
            l.ticks as i64 - o,
            if st.is_mine(l) { "*" } else { "" },
            l.lots as f64 / 1000.0,
        )
    };
    let b: Vec<String> = s.bids.iter().take(n).map(fmt).collect();
    let a: Vec<String> = s.asks.iter().take(n).map(fmt).collect();
    format!("[{}|{}]", b.join(" "), a.join(" "))
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .init();
    let mut turns = Turns::connect()?;
    let env = BotEnv::from_env()?;
    let trader = Trader::connect(&env)?;
    let mut chain = Chain::new(&trader)?;
    let params = Params::from_env();
    let total_steps = turns.hello().steps;
    let mut st = Strategy::new(params.clone(), chain.account, env.max_position, chain.lot_units, total_steps);
    info!(authority = %trader.authority(), account = %chain.account, end_slot = env.end_slot, steps = total_steps, ?params, "connected");

    let mut first = true;
    let mut last_seq: u64 = 0; // max trade seq seen at the previous read
    let mut settled_seq: u64 = 0; // max trade seq seen at the read before my last successful tx
    let mut placed: Vec<(u64, u64)> = Vec::new();
    let mut tx_failed = false;
    let mut realized_fills: f64 = 0.0; // signed base units filled as maker (for logging)
    let mut cash: f64 = 0.0; // signed quote flow from my maker fills (for logging)

    while let Some(step) = turns.next()? {
        let t0 = std::time::Instant::now();
        let s = match chain.read() {
            Ok(s) => s,
            Err(e) => {
                warn!(error = %e, "read failed");
                continue;
            }
        };
        // Learn my trader ids from orders matching what I placed last turn.
        if first {
            st.learn_ids(&s, true);
        } else {
            for l in s.bids.iter().chain(s.asks.iter()) {
                if let Some(t) = l.trader_id {
                    if !st.other_ids.contains(&t) && placed.iter().any(|&(px, _)| px == l.ticks) {
                        st.my_ids.insert(t);
                    }
                }
            }
        }
        let max_seq = s.trades.iter().map(|t| t.seq).max().unwrap_or(0);
        if first {
            last_seq = max_seq;
            settled_seq = max_seq;
        }
        let new_trades: Vec<_> = s.trades.iter().filter(|t| t.seq > last_seq).collect();
        let taker_trades: Vec<_> = new_trades.iter().copied().filter(|t| t.slot != s.my_tx_slot_hint).collect();
        // My maker fills since the last settlement.
        let mut unsettled: i64 = 0;
        for t in s.trades.iter().filter(|t| t.seq > settled_seq && t.maker == chain.account) {
            unsettled += t.qty_lots;
        }
        for t in new_trades.iter().filter(|t| t.maker == chain.account) {
            realized_fills += t.qty_lots as f64 / chain.lot_units;
            cash -= t.qty_lots as f64 / chain.lot_units * t.price_ticks as f64 * chain.tick_usd;
            info!(px = t.price_ticks as i64 - s.oracle_ticks as i64, qty = t.qty_lots as f64 / chain.lot_units, dslot = t.slot as i64 - step.slot as i64, "fill");
        }
        let pos_lots = s.position_lots + unsettled;
        let n_before = st.orders.len();
        st.ingest_trades(&taker_trades, step.slot);
        for od in st.orders.iter().skip(n_before) {
            info!(k = step.k, buy = od.is_buy, size = od.lots as f64 / chain.lot_units, mine = od.my_lots as f64 / chain.lot_units, "flow");
        }
        st.update_oracle(step.k, s.oracle_ticks);
        last_seq = max_seq;

        let d = st.decide(&s, pos_lots, step.slot, step.k, step.last);

        // Diff my resting orders against the decision.
        let mine_bids: Vec<_> = s.bids.iter().filter(|l| st.is_mine(l)).collect();
        let mine_asks: Vec<_> = s.asks.iter().filter(|l| st.is_mine(l)).collect();
        let mut cancels: Vec<(u64, u64)> = Vec::new();
        let mut new_bids = Vec::new();
        let mut new_asks = Vec::new();
        let keep = |l: &chain::Level, want: &[(u64, u64)]| {
            want.iter().any(|&(px, lots)| px == l.ticks && l.lots * 10 >= lots * 6 && l.lots <= lots + 200)
        };
        for l in &mine_bids {
            if !keep(l, &d.bids) { cancels.push((l.ticks, l.seq)); }
        }
        for l in &mine_asks {
            if !keep(l, &d.asks) { cancels.push((l.ticks, l.seq)); }
        }
        for &(px, lots) in &d.bids {
            if !mine_bids.iter().any(|l| keep(l, &[(px, lots)])) { new_bids.push((px, lots)); }
        }
        for &(px, lots) in &d.asks {
            if !mine_asks.iter().any(|l| keep(l, &[(px, lots)])) { new_asks.push((px, lots)); }
        }
        let takes: Vec<(Side, u64, u64)> = d
            .takes
            .iter()
            .map(|&(buy, lots, limit)| (if buy { Side::Bid } else { Side::Ask }, lots, limit))
            .collect();
        let stale_orders = (mine_bids.len() + mine_asks.len()) as i64 - cancels.len() as i64;
        // If a previous send failed we may have unknown resting orders: cancel everything.
        let cancel_all = tx_failed || (first && false);
        let need_tx = cancel_all || !cancels.is_empty() || !new_bids.is_empty() || !new_asks.is_empty() || !takes.is_empty();
        let mut sent = false;
        if need_tx {
            match chain
                .send_turn(&trader, cancel_all, if cancel_all { &[] } else { &cancels }, &takes, &new_bids, &new_asks)
                .await
            {
                Ok(()) => {
                    sent = true;
                    tx_failed = false;
                }
                Err(e) => {
                    warn!(error = %e, "send failed");
                    tx_failed = true;
                }
            }
        }
        if sent {
            settled_seq = max_seq;
        }
        placed = d.bids.iter().chain(d.asks.iter()).copied().collect();
        if step.k % 10 == 0 || step.last || !d.takes.is_empty() {
            match chain.equity(&trader, s.oracle_ticks).await {
                Ok((collateral, upnl)) => info!(k = step.k, collateral, upnl, equity = collateral + upnl, "equity"),
                Err(e) => warn!(error = %e, "equity failed"),
            }
        }
        let o = s.oracle_ticks as i64;
        info!(
            k = step.k,
            slot = step.slot,
            o = s.oracle_ticks as f64 * chain.tick_usd,
            pos = pos_lots as f64 / chain.lot_units,
            chain_pos = s.position_lots as f64 / chain.lot_units,
            mtm = cash + realized_fills * s.oracle_ticks as f64 * chain.tick_usd,
            book = %describe(&s, &st, 3),
            bids = ?d.bids.iter().map(|(t, l)| (*t as i64 - o, *l)).collect::<Vec<_>>(),
            asks = ?d.asks.iter().map(|(t, l)| (*t as i64 - o, *l)).collect::<Vec<_>>(),
            takes = ?d.takes,
            cancels = cancels.len(),
            kept = stale_orders,
            note = %d.note,
            ms = t0.elapsed().as_millis(),
            "step"
        );
        first = false;
    }
    Ok(())
}
