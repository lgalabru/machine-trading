//! Meteora, the venue (`specs/venues/meteora.md`): its liquidity book pools
//! ([`dlmm`], feature `meteora-dlmm`).
//!
//! A pool offers two instruments: `meteora:spot:<BASE-QUOTE>`, swapping
//! against the pool, and `meteora:amm:<BASE-QUOTE>`, providing liquidity to
//! its bins. Both name the pool by its pair, as [`dlmm::POOLS`] lists them.

pub mod dlmm;

/// The venue every Meteora instrument trades on.
pub const VENUE: &str = "meteora";
