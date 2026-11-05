//! The market-making policy: where to quote, how much, when to step aside,
//! when to take, and when to lean.

use crate::chain::{Level, Snapshot, TradeEv};
use crate::params::Params;
use mtx_botkit::Pubkey;
use std::collections::{HashSet, VecDeque};

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Alert {
    None,
    Buy,
    Sell,
}

#[derive(Debug, Default, Clone)]
pub struct Decision {
    pub bids: Vec<(u64, u64)>,
    pub asks: Vec<(u64, u64)>,
    /// (is_buy, lots, limit ticks)
    pub takes: Vec<(bool, u64, u64)>,
    pub note: String,
}

/// One taker order as reconstructed from the trade list.
#[derive(Debug, Clone, Copy)]
pub struct TakerOrder {
    pub slot: u64,
    pub is_buy: bool,
    pub lots: i64,
    pub in_burst: bool,
    /// Lots of this order that filled against me.
    pub my_lots: i64,
}

pub struct Strategy {
    pub p: Params,
    pub me: Pubkey,
    pub my_ids: HashSet<u32>,
    pub other_ids: HashSet<u32>,
    pub max_pos_lots: i64,
    pub lot: f64,
    pub comp_half: Option<i64>,
    pub start_oracle: Option<u64>,
    pub orders: VecDeque<TakerOrder>,
    pub alert: Alert,
    pub alert_until: u64,
    pub alert_started_step: u64,
    pub caution: Alert,
    pub caution_until: u64,
    pub oracle_steps: Vec<(u64, i64)>,
    pub last_jump_step: Option<u64>,
    pub jumps_seen: u32,
    pub trend_z: f64,
    pub z_price: f64,
    pub z_flow: f64,
    pub lean_target: f64,
    pub last_lean_take_step: Option<u64>,
    pub flattened_for_alert: bool,
    pub total_steps: u64,
    /// Lots I took last turn (signed), to strip my own fills from the flow.
    pub my_take_lots: i64,
}

fn clamp_i(x: i64, lo: i64, hi: i64) -> i64 {
    x.max(lo).min(hi)
}

impl Strategy {
    pub fn new(p: Params, me: Pubkey, max_pos: f64, lot: f64, total_steps: u64) -> Self {
        Self {
            p,
            me,
            my_ids: HashSet::new(),
            other_ids: HashSet::new(),
            max_pos_lots: (max_pos * lot) as i64,
            lot,
            comp_half: None,
            start_oracle: None,
            orders: VecDeque::new(),
            alert: Alert::None,
            alert_until: 0,
            alert_started_step: 0,
            caution: Alert::None,
            caution_until: 0,
            oracle_steps: Vec::new(),
            last_jump_step: None,
            jumps_seen: 0,
            trend_z: 0.0,
            z_price: 0.0,
            z_flow: 0.0,
            lean_target: 0.0,
            last_lean_take_step: None,
            flattened_for_alert: false,
            total_steps,
            my_take_lots: 0,
        }
    }

    pub fn is_mine(&self, l: &Level) -> bool {
        l.trader_id.map(|t| self.my_ids.contains(&t)).unwrap_or(false)
    }

    pub fn learn_ids(&mut self, s: &Snapshot, first: bool) {
        for l in s.bids.iter().chain(s.asks.iter()) {
            if let Some(t) = l.trader_id {
                if first {
                    self.other_ids.insert(t);
                } else if !self.other_ids.contains(&t) {
                    self.my_ids.insert(t);
                }
            }
        }
    }

    /// Group new trade events into taker orders (same slot and side), after
    /// removing the fills my own takes produced last turn.
    pub fn ingest_trades(&mut self, new_trades: &[&TradeEv], slot: u64) {
        let mut my_take = self.my_take_lots;
        self.my_take_lots = 0;
        let mut grouped: Vec<TakerOrder> = Vec::new();
        for t in new_trades {
            let is_buy = t.qty_lots < 0; // maker sold → taker bought
            let mut lots = t.qty_lots.abs();
            if t.maker != self.me {
                // Strip my own taker fills (they are against other makers).
                if my_take > 0 && is_buy {
                    let used = my_take.min(lots);
                    my_take -= used;
                    lots -= used;
                } else if my_take < 0 && !is_buy {
                    let used = (-my_take).min(lots);
                    my_take += used;
                    lots -= used;
                }
            }
            if lots <= 0 { continue; }
            let mine = if t.maker == self.me { lots } else { 0 };
            if let Some(last) = grouped.last_mut() {
                if last.slot == t.slot && last.is_buy == is_buy {
                    last.lots += lots;
                    last.my_lots += mine;
                    continue;
                }
            }
            grouped.push(TakerOrder { slot: t.slot, is_buy, lots, in_burst: false, my_lots: mine });
        }
        for o in grouped { self.orders.push_back(o); }
        while let Some(o) = self.orders.front() {
            if o.slot + 2000 < slot { self.orders.pop_front(); } else { break; }
        }
    }

