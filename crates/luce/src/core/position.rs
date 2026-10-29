//! Positions: what is held in one instrument, and what it cost.
//!
//! A [`Position`] carries a **signed** quantity (positive long, negative
//! short) and its **entry notional**: what the open quantity cost, in quote
//! units, at the prices it was opened at. Applying a [`Fill`] moves both and
//! books realized PnL when the fill reduces the position.
//!
//! # Accounting
//!
//! Average-cost accounting, the convention perpetual venues use for entry
//! prices:
//!
//! - A fill that **opens or extends** the position adds `quantity × price`
//!   to the entry notional. Exact.
//! - A fill that **reduces** the position releases the closed quantity's
//!   share of the entry notional, `entry_notional × closed / |quantity|`, and
//!   realizes the difference against the fill price.
//! - A fill larger than the position **flips** it: the whole position closes
//!   at the fill price and the remainder opens a new one at the same price.
//!
//! Fees are tracked separately in [`Position::total_fees`]; realized PnL is
//! gross of fees, and [`Position::net_realized_pnl`] subtracts them.
//!
//! The only division is the proportional release on a partial close, which
//! [`Decimal`] carries to 28 significant digits. Closing the whole position
//! releases the entry notional exactly, so the realized PnL of a round trip
//! is always exact.

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::core::{fill::Fill, instrument::InstrumentId, order::Side, venue::VenueId};

/// Errors from applying a fill.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum PositionError {
    /// The fill is for a different instrument.
    #[error("fill for {fill} cannot apply to a position in {position}")]
    WrongInstrument {
        position: InstrumentId,
        fill: InstrumentId,
    },
    /// Fills have a positive quantity; direction comes from the side.
    #[error("fill quantity must be positive, got {0}")]
    NonPositiveQuantity(Decimal),
    /// Prices are non-negative.
    #[error("fill price must not be negative, got {0}")]
    NegativePrice(Decimal),
}

/// What applying one fill did to a position.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct FillEffect {
    /// Quantity of the existing position the fill closed.
    pub closed: Decimal,
    /// Quantity the fill opened (extending, or after a flip).
    pub opened: Decimal,
    /// PnL realized on the closed quantity, gross of fees.
    pub realized_pnl: Decimal,
}

/// A holding in a single instrument.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Position {
    /// The instrument held.
    pub instrument: InstrumentId,

    /// The venue where this position lives.
    pub venue: VenueId,

    /// Signed quantity in base units: positive long, negative short, zero flat.
    pub quantity: Decimal,

    /// What the open quantity cost, in quote units: `Σ quantity × price` over
    /// the fills that opened it, less what partial closes released. Never
    /// negative; zero when flat.
    pub entry_notional: Decimal,

    /// Cumulative PnL realized by reducing the position, gross of fees.
    pub realized_pnl: Decimal,

    /// Cumulative fees paid on fills applied to this position.
    pub total_fees: Decimal,

    /// When the current position (since it was last flat) was opened.
    pub opened_at: Option<DateTime<Utc>>,

    /// When a fill was last applied.
    pub updated_at: Option<DateTime<Utc>>,

    /// The strategy that originated this position.
    pub strategy: Option<String>,
}

impl Position {
    /// A flat position.
    pub fn flat(instrument: InstrumentId, venue: VenueId) -> Self {
        Self {
            instrument,
            venue,
            quantity: Decimal::ZERO,
            entry_notional: Decimal::ZERO,
            realized_pnl: Decimal::ZERO,
            total_fees: Decimal::ZERO,
            opened_at: None,
            updated_at: None,
            strategy: None,
        }
    }

    /// A position as a venue reports it: a signed quantity and an average
    /// entry price. The entry notional is `|quantity| × entry_price`, exactly.
    pub fn from_entry(
        instrument: InstrumentId,
        venue: VenueId,
        quantity: Decimal,
        entry_price: Decimal,
    ) -> Self {
        Self {
            quantity,
            entry_notional: quantity.abs() * entry_price,
            ..Self::flat(instrument, venue)
        }
    }

    /// `Some(Buy)` when long, `Some(Sell)` when short, `None` when flat.
    pub fn side(&self) -> Option<Side> {
        if self.quantity.is_sign_positive() && !self.quantity.is_zero() {
            Some(Side::Buy)
        } else if self.quantity.is_sign_negative() && !self.quantity.is_zero() {
            Some(Side::Sell)
        } else {
            None
        }
    }

    /// Returns `true` if the position is flat.
    pub fn is_closed(&self) -> bool {
        self.quantity.is_zero()
    }

    /// Average entry price, `entry_notional / |quantity|`. `None` when flat.
    pub fn average_entry_price(&self) -> Option<Decimal> {
        if self.quantity.is_zero() {
            None
        } else {
            Some(self.entry_notional / self.quantity.abs())
        }
    }

