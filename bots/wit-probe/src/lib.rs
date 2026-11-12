//! A probe of `mtx:strategy@0.2.0`: it logs the session and every market it
//! is handed, puts one line of each kind the interface allows on its first
//! sheet, and cancels what rests two steps later.
//!
//! The grader's tests call it with a market they made and check what comes
//! back; graded in a session (`just wit-probe`), its log shows what became
//! of each line on the chain.

use mtx_kit::{log, prelude::*};

#[derive(Default)]
struct Probe;

/// The probe's first sheet, for a market: what the grader's tests expect
/// back.
pub fn first_sheet(market: &Market, orders: &mut Orders) {
    let mark = market.mark;
    let below = market.best_bid.unwrap_or(mark) - mark * 0.05;
    let crossing = market.best_bid.unwrap_or(mark * 0.99);
    orders
        .cancel_all()
        .tx(TxOptions {
            cu_limit: Some(400_000),
            cu_price: Some(1_000),
            tip: Some(10_000),
            bundle: Some(1),
        })
        // Bought now or never: a market order bounded by the price.
        .bid(round(mark * 1.02), 2.0)
        .ioc()
        // Can't sell more than the 2 just bought.
        .sell(5.0, round(mark * 0.98))
        .reduce_only()
        // Rests, far below the book.
        .bid(round(below), 1.0)
        .post_only()
        // Would take the best bid: refused.
        .ask(round(crossing), 1.0)
        .post_only()
        // No tape in a perp session: no volume to take a share of.
        .target_position(3.0)
        .participation(0.1)
        // Another market, which the session doesn't trade.
        .buy(1.0, round(mark * 1.02))
        .at("openbook:spot:SOL-USDC")
        // No requests for quotes in this session.
        .respond("rfq-1", round(mark));
}

fn round(usd: f64) -> f64 {
    (usd * 100.0).round() / 100.0
}

impl Strategy for Probe {
    fn start(&mut self, session: &Session) -> BotResult {
        log(&format!("session {session:?}"));
        Ok(())
    }

    fn step(&mut self, market: &Market, orders: &mut Orders) -> BotResult {
        log(&format!("market {market:?}"));
        match market.k {
            0 => first_sheet(market, orders),
            2 => {
                orders.cancel_all();
            }
            _ => {}
        }
        Ok(())
    }

    fn end(&mut self, last: Option<&Market>) {
        log(&format!("end {}", last.map_or(0, |m| m.k)));
    }
}

mtx_kit::export!(Probe);
