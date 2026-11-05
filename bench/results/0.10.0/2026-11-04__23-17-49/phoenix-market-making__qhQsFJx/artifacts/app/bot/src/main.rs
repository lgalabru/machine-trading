//! SPCX market maker for Phoenix.
//!
//! Two jobs, in order of what they earn:
//!
//! 1. **Ride informed flow.** Before most jumps, a burst of large, urgent
//!    taker orders (limits 2-4x the competitor half-spread) hits the book in
//!    the jump's direction. Every taker order is visible on chain the turn it
//!    lands (`getSignaturesForAddress` on the market + `getTransaction`), so
//!    the bot classifies each one, and when a burst shows up it takes a
//!    position the burst's way, up to the cap, and holds it through the jump.
//! 2. **Make a market.** Otherwise it quotes both sides one tick inside the
//!    best other quote, no closer to the oracle than a minimum, skewed
//!    against its inventory so that positions left over from bursts unwind
//!    passively, earning the spread instead of paying it.
//!
//! A drawdown guard shrinks the allowed exposure as the account falls from
//! its peak, so a jump against the position can't breach the limit.

mod chain;
mod params;

use std::collections::VecDeque;

use anyhow::Result;
use mtx_botkit::{BotEnv, Side, Trader, Turns};

use chain::{Book, Chain, TakerOrder};
use params::Params;

/// A burst of informed-looking orders the bot is trading with.
#[derive(Debug, Clone, Copy)]
struct Burst {
    side: i8,
    /// Slot of the burst's first qualifying order: the jump, if it comes,
    /// lands within `window` slots of it.
    first_slot: u64,
    /// The oracle when the burst was detected.
    oracle: f64,
}

struct State {
    p: Params,
    start_slot: u64,
    end_slot: u64,
    tick_usd: f64,
    lot: f64,
    max_pos: f64,
    max_dd: f64,
    /// Competitor half-spread, as a fraction.
    comp: f64,
    /// Informed-looking taker orders: (slot, side).
    cands: VecDeque<(u64, i8)>,
    burst: Option<Burst>,
    /// Slot of the last burst trigger per side (index 0 sell, 1 buy).
    last_trigger: [u64; 2],
    jumps: Vec<(u64, i8)>,
    prev_oracle: f64,
    peak: f64,
    /// Noise flow imbalance, for trend leaning: buy minus sell counts.
    flow_buy: f64,
    flow_sell: f64,
    first_oracle: f64,
    /// The oracle has moved at least once: before that it still reads the
    /// snapshot's price, not the scenario's.
    synced: bool,
    mid0: f64,
    /// The price used last turn (the scenario's, as best known).
    prev_price: f64,
    /// Walk moves (jumps excluded): sum, sum of squares, count.
    walk_sum: f64,
    walk_sq: f64,
    walk_n: f64,
}

impl State {
    /// Trend z-score: the walk's drift and the noise flow's imbalance.
    fn trend_z(&self) -> f64 {
        if self.walk_n < 5.0 {
            return 0.0;
        }
        let sd = (self.walk_sq / self.walk_n).sqrt().max(1e-9);
        let zp = self.walk_sum / (sd * self.walk_n.sqrt());
        let n = self.flow_buy + self.flow_sell;
        let zf = (self.flow_buy - self.flow_sell) / n.max(1.0).sqrt();
        (zp + zf) / std::f64::consts::SQRT_2
    }
}

fn side_idx(side: i8) -> usize {
    if side > 0 { 1 } else { 0 }
}

#[tokio::main]
async fn main() -> Result<()> {
    // Answer the grader's hello first: the world waits for it.
    let mut turns = Turns::connect()?;
    let hello = turns.hello().clone();
    let env = BotEnv::from_env()?;
    let trader = Trader::connect(&env)?;
    let p = Params::load();
    let chain = Chain::new(&trader)?;
    eprintln!(
        "connected authority={} start={} end={} params={:?}",
        trader.authority(),
        hello.start_slot,
        env.end_slot,
        p
    );

    let mut st = State {
        start_slot: hello.start_slot,
        end_slot: env.end_slot,
        tick_usd: chain.tick_usd,
        lot: chain.lot,
        max_pos: env.max_position,
        max_dd: env.max_drawdown_usd,
        comp: 0.0,
        cands: VecDeque::new(),
        burst: None,
        last_trigger: [0, 0],
        jumps: Vec::new(),
        prev_oracle: 0.0,
        peak: f64::MIN,
        flow_buy: 0.0,
        flow_sell: 0.0,
        first_oracle: 0.0,
        synced: false,
        mid0: 0.0,
        prev_price: 0.0,
        walk_sum: 0.0,
        walk_sq: 0.0,
        walk_n: 0.0,
        p,
    };
    let mut chain = chain;

    while let Some(step) = turns.next()? {
        let t0 = std::time::Instant::now();
        if let Err(e) = turn(&mut st, &mut chain, &trader, step.slot, step.k).await {
            eprintln!("turn {} error: {e:#}", step.k);
        }
        let ms = t0.elapsed().as_millis();
        if ms > 300 {
            eprintln!("slow turn k={} ms={ms}", step.k);
        }
    }
    Ok(())
}

