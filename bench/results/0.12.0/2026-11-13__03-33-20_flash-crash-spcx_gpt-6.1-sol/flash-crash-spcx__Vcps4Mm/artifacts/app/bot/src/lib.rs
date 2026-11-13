//! SPCX shock trading from a conditional one-second oracle-return forecast.
//!
//! The compiled ensemble recognizes continuing impulses and reversals. A decision
//! band limits turnover. Fee-inclusive account equity controls exposure separately
//! from the forecast. Execution uses only the session's own SPCX market.

use mtx_kit::{
    log,
    prelude::*,
    venue::{PhoenixVenue, key, phoenix_rise::accounts::owned::Trader},
};

fn uint(b: &[u8], at: usize) -> usize {
    u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]]) as usize
}

fn number(b: &[u8], at: usize) -> f64 {
    f64::from_le_bytes([
        b[at], b[at + 1], b[at + 2], b[at + 3],
        b[at + 4], b[at + 5], b[at + 6], b[at + 7],
    ])
}

// Embedded at build time: no model files are read at runtime.
// Layout: two u32 counts, u32 roots, then 20-byte nodes containing an i32
// feature, f64 value, i32 left child and i32 right child, all little endian.
// A feature of -1 denotes a leaf. Tree values already include learning rates.
fn predict(b: &[u8], x: &[f64; 37]) -> f64 {
    let nt = uint(b, 0);
    let off = 8 + 4 * nt;
    let mut y = 0.0;
    for t in 0..nt {
        let mut node = uint(b, 8 + t * 4);
        loop {
            let at = off + node * 20;
            let f = uint(b, at);
            let value = number(b, at + 4);
            if f == u32::MAX as usize {
                y += value;
                break;
            }
            node = uint(b, at + if x[f] <= value { 12 } else { 16 });
        }
    }
    y
}

#[derive(Default)]
struct ShockBot {
    trader: String,
    peak: f64,
    limit: f64,
    start: f64,
    history: Vec<f64>,
    previous: f64,
    d: f64,
    last: u64,
    last_big: u64,
    last_large: u64,
    dc: f64,
    db: f64,
    dl: f64,
    emas: [f64; 4],
    extreme: f64,
    base: f64,
    ed: f64,
    born: u64,
    ei: u64,
    anchor: f64,
    lo: f64,
    hi: f64,
    halted: bool,
}

impl Strategy for ShockBot {
    fn start(&mut self, s: &Session) -> BotResult {
        if let Ok(v) = PhoenixVenue::from_json(&s.venue) {
            if let Ok(k) = key(&s.authority) {
                self.trader = v.trader_account(&k).to_string();
            }
        }
        self.peak = 10000.0;
        self.limit = s.max_drawdown * 0.85;
        self.start = s.start_mark;
        self.previous = 1.0;
        self.d = 1.0;
        self.emas = [1.0; 4];
        self.extreme = 1.0;
        self.base = 1.0;
        self.anchor = 1.0;
        self.lo = 1.0;
        self.hi = 1.0;
        Ok(())
    }

