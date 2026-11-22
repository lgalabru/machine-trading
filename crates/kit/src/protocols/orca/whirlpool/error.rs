use thiserror::Error;

use crate::protocols::DecodeError;

/// Why a Whirlpool quote has no answer.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum WhirlpoolError {
    /// An account didn't have the shape the program writes.
    #[error(transparent)]
    Decode(#[from] DecodeError),
    /// The program would fail the swap with this error: the quote stops
    /// exactly where the program would.
    #[error("the program would fail the swap: {0:?}")]
    Program(ProgramFailure),
    /// The swap needs this tick array, which wasn't supplied (neither
    /// decoded nor marked as [`TickArray::uninitialized`](super::TickArray::uninitialized)).
    #[error("tick array starting at tick {start_tick_index} wasn't supplied")]
    MissingTickArray { start_tick_index: i32 },
    /// The pool charges an adaptive fee, which its oracle account holds,
    /// and none was supplied.
    #[error("the pool charges an adaptive fee: its oracle account is needed")]
    MissingOracle,
    /// The swap would take more steps than a transaction's compute budget
    /// allows ([`STEPS_MAX`](super::STEPS_MAX)).
    #[error("the swap takes more than {0} steps")]
    TooManySteps(usize),
}

/// The program's own errors (`programs/whirlpool/src/errors.rs`) that a
/// swap can fail with, by the program's names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProgramFailure {
    AmountCalcOverflow,
    AmountRemainingOverflow,
    DivideByZero,
    InvalidSqrtPriceLimitDirection,
    InvalidTickArraySequence,
    InvalidTimestamp,
    LiquidityOverflow,
    LiquidityUnderflow,
    MulDivOverflow,
    MultiplicationOverflow,
    MultiplicationShiftRightOverflow,
    NumberCastError,
    NumberDownCastError,
    PartialFillError,
    SqrtPriceOutOfBounds,
    TickArrayIndexOutofBounds,
    TickArraySequenceInvalidIndex,
    TokenMaxExceeded,
    TokenMinSubceeded,
    TradeIsNotEnabled,
    ZeroTradableAmount,
}

impl From<ProgramFailure> for WhirlpoolError {
    fn from(failure: ProgramFailure) -> Self {
        Self::Program(failure)
    }
}
