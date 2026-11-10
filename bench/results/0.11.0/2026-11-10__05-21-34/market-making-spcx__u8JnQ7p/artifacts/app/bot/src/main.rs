//! Market maker: quotes around the oracle, skewed by inventory and recent one-sided fills.

use mtx_kit::prelude::*;

fn p(name: &str, d: f64) -> f64 {
    std::env::var(name).ok().and_then(|v| v.parse().ok()).unwrap_or(d)
}

struct Maker {
    half_bp: f64,
    skew_bp: f64,
    flow_bp: f64,
    size: f64,
    soft: f64,
    last_pos: f64,
    flow: f64,
    last_oracle: f64,
}

impl Default for Maker {
    fn default() -> Self {
        Maker {
            half_bp: p("B_HALF", 10.0),
            skew_bp: p("B_SKEW", 25.0),
            flow_bp: p("B_FLOW", 10.0),
            size: p("B_SIZE", 0.7),
            soft: p("B_SOFT", 2.5),
            last_pos: 0.0,
            flow: 0.0,
            last_oracle: 0.0,
        }
    }
}

impl Strategy for Maker {
    async fn step(&mut self, m: &Market, orders: &mut Orders<'_>) -> BotResult {
        let pos = m.position.size;
        let o = m.oracle;
        // signed fills this step decay into a flow estimate
        self.flow = self.flow * 0.8 + (pos - self.last_pos);
        self.last_pos = pos;
        let jump = if self.last_oracle > 0.0 { ((o / self.last_oracle) - 1.0).abs() } else { 0.0 };
        self.last_oracle = o;
        if m.last {
            orders.cancel_all();
            return Ok(());
        }
        let bp = o * 1e-4;
        let inv = pos / self.soft;
        let mut center = -self.skew_bp * inv * bp - self.flow_bp * self.flow.clamp(-3.0, 3.0) * bp;
        let mut bid_h = self.half_bp * bp;
        let mut ask_h = self.half_bp * bp;
        // widen the side being hit
        let hit = (self.flow.abs() * 3.0).min(12.0) * bp;
        if self.flow > 0.0 { ask_h += 0.0; bid_h += hit } else { ask_h += hit }
        if jump > 0.004 {
            bid_h += 10.0 * bp;
            ask_h += 10.0 * bp;
        }
        let stress = m.drawdown > 0.5 * m.max_drawdown;
        if stress {
            center -= 0.0;
            bid_h += 15.0 * bp;
            ask_h += 15.0 * bp;
        }
        let mut bs = self.size;
        let mut asz = self.size;
        if pos >= self.soft { bs = 0.0 }
        if pos <= -self.soft { asz = 0.0 }
        let bid = o + center - bid_h;
        let ask = o + center + ask_h;
        orders.cancel_all();
        if bs > 0.0 { orders.bid(bid, bs); }
        if asz > 0.0 { orders.ask(ask, asz); }
        Ok(())
    }
}

fn main() -> BotResult {
    mtx_kit::run(Maker::default())
}
