//! Instruments: what an order book trades.
//!
//! An [`Instrument`] is a listing on a venue that pairs a **base** asset (what
//! is bought and sold, and what quantities are counted in) with a **quote**
//! asset (what prices are expressed in and what settles). It is either a
//! [`Spot`](InstrumentKind::Spot) market, where the base asset changes hands,
//! or a [`Perpetual`](InstrumentKind::Perpetual) future, where only the quote
//! asset moves and the base asset is a unit of exposure.
//!
//! Everything downstream (orders, fills, positions, portfolios) references
//! instruments by [`InstrumentId`] and is agnostic to the kind.
//!
//! # Exactness
//!
//! Venues accept prices on a tick grid and quantities on a lot grid.
//! [`Instrument::price_to_ticks`] and [`Instrument::quantity_to_lots`] convert
//! exactly or fail; a price between two ticks is an error, not a rounding.
//! When rounding is what the caller wants, [`Instrument::round_price`] and
//! [`Instrument::round_quantity`] do it with an explicit [`Rounding`].

use rust_decimal::{Decimal, prelude::ToPrimitive};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::core::{asset::Asset, venue::VenueId};

/// Unique identifier for a tradeable instrument.
///
/// The convention is `"{venue}:{symbol}"`, e.g. `"phoenix:SOL-PERP"`, which
/// keeps identifiers unique across venues.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct InstrumentId(pub String);

impl InstrumentId {
    /// Creates an identifier.
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }
}

impl std::fmt::Display for InstrumentId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// What holding a position in the instrument means.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum InstrumentKind {
    /// The base asset itself changes hands; positions are inventory.
    Spot,
    /// A perpetual future: positions are margined exposure to the base asset,
    /// settled in the quote asset, with no expiry. Shorting is native.
    Perpetual,
}

impl std::fmt::Display for InstrumentKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Spot => f.write_str("spot"),
            Self::Perpetual => f.write_str("perpetual"),
        }
    }
}

/// Direction of an explicit rounding.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Rounding {
    /// Toward negative infinity.
    Down,
    /// Toward positive infinity.
    Up,
    /// To the nearest grid point, halves away from zero.
    Nearest,
}

/// Errors from validating instruments and converting to the venue's grid.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum InstrumentError {
    /// Tick and lot sizes must be positive.
    #[error("{what} must be positive, got {value}")]
    NonPositiveIncrement { what: &'static str, value: Decimal },
    /// The price is not a whole number of ticks.
    #[error("price {price} is not a multiple of the tick size {tick_size}")]
    OffTick { price: Decimal, tick_size: Decimal },
    /// The quantity is not a whole number of lots.
    #[error("quantity {quantity} is not a multiple of the lot size {lot_size}")]
    OffLot {
        quantity: Decimal,
        lot_size: Decimal,
    },
    /// Prices and quantities on the grid are non-negative.
    #[error("{what} {value} is negative")]
    Negative { what: &'static str, value: Decimal },
    /// The grid index does not fit in 64 bits.
    #[error("{what} {value} is out of range for the venue")]
    OutOfRange { what: &'static str, value: Decimal },
}

/// A market that trades a base asset against a quote asset.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Instrument {
    /// Unique identifier.
    pub id: InstrumentId,
    /// Where it trades.
    pub venue: VenueId,
    /// The venue's own name for the market, e.g. `"SOL"`.
    pub symbol: String,
    /// Spot or perpetual.
    pub kind: InstrumentKind,
    /// What quantities are counted in.
    pub base: Asset,
    /// What prices are expressed in and what settles.
    pub quote: Asset,
    /// Smallest price increment, in quote units per base unit.
    pub tick_size: Decimal,
    /// Smallest quantity increment, in base units.
    pub lot_size: Decimal,
}

impl Instrument {
    /// Creates an instrument, validating that tick and lot sizes are positive.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        id: InstrumentId,
        venue: VenueId,
        symbol: impl Into<String>,
        kind: InstrumentKind,
        base: Asset,
        quote: Asset,
        tick_size: Decimal,
        lot_size: Decimal,
    ) -> Result<Self, InstrumentError> {
        for (what, value) in [("tick size", tick_size), ("lot size", lot_size)] {
            if value <= Decimal::ZERO {
                return Err(InstrumentError::NonPositiveIncrement { what, value });
            }
        }
        Ok(Self {
            id,
            venue,
            symbol: symbol.into(),
            kind,
            base,
            quote,
            tick_size,
            lot_size,
        })
    }

    /// A display name such as `"SOL/USDC perpetual"`.
    pub fn name(&self) -> String {
        format!("{}/{} {}", self.base.symbol, self.quote.symbol, self.kind)
    }

    /// The price of `ticks`, exactly: `ticks × tick_size`.
    pub fn ticks_to_price(&self, ticks: u64) -> Decimal {
        Decimal::from(ticks) * self.tick_size
    }

    /// The number of ticks in `price`. Fails if `price` is off the grid.
    pub fn price_to_ticks(&self, price: Decimal) -> Result<u64, InstrumentError> {
        grid_index(price, self.tick_size, "price").map_err(|e| match e {
            GridError::Off => InstrumentError::OffTick {
                price,
                tick_size: self.tick_size,
            },
            GridError::Other(e) => e,
        })
    }

    /// The quantity of `lots`, exactly: `lots × lot_size`.
    pub fn lots_to_quantity(&self, lots: u64) -> Decimal {
        Decimal::from(lots) * self.lot_size
    }

    /// The number of lots in `quantity`. Fails if `quantity` is off the grid.
    pub fn quantity_to_lots(&self, quantity: Decimal) -> Result<u64, InstrumentError> {
        grid_index(quantity, self.lot_size, "quantity").map_err(|e| match e {
            GridError::Off => InstrumentError::OffLot {
                quantity,
                lot_size: self.lot_size,
            },
            GridError::Other(e) => e,
        })
    }

    /// `price` moved onto the tick grid in the given direction.
    pub fn round_price(&self, price: Decimal, rounding: Rounding) -> Decimal {
        round_to(price, self.tick_size, rounding)
    }

    /// `quantity` moved onto the lot grid in the given direction.
    pub fn round_quantity(&self, quantity: Decimal, rounding: Rounding) -> Decimal {
        round_to(quantity, self.lot_size, rounding)
    }

    /// `true` if `price` is a whole number of ticks.
    pub fn is_on_tick(&self, price: Decimal) -> bool {
        (price % self.tick_size).is_zero()
    }

    /// `true` if `quantity` is a whole number of lots.
    pub fn is_on_lot(&self, quantity: Decimal) -> bool {
        (quantity % self.lot_size).is_zero()
    }

    /// Notional value of `quantity` at `price`, in quote units.
    pub fn notional(&self, price: Decimal, quantity: Decimal) -> Decimal {
        price * quantity
    }
}

