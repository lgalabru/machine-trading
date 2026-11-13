//! Momentum during a shock's moving legs; measured fades during pauses.
mod models;
use mtx_kit::{log, prelude::*, rpc_account, venue::PhoenixVenue};
use mtx_kit::venue::phoenix_rise::accounts::trader::TraderHeader;

#[derive(Clone, Copy)]
struct Timing {
    trigger: f64, push: f64, retracement: f64,
    momentum_steps: u64, fade_steps: u64, expiry_steps: u64,
    reversal_steps: u64, renew: bool, reset_expiry: bool,
    large_fade_steps: u64, large_move: f64,
}
const TIMINGS: [Timing; 4] = [
    Timing { trigger: 0.0025, push: 0.002, retracement: 0.4, momentum_steps: 3, fade_steps: 4, expiry_steps: 300, reversal_steps: 5, renew: true, reset_expiry: true, large_fade_steps: 0, large_move: 0.02 },
    Timing { trigger: 0.003, push: 0.0025, retracement: 0.5, momentum_steps: 4, fade_steps: 6, expiry_steps: 80, reversal_steps: 3, renew: true, reset_expiry: true, large_fade_steps: 0, large_move: 0.015 },
    Timing { trigger: 0.002, push: 0.002, retracement: 0.3, momentum_steps: 5, fade_steps: 5, expiry_steps: 40, reversal_steps: 4, renew: false, reset_expiry: true, large_fade_steps: 0, large_move: 0.02 },
    Timing { trigger: 0.0025, push: 0.003, retracement: 0.5, momentum_steps: 3, fade_steps: 4, expiry_steps: 100, reversal_steps: 4, renew: true, reset_expiry: true, large_fade_steps: 12, large_move: 0.025 },
 ];

#[derive(Default)]
struct Signal {
    previous: f64, baseline: f64, extreme: f64, direction: f64,
    last_push: u64, started: u64, last_reversal: u64,
    active: bool, fading: bool, reverting: bool, exiting: bool,
}
impl Signal {
    fn step(&mut self, p: f64, k: u64, a: Timing) -> f64 {
        let change = (p - self.previous) / self.previous;
        if !self.active {
            if change.abs() >= a.trigger {
                self.active = true;
                self.fading = false;
                self.reverting = false;
                self.exiting = false;
                self.direction = change.signum();
                self.baseline = self.previous;
                self.extreme = p;
                self.last_push = k;
                self.started = k;
            }
        } else {
            if (p - self.extreme) * self.direction > p * a.push {
                if k - self.last_push >= a.fade_steps && change * self.direction > a.trigger && a.reset_expiry {
                    self.started = k;
                }
                self.reverting = false;
                self.extreme = p;
                self.last_push = k;
            }
            if change * self.direction < -a.trigger { self.last_reversal = k; }
            let amplitude = (self.extreme - self.baseline).abs();
            let retrace = (self.extreme - p) * self.direction;
            if retrace >= amplitude * a.retracement { self.exiting = true; }
            if (self.exiting && k - self.last_reversal >= a.reversal_steps) || k - self.started > a.expiry_steps {
                self.active = false;
                self.fading = false;
            }
            if retrace > (p * 0.003).max(amplitude * 0.2) { self.reverting = true; }
            if change * self.direction > a.trigger && a.renew && !self.reverting { self.last_push = k; }
        }
        let mut signal = 0.0;
        if self.active {
            let quiet = k - self.last_push;
            let wait = if (self.extreme-self.baseline).abs()/self.baseline >= a.large_move {
                a.fade_steps.max(a.large_fade_steps)
            } else { a.fade_steps };
            if quiet >= wait { self.fading = true; }
            if self.fading || self.exiting { signal = -self.direction; }
            if quiet < a.momentum_steps && !self.reverting && !self.exiting { signal = self.direction; }
            if change * self.direction > 0.032 && !self.reverting && !self.exiting { signal = self.direction; }
        }
        self.previous = p;
        signal
    }
}

