//! Performance of a position that is fed by discrete cash flows.
//!
//! The question `luce status` has to answer is simple to state and easy to
//! get wrong: *given what I put in and when, and what the position is worth
//! now, what annual rate did I actually earn — and what would a benchmark
//! (staking) have paid on the same money over the same time?*
//!
//! # Method
//!
//! Each cash flow `a_i` at time `t_i` has been exposed for `y_i` years
//! (`now − t_i`). The **capital-years** `Σ a_i · y_i` is the money-weighted
//! denominator; the realized APR is
//!
//! ```text
//! realized_apr = pnl / Σ a_i · y_i       where pnl = value_now − Σ a_i
//! ```
//!
//! This is the Modified Dietz method with no intra-period approximation,
//! which is exact because every flow is time-stamped. A withdrawal is a
//! negative flow: the money stops earning from the moment it leaves.
//!
//! The benchmark earns simple interest at `apr_b` on every flow for the time
//! it was exposed: `Σ a_i · apr_b · y_i`. Both sides use the same flows and
//! the same clock, so the comparison is apples to apples.
//!
//! A year is 365 days, matching the convention used by on-chain lending
//! programs when they quote annual rates.

use chrono::{DateTime, Duration, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::core::rates::Yield;

/// Seconds in the 365-day year used for annualization.
pub const SECONDS_PER_YEAR: i64 = 365 * 24 * 60 * 60;

/// A time-stamped movement of capital, in UI units of the position's asset.
///
/// Positive means money entered the position; negative means it left.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CashFlow {
    /// When the flow happened.
    pub at: DateTime<Utc>,
    /// Signed amount.
    pub amount: Decimal,
}

/// Errors from performance measurement.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum PerformanceError {
    /// There is nothing to measure.
    #[error("no cash flows")]
    NoCashFlows,
    /// A flow dated after `now` means the clock or the data is wrong.
    #[error("cash flow at {at} is after the measurement time {now}")]
    FlowInFuture {
        at: DateTime<Utc>,
        now: DateTime<Utc>,
    },
}

/// Measured performance of a position at a point in time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Performance {
    /// `Σ deposits − Σ withdrawals`, in UI units.
    pub net_contributed: Decimal,
    /// Current value of the position, in UI units.
    pub current_value: Decimal,
    /// `current_value − net_contributed`.
    pub pnl: Decimal,
    /// Time from the first cash flow to the measurement time.
    pub elapsed: Duration,
    /// `Σ amount_i × years_i`: the money-weighted exposure.
    pub capital_years: Decimal,
    /// `pnl / capital_years`, when the exposure is positive.
    pub realized_apr: Option<Decimal>,
    /// The realized APR expressed as a continuously compounded APY, when
    /// representable.
    pub realized_apy: Option<Decimal>,
}

/// Years between `from` and `to` as a decimal, using [`SECONDS_PER_YEAR`].
///
/// Sub-second precision is dropped; it is irrelevant at annual scale.
pub fn years_between(from: DateTime<Utc>, to: DateTime<Utc>) -> Decimal {
    let seconds = (to - from).num_seconds();
    Decimal::from(seconds) / Decimal::from(SECONDS_PER_YEAR)
}

fn validate(flows: &[CashFlow], now: DateTime<Utc>) -> Result<(), PerformanceError> {
    if flows.is_empty() {
        return Err(PerformanceError::NoCashFlows);
    }
    if let Some(future) = flows.iter().find(|flow| flow.at > now) {
        return Err(PerformanceError::FlowInFuture { at: future.at, now });
    }
    Ok(())
}

/// Money-weighted exposure `Σ amount_i × years_i`.
pub fn capital_years(flows: &[CashFlow], now: DateTime<Utc>) -> Decimal {
    flows
        .iter()
        .map(|flow| flow.amount * years_between(flow.at, now))
        .sum()
}

