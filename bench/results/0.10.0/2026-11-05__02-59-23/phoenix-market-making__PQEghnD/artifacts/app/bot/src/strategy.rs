//! The market-making strategy: flow tracking, burst detection, quoting,
//! inventory skew, trend lean, and risk guards.

use std::collections::VecDeque;

use tracing::info;

use crate::chain::{BookView, Level};
use crate::params::Params;

pub const LOTS_PER_UNIT: f64 = 1000.0;
pub const TICK_USD: f64 = 0.01;
pub const MAKER_FEE_BPS: f64 = 0.5;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Dir {
    Up,
    Down,
}

impl Dir {
    pub fn sign(self) -> f64 {
        match self {
            Dir::Up => 1.0,
            Dir::Down => -1.0,
        }
    }
}

/// One taker order as seen through the trade buffer.
#[derive(Debug, Clone, Copy)]
pub struct TakerOrder {
    pub slot: u64,
    pub dir: Dir,
    pub size: f64,
    /// Filled against someone other than us (so its limit reached the
    /// competitor), or filled our whole resting size.
    pub aggressive: bool,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Mode {
    Normal,
    /// Positioned for a jump in `dir`; `since` is the slot the burst began.
    Bet { dir: Dir, since: u64, last_flow: u64 },
}

#[derive(Debug, Clone, Copy)]
pub struct MarketOrder {
    pub buy: bool,
    pub lots: u64,
    pub limit_usd: f64,
    pub label: &'static str,
    /// All or none: the whole transaction fails unless it fills completely.
    pub fok: bool,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Quote {
    pub ticks: u64,
    pub lots: u64,
}

#[derive(Debug, Default)]
pub struct Decision {
    pub cancel_bids: bool,
    pub cancel_asks: bool,
    pub markets: Vec<MarketOrder>,
    pub bid: Option<Quote>,
    pub ask: Option<Quote>,
}

pub struct Obs<'a> {
    pub k: u64,
    pub slot: u64,
    pub oracle: f64,
    pub pos_lots: i64,
    pub book: &'a BookView,
    pub my_account: [u8; 32],
    pub equity: Option<f64>,
    /// Spline fills since the last turn, in base units: (taker bought, taker sold).
    pub spline_flow: (f64, f64),
    /// Slots at which we sent market orders ourselves (not taker flow).
    pub my_taker_slots: &'a [u64],
}

pub struct Strategy {
    pub p: Params,
    pub max_pos_lots: i64,
    pub c_est: Option<f64>,
    pub f_est: f64,
    pub last_oracle: Option<f64>,
    pub last_trade_seq: Option<u64>,
    pub orders: VecDeque<TakerOrder>,
    pub mode: Mode,
    pub last_jump: Option<(u64, Dir)>,
    pub drift_ewma: f64,
    pub flow_ewma: f64,
    pub peak_equity: f64,
    pub dd: f64,
    pub last_bid: Option<Quote>,
    pub last_ask: Option<Quote>,
    pub last_bid_lots_placed: u64,
    pub last_ask_lots_placed: u64,
    pub triggers: u32,
    pub halted: bool,
    /// After a fake-out: ignore same-direction triggers until this slot.
    pub cooldown: Option<(u64, Dir)>,
    /// Flatten the whole position by market this turn (after a jump).
    pub flatten_all: bool,
}

fn bps(oracle: f64, px: f64) -> f64 {
    (px - oracle) / oracle * 1e4
}

impl Strategy {
    pub fn new(p: Params, max_pos: f64) -> Self {
        Self {
            p,
            max_pos_lots: (max_pos * LOTS_PER_UNIT).round() as i64,
            c_est: None,
            f_est: 6.0,
            last_oracle: None,
            last_trade_seq: None,
            orders: VecDeque::new(),
            mode: Mode::Normal,
            last_jump: None,
            drift_ewma: 0.0,
            flow_ewma: 0.0,
            peak_equity: f64::NEG_INFINITY,
            dd: 0.0,
            last_bid: None,
            last_ask: None,
            last_bid_lots_placed: 0,
            last_ask_lots_placed: 0,
            triggers: 0,
            halted: false,
            cooldown: None,
            flatten_all: false,
        }
    }

