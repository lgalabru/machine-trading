//! Order types and lifecycle.
//!
//! Orders are the mechanism for expressing intent to trade. The types here
//! are intentionally asset-agnostic: a limit buy works identically whether it
//! buys SOL against USDC on a spot book or opens a long on a perpetual.
//! Quantities are in base units of the [`Instrument`](crate::core::Instrument),
//! prices in quote units per base unit.

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::core::{instrument::InstrumentId, venue::VenueId};

/// Unique identifier for an order.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct OrderId(pub Uuid);

impl OrderId {
    /// Generates a new random order ID.
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for OrderId {
    fn default() -> Self {
        Self::new()
    }
}

/// Which side of the order book: buying or selling.
///
/// This is the universal market primitive. On a spot market buying acquires
/// the base asset and pays the quote asset. On a perpetual, buying adds to a
/// long position or reduces a short one; selling does the opposite.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Side {
    /// Acquiring the instrument (placing a bid).
    Buy,
    /// Disposing of the instrument (placing an ask).
    Sell,
}

impl Side {
    /// `+1` for a buy, `-1` for a sell: the sign a fill of this side gives a
    /// position's quantity.
    pub fn sign(self) -> Decimal {
        match self {
            Side::Buy => Decimal::ONE,
            Side::Sell => Decimal::NEGATIVE_ONE,
        }
    }

    /// Returns the opposite side.
    pub fn opposite(self) -> Self {
        match self {
            Side::Buy => Side::Sell,
            Side::Sell => Side::Buy,
        }
    }
}

impl std::fmt::Display for Side {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Side::Buy => f.write_str("buy"),
            Side::Sell => f.write_str("sell"),
        }
    }
}

/// How the order should be priced.
///
/// The choice between limit and market orders has a direct impact on fill
/// quality and adverse selection (see [`fill`](crate::core::fill)): market
/// orders are the most adversely selected, patient limit orders the least.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum OrderType {
    /// Execute at a specific price or better.
    ///
    /// The order rests on the book until filled, cancelled, or expired.
    /// Limit orders provide liquidity and generally receive better fills.
    Limit,

    /// Execute immediately at the best available price.
    ///
    /// Market orders consume liquidity and are subject to adverse selection.
    /// Use sparingly — primarily for emergency exits.
    Market,
}

/// How long the order remains active.
///
/// Controls the order's lifetime on the book. Different strategies call for
/// different time-in-force values: market-making uses IOC/FOK to avoid
/// leaving stale quotes, while directional trades use GTC.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
#[non_exhaustive]
pub enum TimeInForce {
    /// Good til cancelled — remains active until explicitly cancelled or filled.
    Gtc,
    /// Immediate or cancel — fill whatever is available now, cancel the rest.
    Ioc,
    /// Fill or kill — fill the entire quantity or cancel entirely.
    Fok,
}

/// Lifecycle state of an order.
///
/// Orders progress through a well-defined state machine. Tracking state
/// transitions is essential for the monitoring layer (fill latency, rejection
/// rates, partial fill frequency).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum OrderStatus {
    /// Submitted to the venue but not yet acknowledged.
    Pending,
    /// Acknowledged and resting on the order book.
    Open,
    /// Some quantity has been filled; the remainder is still on the book.
    PartiallyFilled,
    /// The entire quantity has been filled.
    Filled,
    /// Cancelled by the user or system before being fully filled.
    Cancelled,
    /// Rejected by the venue (insufficient funds, invalid parameters, etc.).
    Rejected,
}

/// An order to buy or sell an instrument.
///
/// Orders are the atomic unit of intent in the trading system. Every order
/// should pass [`RiskLimits::evaluate`](crate::core::risk::RiskLimits::evaluate)
/// before it is handed to an [`ExecutionDriver`](crate::core::ExecutionDriver).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Order {
    /// Unique order identifier (locally generated).
    pub id: OrderId,

    /// The instrument being traded.
    pub instrument: InstrumentId,

    /// The venue where the order will be submitted.
    pub venue: VenueId,

    /// Buy or sell.
    pub side: Side,

    /// Limit or market.
    pub order_type: OrderType,

    /// How long the order remains active.
    pub time_in_force: TimeInForce,

    /// Desired quantity, in base units.
    pub quantity: Decimal,

    /// Limit price, in quote units per base unit. Required for limit orders.
    /// For market orders it is the worst acceptable price (slippage
    /// protection); drivers whose venue requires one reject market orders
    /// without it.
    pub price: Option<Decimal>,

    /// Quantity filled so far.
    pub filled_quantity: Decimal,

    /// Current lifecycle state.
    pub status: OrderStatus,

    /// When the order was created locally.
    pub created_at: DateTime<Utc>,

    /// When the order was last updated (fill, cancel, etc.).
    pub updated_at: DateTime<Utc>,

    /// Venue-assigned order ID, populated after acknowledgement.
    pub venue_order_id: Option<String>,

    /// The strategy that generated this order, if any.
    pub strategy: Option<String>,
}