    pub fn update_oracle(&mut self, step: u64, oracle: u64) {
        if self.start_oracle.is_none() {
            self.start_oracle = Some(oracle);
        }
        let o = oracle as i64;
        let start = self.start_oracle.unwrap() as f64;
        if let Some(&(_, prev)) = self.oracle_steps.last() {
            let d = o - prev;
            if (d as f64).abs() > start * 0.007 {
                self.last_jump_step = Some(step);
                self.jumps_seen += 1;
            }
        }
        self.oracle_steps.push((step, o));
        let mut n = 0.0;
        let mut sum = 0.0;
        let mut sumsq = 0.0;
        for w in self.oracle_steps.windows(2) {
            let d = (w[1].1 - w[0].1) as f64;
            if d.abs() > start * 0.007 { continue; }
            n += 1.0;
            sum += d;
            sumsq += d * d;
        }
        if n >= 40.0 {
            let mean = sum / n;
            let var = (sumsq / n - mean * mean).max(1e-9);
            self.z_price = sum / (var.sqrt() * n.sqrt());
            let (mut nb, mut ns) = (0.0f64, 0.0f64);
            for o in self.orders.iter() {
                if o.in_burst { continue; }
                if o.is_buy { nb += 1.0 } else { ns += 1.0 }
            }
            self.z_flow = if nb + ns > 0.0 { (nb - ns) / (nb + ns).sqrt() } else { 0.0 };
            self.trend_z = (self.z_price + self.p.flow_weight * self.z_flow) / (1.0 + self.p.flow_weight * self.p.flow_weight).sqrt();
        }
    }

    fn update_alert(&mut self, slot: u64, step: u64, ask_wiped: bool, bid_wiped: bool) {
        let p = &self.p;
        let w = p.burst_window;
        let (mut vb, mut vs, mut nb, mut ns) = (0f64, 0f64, 0u64, 0u64);
        let (mut cb, mut cs, mut maxb, mut maxs) = (0f64, 0f64, 0f64, 0f64);
        for o in self.orders.iter() {
            let size = o.lots as f64 / self.lot;
            if o.slot + w >= slot {
                if o.is_buy { vb += size; nb += 1 } else { vs += size; ns += 1 }
            }
            if o.slot + p.caution_window >= slot {
                if o.is_buy { cb += size; maxb = maxb.max(size) } else { cs += size; maxs = maxs.max(size) }
            }
        }
        if ask_wiped && nb >= 2 { vb += p.wipe_bonus; }
        if bid_wiped && ns >= 2 { vs += p.wipe_bonus; }
        let buy_sig = vb >= p.burst_volume && nb >= p.burst_count && vb >= p.burst_ratio * vs.max(0.3);
        let sell_sig = vs >= p.burst_volume && ns >= p.burst_count && vs >= p.burst_ratio * vb.max(0.3);
        let new = if buy_sig { Alert::Buy } else if sell_sig { Alert::Sell } else { Alert::None };
        if new != Alert::None {
            if self.alert != new {
                self.alert_started_step = step;
                self.flattened_for_alert = false;
            }
            self.alert = new;
            self.alert_until = slot + p.alert_slots;
            // Mark the window's orders as burst flow (kept out of the trend count).
            for o in self.orders.iter_mut() {
                if o.slot + w >= slot && o.is_buy == (new == Alert::Buy) { o.in_burst = true; }
            }
        } else if slot > self.alert_until {
            self.alert = Alert::None;
        }
        let cbuy = maxb >= p.caution_size || cb >= p.caution_volume;
        let csell = maxs >= p.caution_size || cs >= p.caution_volume;
        let newc = if cbuy && !csell { Alert::Buy } else if csell && !cbuy { Alert::Sell } else { Alert::None };
        if newc != Alert::None {
            self.caution = newc;
            self.caution_until = slot + p.caution_slots;
        } else if slot > self.caution_until {
            self.caution = Alert::None;
        }
        if let Some(j) = self.last_jump_step {
            if j == step {
                self.alert = Alert::None;
                self.alert_until = 0;
                self.caution = Alert::None;
            }
        }
    }

    fn avail_lots(&self, levels: &[Level], within: impl Fn(i64) -> bool) -> i64 {
        levels
            .iter()
            .filter(|l| !self.is_mine(l) && within(l.ticks as i64))
            .map(|l| l.lots as i64)
            .sum()
    }

