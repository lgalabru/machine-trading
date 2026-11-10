use mtx_kit::prelude::*;

fn env(name: &str, d: f64) -> f64 {
    if let Ok(t) = std::fs::read_to_string("/tmp/botparams") {
        for l in t.lines() {
            if let Some((k, v)) = l.split_once('=') {
                if k == name {
                    if let Ok(x) = v.parse() { return x; }
                }
            }
        }
    }
    d
}

struct Maker {
    last_pos: f64,
    flow: f64, // signed EWMA of fills (positive = we bought)
    base_bps: f64,
    skew_bps: f64,
    widen_bps: f64,
    size: f64,
    decay: f64,
}

impl Default for Maker {
    fn default() -> Self {
        Maker {
            last_pos: 0.0,
            flow: 0.0,
            base_bps: env("B_BASE", 16.0),
            skew_bps: env("B_SKEW", 25.0),
            widen_bps: env("B_WIDEN", 4.0),
            size: env("B_SIZE", 1.0),
            decay: env("B_DECAY", 0.8),
        }
    }
}

impl Strategy for Maker {
    async fn step(&mut self, m: &Market, orders: &mut Orders<'_>) -> BotResult {
        let pos = m.position.size;
        self.flow = self.flow * self.decay + (pos - self.last_pos);
        self.last_pos = pos;
        if m.last || m.oracle <= 0.0 {
            orders.cancel_all();
            return Ok(());
        }
        let o = m.oracle;
        let bp = o * 1e-4;
        let inv = pos / m.cap;
        // Fills one-sided: buys mean informed sellers may be hitting us -> lean away.
        let f = self.flow;
        let mut half = self.base_bps + self.widen_bps * f.abs();
        half = half.min(60.0);
        let center = -self.skew_bps * inv * 1.0 - 2.0 * f.clamp(-3.0, 3.0);
        let mut bid = o + (center - half) * bp;
        let mut ask = o + (center + half) * bp;
        let mut size = self.size;
        // dd guard: stop quoting if close to drawdown limit
        if m.drawdown > 0.7 * m.max_drawdown {
            size = 0.0;
        }
        // near cap, only quote reducing side
        let (mut bs, mut asz) = (size.min(m.room_to_buy()), size.min(m.room_to_sell()));
        if inv > 0.7 { bs = 0.0; }
        if inv < -0.7 { asz = 0.0; }
        bid = (bid * 100.0).floor() / 100.0;
        ask = (ask * 100.0).ceil() / 100.0;
        orders.cancel_all();
        if bs > 0.0 { orders.bid(bid, bs); }
        if asz > 0.0 { orders.ask(ask, asz); }
        Ok(())
    }
}

fn main() -> BotResult {
    mtx_kit::run(Maker::default())
}
