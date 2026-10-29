//! Simulated execution driver for backtesting and unit tests.
//!
//! [`SimulatedExecution`] generates synthetic order books from configurable
//! parameters and fills orders deterministically. No network I/O — everything
//! resolves in-process. This lets you test trading strategies under controlled
//! market conditions: wide spreads, high slippage, variable fees, or the
//! frictionless ideal.
//!
//! # Example
//!
//! ```
//! use luce::core::sim::{SimulatedExecution, SimulationConfig};
//! use luce::core::instrument::InstrumentId;
//! use luce::Decimal;
//!
//! let driver = SimulatedExecution::new(SimulationConfig {
//!     half_spread: Decimal::new(2, 2),    // 0.02
//!     slippage: Decimal::new(5, 3),       // 0.005
//!     fee_per_unit: Decimal::new(1, 2),   // 0.01
//!     default_midpoint: Decimal::new(50, 2), // 0.50
//!     depth_per_level: Decimal::new(1000, 0),
//! });
//!
//! // Override the midpoint for a specific instrument.
//! driver.set_midpoint(InstrumentId("sim:SOL-PERP".into()), Decimal::new(70, 2));
//! ```

use std::{collections::HashMap, sync::Mutex};

use chrono::Utc;
use rust_decimal::Decimal;
use uuid::Uuid;

use crate::core::{
    book::{OrderBook, PriceLevel},
    execution::{ExecutionDriver, ExecutionError, ExecutionReport},
    fill::{Fill, FillId},
    instrument::InstrumentId,
    order::{Order, OrderId, OrderStatus, OrderType, Side},
};

/// Configuration for the simulated execution environment.
///
/// Prices are in quote units per base unit; the defaults (a midpoint of
/// `0.50`) are arbitrary and only matter for tests that do not set one.
///
/// Controls how the simulator generates synthetic order books and computes
/// fill prices. Tweak these parameters to test how strategies behave under
/// different market microstructure conditions.
///
/// # Fields
///
/// | Parameter | Effect |
/// |-----------|--------|
/// | `half_spread` | Distance from midpoint to best bid/ask |
/// | `slippage` | Extra cost on market orders (simulates walking the book) |
/// | `fee_per_unit` | Per-contract/share fee deducted on every fill |
/// | `default_midpoint` | Fallback midpoint when no instrument-specific price is set |
/// | `depth_per_level` | Quantity available at each synthetic price level |
#[derive(Debug, Clone)]
pub struct SimulationConfig {
    /// Half the bid-ask spread, applied symmetrically around the midpoint.
    ///
    /// The simulated best bid is `midpoint - half_spread` and the simulated
    /// best ask is `midpoint + half_spread`. A `half_spread` of `0.02` with
    /// a midpoint of `0.50` produces bid = 0.48, ask = 0.52.
    pub half_spread: Decimal,

    /// Additional price slippage applied to market order fills.
    ///
    /// Simulates the cost of walking through multiple price levels. A buy
    /// market order fills at `ask + slippage`; a sell market order fills at
    /// `bid - slippage`. Set to zero for ideal top-of-book fills.
    pub slippage: Decimal,

    /// Fee per contract or share, deducted from every fill.
    ///
    /// Applied as `fee_per_unit * quantity` on the resulting [`Fill`].
    pub fee_per_unit: Decimal,

    /// Default midpoint price for instruments without an explicit override.
    ///
    /// Use [`SimulatedExecution::set_midpoint`] to configure per-instrument
    /// prices.
    pub default_midpoint: Decimal,

    /// Quantity available at each price level in the synthetic order book.
    ///
    /// Controls how deep the simulated book appears. Does not currently
    /// affect fill logic (all orders fill in full if they cross the spread),
    /// but is reflected in [`ExecutionDriver::order_book`] responses.
    pub depth_per_level: Decimal,
}

impl SimulationConfig {
    /// Creates a zero-friction configuration: no spread, no slippage, no fees.
    ///
    /// Useful for testing pure trading logic in isolation, without any market
    /// microstructure effects. Market orders fill exactly at the midpoint.
    pub fn frictionless() -> Self {
        Self {
            half_spread: Decimal::ZERO,
            slippage: Decimal::ZERO,
            fee_per_unit: Decimal::ZERO,
            default_midpoint: Decimal::new(50, 2),
            depth_per_level: Decimal::new(1000, 0),
        }
    }
}