    fn step(&mut self, m: &Market, o: &mut Orders) -> BotResult {
        let p = m.oracle / self.start;
        let k = m.k;
        let ch = p - self.previous;

        // The SDK PnL estimate omits fees and slippage. Read the actual cash
        // and virtual quote balances, then value the position at the mark.
        let mut dd = m.drawdown;
        if let Ok(Some(data)) = mtx_kit::rpc_account(&self.trader) {
            if let Ok(a) = Trader::try_from_account_bytes(&data) {
                let cash = a.state.quote_lot_collateral.as_i128() as f64 / 1e6;
                let vquote: f64 = a.position_entries()
                    .map(|x| x.position.virtual_quote_lot_position.as_i128() as f64 / 1e6)
                    .sum();
                let equity = cash + vquote + m.position.size * m.mark;
                self.peak = self.peak.max(equity);
                dd = self.peak - equity;
            }
        }
        if dd > self.limit {
            self.halted = true;
        }

        if ch.abs() > 0.000001 {
            self.last = k;
            self.dc = ch;
        }
        if ch.abs() > 0.0025 {
            self.d = ch.signum();
            self.last_big = k;
            self.db = ch;
        }
        if ch.abs() > 0.009 {
            self.last_large = k;
            self.dl = ch;
        }

        // Track the current excursion and its retracement independently of
        // the smaller price changes used by the impulse features.
        if self.ed == 0.0 {
            let dev = p - self.anchor;
            if dev.abs() > 0.004 {
                self.ed = dev.signum();
                self.base = self.anchor;
                self.extreme = p;
                self.born = k;
                self.ei = k;
            } else {
                self.anchor += (p - self.anchor) * 0.08;
            }
        }
        if self.ed != 0.0 {
            if self.ed * (p - self.extreme) > 0.0 {
                self.extreme = p;
            }
            if self.ed * ch > 0.0022 {
                self.ei = k;
            }
            let amp = self.ed * (self.extreme - self.base);
            let rt = self.ed * (self.extreme - p);
            if rt > amp * 0.7 || k - self.born > 220 {
                self.ed = 0.0;
                self.anchor = p;
            }
        }
        self.lo = self.lo.min(p);
        self.hi = self.hi.max(p);

        // Canonicalize by the latest significant impulse, sharing one model
        // between crashes and squeezes. The following feature order is fixed.
        let d = self.d;
        let mut x = [0.0; 37];
        let first = [
            k as f64 / 2.0,
            (m.steps_left + 1) as f64 / 2.0,
            d * (p - 1.0),
            d * ch,
            (k - self.last) as f64 / 2.0,
            d * self.dc,
            (k - self.last_big) as f64 / 2.0,
            d * self.db,
            (k - self.last_large) as f64 / 2.0,
            d * self.dl,
        ];
        x[..10].copy_from_slice(&first);
        for (i, lag) in [1, 2, 3, 4, 6, 10, 20, 40, 80, 120].iter().enumerate() {
            let old = if self.history.len() >= *lag {
                self.history[self.history.len() - lag]
            } else {
                1.0
            };
            x[10 + i] = d * (p - old);
        }
        for (i, alpha) in [0.5, 0.15, 0.05, 0.015].iter().enumerate() {
            x[20 + i] = d * (p - self.emas[i]);
            self.emas[i] += (p - self.emas[i]) * alpha;
        }
        let tail = [
            d * (p - self.lo),
            d * (p - self.hi),
            d * self.ed,
            (k - self.born) as f64 / 2.0,
            (k - self.ei) as f64 / 2.0,
            self.ed * (self.extreme - self.base),
            self.ed * (self.extreme - p),
            d * (p - self.anchor),
            d * (p - self.base),
            d * (p - self.extreme),
            self.hi - self.lo,
            if k < 100 { 1.0 } else { 0.0 },
            self.db.abs(),
        ];
        x[24..].copy_from_slice(&tail);
        // Preserve the training representation at split boundaries.
        for value in &mut x {
            *value = (*value as f32) as f64;
        }
        let signal = predict(include_bytes!("policy.bin"), &x) * d;
        let mut target = m.position.size;
        if signal > 0.0004 {
            target = 70.0;
        } else if signal < -0.0004 {
            target = -70.0;
        }

        // Retain 15% of the loss limit and size for another 6% adverse step.
        // Exposure shrinks before the hard halt; equity includes trading costs.
        let allowed = ((self.limit - dd).max(0.0) / (0.06 * m.mark))
            .min(m.cap)
            .floor();
        target = target.clamp(-allowed, allowed);
        if self.halted {
            target = 0.0;
        }
        o.target_position(target);
        log(&format!(
            "{} p {:.5} pos {:.1} target {:.1} signal {:.7} dd {:.2}",
            k, p, m.position.size, target, signal, dd
        ));
        self.history.push(p);
        self.previous = p;
        Ok(())
    }
}

mtx_kit::export!(ShockBot);
