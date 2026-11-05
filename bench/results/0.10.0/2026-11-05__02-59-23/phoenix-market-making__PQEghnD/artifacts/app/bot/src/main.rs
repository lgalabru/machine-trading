//! SPCX market maker on Phoenix.

mod chain;
mod params;
mod strategy;

use std::time::Instant;

use anyhow::{Context, Result};
use mtx_botkit::{BotEnv, Side, Trader, Turns, phoenix_rise};
use phoenix_rise::core::MarketOrderTicket;
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

use chain::Chain;
use params::Params;
use strategy::{Decision, LOTS_PER_UNIT, Obs, Strategy, TICK_USD};

fn ticks_to_usd(t: u64) -> f64 {
    t as f64 * TICK_USD
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .with_writer(std::io::stderr)
        .init();
    let mut turns = Turns::connect()?;
    let env = BotEnv::from_env()?;
    let trader = Trader::connect(&env)?;
    let mut chain = Chain::new(&trader)?;
    let hello = turns.hello().clone();
    info!(authority = %trader.authority(), account = %trader.account, end_slot = env.end_slot, steps = hello.steps, "connected");

    let params = Params::load();
    info!(?params, "params");
    let mut strat = Strategy::new(params, env.max_position);
    let mut order_id: u64 = 1;
    let my_account = trader.account.to_bytes();
    let mut last_splines: Option<chain::SplineFills> = None;
    let mut my_taker_slots: Vec<u64> = Vec::new();

    while let Some(step) = turns.next()? {
        let t0 = Instant::now();
        match turn(&trader, &mut chain, &mut strat, step.k, step.slot, my_account, &mut order_id, &mut last_splines, &mut my_taker_slots).await {
            Ok(()) => {}
            Err(e) => {
                warn!(step = step.k, error = %e, "turn failed");
                // Leave nothing stale behind if we can help it.
                if let Err(e2) = trader.cancel_all() {
                    warn!(error = %e2, "cancel all after failure also failed");
                }
            }
        }
        let ms = t0.elapsed().as_millis() as u64;
        if ms > 200 || step.k % 50 == 0 {
            info!(step = step.k, ms, "turn done");
        }
        if step.last {
            info!(triggers = strat.triggers, dd = strat.dd, "session end");
        }
    }
    Ok(())
}