/// Measures a position's performance at `now`.
pub fn measure(
    flows: &[CashFlow],
    current_value: Decimal,
    now: DateTime<Utc>,
) -> Result<Performance, PerformanceError> {
    validate(flows, now)?;

    let first = flows.iter().map(|flow| flow.at).min().expect("non-empty");
    let net_contributed: Decimal = flows.iter().map(|flow| flow.amount).sum();
    let pnl = current_value - net_contributed;
    let capital_years = capital_years(flows, now);

    let realized_apr = if capital_years > Decimal::ZERO {
        Some(pnl / capital_years)
    } else {
        None
    };
    let realized_apy =
        realized_apr.and_then(|apr| Yield::from_apr_continuous(apr).ok().map(|y| y.apy));

    Ok(Performance {
        net_contributed,
        current_value,
        pnl,
        elapsed: now - first,
        capital_years,
        realized_apr,
        realized_apy,
    })
}

/// Simple interest a benchmark paying `apr` would have earned on the same
/// flows over the same exposure: `Σ amount_i × apr × years_i`.
pub fn benchmark_pnl(
    flows: &[CashFlow],
    apr: Decimal,
    now: DateTime<Utc>,
) -> Result<Decimal, PerformanceError> {
    validate(flows, now)?;
    Ok(capital_years(flows, now) * apr)
}

/// A position measured side by side with a benchmark rate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Comparison {
    /// The position's own performance.
    pub position: Performance,
    /// The benchmark's simple annual rate.
    pub benchmark_apr: Decimal,
    /// What the benchmark would have earned on the same flows.
    pub benchmark_pnl: Decimal,
    /// `position.pnl − benchmark_pnl`. Positive means the position won.
    pub advantage: Decimal,
}

