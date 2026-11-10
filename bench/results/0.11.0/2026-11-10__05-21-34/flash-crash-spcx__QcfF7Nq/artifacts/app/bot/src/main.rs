//! Fade sudden shocks: enter against a fast jump, take profit on a partial
//! retrace, stop out if the move extends, time out otherwise.

use mtx_kit::prelude::*;
use std::collections::VecDeque;

fn p(name: &str, d: f64) -> f64 {
    // tuning hook: optional /tmp/params file of NAME=value lines
    if let Ok(t) = std::fs::read_to_string("/tmp/params") {
        for l in t.lines() {
            if let Some((k, v)) = l.split_once('=') {
                if k.trim() == name {
                    if let Ok(x) = v.trim().parse() {
                        return x;
                    }
                }
            }
        }
    }
    d
}

struct Fade {
    hist: VecDeque<f64>,
    // active trade
    dir: f64,       // +1 = we are long (faded a drop), -1 short, 0 flat
    base: f64,      // pre-jump price
    extreme: f64,   // most extreme price of the shock so far
    entered: u64,
    cooldown: u64,
    win: usize,
    delay: usize,
    thr: f64,
    take: f64,
    stop_abs: f64,
    stop_rel: f64,
    max_hold: u64,
    size_frac: f64,
}

impl Default for Fade {
    fn default() -> Self {
        Fade {
            hist: VecDeque::new(),
            dir: 0.0,
            base: 0.0,
            extreme: 0.0,
            entered: 0,
            cooldown: 0,
            win: p("W", 6.0) as usize,
            delay: p("D", 6.0) as usize,
            thr: p("THR", 0.005),
            take: p("TAKE", 1.2),
            stop_abs: p("STOPA", 0.004),
            stop_rel: p("STOPR", 0.4),
            max_hold: p("HOLD", 40.0) as u64,
            size_frac: p("SIZE", 0.7),
        }
    }
}

impl Strategy for Fade {
    async fn step(&mut self, m: &Market, orders: &mut Orders<'_>) -> BotResult {
        let mark = m.mark;
        tracing::info!(k = m.k, mark, pos = m.position.size, dir = self.dir, "s");
        self.hist.push_back(mark);
        if self.hist.len() > 60 {
            self.hist.pop_front();
        }
        let n = self.hist.len();
        let size = m.cap * self.size_frac;

        if self.dir == 0.0 {
            if m.k < self.cooldown || n <= self.win + self.delay || m.steps_left < 8 {
                return Ok(());
            }
            let cur = self.hist[n - 1 - self.delay];
            let past = self.hist[n - 1 - self.delay - self.win];
            let r = (cur - past) / past;
            if r.abs() >= self.thr && (mark - cur).abs() <= 0.002 * mark {
                self.dir = if r < 0.0 { 1.0 } else { -1.0 };
                self.base = past;
                self.extreme = mark;
                self.entered = m.k;
                orders.target_position(self.dir * size);
            }
            return Ok(());
        }

        // in a trade: track extension of the shock
        if self.dir > 0.0 {
            self.extreme = self.extreme.min(mark);
        } else {
            self.extreme = self.extreme.max(mark);
        }
        let jump = (self.base - self.extreme).abs();
        // retrace progress from extreme toward base
        let retr = (mark - self.extreme).abs() * if (mark - self.extreme) * self.dir > 0.0 { 1.0 } else { -1.0 };
        let adverse = (self.extreme - mark) * self.dir * -1.0; // unused sign helper
        let _ = adverse;
        let stop_dist = (self.stop_abs * self.base).max(self.stop_rel * jump);
        // adverse beyond entry extreme: price moved further against us than the
        // extreme at which we last measured; extreme tracks it, so measure from
        // the entry-time mark instead via position pnl proxy
        let entry_mark = self.hist.get(n.saturating_sub((m.k - self.entered) as usize + 1)).copied().unwrap_or(mark);
        let loss = (entry_mark - mark) * self.dir;
        let done = retr >= self.take * jump
            || loss >= stop_dist
            || m.k - self.entered >= self.max_hold
            || m.steps_left < 3;
        if done {
            orders.target_position(0.0);
            // wait for flat before re-arming
            self.dir = 0.0;
            self.cooldown = m.k + 4;
            self.hist.clear();
        } else {
            orders.target_position(self.dir * size);
        }
        Ok(())
    }
}

fn main() -> BotResult {
    mtx_kit::run(Fade::default())
}