    /// Entry notional with the position's sign: positive long, negative short.
    fn signed_entry(&self) -> Decimal {
        if self.quantity.is_sign_negative() {
            -self.entry_notional
        } else {
            self.entry_notional
        }
    }

    /// Unrealized PnL at `mark`: `quantity × mark − signed entry notional`.
    ///
    /// Exit fees are not included; they depend on how the position is closed.
    pub fn unrealized_pnl(&self, mark: Decimal) -> Decimal {
        self.quantity * mark - self.signed_entry()
    }

    /// Absolute exposure at `mark`: `|quantity| × mark`.
    pub fn market_value(&self, mark: Decimal) -> Decimal {
        self.quantity.abs() * mark
    }

    /// Realized PnL net of fees.
    pub fn net_realized_pnl(&self) -> Decimal {
        self.realized_pnl - self.total_fees
    }

    /// The position after a hypothetical trade of `quantity` on `side`.
    pub fn quantity_after(&self, side: Side, quantity: Decimal) -> Decimal {
        self.quantity + side.sign() * quantity
    }

    /// Applies a fill: updates quantity and entry notional, books realized
    /// PnL on any reduction, and adds the fill's fees.
    pub fn apply(&mut self, fill: &Fill) -> Result<FillEffect, PositionError> {
        if fill.instrument != self.instrument {
            return Err(PositionError::WrongInstrument {
                position: self.instrument.clone(),
                fill: fill.instrument.clone(),
            });
        }
        if fill.quantity <= Decimal::ZERO {
            return Err(PositionError::NonPositiveQuantity(fill.quantity));
        }
        if fill.price.is_sign_negative() && !fill.price.is_zero() {
            return Err(PositionError::NegativePrice(fill.price));
        }

        let sign = fill.side.sign();
        let held = self.quantity.abs();
        let reduces =
            !self.quantity.is_zero() && self.quantity.is_sign_negative() == (sign > Decimal::ZERO);

        let mut effect = FillEffect::default();
        if reduces {
            let closed = fill.quantity.min(held);
            let released = if closed == held {
                self.entry_notional
            } else {
                self.entry_notional * closed / held
            };
            // Long closes by selling: proceeds − cost. Short closes by
            // buying: cost − what it takes to buy back.
            let proceeds = closed * fill.price;
            effect.realized_pnl = if self.quantity.is_sign_positive() {
                proceeds - released
            } else {
                released - proceeds
            };
            effect.closed = closed;
            self.entry_notional -= released;
            self.quantity += sign * closed;
            self.realized_pnl += effect.realized_pnl;

            let remainder = fill.quantity - closed;
            if remainder > Decimal::ZERO {
                effect.opened = remainder;
                self.quantity = sign * remainder;
                self.entry_notional = remainder * fill.price;
                self.opened_at = Some(fill.timestamp);
            } else if self.quantity.is_zero() {
                self.entry_notional = Decimal::ZERO;
                self.opened_at = None;
            }
        } else {
            if self.quantity.is_zero() {
                self.opened_at = Some(fill.timestamp);
            }
            effect.opened = fill.quantity;
            self.quantity += sign * fill.quantity;
            self.entry_notional += fill.quantity * fill.price;
        }

        self.total_fees += fill.fees;
        self.updated_at = Some(fill.timestamp);
        if self.strategy.is_none() {
            self.strategy.clone_from(&fill.strategy);
        }
        Ok(effect)
    }
}

#[cfg(test)]
mod tests {
    use rust_decimal_macros::dec;

    use super::*;
    use crate::core::{fill::FillId, order::OrderId};

    fn instrument() -> InstrumentId {
        InstrumentId::new("test:SOL-PERP")
    }

    fn fill(side: Side, quantity: Decimal, price: Decimal) -> Fill {
        Fill {
            id: FillId::new(),
            order_id: OrderId::new(),
            instrument: instrument(),
            venue: VenueId::new("test"),
            side,
            price,
            quantity,
            fees: dec!(0.1),
            timestamp: Utc::now(),
            latency: None,
            strategy: None,
        }
    }

    fn flat() -> Position {
        Position::flat(instrument(), VenueId::new("test"))
    }

    #[test]
    fn opening_and_extending_average_the_entry() {
        let mut p = flat();
        let e = p.apply(&fill(Side::Buy, dec!(2), dec!(100))).unwrap();
        assert_eq!(e.opened, dec!(2));
        assert_eq!(e.realized_pnl, dec!(0));
        assert!(p.opened_at.is_some());
        p.apply(&fill(Side::Buy, dec!(2), dec!(110))).unwrap();
        assert_eq!(p.quantity, dec!(4));
        assert_eq!(p.entry_notional, dec!(420));
        assert_eq!(p.average_entry_price(), Some(dec!(105)));
        assert_eq!(p.side(), Some(Side::Buy));
        assert_eq!(p.total_fees, dec!(0.2));
        assert_eq!(p.unrealized_pnl(dec!(120)), dec!(60));
        assert_eq!(p.market_value(dec!(120)), dec!(480));
    }

