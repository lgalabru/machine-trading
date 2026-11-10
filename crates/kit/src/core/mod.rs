//! The trading model: pure data and pure functions, no I/O. Its vocabulary
//! (assets, instruments, orders, fills, positions, holdings, the meter) is
//! `mtx-core`'s, re-exported here; risk, books, simulation and capital are
//! the kit's own.
//!
//! Two sides share one vocabulary:
//!
//! - **Trading**: an [`Instrument`] (base and quote [`Asset`]s, spot or
//!   perpetual, tick and lot grid) trades on a venue's order book. Orders
//!   become fills, fills move [`Position`]s, positions roll up into a
//!   [`Portfolio`]. Every order passes [`RiskLimits::evaluate`] first and goes
//!   out through an [`ExecutionDriver`].
//! - **Holding and measuring**: what an account holds differs by instrument,
//!   not by how it trades: a [`Holding`] is a perp's [`Position`], a spot
//!   market's [`Holdings`] or a money market's [`LendingBalance`]. An
//!   [`AccountMeter`] reads them, with value, fills and health,
//!   from a venue's own records, for an enforcer.
//! - **Capital**: where money sits when it is not in a trade.
//!   [`Strategy`], [`performance`] and [`rates`] measure yield-bearing
//!   positions (lending, staking) with money-weighted arithmetic.
//!
//! Every amount is a [`Decimal`](rust_decimal::Decimal). Conversions to a
//! venue's integer units ([`Asset::to_native`], [`Instrument::price_to_ticks`],
//! [`Instrument::quantity_to_lots`]) are exact or they fail; rounding, where
//! wanted, is explicit.
//!
//! | Module | Purpose |
//! |--------|---------|
//! | [`asset`] | Tokens and exact native ⇄ UI amount conversion |
//! | [`instrument`] | Base/quote markets, spot or perpetual, tick and lot grids |
//! | [`venue`] | Venues and fee schedules |
//! | [`order`] | Orders: buy/sell, limit/market, lifecycle states |
//! | [`book`] | Order book snapshots, price levels, spread |
//! | [`fill`] | Executed trades and fill quality (adverse selection) |
//! | [`position`] | Signed positions and fill application (average cost, realized PnL) |
//! | [`portfolio`] | Aggregated positions and capital tracking |
//! | [`risk`] | Risk limits, exposure, and pre-trade evaluation |
//! | [`execution`] | The [`ExecutionDriver`] trait |
//! | [`holding`] | What an account holds in an instrument, by instrument kind |
//! | [`meter`] | The [`AccountMeter`] trait: what an enforcer reads from a venue |
//! | [`sim`] | Simulated execution driver for backtesting |
//! | [`command`] | Command pattern wrapping execution with measurement |
//! | [`rates`] | APR/APY conventions and compounding |
//! | [`account`] | Wallets a user controls, and how they are signed for |
//! | [`strategy`] | Long-lived strategies and their cash-flow events |
//! | [`performance`] | Money-weighted performance against a benchmark |

// The vocabulary lives in mtx-core; it is re-exported here so every type the
// trading model uses is under one path.
pub use mtx_core::{asset, fill, holding, instrument, meter, order, position, venue};

pub mod account;
pub mod book;
pub mod command;
pub mod execution;
pub mod performance;
pub mod portfolio;
pub mod rates;
pub mod risk;
pub mod sim;
pub mod strategy;

pub use account::{Account, AccountId, AccountName, SignerBackend};
pub use asset::{Asset, AssetError, AssetId};
pub use book::{OrderBook, PriceLevel, Spread};
pub use command::{CommandOutcome, ExecutionCommand, MeasuredSubmit, SubmitOrder};
pub use execution::{ExecutionDriver, ExecutionError, ExecutionReport};
pub use fill::{Fill, FillId, FillQuality};
pub use holding::{Holding, Holdings, LendingBalance};
pub use instrument::{Instrument, InstrumentError, InstrumentId, InstrumentKind, Rounding};
pub use meter::{AccountMeter, Measurement, MeterError, RecordedFill};
pub use order::{Order, OrderId, OrderStatus, OrderType, Side, TimeInForce};
pub use performance::{CashFlow, Performance};
pub use portfolio::Portfolio;
pub use position::{FillEffect, Position, PositionError};
pub use rates::Yield;
pub use risk::{
    Exposure, RiskCheck, RiskCheckResult, RiskLimits, RiskReport, TradeContext, Verdict,
};
pub use sim::{SimulatedExecution, SimulationConfig};
pub use strategy::{
    ProviderId, Strategy, StrategyEvent, StrategyId, StrategyKind, StrategyStatus, TradeStrategy,
};
pub use venue::{FeeSchedule, Liquidity, VenueId, VenueMetadata};
