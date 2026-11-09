//! A market maker for Phoenix's SPCX perpetual.
//!
//! Two jobs, one transaction a turn:
//!
//! - **Read the flow.** Every taker order lands on the order book account, so
//!   its transaction shows up in `getSignaturesForAddress` for the book, and
//!   `getTransaction` gives its side, size and limit, filled or not. Informed
//!   takers come in bursts: many orders on one side, all with limits two to
//!   four times the competitor's half-spread. On a burst the bot crosses to
//!   the position cap in the burst's direction and holds it: a third of bursts
//!   come before a jump of a few percent, the rest are fake-outs whose cost is
//!   one crossing of the spread. Holding is free in expectation, so the bot
//!   only changes its position on the next opposite burst.
//! - **Make the market.** Quote both sides one tick inside the competing
//!   maker, never closer to the oracle than a minimum, sized so no fill can
//!   take the position past the cap, skewed back toward the target position,
//!   and pulling the side a hot order would hit.

use std::{collections::VecDeque, path::Path, time::Instant};

use anyhow::{Context, Result, anyhow};
use mtx_botkit::{
    BotEnv, Pubkey, Side, Trader, Turns,
    phoenix_rise::{accounts::orderbook::Orderbook, core::MarketOrderTicket},
};
use serde::Deserialize;
use serde_json::{Value, json};
use solana_rpc_client_api::request::RpcRequest;

/// Phoenix's `PlaceMarketOrder` instruction discriminant.
const MARKET_ORDER_DISC: [u8; 8] = [90, 118, 192, 252, 192, 99, 39, 145];
/// USD per tick and lots per base unit for SPCX.
const TICK_USD: f64 = 0.01;
const LOTS_PER_UNIT: f64 = 1000.0;

#[derive(Deserialize, Clone, Debug)]
#[serde(default)]
struct Params {
    /// A taker order is hot when its limit is at least this many competitor
    /// half-spreads from the price...
    hot_ratio: f64,
    /// ...and it is at least this large.
    hot_min_size: f64,
    /// A burst is this many hot orders on one side within `burst_window` slots.
    burst_need: usize,
    burst_window: u64,
    /// A new burst the same way as the last one counts after this many slots.
    retrigger_slots: u64,
    /// The position taken on a burst.
    target: f64,
    /// Skip bursts that start less than this many slots after the session
    /// starts or after the last jump, or less than `late_slots` before the end:
    /// no jump can follow them.
    early_slots: u64,
    late_slots: u64,
    after_jump_slots: u64,
    /// Keep crossing toward the target for this many slots after a burst.
    chase_slots: u64,
    /// How far through the book a crossing order may go, beyond the
    /// competitor half-spread.
    chase_extra_bps: f64,
    /// A move of at least this much between turns is a jump.
    jump_frac: f64,
    /// Quoting.
    quote: bool,
    quote_size: f64,
    min_half_bps: f64,
    /// Pull the side a hot order would hit for this many slots.
    pull_slots: u64,
    /// Inventory beyond the target by this much stops quoting the side that
    /// adds to it.
    skew_stop: f64,
    /// Once a burst has proved fake (no jump `fake_after` slots after it),
    /// or once its jump has landed, let the position drift back to flat
    /// through the quotes.
    fake_unwind: bool,
    fake_after: u64,
    jump_unwind: bool,
    /// The reducing quote's size while unwinding.
    unwind_size: f64,
    /// Margin kept under the position cap.
    cap_margin: f64,
    /// Flatten and stop taking positions past this drawdown.
    dd_stop: f64,
    /// The position is kept small enough that a jump of `jump_max` against
    /// it leaves the drawdown `risk_safety` short of the limit.
    jump_max: f64,
    risk_safety: f64,
    verbose: bool,
}