#[derive(Default)]
struct ShockBot {
    trader: String,
    signals: [Signal; 4],
    equity_peak: f64,
    halted: bool,
    history: Vec<f64>,
    orientation: f64,
    impulse: f64,
    times: [u64;4],
    start_mark: f64,
    start_oracle: f64,
    #[cfg(test)]
    last_score: f64,
}
impl Strategy for ShockBot {
    fn start(&mut self, s: &Session) -> BotResult {
        self.equity_peak = 10000.0;
        self.orientation = 1.0;
        self.start_mark = s.start_mark;
        self.start_oracle = s.start_oracle;
        for signal in &mut self.signals {
            signal.previous = s.start_oracle;
            signal.baseline = s.start_oracle;
            signal.extreme = s.start_oracle;
        }
        if let Ok(v) = PhoenixVenue::from_json(&s.venue) {
            if let Ok(a) = s.authority.parse() { self.trader = v.trader_account(&a).to_string(); }
        }
        Ok(())
    }
    fn step(&mut self, m: &Market, o: &mut Orders) -> BotResult {
        let p = m.oracle;
        if !p.is_finite() || p <= 0.0 { return Ok(()); }
        let mut dd = m.drawdown;
        if let Ok(Some(data)) = rpc_account(&self.trader) {
            if let Ok(t) = TraderHeader::try_from_account_bytes(&data) {
                let equity = t.state().quote_lot_collateral.as_inner() as f64 / 1e6
                    + m.position.size * (m.mark - m.position.entry_usd);
                self.equity_peak = self.equity_peak.max(equity);
                dd = self.equity_peak - equity;
            }
        }
        if dd > 550.0 { self.halted = true; }
        let previous = self.history.last().copied().unwrap_or(self.start_oracle);
        let change = (p-previous)/previous;
        if change.abs() >= 0.002 {
            self.orientation = change.signum();
            self.impulse = change*100.0;
        }
        for (i,threshold) in [0.002,0.005,0.015,0.03].iter().enumerate() {
            if change.abs() >= *threshold { self.times[i] = m.k; }
        }
        let mut strength = 0.0;
        let mut primary = 0.0;
        for (i,(signal,timing)) in self.signals.iter_mut().zip(TIMINGS).enumerate() {
            let value = signal.step(p,m.k,timing);
            strength += value / TIMINGS.len() as f64;
            if i == 0 { primary = value; }
        }
        self.history.push(p);
        let k = self.history.len()-1;
        let direction = self.orientation;
        let mut features = Vec::with_capacity(37);
        features.push((p/self.start_mark-1.0)*direction*100.0);
        for lag in [1,2,4,8,16,32,64,128] {
            let prior = if k>=lag { self.history[k-lag] } else { self.start_oracle };
            features.push((p-prior)/self.start_mark*direction*100.0);
        }
        for window in [8,32,80,160] {
            let mut low = p;
            let mut high = p;
            for value in &self.history[(k+1).saturating_sub(window)..=k] {
                low = low.min(*value);
                high = high.max(*value);
            }
            features.push((if direction>0.0 {p-high} else {low-p}) /self.start_mark*100.0);
            features.push((if direction>0.0 {p-low} else {high-p}) /self.start_mark*100.0);
        }
        for time in self.times { features.push((m.k-time).min(400) as f64); }
        features.push(self.impulse*direction);
        features.push(m.k as f64*0.5);
        features.push(m.steps_left as f64*0.5);
        for window in [32,80] {
            let mut variation = 0.0;
            for z in (k+1).saturating_sub(window).max(1)..=k {
                variation += (self.history[z]-self.history[z-1]).abs();
            }
            features.push(variation/self.start_mark*100.0);
        }
        let signal = &self.signals[0];
        features.push(primary*direction);
        features.push(signal.direction*direction);
        features.push((signal.extreme-signal.baseline).abs()/self.start_mark*100.0);
        features.push(signal.direction*(signal.extreme-p)/(signal.extreme-signal.baseline).abs().max(0.01));
        features.push((m.k-signal.last_push).min(400) as f64);
        features.push((m.k-signal.started).min(400) as f64);
        features.push(u8::from(signal.active) as f64);
        features.push(u8::from(signal.reverting) as f64);
        features.push(u8::from(signal.exiting) as f64);
        features.push(u8::from(signal.fading) as f64);
        features.push(strength*direction);
        let mut x = [0.0;37];
        for (i,v) in features.iter().take(37).enumerate() { x[i] = *v as f32 as f64; }
        let p0 = models::predict(&x,models::MODEL_0,models::INTERCEPT_0);
        let p1 = models::predict(&x,models::MODEL_1,models::INTERCEPT_1);
        let p2 = models::predict(&x,models::MODEL_2,models::INTERCEPT_2);
        let score = (16.0*p0+16.0*p1+4.0*p2)/36.0*direction;
        #[cfg(test)]
        { self.last_score = score; }
        let strength = if score.abs() > 0.1 {
            score.signum()
        } else if score*m.position.size > 0.0 && score.abs()>0.005 {
            m.position.size.signum()
        } else { 0.0 };
        let risk_cap = ((850.0-dd).max(0.0) / (p * 0.055)).min(m.cap);
        let mut target = (strength * m.cap).clamp(-risk_cap, risk_cap);
        if self.halted { target = 0.0; }
        if (target-m.position.size).abs() >= 1.0 {
            log(&format!("k {} oracle {:.4} target {:.2} pos {:.2} dd {:.2}",m.k,p,target,m.position.size,dd));
            o.target_position_with(target, Execution { slippage_bps: 90.0, ..Execution::default() });
        }
        Ok(())
    }
}
mtx_kit::export!(ShockBot);

