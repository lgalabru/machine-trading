use mtx_kit::prelude::*;

struct Mm {
    last_pos: f64,
    flow: f64, // decayed signed fills (positive = we bought)
}

impl Default for Mm {
    fn default() -> Self {
        Mm { last_pos: 0.0, flow: 0.0 }
    }
}

fn env(name: &str, d: f64) -> f64 {
    let f = std::fs::read_to_string("/tmp/mm_params").unwrap_or_default();
    for l in f.lines() {
        if let Some((k, v)) = l.split_once('=') {
            if k.trim() == name {
                return v.trim().parse().unwrap_or(d);
            }
        }
    }
    d
}

impl Strategy for Mm {
    async fn step(&mut self, m: &Market, orders: &mut Orders<'_>) -> BotResult {
        let half_bps = env("HALF", 13.0);
        let skew_bps = env("SKEW", 0.6);
        let widen = env("WIDEN", 4.0);
        let size = env("SIZE", 1.0);
        let decay = env("DECAY", 0.8);
        let softcap = env("SOFT", 12.0);

        let pos = m.position.size;
        self.flow = self.flow * decay + (pos - self.last_pos);
        self.last_pos = pos;
        if m.last {
            orders.cancel_all();
            return Ok(());
        }
        let o = m.oracle;
        let bp = o * 1e-4;
        // flow<0: we were sold into... (we sold => short) price likely rising
        let mid = o - pos * skew_bps * bp - self.flow * widen * 0.5 * bp;
        let mut hb = half_bps * bp;
        let mut ha = half_bps * bp;
        // widen the side that is being hit
        if self.flow < 0.0 { ha += -self.flow * widen * bp; }
        if self.flow > 0.0 { hb += self.flow * widen * bp; }
        let bid = mid - hb;
        let ask = mid + ha;
        orders.cancel_all();
        if pos < softcap { orders.bid(bid, size); }
        if pos > -softcap { orders.ask(ask, size); }
        Ok(())
    }
}

fn main() -> BotResult {
    mtx_kit::run(Mm::default())
}
