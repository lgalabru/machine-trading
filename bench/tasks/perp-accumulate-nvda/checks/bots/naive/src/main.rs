//! The ladder's naive rungs and exploit bots, one picked by `MTX_NAIVE`:
//!
//! - `dump`: the whole order in one market order at the first step, up to
//!   the price cap.
//! - `twap-cross`: equal market orders every step, each no bigger than the
//!   grader's maker's usual quote (two units), finishing at the close.
//! - `twap-passive`: rests a bid a tick inside the maker's bid while behind a
//!   straight-line schedule, and crosses the shortfall in equal market
//!   orders over the last fifth.
//! - `cap-blind` (exploit): TWAP-cross with its limit a little over the
//!   oracle and no regard for the price cap, so in a session that rises
//!   through the cap it buys over it.
//! - `sell-back` (exploit): buys the order at the open and sells a third of
//!   it back half way, against the mandate's buy-only side.

use mtx_bot_perp_accumulate::{Order, round_down_cents, round_lots};
use mtx_kit::prelude::*;

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Dump,
    TwapCross,
    TwapPassive,
    CapBlind,
    SellBack,
}

struct Naive {
    kind: Kind,
    order: Order,
    done: bool,
    /// The maker's half-spread at the first step, in basis points.
    half: Option<f64>,
}

impl Strategy for Naive {
    fn start(&mut self, session: &Session) -> BotResult {
        self.order = Order::from_session(session)?;
        Ok(())
    }

    fn step(&mut self, market: &Market, orders: &mut Orders) -> BotResult {
        let held = market.position.size;
        let q = self.order.quantity;
        let left = round_lots((q - held).max(0.0));
        let cap = self.order.max_buy_price;
        let t = self.order.progress(market.slot);
        let steps = (market.steps_left.max(1)) as f64;
        match self.kind {
            Kind::Dump if !self.done => {
                orders.buy(left, round_down_cents(cap));
                self.done = true;
            }
            Kind::TwapCross if left > 0.0 => {
                let size = round_lots((left / steps).min(2.0)).max(0.001);
                orders.buy(size.min(left), round_down_cents(cap));
            }
            Kind::TwapPassive if left > 0.0 => {
                orders.cancel_all();
                if t > 0.8 {
                    let size = round_lots(left / steps).max(0.001);
                    orders.buy(size.min(left), round_down_cents(cap));
                } else if held < q * t / 0.8 {
                    // A tick inside the maker's bid as it first stood: its
                    // own resting bid may be the best one now.
                    let half = *self.half.get_or_insert_with(|| {
                        market
                            .best_ask
                            .map_or(5.0, |ask| (ask / market.oracle - 1.0) * 10_000.0)
                    });
                    let bid = market.oracle * (1.0 - half / 10_000.0);
                    let price = round_down_cents((bid + 0.01).min(cap));
                    if market.best_ask.is_none_or(|ask| price < ask) {
                        orders.bid(price, round_lots(1.0f64.min(left))).post_only();
                    }
                }
            }
            Kind::CapBlind if left > 0.0 => {
                let size = round_lots((left / steps).min(2.0)).max(0.001);
                orders.buy(size.min(left), round_down_cents(market.oracle * 1.003));
            }
            Kind::SellBack => {
                if !self.done && left > 0.0 {
                    orders.buy(left, round_down_cents(cap));
                    self.done = true;
                } else if t > 0.5 && held > q * 0.9 {
                    orders.sell(round_lots(q / 3.0), round_down_cents(market.oracle * 0.99));
                }
            }
            _ => {}
        }
        Ok(())
    }
}

fn main() -> BotResult {
    let kind = match std::env::var("MTX_NAIVE").as_deref() {
        Ok("dump") => Kind::Dump,
        Ok("twap-cross") => Kind::TwapCross,
        Ok("twap-passive") => Kind::TwapPassive,
        Ok("cap-blind") => Kind::CapBlind,
        Ok("sell-back") => Kind::SellBack,
        other => return Err(format!("MTX_NAIVE: {other:?}").into()),
    };
    mtx_kit::run(Naive {
        kind,
        order: Order::default(),
        done: false,
        half: None,
    })
}
