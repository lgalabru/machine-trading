use mtx_kit::prelude::*;

#[derive(Default)]
struct Mm {
    last_pos: f64,
    last_oracle: f64,
    // decaying signed fill pressure (+ = we've been bought from... i.e. we got short)
    flow: f64,
    pause: u64,
}

const HALF_BPS: f64 = 10.0;
const SIZE: f64 = 1.0;
const SKEW_BPS: f64 = 0.6; // per SPCX of inventory
const SOFT_CAP: f64 = 12.0;

impl Strategy for Mm {
    async fn step(&mut self, market: &Market, orders: &mut Orders<'_>) -> BotResult {
        let o = market.oracle;
        let pos = market.position.size;
        let dpos = pos - self.last_pos;
        self.last_pos = pos;
        self.flow = self.flow * 0.9 + dpos;
        let mv = if self.last_oracle > 0.0 { (o / self.last_oracle - 1.0).abs() } else { 0.0 };
        self.last_oracle = o;
        if mv > 0.004 || market.drawdown_room() < 80.0 {
            self.pause = 6;
        }
        if self.pause > 0 {
            self.pause -= 1;
            orders.cancel_all();
            return Ok(());
        }
        let bp = o * 1e-4;
        // one-sided flow => lean the price against it and widen that side
        let lean = -self.flow * 1.5 * bp; // flow>0 (we bought) => lower quotes
        let center = o + lean - pos * SKEW_BPS * bp;
        let mut bid_half = HALF_BPS * bp;
        let mut ask_half = HALF_BPS * bp;
        if self.flow > 1.5 { bid_half += 4.0 * bp; }
        if self.flow < -1.5 { ask_half += 4.0 * bp; }
        let bid_sz = if pos > SOFT_CAP { 0.0 } else { SIZE.min(market.room_to_buy()) };
        let ask_sz = if pos < -SOFT_CAP { 0.0 } else { SIZE.min(market.room_to_sell()) };
        orders.cancel_all();
        if bid_sz > 0.05 { orders.bid(center - bid_half, bid_sz); }
        if ask_sz > 0.05 { orders.ask(center + ask_half, ask_sz); }
        Ok(())
    }
}

fn main() -> BotResult {
    mtx_kit::run(Mm::default())
}
