//! Orca, the venue (`specs/venues/orca.md`): its Whirlpool concentrated
//! liquidity pools ([`whirlpool`], feature `orca-whirlpool`).
//!
//! A pool offers two instruments: `orca:spot:<BASE-QUOTE>`, swapping
//! against the pool, and `orca:amm:<BASE-QUOTE>`, providing liquidity to
//! it. Both name the pool by its pair, as [`whirlpool::POOLS`] lists them.

pub mod whirlpool;

/// The venue every Orca instrument trades on.
pub const VENUE: &str = "orca";
