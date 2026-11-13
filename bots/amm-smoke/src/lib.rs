//! The pool smoke: a session's whole pool round trip on every pool it
//! names (Raydium's, Meteora's and Orca's). On the first step it sells 0.5 of the base on each spot market
//! (a swap of base units) and buys with 50 of the quote on each (a swap of
//! quote units); on the third it opens a position on each amm market, 3%
//! around the price, curved on a pool of bins; on the sixth it takes half
//! of each position out; on the ninth the rest. Every step it logs each
//! market's view (top, numbers, positions) and what became of its orders.

use mtx_kit::{AmountUnit, LineOptions, LiquidityShape, PriceRange, VenueView, log, prelude::*};

#[derive(Default)]
struct AmmSmoke;

/// Whether a market's id names an instrument of `kind`.
fn is(view: &VenueView, kind: &str) -> bool {
    view.id.split(':').nth(1) == Some(kind)
}

impl Strategy for AmmSmoke {
    fn start(&mut self, session: &Session) -> BotResult {
        let markets: Vec<_> = session.venues.iter().map(|v| v.id.as_str()).collect();
        log(&format!(
            "start: {} steps, markets {markets:?}",
            session.steps
        ));
        Ok(())
    }

    fn step(&mut self, market: &Market, orders: &mut Orders) -> BotResult {
        for sent in &market.sends {
            log(&format!(
                "sent {} {:?}: {:?}",
                sent.venue, sent.order, sent.state
            ));
        }
        for view in &market.venues {
            log(&format!(
                "step {} {}: bid {:?} ask {:?} stats {:?} positions {:?}",
                market.k, view.id, view.best_bid, view.best_ask, view.stats, view.liquidity
            ));
        }
        for view in &market.venues {
            let (Some(bid), Some(ask)) = (view.best_bid, view.best_ask) else {
                continue;
            };
            match market.k {
                0 if is(view, "spot") => {
                    orders
                        .swap(Side::Ask, 0.5, AmountUnit::Base, bid * 0.97)
                        .at(&view.id);
                    orders
                        .swap(Side::Bid, 50.0, AmountUnit::Quote, ask * 1.03)
                        .at(&view.id);
                }
                2 if is(view, "amm") => {
                    let mid = (bid + ask) / 2.0;
                    let range = PriceRange {
                        lower: mid * 0.97,
                        upper: mid * 1.03,
                    };
                    let shape = view
                        .id
                        .starts_with("meteora:")
                        .then_some(LiquidityShape::Curve);
                    orders.push(
                        Intent::AddLiquidity {
                            base: 1.0,
                            quote: 150.0,
                            range: Some(range),
                            position: None,
                            shape,
                        },
                        LineOptions {
                            venue: Some(view.id.clone()),
                            ..LineOptions::default()
                        },
                    );
                }
                5 if is(view, "amm") => {
                    let id = view.liquidity.first().map(|p| p.id.clone());
                    orders.remove_liquidity(id.as_deref(), 0.5).at(&view.id);
                }
                8 if is(view, "amm") => {
                    orders.remove_liquidity(None, 1.0).at(&view.id);
                }
                _ => {}
            }
        }
        Ok(())
    }
}

mtx_kit::export!(AmmSmoke);
