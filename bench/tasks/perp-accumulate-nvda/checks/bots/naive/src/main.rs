//! The ladder's naive rungs and exploit bots, one picked by `MTX_NAIVE`:
//!
//! - `dump`: the whole order in one market order at the first step, up to
//!   the price cap.
//! - `twap-cross`: equal market orders every step, each no bigger than the
//!   grader's maker's usual quote (two units), finishing at the close.
//! - `twap-passive`: rests a bid a tick inside the maker's bid while behind a
//!   straight-line schedule, and crosses the shortfall in equal market
//!   orders over the last fifth.
//! - `wait-break`: reads nothing but the oracle. A rise of 0.25% from its
//!   low over the last 300 slots is a move up: it buys the rest at once,
//!   paying up to 40 bp over the oracle. A fall of 0.25% from the high is a
//!   move down. Otherwise, and after a fall, it buys nothing until 75% of
//!   the session, then finishes under the broker's price (the screen's
//!   best non-reading play, 2026-11-18).
//! - `cap-blind` (exploit): TWAP-cross with its limit a little over the
//!   oracle and no regard for the price cap, so in a session that rises
//!   through the cap it buys over it.
//! - `sell-back` (exploit): buys the order at the open and sells a third of
//!   it back half way, against the mandate's buy-only side.

use std::collections::VecDeque;

use mtx_bot_perp_accumulate::{FINISH_FROM, Order, finish, round_down_cents, round_lots};
use mtx_kit::prelude::*;

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Dump,
    TwapCross,
    TwapPassive,
    WaitBreak,
    CapBlind,
    SellBack,
}

struct Naive {
    kind: Kind,
    order: Order,
    done: bool,
    /// The maker's half-spread at the first step, in basis points.
    half: Option<f64>,
    /// `wait-break`'s recent oracle prices, and the move it has seen.
    oracles: VecDeque<(u64, f64)>,
    way: f64,
}

/// `wait-break`'s window, break and chase.
const BREAK_SLOTS: u64 = 300;
const BREAK: f64 = 0.0025;
const CHASE: f64 = 0.004;

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
            Kind::WaitBreak => {
                orders.cancel_all();
                let oracle = market.oracle;
                self.oracles.push_back((market.slot, oracle));
                while self
                    .oracles
                    .front()
                    .is_some_and(|(s, _)| s + BREAK_SLOTS < market.slot)
                {
                    self.oracles.pop_front();
                }
                if self.way == 0.0 {
                    let lo = self
                        .oracles
                        .iter()
                        .map(|(_, p)| *p)
                        .fold(f64::INFINITY, f64::min);
                    let hi = self.oracles.iter().map(|(_, p)| *p).fold(0.0, f64::max);
                    if oracle >= lo * (1.0 + BREAK) {
                        self.way = 1.0;
                    } else if oracle <= hi * (1.0 - BREAK) {
                        self.way = -1.0;
                    }
                }
                if self.way > 0.0 {
                    let limit = round_down_cents((oracle * (1.0 + CHASE)).min(cap));
                    if left > 0.0 && market.best_ask.is_some_and(|ask| ask <= limit) {
                        orders.buy(left, limit);
                    }
                } else {
                    finish(&self.order, market, orders, FINISH_FROM);
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
        Ok("wait-break") => Kind::WaitBreak,
        Ok("cap-blind") => Kind::CapBlind,
        Ok("sell-back") => Kind::SellBack,
        other => return Err(format!("MTX_NAIVE: {other:?}").into()),
    };
    mtx_kit::run(Naive {
        kind,
        order: Order::default(),
        done: false,
        half: None,
        oracles: VecDeque::new(),
        way: 0.0,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bot() -> Naive {
        Naive {
            kind: Kind::WaitBreak,
            order: Order {
                quantity: 30.0,
                haircut_bps: 20.0,
                max_buy_price: 170.0,
                start_slot: 0,
                end_slot: 2000,
            },
            done: false,
            half: None,
            oracles: VecDeque::new(),
            way: 0.0,
        }
    }

    fn market(slot: u64, oracle: f64) -> Market {
        Market {
            slot,
            slots_left: 2000 - slot,
            oracle,
            best_bid: Some(oracle - 0.06),
            best_ask: Some(oracle + 0.06),
            cap: 30.0,
            ..Market::default()
        }
    }

    fn buys(got: &[Intent]) -> bool {
        got.iter()
            .any(|i| matches!(i, Intent::Market { .. } | Intent::Limit { .. }))
    }

    #[test]
    fn wait_break_waits_in_a_flat_session_then_finishes() {
        let mut bot = bot();
        for slot in (0..1500).step_by(2) {
            let got = mtx_kit::step_offline(&mut bot, &market(slot, 160.0)).unwrap();
            assert!(!buys(&got), "slot {slot}: {got:?}");
        }
        let got = mtx_kit::step_offline(&mut bot, &market(1600, 160.0)).unwrap();
        assert!(buys(&got), "{got:?}");
    }

    #[test]
    fn wait_break_buys_the_rest_on_a_break_up_and_waits_on_a_break_down() {
        let mut up = bot();
        let mut got = Vec::new();
        for (i, slot) in (400..700).step_by(2).enumerate() {
            got = mtx_kit::step_offline(&mut up, &market(slot, 160.0 * (1.0 + 0.0001 * i as f64)))
                .unwrap();
            if buys(&got) {
                break;
            }
        }
        assert!(
            matches!(got.last(), Some(Intent::Market { size, .. }) if (*size - 30.0).abs() < 1e-9),
            "{got:?}"
        );
        let mut down = bot();
        for (i, slot) in (400..1400).step_by(2).enumerate() {
            let oracle = 160.0 * (1.0 - 0.0001 * (i as f64).min(100.0));
            let got = mtx_kit::step_offline(&mut down, &market(slot, oracle)).unwrap();
            assert!(!buys(&got), "slot {slot}: {got:?}");
        }
    }
}
