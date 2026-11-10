//! Phoenix, the venue (`specs/venues/phoenix.md`): two programs, one venue.
//!
//! | Program | Markets | Module | Feature |
//! |---------|---------|--------|---------|
//! | Phoenix Eternal | perpetuals, cross-margin trader accounts | [`eternal`] | `phoenix` |
//! | Phoenix v1 | spot order books, one seat per trader per market | [`v1`] | `phoenix-v1` |
//!
//! Both trade through an [`ExecutionDriver`](crate::core::ExecutionDriver)
//! and name their instruments `phoenix:<kind>:<SYMBOL>`, on the same
//! [`VENUE`]: `phoenix:perp:SOL` is Eternal's, `phoenix:spot:SOL-USDC` is
//! v1's. What an account holds differs: a perp
//! [`Position`](crate::core::Position) on Eternal,
//! [`SpotBalances`](crate::core::SpotBalances) on v1.

#[cfg(feature = "phoenix")]
pub mod eternal;
#[cfg(feature = "phoenix-v1")]
pub mod v1;

/// The venue every Phoenix instrument trades on, perpetual or spot.
pub const VENUE: &str = "phoenix";
