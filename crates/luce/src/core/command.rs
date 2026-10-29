//! Higher-order execution via the Command pattern.
//!
//! Commands wrap [`ExecutionDriver`] calls with composable behavior:
//! diagnostics, measurement, risk checks, logging. Each command is a
//! self-contained unit that receives an order and a driver and produces
//! a [`CommandOutcome`] enriched with whatever data the command computes.
//!
//! The foundational command is [`MeasuredSubmit`], which snapshots the
//! order book before submission and scores every resulting fill for
//! adverse selection using the fill quality metric from the risk
//! management framework.
//!
//! # Fill quality (adverse selection)
//!
//! ```text
//! Fill Quality = (Executed Price - Midpoint at Order Time) / Spread
//! ```
//!
//! | Score | Interpretation |
//! |-------|----------------|
//! | > 0   | Favorable — filled better than midpoint |
//! | ~ 0   | Neutral — filled near midpoint |
//! | < 0   | Adversely selected — filled worse than midpoint |
//! | < -0.5 | Severely adversely selected |
//!
//! Empirical figures from prediction-market CLOBs (the pattern is general):
//!
//! | Order type | Avg. fill quality |
//! |-----------|-------------------|
//! | Market orders | **-0.72** |
//! | Limit, filled < 1 min | **-0.31** |
//! | Limit, filled 1-10 min | **+0.12** |
//! | Limit, filled > 10 min | **+0.43** |
//!
//! Track this as a rolling average. If it trends negative, the strategy is
//! being exploited by faster or better-informed participants.

use serde::{Deserialize, Serialize};

use crate::core::{
    book::OrderBook,
    execution::{ExecutionDriver, ExecutionError, ExecutionReport},
    fill::FillQuality,
    order::Order,
};

/// The outcome of executing an [`ExecutionCommand`].
///
/// Extends the raw [`ExecutionReport`] with diagnostic data computed by
/// the command — most importantly, fill quality scores for detecting
/// adverse selection.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommandOutcome {
    /// The raw execution report returned by the driver.
    pub report: ExecutionReport,

    /// The order book snapshot taken immediately before submission.
    ///
    /// Captures the market state the command observed when computing fill
    /// quality. `None` if the book was unavailable or the command did not
    /// request it (e.g., [`SubmitOrder`]).
    pub book_at_submission: Option<OrderBook>,

    /// Fill quality score for each fill, in the same order as
    /// [`report.fills`](ExecutionReport::fills).
    ///
    /// An entry is `None` when the score could not be computed — either
    /// the book snapshot was unavailable or the spread was zero.
    pub fill_quality: Vec<Option<FillQuality>>,
}

/// A composable unit of execution behavior.
///
/// Commands decouple *what happens around an order submission* from the
/// submission itself. The [`ExecutionDriver`] handles venue-specific I/O;
/// the command handles the surrounding logic: measurement, risk checks,
/// logging, or any combination.
///
/// # Composing commands
///
/// Commands can delegate to other commands to layer behavior:
///
/// ```ignore
/// struct AuditedSubmit;
///
/// #[async_trait::async_trait]
/// impl ExecutionCommand for AuditedSubmit {
///     async fn execute(
///         &self,
///         driver: &dyn ExecutionDriver,
///         order: &Order,
///     ) -> Result<CommandOutcome, ExecutionError> {
///         log::info!("submitting {:?}", order.id);
///         let outcome = MeasuredSubmit.execute(driver, order).await?;
///         if let Some(fq) = outcome.fill_quality.first().flatten() {
///             log::info!("fill quality: {}", fq.score);
///         }
///         Ok(outcome)
///     }
/// }
/// ```
#[async_trait::async_trait]
pub trait ExecutionCommand: Send + Sync {
    /// Execute the command: submit the order through the driver and return
    /// an enriched [`CommandOutcome`].
    async fn execute(
        &self,
        driver: &dyn ExecutionDriver,
        order: &Order,
    ) -> Result<CommandOutcome, ExecutionError>;
}

/// Submits an order with no additional measurement.
///
/// The baseline command: delegates directly to
/// [`ExecutionDriver::submit_order`] and wraps the result in a
/// [`CommandOutcome`] with no fill quality data or book snapshot.
///
/// Use this when diagnostics are unnecessary — for example, emergency
/// exit orders where latency trumps measurement.
pub struct SubmitOrder;

#[async_trait::async_trait]
impl ExecutionCommand for SubmitOrder {
    async fn execute(
        &self,
        driver: &dyn ExecutionDriver,
        order: &Order,
    ) -> Result<CommandOutcome, ExecutionError> {
        let report = driver.submit_order(order).await?;
        let fill_count = report.fills.len();

        Ok(CommandOutcome {
            report,
            book_at_submission: None,
            fill_quality: vec![None; fill_count],
        })
    }
}