    pub fn decide(&mut self, s: &Snapshot, pos_lots: i64, slot: u64, step: u64, last: bool) -> Decision {
        let o = s.oracle_ticks as i64;
        let mut d = Decision::default();
        let lot = self.lot;

        let cb0 = s.bids.iter().find(|l| !self.is_mine(l)).map(|l| l.ticks as i64);
        let ca0 = s.asks.iter().find(|l| !self.is_mine(l)).map(|l| l.ticks as i64);
        if self.comp_half.is_none() {
            if let (Some(b), Some(a)) = (cb0, ca0) {
                self.comp_half = Some(((a - b) / 2).max(2));
            }
        }
        let c = self.comp_half.unwrap_or(15);
        let cb = cb0.filter(|&b| o - b <= 3 * c);
        let ca = ca0.filter(|&a| a - o <= 3 * c);
        self.update_alert(slot, step, ca.is_none(), cb.is_none());
        let p = self.p.clone();
        let steps_left = self.total_steps.saturating_sub(step + 1);

        let max_pos = self.max_pos_lots as f64 / lot - p.cap_margin;
        let mut pos = pos_lots as f64 / lot;
        let mut takes: Vec<(bool, u64, u64)> = Vec::new();

        // ---- Trend / lean target ----
        let trend_allowed = self.jumps_seen == 0 && p.trend_lean > 0.0;
        let trend_confident = trend_allowed && self.trend_z.abs() >= p.trend_z_full;
        let mut target = 0.0f64;
        if trend_allowed && self.trend_z.abs() >= p.trend_z_on {
            let f = ((self.trend_z.abs() - p.trend_z_on) / (p.trend_z_full - p.trend_z_on)).clamp(0.25, 1.0);
            target = f * p.trend_lean * self.trend_z.signum();
        }
        // A confident trend session has no jumps: bursts are fake-outs.
        let alert = if trend_confident { Alert::None } else { self.alert };
        let caution = if trend_confident { Alert::None } else { self.caution };
        let burst_ok = slot >= 36 && steps_left > 20;
        if alert != Alert::None && burst_ok && p.burst_lean > 0.0 {
            target += match alert { Alert::Buy => p.burst_lean, Alert::Sell => -p.burst_lean, _ => 0.0 };
        }
        target = target.clamp(-(max_pos - 1.0), max_pos - 1.0);
        self.lean_target = target;

        // ---- Jump arbitrage: resting orders crossed with the oracle ----
        {
            let edge = p.arb_min_ticks;
            let room_buy = ((max_pos - pos) * lot).max(0.0) as i64;
            let room_sell = ((max_pos + pos) * lot).max(0.0) as i64;
            if alert != Alert::Sell && room_buy > 0 {
                let mut buy = 0i64;
                let mut worst = 0i64;
                for l in s.asks.iter().filter(|l| !self.is_mine(l)) {
                    if (l.ticks as i64) <= o - edge && buy < room_buy { buy += l.lots as i64; worst = l.ticks as i64; } else { break; }
                }
                if buy > 0 {
                    let lots = buy.min(room_buy);
                    takes.push((true, lots as u64, worst as u64));
                    pos += lots as f64 / lot;
                }
            }
            if alert != Alert::Buy && room_sell > 0 {
                let mut sell = 0i64;
                let mut worst = 0i64;
                for l in s.bids.iter().filter(|l| !self.is_mine(l)) {
                    if (l.ticks as i64) >= o + edge && sell < room_sell { sell += l.lots as i64; worst = l.ticks as i64; } else { break; }
                }
                if sell > 0 {
                    let lots = sell.min(room_sell);
                    takes.push((false, lots as u64, worst as u64));
                    pos -= lots as f64 / lot;
                }
            }
        }

        // ---- Taking toward a target (flatten against an alert; build a lean) ----
        let take_max = (p.take_max_frac * c as f64) as i64;
        let flat_max = (p.flatten_max_frac * c as f64) as i64;
        let avail_asks = |max: i64| self.avail_lots(&s.asks, |t| t <= o + max);
        let avail_bids = |max: i64| self.avail_lots(&s.bids, |t| t >= o - max);
        // Returns true when a take was issued.
        let take_toward = |want: f64, pos: &mut f64, takes: &mut Vec<(bool, u64, u64)>, max: i64| -> bool {
            let diff = want - *pos;
            if diff > 0.05 {
                let lots = ((diff * lot) as i64).min(avail_asks(max)).min(((max_pos - *pos) * lot) as i64);
                if lots >= 100 { takes.push((true, lots as u64, (o + max) as u64)); *pos += lots as f64 / lot; return true; }
            } else if diff < -0.05 {
                let lots = (((-diff) * lot) as i64).min(avail_bids(max)).min(((max_pos + *pos) * lot) as i64);
                if lots >= 100 { takes.push((false, lots as u64, (o - max) as u64)); *pos -= lots as f64 / lot; return true; }
            }
            false
        };
        match alert {
            Alert::Buy if pos < target.max(0.0) - p.flatten_against && !self.flattened_for_alert => {
                if take_toward(target.max(0.0), &mut pos, &mut takes, flat_max) { self.flattened_for_alert = true; }
            }
            Alert::Sell if pos > target.min(0.0) + p.flatten_against && !self.flattened_for_alert => {
                if take_toward(target.min(0.0), &mut pos, &mut takes, flat_max) { self.flattened_for_alert = true; }
            }
            Alert::Buy if p.burst_lean > 0.0 && burst_ok && pos < target - 1.0 => {
                take_toward(target, &mut pos, &mut takes, take_max);
            }
            Alert::Sell if p.burst_lean > 0.0 && burst_ok && pos > target + 1.0 => {
                take_toward(target, &mut pos, &mut takes, take_max);
            }
            _ => {}
        }
        if alert == Alert::None && trend_confident && target != 0.0 && (pos - target).abs() > p.lean_take_gap {
            let recent = self.last_lean_take_step.map(|k| step - k < p.lean_take_every).unwrap_or(false);
            if !recent && take_toward(target, &mut pos, &mut takes, take_max) {
                self.last_lean_take_step = Some(step);
            }
        }

        // ---- Quotes ----
        let d_min = ((p.min_frac * c as f64).round() as i64).max(p.min_ticks);
        let d_max = ((p.max_frac * c as f64).round() as i64).max(d_min);
        let d_reduce = p.reduce_min_ticks.max(1);
        let excess = pos - target;
        // Reduce side: the side whose fill brings the position toward the target.
        let reduce_bid = excess < -p.reduce_threshold;
        let reduce_ask = excess > p.reduce_threshold;
        let dist = |other: Option<i64>, sign: i64, reduce: bool| -> i64 {
            let floor = if reduce { d_reduce } else { d_min };
            match other {
                Some(q) => ((q - o) * sign - p.undercut_ticks).max(floor).min(d_max),
                None => d_max,
            }
        };
        let mut bid_d = dist(cb, -1, reduce_bid);
        let mut ask_d = dist(ca, 1, reduce_ask);
        let skew = (excess * p.skew_ticks).round() as i64;
        if !reduce_bid { bid_d = clamp_i(bid_d + skew, d_min, d_max + 2 * c); }
        if !reduce_ask { ask_d = clamp_i(ask_d - skew, d_min, d_max + 2 * c); }

        let band = p.inv_band;
        let size_for = |adds: f64| -> f64 {
            let room = (band - adds).clamp(0.0, band) / band;
            p.quote_size * (0.25 + 0.75 * room)
        };
        let mut bid_sz = size_for(excess);
        let mut ask_sz = size_for(-excess);
        // Reduce side: more size when far from the target.
        if excess > band * 0.5 { ask_sz = ask_sz.max(p.quote_size * 1.5); }
        if excess < -band * 0.5 { bid_sz = bid_sz.max(p.quote_size * 1.5); }
        match caution {
            Alert::Buy => { ask_sz *= p.caution_size_mult; ask_d += p.caution_widen; }
            Alert::Sell => { bid_sz *= p.caution_size_mult; bid_d += p.caution_widen; }
            Alert::None => {}
        }
        match alert {
            Alert::Buy => { ask_sz = 0.0; bid_d = bid_d.min(d_min + 1); }
            Alert::Sell => { bid_sz = 0.0; ask_d = ask_d.min(d_min + 1); }
            Alert::None => {}
        }
        if last {
            if pos > 0.0 { bid_sz = 0.0 } else { ask_sz = 0.0 }
        }
        bid_sz = bid_sz.min(max_pos - pos).max(0.0);
        ask_sz = ask_sz.min(max_pos + pos).max(0.0);

        let to_lots = |x: f64| ((x * lot) / 100.0).floor() as u64 * 100;
        let bl = to_lots(bid_sz);
        let al = to_lots(ask_sz);
        if bl >= 100 { d.bids.push(((o - bid_d).max(1) as u64, bl)); }
        if al >= 100 { d.asks.push(((o + ask_d) as u64, al)); }
        self.my_take_lots = takes.iter().map(|t| if t.0 { t.1 as i64 } else { -(t.1 as i64) }).sum();
        d.takes = takes;
        d.note = format!(
            "c={c} cb={:?} ca={:?} al={:?} ca={:?} z={:.1}({:.1}/{:.1}) tgt={:.1} bd={bid_d} ad={ask_d}",
            cb.map(|b| b - o), ca.map(|a| a - o), alert, caution, self.trend_z, self.z_price, self.z_flow, target
        );
        d
    }
}
