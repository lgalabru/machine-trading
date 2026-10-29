//! Interest-rate conventions.
//!
//! Lending protocols quote an **APR** (simple annual rate) and accrue it
//! continuously or per slot; what a depositor actually earns over a year is
//! the **APY**, which includes compounding. The two differ by less than a
//! percentage point at today's rates, but comparisons across venues must use
//! the same convention or they are meaningless. This module is the one place
//! that convention lives.
//!
//! All rates are fractions, not percentages: `0.05` means five percent.

use rust_decimal::{Decimal, MathematicalOps};
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// A yield expressed under both conventions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Yield {
    /// Simple annual rate (no compounding), as a fraction.
    pub apr: Decimal,
    /// Effective annual rate after compounding, as a fraction.
    pub apy: Decimal,
}

/// Errors from rate arithmetic.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum RateError {
    /// The computation left the representable range of [`Decimal`].
    #[error("rate arithmetic overflowed for apr={0}")]
    Overflow(Decimal),
    /// `periods_per_year` must be at least 1.
    #[error("compounding requires at least one period per year")]
    ZeroPeriods,
}

impl Yield {
    /// Zero yield under both conventions.
    pub const ZERO: Yield = Yield {
        apr: Decimal::ZERO,
        apy: Decimal::ZERO,
    };

    /// APY under continuous compounding: `apy = e^apr − 1`.
    ///
    /// Protocols that accrue every second or every slot are, at display
    /// precision, indistinguishable from continuous compounding (the gap is
    /// below `1e-9` for any realistic rate), so this is the default
    /// conversion for on-chain lending rates.
    pub fn from_apr_continuous(apr: Decimal) -> Result<Self, RateError> {
        let apy = apr
            .checked_exp()
            .and_then(|e| e.checked_sub(Decimal::ONE))
            .ok_or(RateError::Overflow(apr))?;
        Ok(Self { apr, apy })
    }

    /// APY under discrete compounding `n` times per year:
    /// `apy = (1 + apr/n)^n − 1`.
    ///
    /// Use this when a venue states its compounding period explicitly
    /// (daily, per epoch). For very large `n` prefer
    /// [`Yield::from_apr_continuous`], which is both faster and exact in the
    /// limit.
    pub fn from_apr_compounded(apr: Decimal, periods_per_year: u32) -> Result<Self, RateError> {
        if periods_per_year == 0 {
            return Err(RateError::ZeroPeriods);
        }
        let n = Decimal::from(periods_per_year);
        let base = Decimal::ONE + apr / n;
        let apy = base
            .checked_powd(n)
            .and_then(|p| p.checked_sub(Decimal::ONE))
            .ok_or(RateError::Overflow(apr))?;
        Ok(Self { apr, apy })
    }

    /// A yield known only as an APR, with the APY left equal to it.
    ///
    /// For benchmarks that are already quoted as effective annual rates, or
    /// where compounding is not applicable (a fixed-term deal).
    pub fn simple(apr: Decimal) -> Self {
        Self { apr, apy: apr }
    }
}

/// Formats a fraction as a percentage with the given number of decimals,
/// e.g. `0.0523` → `"5.23%"`.
pub fn format_pct(rate: Decimal, decimals: u32) -> String {
    let pct = (rate * Decimal::ONE_HUNDRED).round_dp(decimals);
    format!("{pct:.prec$}%", prec = decimals as usize)
}

#[cfg(test)]
mod tests {
    use rust_decimal_macros::dec;

    use super::*;

    fn close(a: Decimal, b: Decimal, tol: Decimal) -> bool {
        (a - b).abs() <= tol
    }

    #[test]
    fn continuous_matches_known_values() {
        let y = Yield::from_apr_continuous(dec!(0.05)).unwrap();
        assert_eq!(y.apr, dec!(0.05));
        // e^0.05 − 1 = 0.051271096...
        assert!(close(y.apy, dec!(0.0512710964), dec!(1e-9)), "{}", y.apy);

        let y = Yield::from_apr_continuous(Decimal::ZERO).unwrap();
        assert_eq!(y, Yield::ZERO);

        let y = Yield::from_apr_continuous(dec!(1)).unwrap();
        assert!(close(y.apy, dec!(1.718281828), dec!(1e-8)), "{}", y.apy);
    }

    #[test]
    fn discrete_compounding_converges_to_continuous() {
        let apr = dec!(0.08);
        let yearly = Yield::from_apr_compounded(apr, 1).unwrap();
        assert_eq!(yearly.apy, apr);

        let monthly = Yield::from_apr_compounded(apr, 12).unwrap();
        // (1 + 0.08/12)^12 − 1 = 0.0829995...
        assert!(
            close(monthly.apy, dec!(0.0829995), dec!(1e-6)),
            "{}",
            monthly.apy
        );

        let daily = Yield::from_apr_compounded(apr, 365).unwrap();
        let continuous = Yield::from_apr_continuous(apr).unwrap();
        assert!(daily.apy < continuous.apy);
        assert!(close(daily.apy, continuous.apy, dec!(1e-5)));
    }

    #[test]
    fn discrete_rejects_zero_periods() {
        assert_eq!(
            Yield::from_apr_compounded(dec!(0.05), 0).unwrap_err(),
            RateError::ZeroPeriods
        );
    }

    #[test]
    fn simple_keeps_both_equal() {
        let y = Yield::simple(dec!(0.07));
        assert_eq!(y.apr, y.apy);
    }

    #[test]
    fn format_percentage() {
        assert_eq!(format_pct(dec!(0.0523), 2), "5.23%");
        assert_eq!(format_pct(dec!(0.05), 2), "5.00%");
        assert_eq!(format_pct(dec!(0), 1), "0.0%");
        assert_eq!(format_pct(dec!(-0.0125), 2), "-1.25%");
        assert_eq!(format_pct(dec!(0.123456), 3), "12.346%");
    }
}
