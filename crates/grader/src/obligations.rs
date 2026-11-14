//! A market maker's quoting obligations, and how a session measures them.
//!
//! A market-making scenario that opts in (its making block's
//! `obligations`, see [`crate::making::Making`]) states three numbers, as
//! designated market-maker programs do: the share of the session's steps
//! the bot must quote both sides (`presence`), how far from fair value each
//! side may be (`max_half_spread_bps`), and the least size each side must
//! show there (`min_size`). Obligations are a duty, not a mandate limit
//! (`specs/mandate.md`, Non-goals): missing them is reported, never a
//! breach.
//!
//! The session samples the bot's resting orders once per step, after its
//! turn and before the next step's takers trade: what they then trade
//! against. Fair value is the scenario's level, the price the account is
//! valued at. A side counts if the bot's orders within the band, summed,
//! reach `min_size`; orders outside the band count for nothing. Steps the
//! session never reached (it ended at a stop-out) count as missed.

use serde::{Deserialize, Serialize};

/// What a market-making scenario obliges the bot to quote.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Obligations {
    /// The share of the session's steps, in (0, 1], at which the bot must
    /// quote both sides within the band at the minimum size.
    pub presence: f64,
    /// The band: how far each side may be from fair value, in basis points
    /// of it.
    pub max_half_spread_bps: f64,
    /// The least the bot's orders within the band must add up to on each
    /// side, in base units.
    pub min_size: f64,
}

/// How far a price may sit past the band's edge and still be in it:
/// prices convert from ticks in floating point.
const BAND_TOLERANCE: f64 = 1e-9;

impl Obligations {
    pub fn validate(&self) -> Result<(), String> {
        if !(self.presence.is_finite() && self.presence > 0.0 && self.presence <= 1.0) {
            return Err(format!(
                "obligations presence {} must be in (0, 1]",
                self.presence
            ));
        }
        if !(self.max_half_spread_bps.is_finite() && self.max_half_spread_bps > 0.0) {
            return Err(format!(
                "obligations max_half_spread_bps {} must be positive",
                self.max_half_spread_bps
            ));
        }
        if !(self.min_size.is_finite() && self.min_size > 0.0) {
            return Err(format!(
                "obligations min_size {} must be positive",
                self.min_size
            ));
        }
        Ok(())
    }

    /// One sample of a trader's resting orders, `(price, size)` in USD and
    /// base units, against fair value `fair_usd`.
    pub fn check(&self, fair_usd: f64, bids: &[(f64, f64)], asks: &[(f64, f64)]) -> Quoting {
        let band = fair_usd * self.max_half_spread_bps / 10_000.0;
        let slack = fair_usd * BAND_TOLERANCE;
        let side = |orders: &[(f64, f64)], within: &dyn Fn(f64) -> bool| {
            if orders.is_empty() {
                return Quoting::OneSided;
            }
            let size: f64 = orders
                .iter()
                .filter(|(price, _)| within(*price))
                .map(|(_, size)| size)
                .sum();
            if size <= 0.0 {
                Quoting::TooWide
            } else if size + self.min_size * BAND_TOLERANCE < self.min_size {
                Quoting::TooSmall
            } else {
                Quoting::TwoSided
            }
        };
        let bid = side(bids, &|price| price >= fair_usd - band - slack);
        let ask = side(asks, &|price| price <= fair_usd + band + slack);
        bid.max(ask)
    }
}

/// What one sample found, worst side first: a side with no order at all,
/// then one with orders but none within the band, then one whose orders
/// within it are too small.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Quoting {
    TwoSided,
    TooSmall,
    TooWide,
    OneSided,
}

/// How a session's bot met its obligations.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ObligationReport {
    pub obligations: Obligations,
    /// The session's steps, sampled or not: what `presence` is a share of.
    pub steps: u64,
    /// Steps at which it quoted both sides within the band at the minimum
    /// size, and, for the rest, what fell short: a side with no order, a
    /// side with orders all outside the band, a side too small within it,
    /// or a step the session never reached.
    pub two_sided: u64,
    pub one_sided: u64,
    pub too_wide: u64,
    pub too_small: u64,
    pub unsampled: u64,
    /// `two_sided` over `steps`.
    pub uptime: f64,
    /// Whether `uptime` reached `presence`.
    pub met: bool,
}