#[cfg(test)]
mod model_tests;

#[cfg(test)]
mod tests {
    use super::*;

    fn signal(price: f64) -> Signal {
        Signal { previous: price, baseline: price, extreme: price, ..Signal::default() }
    }

    #[test]
    fn follows_a_leg_then_fades_a_pause() {
        let mut s = signal(100.0);
        let a = TIMINGS[0];
        assert_eq!(s.step(101.0, 0, a), 1.0);
        assert_eq!(s.step(101.0, 1, a), 1.0);
        assert_eq!(s.step(101.0, 2, a), 1.0);
        assert_eq!(s.step(101.0, 3, a), 0.0);
        assert_eq!(s.step(101.0, 4, a), -1.0);
    }

    #[test]
    fn holds_through_rebound_chunks_then_exits() {
        let mut s = signal(100.0);
        let a = TIMINGS[0];
        s.step(104.0,0,a);
        s.step(104.0,4,a);
        assert_eq!(s.step(102.0,5,a),-1.0);
        assert_eq!(s.step(101.0,7,a),-1.0);
        assert_eq!(s.step(101.0,11,a),-1.0);
        assert_eq!(s.step(101.0,12,a),0.0);
    }

    #[test]
    fn a_new_leg_restarts_momentum() {
        let mut s = signal(100.0);
        let a = TIMINGS[0];
        s.step(101.0,0,a);
        assert_eq!(s.step(101.0,10,a),-1.0);
        assert_eq!(s.step(103.0,11,a),1.0);
        assert_eq!(s.started,11);
    }

    #[test]
    fn downward_shocks_use_the_same_rules() {
        let mut s = signal(100.0);
        let a = TIMINGS[0];
        assert_eq!(s.step(99.0,0,a),-1.0);
        assert_eq!(s.step(99.0,4,a),1.0);
        assert_eq!(s.step(99.6,5,a),1.0);
        assert_eq!(s.step(99.6,10,a),0.0);
    }
}

#[cfg(test)]
mod risk_tests {
    use super::*;

    fn initialized() -> ShockBot {
        ShockBot {
            start_mark: 143.11,
            start_oracle: 143.05,
            orientation: 1.0,
            equity_peak: 10000.0,
            signals: std::array::from_fn(|_| Signal {
                previous: 143.05, baseline: 143.05, extreme: 143.05,
                ..Signal::default()
            }),
            ..ShockBot::default()
        }
    }

    #[test]
    fn drawdown_halt_is_latched_and_closes_the_position() {
        let mut bot = initialized();
        let mut market = Market {
            oracle: 143.05, mark: 143.05, cap: 70.0, max_drawdown: 1000.0,
            drawdown: 600.0, position: Position { size: 20.0, entry_usd: 143.05 },
            ..Market::default()
        };
        let mut orders = Orders::new();
        bot.step(&market, &mut orders).unwrap();
        assert!(bot.halted);
        assert!(orders.lines().any(|(intent, _)| matches!(intent, Intent::Target {position, ..} if *position == 0.0)));
        market.k = 1;
        market.drawdown = 0.0;
        market.oracle = 150.0;
        market.mark = 150.0;
        market.position.size = 0.0;
        let mut orders = Orders::new();
        bot.step(&market, &mut orders).unwrap();
        assert!(bot.halted);
        assert!(orders.lines().all(|(intent, _)| matches!(intent, Intent::Target {position, ..} if *position == 0.0)));
    }

    #[test]
    fn targets_preserve_cap_and_adverse_step_headroom() {
        let mut bot = initialized();
        let mut state = 711u64;
        for k in 0..720 {
            state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
            let price = 75.0 + ((state >> 32) % 12500) as f64 / 100.0;
            let dd = ((state >> 16) % 550) as f64;
            let position = ((state >> 8) % 141) as f64 - 70.0;
            let market = Market {
                k, oracle: price, mark: price, cap: 70.0, max_drawdown: 1000.0,
                drawdown: dd, steps_left: 720-k,
                position: Position {size:position,entry_usd:price},
                ..Market::default()
            };
            let mut orders = Orders::new();
            bot.step(&market,&mut orders).unwrap();
            for (intent,options) in orders.lines() {
                assert!(options.venue.is_none());
                match intent {
                    Intent::Target {position,execution} => {
                        assert!(position.abs() <= 70.0);
                        assert!(execution.slice <= 10.0);
                        // A 4% adverse step plus $50 execution/mark allowance.
                        assert!(dd + position.abs()*price*0.04 + 50.0 < 1000.0);
                    },
                    _ => panic!("unexpected order type"),
                }
            }
        }
    }
}
