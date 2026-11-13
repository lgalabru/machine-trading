//! Oracle-anchored Phoenix market maker with public-flow adverse-selection control.
//!
//! Only completed blocks and this account's state are read. All trading goes
//! through the order sheet. A directional burst temporarily suspends passive
//! quotes; ordinary inventory is acquired and unwound with skewed two-sided quotes.

mod flow;
mod model;
use mtx_kit::{
    log,
    prelude::*,
    rpc_account,
    venue::{PhoenixVenue, phoenix_rise::accounts::trader::Trader},
};

#[derive(Default)]
struct Maker {
    tape: flow::Flow,
    earliest_jump: u64,
    start_slot: u64,
    halfspread: f64,
    jump_count: f64,
    changes: f64,
    sum: f64,
    sumsq: f64,
    jumped: bool,
    own_trade_pending: bool,
    tick: f64,
    lot_scale: f64,
    asset_id: u64,
    account: String,
    last_price: f64,
    last_pos: f64,
    bid: f64,
    ask: f64,
    flow: f64,
    drift: f64,
    direction: f64,
    until: u64,
    cooldown: u64,
    peak: f64,
    halted: bool,
}
impl Maker {
    // Phoenix can defer maker-fill settlement. Reconcile the stored trader
    // position with the current position using the prices we left resting.
    fn equity(&self, p: f64, current_pos: f64) -> Option<f64> {
        let data = rpc_account(&self.account).ok()??;
        let t = Trader::try_from_account_bytes(&data).ok()?;
        let mut eq = t.header().state().quote_lot_collateral.as_inner() as f64 / 1e6;
        let mut raw_pos = 0.0;
        for (id, pos) in t.positions() {
            if id == self.asset_id {
                raw_pos = pos.base_lot_position().as_inner() as f64 / self.lot_scale;
                eq += pos.base_lot_position().as_inner() as f64 / self.lot_scale * p
                    + pos.virtual_quote_lot_position().as_inner() as f64 / 1e6;
            }
        }
        let pending = current_pos - raw_pos;
        if pending > 0.0 && self.bid > 0.0 {
            eq += pending * (p - self.bid) - pending * self.bid * 0.00005;
        }
        if pending < 0.0 && self.ask > 0.0 {
            eq += -pending * (self.ask - p) + pending * self.ask * 0.00005;
        }
        Some(eq)
    }
    fn trade_to(&mut self, o: &mut Orders, pos: f64) {
        self.own_trade_pending = true;
        o.target_position_with(
            pos,
            Execution {
                max_per_step: 40.0,
                slice: 8.0,
                slippage_bps: 60.0,
                min_trade: 0.01,
            },
        );
    }
}
impl Strategy for Maker {
    fn start(&mut self, s: &Session) -> BotResult {
        let v = PhoenixVenue::from_json(&s.venue)?;
        let config = v.market(&s.symbol)?;
        self.tick = v.ticks_to_usd(config, 1);
        self.lot_scale = 10f64.powi(config.base_lots_decimals as i32);
        self.asset_id = config.asset_id as u64;
        self.account = v.trader_account(&s.authority.parse()?).to_string();
        self.last_price = s.start_oracle;
        self.tape.authority = s.authority.clone();
        self.tape.market = config.market_pubkey.clone();
        self.tape.cursor = s.start_slot;
        self.peak = 10000.0;
        self.earliest_jump = s.start_slot + 54;
        self.start_slot = s.start_slot;
        log(&format!(
            "start oracle={} tick={}",
            s.start_oracle, self.tick
        ));
        Ok(())
    }
    fn step(&mut self, m: &Market, o: &mut Orders) -> BotResult {
        let p = m.oracle;
        o.cancel_all();
        if !p.is_finite() || p <= 0.0 {
            return Ok(());
        }
        if m.k == 0 {
            self.halfspread =
                ((m.best_ask.unwrap_or(p * 1.002) - m.best_bid.unwrap_or(p * 0.998)) * 5000.0 / p)
                    .clamp(8.0, 30.0);
        }
        let (fast, slow, _volume) = self.tape.update(m.slot);
        let pos = m.position.size;
        let fill = pos - self.last_pos;
        self.flow = self.flow * 0.85 - if self.own_trade_pending { 0.0 } else { fill };
        self.own_trade_pending = false;
        let ret = (p / self.last_price - 1.0) * 10000.0;
        self.last_price = p;
        self.last_pos = pos;
        let eq = self.equity(p, pos).unwrap_or(10000.0 + m.pnl);
        self.peak = self.peak.max(eq);
        let dd = self.peak - eq;
        // Reserve $35 for closing costs and valuation differences, and size for
        // an adverse 4.5% move. The session family caps individual jumps at 4%.
        let risk_cap = ((m.max_drawdown - dd - 35.0) / (p * 0.045))
            .clamp(0.0, 19.99)
            .min(m.cap - 0.01);
        if dd > 120.0 {
            self.halted = true;
        }
        if self.halted {
            self.trade_to(o, 0.0);
            return Ok(());
        }
        if ret.abs() > 100.0 {
            self.jump_count += 1.0;
            self.jumped = true;
            self.earliest_jump = m.slot + 54;
            self.direction = 0.0;
            self.until = 0;
            self.cooldown = m.slot + 10;
            self.tape.events.clear();
            self.flow = 0.0;
        } else {
            self.drift = self.drift * 0.97 + ret * 0.03;
            if ret.abs() > 0.00001 {
                self.changes += 1.0;
                self.sum += ret;
                self.sumsq += ret * ret;
            }
        }
        let old_dir = self.direction;
        let mean = self.sum / (self.changes + 1.0);
        let volatility = ((self.sumsq + 100.0) / (self.changes + 4.0) - mean * mean)
            .max(4.0)
            .sqrt();
        // Feature order must match model.rs. Evaluate both orientations so
        // the decision remains symmetric between buying and selling.
        let mut x = [0.0; 20];
        for (i, w) in [2u64, 3, 5, 8, 12, 16].into_iter().enumerate() {
            for &(s, d, _) in &self.tape.events {
                if s + 2 * w >= m.slot {
                    x[2 * i + if d > 0.0 { 0 } else { 1 }] += 1.0;
                }
            }
        }
        x[12] = (self.tape.buys + 6.4) / (m.k as f64 + 21.0);
        x[13] = (self.tape.sells + 6.4) / (m.k as f64 + 21.0);
        x[14] = mean;
        x[15] = volatility;
        x[16] = (m.slot - self.start_slot).min(200) as f64;
        x[17] = m.slots_left.min(200) as f64;
        x[18] = (m.slot.saturating_sub(self.earliest_jump.saturating_sub(54))).min(200) as f64;
        x[19] = self.jump_count;
        let pup = model::probability(&x);
        for i in (0..14).step_by(2) {
            x.swap(i, i + 1);
        }
        x[14] = -x[14];
        let pdn = model::probability(&x);
        let signal = 275.0 * (pup - pdn);
        let b = pup;
        let a = pdn;
        if m.slot >= self.cooldown {
            if signal > 2.0 * self.halfspread + 40.0 {
                self.direction = 1.0;
                self.until = m.slot + 50;
            }
            if signal < -(2.0 * self.halfspread + 40.0) {
                self.direction = -1.0;
                self.until = m.slot + 50;
            }
        }
        if m.slot >= self.until {
            self.direction = 0.0;
        }
        let active_dir = if m.slot >= self.earliest_jump && m.slots_left > 60 {
            self.direction
        } else {
            0.0
        };
        // A mixture of zero/up/down drift hypotheses controls passive inventory.
        // It never initiates a market order solely to acquire trend exposure.
        let mut up = 0.0;
        let mut down = 0.0;
        let variance = ((self.sumsq + 100.0) / (self.changes + 4.0)
            - (self.sum / (self.changes + 1.0)).powi(2))
        .max(4.0);
        for mu in [0.6, 1.2, 1.8] {
            for probability in [0.55_f64, 0.585, 0.62] {
                let common = -0.5 * mu * mu * self.changes / variance;
                let lp = mu * self.sum / variance
                    + common
                    + 0.6
                        * (self.tape.buys * (probability / 0.5).ln()
                            + self.tape.sells * ((1.0 - probability) / 0.5).ln());
                let lm = -mu * self.sum / variance
                    + common
                    + 0.6
                        * (self.tape.sells * (probability / 0.5).ln()
                            + self.tape.buys * ((1.0 - probability) / 0.5).ln());
                up += lp.clamp(-50.0, 50.0).exp() / 9.0;
                down += lm.clamp(-50.0, 50.0).exp() / 9.0;
            }
        }
        let trend = if self.jumped {
            0.0
        } else {
            (up - down) / (6.0 + up + down)
        };
        let target = if active_dir != 0.0 {
            active_dir * risk_cap.min(19.0)
        } else {
            6.0 * trend
        };
        log(&format!(
            "STATE k={} slot={} p={:.4} pos={:.3} eq={:.3} dd={:.3} dir={} b={} a={} fast={} slow={}",
            m.k, m.slot, p, pos, eq, dd, self.direction, b, a, fast, slow
        ));
        if active_dir != 0.0 && (pos - target).abs() > 2.0
            || active_dir == 0.0 && (pos.abs() > 9.0 || old_dir != 0.0 && self.direction == 0.0)
        {
            self.trade_to(o, target);
            return Ok(());
        }
        // Do not supply the informed side, or churn the burst position back
        // through taker fees while the signal is active.
        if active_dir != 0.0 {
            return Ok(());
        }
        let skew = ((pos - target) * 2.5 - self.drift * 5.0 - self.flow * 0.8).clamp(-15.0, 15.0);
        let mut bid = m.best_bid.unwrap_or(p * 0.999) + self.tick;
        let mut ask = m.best_ask.unwrap_or(p * 1.001) - self.tick;
        if (bid - self.tick - self.bid).abs() < self.tick * 0.1 {
            bid = self.bid;
        }
        if (ask + self.tick - self.ask).abs() < self.tick * 0.1 {
            ask = self.ask;
        }
        bid = bid.min(p * (1.0 - (3.0 + skew).max(1.0) / 10000.0));
        ask = ask.max(p * (1.0 + (3.0 - skew).max(1.0) / 10000.0));
        self.bid = (bid / self.tick).floor() * self.tick;
        self.ask = (ask / self.tick).ceil() * self.tick;
        let lo = (target - 3.0).max(-risk_cap);
        let hi = (target + 3.0).min(risk_cap);
        let bs = (hi - pos).clamp(0.0, 1.5);
        let az = (pos - lo).clamp(0.0, 1.5);
        if bs > 0.001 {
            o.bid(self.bid, bs);
        }
        if az > 0.001 {
            o.ask(self.ask, az);
        }
        Ok(())
    }
}
mtx_kit::export!(Maker);