async fn turn(st: &mut State, chain: &mut Chain, trader: &Trader, slot: u64, k: u64) -> Result<()> {
    let orders = chain.new_taker_orders()?;
    let oracle = trader.oracle_usd().await?;
    let acct = chain.account(oracle).await?;
    let book = chain.book()?;

    if st.prev_oracle == 0.0 {
        st.prev_oracle = oracle;
        st.first_oracle = oracle;
    }
    if st.prev_price == 0.0 {
        st.prev_price = oracle;
    }
    let raw_prev = st.prev_oracle;
    let rel = slot.saturating_sub(st.start_slot);

    // Competitor half-spread, from the first turn's book: the other makers
    // quote it on both sides of the scenario's price before anyone reacts.
    if st.comp == 0.0 {
        st.comp = 0.0018;
        if let (Some(b), Some(a)) = (book.best_other_bid, book.best_other_ask) {
            let mid = (a + b) as f64 / 2.0;
            let half = ((a - b) as f64 / 2.0 - 0.5) / mid;
            if (0.0005..0.0040).contains(&half) {
                st.comp = half;
                st.mid0 = mid * st.tick_usd;
            }
        }
        eprintln!("comp_bps={:.2} mid0={:.3} oracle={oracle:.3}", st.comp * 1e4, st.mid0);
    }
    // Until the oracle syncs to the scenario, the book's mid is the price.
    let oracle_raw = oracle;
    let oracle = if st.synced || st.mid0 == 0.0 || (oracle_raw / st.prev_oracle - 1.0) != 0.0 {
        oracle_raw
    } else {
        st.mid0
    };

    // Jumps: a single step move far beyond the walk.
    let mv = oracle_raw / raw_prev - 1.0;
    if mv != 0.0 && !st.synced {
        st.synced = true;
    } else if mv != 0.0 && mv.abs() < st.p.jump_thr {
        st.walk_sum += mv;
        st.walk_sq += mv * mv;
        st.walk_n += 1.0;
    }
    if mv.abs() >= st.p.jump_thr {
        let dir = if mv > 0.0 { 1 } else { -1 };
        st.jumps.push((slot, dir));
        eprintln!("JUMP k={k} slot={rel} move={:.4}", mv);
        if let Some(b) = st.burst {
            eprintln!(
                "BURST_END jump side={} dir={} lead={}",
                b.side,
                dir,
                slot.saturating_sub(b.first_slot)
            );
            st.burst = None;
        }
    }

    // Classify the takers' orders, against the oracle they were sent at.
    for o in &orders {
        let px = o.limit_ticks as f64 * st.tick_usd;
        let lim = (px / st.prev_price - 1.0) * o.side as f64;
        let informed = lim >= st.p.k_lim * st.comp && o.size >= st.p.min_size;
        if !informed && lim < 1.6 * st.comp {
            if o.side > 0 {
                st.flow_buy += 1.0;
            } else {
                st.flow_sell += 1.0;
            }
        }
        if informed {
            st.cands.push_back((o.slot, o.side));
        }
        eprintln!(
            "TAKER slot={} side={} size={:.3} lim_bps={:.1} comp_bps={:.1}{}",
            o.slot.saturating_sub(st.start_slot),
            o.side,
            o.size,
            lim * 1e4,
            st.comp * 1e4,
            if informed { " *" } else { "" }
        );
    }
    while let Some(&(s, _)) = st.cands.front() {
        if s + st.p.window < slot {
            st.cands.pop_front();
        } else {
            break;
        }
    }

    // Burst deadline: no jump by the end of the window means a fake-out.
    if let Some(b) = st.burst {
        if slot > b.first_slot + st.p.deadline {
            eprintln!("BURST_END fake side={}", b.side);
            st.burst = None;
        }
    }

    // Burst trigger.
    for side in [1i8, -1i8] {
        let same: Vec<u64> = st
            .cands
            .iter()
            .filter(|(_, s)| *s == side)
            .map(|(sl, _)| *sl)
            .collect();
        if same.len() < st.p.n_trig {
            continue;
        }
        if st.burst.map(|b| b.side == side).unwrap_or(false) {
            continue;
        }
        let first = same[0];
        if st.last_trigger[side_idx(side)] != 0
            && first <= st.last_trigger[side_idx(side)] + st.p.deadline
        {
            // The same burst as last time (or its tail).
            continue;
        }
        let rel_first = first.saturating_sub(st.start_slot);
        let dur = st.end_slot.saturating_sub(st.start_slot);
        let edge = st.p.edge_slots;
        // The jump, if any, lands after now and within `window` slots of
        // the burst's first order: skip when no such slot is allowed.
        let latest = first + st.p.window + 2;
        let too_early = rel_first + st.p.window + 2 < edge;
        let too_late = rel + edge > dur;
        let after_jump = st
            .jumps
            .last()
            .map(|(s, _)| latest < s + edge)
            .unwrap_or(false);
        let all_jumps = st.jumps.len() >= 3;
        // A strong trend before any jump: most likely a trend session,
        // which never jumps.
        let z = st.trend_z();
        let trending = st.jumps.is_empty() && z.abs() > st.p.trend_skip_z;
        if st.p.filters && (too_early || too_late || after_jump || all_jumps || trending) {
            eprintln!(
                "TRIGGER_SKIP side={side} rel={rel} early={too_early} late={too_late} after_jump={after_jump} all={all_jumps} trending={trending} z={z:.2}"
            );
            st.last_trigger[side_idx(side)] = slot;
            continue;
        }
        eprintln!("TRIGGER side={side} rel={rel} first={rel_first} n={}", same.len());
        st.last_trigger[side_idx(side)] = slot;
        st.burst = Some(Burst {
            side,
            first_slot: first,
            oracle,
        });
    }

    // Drawdown guard.
    let value = acct.value;
    st.peak = st.peak.max(value);
    let dd = st.peak - value;
    let jump_risk = oracle * st.p.worst_jump;
    let guard = ((st.max_dd * st.p.dd_budget - dd) / jump_risk).max(0.0);
    let cap = (st.max_pos - st.p.cap_margin).min(guard);

    let pos = acct.position;
    let mut plan = Plan::default();

    // Trend lean: with no burst, a small target the noise flow's way.
    let z = st.trend_z();
    let lean_target = z.signum()
        * (st.p.lean_gain * (z.abs() - st.p.lean_z0)).clamp(0.0, st.p.lean_max);

    if let Some(b) = st.burst {
        let target = b.side as f64 * cap.min(st.p.qmax);
        let need = target - pos;
        if need * b.side as f64 > st.p.min_take {
            let lim = oracle * (1.0 + b.side as f64 * (st.comp + st.p.take_extra));
            plan.take = Some((b.side, need.abs(), lim));
        }
        // Keep a quote on the burst's side only, to collect noise flow
        // the same way while the position builds.
        let room = (target - pos).abs().min(st.p.mm_size);
        quote_side(st, &book, oracle, b.side, room, pos, &mut plan, true);
    } else {
        // Over the guard: shed exposure now.
        if pos.abs() > cap + st.p.min_take {
            let side: i8 = if pos > 0.0 { -1 } else { 1 };
            let lim = oracle * (1.0 + side as f64 * (st.comp + st.p.take_extra));
            plan.take = Some((side, pos.abs() - cap, lim));
        }
        for side in [1i8, -1i8] {
            // Room before the cap if this side fills.
            let room = (cap - pos * side as f64).max(0.0);
            let mut size = st.p.mm_size;
            // Reducing side: offer the whole inventory.
            let excess = pos - lean_target;
            if excess * (side as f64) < 0.0 {
                size = size.max(excess.abs().min(st.p.unwind_size));
            }
            let size = size.min(room);
            quote_side(st, &book, oracle, side, size, excess, &mut plan, false);
        }
    }

    // Hard cap: whatever fills, the position stays inside the limit.
    let hard = st.max_pos - 0.35;
    if let Some((side, q, lim)) = plan.take {
        let room = (hard - pos * side as f64).max(0.0);
        plan.take = Some((side, q.min(room), lim));
    }
    let post = pos + plan.take.map(|(s, q, _)| s as f64 * q).unwrap_or(0.0);
    let lot = st.lot;
    let clamp = |q: Option<(u64, u64)>, room: f64| {
        q.and_then(|(px, lots)| {
            let max_lots = (room.max(0.0) / lot).floor() as u64;
            let lots = lots.min(max_lots);
            (lots > 0).then_some((px, lots))
        })
    };
    plan.bid = clamp(plan.bid, hard - post);
    plan.ask = clamp(plan.ask, hard + post);

    // Execute.
    chain.execute(&plan, &book, st.lot).await?;
    if let Some((side, q, _)) = plan.take {
        if let Ok(after) = chain.account(oracle).await {
            let filled = (after.position - pos).abs();
            let cost = value - after.value;
            eprintln!(
                "TAKE side={side} want={q:.3} filled={filled:.3} cost_usd={cost:.2} cost_bps={:.1} comp_bps={:.1}",
                if filled > 0.0 { cost / (filled * oracle) * 1e4 } else { 0.0 },
                st.comp * 1e4
            );
        }
    }

    eprintln!(
        "STEP k={k} rel={rel} oracle={oracle:.2} pos={pos:.3} value={value:.2} dd={dd:.2} comp={:.1} z={z:.2} lean={lean_target:.1} burst={:?} bid={:?} ask={:?} take={:?} obid={:?} oask={:?}",
        st.comp * 1e4,
        st.burst.map(|b| b.side),
        plan.bid,
        plan.ask,
        plan.take.map(|(s, q, _)| (s, (q * 1000.0).round() / 1000.0)),
        book.best_other_bid,
        book.best_other_ask,
    );
    st.prev_oracle = oracle_raw;
    st.prev_price = oracle;
    let _ = st.first_oracle;
    let _ = st.flow_buy - st.flow_sell;
    let _ = st.burst.map(|b| b.oracle);
    Ok(())
}

