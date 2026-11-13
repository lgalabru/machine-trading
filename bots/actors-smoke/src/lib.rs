//! The library's smoke bot: for the first half of the session it sells a
//! little every step, a pattern a front-runner can see, then buys it back;
//! throughout, it answers every request for a quote one tick inside the
//! book, where a client takes it.

use mtx_kit::{log, prelude::*};

/// What it sells a step, and the market's tick.
const SLICE: f64 = 0.5;
const TICK: f64 = 0.01;

#[derive(Default)]
struct Smoke;

fn cents(usd: f64) -> f64 {
    (usd * 100.0).round() / 100.0
}

impl Strategy for Smoke {
    fn step(&mut self, market: &Market, orders: &mut Orders) -> BotResult {
        let half = market.k + market.steps_left < 2 * market.k;
        if !half {
            orders.sell(SLICE, cents(market.mark * 0.99));
        } else if market.position.size < -SLICE / 2.0 {
            orders.buy(SLICE.min(-market.position.size), cents(market.mark * 1.01));
        }
        let (bid, ask) = match (market.best_bid, market.best_ask) {
            (Some(bid), Some(ask)) if ask - bid > 1.5 * TICK => (bid + TICK, ask - TICK),
            _ => (market.mark - TICK, market.mark + TICK),
        };
        for rfq in &market.rfqs {
            orders
                .respond(&rfq.id, Some(cents(bid)), Some(cents(ask)))
                .at(&rfq.venue);
        }
        if !market.rfqs.is_empty() {
            log(&format!(
                "step {}: answered {} requests",
                market.k,
                market.rfqs.len()
            ));
        }
        Ok(())
    }
}

mtx_kit::export!(Smoke);
