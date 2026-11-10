use mtx_kit::prelude::*;

fn p(i: usize, d: f64) -> f64 {
    std::fs::read_to_string("/nonexistent/mmparams")
        .ok()
        .and_then(|s| s.split_whitespace().nth(i).and_then(|v| v.parse().ok()))
        .unwrap_or(d)
}

#[derive(Default)]
struct Mm {
    last_pos: f64,
    flow: f64, // decayed signed fill flow, + = we've been bought... (position up)
    last_oracle: f64,
    mom: f64,
}

impl Strategy for Mm {
    async fn step(&mut self, m: &Market, orders: &mut Orders<'_>) -> BotResult {
        let pos = m.position.size;
        let d = pos - self.last_pos;
        self.last_pos = pos;
        self.flow = self.flow * 0.8 + d;
        if self.last_oracle > 0.0 {
            let r = (m.oracle / self.last_oracle - 1.0) * 1e4;
            self.mom = self.mom * 0.7 + r;
        }
        self.last_oracle = m.oracle;

        let px = m.oracle;
        let bp = px * 1e-4;
        let (base, size, skewk, flowk) = (p(0, 13.0), p(1, 1.0), p(2, 0.8), p(3, 4.0));
        // inventory skew: long -> lower both quotes
        let skew = -pos * skewk;
        // adverse selection: after we got bought (flow>0 => price likely falling), widen bid
        let f = self.flow;
        let widen_bid = (f.max(0.0) * flowk).min(40.0);
        let widen_ask = ((-f).max(0.0) * flowk).min(40.0);
        // momentum: price rising -> raise quotes, widen the ask
        let mom_shift = self.mom.clamp(-30.0, 30.0) * 0.3;
        let bid = px + (skew + mom_shift - base - widen_bid) * bp;
        let ask = px + (skew + mom_shift + base + widen_ask) * bp;
        let bs = size.min(m.room_to_buy());
        let as_ = size.min(m.room_to_sell());
        orders.cancel_all();
        if m.drawdown_room() < 60.0 || m.last { return Ok(()); }
        if bs > 0.05 { orders.bid(bid, bs); }
        if as_ > 0.05 { orders.ask(ask, as_); }
        Ok(())
    }
}

fn main() -> BotResult {
    mtx_kit::run(Mm::default())
}
