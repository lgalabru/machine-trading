//! Protocol adapters, each behind a Cargo feature.
//!
//! An adapter maps one protocol onto the [`core`](crate::core) model: its
//! markets become [`Instrument`](crate::core::Instrument)s, its books
//! [`OrderBook`](crate::core::OrderBook)s, its accounts
//! [`Position`](crate::core::Position)s, and its order flow an
//! [`ExecutionDriver`](crate::core::ExecutionDriver). Transactions go through
//! [`chain`](crate::chain).

#[cfg(feature = "phoenix")]
pub mod phoenix;
