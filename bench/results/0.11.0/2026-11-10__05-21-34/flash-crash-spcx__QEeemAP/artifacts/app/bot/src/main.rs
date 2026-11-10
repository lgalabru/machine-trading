//! Fades sudden shocks: enter against a settled jump, exit on reversion, stop or timeout.

use mtx_kit::prelude::*;
use std::collections::VecDeque;

fn p(name: &str, d: f64) -> f64 {
    let f = std::fs::read_to_string("/tmp/botcfg").unwrap_or_default();
    for l in f.lines() {
        if let Some((k, v)) = l.split_once('=') {
            if k == name {
                if let Ok(x) = v.parse() {
                    return x;
                }
            }
        }
    }
    d
}

struct Fader {
    hist: VecDeque<f64>,
    anchor: f64,
    entry_dev: f64,
    entry_k: u64,
    mode: u8,
    last_d: f64,
    conf: f64,
    ride_dir: f64,
    ride_thr: f64,
    cool_until: u64,
    enter: f64,
    tp: f64,
    stop: f64,
    hold: u64,
    lookback: usize,
    size: f64,
}

impl Default for Fader {
    fn default() -> Self {
        Fader {
            hist: VecDeque::new(),
            anchor: 0.0,
            entry_dev: 0.0,
            entry_k: 0,
            mode: 0,
            last_d: 0.0,
            conf: p("CONF", 0.0),
            ride_dir: 0.0,
            ride_thr: p("RIDE", 0.015),
            cool_until: 0,
            enter: p("ENTER", 9.0),
            tp: p("TP", 0.4),
            stop: p("STOP", 0.02),
            hold: p("HOLD", 160.0) as u64,
            lookback: p("LOOK", 8.0) as usize,
            size: p("SIZE", 1.0),
        }
    }
}

impl Strategy for Fader {
    async fn step(&mut self, m: &Market, orders: &mut Orders<'_>) -> BotResult {
        self.hist.push_back(m.mark);
        while self.hist.len() > self.lookback + 3 {
            self.hist.pop_front();
        }
        let n = self.hist.len();
        let pos = m.position.size;
        if m.last || m.steps_left <= 2 {
            orders.target_position(0.0);
            return Ok(());
        }
        let prev = if n >= 2 { self.hist[n - 2] } else { m.mark };
        let d = (m.mark - prev) / prev;
        let moving = d.abs() > self.ride_thr;
        let cap = m.cap * self.size;
        let dd_ok = m.drawdown_room() > 300.0;
        let ld = self.last_d;
        self.last_d = d;
        let _ = ld;
        match self.mode {
            0 => {
                if n < self.lookback + 3 {
                    return Ok(());
                }
                let conf = self.conf < 0.5 || (ld.signum() == d.signum() && ld.abs() > self.ride_thr);
                if moving && conf && dd_ok && m.k >= self.cool_until {
                    self.mode = 1;
                    self.anchor = prev;
                    self.ride_dir = d.signum();
                    orders.target_position(self.ride_dir * cap);
                    return Ok(());
                }
                self.anchor = self.hist[0];
                let dev = (m.mark - self.anchor) / self.anchor;
                if m.k >= self.cool_until && dev.abs() > self.enter && d.abs() < 0.0015 && dd_ok {
                    self.mode = 2;
                    self.entry_dev = dev;
                    self.entry_k = m.k;
                    orders.target_position(-dev.signum() * cap);
                }
            }
            1 => {
                if moving && d.signum() == self.ride_dir && dd_ok {
                    orders.target_position(self.ride_dir * cap);
                } else {
                    let dev = (m.mark - self.anchor) / self.anchor;
                    if !moving && dev.abs() > self.enter && dd_ok {
                        self.mode = 2;
                        self.entry_dev = dev;
                        self.entry_k = m.k;
                        orders.target_position(-dev.signum() * cap);
                    } else {
                        self.mode = 0;
                        self.cool_until = m.k + 6;
                        orders.target_position(0.0);
                    }
                }
            }
            _ => {
                let dev = (m.mark - self.anchor) / self.anchor;
                let reverted = dev.abs() < self.entry_dev.abs() * self.tp;
                let adverse = (dev - self.entry_dev) * self.entry_dev.signum() > self.stop;
                let timeout = m.k - self.entry_k > self.hold;
                if reverted || adverse || timeout || !dd_ok {
                    orders.target_position(0.0);
                    self.mode = 0;
                    self.cool_until = m.k + 10;
                } else {
                    orders.target_position(-self.entry_dev.signum() * cap);
                }
            }
        }
        let _ = pos;
        Ok(())
    }
}

fn main() -> BotResult {
    mtx_kit::run(Fader::default())
}
