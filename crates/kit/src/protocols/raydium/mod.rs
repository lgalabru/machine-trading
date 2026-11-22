//! Raydium, the venue: its pools are instruments of the `amm` kind
//! (`specs/instruments/amm.md`), named by their account address.
//!
//! | Program | Pools | Module | Feature |
//! |---------|-------|--------|---------|
//! | CLMM `CAMMCzo5YL8w4VFF8KVHrK22GGUsp5VTaW7grrKgrWqK` | concentrated liquidity, ticks | [`clmm`] | `raydium-clmm` |
//!
//! Raydium's constant-product programs (AMM v4, CPMM) are not adapted yet.

#[cfg(feature = "raydium-clmm-venue")]
pub mod clmm;

/// The venue every Raydium instrument trades on.
pub const VENUE: &str = "raydium";