impl Default for Params {
    fn default() -> Self {
        Self {
            hot_ratio: 1.85,
            hot_min_size: 0.5,
            burst_need: 3,
            burst_window: 8,
            retrigger_slots: 40,
            target: 19.9,
            early_slots: 33,
            late_slots: 65,
            after_jump_slots: 33,
            chase_slots: 30,
            chase_extra_bps: 30.0,
            jump_frac: 0.01,
            quote: true,
            quote_size: 1.0,
            min_half_bps: 3.0,
            pull_slots: 4,
            skew_stop: 3.0,
            fake_unwind: false,
            fake_after: 32,
            jump_unwind: false,
            unwind_size: 1.0,
            cap_margin: 0.05,
            dd_stop: 1e9,
            jump_max: 0.045,
            risk_safety: 20.0,
            verbose: true,
        }
    }
}

fn load_params() -> Params {
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()));
    let candidates = [
        exe_dir.map(|d| d.join("../params.json")),
        Some(Path::new("params.json").to_path_buf()),
    ];
    for path in candidates.into_iter().flatten() {
        if let Ok(text) = std::fs::read_to_string(&path) {
            match serde_json::from_str(&text) {
                Ok(p) => return p,
                Err(e) => eprintln!("bad params {}: {e}", path.display()),
            }
        }
    }
    Params::default()
}

/// A taker's market order, as decoded from its transaction.
#[derive(Debug, Clone)]
struct TakerOrder {
    slot: u64,
    buy: bool,
    size: f64,
    limit_ticks: Option<u64>,
}

fn u64_at(d: &[u8], at: usize) -> Option<u64> {
    d.get(at..at + 8)
        .map(|b| u64::from_le_bytes(b.try_into().unwrap()))
}

fn decode_market_order(d: &[u8]) -> Option<(bool, Option<u64>, u64)> {
    if d.len() < 20 || d[..8] != MARKET_ORDER_DISC || d[8] != 2 {
        return None;
    }
    let buy = d[9] == 0;
    match d[10] {
        1 => Some((buy, Some(u64_at(d, 11)?), u64_at(d, 19)?)),
        0 => Some((buy, None, u64_at(d, 11)?)),
        _ => None,
    }
}

/// Reads new taker orders from the book's transaction history.
struct FlowReader {
    book: String,
    me: String,
    program: String,
    last_sig: Option<String>,
}

impl FlowReader {
    fn poll(&mut self, trader: &Trader) -> Result<Vec<TakerOrder>> {
        let mut out = Vec::new();
        let mut sigs: Vec<Value> = Vec::new();
        let mut before: Option<String> = None;
        // Page back to the last signature seen (normally one page).
        for _ in 0..5 {
            let mut cfg = json!({"limit": 200, "commitment": "confirmed"});
            if let Some(s) = &self.last_sig {
                cfg["until"] = json!(s);
            }
            if let Some(b) = &before {
                cfg["before"] = json!(b);
            }
            let page: Value = trader
                .rpc
                .send(RpcRequest::GetSignaturesForAddress, json!([self.book, cfg]))?;
            let page = page.as_array().cloned().unwrap_or_default();
            let n = page.len();
            before = page
                .last()
                .and_then(|v| v["signature"].as_str().map(str::to_string));
            sigs.extend(page);
            if n < 200 || self.last_sig.is_none() {
                break;
            }
        }
        if let Some(first) = sigs.first() {
            self.last_sig = first["signature"].as_str().map(str::to_string);
        }
        for sig in sigs.iter().rev() {
            if !sig["err"].is_null() {
                continue;
            }
            let Some(s) = sig["signature"].as_str() else {
                continue;
            };
            let slot = sig["slot"].as_u64().unwrap_or(0);
            let tx: Value = match trader.rpc.send(
                RpcRequest::GetTransaction,
                json!([s, {"encoding": "json", "maxSupportedTransactionVersion": 0, "commitment": "confirmed"}]),
            ) {
                Ok(v) => v,
                Err(_) => continue,
            };
            let msg = &tx["transaction"]["message"];
            let keys = msg["accountKeys"].as_array().cloned().unwrap_or_default();
            let signer = keys.first().and_then(|k| k.as_str()).unwrap_or("");
            if signer == self.me {
                continue;
            }
            for ix in msg["instructions"].as_array().cloned().unwrap_or_default() {
                let pid = ix["programIdIndex"]
                    .as_u64()
                    .and_then(|i| keys.get(i as usize))
                    .and_then(|k| k.as_str())
                    .unwrap_or("");
                if pid != self.program {
                    continue;
                }
                let data = bs58::decode(ix["data"].as_str().unwrap_or(""))
                    .into_vec()
                    .unwrap_or_default();
                if let Some((buy, limit_ticks, lots)) = decode_market_order(&data) {
                    out.push(TakerOrder {
                        slot,
                        buy,
                        size: lots as f64 / LOTS_PER_UNIT,
                        limit_ticks,
                    });
                }
            }
        }
        Ok(out)
    }
}

