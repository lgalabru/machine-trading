//! OpenBook, the venue (`specs/venues/openbook.md`).
//!
//! | Program | Markets | Module | Feature |
//! |---------|---------|--------|---------|
//! | OpenBook v2 | spot order books, open orders accounts | [`v2`] | `openbook-v2` |
//!
//! Its instruments are `openbook:spot:<SYMBOL>`, on [`VENUE`]. An account
//! holds [`SpotBalances`](crate::core::SpotBalances), and trades through an
//! [`ExecutionDriver`](crate::core::ExecutionDriver).

#[cfg(feature = "openbook-v2")]
pub mod v2;

/// The venue every OpenBook instrument trades on.
pub const VENUE: &str = "openbook";
