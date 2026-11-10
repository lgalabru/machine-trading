//! What an enforcer reads from a venue: the [`AccountMeter`] trait.
//!
//! Trading goes through one trait per mechanism, an
//! [`ExecutionDriver`](super::ExecutionDriver) for order books and a
//! [`LendingProvider`](crate::lending::LendingProvider) for pools. Measuring
//! an account is different: an enforcer needs the same five things from
//! every venue, whatever it trades, and must read them from the venue's own
//! records rather than from the bot. They are what each venue profile in
//! `specs/venues/` lists under Measurements:
//!
//! - the account's [`Holding`] in each instrument;
//! - the instruments it has open orders in, for the universe rule;
//! - the fills the venue recorded, for the sides and limit-price rules;
//! - the account's value in USD, at the profile's reference prices;
//! - the health of each borrowing position, where the venue lends.
//!
//! A grader holds one meter per venue in a mandate's universe and audits
//! what they report; see `docs/rfd/0001-instrument-traits.md`.

use rust_decimal::Decimal;
use thiserror::Error;

use super::{Fill, Holding, InstrumentId};

/// A fill as a venue recorded it, with the slot it landed in: enforcers
/// count in slots.
#[derive(Debug, Clone)]
pub struct RecordedFill {
    /// The slot the fill's transaction landed in.
    pub slot: u64,
    /// The fill.
    pub fill: Fill,
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
    /// The account's holding in each instrument it holds anything in, on this
    /// venue.
    async fn holdings(&self) -> Result<Vec<(InstrumentId, Holding)>, MeterError>;

    /// The instruments the account has open orders in.
    async fn open_orders(&self) -> Result<Vec<InstrumentId>, MeterError>;

    /// Every fill the venue recorded for the account in slots after
    /// `after_slot`, in order, including a maker's fills the venue hasn't
    /// settled into its account yet.
    async fn fills(&self, after_slot: u64) -> Result<Vec<RecordedFill>, MeterError>;

    /// The account's value on this venue, in USD, at the profile's reference
    /// prices: holdings, plus perpetuals' unrealized PnL, minus what it owes.
    /// Wallet balances other venues also count are the grader's to count
    /// once.
    async fn value_usd(&self) -> Result<Decimal, MeterError>;

    /// The health of each of the account's borrowing positions, as the venue
    /// profile defines it: 1 or below is liquidatable. Empty on venues that
    /// don't lend.
    async fn health(&self) -> Result<Vec<(InstrumentId, Decimal)>, MeterError> {
        Ok(Vec::new())
    }
}
