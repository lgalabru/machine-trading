//! Trade fills and fill quality measurement.
//!
//! A fill represents a completed execution — your order (or part of it) was
//! matched against a counterparty. Fill quality measures how favorable the
//! execution was, which is the primary metric for detecting adverse selection.
//!
//! Empirical studies of central limit order books show stark differences by
//! order type (figures from prediction-market CLOBs; the pattern is general):
//!
//! | Order type | Avg. fill quality | Interpretation |
//! |-----------|-------------------|----------------|
//! | Market orders | **-0.72** | Severely adversely selected |
//! | Limit, filled < 1 min | **-0.31** | Still adversely selected |
//! | Limit, filled 1-10 min | **+0.12** | Slightly positive |
//! | Limit, filled > 10 min | **+0.43** | Favorable fills |
//!
//! The pattern: **fast fills are bad fills.** If your order executes instantly,
//! someone with better information was happy to take the other side.

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::core::{
    instrument::InstrumentId,
    order::{OrderId, Side},
    venue::VenueId,
};

/// Unique identifier for a fill.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct FillId(pub Uuid);

impl FillId {
    /// Generates a new random fill ID.
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for FillId {
    fn default() -> Self {
        Self::new()
    }
}

/// A completed trade execution.
///
/// Fills are the source of truth for portfolio state and PnL. Every fill
/// updates the position, adjusts available capital, and feeds into the
/// monitoring layer's metrics (fill quality, strategy performance, etc.).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Fill {
    /// Unique fill identifier.
    pub id: FillId,

    /// The order that produced this fill.
    pub order_id: OrderId,

    /// The instrument that was traded.
    pub instrument: InstrumentId,

    /// The venue where the fill occurred.
    pub venue: VenueId,

    /// Buy or sell.
    pub side: Side,

    /// Execution price, in quote units per base unit.
    pub price: Decimal,

    /// Quantity filled, in base units. Always positive; direction is `side`.
    pub quantity: Decimal,

    /// Fees charged by the venue for this fill, in quote units. Negative for
    /// a maker rebate.
    pub fees: Decimal,

    /// When the fill occurred.
    pub timestamp: DateTime<Utc>,

    /// Time elapsed between order submission and this fill.
    ///
    /// Critical for measuring adverse selection — fills that occur within
    /// seconds of order placement are far more likely to be adversely selected.
    pub latency: Option<chrono::Duration>,

    /// The strategy that generated the originating order.
    pub strategy: Option<String>,
}

impl Fill {
    /// Returns the traded notional, `price * quantity`, in quote units.
    pub fn notional(&self) -> Decimal {
        self.price * self.quantity
    }

    /// Returns the total cost of this fill: `price * quantity + fees`.
    ///
    /// For buy fills, this is the capital outflow. For sell fills, this is
    /// the gross proceeds minus fees.
    pub fn total_cost(&self) -> Decimal {
        self.price * self.quantity + self.fees
    }

    /// Returns the net proceeds of this fill: `price * quantity - fees`.
    pub fn net_proceeds(&self) -> Decimal {
        self.price * self.quantity - self.fees
    }
}

/// Measures adverse selection on a single fill.
///
/// Fill quality quantifies how favorable an execution was relative to the
/// market midpoint at the time of the fill. It is the primary diagnostic
/// for whether your strategy is being exploited by faster or better-informed
/// participants.
///
/// Formula: `(executed_price - midpoint_at_fill) / spread_at_fill`
///
/// For buyers: negative means you overpaid (bad), positive means you got a
/// discount (good). For sellers: negative means you sold too cheaply (bad),
/// positive means you got a premium (good).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FillQuality {
    /// The fill quality score, typically in the range `[-1.0, +1.0]`.
    ///
    /// Negative values indicate adverse selection. A rolling average trending
    /// negative means the strategy is being exploited and should be reviewed.
    pub score: Decimal,

    /// The order book midpoint at the time of the fill.
    pub midpoint_at_fill: Decimal,

    /// The bid-ask spread at the time of the fill.
    pub spread_at_fill: Decimal,
}

impl FillQuality {
    /// Calculates fill quality from execution parameters.
    ///
    /// Returns `None` if the spread is zero (cannot divide).
    pub fn calculate(
        executed_price: Decimal,
        midpoint: Decimal,
        spread: Decimal,
        side: Side,
    ) -> Option<Self> {
        if spread.is_zero() {
            return None;
        }

        let raw = (executed_price - midpoint) / spread;

        // For sellers, a higher execution price is good (invert the sign).
        let score = match side {
            Side::Buy => -raw,
            Side::Sell => raw,
        };

        Some(Self {
            score,
            midpoint_at_fill: midpoint,
            spread_at_fill: spread,
        })
    }
}
