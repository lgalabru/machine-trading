//! Portfolio: aggregated positions and capital tracking.
//!
//! The portfolio is the top-level view of all trading activity: positions
//! across instruments and venues, the capital behind them, and the aggregate
//! figures the risk layer needs.

use std::collections::HashMap;

use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use crate::core::{
    fill::Fill,
    instrument::InstrumentId,
    position::{FillEffect, Position, PositionError},
};

/// A collection of positions across instruments and venues.
///
/// Capital is in one quote asset (typically USDC); positions quoted in
/// different assets must be converted by the caller before they share a
/// portfolio.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Portfolio {
    /// All positions, keyed by instrument. Flat positions are kept so their
    /// realized PnL stays visible.
    pub positions: HashMap<InstrumentId, Position>,

    /// Total capital allocated to trading (across all venues).
    pub total_capital: Decimal,

    /// Capital not currently deployed. Maintained by the caller: what
    /// "deployed" means differs between spot inventory and perp margin.
    pub available_capital: Decimal,

    /// Cumulative realized PnL across all positions, gross of fees.
    pub realized_pnl: Decimal,

    /// Cumulative fees across all fills applied.
    pub total_fees: Decimal,
}

impl Portfolio {
    /// Creates an empty portfolio with the given starting capital.
    pub fn new(starting_capital: Decimal) -> Self {
        Self {
            positions: HashMap::new(),
            total_capital: starting_capital,
            available_capital: starting_capital,
            realized_pnl: Decimal::ZERO,
            total_fees: Decimal::ZERO,
        }
    }

    /// The position in `instrument`, if one was ever opened.
    pub fn position(&self, instrument: &InstrumentId) -> Option<&Position> {
        self.positions.get(instrument)
    }

    /// Signed quantity held in `instrument` (zero if none).
    pub fn quantity(&self, instrument: &InstrumentId) -> Decimal {
        self.position(instrument)
            .map_or(Decimal::ZERO, |p| p.quantity)
    }

    /// Applies a fill to its instrument's position, opening one if needed,
    /// and updates the portfolio's realized PnL and fees.
    pub fn apply_fill(&mut self, fill: &Fill) -> Result<FillEffect, PositionError> {
        let position = self
            .positions
            .entry(fill.instrument.clone())
            .or_insert_with(|| Position::flat(fill.instrument.clone(), fill.venue.clone()));
        let effect = position.apply(fill)?;
        self.realized_pnl += effect.realized_pnl;
        self.total_fees += fill.fees;
        Ok(effect)
    }

    /// Computes total unrealized PnL across all open positions.
    ///
    /// Requires a function that provides the current mark for each
    /// instrument. Returns `None` if any open instrument's price is
    /// unavailable.
    pub fn unrealized_pnl(
        &self,
        current_price: impl Fn(&InstrumentId) -> Option<Decimal>,
    ) -> Option<Decimal> {
        let mut total = Decimal::ZERO;
        for (id, position) in self.positions.iter().filter(|(_, p)| !p.is_closed()) {
            total += position.unrealized_pnl(current_price(id)?);
        }
        Some(total)
    }

    /// Gross exposure, `Σ |quantity| × mark`, or `None` if any open
    /// instrument's price is unavailable.
    pub fn gross_exposure(
        &self,
        current_price: impl Fn(&InstrumentId) -> Option<Decimal>,
    ) -> Option<Decimal> {
        let mut total = Decimal::ZERO;
        for (id, position) in self.positions.iter().filter(|(_, p)| !p.is_closed()) {
            total += position.market_value(current_price(id)?);
        }
        Some(total)
    }

    /// Equity: capital plus net realized and unrealized PnL.
    pub fn equity(
        &self,
        current_price: impl Fn(&InstrumentId) -> Option<Decimal>,
    ) -> Option<Decimal> {
        Some(
            self.total_capital + self.realized_pnl - self.total_fees
                + self.unrealized_pnl(current_price)?,
        )
    }

    /// Returns the number of open (non-zero quantity) positions.
    pub fn open_position_count(&self) -> usize {
        self.positions.values().filter(|p| !p.is_closed()).count()
    }

    /// Returns the total capital currently deployed in positions.
    pub fn deployed_capital(&self) -> Decimal {
        self.total_capital - self.available_capital
    }
}

/// Summary snapshot of portfolio state for monitoring.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PortfolioSnapshot {
    /// Total capital (deployed + available).
    pub total_capital: Decimal,

    /// Capital not currently in positions.
    pub available_capital: Decimal,

    /// Total deployed in open positions.
    pub deployed_capital: Decimal,

    /// Cumulative realized PnL.
    pub realized_pnl: Decimal,

    /// Current unrealized PnL (if market prices are available).
    pub unrealized_pnl: Option<Decimal>,

    /// Number of open positions.
    pub open_positions: usize,

    /// Current drawdown from peak equity, as a fraction (e.g., 0.05 = 5%).
    pub drawdown: Decimal,
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use rust_decimal_macros::dec;

    use super::*;
    use crate::core::{
        fill::FillId,
        order::{OrderId, Side},
        venue::VenueId,
    };

    fn fill(instrument: &str, side: Side, quantity: Decimal, price: Decimal) -> Fill {
        Fill {
            id: FillId::new(),
            order_id: OrderId::new(),
            instrument: InstrumentId::new(instrument),
            venue: VenueId::new("test"),
            side,
            price,
            quantity,
            fees: dec!(1),
            timestamp: Utc::now(),
            latency: None,
            strategy: None,
        }
    }

    #[test]
    fn fills_build_positions_and_pnl() {
        let mut p = Portfolio::new(dec!(10000));
        p.apply_fill(&fill("a", Side::Buy, dec!(2), dec!(100)))
            .unwrap();
        p.apply_fill(&fill("b", Side::Sell, dec!(1), dec!(50)))
            .unwrap();
        p.apply_fill(&fill("a", Side::Sell, dec!(1), dec!(110)))
            .unwrap();
        assert_eq!(p.open_position_count(), 2);
        assert_eq!(p.quantity(&InstrumentId::new("a")), dec!(1));
        assert_eq!(p.quantity(&InstrumentId::new("b")), dec!(-1));
        assert_eq!(p.realized_pnl, dec!(10));
        assert_eq!(p.total_fees, dec!(3));

        let marks = |id: &InstrumentId| match id.0.as_str() {
            "a" => Some(dec!(120)),
            "b" => Some(dec!(40)),
            _ => None,
        };
        // a: 1 × (120 − 100) = 20; b: 1 × (50 − 40) = 10.
        assert_eq!(p.unrealized_pnl(marks), Some(dec!(30)));
        assert_eq!(p.gross_exposure(marks), Some(dec!(160)));
        assert_eq!(p.equity(marks), Some(dec!(10037)));
        assert_eq!(p.unrealized_pnl(|_| None), None);
    }
}