impl Order {
    fn new(
        instrument: InstrumentId,
        venue: VenueId,
        side: Side,
        order_type: OrderType,
        time_in_force: TimeInForce,
        quantity: Decimal,
        price: Option<Decimal>,
    ) -> Self {
        let now = Utc::now();
        Self {
            id: OrderId::new(),
            instrument,
            venue,
            side,
            order_type,
            time_in_force,
            quantity,
            price,
            filled_quantity: Decimal::ZERO,
            status: OrderStatus::Pending,
            created_at: now,
            updated_at: now,
            venue_order_id: None,
            strategy: None,
        }
    }

    /// A good-til-cancelled limit order at `price`.
    pub fn limit(
        instrument: InstrumentId,
        venue: VenueId,
        side: Side,
        quantity: Decimal,
        price: Decimal,
    ) -> Self {
        Self::new(
            instrument,
            venue,
            side,
            OrderType::Limit,
            TimeInForce::Gtc,
            quantity,
            Some(price),
        )
    }

    /// An immediate-or-cancel market order, filling at no worse than
    /// `worst_price` when given.
    pub fn market(
        instrument: InstrumentId,
        venue: VenueId,
        side: Side,
        quantity: Decimal,
        worst_price: Option<Decimal>,
    ) -> Self {
        Self::new(
            instrument,
            venue,
            side,
            OrderType::Market,
            TimeInForce::Ioc,
            quantity,
            worst_price,
        )
    }

    /// Tags the order with the strategy that generated it.
    pub fn with_strategy(mut self, strategy: impl Into<String>) -> Self {
        self.strategy = Some(strategy.into());
        self
    }

    /// Returns the unfilled quantity remaining on this order.
    pub fn remaining_quantity(&self) -> Decimal {
        self.quantity - self.filled_quantity
    }

    /// Returns `true` if the order is in a terminal state (filled, cancelled,
    /// or rejected).
    pub fn is_terminal(&self) -> bool {
        matches!(
            self.status,
            OrderStatus::Filled | OrderStatus::Cancelled | OrderStatus::Rejected
        )
    }
}

#[cfg(test)]
mod tests {
    use rust_decimal_macros::dec;

    use super::*;

    #[test]
    fn constructors_set_type_and_time_in_force() {
        let id = InstrumentId("phoenix:SOL-PERP".into());
        let venue = VenueId("phoenix".into());
        let limit = Order::limit(id.clone(), venue.clone(), Side::Buy, dec!(2), dec!(150.25));
        assert_eq!(limit.order_type, OrderType::Limit);
        assert_eq!(limit.time_in_force, TimeInForce::Gtc);
        assert_eq!(limit.price, Some(dec!(150.25)));
        assert_eq!(limit.status, OrderStatus::Pending);
        assert_eq!(limit.remaining_quantity(), dec!(2));
        assert!(!limit.is_terminal());

        let market = Order::market(id, venue, Side::Sell, dec!(1), None).with_strategy("mm");
        assert_eq!(market.order_type, OrderType::Market);
        assert_eq!(market.time_in_force, TimeInForce::Ioc);
        assert_eq!(market.price, None);
        assert_eq!(market.strategy.as_deref(), Some("mm"));
    }

    #[test]
    fn side_sign_and_opposite() {
        assert_eq!(Side::Buy.sign(), dec!(1));
        assert_eq!(Side::Sell.sign(), dec!(-1));
        assert_eq!(Side::Buy.opposite(), Side::Sell);
        assert_eq!(Side::Sell.to_string(), "sell");
    }
}