/// The resting orders on the FIFO book, best first: (ticks, lots, owner).
struct Book {
    bids: Vec<(u64, u64, String)>,
    asks: Vec<(u64, u64, String)>,
}

fn read_book(trader: &Trader, key: &Pubkey) -> Result<Book> {
    let acc = trader.rpc.get_account(key)?;
    // The zero-copy view wants aligned bytes.
    let mut words = vec![0u64; acc.data.len().div_ceil(8)];
    let bytes = unsafe {
        std::slice::from_raw_parts_mut(words.as_mut_ptr() as *mut u8, acc.data.len())
    };
    bytes.copy_from_slice(&acc.data);
    let ob = Orderbook::try_from_account_bytes(bytes).map_err(|e| anyhow!("orderbook: {e:?}"))?;
    let side = |s: &mtx_botkit::phoenix_rise::accounts::orderbook::OrderbookSide| {
        s.iter()
            .take(16)
            .map(|e| {
                (
                    e.price_in_ticks().as_inner(),
                    e.order().num_base_lots_remaining().as_inner(),
                    format!("{:?}", e.order().trader_position_id()),
                )
            })
            .collect::<Vec<_>>()
    };
    Ok(Book {
        bids: side(ob.bids()),
        asks: side(ob.asks()),
    })
}

struct State {
    p: Params,
    start_slot: u64,
    end_slot: u64,
    cap: f64,
    c_bps: Option<f64>,
    taker_limits: Vec<f64>,
    hot_buys: VecDeque<u64>,
    hot_sells: VecDeque<u64>,
    prev_oracle: Option<f64>,
    last_jump: Option<u64>,
    trig_dir: i32,
    trig_slot: u64,
    target: f64,
    peak: f64,
    halted: bool,
    my_id: Option<String>,
    last_quotes: Vec<(u64, u64)>,
    next_coid: u128,
}

impl State {
    fn c(&self) -> f64 {
        self.c_bps.unwrap_or(15.0)
    }
}

struct Plan {
    market: Option<(Side, f64, f64)>,
    bids: Vec<(f64, u64)>,
    asks: Vec<(f64, u64)>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let mut turns = Turns::connect()?;
    let env = BotEnv::from_env()?;
    let trader = Trader::connect(&env)?;
    let p = load_params();
    let hello = turns.hello().clone();
    eprintln!(
        "connected {} start {} end {} params {:?}",
        trader.authority(),
        hello.start_slot,
        env.end_slot,
        p
    );
    let market = trader.phoenix.market(&env.symbol)?.clone();
    let book_key: Pubkey = market.market_pubkey.parse()?;
    let program = trader
        .phoenix
        .tx()
        .build_cancel_all_orders(trader.authority(), trader.account, &env.symbol)?
        .first()
        .map(|ix| ix.program_id.to_string())
        .context("no cancel instruction")?;
    let mut flow = FlowReader {
        book: book_key.to_string(),
        me: trader.authority().to_string(),
        program,
        last_sig: None,
    };
    let cap = env.max_position;
    let mut st = State {
        p: p.clone(),
        start_slot: hello.start_slot,
        end_slot: env.end_slot,
        cap,
        c_bps: None,
        taker_limits: Vec::new(),
        hot_buys: VecDeque::new(),
        hot_sells: VecDeque::new(),
        prev_oracle: None,
        last_jump: None,
        trig_dir: 0,
        trig_slot: 0,
        target: 0.0,
        peak: f64::MIN,
        halted: false,
        my_id: None,
        last_quotes: Vec::new(),
        next_coid: 1,
    };