impl Default for SimulationConfig {
    /// Returns a moderately realistic configuration.
    ///
    /// - Half-spread: 0.02 (4% round-trip on a $1 contract)
    /// - Slippage: 0.005
    /// - Fee: 0.01 per unit
    /// - Default midpoint: 0.50
    /// - Depth: 1000 per level
    fn default() -> Self {
        Self {
            half_spread: Decimal::new(2, 2),
            slippage: Decimal::new(5, 3),
            fee_per_unit: Decimal::new(1, 2),
            default_midpoint: Decimal::new(50, 2),
            depth_per_level: Decimal::new(1000, 0),
        }
    }
}

/// Internal mutable state for the simulated driver.
struct SimulationState {
    /// Per-instrument midpoint prices.
    midpoints: HashMap<InstrumentId, Decimal>,

    /// Orders that are resting on the simulated book (limit orders that
    /// did not cross the spread on submission).
    open_orders: HashMap<OrderId, Order>,
}

/// A simulated execution driver for backtesting and unit tests.
///
/// Implements [`ExecutionDriver`] with fully deterministic, in-process
/// execution. Construct one with a [`SimulationConfig`] that controls
/// spread, slippage, and fees, then submit orders against it exactly as
/// you would against a live venue.
///
/// # Fill semantics
///
/// - **Market buy**: fills at `midpoint + half_spread + slippage`.
/// - **Market sell**: fills at `midpoint - half_spread - slippage`.
/// - **Limit buy at price >= ask**: fills at `ask` (price improvement).
/// - **Limit sell at price <= bid**: fills at `bid` (price improvement).
/// - **Limit order that doesn't cross**: rests with status
///   [`OrderStatus::Open`], cancellable via [`cancel_order`](ExecutionDriver::cancel_order).
///
/// All fills execute for the full requested quantity (no partial fills in
/// this model).
pub struct SimulatedExecution {
    /// The market microstructure parameters.
    config: SimulationConfig,

    /// Mutable state behind a mutex for interior mutability.
    state: Mutex<SimulationState>,
}

impl SimulatedExecution {
    /// Creates a new simulated execution driver with the given configuration.
    pub fn new(config: SimulationConfig) -> Self {
        Self {
            config,
            state: Mutex::new(SimulationState {
                midpoints: HashMap::new(),
                open_orders: HashMap::new(),
            }),
        }
    }

    /// Sets the midpoint price for a specific instrument.
    ///
    /// The synthetic order book will be centered on this price:
    /// `bid = midpoint - half_spread`, `ask = midpoint + half_spread`.
    ///
    /// Instruments without an explicit midpoint use
    /// [`SimulationConfig::default_midpoint`].
    pub fn set_midpoint(&self, instrument: InstrumentId, midpoint: Decimal) {
        self.state
            .lock()
            .unwrap()
            .midpoints
            .insert(instrument, midpoint);
    }

    /// Returns the current midpoint for an instrument, falling back to the
    /// configured default.
    fn midpoint_for(&self, instrument: &InstrumentId) -> Decimal {
        self.state
            .lock()
            .unwrap()
            .midpoints
            .get(instrument)
            .copied()
            .unwrap_or(self.config.default_midpoint)
    }

    /// Builds a synthetic order book for an instrument from the current
    /// midpoint and spread configuration.
    fn synthetic_book(&self, instrument: &InstrumentId) -> OrderBook {
        let mid = self.midpoint_for(instrument);
        let bid_price = mid - self.config.half_spread;
        let ask_price = mid + self.config.half_spread;

        OrderBook {
            instrument: instrument.clone(),
            timestamp: Utc::now(),
            slot: None,
            bids: vec![PriceLevel::new(bid_price, self.config.depth_per_level)],
            asks: vec![PriceLevel::new(ask_price, self.config.depth_per_level)],
        }
    }

