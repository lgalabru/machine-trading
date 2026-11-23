//! Orca, the venue: its pools are instruments of the `amm` kind
//! (`specs/instruments/amm.md`), named by their account address.
//!
//! | Program | Pools | Module | Feature |
//! |---------|-------|--------|---------|
//! | Whirlpool `whirLbMiicVdio4qvUfM5KAg6Ct8VwpYzGff3uctyCc` | concentrated liquidity, ticks | [`whirlpool`] | `orca-whirlpool` |

#[cfg(feature = "orca-whirlpool-venue")]
pub mod whirlpool;

/// The venue every Orca instrument trades on.
pub const VENUE: &str = "orca";
