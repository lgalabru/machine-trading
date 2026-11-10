//! mtx-core: the vocabulary every other mtx crate shares, and no chain code.
//!
//! - **Trading**: an [`Instrument`] (base and quote [`Asset`]s on a
//!   [`venue`], spot or perpetual, tick and lot grid), its [`Order`]s, the
//!   [`Fill`]s they make, and the [`Position`]s fills move.
//! - **Holding and measuring**: what an account holds differs by instrument,
//!   not by how it trades: a [`Holding`] is a perp's [`Position`], a spot
//!   market's [`Holdings`] or a money market's [`LendingBalance`]. An
//!   [`AccountMeter`] reads them, with value, fills and health, from a
//!   venue's own records, for an enforcer.
//! - **The mandate** ([`mandate`], `specs/mandate.md`): what a bot may hold,
//!   and the audit of what a meter measured against it.
//! - **The turn protocol** ([`turns`], `mtx-turn-1`): how the grader and a
//!   bot take turns.
//!
//! `mtx-kit` trades on these types; `mtx-grader` measures and audits with
//! them. Every amount is a [`Decimal`]: conversions to a venue's integer
//! units are exact or they fail.
//!
//! | Module | Purpose |
//! |--------|---------|
//! | [`asset`] | Tokens and exact native ⇄ UI amount conversion |
//! | [`instrument`] | Base/quote markets, spot or perpetual, tick and lot grids |
//! | [`venue`] | Venues and fee schedules |
//! | [`order`] | Orders: buy/sell, limit/market, lifecycle states |
//! | [`fill`] | Executed trades and fill quality (adverse selection) |
//! | [`position`] | Signed positions and fill application (average cost, realized PnL) |
//! | [`holding`] | What an account holds in an instrument, by instrument kind |
//! | [`meter`] | The [`AccountMeter`] trait: what an enforcer reads from a venue |
//! | [`mandate`] | The mandate: universe, limits, and the audit |
//! | [`turns`] | The `mtx-turn-1` protocol |

pub mod asset;
mod error;
pub mod fill;
pub mod holding;
pub mod instrument;
pub mod mandate;
pub mod meter;
pub mod order;
pub mod position;
pub mod turns;
pub mod venue;

pub use asset::{Asset, AssetError, AssetId};
pub use error::{Error, Result};
pub use fill::{Fill, FillId, FillQuality};
pub use holding::{Holding, Holdings, LendingBalance, SpotBalances};
pub use instrument::{Instrument, InstrumentError, InstrumentId, InstrumentKind, Rounding};
pub use mandate::Mandate;
pub use meter::{AccountMeter, Measurement, MeterError, RecordedFill};
pub use order::{Order, OrderId, OrderStatus, OrderType, Side, TimeInForce};
pub use position::{FillEffect, Position, PositionError};
pub use rust_decimal::{self, Decimal};
pub use turns::{Step, Turns};
pub use venue::{FeeSchedule, Liquidity, VenueId, VenueMetadata};
