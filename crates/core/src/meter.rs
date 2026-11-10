//! What an enforcer reads from a venue: the [`AccountMeter`] trait.
//!
//! Trading goes through one trait per mechanism, an
//! `ExecutionDriver` for order books and a
//! `LendingProvider`, both in `mtx-kit`, for pools. Measuring
//! an account is different: an enforcer needs the same five things from
//! every venue, whatever it trades, and must read them from the venue's own
//! records rather than from the bot. They are what each venue profile in
//! `specs/venues/` lists under Measurements:
//!
//! - the account's [`Holding`] in each instrument, its value in USD at the
//!   profile's reference prices, and how many markets it holds outside the
//!   mandate's universe, read together as one [`Measurement`];
//! - the fills the venue recorded, for the sides and limit-price rules;
//! - whether the account can be liquidated, and the health of each
//!   borrowing position, where the venue lends.
//!
//! A grader holds one meter per venue in a mandate's universe and audits
//! what they report; see `docs/rfd/0001-instrument-traits.md`.
//!
//! Holdings, value and the universe count come from one call because they
//! must agree: a bot trading between separate reads would show a position in
//! one that the value in another doesn't price.

use rust_decimal::Decimal;
use thiserror::Error;

use crate::{Fill, Holding, InstrumentId};

/// A fill as a venue recorded it, with the slot it landed in: enforcers
/// count in slots.
#[derive(Debug, Clone)]
pub struct RecordedFill {
    /// The slot the fill's transaction landed in.
    pub slot: u64,
    /// The fill.
    pub fill: Fill,
}

/// What a venue holds for the account at one moment, read together.
#[derive(Debug, Clone, PartialEq)]
pub struct Measurement {
    /// The account's holding in each instrument the meter measures, flat
    /// ones included.
    pub holdings: Vec<(InstrumentId, Holding)>,
    /// The account's value on this venue, in USD, at the profile's reference
    /// prices: holdings, plus perpetuals' unrealized PnL, minus what it owes.
    /// Wallet balances other venues also count are the grader's to count
    /// once.
    pub value_usd: Decimal,
    /// Markets on this venue the account holds a position or orders in that
    /// are not in the universe.
    pub outside_universe: u32,
}

impl Measurement {
    /// The account's holding in `instrument`, if the meter measures it.
    pub fn holding(&self, instrument: &InstrumentId) -> Option<&Holding> {
        self.holdings
            .iter()
            .find(|(id, _)| id == instrument)
            .map(|(_, holding)| holding)
    }
}

/// Errors from measuring an account.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum MeterError {
    /// The chain or the venue's service could not be read.
    #[error("read failed: {0}")]
    Read(String),
    /// An account or a record did not decode as the profile describes it.
    #[error("decode {what}: {detail}")]
    Decode {
        /// What was being decoded.
        what: &'static str,
        /// Why it failed.
        detail: String,
    },
    /// The venue has no such measurement.
    #[error("unsupported: {0}")]
    Unsupported(String),
}

/// One venue's measurements of one account, as its venue profile defines
/// them. Everything is read from the venue, never from the trading bot.
#[async_trait::async_trait]
pub trait AccountMeter: Send + Sync {
    /// The account's holdings, value and markets held outside `universe`,
    /// as one measurement.
    async fn measure(&self, universe: &[InstrumentId]) -> Result<Measurement, MeterError>;

    /// Every fill the venue recorded for the account in slots after
    /// `after_slot`, in order, including a maker's fills the venue hasn't
    /// settled into its account yet.
    async fn fills(&self, after_slot: u64) -> Result<Vec<RecordedFill>, MeterError>;

    /// Whether the venue would liquidate the account now. False on venues
    /// that neither margin nor lend.
    async fn liquidatable(&self) -> Result<bool, MeterError> {
        Ok(false)
    }

    /// The health of each of the account's borrowing positions, as the venue
    /// profile defines it: 1 or below is liquidatable. Empty on venues that
    /// don't lend.
    async fn health(&self) -> Result<Vec<(InstrumentId, Decimal)>, MeterError> {
        Ok(Vec::new())
    }
}