async fn turn(
    trader: &Trader,
    chain: &mut Chain,
    strat: &mut Strategy,
    k: u64,
    slot: u64,
    my_account: [u8; 32],
    order_id: &mut u64,
    last_splines: &mut Option<chain::SplineFills>,
    my_taker_slots: &mut Vec<u64>,
) -> Result<()> {
    let oracle = trader.oracle_usd().await.context("oracle")?;
    let mut book = chain.read_book(trader).context("book")?;
    let splines = chain.read_splines(trader).context("splines")?;
    let spline_flow = match *last_splines {
        Some(prev) => (
            splines.ask_filled.saturating_sub(prev.ask_filled) as f64 / LOTS_PER_UNIT,
            splines.bid_filled.saturating_sub(prev.bid_filled) as f64 / LOTS_PER_UNIT,
        ),
        None => (0.0, 0.0),
    };
    *last_splines = Some(splines);
    let margin = trader.luce.margin().await.context("margin")?;
    let pos_lots = margin.base_lots;
    // Equity at the oracle: collateral + position * (oracle - entry).
    let equity = {
        let acct = trader
            .phoenix
            .hawkeye(trader.authority())
            .view_margin(trader.authority(), 0, 0)
            .await
            .ok()
            .map(|m| m.value);
        acct.map(|m| {
            let coll = m.collateral_quote_lots as f64 / 1e6;
            let entry = margin.entry_price_quote_lots_per_base_lot as f64 * 1e3 / 1e6;
            let upnl = pos_lots as f64 / LOTS_PER_UNIT * (oracle - entry);
            coll + upnl
        })
    };

    // Learn our trader id from the orders we placed last turn.
    if chain.my_trader_id.is_none() {
        let mut found = None;
        for (side, q) in [(Side::Bid, strat.last_bid), (Side::Ask, strat.last_ask)] {
            if let Some(q) = q {
                let raw = match side {
                    Side::Bid => &book.raw_bids,
                    Side::Ask => &book.raw_asks,
                };
                if let Some(o) = raw.iter().find(|o| o.ticks == q.ticks && o.initial_lots == q.lots) {
                    found = Some(o.trader_id);
                }
            }
        }
        if let Some(id) = found {
            chain.my_trader_id = Some(id);
            info!(trader_id = id, "learned trader id");
            chain.split(&mut book);
        }
    }

    let fmt_levels = |ls: &[chain::Level]| -> String {
        ls.iter()
            .take(2)
            .map(|l| format!("{:.2}x{:.3}", ticks_to_usd(l.ticks), l.lots as f64 / LOTS_PER_UNIT))
            .collect::<Vec<_>>()
            .join(" ")
    };
    let fmt_mine = |ls: &[chain::MyOrder]| -> String {
        ls.iter()
            .map(|l| format!("{:.2}x{:.3}", ticks_to_usd(l.ticks), l.lots as f64 / LOTS_PER_UNIT))
            .collect::<Vec<_>>()
            .join(" ")
    };
    info!(
        k,
        slot,
        oracle,
        pos = pos_lots as f64 / LOTS_PER_UNIT,
        eq = equity.map(|e| format!("{e:.2}")),
        bids = %fmt_levels(&book.bids),
        asks = %fmt_levels(&book.asks),
        mb = %fmt_mine(&book.my_bids),
        ma = %fmt_mine(&book.my_asks),
        sp_up = spline_flow.0,
        sp_dn = spline_flow.1,
        sp_mid = splines.mid_ticks as f64 * TICK_USD,
        sp_n = splines.active,
        "state"
    );

    let obs = Obs { k, slot, oracle, pos_lots, book: &book, my_account, equity, spline_flow, my_taker_slots };
    let d: Decision = strat.decide(&obs);

    // Build one transaction: cancels, market orders, then post-only quotes.
    let mut ixs = Vec::new();
    let tx = trader.phoenix.tx();
    let sym = &trader.env.symbol;
    let unknown_id = chain.my_trader_id.is_none();
    if (d.cancel_bids && d.cancel_asks) || (unknown_id && (d.cancel_bids || d.cancel_asks)) {
        ixs.extend(tx.build_cancel_all_orders(trader.authority(), trader.account, sym)?);
    } else {
        if d.cancel_bids {
            ixs.extend(tx.build_cancel_up_to(trader.authority(), trader.account, sym, Side::Bid, None, None)?);
        }
        if d.cancel_asks {
            ixs.extend(tx.build_cancel_up_to(trader.authority(), trader.account, sym, Side::Ask, None, None)?);
        }
    }
    let cancel_ixs = ixs.clone();
    for m in &d.markets {
        let side = if m.buy { Side::Bid } else { Side::Ask };
        let price = if m.buy {
            (m.limit_usd / TICK_USD).floor() * TICK_USD
        } else {
            (m.limit_usd / TICK_USD).ceil() * TICK_USD
        };
        *order_id += 1;
        let mut ticket = MarketOrderTicket::builder()
            .authority(trader.authority())
            .trader_account(trader.account)
            .symbol(sym.clone())
            .side(side)
            .price(price)
            .num_base_lots(m.lots)
            .client_order_id(u128::from(*order_id));
        if m.fok {
            ticket = ticket.min_base_lots_to_fill(m.lots);
        }
        let ticket = ticket.build()?;
        ixs.extend(tx.place_market_order(ticket).await?);
        info!(k, buy = m.buy, lots = m.lots, price, label = m.label, "market order");
    }
    let mut bids = vec![];
    let mut asks = vec![];
    if let Some(b) = d.bid {
        bids.push((ticks_to_usd(b.ticks), b.lots));
    }
    if let Some(a) = d.ask {
        asks.push((ticks_to_usd(a.ticks), a.lots));
    }
    if !bids.is_empty() || !asks.is_empty() {
        ixs.extend(tx.build_multi_limit_order(trader.authority(), trader.account, sym, &bids, &asks, false)?);
    }
    if !ixs.is_empty() {
        let t1 = Instant::now();
        let res = mtx_botkit::send(&trader.rpc, &trader.keypair, ixs, "turn");
        match res {
            Ok(_) => {
                if d.bid.is_some() || !d.cancel_bids {
                    if let Some(b) = d.bid { strat.last_bid = Some(b); }
                } else {
                    strat.last_bid = None;
                }
                if d.ask.is_some() || !d.cancel_asks {
                    if let Some(a) = d.ask { strat.last_ask = Some(a); }
                } else {
                    strat.last_ask = None;
                }
                info!(k, ?bids, ?asks, cancel_bids = d.cancel_bids, cancel_asks = d.cancel_asks, markets = d.markets.len(), ms = t1.elapsed().as_millis() as u64, "sent");
                if !d.markets.is_empty() {
                    my_taker_slots.push(slot);
                    my_taker_slots.push(slot + 1);
                    if my_taker_slots.len() > 64 {
                        my_taker_slots.drain(..32);
                    }
                    // Diagnostics: did the market orders move the position?
                    if let Ok(m2) = trader.luce.margin().await {
                        let sp2 = chain.read_splines(trader).ok();
                        info!(k, before = pos_lots, after = m2.base_lots, splines = ?sp2.map(|s| (s.bid_filled, s.ask_filled)), "post-market position");
                    }
                }
            }
            Err(e) => {
                warn!(k, error = %e, markets = d.markets.len(), "send failed");
                if !d.markets.is_empty() {
                    // Keep the book clean: cancel what we meant to cancel, place
                    // nothing new (sizes assumed the market orders filled).
                    if !cancel_ixs.is_empty() {
                        mtx_botkit::send(&trader.rpc, &trader.keypair, cancel_ixs, "cancel-fallback")?;
                        if d.cancel_bids { strat.last_bid = None; }
                        if d.cancel_asks { strat.last_ask = None; }
                    }
                    return Ok(());
                }
                return Err(e.into());
            }
        }
    }
    Ok(())
}