/// Submits an order and measures fill quality on every resulting fill.
///
/// Implements the adverse selection measurement from the risk management
/// framework:
///
/// 1. **Snapshot** the order book immediately before submission.
/// 2. **Submit** the order via the driver.
/// 3. **Score** each fill against the pre-submission midpoint and spread.
///
/// If the book snapshot fails (instrument not found, driver error), the
/// order is still submitted — fill quality entries will be `None`.
///
/// # Example
///
/// ```ignore
/// let outcome = MeasuredSubmit.execute(&driver, &order).await?;
///
/// for (fill, quality) in outcome.report.fills.iter()
///     .zip(outcome.fill_quality.iter())
/// {
///     match quality {
///         Some(fq) if fq.score < Decimal::ZERO =>
///             warn!("adversely selected: score={}", fq.score),
///         Some(fq) =>
///             info!("good fill: score={}", fq.score),
///         None =>
///             info!("fill quality unavailable (zero spread?)"),
///     }
/// }
/// ```
pub struct MeasuredSubmit;

#[async_trait::async_trait]
impl ExecutionCommand for MeasuredSubmit {
    async fn execute(
        &self,
        driver: &dyn ExecutionDriver,
        order: &Order,
    ) -> Result<CommandOutcome, ExecutionError> {
        // 1. Snapshot the book before we touch anything.
        //    A failure here must not prevent submission — we just lose
        //    the fill quality measurement.
        let book = driver.order_book(&order.instrument).await.ok();

        let spread_data = book.as_ref().and_then(|b| {
            let spread = b.spread()?;
            Some((spread.midpoint, spread.absolute))
        });

        // 2. Submit the order.
        let report = driver.submit_order(order).await?;

        // 3. Score each fill against the pre-submission book.
        let fill_quality = report
            .fills
            .iter()
            .map(|fill| {
                let (midpoint, spread) = spread_data?;
                FillQuality::calculate(fill.price, midpoint, spread, fill.side)
            })
            .collect();

        Ok(CommandOutcome {
            report,
            book_at_submission: book,
            fill_quality,
        })
    }
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use rust_decimal::Decimal;
    use rust_decimal_macros::dec;

    use super::*;
    use crate::core::{
        instrument::InstrumentId,
        order::{OrderId, OrderStatus, OrderType, Side, TimeInForce},
        sim::{SimulatedExecution, SimulationConfig},
        venue::VenueId,
    };

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
    // SubmitOrder (bare passthrough)
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn submit_order_passes_through_without_measurement() {
        let driver = SimulatedExecution::new(SimulationConfig::frictionless());
        let order = test_order(Side::Buy, OrderType::Market, None);

        let outcome = SubmitOrder.execute(&driver, &order).await.unwrap();

        assert_eq!(outcome.report.status, OrderStatus::Filled);
        assert!(outcome.book_at_submission.is_none());
        assert_eq!(outcome.fill_quality.len(), 1);
        assert!(outcome.fill_quality[0].is_none());
    }

    // -----------------------------------------------------------------------
    // MeasuredSubmit — market orders (adverse selection expected)
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn measured_market_buy_shows_adverse_selection() {
        // Book: bid=0.48, ask=0.52, midpoint=0.50, spread=0.04
        // Market buy fills at ask + slippage = 0.52 + 0.005 = 0.525
        // raw = (0.525 - 0.50) / 0.04 = 0.625
        // buyer score = -raw = -0.625
        let driver = SimulatedExecution::new(SimulationConfig {
            half_spread: dec!(0.02),
            slippage: dec!(0.005),
            fee_per_unit: Decimal::ZERO,
            default_midpoint: dec!(0.50),
            depth_per_level: dec!(1000),
        });

        let order = test_order(Side::Buy, OrderType::Market, None);
        let outcome = MeasuredSubmit.execute(&driver, &order).await.unwrap();

        assert_eq!(outcome.report.status, OrderStatus::Filled);
        assert!(outcome.book_at_submission.is_some());

        let fq = outcome.fill_quality[0].as_ref().unwrap();
        assert_eq!(fq.midpoint_at_fill, dec!(0.50));
        assert_eq!(fq.spread_at_fill, dec!(0.04));
        assert_eq!(fq.score, dec!(-0.625));
    }

    #[tokio::test]
    async fn measured_market_sell_shows_adverse_selection() {
        // Sell fills at bid - slippage = 0.48 - 0.005 = 0.475
        // raw = (0.475 - 0.50) / 0.04 = -0.625
        // seller score = raw = -0.625
        let driver = SimulatedExecution::new(SimulationConfig {
            half_spread: dec!(0.02),
            slippage: dec!(0.005),
            fee_per_unit: Decimal::ZERO,
            default_midpoint: dec!(0.50),
            depth_per_level: dec!(1000),
        });

        let order = test_order(Side::Sell, OrderType::Market, None);
        let outcome = MeasuredSubmit.execute(&driver, &order).await.unwrap();

        let fq = outcome.fill_quality[0].as_ref().unwrap();
        assert_eq!(fq.score, dec!(-0.625));
    }

