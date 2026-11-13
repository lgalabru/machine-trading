//! Raydium, the venue (`specs/venues/raydium.md`): its concentrated
//! liquidity pools ([`clmm`], feature `raydium-clmm`).
//!
//! A pool offers two instruments: `raydium:spot:<BASE-QUOTE>`, swapping
//! against the pool, and `raydium:amm:<BASE-QUOTE>`, providing liquidity to
//! it. Both name the pool by its pair, as [`clmm::POOLS`] lists them.

pub mod clmm;

/// The venue every Raydium instrument trades on.
pub const VENUE: &str = "raydium";