impl ObligationReport {
    /// The report for `samples`, one per step the session sampled, of a
    /// session of `steps` steps.
    pub fn of(obligations: Obligations, steps: u64, samples: &[Quoting]) -> Self {
        let count = |q: Quoting| samples.iter().filter(|s| **s == q).count() as u64;
        let two_sided = count(Quoting::TwoSided);
        let uptime = if steps == 0 {
            0.0
        } else {
            two_sided as f64 / steps as f64
        };
        Self {
            obligations,
            steps,
            two_sided,
            one_sided: count(Quoting::OneSided),
            too_wide: count(Quoting::TooWide),
            too_small: count(Quoting::TooSmall),
            unsampled: steps.saturating_sub(samples.len() as u64),
            uptime,
            met: uptime + BAND_TOLERANCE >= obligations.presence,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn obligations() -> Obligations {
        Obligations {
            presence: 0.75,
            max_half_spread_bps: 20.0,
            min_size: 1.0,
        }
    }

    // Fair value 100: the band is 99.80 to 100.20.
    const FAIR: f64 = 100.0;

    #[test]
    fn quotes_on_both_sides_within_the_band_at_the_size_comply() {
        let o = obligations();
        assert_eq!(
            o.check(FAIR, &[(99.9, 1.0)], &[(100.1, 2.5)]),
            Quoting::TwoSided
        );
        // At the band's edge, and the minimum made up of two orders.
        assert_eq!(
            o.check(FAIR, &[(99.8, 0.4), (99.85, 0.6)], &[(100.2, 1.0)]),
            Quoting::TwoSided
        );
        // A bid above fair is no further from it.
        assert_eq!(
            o.check(FAIR, &[(100.05, 1.0)], &[(100.1, 1.0)]),
            Quoting::TwoSided
        );
    }

    #[test]
    fn a_missing_side_is_one_sided() {
        let o = obligations();
        assert_eq!(o.check(FAIR, &[(99.9, 5.0)], &[]), Quoting::OneSided);
        assert_eq!(o.check(FAIR, &[], &[(100.1, 5.0)]), Quoting::OneSided);
        assert_eq!(o.check(FAIR, &[], &[]), Quoting::OneSided);
        // Worse than any other shortfall on the other side.
        assert_eq!(o.check(FAIR, &[(99.0, 5.0)], &[]), Quoting::OneSided);
    }

    #[test]
    fn a_side_quoted_only_outside_the_band_is_too_wide() {
        let o = obligations();
        assert_eq!(
            o.check(FAIR, &[(99.79, 5.0)], &[(100.1, 1.0)]),
            Quoting::TooWide
        );
        assert_eq!(
            o.check(FAIR, &[(99.9, 1.0)], &[(100.21, 5.0)]),
            Quoting::TooWide
        );
        // Size outside the band doesn't make up for size inside it.
        assert_eq!(
            o.check(FAIR, &[(99.9, 0.5), (99.0, 9.0)], &[(100.1, 1.0)]),
            Quoting::TooSmall
        );
    }

    #[test]
    fn a_side_under_the_minimum_size_is_too_small() {
        let o = obligations();
        assert_eq!(
            o.check(FAIR, &[(99.9, 0.6)], &[(100.1, 1.0)]),
            Quoting::TooSmall
        );
        assert_eq!(
            o.check(FAIR, &[(99.9, 1.0)], &[(100.1, 0.99)]),
            Quoting::TooSmall
        );
        // Too wide on one side is the worse finding.
        assert_eq!(
            o.check(FAIR, &[(99.9, 0.6)], &[(100.3, 1.0)]),
            Quoting::TooWide
        );
    }

    #[test]
    fn uptime_is_a_share_of_every_step_sampled_or_not() {
        use Quoting::*;
        let o = obligations();
        let samples = [TwoSided, TwoSided, TwoSided, OneSided, TooWide, TooSmall];
        let report = ObligationReport::of(o, 8, &samples);
        assert_eq!(
            (
                report.two_sided,
                report.one_sided,
                report.too_wide,
                report.too_small,
                report.unsampled
            ),
            (3, 1, 1, 1, 2)
        );
        assert_eq!(report.uptime, 0.375);
        assert!(!report.met);
        let report = ObligationReport::of(o, 4, &[TwoSided, TwoSided, TwoSided, OneSided]);
        assert_eq!(report.uptime, 0.75);
        assert!(report.met);
    }

    #[test]
    fn obligations_validate_their_numbers() {
        assert_eq!(obligations().validate(), Ok(()));
        for bad in [
            Obligations {
                presence: 0.0,
                ..obligations()
            },
            Obligations {
                presence: 1.5,
                ..obligations()
            },
            Obligations {
                max_half_spread_bps: f64::NAN,
                ..obligations()
            },
            Obligations {
                min_size: -1.0,
                ..obligations()
            },
        ] {
            assert!(bad.validate().is_err(), "{bad:?}");
        }
        let json = r#"{"presence":0.75,"max_half_spread_bps":20,"min_size":1}"#;
        assert_eq!(
            serde_json::from_str::<Obligations>(json).unwrap(),
            obligations()
        );
        assert!(serde_json::from_str::<Obligations>(r#"{"presence":0.75}"#).is_err());
    }
}
