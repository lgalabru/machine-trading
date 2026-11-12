use thiserror::Error;

use crate::{
    chain::{ChainError, PlanError},
    core::InstrumentError,
};

/// Errors from the Phoenix v1 adapter.
#[derive(Debug, Error)]
pub enum PhoenixV1Error {
    /// Chain access, account decoding or plan execution failed.
    #[error(transparent)]
    Chain(#[from] ChainError),
    /// Instructions could not be packed.
    #[error(transparent)]
    Plan(#[from] PlanError),
    /// A price or quantity is off the market's grid, or the header does not
    /// describe a valid instrument.
    #[error(transparent)]
    Instrument(#[from] InstrumentError),
    /// The order cannot be expressed on Phoenix v1.
    #[error("invalid order: {0}")]
    InvalidOrder(String),
}

impl From<crate::protocols::DecodeError> for PhoenixV1Error {
    fn from(err: crate::protocols::DecodeError) -> Self {
        Self::Chain(err.into())
    }
}
