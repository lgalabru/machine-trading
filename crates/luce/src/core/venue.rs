//! Venue identification and metadata.
//!
//! A venue is any exchange or protocol where instruments trade: an on-chain
//! order book such as Phoenix, an AMM, a centralized exchange. The trading
//! system needs to know about venues for fee calculations and cross-venue
//! comparisons. What a venue settles in is a property of each
//! [`Instrument`](crate::core::Instrument) (its quote asset), not of the venue:
//! one venue may list markets quoted in several assets.

use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

/// Unique identifier for a trading venue, e.g. `"phoenix"`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct VenueId(pub String);

impl VenueId {
    /// Creates a venue identifier.
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }
}

impl std::fmt::Display for VenueId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Which side of the trade provided liquidity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Liquidity {
    /// The order rested on the book and was filled by someone else.
    Maker,
    /// The order took liquidity that was resting on the book.
    Taker,
}

/// Fee schedule for a trading venue, as fractions of traded notional.
///
/// Crypto venues charge a rate on notional (`0.0005` is 5 basis points)
/// rather than a fixed amount per unit. A negative maker rate is a rebate.
/// Fees directly reduce net edge: a strategy that shows 3 bps of gross edge
/// but pays 5 bps to take liquidity is a losing strategy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct FeeSchedule {
    /// Rate charged when providing liquidity. May be negative (rebate).
    pub maker_rate: Decimal,
    /// Rate charged when taking liquidity.
    pub taker_rate: Decimal,
}

impl FeeSchedule {
    /// No fees on either side.
    pub const ZERO: Self = Self {
        maker_rate: Decimal::ZERO,
        taker_rate: Decimal::ZERO,
    };

    /// The rate that applies to `liquidity`.
    pub fn rate(&self, liquidity: Liquidity) -> Decimal {
        match liquidity {
            Liquidity::Maker => self.maker_rate,
            Liquidity::Taker => self.taker_rate,
        }
    }

    /// The fee on `notional` (quote units), exactly: `notional × rate`.
    pub fn fee(&self, notional: Decimal, liquidity: Liquidity) -> Decimal {
        notional * self.rate(liquidity)
    }
}

/// Static metadata about a trading venue.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VenueMetadata {
    /// Unique venue identifier.
    pub id: VenueId,
    /// Human-readable venue name, e.g. `"Phoenix"`.
    pub name: String,
    /// Default fee schedule. Individual markets may override it.
    pub fees: FeeSchedule,
}

#[cfg(test)]
mod tests {
    use rust_decimal_macros::dec;

    use super::*;

    #[test]
    fn fees_are_rates_on_notional() {
        let fees = FeeSchedule {
            maker_rate: dec!(-0.0001),
            taker_rate: dec!(0.0005),
        };
        assert_eq!(fees.fee(dec!(10000), Liquidity::Taker), dec!(5));
        assert_eq!(fees.fee(dec!(10000), Liquidity::Maker), dec!(-1));
        assert_eq!(FeeSchedule::ZERO.fee(dec!(123), Liquidity::Taker), dec!(0));
    }
}