    /// Computes the fill price for an order, or `None` if the order cannot
    /// fill (limit order that doesn't cross the spread).
    fn compute_fill_price(&self, order: &Order) -> Option<Decimal> {
        let mid = self.midpoint_for(&order.instrument);
        let best_ask = mid + self.config.half_spread;
        let best_bid = mid - self.config.half_spread;

        match order.order_type {
            OrderType::Market => {
                let price = match order.side {
                    Side::Buy => best_ask + self.config.slippage,
                    Side::Sell => best_bid - self.config.slippage,
                };
                Some(price)
            }
            OrderType::Limit => {
                let limit_price = order.price?;
                match order.side {
                    Side::Buy if limit_price >= best_ask => Some(best_ask),
                    Side::Sell if limit_price <= best_bid => Some(best_bid),
                    _ => None,
                }
            }
        }
    }
}

#[async_trait::async_trait]
impl ExecutionDriver for SimulatedExecution {
    async fn submit_order(&self, order: &Order) -> Result<ExecutionReport, ExecutionError> {
        match self.compute_fill_price(order) {
            Some(price) => {
                let fees = self.config.fee_per_unit * order.quantity;
                let fill = Fill {
                    id: FillId(Uuid::new_v4()),
                    order_id: order.id.clone(),
                    instrument: order.instrument.clone(),
                    venue: order.venue.clone(),
                    side: order.side,
                    price,
                    quantity: order.quantity,
                    fees,
                    timestamp: Utc::now(),
                    latency: None,
                    strategy: order.strategy.clone(),
                };

                Ok(ExecutionReport {
                    order_id: order.id.clone(),
                    status: OrderStatus::Filled,
                    fills: vec![fill],
                    venue_reference: None,
                })
            }
            None if order.order_type == OrderType::Limit => {
                // Limit order didn't cross the spread — rests on the book.
                self.state
                    .lock()
                    .unwrap()
                    .open_orders
                    .insert(order.id.clone(), order.clone());

                Ok(ExecutionReport {
                    order_id: order.id.clone(),
                    status: OrderStatus::Open,
                    fills: vec![],
                    venue_reference: None,
                })
            }
            None => {
                // Market order with no computable fill price (shouldn't
                // happen in normal usage).
                Err(ExecutionError::Rejected {
                    reason: "no fill price available".into(),
                })
            }
        }
    }

    async fn cancel_order(&self, order_id: &OrderId) -> Result<OrderStatus, ExecutionError> {
        let mut state = self.state.lock().unwrap();
        match state.open_orders.remove(order_id) {
            Some(_) => Ok(OrderStatus::Cancelled),
            None => Err(ExecutionError::OrderNotFound(order_id.clone())),
        }
    }

    async fn cancel_all(&self, instrument: &InstrumentId) -> Result<(), ExecutionError> {
        self.state
            .lock()
            .unwrap()
            .open_orders
            .retain(|_, order| &order.instrument != instrument);
        Ok(())
    }

    async fn order_book(&self, instrument: &InstrumentId) -> Result<OrderBook, ExecutionError> {
        Ok(self.synthetic_book(instrument))
    }
}

#[cfg(test)]
mod tests {
    use rust_decimal_macros::dec;

    use super::*;
    use crate::core::{order::TimeInForce, venue::VenueId};

    /// Helper to build a test order with minimal boilerplate.
    fn test_order(side: Side, order_type: OrderType, price: Option<Decimal>) -> Order {
        Order {
            id: OrderId::new(),
            instrument: InstrumentId("test:SOL-PERP".into()),
            venue: VenueId("test".into()),
            side,
            order_type,
            time_in_force: TimeInForce::Gtc,
            quantity: dec!(10),
            price,
            filled_quantity: Decimal::ZERO,
            status: OrderStatus::Pending,
            created_at: Utc::now(),
            updated_at: Utc::now(),
            venue_order_id: None,
            strategy: None,
        }
    }