    fn c(&self) -> f64 {
        self.c_est.unwrap_or(20.0)
    }

    /// Distance in bps of the competitor's best level on each side.
    fn comp_levels<'b>(&self, oracle: f64, book: &'b BookView) -> (Option<&'b Level>, Option<&'b Level>) {
        let lim = 3.0 * self.c().max(20.0);
        let near = |l: &&Level| (bps(oracle, l.ticks as f64 * TICK_USD)).abs() < lim;
        (book.bids.iter().find(near), book.asks.iter().find(near))
    }

    /// Ingest new trades, update flow statistics and the jump detector.
    fn observe(&mut self, obs: &Obs) {
        let oracle = obs.oracle;
        // Jump / drift from the oracle path.
        let mut jump_now = false;
        if let Some(prev) = self.last_oracle {
            let r = (oracle / prev).ln();
            if r.abs() >= self.p.jump_frac {
                let dir = if r > 0.0 { Dir::Up } else { Dir::Down };
                self.last_jump = Some((obs.slot, dir));
                jump_now = true;
                info!(k = obs.k, slot = obs.slot, pct = r * 100.0, "jump observed");
            } else {
                // Per-step drift in bps, half-life ~40 steps.
                let a = 1.0 - 0.5f64.powf(1.0 / 40.0);
                self.drift_ewma = (1.0 - a) * self.drift_ewma + a * r * 1e4;
            }
        }
        self.last_oracle = Some(oracle);

        // New trades since the last turn: events with seq >= last header seq.
        // On the jump step the book uncrosses against stale quotes; those
        // prints are not taker flow, so skip the step's trades.
        let book = obs.book;
        if let (Some(last), false) = (self.last_trade_seq, jump_now) {
            let mut by_seq: Vec<(u64, u64, f64, bool, Dir)> = Vec::new(); // seq, slot, size, aggressive, dir
            for t in book.trades.iter().filter(|t| t.seq >= last && !obs.my_taker_slots.contains(&t.slot)) {
                let size = t.maker_lots.unsigned_abs() as f64 / LOTS_PER_UNIT;
                let dir = if t.maker_lots < 0 { Dir::Up } else { Dir::Down }; // maker sold => taker bought
                let other = t.maker.to_bytes() != obs.my_account;
                let mine_full = !other && {
                    let placed = if t.maker_lots < 0 { self.last_ask_lots_placed } else { self.last_bid_lots_placed };
                    placed > 0 && t.maker_lots.unsigned_abs() >= placed
                };
                match by_seq.iter_mut().find(|e| e.0 == t.seq) {
                    Some(e) => {
                        e.2 += size;
                        e.3 |= other || mine_full;
                    }
                    None => by_seq.push((t.seq, t.slot, size, other || mine_full, dir)),
                }
            }
            // Spline fills are invisible in the trade list: attribute them to the
            // largest same-direction order of this step, or to a synthetic one.
            let (sp_up, sp_down) = obs.spline_flow;
            for (dir, vol) in [(Dir::Up, sp_up), (Dir::Down, sp_down)] {
                if vol < 0.05 {
                    continue;
                }
                match by_seq.iter_mut().filter(|e| e.4 == dir).max_by(|a, b| a.2.partial_cmp(&b.2).unwrap()) {
                    Some(e) => {
                        e.2 += vol;
                        e.3 = true;
                    }
                    None => by_seq.push((0, obs.slot.saturating_sub(1), vol, true, dir)),
                }
            }
            for (seq, slot, size, aggressive, dir) in by_seq {
                self.orders.push_back(TakerOrder { slot, dir, size, aggressive });
                // Flow imbalance EWMA over ~60 orders, signed size-weighted.
                let a = 1.0 / 60.0;
                self.flow_ewma = (1.0 - a) * self.flow_ewma + a * dir.sign() * size.min(2.0);
                info!(k = obs.k, seq, slot, ?dir, size, aggressive, "taker");
            }
        }
        self.last_trade_seq = Some(book.trade_seq);
        while self.orders.len() > 400 {
            self.orders.pop_front();
        }

        // Competitor spread estimates.
        let (cb, ca) = self.comp_levels(oracle, book);
        if let (Some(b), Some(a)) = (cb, ca) {
            let db = -bps(oracle, b.ticks as f64 * TICK_USD);
            let da = bps(oracle, a.ticks as f64 * TICK_USD);
            if self.c_est.is_none() && db > 0.0 && da > 0.0 {
                self.c_est = Some((db + da) / 2.0);
                info!(k = obs.k, c_est = self.c_est, "competitor half-spread estimated");
            }
            for d in [db, da] {
                if d > 0.5 && d < self.f_est {
                    self.f_est = d;
                }
            }
        }

        // Equity and drawdown.
        if let Some(eq) = obs.equity {
            if eq > self.peak_equity {
                self.peak_equity = eq;
            }
            self.dd = self.peak_equity - eq;
        }
    }

