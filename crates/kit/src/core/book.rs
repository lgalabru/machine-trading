//! Order book snapshots, price levels, and spread calculations.
//!
//! Every market this crate trades clears through a central limit order book
//! (CLOB): buyers post bids, sellers post asks, and the matching engine
//! crosses them. The data structures here model a point-in-time snapshot of
//! that book.
//!
//! Some venues only report the top of the book, or report prices without the
//! size behind them (Phoenix's on-chain views give the best bid and ask
//! only). A level's [`quantity`](PriceLevel::quantity) is therefore optional,
//! and depth sums are `None` rather than a silently wrong zero when any
//! level's size is unknown.

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use crate::core::instrument::InstrumentId;

/// A single price level in an order book.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PriceLevel {
    /// The price at this level, in quote units per base unit.
    pub price: Decimal,

    /// Total quantity available at this price, in base units, when the venue
    /// reports it.
    pub quantity: Option<Decimal>,
}

impl PriceLevel {
    /// A level whose size is known.
    pub fn new(price: Decimal, quantity: Decimal) -> Self {
        Self {
            price,
            quantity: Some(quantity),
        }
    }

    /// A level whose size the venue does not report.
    pub fn price_only(price: Decimal) -> Self {
        Self {
            price,
            quantity: None,
        }
    }
}

/// The bid-ask spread for an instrument.
///
/// Tight spreads indicate efficient, liquid markets. Wide spreads signal
/// either opportunity (if you can provide liquidity) or danger (if you need
/// to cross the spread to execute).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Spread {
    /// Absolute spread: `best_ask - best_bid`.
    pub absolute: Decimal,

    /// Relative spread as a fraction of the midpoint: `absolute / midpoint`.
    pub relative: Decimal,

    /// The midpoint price: `(best_bid + best_ask) / 2`.
    pub midpoint: Decimal,
}

/// A point-in-time snapshot of an order book.
///
/// Bids are sorted **descending** by price (best bid first).
/// Asks are sorted **ascending** by price (best ask first).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrderBook {
    /// The instrument this book belongs to.
    pub instrument: InstrumentId,

    /// When this snapshot was captured.
    pub timestamp: DateTime<Utc>,

    /// The chain slot the snapshot reflects, for on-chain venues.
    pub slot: Option<u64>,

    /// Bid (buy) side, sorted descending by price.
    pub bids: Vec<PriceLevel>,

    /// Ask (sell) side, sorted ascending by price.
    pub asks: Vec<PriceLevel>,
}

impl OrderBook {
    /// Returns the best bid (highest buy price), if any bids exist.
    pub fn best_bid(&self) -> Option<&PriceLevel> {
        self.bids.first()
    }

    /// Returns the best ask (lowest sell price), if any asks exist.
    pub fn best_ask(&self) -> Option<&PriceLevel> {
        self.asks.first()
    }

    /// Returns the midpoint price between best bid and best ask.
    ///
    /// Returns `None` if either side of the book is empty.
    pub fn midpoint(&self) -> Option<Decimal> {
        let bid = self.best_bid()?.price;
        let ask = self.best_ask()?.price;
        Some((bid + ask) / Decimal::TWO)
    }

    /// Calculates the bid-ask spread.
    ///
    /// Returns `None` if either side of the book is empty or if the midpoint
    /// is zero (degenerate book).
    pub fn spread(&self) -> Option<Spread> {
        let bid = self.best_bid()?.price;
        let ask = self.best_ask()?.price;
        let midpoint = (bid + ask) / Decimal::TWO;

        if midpoint.is_zero() {
            return None;
        }

        let absolute = ask - bid;
        let relative = absolute / midpoint;

        Some(Spread {
            absolute,
            relative,
            midpoint,
        })
    }

    /// Total bid-side depth, or `None` if any bid level's size is unknown.
    pub fn bid_depth(&self) -> Option<Decimal> {
        depth(&self.bids)
    }

    /// Total ask-side depth, or `None` if any ask level's size is unknown.
    pub fn ask_depth(&self) -> Option<Decimal> {
        depth(&self.asks)
    }

    /// Returns `true` if the book has no bids and no asks.
    pub fn is_empty(&self) -> bool {
        self.bids.is_empty() && self.asks.is_empty()
    }

    /// `true` if the best bid is at or above the best ask. Real books never
    /// stay crossed; a crossed snapshot is stale or mixes two moments.
    pub fn is_crossed(&self) -> bool {
        matches!((self.best_bid(), self.best_ask()), (Some(b), Some(a)) if b.price >= a.price)
    }
}

fn depth(levels: &[PriceLevel]) -> Option<Decimal> {
    levels.iter().map(|l| l.quantity).sum()
}

#[cfg(test)]
mod tests {
    use rust_decimal_macros::dec;

    use super::*;

    fn book(bids: Vec<PriceLevel>, asks: Vec<PriceLevel>) -> OrderBook {
        OrderBook {
            instrument: InstrumentId::new("test:SOL"),
            timestamp: Utc::now(),
            slot: None,
            bids,
            asks,
        }
    }

    #[test]
    fn spread_and_midpoint() {
        let b = book(
            vec![PriceLevel::new(dec!(99), dec!(1))],
            vec![PriceLevel::new(dec!(101), dec!(2))],
        );
        assert_eq!(b.midpoint(), Some(dec!(100)));
        let s = b.spread().unwrap();
        assert_eq!(s.absolute, dec!(2));
        assert_eq!(s.relative, dec!(0.02));
        assert!(!b.is_crossed());
        assert!(book(vec![], vec![]).spread().is_none());
    }

    #[test]
    fn depth_is_unknown_when_any_size_is() {
        let known = book(
            vec![
                PriceLevel::new(dec!(99), dec!(1)),
                PriceLevel::new(dec!(98), dec!(3)),
            ],
            vec![PriceLevel::price_only(dec!(101))],
        );
        assert_eq!(known.bid_depth(), Some(dec!(4)));
        assert_eq!(known.ask_depth(), None);
        assert_eq!(book(vec![], vec![]).bid_depth(), Some(dec!(0)));
    }

    #[test]
    fn crossed_book_is_detected() {
        let b = book(
            vec![PriceLevel::price_only(dec!(101))],
            vec![PriceLevel::price_only(dec!(100))],
        );
        assert!(b.is_crossed());
    }
}
