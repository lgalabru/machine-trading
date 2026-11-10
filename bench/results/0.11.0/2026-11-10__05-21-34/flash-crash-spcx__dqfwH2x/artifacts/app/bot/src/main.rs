use mtx_kit::prelude::*;
use std::collections::VecDeque;

#[derive(Default)]
struct Fade {
    hist: VecDeque<f64>,
    pos: f64,
    last: u64,
    halted: bool,
}

fn p(name: &str, d: f64) -> f64 {
    d
}

impl Strategy for Fade {
    async fn step(&mut self, m: &Market, orders: &mut Orders<'_>) -> BotResult {
        let thr = p("THR", 0.003);
        let q = p("Q", 2.0) as u64;
        let w = p("W", 2.0) as usize;
        let size = p("SIZE", 70.0).min(m.cap);
        self.hist.push_back(m.mark);
        if self.hist.len() > 40 { self.hist.pop_front(); }
        let n = self.hist.len();
        if m.last { return Ok(()); }
        if m.drawdown > 0.6 * m.max_drawdown {
            self.halted = true;
        }
        if self.halted {
            if self.pos != 0.0 {
                orders.target_position(0.0);
                self.pos = 0.0;
            }
            return Ok(());
        }
        let r = self.hist[n.saturating_sub(1 + w)];
        let mv = m.mark / r - 1.0;
        if mv.abs() > thr && m.steps_left > 2 {
            let s = mv.signum();
            if s != self.pos.signum() {
                orders.target_position(s * size);
                self.pos = s;
            }
            self.last = m.k;
        } else if self.pos != 0.0 && (m.k - self.last >= q || m.steps_left <= 1) {
            orders.target_position(0.0);
            self.pos = 0.0;
        }
        Ok(())
    }
}

fn main() -> BotResult {
    mtx_kit::run(Fade::default())
}
