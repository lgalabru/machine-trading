//! SPCX shock trading with causal price forecasts and an account-equity risk limit.
//!
//! The embedded regressions estimate the average future index displacement over
//! 2 and 8 seconds. Their inputs contain only prices observed up to this turn.
//! During a fresh move the short forecast follows continuation; once it settles,
//! the longer forecast can trade the likely retracement. Both directions use the
//! same policy. Additional action-value models filter weak exits that would
//! incur unnecessary switching costs. The models were fitted to 10,000
//! independently generated scenarios.

use mtx_kit::{
    log,
    prelude::*,
    rpc_account,
    venue::{self, PhoenixVenue, phoenix_rise::accounts::trader::Trader},
};
mod model;
mod q_model;
#[derive(Default)]
struct ShockBot {
    trader: String,
    decimals: f64,
    asset_id: u64,
    equity_peak: f64,
    start_mark: f64,
    initial: f64,
    last: f64,
    prices: Vec<f64>,
    orient: f64,
    move_age: u64,
    ages: [u64; 3],
    jumps: [f64; 3],
    anchor: f64,
    extreme: f64,
    direction: f64,
    progress: u64,
    born: u64,
    halted: bool,
    turnover: f64,
    previous_position: f64,
}
impl ShockBot {
    // Orient returns to the latest impulse so crashes and squeezes share a
    // feature space. Keep f32 inputs identical to the training representation.
    fn features(&mut self, m: &Market) -> [f32; 27] {
        let p = m.index;
        let r = p / self.last - 1.0;
        if r.abs() > 0.002 {
            self.orient = r.signum();
            self.move_age = 0;
        } else {
            self.move_age += 1;
        }
        for (j, t) in [0.001, 0.003, 0.01].iter().enumerate() {
            self.ages[j] += 1;
            if r.abs() > *t {
                self.ages[j] = 0;
                self.jumps[j] = r;
            }
        }
        if r.abs() > 0.002 {
            if r.signum() != self.direction || self.direction == 0.0 {
                self.anchor = self.last;
                self.direction = r.signum();
                self.born = m.k;
            }
            self.extreme = p;
            self.progress = m.k;
        } else if self.direction != 0.0 {
            if (p - self.extreme) * self.direction / self.extreme > 0.001 {
                self.extreme = p;
                self.progress = m.k;
            }
            if (self.extreme - p) * self.direction > (self.extreme - self.anchor).abs() * 0.7
                || m.k - self.born > 220
            {
                self.direction = 0.0;
            }
        }
        self.prices.push(p);
        let k = self.prices.len() - 1;
        let mut x = [0f32; 27];
        let mut i = 0;
        for w in [1, 2, 4, 8, 16, 32, 64, 128] {
            let prev = if k >= w {
                self.prices[k - w]
            } else {
                self.initial
            };
            x[i] = ((p / prev - 1.0) * 100.0 * self.orient) as f32;
            i += 1;
        }
        for w in [8, 32, 128] {
            let mut lo = p;
            let mut hi = p;
            for &v in &self.prices[k.saturating_sub(w)..=k] {
                lo = lo.min(v);
                hi = hi.max(v);
            }
            x[i] = ((p / hi - 1.0) * 100.0 * self.orient) as f32;
            i += 1;
            x[i] = ((p / lo - 1.0) * 100.0 * self.orient) as f32;
            i += 1;
        }
        for a in self.ages {
            x[i] = a.min(300) as f32 / 10.0;
            i += 1;
        }
        for a in self.jumps {
            x[i] = (a * 100.0 * self.orient) as f32;
            i += 1;
        }
        for a in [
            (p / self.start_mark - 1.0) * 100.0 * self.orient,
            (p / self.anchor - 1.0) * 100.0 * self.orient,
            (self.extreme / self.anchor - 1.0) * 100.0 * self.orient,
            self.direction * self.orient,
            (m.k - self.progress).min(300) as f64 / 10.0,
            (m.k - self.born).min(400) as f64 / 10.0,
            m.k.min(140) as f64 / 10.0,
        ] {
            x[i] = a as f32;
            i += 1;
        }
        self.last = p;
        x
    }
    // The SDK estimate omits execution costs. Read collateral and the SPCX
    // virtual quote balance to mark the actual account, fees included. A read
    // failure falls back to the SDK estimate plus a turnover cost allowance.
    fn drawdown(&mut self, m: &Market) -> f64 {
        self.turnover += (m.position.size - self.previous_position).abs() * m.mark;
        self.previous_position = m.position.size;
        let mut dd = m.drawdown + self.turnover * 0.0006;
        if let Ok(Some(data)) = rpc_account(&self.trader) {
            if let Ok(account) = Trader::try_from_account_bytes(&data) {
                let mut equity =
                    account.header.trader_state.quote_lot_collateral.as_inner() as f64 / 1e6;
                for (asset, pos) in account.positions() {
                    if asset == self.asset_id {
                        equity += pos.base_lot_position().as_inner() as f64 / self.decimals
                            * m.mark
                            + pos.virtual_quote_lot_position().as_inner() as f64 / 1e6;
                    }
                }
                self.equity_peak = self.equity_peak.max(equity);
                dd = self.equity_peak - equity;
            }
        }
        dd
    }
}
impl Strategy for ShockBot {
    fn start(&mut self, s: &Session) -> BotResult {
        let venue = PhoenixVenue::from_json(&s.venue)?;
        self.trader = venue.trader_account(&venue::key(&s.authority)?).to_string();
        let config = venue.market(&s.symbol)?;
        self.decimals = 10f64.powi(config.base_lots_decimals as i32);
        self.asset_id = config.asset_id as u64;
        self.equity_peak = 10_000.0;
        self.start_mark = s.start_mark;
        self.initial = s.start_oracle;
        self.last = self.initial;
        self.anchor = self.initial;
        self.extreme = self.initial;
        self.orient = 1.0;
        self.ages = [1000; 3];
        self.move_age = 1000;
        Ok(())
    }
    fn step(&mut self, m: &Market, orders: &mut Orders) -> BotResult {
        // A final-turn trade adds costs without earning a subsequent move.
        if m.last {
            return Ok(());
        }
        let dd = self.drawdown(m);
        if dd > m.max_drawdown * 0.75 {
            self.halted = true;
        }
        let x = self.features(m);
        let score = if self.move_age < 3 {
            model::predict_4(&x)
        } else {
            model::predict_16(&x)
        } * self.orient;
        let mut target = m.position.size;
        if score > 0.10 {
            target = m.cap;
        } else if score < -0.10 {
            target = -m.cap;
        } else if score * target < 0.0 {
            target = 0.0;
        }
        // A weak opposite forecast is not always worth paying to exit. The
        // action values include future trading opportunities; retain exposure
        // only when holding beats flattening by another two basis points.
        if target == 0.0 && m.position.size.abs() > 1.0 {
            let plus = q_model::predict_plus(&x);
            let minus = q_model::predict_minus(&x);
            let current = m.position.size / m.cap;
            let hold = if current * self.orient > 0.0 {
                plus - 0.09 * (self.orient - current).abs()
            } else {
                minus - 0.09 * (-self.orient - current).abs()
            };
            let flat = -0.09 * current.abs();
            if hold > flat + 0.02 {
                target = m.position.size;
            }
        }
        // Reserve $100 for execution and allow an adverse 4.5% one-turn move,
        // above the replay's roughly 3.8% price-update limit. Reduce exposure
        // continuously as the equity cushion shrinks. A 75% drawdown halts
        // trading permanently and repeatedly targets flat until filled.
        let risk_cap = ((m.max_drawdown - dd - 100.0) / (0.045 * m.mark)).clamp(0.0, m.cap);
        target = target.clamp(-risk_cap, risk_cap);
        if self.halted {
            target = 0.0;
        }
        orders.target_position(target);
        log(&format!(
            "{} index={:.2} pos={:.1} score={:.4} target={:.1} dd={:.2}",
            m.k, m.index, m.position.size, score, target, dd
        ));
        Ok(())
    }
}
mtx_kit::export!(ShockBot);

#[cfg(test)]
mod tests;
