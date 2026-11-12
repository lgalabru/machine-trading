//! A probe of `mtx:strategy@0.2.0`: it logs the session and every market it
//! is handed, and puts one line of each kind the interface allows on its
//! first sheet. At step 3 it rests a reduce-only ask, which Phoenix holds to
//! reduce-only, and asks for a swap the session can't execute before a buy
//! past what the cap allows without it; at step 4 it cancels everything and
//! goes flat.
//!
//! The grader's tests call it with a market they made and check what comes
//! back; graded in a session (`just wit-probe`), its log shows what became
//! of each line on the chain.

use mtx_kit::{
    AmountUnit, Intent, LineOptions, LiquidityShape, PriceRange, Route, log, prelude::*,
};

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
            route: Some(Route::Jito {
                bundle_only: true,
                dont_front: true,
            }),
            retry_until_slot: Some(market.slot + 10),
        })
        // Bought now or never: a market order bounded by the price.
        .bid(round(mark * 1.02), 2.0)
        .ioc()
        .client_id(1)
        // Reduce-only: the buy above may not fill, so it doesn't count.
        .sell(5.0, round(mark * 0.98))
        .reduce_only()
        // Rests, far below the book.
        .bid(round(below), 1.0)
        .post_only()
        .client_id(2)
        // Would take the best bid: not placed, and failed.
        .ask(round(crossing), 1.0)
        .post_only()
        // Reduce-only, resting, until a slot: cut to what the position
        // allows, whatever the sheet's buys do.
        .ask(round(mark * 1.1), 1.0)
        .reduce_only()
        .expires(market.slot + 100)
        // No tape in a perp session: no volume to take a share of.
        .target_position(3.0)
        .participation(0.1)
        // Another market, which the session doesn't trade.
        .buy(1.0, round(mark * 1.02))
        .at("openbook:spot:SOL-USDC")
        // No requests for quotes in this session.
        .respond("rfq-1", Some(round(mark)), None)
        .expires(market.slot + 5)
        // Not supported on a perp yet: each comes back not sent.
        .cancel(2)
        .swap(Side::Bid, 100.0, AmountUnit::Quote, round(mark * 1.01))
        .add_liquidity(
            1.0,
            round(mark),
            Some(PriceRange {
                lower: round(mark * 0.9),
                upper: round(mark * 1.1),
            }),
        )
        .push(
            Intent::AddLiquidity {
                base: 1.0,
                quote: round(mark),
                range: None,
                position: Some("position".into()),
                shape: Some(LiquidityShape::BidAsk),
            },
            LineOptions::default(),
        )
        .remove_liquidity(None, 0.0)
        .deposit(10.0)
        .withdraw(None)
        .borrow(5.0)
        .repay(None)
        .push(
            Intent::Liquidate {
                obligation: "11111111111111111111111111111111".into(),
                collateral: "phoenix:perp:SPCX".into(),
                repay: 1.0,
                min_received: Some(0.9),
            },
            LineOptions::default(),
        );
}

/// Step 3's sheet: a reduce-only ask resting above the book; a swap the
/// session can't execute, which mustn't make room under the cap; and a buy
/// of 80, which the cap of 70 cuts to what is left after the position and
/// the bid resting from step 0.
pub fn reduce_only_and_unexecuted(market: &Market, orders: &mut Orders) {
    let mark = market.mark;
    orders
        .ask(round(mark * 1.1), 2.0)
        .reduce_only()
        .expires(market.slot + 100)
        .swap(Side::Ask, market.cap, AmountUnit::Base, round(mark * 0.9))
        .buy(80.0, round(mark * 1.02));
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
            3 => reduce_only_and_unexecuted(market, orders),
            4 => {
                orders.cancel_all().target_position(0.0);
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