/// Measures a position and compares it to a benchmark in one step.
pub fn compare(
    flows: &[CashFlow],
    current_value: Decimal,
    benchmark_apr: Decimal,
    now: DateTime<Utc>,
) -> Result<Comparison, PerformanceError> {
    let position = measure(flows, current_value, now)?;
    let benchmark_pnl = benchmark_pnl(flows, benchmark_apr, now)?;
    Ok(Comparison {
        advantage: position.pnl - benchmark_pnl,
        position,
        benchmark_apr,
        benchmark_pnl,
    })
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;
    use rust_decimal_macros::dec;

    use super::*;

    fn t0() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap()
    }

    fn days(n: i64) -> DateTime<Utc> {
        t0() + Duration::days(n)
    }

    fn close(a: Decimal, b: Decimal, tol: Decimal) -> bool {
        (a - b).abs() <= tol
    }

    #[test]
    fn years_between_uses_365_day_year() {
        assert_eq!(years_between(t0(), days(365)), dec!(1));
        assert_eq!(years_between(t0(), days(73)), dec!(0.2));
        assert_eq!(years_between(t0(), t0()), dec!(0));
    }

    #[test]
    fn single_deposit_one_year() {
        let flows = [CashFlow {
            at: t0(),
            amount: dec!(100),
        }];
        let p = measure(&flows, dec!(105), days(365)).unwrap();
        assert_eq!(p.net_contributed, dec!(100));
        assert_eq!(p.pnl, dec!(5));
        assert_eq!(p.capital_years, dec!(100));
        assert_eq!(p.realized_apr, Some(dec!(0.05)));
        assert_eq!(p.elapsed, Duration::days(365));
        let apy = p.realized_apy.unwrap();
        assert!(close(apy, dec!(0.0512710964), dec!(1e-9)), "{apy}");
    }

    #[test]
    fn single_deposit_partial_year_annualizes() {
        let flows = [CashFlow {
            at: t0(),
            amount: dec!(100),
        }];
        // +1 over 73 days (0.2y) → APR 5%.
        let p = measure(&flows, dec!(101), days(73)).unwrap();
        assert_eq!(p.realized_apr, Some(dec!(0.05)));
    }

    #[test]
    fn multiple_flows_are_money_weighted() {
        // 100 for a full year, another 100 for the second half-year.
        let flows = [
            CashFlow {
                at: t0(),
                amount: dec!(100),
            },
            CashFlow {
                at: t0() + Duration::seconds(SECONDS_PER_YEAR / 2),
                amount: dec!(100),
            },
        ];
        // capital-years = 100·1 + 100·0.5 = 150. At 4% APR the pnl is 6.
        let p = measure(&flows, dec!(206), days(365)).unwrap();
        assert_eq!(p.net_contributed, dec!(200));
        assert_eq!(p.pnl, dec!(6));
        assert_eq!(p.capital_years, dec!(150));
        assert_eq!(p.realized_apr, Some(dec!(0.04)));
    }

    #[test]
    fn withdrawal_stops_earning_from_its_date() {
        // Deposit 100 at t0, withdraw 50 at mid-year: exposure = 100·1 − 50·0.5 = 75.
        let flows = [
            CashFlow {
                at: t0(),
                amount: dec!(100),
            },
            CashFlow {
                at: t0() + Duration::seconds(SECONDS_PER_YEAR / 2),
                amount: dec!(-50),
            },
        ];
        let p = measure(&flows, dec!(53), days(365)).unwrap();
        assert_eq!(p.net_contributed, dec!(50));
        assert_eq!(p.pnl, dec!(3));
        assert_eq!(p.capital_years, dec!(75));
        assert_eq!(p.realized_apr, Some(dec!(0.04)));
    }

    #[test]
    fn fully_exited_position_reports_realized_pnl() {
        let flows = [
            CashFlow {
                at: t0(),
                amount: dec!(100),
            },
            CashFlow {
                at: days(365),
                amount: dec!(-104),
            },
        ];
        // Exposure: 100·1 − 104·0 = 100. Value now is 0.
        let p = measure(&flows, dec!(0), days(365)).unwrap();
        assert_eq!(p.net_contributed, dec!(-4));
        assert_eq!(p.pnl, dec!(4));
        assert_eq!(p.realized_apr, Some(dec!(0.04)));
    }

    #[test]
    fn zero_elapsed_has_no_rate() {
        let flows = [CashFlow {
            at: t0(),
            amount: dec!(100),
        }];
        let p = measure(&flows, dec!(100), t0()).unwrap();
        assert_eq!(p.capital_years, dec!(0));
        assert_eq!(p.realized_apr, None);
        assert_eq!(p.realized_apy, None);
        assert_eq!(p.pnl, dec!(0));
    }

    #[test]
    fn negative_pnl_is_reported_as_negative_rate() {
        let flows = [CashFlow {
            at: t0(),
            amount: dec!(100),
        }];
        let p = measure(&flows, dec!(98), days(365)).unwrap();
        assert_eq!(p.realized_apr, Some(dec!(-0.02)));
        let apy = p.realized_apy.unwrap();
        assert!(apy < Decimal::ZERO && apy > dec!(-0.02), "{apy}");
    }

    #[test]
    fn errors_on_empty_and_future_flows() {
        assert_eq!(
            measure(&[], dec!(0), t0()).unwrap_err(),
            PerformanceError::NoCashFlows
        );
        let flows = [CashFlow {
            at: days(1),
            amount: dec!(1),
        }];
        assert_eq!(
            measure(&flows, dec!(1), t0()).unwrap_err(),
            PerformanceError::FlowInFuture {
                at: days(1),
                now: t0()
            }
        );
        assert_eq!(
            benchmark_pnl(&flows, dec!(0.05), t0()).unwrap_err(),
            PerformanceError::FlowInFuture {
                at: days(1),
                now: t0()
            }
        );
    }

    #[test]
    fn benchmark_uses_same_exposure() {
        let flows = [
            CashFlow {
                at: t0(),
                amount: dec!(100),
            },
            CashFlow {
                at: t0() + Duration::seconds(SECONDS_PER_YEAR / 2),
                amount: dec!(100),
            },
        ];
        // capital-years 150 × 7% = 10.5
        assert_eq!(
            benchmark_pnl(&flows, dec!(0.07), days(365)).unwrap(),
            dec!(10.5)
        );

        let c = compare(&flows, dec!(206), dec!(0.07), days(365)).unwrap();
        assert_eq!(c.position.pnl, dec!(6));
        assert_eq!(c.benchmark_pnl, dec!(10.5));
        assert_eq!(c.advantage, dec!(-4.5));
    }
}
