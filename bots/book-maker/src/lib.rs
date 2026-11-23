//! A maker that quotes off the book, not the oracle.
//!
//! On mainnet the Phoenix book leads and the oracle follows: the gap
//! between the book's mid and the oracle predicts the oracle's next move
//! (correlation 0.74 four seconds ahead) and says nothing of the mid's, and
//! the spread is one tick nearly always. A maker quoting around the oracle
//! quotes around a price a few seconds old, 5 bps from the book on average,
//! and its "one tick inside" crosses the book: over half the oracle maker's
//! paper fills took liquidity, and its markouts were -2 to -3 bps.
//!
//! So this maker:
//!
//! - **anchors on the book**: it joins the best bid and ask, and steps one
//!   tick inside only where the spread leaves room without crossing;
//! - **never takes**: every quote is post-only, so a quote the book moved
//!   through is refused rather than filled as a taker;
//! - **skews against inventory in ticks**: holding long, its bid steps back
//!   and its ask stays at the touch, so fills shed the position;
//! - **steps back from a moving book**: when the mid ran more than a few
//!   bps over the last few steps, the side the move runs into (the ask in
//!   a rally) steps back, where a stale quote is picked off;
//! - **steps back when the oracle is far**: a wide gap is a fast market
//!   the book is still finding; both sides step back.
//!
//! The quoting is `mtx_kit::making`'s ([`touch_targets`] with its default
//! [`TouchConfig`]), shared with the flow maker and tested there.
//!
//! It requotes only when a target price changes, or every
//! [`REQUOTE_SLOTS`], to keep its transactions few.

use mtx_kit::{
    log,
    making::{Momentum, Requote, Sizing, TouchConfig, View, place, quotes, touch_targets},
    prelude::*,
    venue::PhoenixVenue,
};

/// The quote's size, as a share of the position cap: two fills the same
/// way reach it.
const QUOTE_SHARE_OF_CAP: f64 = 0.5;

/// The momentum window, in steps (2 slots each: about 2.4 s).
const MOMENTUM_STEPS: usize = 3;

/// Requote at least this often even if nothing moved, so a quote refused
/// as crossing comes back.
const REQUOTE_SLOTS: u64 = 20;

/// A quote under this size isn't worth a transaction: Phoenix's SOL base
/// lot is 0.001.
const SIZE_MIN: f64 = 0.001;

/// The strategy.
#[derive(Default)]
pub struct BookMaker {
    tick: f64,
    sizing: Option<Sizing>,
    momentum: Momentum,
    requote: Option<Requote>,
}

impl Strategy for BookMaker {
    fn start(&mut self, session: &Session) -> BotResult {
        let cap = session.mandate.universe[0].max_position;
        let venue = PhoenixVenue::from_json(&session.venue)?;
        let market = venue.market(&session.symbol)?;
        self.tick = venue.ticks_to_usd(market, 1);
        if !(self.tick > 0.0 && cap > 0.0) {
            return Err(format!("a tick of {} and a cap of {cap}", self.tick).into());
        }
        let sizing = Sizing {
            size: cap * QUOTE_SHARE_OF_CAP,
            cap,
            size_min: SIZE_MIN,
        };
        self.sizing = Some(sizing);
        self.momentum = Momentum::new(MOMENTUM_STEPS);
        self.requote = Some(Requote::new(REQUOTE_SLOTS));
        log(&format!(
            "quoting off the book: tick {}, cap {cap}, quote size {}",
            self.tick, sizing.size
        ));
        Ok(())
    }

    fn step(&mut self, market: &Market, orders: &mut Orders) -> BotResult {
        let (Some(sizing), Some(requote)) = (self.sizing, self.requote.as_mut()) else {
            return Err("stepped before it started".into());
        };
        let (Some(best_bid), Some(best_ask)) = (market.best_bid, market.best_ask) else {
            return Ok(());
        };
        if !(best_bid > 0.0 && best_ask > best_bid) {
            return Ok(());
        }
        let held = market.position.size;
        let view = View {
            best_bid,
            best_ask,
            oracle: market.oracle,
            inventory: (held / sizing.cap).clamp(-1.0, 1.0),
            momentum_bps: self.momentum.push((best_bid + best_ask) / 2.0),
        };
        let targets = touch_targets(&view, self.tick, &TouchConfig::default());
        let quotes = quotes(targets, held, sizing);
        if requote.due(market.slot, quotes) {
            place(orders, quotes, self.tick);
        }
        Ok(())
    }
}

#[cfg(target_arch = "wasm32")]
mtx_kit::export!(BookMaker);
