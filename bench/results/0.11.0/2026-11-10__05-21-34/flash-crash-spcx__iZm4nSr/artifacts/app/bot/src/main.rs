//! Fade sudden shocks against a slow anchor; stop out if they extend.
use mtx_kit::prelude::*;

const ENTER: f64 = 0.006;
const EXIT_FRAC: f64 = 0.3;
const STOP_EXTRA: f64 = 0.015;
const MAX_HOLD: u64 = 120; // steps
const MOVE: f64 = 0.0012;
const SETTLE: u64 = 4;
const COOLDOWN: u64 = 20;

#[derive(Default)]
struct Fader {
    anchor: f64,
    engaged: bool,
    sign: f64,
    peak: f64,
    since: u64,
    cool_until: u64,
    prev: f64,
    last_move: u64,
}

impl Strategy for Fader {
    async fn step(&mut self, m: &Market, orders: &mut Orders<'_>) -> BotResult {
        if self.anchor == 0.0 {
            self.anchor = m.mark;
        }
        if self.prev == 0.0 || ((m.mark - self.prev) / self.prev).abs() > MOVE {
            self.last_move = m.k;
        }
        self.prev = m.mark;
        let dev = (m.mark - self.anchor) / self.anchor;
        let mut target = 0.0;
        if !self.engaged {
            if m.k >= self.cool_until && dev.abs() > ENTER && m.k >= self.last_move + SETTLE && m.drawdown_room() > 300.0 {
                self.engaged = true;
                self.sign = dev.signum();
                self.peak = dev.abs();
                self.since = m.k;
            } else {
                self.anchor += 0.03 * (m.mark - self.anchor);
            }
        }
        if self.engaged {
            let d = dev * self.sign; // signed along the shock
            if d > self.peak {
                self.peak = d;
            }
            let stop = d > self.peak.min(ENTER * 2.0).max(ENTER) + STOP_EXTRA && d > ENTER + STOP_EXTRA;
            let done = d < self.peak * EXIT_FRAC || d < 0.0015;
            let timeout = m.k - self.since > MAX_HOLD;
            let late = m.steps_left < 4;
            if stop || done || timeout || late || m.drawdown_room() < 250.0 {
                self.engaged = false;
                self.cool_until = m.k + COOLDOWN;
                if stop || timeout {
                    self.anchor = m.mark;
                }
                if done {
                    self.anchor += 0.5 * (m.mark - self.anchor);
                }
            } else {
                target = -self.sign * m.cap;
            }
        }
        orders.target_position(target);
        Ok(())
    }
}

fn main() -> BotResult {
    mtx_kit::run(Fader::default())
}