    while let Some(step) = turns.next()? {
        let t0 = Instant::now();
        if let Err(e) = turn(&trader, &env, &mut flow, &mut st, step.slot, &book_key).await {
            eprintln!("turn {} error: {e:#}", step.k);
        }
        if st.p.verbose && t0.elapsed().as_millis() > 200 {
            eprintln!("turn {} slow: {:?}", step.k, t0.elapsed());
        }
    }
    Ok(())
}

async fn turn(
    trader: &Trader,
    env: &BotEnv,
    flow: &mut FlowReader,
    st: &mut State,
    slot: u64,
    book_key: &Pubkey,
) -> Result<()> {
    let p = st.p.clone();
    let oracle = trader.oracle_usd().await?;
    // The takers traded before this step's move: measure their limits
    // against the price they saw.
    let ref_px = st.prev_oracle.unwrap_or(oracle);
    if let Some(prev) = st.prev_oracle {
        if (oracle / prev - 1.0).abs() >= p.jump_frac {
            st.last_jump = Some(slot);
            eprintln!("JUMP slot {} {:.2} -> {:.2}", slot - st.start_slot, prev, oracle);
        }
    }
    st.prev_oracle = Some(oracle);

    let book = read_book(trader, book_key)?;
    // The competitor half-spread, from the makers' quotes before ours.
    if st.c_bps.is_none() {
        if let (Some(b), Some(a)) = (book.bids.first(), book.asks.first()) {
            let half = (a.0 as f64 - b.0 as f64) / 2.0 * TICK_USD / oracle * 1e4;
            st.c_bps = Some((half - 0.35).clamp(6.0, 35.0));
            eprintln!("competitor half-spread est {:.2} bps", st.c_bps.unwrap());
        }
    }
    // Learn our own book identity from the orders we placed.
    if st.my_id.is_none() && !st.last_quotes.is_empty() {
        for (t, l, owner) in book.bids.iter().chain(book.asks.iter()) {
            if st.last_quotes.iter().any(|q| q == &(*t, *l)) {
                st.my_id = Some(owner.clone());
                eprintln!("my book id {owner}");
                break;
            }
        }
    }

    let my_id = st.my_id.clone();
    let mine = |owner: &String| my_id.as_ref() == Some(owner);
    let takers = flow.poll(trader)?;
    let c = st.c();
    for t in &takers {
        let Some(lt) = t.limit_ticks else { continue };
        let lpx = lt as f64 * TICK_USD;
        let bps = if t.buy { lpx / ref_px - 1.0 } else { 1.0 - lpx / ref_px } * 1e4;
        st.taker_limits.push(bps);
        let ratio = bps / c;
        let hot = ratio >= p.hot_ratio && t.size >= p.hot_min_size;
        if p.verbose {
            eprintln!(
                "TAKER s{} {} {:.3} lim {:.1}bps ({:.2}c){}",
                t.slot - st.start_slot,
                if t.buy { "buy" } else { "sell" },
                t.size,
                bps,
                ratio,
                if hot { " HOT" } else { "" }
            );
        }
        if hot {
            if t.buy {
                st.hot_buys.push_back(t.slot);
            } else {
                st.hot_sells.push_back(t.slot);
            }
        }
    }
    for q in [&mut st.hot_buys, &mut st.hot_sells] {
        while q.front().is_some_and(|&s| s + 64 < slot) {
            q.pop_front();
        }
    }

    // Position and value at the oracle.
    let hawk = trader.phoenix.hawkeye(trader.authority());
    let margin = hawk.view_margin(trader.authority(), 0, 0).await?.value;
    let asset = trader.luce.margin().await?;
    let pos = asset.base_lots as f64 / LOTS_PER_UNIT;
    let mark = asset.mark_price_ticks as f64 * TICK_USD;
    let value = margin.collateral_quote_lots as f64 / 1e6
        + asset.unrealized_pnl_quote_lots as f64 / 1e6
        + pos * (oracle - mark);
    st.peak = st.peak.max(value);
    let dd = st.peak - value;

    // Bursts.
    let since = |q: &VecDeque<u64>| -> Vec<u64> {
        q.iter()
            .copied()
            .filter(|&s| s + p.burst_window > slot)
            .collect()
    };
    for (dir, recent) in [(1, since(&st.hot_buys)), (-1, since(&st.hot_sells))] {
        if recent.len() < p.burst_need {
            continue;
        }
        if st.trig_dir == dir && slot < st.trig_slot + p.retrigger_slots {
            continue;
        }
        let start = *recent.iter().min().unwrap();
        let rel = start.saturating_sub(st.start_slot);
        let ok_time = rel >= p.early_slots
            && start + p.late_slots <= st.end_slot
            && st.last_jump.is_none_or(|j| start >= j + p.after_jump_slots);
        st.trig_dir = dir;
        st.trig_slot = slot;
        eprintln!(
            "BURST {} at s{} (n{}) {}",
            if dir > 0 { "up" } else { "down" },
            rel,
            recent.len(),
            if ok_time { "trade" } else { "skip" }
        );
        if ok_time && !st.halted {
            st.target = dir as f64 * p.target;
        }
    }
    if st.target != 0.0 && !st.halted {
        let jumped = st.last_jump.is_some_and(|j| j >= st.trig_slot);
        if (p.jump_unwind && jumped) || (p.fake_unwind && !jumped && slot >= st.trig_slot + p.fake_after) {
            eprintln!("UNWIND {}", if jumped { "after jump" } else { "fake-out" });
            st.target = 0.0;
        }
    }
    if !st.halted && dd > p.dd_stop {
        eprintln!("DRAWDOWN {dd:.1}: flattening");
        st.halted = true;
        st.target = 0.0;
        st.trig_slot = slot;
    }

    let mut plan = Plan {
        market: None,
        bids: vec![],
        asks: vec![],
    };
    let budget = (env.max_drawdown_usd - p.risk_safety - dd) / (oracle * p.jump_max);
    let cap = (st.cap - p.cap_margin).min(budget.max(0.0));
    let target = st.target.clamp(-cap, cap);
    let want = target - pos;
    let over = pos.abs() > cap + 0.3;
    if over {
        eprintln!("RISK pos {pos:.2} over budget {cap:.2} (dd {dd:.1})");
    }
    let chasing = over
        || (slot <= st.trig_slot + p.chase_slots
            && st.last_jump.is_none_or(|j| j < st.trig_slot || st.halted));
    let mut pos_after_lo = pos;
    let mut pos_after_hi = pos;
    if chasing && want.abs() >= 0.3 {
        let reach = (c + p.chase_extra_bps) / 1e4;
        let (side, limit) = if want > 0.0 {
            (Side::Bid, oracle * (1.0 + reach))
        } else {
            (Side::Ask, oracle * (1.0 - reach))
        };
        // Never past the cap, whatever the target says.
        let qty = if over {
            pos.abs() - cap
        } else if want > 0.0 {
            want.min(cap - pos)
        } else {
            (-want).min(cap + pos)
        };
        if qty >= 0.05 {
            plan.market = Some((side, qty, limit));
            if want > 0.0 {
                pos_after_hi = pos + qty;
            } else {
                pos_after_lo = pos - qty;
            }
        }
    }

    // Quotes.
    let last_step = slot + 2 >= st.end_slot;
    if p.quote && !last_step {
        let comp_ask = book
            .asks
            .iter()
            .find(|o| !mine(&o.2))
            .map(|o| o.0)
            .unwrap_or(u64::MAX)
            .min((oracle * (1.0 + c / 1e4) / TICK_USD).ceil() as u64);
        let comp_bid = book
            .bids
            .iter()
            .find(|o| !mine(&o.2))
            .map(|o| o.0)
            .unwrap_or(0)
            .max((oracle * (1.0 - c / 1e4) / TICK_USD).floor() as u64);
        let min_ask = (oracle * (1.0 + p.min_half_bps / 1e4) / TICK_USD).ceil() as u64;
        let max_bid = (oracle * (1.0 - p.min_half_bps / 1e4) / TICK_USD).floor() as u64;
        let ask_t = comp_ask.saturating_sub(1).max(min_ask);
        let bid_t = (comp_bid + 1).min(max_bid);
        let excess = pos - target;
        let pull_ask = st.hot_buys.back().is_some_and(|&s| s + p.pull_slots >= slot);
        let pull_bid = st.hot_sells.back().is_some_and(|&s| s + p.pull_slots >= slot);
        let bid_room = cap - pos_after_hi;
        let ask_room = cap + pos_after_lo;
        let unwinding = excess.abs() >= p.skew_stop;
        let reduce = |base: f64| if unwinding { base.max(p.unwind_size.min(excess.abs())) } else { base };
        let bid_size = if excess < 0.0 { reduce(p.quote_size) } else { p.quote_size }.min(bid_room);
        let ask_size = if excess > 0.0 { reduce(p.quote_size) } else { p.quote_size }.min(ask_room);
        if bid_size >= 0.05 && !pull_bid && excess < p.skew_stop && plan.market.is_none() {
            plan.bids.push((ticks_px(bid_t), (bid_size * LOTS_PER_UNIT) as u64));
        }
        if ask_size >= 0.05 && !pull_ask && excess > -p.skew_stop && plan.market.is_none() {
            plan.asks.push((ticks_px(ask_t), (ask_size * LOTS_PER_UNIT) as u64));
        }
    }

    if p.verbose {
        eprintln!(
            "STEP s{} px {:.2} pos {:.3} tgt {:.1} val {:.2} dd {:.1} c {:.1} mkt {:?} bids {:?} asks {:?}",
            slot - st.start_slot,
            oracle,
            pos,
            st.target,
            value,
            dd,
            c,
            plan.market,
            plan.bids,
            plan.asks
        );
    }
    send_plan(trader, env, st, plan).await
}