    /// Recent large aggressive flow per direction: (count, volume) within the
    /// detector window, for the direction with more volume, if it dominates.
    fn burst(&self, slot: u64) -> Option<(Dir, usize, f64)> {
        let p = &self.p;
        let win: Vec<&TakerOrder> = self
            .orders
            .iter()
            .filter(|o| o.slot + p.det_window > slot && o.aggressive && o.size >= p.det_size)
            .collect();
        let mut best: Option<(Dir, usize, f64)> = None;
        for dir in [Dir::Up, Dir::Down] {
            let same: Vec<&&TakerOrder> = win.iter().filter(|o| o.dir == dir).collect();
            let opp_vol: f64 = win.iter().filter(|o| o.dir != dir).map(|o| o.size).sum();
            let vol: f64 = same.iter().map(|o| o.size).sum();
            if !same.is_empty() && opp_vol <= 0.34 * vol && best.map(|b| vol > b.2).unwrap_or(true) {
                best = Some((dir, same.len(), vol));
            }
        }
        best
    }

    /// Burst detector: returns a direction when the recent flow looks informed.
    fn detect(&self, slot: u64) -> Option<Dir> {
        match self.burst(slot) {
            Some((dir, n, vol)) if (n >= self.p.det_n && vol >= self.p.det_vol) || (n >= 2 && vol >= self.p.det_vol_alone) => Some(dir),
            _ => None,
        }
    }

    /// Slot of the earliest large aggressive order of `dir` in the window.
    fn burst_start(&self, slot: u64, dir: Dir) -> u64 {
        self.orders
            .iter()
            .filter(|o| o.slot + self.p.det_window > slot && o.aggressive && o.size >= self.p.det_size && o.dir == dir)
            .map(|o| o.slot)
            .min()
            .unwrap_or(slot)
    }

    /// Weaker signal: enough one-sided flow to stop quoting against it.
    fn pull_side(&self, slot: u64) -> Option<Dir> {
        match self.burst(slot) {
            Some((dir, n, vol)) if n >= self.p.pull_n && vol >= self.p.pull_vol => Some(dir),
            _ => None,
        }
    }