    #[test]
    fn partial_close_realizes_against_average_cost() {
        let mut p = flat();
        p.apply(&fill(Side::Buy, dec!(4), dec!(105))).unwrap();
        let e = p.apply(&fill(Side::Sell, dec!(1), dec!(115))).unwrap();
        assert_eq!(e.closed, dec!(1));
        assert_eq!(e.opened, dec!(0));
        assert_eq!(e.realized_pnl, dec!(10));
        assert_eq!(p.quantity, dec!(3));
        assert_eq!(p.average_entry_price(), Some(dec!(105)));
        assert_eq!(p.realized_pnl, dec!(10));
        assert_eq!(p.net_realized_pnl(), dec!(9.8));
    }

    #[test]
    fn full_close_is_exact_and_resets_entry() {
        let mut p = flat();
        // Three fills at prices whose average does not terminate in decimal.
        p.apply(&fill(Side::Buy, dec!(1), dec!(100))).unwrap();
        p.apply(&fill(Side::Buy, dec!(1), dec!(100))).unwrap();
        p.apply(&fill(Side::Buy, dec!(1), dec!(101))).unwrap();
        // Close a third, then the rest.
        p.apply(&fill(Side::Sell, dec!(1), dec!(102))).unwrap();
        p.apply(&fill(Side::Sell, dec!(2), dec!(102))).unwrap();
        assert!(p.is_closed());
        assert_eq!(p.side(), None);
        assert_eq!(p.entry_notional, dec!(0));
        assert_eq!(p.average_entry_price(), None);
        assert_eq!(p.opened_at, None);
        // Bought for 301, sold for 306: exactly 5, whatever the rounding of
        // the intermediate release.
        assert_eq!(p.realized_pnl, dec!(5));
    }

    #[test]
    fn short_positions_profit_when_price_falls() {
        let mut p = flat();
        p.apply(&fill(Side::Sell, dec!(3), dec!(100))).unwrap();
        assert_eq!(p.quantity, dec!(-3));
        assert_eq!(p.side(), Some(Side::Sell));
        assert_eq!(p.unrealized_pnl(dec!(90)), dec!(30));
        let e = p.apply(&fill(Side::Buy, dec!(1), dec!(90))).unwrap();
        assert_eq!(e.realized_pnl, dec!(10));
        assert_eq!(p.quantity, dec!(-2));
        assert_eq!(p.average_entry_price(), Some(dec!(100)));
        assert_eq!(p.unrealized_pnl(dec!(110)), dec!(-20));
    }

    #[test]
    fn oversized_fill_flips_the_position() {
        let mut p = flat();
        p.apply(&fill(Side::Buy, dec!(2), dec!(100))).unwrap();
        let e = p.apply(&fill(Side::Sell, dec!(5), dec!(90))).unwrap();
        assert_eq!(e.closed, dec!(2));
        assert_eq!(e.opened, dec!(3));
        assert_eq!(e.realized_pnl, dec!(-20));
        assert_eq!(p.quantity, dec!(-3));
        assert_eq!(p.entry_notional, dec!(270));
        assert_eq!(p.average_entry_price(), Some(dec!(90)));
        assert_eq!(p.realized_pnl, dec!(-20));

        // And back: short 3 at 90, buy 4 at 80 → +30 realized, long 1 at 80.
        let e = p.apply(&fill(Side::Buy, dec!(4), dec!(80))).unwrap();
        assert_eq!(e.realized_pnl, dec!(30));
        assert_eq!(p.quantity, dec!(1));
        assert_eq!(p.average_entry_price(), Some(dec!(80)));
        assert_eq!(p.realized_pnl, dec!(10));
    }

    #[test]
    fn rejects_invalid_fills() {
        let mut p = flat();
        let mut other = fill(Side::Buy, dec!(1), dec!(1));
        other.instrument = InstrumentId::new("test:BTC-PERP");
        assert!(matches!(
            p.apply(&other).unwrap_err(),
            PositionError::WrongInstrument { .. }
        ));
        assert_eq!(
            p.apply(&fill(Side::Buy, dec!(0), dec!(1))).unwrap_err(),
            PositionError::NonPositiveQuantity(dec!(0))
        );
        assert_eq!(
            p.apply(&fill(Side::Buy, dec!(1), dec!(-1))).unwrap_err(),
            PositionError::NegativePrice(dec!(-1))
        );
        assert_eq!(
            p,
            flat(),
            "failed applications leave the position unchanged"
        );
    }

    #[test]
    fn from_entry_matches_venue_report() {
        let p = Position::from_entry(instrument(), VenueId::new("test"), dec!(-1.5), dec!(150));
        assert_eq!(p.entry_notional, dec!(225));
        assert_eq!(p.average_entry_price(), Some(dec!(150)));
        assert_eq!(p.unrealized_pnl(dec!(140)), dec!(15));
        assert_eq!(p.quantity_after(Side::Buy, dec!(2)), dec!(0.5));
    }
}