    // -----------------------------------------------------------------------
    // Market orders
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn market_buy_fills_at_ask_plus_slippage() {
        let driver = SimulatedExecution::new(SimulationConfig {
            half_spread: dec!(0.02),
            slippage: dec!(0.005),
            fee_per_unit: dec!(0.01),
            default_midpoint: dec!(0.50),
            depth_per_level: dec!(1000),
        });

        let order = test_order(Side::Buy, OrderType::Market, None);
        let report = driver.submit_order(&order).await.unwrap();

        assert_eq!(report.status, OrderStatus::Filled);
        assert_eq!(report.fills.len(), 1);
        // ask = 0.50 + 0.02 = 0.52, + slippage 0.005 = 0.525
        assert_eq!(report.fills[0].price, dec!(0.525));
        // fees = 0.01 * 10 = 0.10
        assert_eq!(report.fills[0].fees, dec!(0.10));
    }

    #[tokio::test]
    async fn market_sell_fills_at_bid_minus_slippage() {
        let driver = SimulatedExecution::new(SimulationConfig {
            half_spread: dec!(0.02),
            slippage: dec!(0.005),
            fee_per_unit: dec!(0.01),
            default_midpoint: dec!(0.50),
            depth_per_level: dec!(1000),
        });

        let order = test_order(Side::Sell, OrderType::Market, None);
        let report = driver.submit_order(&order).await.unwrap();

        assert_eq!(report.status, OrderStatus::Filled);
        assert_eq!(report.fills.len(), 1);
        // bid = 0.50 - 0.02 = 0.48, - slippage 0.005 = 0.475
        assert_eq!(report.fills[0].price, dec!(0.475));
    }

    // -----------------------------------------------------------------------
    // Limit orders — crossing the spread
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn limit_buy_above_ask_fills_at_ask() {
        let driver = SimulatedExecution::new(SimulationConfig {
            half_spread: dec!(0.02),
            slippage: Decimal::ZERO,
            fee_per_unit: Decimal::ZERO,
            default_midpoint: dec!(0.50),
            depth_per_level: dec!(1000),
        });

        // Limit buy at 0.55, ask is 0.52 → fills at 0.52 (price improvement).
        let order = test_order(Side::Buy, OrderType::Limit, Some(dec!(0.55)));
        let report = driver.submit_order(&order).await.unwrap();

        assert_eq!(report.status, OrderStatus::Filled);
        assert_eq!(report.fills[0].price, dec!(0.52));
    }

    #[tokio::test]
    async fn limit_sell_below_bid_fills_at_bid() {
        let driver = SimulatedExecution::new(SimulationConfig {
            half_spread: dec!(0.02),
            slippage: Decimal::ZERO,
            fee_per_unit: Decimal::ZERO,
            default_midpoint: dec!(0.50),
            depth_per_level: dec!(1000),
        });

        // Limit sell at 0.45, bid is 0.48 → fills at 0.48 (price improvement).
        let order = test_order(Side::Sell, OrderType::Limit, Some(dec!(0.45)));
        let report = driver.submit_order(&order).await.unwrap();

        assert_eq!(report.status, OrderStatus::Filled);
        assert_eq!(report.fills[0].price, dec!(0.48));
    }

    // -----------------------------------------------------------------------
    // Limit orders — resting (no fill)
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn limit_buy_below_ask_rests_on_book() {
        let driver = SimulatedExecution::new(SimulationConfig::default());

        // Limit buy at 0.49, ask is 0.52 → doesn't cross.
        let order = test_order(Side::Buy, OrderType::Limit, Some(dec!(0.49)));
        let report = driver.submit_order(&order).await.unwrap();

        assert_eq!(report.status, OrderStatus::Open);
        assert!(report.fills.is_empty());
    }

    #[tokio::test]
    async fn limit_sell_above_bid_rests_on_book() {
        let driver = SimulatedExecution::new(SimulationConfig::default());

        // Limit sell at 0.51, bid is 0.48 → doesn't cross.
        let order = test_order(Side::Sell, OrderType::Limit, Some(dec!(0.51)));
        let report = driver.submit_order(&order).await.unwrap();

        assert_eq!(report.status, OrderStatus::Open);
        assert!(report.fills.is_empty());
    }

