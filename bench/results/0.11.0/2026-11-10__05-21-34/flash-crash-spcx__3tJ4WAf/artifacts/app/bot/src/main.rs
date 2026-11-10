use mtx_kit::prelude::*;

fn p(name: &str, d: f64) -> f64 {
    std::fs::read_to_string("/tmp/params.txt")
        .ok()
        .and_then(|t| {
            t.split_whitespace()
                .find_map(|kv| kv.strip_prefix(&format!("{name}=")).and_then(|v| v.parse().ok()))
        })
        .unwrap_or(d)
}

struct Fader {
    hist: Vec<f64>,
    anchor: f64,
    ext: f64,
    entry_k: u64,
    dir: f64,
    cool: u64,
    thr: f64,
    win: usize,
    size: f64,
    tp: f64,
    stop: f64,
    hold: u64,
}

impl Default for Fader {
    fn default() -> Self {
        Fader {
            hist: vec![],
            anchor: 0.0,
            ext: 0.0,
            entry_k: 0,
            dir: 0.0,
            cool: 0,
            thr: p("THR", 0.008),
            win: p("WIN", 8.0) as usize,
            size: p("SIZE", 40.0),
            tp: p("TP", 1.0),
            stop: p("STOP", 0.02),
            hold: p("HOLD", 60.0) as u64,
        }
    }
}

impl Strategy for Fader {
    async fn step(&mut self, m: &Market, orders: &mut Orders<'_>) -> BotResult {
        self.hist.push(m.mark);
        let n = self.hist.len();
        let pos = m.position.size;
        if m.last {
            orders.target_position(0.0);
            return Ok(());
        }
        if pos.abs() < 0.5 {
            if self.cool > 0 {
                self.cool -= 1;
            }
            if n > self.win && self.cool == 0 {
                let base = self.hist[n - 1 - self.win];
                let dev = m.mark / base - 1.0;
                let last = m.mark / self.hist[n - 2] - 1.0;
                if dev.abs() > self.thr && last.abs() < 0.002 {
                    self.dir = -dev.signum();
                    self.anchor = base;
                    self.ext = m.mark;
                    self.entry_k = m.k;
                    orders.target_position(self.dir * self.size.min(m.cap));
                }
            }
        } else {
            let shock = self.ext - self.anchor;
            // profit target: retraced tp of shock
            let target = self.ext - self.tp * shock;
            let done_tp = if self.dir > 0.0 { m.mark >= target } else { m.mark <= target };
            let beyond = (m.mark - self.ext) * (-self.dir) / self.ext;
            let stopped = beyond > self.stop;
            let timeout = m.k - self.entry_k > self.hold;
            if done_tp || stopped || timeout {
                orders.target_position(0.0);
                self.cool = 6;
            } else {
                orders.target_position(self.dir * self.size.min(m.cap));
            }
        }
        Ok(())
    }
}

fn main() -> BotResult {
    mtx_kit::run(Fader::default())
}