/// What to send this turn. `None` for a side means no quote on it.
#[derive(Debug, Default, Clone)]
pub struct Plan {
    /// (side, base units, limit price USD)
    pub take: Option<(i8, f64, f64)>,
    /// (price ticks, base lots)
    pub bid: Option<(u64, u64)>,
    pub ask: Option<(u64, u64)>,
}

/// Quote `side` with `size` base units, one tick inside the best other
/// maker, no closer to the (inventory-skewed) oracle than the minimum.
#[allow(clippy::too_many_arguments)]
fn quote_side(
    st: &State,
    book: &Book,
    oracle: f64,
    side: i8,
    size: f64,
    pos: f64,
    plan: &mut Plan,
    aggressive: bool,
) {
    let lots = (size / st.lot).floor() as u64;
    if lots < (st.p.min_quote / st.lot) as u64 {
        return;
    }
    let p = &st.p;
    let o_ticks = oracle / st.tick_usd;
    // Inventory skew, in bps of the price: long shifts both quotes down.
    let skew = -pos * p.skew_bps / 1e4;
    let min_d = if aggressive { p.mm_min_bps } else { p.mm_min_bps } / 1e4;
    let comp_ticks = st.comp * o_ticks;
    if side > 0 {
        // Bid: inside the best other bid, but no higher than the limit.
        let other = book
            .best_other_bid
            .map(|b| b as f64)
            .unwrap_or(o_ticks - comp_ticks)
            .max(o_ticks - comp_ticks);
        let limit = o_ticks * (1.0 + skew - min_d);
        let limit = limit.min(o_ticks * (1.0 - p.min_edge_bps / 1e4));
        let px = (other + 1.0).min(limit).floor();
        if px > 0.0 {
            plan.bid = Some((px as u64, lots));
        }
    } else {
        let other = book
            .best_other_ask
            .map(|a| a as f64)
            .unwrap_or(o_ticks + comp_ticks)
            .min(o_ticks + comp_ticks);
        let limit = o_ticks * (1.0 + skew + min_d);
        let limit = limit.max(o_ticks * (1.0 + p.min_edge_bps / 1e4));
        let px = (other - 1.0).max(limit).ceil();
        plan.ask = Some((px as u64, lots));
    }
}

#[allow(dead_code)]
fn side_of(s: i8) -> Side {
    if s > 0 { Side::Bid } else { Side::Ask }
}

#[allow(dead_code)]
fn unused(_: TakerOrder) {}
