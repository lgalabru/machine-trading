//! What an account holds in one instrument, as its instrument profile
//! defines it.
//!
//! Instruments differ in what holding them means, not in how they trade:
//! perpetuals and spot markets both trade through an
//! [`ExecutionDriver`](super::ExecutionDriver), but a perp is a signed
//! position, a spot market is a balance of the base asset wherever it sits,
//! and a money market is what was supplied and what was borrowed. A
//! [`Holding`] is one of the three, each the shape its profile in
//! `specs/instruments/` gives it, so the mandate's rules read one
//! [`Holding::position`] whatever the instrument.
//!
//! Every venue fills in the same type: a Phoenix v1 and an OpenBook spot
//! market both report [`Holdings`].

use rust_decimal::Decimal;

use super::{Asset, Position};

/// What the account holds in one instrument.
#[derive(Debug, Clone, PartialEq)]
pub enum Holding {
    /// A perpetual (`specs/instruments/perp.md`): a signed position and its
    /// entry.
    Perp(Position),
    /// A spot market (`specs/instruments/spot.md`): the base asset, wherever
    /// it sits.
    Spot(Holdings),
    /// A money market (`specs/instruments/money-market.md`): supplied and
    /// borrowed.
    MoneyMarket(LendingBalance),
}

impl Holding {
    /// The position the mandate's position cap and `min_position` bound, in
    /// base units: the perp's signed quantity, the spot base asset held, or
    /// what is supplied minus what is borrowed.
    pub fn position(&self) -> Decimal {
        match self {
            Holding::Perp(position) => position.quantity,
            Holding::Spot(holdings) => holdings.total(),
            Holding::MoneyMarket(balance) => balance.net(),
        }
    }
}

/// A spot market's base asset, as the spot profile counts it: in the wallet,
/// free on the venue, and locked behind the account's open orders. A venue
/// with several accounts per owner (OpenBook's open orders accounts) sums
/// them into `free` and `locked`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Holdings {
    /// The base asset.
    pub base: Asset,
    /// In the owner's wallet, in base units. For SOL, wrapped SOL only: the
    /// profiles leave native SOL out, since it pays fees.
    pub wallet: Decimal,
    /// On the venue and free to withdraw, in base units.
    pub free: Decimal,
    /// On the venue, locked behind the account's resting asks, in base
    /// units.
    pub locked: Decimal,
}

impl Holdings {
    /// Nothing held.
    pub fn empty(base: Asset) -> Self {
        Self {
            base,
            wallet: Decimal::ZERO,
            free: Decimal::ZERO,
            locked: Decimal::ZERO,
        }
    }

    /// Everything held, wherever: the spot position.
    pub fn total(&self) -> Decimal {
        self.wallet + self.free + self.locked
    }
}

/// A money market's reserve, as the money-market profile counts it: what the
/// account supplied and what it borrowed, interest included, in the asset's
/// base units.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LendingBalance {
    /// The reserve's asset.
    pub asset: Asset,
    /// Supplied, at what it redeems for now.
    pub supplied: Decimal,
    /// Borrowed, with the interest owed so far.
    pub borrowed: Decimal,
}

impl LendingBalance {
    /// Supplied minus borrowed: the money-market position, negative when the
    /// account owes more than it lent.
    pub fn net(&self) -> Decimal {
        self.supplied - self.borrowed
    }
}

#[cfg(test)]
mod tests {
    use rust_decimal_macros::dec;

    use super::*;
    use crate::core::{InstrumentId, VenueId};

    fn usdc() -> Asset {
        Asset::new("EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v", "USDC", 6).unwrap()
    }

    #[test]
    fn each_kind_reports_its_profiles_position() {
        let mut perp = Position::flat(
            InstrumentId::new("phoenix:SPCX-PERP"),
            VenueId::new("phoenix"),
        );
        perp.quantity = dec!(-12.5);
        assert_eq!(Holding::Perp(perp).position(), dec!(-12.5));

        let spot = Holdings {
            wallet: dec!(6),
            free: dec!(1),
            locked: dec!(4),
            ..Holdings::empty(usdc())
        };
        assert_eq!(spot.total(), dec!(11));
        assert_eq!(Holding::Spot(spot).position(), dec!(11));

        let lent = LendingBalance {
            asset: usdc(),
            supplied: dec!(1000),
            borrowed: dec!(0),
        };
        assert_eq!(Holding::MoneyMarket(lent).position(), dec!(1000));
        let owed = LendingBalance {
            asset: usdc(),
            supplied: dec!(0),
            borrowed: dec!(500),
        };
        assert_eq!(Holding::MoneyMarket(owed).position(), dec!(-500));
    }
}