    pub fn decide(&mut self, obs: &Obs) -> Decision {
        self.observe(obs);
        let p = self.p.clone();
        let oracle = obs.oracle;
        let pos = obs.pos_lots;
        let cap = self.max_pos_lots;
        let mut d = Decision::default();
        let mut pos_after = pos; // expected position after market orders
        let (cb, ca) = {
            let (b, a) = self.comp_levels(oracle, obs.book);
            (b.copied(), a.copied())
        };
        let comp_bid_bps = cb.map(|l| -bps(oracle, l.ticks as f64 * TICK_USD));
        let comp_ask_bps = ca.map(|l| bps(oracle, l.ticks as f64 * TICK_USD));

        // --- Risk guards.
        if self.dd >= p.dd_hard {
            if !self.halted {
                info!(k = obs.k, dd = self.dd, "hard drawdown: halting");
            }
            self.halted = true;
        }
        let risk_cut = self.dd >= p.dd_soft || self.halted;
        if self.halted {
            d.cancel_bids = true;
            d.cancel_asks = true;
            if pos != 0 {
                d.markets.push(flatten(pos, oracle, self.c()));
            }
            return d;
        }

        // --- Bet state machine.
        let jump_recent = |dir: Dir, within: u64| {
            self.last_jump
                .map(|(s, jd)| jd == dir && obs.slot.saturating_sub(s) <= within)
                .unwrap_or(false)
        };
        match self.mode {
            Mode::Normal => {
                if p.enabled_bets && !risk_cut {
                    if let Some(dir) = self.detect(obs.slot) {
                        let cooling = self.cooldown.map(|(until, d)| d == dir && obs.slot < until).unwrap_or(false);
                        if !jump_recent(dir, 16) && !cooling {
                            let since = self.burst_start(obs.slot, dir);
                            self.mode = Mode::Bet { dir, since, last_flow: obs.slot };
                            self.triggers += 1;
                            info!(k = obs.k, slot = obs.slot, ?dir, since, "burst trigger");
                        }
                    }
                }
            }
            Mode::Bet { dir, since, mut last_flow } => {
                if let Some(o) = self.orders.iter().rev().find(|o| o.dir == dir && o.aggressive && o.size >= p.det_size) {
                    last_flow = last_flow.max(o.slot);
                }
                let other = if dir == Dir::Up { Dir::Down } else { Dir::Up };
                let jumped = jump_recent(dir, 2);
                let opposite = jump_recent(other, 2);
                let timeout = obs.slot.saturating_sub(since) > p.bet_timeout;
                let flip = self.detect(obs.slot) == Some(other);
                if jumped || opposite || timeout || risk_cut || flip {
                    info!(k = obs.k, slot = obs.slot, ?dir, jumped, opposite, timeout, flip, "bet over");
                    if flip && !risk_cut {
                        self.mode = Mode::Bet { dir: other, since: obs.slot, last_flow: obs.slot };
                        self.triggers += 1;
                        info!(k = obs.k, slot = obs.slot, dir = ?other, "burst flip");
                    } else {
                        self.mode = Mode::Normal;
                        self.flatten_all = jumped || opposite;
                        if timeout {
                            self.cooldown = Some((obs.slot + p.cooldown_slots, dir));
                        }
                    }
                } else {
                    self.mode = Mode::Bet { dir, since, last_flow };
                }
            }
        }

        if let Mode::Bet { dir, .. } = self.mode {
            // Hold the position, no quotes.
            let target = ((cap - 100) as f64 * p.bet_frac * dir.sign()).round() as i64;
            let delta = target - pos;
            if delta.abs() >= 100 {
                let buy = delta > 0;
                let limit = oracle * (1.0 + dir.sign() * 2.5 * self.c() / 1e4);
                d.markets.push(MarketOrder { buy, lots: delta.unsigned_abs(), limit_usd: limit, label: "bet", fok: true });
                pos_after = target;
            }
            d.cancel_bids = true;
            d.cancel_asks = true;
            self.last_bid = None;
            self.last_ask = None;
            return d;
        }

        // --- Leaving a bet or over-inventory: flatten by market.
        // `pos_lo..pos_hi` brackets the position after the market orders:
        // all-or-none orders are certain, pick-offs may fill partially.
        let mut pos_lo = pos;
        let mut pos_hi = pos;
        let band = (p.q_soft * LOTS_PER_UNIT) as i64;
        let flatten_all = std::mem::take(&mut self.flatten_all);
        if flatten_all && pos != 0 {
            d.markets.push(flatten(pos, oracle, self.c()));
            pos_after = 0;
            pos_lo = 0;
            pos_hi = 0;
        } else if pos.abs() > band + 2000 {
            let excess = pos - pos.signum() * band;
            d.markets.push(flatten(excess, oracle, self.c()));
            pos_after = pos - excess;
            pos_lo = pos_after;
            pos_hi = pos_after;
        }

        // --- Pick-offs: resting orders crossed against the oracle (stale
        // competitor quotes after a jump, far mainnet orders after a crash).
        if p.enabled_pickoff {
            let thr = p.pickoff_bps;
            let crossed_asks: Vec<&Level> = obs.book.asks.iter().filter(|l| bps(oracle, l.ticks as f64 * TICK_USD) <= -thr).collect();
            if !crossed_asks.is_empty() {
                let room = (cap - pos_hi).max(0) as u64;
                let total: u64 = crossed_asks.iter().map(|l| l.lots).sum();
                let lots = total.min(room);
                let worst = crossed_asks.iter().map(|l| l.ticks).max().unwrap();
                if lots >= 100 {
                    d.markets.push(MarketOrder { buy: true, lots, limit_usd: worst as f64 * TICK_USD, label: "pickoff", fok: false });
                    pos_hi += lots as i64;
                    pos_after += lots as i64;
                    info!(k = obs.k, lots, worst = worst as f64 * TICK_USD, oracle, "pick off asks");
                }
            }
            let crossed_bids: Vec<&Level> = obs.book.bids.iter().filter(|l| bps(oracle, l.ticks as f64 * TICK_USD) >= thr).collect();
            if !crossed_bids.is_empty() {
                let room = (cap + pos_lo).max(0) as u64;
                let total: u64 = crossed_bids.iter().map(|l| l.lots).sum();
                let lots = total.min(room);
                let worst = crossed_bids.iter().map(|l| l.ticks).min().unwrap();
                if lots >= 100 {
                    d.markets.push(MarketOrder { buy: false, lots, limit_usd: worst as f64 * TICK_USD, label: "pickoff", fok: false });
                    pos_lo -= lots as i64;
                    pos_after -= lots as i64;
                    info!(k = obs.k, lots, worst = worst as f64 * TICK_USD, oracle, "pick off bids");
                }
            }
        }

        // --- Quotes.
        let c = self.c();
        let q = pos_after as f64 / LOTS_PER_UNIT;
        let mut q_star = 0.0;
        if p.enabled_lean && !risk_cut {
            q_star = (p.lean_gain_flow * self.flow_ewma + p.lean_gain_drift * self.drift_ewma)
                .clamp(-p.lean_max, p.lean_max);
        }
        let dq = q - q_star;
        let h_base = p.rho * c;
        let floor = p.h_floor_bps.max(MAKER_FEE_BPS + 1.0);
        let h_max = p.h_max_mult * c;
        let target = |comp: Option<f64>| -> f64 {
            let mut h = h_base;
            if p.inside {
                if let Some(cd) = comp {
                    h = h.min(cd - 0.75); // one tick (0.7 bp) inside
                }
            }
            h
        };
        let skew = p.skew_bps_per_unit * dq;
        let mut bid_h = (target(comp_bid_bps) + skew).clamp(floor, h_max);
        let mut ask_h = (target(comp_ask_bps) - skew).clamp(floor, h_max);
        if risk_cut {
            bid_h = bid_h.max(c);
            ask_h = ask_h.max(c);
        }
        // Sizes shrink on the side that adds to |dq|.
        let shrink = |adds: bool| -> f64 {
            if adds {
                (1.0 - dq.abs() / p.q_soft).clamp(0.0, 1.0)
            } else {
                1.0 + (dq.abs() / p.q_soft).min(0.5)
            }
        };
        let size_mult = if risk_cut { 0.25 } else { 1.0 };
        let bid_size = p.size * size_mult * shrink(dq > 0.0);
        let ask_size = p.size * size_mult * shrink(dq < 0.0);
        let bid_room = (cap - pos_hi).max(0);
        let ask_room = (cap + pos_lo).max(0);
        let bid_lots = ((bid_size * LOTS_PER_UNIT) as i64).min(bid_room).max(0) as u64;
        let ask_lots = ((ask_size * LOTS_PER_UNIT) as i64).min(ask_room).max(0) as u64;
        let bid_ticks = (oracle * (1.0 - bid_h / 1e4) / TICK_USD).floor() as u64;
        let ask_ticks = (oracle * (1.0 + ask_h / 1e4) / TICK_USD).ceil() as u64;
        let pull = if p.enabled_bets { self.pull_side(obs.slot) } else { None };
        let mut bid = if bid_lots >= 100 { Some(Quote { ticks: bid_ticks, lots: bid_lots }) } else { None };
        let mut ask = if ask_lots >= 100 { Some(Quote { ticks: ask_ticks, lots: ask_lots }) } else { None };
        match pull {
            Some(Dir::Up) => ask = None,
            Some(Dir::Down) => bid = None,
            None => {}
        }

        // Keep a resting order when it is close enough to the target.
        let keep = |current: &[crate::chain::MyOrder], want: Option<Quote>, is_bid: bool| -> bool {
            match (current, want) {
                ([], None) => true,
                ([o], Some(w)) => {
                    let off = o.ticks as i64 - w.ticks as i64;
                    let ok_price = off.abs() <= p.requote_ticks;
                    // Never keep an order that got tighter than the floor or exceeds room.
                    let dist = if is_bid { -bps(oracle, o.ticks as f64 * TICK_USD) } else { bps(oracle, o.ticks as f64 * TICK_USD) };
                    let ok_size = o.lots as f64 >= p.requote_size_frac * w.lots as f64 && o.lots <= w.lots + 100;
                    ok_price && ok_size && dist >= floor - 0.2
                }
                _ => false,
            }
        };
        let keep_bid = keep(&obs.book.my_bids, bid, true) && d.markets.is_empty();
        let keep_ask = keep(&obs.book.my_asks, ask, false) && d.markets.is_empty();
        if !keep_bid {
            d.cancel_bids = !obs.book.my_bids.is_empty();
            d.bid = bid;
            self.last_bid_lots_placed = bid.map(|b| b.lots).unwrap_or(0);
        }
        if !keep_ask {
            d.cancel_asks = !obs.book.my_asks.is_empty();
            d.ask = ask;
            self.last_ask_lots_placed = ask.map(|a| a.lots).unwrap_or(0);
        }
        info!(
            k = obs.k,
            q,
            q_star = format!("{q_star:.2}"),
            bid_h = format!("{bid_h:.1}"),
            ask_h = format!("{ask_h:.1}"),
            cb = comp_bid_bps.map(|x| format!("{x:.1}")),
            ca = comp_ask_bps.map(|x| format!("{x:.1}")),
            keep_bid,
            keep_ask,
            ?pull,
            dd = format!("{:.1}", self.dd),
            flow = format!("{:.2}", self.flow_ewma),
            drift = format!("{:.2}", self.drift_ewma),
            "quote"
        );
        d
    }
}

fn flatten(pos_lots: i64, oracle: f64, c: f64) -> MarketOrder {
    let buy = pos_lots < 0;
    let sign = if buy { 1.0 } else { -1.0 };
    MarketOrder {
        buy,
        lots: pos_lots.unsigned_abs(),
        limit_usd: oracle * (1.0 + sign * 2.5 * c / 1e4),
        label: "flatten",
        fok: true,
    }
}