async fn send_plan(trader: &Trader, env: &BotEnv, st: &mut State, plan: Plan) -> Result<()> {
    let tx = trader.phoenix.tx();
    let mut ixs = tx.build_cancel_all_orders(trader.authority(), trader.account, &env.symbol)?;
    if let Some((side, qty, limit)) = plan.market {
        let lots = (qty * LOTS_PER_UNIT).floor() as u64;
        let limit_ticks = match side {
            Side::Bid => (limit / TICK_USD).floor(),
            Side::Ask => (limit / TICK_USD).ceil(),
        };
        st.next_coid += 1;
        let ticket = MarketOrderTicket::builder()
            .authority(trader.authority())
            .trader_account(trader.account)
            .symbol(&env.symbol)
            .side(side)
            .num_base_lots(lots)
            .price(limit_ticks / 100.0)
            .client_order_id(st.next_coid)
            .build()?;
        ixs.extend(tx.place_market_order(ticket).await?);
    }
    if !plan.bids.is_empty() || !plan.asks.is_empty() {
        ixs.extend(tx.build_multi_limit_order(
            trader.authority(),
            trader.account,
            &env.symbol,
            &plan.bids,
            &plan.asks,
            true,
        )?);
    }
    st.last_quotes = plan
        .bids
        .iter()
        .chain(plan.asks.iter())
        .map(|(px, lots)| ((px / TICK_USD).round() as u64, *lots))
        .collect();
    mtx_botkit::send(&trader.rpc, &trader.keypair, ixs, "quote")?;
    Ok(())
}

/// A price in ticks as the float whose shortest form is the exact decimal,
/// which is what Phoenix's order tickets round from.
fn ticks_px(ticks: u64) -> f64 {
    ticks as f64 / 100.0
}
