//! A probe of the transaction layer's searchers (`docs/tx-layer.md`): it
//! sells `SIZE` SPCX on a pool once (or on each of two), the way its regime (`MTX_REGIME`)
//! says, and logs each market's top every step and what became of every
//! send. The grader's tests run it under sandwichers, JIT liquidity and
//! backrunners and compare what each way of selling got.
//!
//! - `public`: one swap on Raydium at step 2, on the public route, taking
//!   down to 5% under the bid.
//! - `tight`: the same swap, its limit 1% under the bid, a little more than its own price impact.
//! - `private`: the same swap, sent alone as a Jito bundle with a tip.
//! - `split`: `SIZE` on Raydium at step 2, and a tenth of it on Meteora,
//!   the shallower pool, at step 5.
//! - `together`: the two at step 2, in one bundle with a tip.
//! - `perp-public`: on a perp session's own market, a sale of `PERP_SIZE`
//!   at every step from 1 to 16, on the public route, down to 2% under the
//!   bid; `perp-private`: the same, each a Jito bundle with a tip.
//! - anything else: nothing.

use mtx_kit::{AmountUnit, Route, log, prelude::*};

const SIZE: f64 = 2_000.0;
const TIP: u64 = 20_000;
const PERP_SIZE: f64 = 2.0;

#[derive(Default)]
struct Probe {
    regime: String,
}

fn private(bundle: Option<u32>) -> TxOptions {
    TxOptions {
        tip: Some(TIP),
        bundle,
        route: Some(Route::Jito {
            bundle_only: true,
            dont_front: false,
        }),
        ..TxOptions::default()
    }
}

impl Strategy for Probe {
    fn start(&mut self, session: &Session) -> BotResult {
        self.regime = session.var("MTX_REGIME").unwrap_or("idle").to_string();
        log(&format!("start: regime {}", self.regime));
        Ok(())
    }

    fn step(&mut self, market: &Market, orders: &mut Orders) -> BotResult {
        for sent in &market.sends {
            log(&format!(
                "sent {} {:?}: {:?} slot {:?} fee {:?} tip {:?}",
                sent.venue, sent.order, sent.state, sent.slot, sent.fee, sent.tip
            ));
        }
        let top = |venue: &str| {
            market
                .venues
                .iter()
                .find(|v| v.id == format!("{venue}:spot:SPCX-USDC"))
                .and_then(|v| v.best_bid.map(|bid| (v.id.clone(), bid)))
        };
        for view in &market.venues {
            log(&format!(
                "step {} {}: bid {:?} ask {:?}",
                market.k, view.id, view.best_bid, view.best_ask
            ));
        }
        if let Some(private_perp) = match self.regime.as_str() {
            "perp-public" => Some(false),
            "perp-private" => Some(true),
            _ => None,
        } {
            if let Some(bid) = market.venues.first().and_then(|v| v.best_bid)
                && (1..=16).contains(&market.k)
            {
                orders.sell(PERP_SIZE, bid * 0.98);
                if private_perp {
                    orders.tx(private(None));
                }
            }
            return Ok(());
        }
        let (Some((raydium, r_bid)), Some((meteora, m_bid))) = (top("raydium"), top("meteora"))
        else {
            return Ok(());
        };
        let sell = |orders: &mut Orders, venue: &str, size: f64, limit: f64| {
            orders
                .swap(Side::Ask, size, AmountUnit::Base, limit)
                .at(venue);
        };
        match (self.regime.as_str(), market.k) {
            ("public", 2) => sell(orders, &raydium, SIZE, r_bid * 0.95),
            ("tight", 2) => sell(orders, &raydium, SIZE, r_bid * 0.99),
            ("private", 2) => {
                sell(orders, &raydium, SIZE, r_bid * 0.95);
                orders.tx(private(None));
            }
            ("split", 2) => sell(orders, &raydium, SIZE, r_bid * 0.95),
            ("split", 5) => sell(orders, &meteora, SIZE / 10.0, m_bid * 0.95),
            ("together", 2) => {
                sell(orders, &raydium, SIZE, r_bid * 0.95);
                orders.tx(TxOptions {
                    bundle: Some(1),
                    ..TxOptions::default()
                });
                sell(orders, &meteora, SIZE / 10.0, m_bid * 0.95);
                orders.tx(private(Some(1)));
            }
            _ => {}
        }
        Ok(())
    }
}

mtx_kit::export!(Probe);
