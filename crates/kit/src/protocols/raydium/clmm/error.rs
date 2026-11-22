use thiserror::Error;

use crate::protocols::DecodeError;

/// Why a Raydium CLMM quote failed: each variant is a way the program's
/// swap fails (its `ErrorCode`, or a panic in its integer math), so a
/// quote that fails here is a transaction that would fail on chain.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ClmmError {
    /// An account didn't have the program's layout.
    #[error(transparent)]
    Decode(#[from] DecodeError),
    /// `ZeroAmountSpecified`.
    #[error("the swap amount is zero")]
    ZeroAmount,
    /// `NotApproved`: the pool's status disables swaps.
    #[error("swaps are disabled on this pool")]
    SwapDisabled,
    /// The pool's `open_time` is not yet past.
    #[error("the pool opens at {open_time_s}, after {block_timestamp_s}")]
    PoolNotOpen {
        open_time_s: u64,
        block_timestamp_s: u64,
    },
    /// `SqrtPriceLimitOverflow`: the limit is on the wrong side of the
    /// price, or outside the program's bounds.
    #[error("sqrt price limit {sqrt_price_limit_x64} is invalid for this direction")]
    SqrtPriceLimit { sqrt_price_limit_x64: u128 },
    /// The swap reaches a tick array the caller didn't supply: the
    /// program would fail with `NotEnoughTickArrayAccount`.
    #[error("the swap needs the tick array starting at tick {start_tick_index}")]
    MissingTickArray { start_tick_index: i32 },
    /// `MissingTickArrayBitmapExtensionAccount`.
    #[error("the swap needs the pool's tick array bitmap extension")]
    MissingBitmapExtension,
    /// `LiquidityInsufficient` or `InsufficientLiquidityForDirection`: no
    /// initialized tick array is left in the swap's direction.
    #[error("no liquidity left in the swap's direction")]
    LiquidityInsufficient,
    /// `TooSmallInputOrOutputAmount`: one side of the swap rounds to zero.
    #[error("the swap moves zero of one token")]
    TooSmallAmount,
    /// The swap stopped short of the amount and no price limit was given:
    /// `swap_v2` refuses partial fills then.
    #[error("only {filled} of {amount} could be swapped")]
    PartialFill { amount: u64, filled: u64 },
    /// A tick array or tick index the program rejects (`InvalidTickArray`,
    /// `InvalidTickIndex`, `InvalidTickArrayBoundary`,
    /// `InvalidFirstTickArrayAccount`).
    #[error("invalid tick array or tick: {0}")]
    InvalidTick(&'static str),
    /// A limit order on a tick holds less than the swap takes from it
    /// (`InvalidLimitOrderAmount`).
    #[error("a limit order holds less than the swap takes")]
    InvalidLimitOrderAmount,
    /// Arithmetic the program checks overflowed (`CalculateOverflow`,
    /// `MaxTokenOverflow`, a `uint` panic), or a `require!` on the swap's
    /// invariants failed.
    #[error("math: {0}")]
    Math(&'static str),
    /// The swap needs more steps or tick arrays than a transaction can
    /// hold (see [`STEPS_MAX`](super::STEPS_MAX) and
    /// [`TICK_ARRAYS_MAX`](super::TICK_ARRAYS_MAX)).
    #[error("the swap exceeds {0}")]
    Limit(&'static str),
}