enum GridError {
    Off,
    Other(InstrumentError),
}

fn grid_index(value: Decimal, step: Decimal, what: &'static str) -> Result<u64, GridError> {
    if value.is_sign_negative() && !value.is_zero() {
        return Err(GridError::Other(InstrumentError::Negative { what, value }));
    }
    let index = value
        .checked_div(step)
        .ok_or(GridError::Other(InstrumentError::OutOfRange {
            what,
            value,
        }))?;
    // Decimal division rounds at 28 significant digits; the product check
    // below makes sure we never accept a value that only *looks* aligned.
    if !index.fract().is_zero() || index * step != value {
        return Err(GridError::Off);
    }
    index
        .to_u64()
        .ok_or(GridError::Other(InstrumentError::OutOfRange {
            what,
            value,
        }))
}

fn round_to(value: Decimal, step: Decimal, rounding: Rounding) -> Decimal {
    let steps = value / step;
    let whole = match rounding {
        Rounding::Down => steps.floor(),
        Rounding::Up => steps.ceil(),
        Rounding::Nearest => {
            steps.round_dp_with_strategy(0, rust_decimal::RoundingStrategy::MidpointAwayFromZero)
        }
    };
    whole * step
}

#[cfg(test)]
mod tests {
    use rust_decimal_macros::dec;

    use super::*;

    fn sol_perp() -> Instrument {
        Instrument::new(
            InstrumentId::new("phoenix:SOL"),
            VenueId::new("phoenix"),
            "SOL",
            InstrumentKind::Perpetual,
            Asset::new("phoenix:SOL", "SOL", 2).unwrap(),
            Asset::new("EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v", "USDC", 6).unwrap(),
            dec!(0.01),
            dec!(0.01),
        )
        .unwrap()
    }

    #[test]
    fn rejects_non_positive_increments() {
        let mut bad = sol_perp();
        bad.tick_size = dec!(0);
        let err = Instrument::new(
            bad.id,
            bad.venue,
            bad.symbol,
            bad.kind,
            bad.base,
            bad.quote,
            bad.tick_size,
            bad.lot_size,
        )
        .unwrap_err();
        assert!(matches!(
            err,
            InstrumentError::NonPositiveIncrement {
                what: "tick size",
                ..
            }
        ));
    }

    #[test]
    fn exact_tick_conversion() {
        let i = sol_perp();
        assert_eq!(i.price_to_ticks(dec!(150.25)).unwrap(), 15025);
        assert_eq!(i.ticks_to_price(15025), dec!(150.25));
        assert_eq!(i.price_to_ticks(dec!(0)).unwrap(), 0);
        assert_eq!(
            i.price_to_ticks(dec!(150.255)).unwrap_err(),
            InstrumentError::OffTick {
                price: dec!(150.255),
                tick_size: dec!(0.01)
            }
        );
        assert!(matches!(
            i.price_to_ticks(dec!(-1)).unwrap_err(),
            InstrumentError::Negative { .. }
        ));
    }

    #[test]
    fn exact_lot_conversion() {
        let i = sol_perp();
        assert_eq!(i.quantity_to_lots(dec!(2.5)).unwrap(), 250);
        assert_eq!(i.lots_to_quantity(250), dec!(2.50));
        assert!(matches!(
            i.quantity_to_lots(dec!(0.001)).unwrap_err(),
            InstrumentError::OffLot { .. }
        ));
    }

    #[test]
    fn explicit_rounding() {
        let i = sol_perp();
        assert_eq!(i.round_price(dec!(150.257), Rounding::Down), dec!(150.25));
        assert_eq!(i.round_price(dec!(150.251), Rounding::Up), dec!(150.26));
        assert_eq!(
            i.round_price(dec!(150.255), Rounding::Nearest),
            dec!(150.26)
        );
        assert_eq!(i.round_price(dec!(150.25), Rounding::Up), dec!(150.25));
        assert_eq!(i.round_quantity(dec!(1.239), Rounding::Down), dec!(1.23));
        assert!(i.is_on_tick(dec!(1.20)));
        assert!(!i.is_on_lot(dec!(1.201)));
    }

    #[test]
    fn name_and_notional() {
        let i = sol_perp();
        assert_eq!(i.name(), "SOL/USDC perpetual");
        assert_eq!(i.notional(dec!(150), dec!(2.5)), dec!(375));
    }
}