    // -----------------------------------------------------------------------
    // Cancellation
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn cancel_resting_order_succeeds() {
        let driver = SimulatedExecution::new(SimulationConfig::default());

        let order = test_order(Side::Buy, OrderType::Limit, Some(dec!(0.01)));
        let report = driver.submit_order(&order).await.unwrap();
        assert_eq!(report.status, OrderStatus::Open);

        let status = driver.cancel_order(&order.id).await.unwrap();
        assert_eq!(status, OrderStatus::Cancelled);
    }

    #[tokio::test]
    async fn cancel_unknown_order_returns_error() {
        let driver = SimulatedExecution::new(SimulationConfig::default());

        let bogus_id = OrderId::new();
        let result = driver.cancel_order(&bogus_id).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn cancel_already_cancelled_order_returns_error() {
        let driver = SimulatedExecution::new(SimulationConfig::default());

        let order = test_order(Side::Buy, OrderType::Limit, Some(dec!(0.01)));
        driver.submit_order(&order).await.unwrap();
        driver.cancel_order(&order.id).await.unwrap();

        // Second cancel should fail — order is gone.
        let result = driver.cancel_order(&order.id).await;
        assert!(result.is_err());
    }

    // -----------------------------------------------------------------------
    // Synthetic order book
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn order_book_reflects_midpoint_and_spread() {
        let driver = SimulatedExecution::new(SimulationConfig {
            half_spread: dec!(0.03),
            ..SimulationConfig::default()
        });
        let inst = InstrumentId("test:BTC-PERP".into());
        driver.set_midpoint(inst.clone(), dec!(150.00));

        let book = driver.order_book(&inst).await.unwrap();

        assert_eq!(book.bids[0].price, dec!(149.97));
        assert_eq!(book.asks[0].price, dec!(150.03));
        assert_eq!(book.bids[0].quantity, Some(dec!(1000)));
    }

    #[tokio::test]
    async fn cancel_all_clears_only_that_instrument() {
        let driver = SimulatedExecution::new(SimulationConfig::default());
        let resting = test_order(Side::Buy, OrderType::Limit, Some(dec!(0.01)));
        let mut other = test_order(Side::Buy, OrderType::Limit, Some(dec!(0.01)));
        other.instrument = InstrumentId("test:BTC-PERP".into());
        driver.submit_order(&resting).await.unwrap();
        driver.submit_order(&other).await.unwrap();

        driver.cancel_all(&resting.instrument).await.unwrap();
        assert!(driver.cancel_order(&resting.id).await.is_err());
        assert_eq!(
            driver.cancel_order(&other.id).await.unwrap(),
            OrderStatus::Cancelled
        );
    }

    // -----------------------------------------------------------------------
    // Frictionless mode
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn frictionless_fills_at_midpoint_with_zero_fees() {
        let driver = SimulatedExecution::new(SimulationConfig::frictionless());
        let inst = InstrumentId("test:SOL-PERP".into());
        driver.set_midpoint(inst, dec!(0.70));

        let order = test_order(Side::Buy, OrderType::Market, None);
        let report = driver.submit_order(&order).await.unwrap();

        assert_eq!(report.fills[0].price, dec!(0.70));
        assert_eq!(report.fills[0].fees, Decimal::ZERO);
    }

    // -----------------------------------------------------------------------
    // Per-instrument midpoint override
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn per_instrument_midpoint_overrides_default() {
        let driver = SimulatedExecution::new(SimulationConfig {
            half_spread: Decimal::ZERO,
            slippage: Decimal::ZERO,
            fee_per_unit: Decimal::ZERO,
            default_midpoint: dec!(0.50),
            depth_per_level: dec!(1000),
        });

        let custom = InstrumentId("test:CUSTOM".into());
        driver.set_midpoint(custom.clone(), dec!(0.85));

        // Order on the custom instrument should use 0.85 midpoint.
        let mut order = test_order(Side::Buy, OrderType::Market, None);
        order.instrument = custom;
        let report = driver.submit_order(&order).await.unwrap();
        assert_eq!(report.fills[0].price, dec!(0.85));

        // Order on the default instrument should use 0.50.
        let default_order = test_order(Side::Buy, OrderType::Market, None);
        let report = driver.submit_order(&default_order).await.unwrap();
        assert_eq!(report.fills[0].price, dec!(0.50));
    }
}
