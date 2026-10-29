//! Order execution abstraction.
//!
//! The [`ExecutionDriver`] trait abstracts over venue-specific order
//! execution, so the same trading logic runs against a live venue or a
//! simulated one.
//!
//! # Implementations
//!
//! - [`SimulatedExecution`](crate::core::sim::SimulatedExecution):
//!   deterministic fills with configurable spread, slippage, and fees. No I/O.
//! - `PhoenixDriver` (feature `phoenix`): Phoenix Eternal perpetuals on
//!   Solana.

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::core::{
    book::OrderBook,
    fill::Fill,
    instrument::InstrumentId,
    order::{Order, OrderId, OrderStatus},
};

/// The result of submitting an order to an execution driver.
///
/// Contains the order's status after submission and any fills the driver
/// observed. A simulated driver resolves fills immediately. A live driver
/// reports what the venue confirmed synchronously; further fills (a resting
/// limit order being hit later) arrive outside this report.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutionReport {
    /// The order ID (matches the submitted order's ID).
    pub order_id: OrderId,

    /// The order's status after submission.
    ///
    /// - [`Filled`](OrderStatus::Filled): fully executed.
    /// - [`Open`](OrderStatus::Open): limit order resting on the book.
    /// - [`Pending`](OrderStatus::Pending): accepted by the venue, outcome not
    ///   observed by the driver (on-chain venues whose fills arrive as
    ///   events).
    /// - [`Rejected`](OrderStatus::Rejected): venue refused the order.
    pub status: OrderStatus,

    /// Fills that resulted from this submission, as far as the driver saw.
    pub fills: Vec<Fill>,

    /// Venue-side reference for the submission: a transaction signature on
    /// chain, an exchange order ID off chain.
    pub venue_reference: Option<String>,
}

/// Errors that can occur during order execution.
///
/// Covers logical errors (instrument not found, order already cancelled) and
/// infrastructure errors (network failure). Venue drivers map their own
/// errors into these variants.
#[derive(Debug, Error)]
pub enum ExecutionError {
    /// The venue or driver rejected the order (insufficient margin, a price
    /// off the tick grid, etc.).
    #[error("order rejected: {reason}")]
    Rejected {
        /// Human-readable rejection reason.
        reason: String,
    },

    /// The requested instrument does not exist on this driver.
    #[error("instrument not found: {0}")]
    InstrumentNotFound(InstrumentId),

    /// The referenced order was not found (already filled, cancelled, or
    /// never submitted to this driver).
    #[error("order not found: {0:?}")]
    OrderNotFound(OrderId),

    /// The driver cannot do this on this venue.
    #[error("unsupported: {0}")]
    Unsupported(String),

    /// A venue-specific error (program error, rate limit, maintenance).
    #[error("venue error: {0}")]
    Venue(String),

    /// Network or connection failure.
    #[error("connection error: {0}")]
    Connection(String),
}

/// Abstraction over order execution on a trading venue.
///
/// This is the boundary between trading logic and venue-specific I/O.
/// Strategies, risk checks, and portfolio management operate above this
/// trait; venue APIs, signing, and wire protocols operate below it.
///
/// The trait is async because real venue drivers perform network I/O. The
/// simulated driver completes immediately but conforms to the same
/// interface, so trading logic does not know whether it runs live or in a
/// backtest.
#[async_trait::async_trait]
pub trait ExecutionDriver: Send + Sync {
    /// Submits an order and returns the execution result.
    ///
    /// **Market orders** fill immediately at the best available price, up to
    /// [`Order::price`] when set. **Limit orders** fill if they cross the book
    /// and otherwise rest with status [`OrderStatus::Open`].
    async fn submit_order(&self, order: &Order) -> Result<ExecutionReport, ExecutionError>;

    /// Cancels one open order.
    ///
    /// Returns the order's final status. Fails if the order was never
    /// submitted to this driver or is already terminal, and with
    /// [`ExecutionError::Unsupported`] on venues that cannot address
    /// individual orders by this crate's [`OrderId`].
    async fn cancel_order(&self, order_id: &OrderId) -> Result<OrderStatus, ExecutionError>;

    /// Cancels every open order in `instrument`.
    async fn cancel_all(&self, instrument: &InstrumentId) -> Result<(), ExecutionError>;

    /// Returns the current order book for an instrument.
    async fn order_book(&self, instrument: &InstrumentId) -> Result<OrderBook, ExecutionError>;
}