    // -----------------------------------------------------------------------
    // MeasuredSubmit — limit orders filling at the book
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn measured_limit_buy_crossing_ask_shows_half_spread_cost() {
        // Limit buy at 0.55 fills at ask = 0.52 (no slippage on limits).
        // raw = (0.52 - 0.50) / 0.04 = 0.5
        // buyer score = -0.5
        let driver = SimulatedExecution::new(SimulationConfig {
            half_spread: dec!(0.02),
            slippage: Decimal::ZERO,
            fee_per_unit: Decimal::ZERO,
            default_midpoint: dec!(0.50),
            depth_per_level: dec!(1000),
        });

        let order = test_order(Side::Buy, OrderType::Limit, Some(dec!(0.55)));
        let outcome = MeasuredSubmit.execute(&driver, &order).await.unwrap();

        let fq = outcome.fill_quality[0].as_ref().unwrap();
        assert_eq!(fq.score, dec!(-0.5));
    }

    #[tokio::test]
    async fn measured_limit_sell_crossing_bid_shows_half_spread_cost() {
        // Limit sell at 0.45 fills at bid = 0.48.
        // raw = (0.48 - 0.50) / 0.04 = -0.5
        // seller score = raw = -0.5
        let driver = SimulatedExecution::new(SimulationConfig {
            half_spread: dec!(0.02),
            slippage: Decimal::ZERO,
            fee_per_unit: Decimal::ZERO,
            default_midpoint: dec!(0.50),
            depth_per_level: dec!(1000),
        });

        let order = test_order(Side::Sell, OrderType::Limit, Some(dec!(0.45)));
        let outcome = MeasuredSubmit.execute(&driver, &order).await.unwrap();

        let fq = outcome.fill_quality[0].as_ref().unwrap();
        assert_eq!(fq.score, dec!(-0.5));
    }

    // -----------------------------------------------------------------------
    // MeasuredSubmit — resting orders (no fills to measure)
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn measured_resting_order_has_empty_fill_quality() {
        let driver = SimulatedExecution::new(SimulationConfig::default());

        // Limit buy far below ask → rests on the book.
        let order = test_order(Side::Buy, OrderType::Limit, Some(dec!(0.01)));
        let outcome = MeasuredSubmit.execute(&driver, &order).await.unwrap();

        assert_eq!(outcome.report.status, OrderStatus::Open);
        assert!(outcome.report.fills.is_empty());
        assert!(outcome.fill_quality.is_empty());
        // Book was still captured for the record.
        assert!(outcome.book_at_submission.is_some());
    }

    // -----------------------------------------------------------------------
    // MeasuredSubmit — edge cases
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn measured_frictionless_yields_none_quality_on_zero_spread() {
        // Frictionless: spread=0, so FillQuality::calculate returns None.
        let driver = SimulatedExecution::new(SimulationConfig::frictionless());

        let order = test_order(Side::Buy, OrderType::Market, None);
        let outcome = MeasuredSubmit.execute(&driver, &order).await.unwrap();

        assert_eq!(outcome.report.status, OrderStatus::Filled);
        assert_eq!(outcome.fill_quality.len(), 1);
        assert!(outcome.fill_quality[0].is_none());
    }

    #[tokio::test]
    async fn measured_submit_captures_correct_book_snapshot() {
        let driver = SimulatedExecution::new(SimulationConfig {
            half_spread: dec!(0.03),
            ..SimulationConfig::default()
        });
        let inst = InstrumentId("test:SOL-PERP".into());
        driver.set_midpoint(inst, dec!(0.70));

        let order = test_order(Side::Buy, OrderType::Market, None);
        let outcome = MeasuredSubmit.execute(&driver, &order).await.unwrap();

        let book = outcome.book_at_submission.unwrap();
        assert_eq!(book.bids[0].price, dec!(0.67));
        assert_eq!(book.asks[0].price, dec!(0.73));
    }

    // -----------------------------------------------------------------------
    // MeasuredSubmit — real-world scenario: tight spread, no slippage
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn tight_spread_limit_buy_at_exact_ask_price() {
        // Tight market: half_spread=0.005, midpoint=0.65
        // bid=0.645, ask=0.655, spread=0.01
        // Limit buy at exactly 0.655 fills at 0.655 (the ask).
        // raw = (0.655 - 0.65) / 0.01 = 0.5
        // buyer score = -0.5
        let driver = SimulatedExecution::new(SimulationConfig {
            half_spread: dec!(0.005),
            slippage: Decimal::ZERO,
            fee_per_unit: Decimal::ZERO,
            default_midpoint: dec!(0.65),
            depth_per_level: dec!(1000),
        });

        let order = test_order(Side::Buy, OrderType::Limit, Some(dec!(0.655)));
        let outcome = MeasuredSubmit.execute(&driver, &order).await.unwrap();

        let fq = outcome.fill_quality[0].as_ref().unwrap();
        assert_eq!(fq.midpoint_at_fill, dec!(0.65));
        assert_eq!(fq.spread_at_fill, dec!(0.01));
        assert_eq!(fq.score, dec!(-0.5));
    }
}
