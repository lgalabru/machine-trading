//! The RFQ desk check's bot: it answers every request for a quote one tick
//! either side of the mark, where clients take some of them, and never
//! trades the perp. Its account changes only by its desk's fills and the
//! desk's asset moving with the index.

use mtx_kit::prelude::*;

const TICK: f64 = 0.01;

#[derive(Default)]
struct Desk;

fn cents(usd: f64) -> f64 {
    (usd * 100.0).round() / 100.0
}

impl Strategy for Desk {
    fn step(&mut self, market: &Market, orders: &mut Orders) -> BotResult {
        let (bid, ask) = (cents(market.mark - TICK), cents(market.mark + TICK));
        for rfq in &market.rfqs {
            orders.respond(&rfq.id, Some(bid), Some(ask)).at(&rfq.venue);
        }
        Ok(())
    }
}

mtx_kit::export!(Desk);
