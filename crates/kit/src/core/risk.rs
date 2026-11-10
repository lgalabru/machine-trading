//! Risk management: limits, exposure tracking, and pre-trade checks.
//!
//! Risk management follows one rule: **survive long enough for your edge to
//! compound.** A strategy with positive expected value is worthless if a
//! single bad day wipes out the account. Everything here is about staying in
//! the game.
//!
//! Every order should pass [`RiskLimits::evaluate`] before submission. The
//! result is a [`RiskReport`]: one [`RiskCheck`] per limit, each
//! [`Pass`](RiskCheckResult::Pass), [`Warn`](RiskCheckResult::Warn) or
//! [`Reject`](RiskCheckResult::Reject), and a combined [`Verdict`]. Any
//! rejection blocks the order. The same vocabulary is used by the lending
//! side ([`crate::lending::risk`]).
//!
//! # Reducing is always allowed
//!
//! A limit exists to stop risk from growing. An order that brings the
//! position closer to flat never makes a breach worse, so it passes the
//! position and loss checks even when the account is already over a limit:
//! a bot past its drawdown limit must still be able to close out.

use std::collections::HashMap;

use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use crate::core::order::Side;

/// Configurable risk limits for the trading system.
///
/// Fractions are of total capital. The absolute limits are optional; they
/// match how a venue or a competition states its rules ("never hold more
/// than 50 SOL", "never lose more than $500 from the peak").
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RiskLimits {
    /// Maximum single position notional as a fraction of total capital.
    ///
    /// Hard ceiling that overrides any sizing model. Typical starting value:
    /// `0.05` (5% of bankroll per position).
    pub max_position_pct: Decimal,

    /// Kelly fraction multiplier for sizing models.
    ///
    /// Full Kelly assumes perfect edge estimates, which never exist.
    /// Fractional Kelly (0.25–0.5) gives a large safety margin at a modest
    /// cost to growth. Not checked by [`evaluate`](Self::evaluate); sizing
    /// code reads it.
    pub kelly_fraction: Decimal,

    /// Maximum loss allowed in a single day, as a fraction of total capital.
    pub max_daily_loss_pct: Decimal,

    /// Maximum drawdown from peak equity, as a fraction of the peak.
    pub max_drawdown_pct: Decimal,

    /// Maximum total exposure (deployed capital) as a fraction of capital.
    pub max_total_exposure_pct: Decimal,

    /// Maximum exposure per correlated theme, as a fraction of capital.
    ///
    /// Positions driven by the same underlying factor (several SOL
    /// ecosystem tokens, say) should be treated as one large position.
    pub max_theme_exposure_pct: Decimal,

    /// Largest position, long or short, in base units of the instrument.
    pub max_position_quantity: Option<Decimal>,

    /// Largest allowed fall of equity below its peak, in quote units.
    pub max_drawdown_value: Option<Decimal>,

    /// Fraction of a limit at which a check warns before it rejects.
    /// `0.8` warns from 80% of the limit.
    pub warn_fraction: Decimal,
}

impl Default for RiskLimits {
    /// Conservative default limits suitable for initial deployment.
    fn default() -> Self {
        Self {
            max_position_pct: Decimal::new(5, 2),        // 5%
            kelly_fraction: Decimal::new(25, 2),         // 0.25 (quarter Kelly)
            max_daily_loss_pct: Decimal::new(3, 2),      // 3%
            max_drawdown_pct: Decimal::new(15, 2),       // 15%
            max_total_exposure_pct: Decimal::new(80, 2), // 80%
            max_theme_exposure_pct: Decimal::new(20, 2), // 20%
            max_position_quantity: None,
            max_drawdown_value: None,
            warn_fraction: Decimal::new(80, 2), // 80%
        }
    }
}

impl RiskLimits {
    /// A fraction limit with this value is off: [`evaluate`](Self::evaluate)
    /// skips its check.
    pub const OFF: Decimal = Decimal::MAX;

    /// Only the absolute limits, each optional; every fraction limit is
    /// [`OFF`](Self::OFF). For rules stated in units ("hold at most 50 SOL",
    /// "lose at most $500 from the peak") rather than as fractions.
    pub fn absolute(
        max_position_quantity: Option<Decimal>,
        max_drawdown_value: Option<Decimal>,
    ) -> Self {
        Self {
            max_position_pct: Self::OFF,
            max_daily_loss_pct: Self::OFF,
            max_drawdown_pct: Self::OFF,
            max_total_exposure_pct: Self::OFF,
            max_theme_exposure_pct: Self::OFF,
            max_position_quantity,
            max_drawdown_value,
            ..Self::default()
        }
    }
}

/// Real-time exposure and drawdown tracking.
///
/// Updated on every fill, position change, and mark. The live state the risk
/// layer checks against [`RiskLimits`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Exposure {
    /// Total capital currently deployed in open positions (gross notional).
    pub deployed: Decimal,

    /// Capital available for new positions.
    pub available: Decimal,

    /// Current drawdown from peak equity, as a fraction of the peak.
    pub drawdown: Decimal,

    /// Peak equity value (high-water mark). Only ever increases.
    pub peak_equity: Decimal,

    /// Current equity.
    pub equity: Decimal,

    /// Today's realized + unrealized PnL.
    pub daily_pnl: Decimal,

    /// Aggregate exposure grouped by theme.
    pub theme_exposures: HashMap<String, Decimal>,
}

impl Exposure {
    /// Records a new equity value, raising the peak and recomputing the
    /// drawdown fraction.
    pub fn mark_equity(&mut self, equity: Decimal) {
        self.equity = equity;
        self.peak_equity = self.peak_equity.max(equity);
        self.drawdown = if self.peak_equity > Decimal::ZERO {
            (self.peak_equity - equity).max(Decimal::ZERO) / self.peak_equity
        } else {
            Decimal::ZERO
        };
    }

    /// How far equity is below its peak, in quote units.
    pub fn drawdown_value(&self) -> Decimal {
        (self.peak_equity - self.equity).max(Decimal::ZERO)
    }
}

/// Result of a pre-trade risk check.
///
/// Any `Reject` blocks the order. `Warn` allows it but flags it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum RiskCheckResult {
    /// The check passed.
    Pass,

    /// Allowed, but close to a limit or otherwise worth attention.
    Warn {
        /// Human-readable explanation of the warning.
        reason: String,
    },

    /// Blocked by a limit. The order must not be submitted.
    Reject {
        /// Human-readable explanation of which limit was breached.
        reason: String,
    },
}

impl RiskCheckResult {
    /// `true` for [`RiskCheckResult::Reject`].
    pub fn is_reject(&self) -> bool {
        matches!(self, Self::Reject { .. })
    }

    /// `true` for [`RiskCheckResult::Warn`].
    pub fn is_warn(&self) -> bool {
        matches!(self, Self::Warn { .. })
    }
}

/// A single entry in the pre-trade risk checklist.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RiskCheck {
    /// What is being checked (e.g. `"position_quantity"`, `"drawdown"`).
    pub name: String,

    /// The result of this check.
    pub result: RiskCheckResult,

    /// The value of the metric being checked.
    pub current_value: Decimal,

    /// The limit it was checked against.
    pub limit_value: Decimal,
}

impl RiskCheck {
    /// Builds a check.
    pub fn new(
        name: impl Into<String>,
        result: RiskCheckResult,
        current_value: Decimal,
        limit_value: Decimal,
    ) -> Self {
        Self {
            name: name.into(),
            result,
            current_value,
            limit_value,
        }
    }
}

/// Overall outcome of a set of checks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    /// Every check passed.
    Clear,
    /// At least one warning, no rejection. Proceeding is the caller's call.
    Warnings,
    /// At least one rejection. Must not proceed.
    Blocked,
}

impl Verdict {
    /// The verdict for a set of checks.
    pub fn of(checks: &[RiskCheck]) -> Self {
        if checks.iter().any(|c| c.result.is_reject()) {
            Self::Blocked
        } else if checks.iter().any(|c| c.result.is_warn()) {
            Self::Warnings
        } else {
            Self::Clear
        }
    }
}

impl std::fmt::Display for Verdict {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Clear => f.write_str("clear"),
            Self::Warnings => f.write_str("warnings"),
            Self::Blocked => f.write_str("blocked"),
        }
    }
}

/// Checks and their combined verdict.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RiskReport {
    /// Individual checks, in evaluation order.
    pub checks: Vec<RiskCheck>,
    /// Combined verdict.
    pub verdict: Verdict,
}

impl RiskReport {
    /// Combines checks into a report.
    pub fn from_checks(checks: Vec<RiskCheck>) -> Self {
        let verdict = Verdict::of(&checks);
        Self { checks, verdict }
    }

    /// `true` unless the verdict is [`Verdict::Blocked`].
    pub fn allows_proceeding(&self) -> bool {
        self.verdict != Verdict::Blocked
    }

    /// Checks that did not pass.
    pub fn findings(&self) -> impl Iterator<Item = &RiskCheck> {
        self.checks
            .iter()
            .filter(|c| !matches!(c.result, RiskCheckResult::Pass))
    }

    /// The check called `name`, if it ran.
    pub fn check(&self, name: &str) -> Option<&RiskCheck> {
        self.checks.iter().find(|c| c.name == name)
    }
}

/// A proposed trade and the state it would change, for
/// [`RiskLimits::evaluate`]. Prices and values are in the quote asset.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TradeContext<'a> {
    /// Direction of the proposed trade.
    pub side: Side,
    /// Quantity of the proposed trade, in base units (positive).
    pub quantity: Decimal,
    /// Price the trade is expected to fill at (the limit, or the mark).
    pub price: Decimal,
    /// Signed quantity currently held in the instrument.
    pub position: Decimal,
    /// Total capital the percentage limits are fractions of.
    pub capital: Decimal,
    /// Live exposure and drawdown.
    pub exposure: &'a Exposure,
    /// The instrument's theme, if it belongs to one.
    pub theme: Option<&'a str>,
}

impl TradeContext<'_> {
    fn position_after(&self) -> Decimal {
        self.position + self.side.sign() * self.quantity
    }

    /// `true` if the trade leaves the position no larger and on the same
    /// side (or flat): it can only reduce risk.
    pub fn is_reducing(&self) -> bool {
        let after = self.position_after();
        after.abs() <= self.position.abs()
            && (after.is_zero() || after.is_sign_negative() == self.position.is_sign_negative())
    }
}

impl RiskLimits {
    /// Evaluates a proposed trade against every limit.
    ///
    /// Checks, in order:
    ///
    /// | Check | Value | Limit |
    /// |-------|-------|-------|
    /// | `position_quantity` | `|position after|`, base units | `max_position_quantity` (if set) |
    /// | `position_size` | `|position after| × price / capital` | `max_position_pct` |
    /// | `total_exposure` | `(deployed + added notional) / capital` | `max_total_exposure_pct` |
    /// | `theme_exposure` | `(theme exposure + added notional) / capital` | `max_theme_exposure_pct` (if a theme is given) |
    /// | `daily_loss` | `max(0, −daily_pnl) / capital` | `max_daily_loss_pct` |
    /// | `drawdown` | `exposure.drawdown` | `max_drawdown_pct` |
    /// | `drawdown_value` | `peak − equity` | `max_drawdown_value` (if set) |
    ///
    /// A value above its limit rejects; at or above `warn_fraction` of it,
    /// warns. A [reducing](TradeContext::is_reducing) trade passes every
    /// check, with a warning where the account is already over a limit.
    /// Percentage checks need positive capital; with none, they reject. A
    /// fraction limit set to [`RiskLimits::OFF`] is skipped.
    pub fn evaluate(&self, ctx: &TradeContext<'_>) -> RiskReport {
        let reducing = ctx.is_reducing();
        let after = ctx.position_after().abs();
        let added_notional = if reducing {
            Decimal::ZERO
        } else {
            (after - ctx.position.abs()).max(Decimal::ZERO) * ctx.price
        };
        let mut checks = Vec::with_capacity(7);
        let gate = |name: &str, value: Decimal, limit: Decimal, what: &str| {
            self.gate(name, value, limit, reducing, what)
        };

        if let Some(max) = self.max_position_quantity {
            checks.push(gate(
                "position_quantity",
                after,
                max,
                "position size (base units)",
            ));
        }

        let fraction = |value: Decimal| (ctx.capital > Decimal::ZERO).then(|| value / ctx.capital);
        let mut pct_check =
            |name: &str, value: Decimal, limit: Decimal, what: &str| match fraction(value) {
                _ if limit == Self::OFF => {}
                Some(f) => checks.push(gate(name, f, limit, what)),
                None => checks.push(RiskCheck::new(
                    name,
                    RiskCheckResult::Reject {
                        reason: format!(
                            "{what}: capital is {}, so no fraction of it is allowed",
                            ctx.capital
                        ),
                    },
                    value,
                    limit,
                )),
            };
        pct_check(
            "position_size",
            after * ctx.price,
            self.max_position_pct,
            "position notional / capital",
        );
        pct_check(
            "total_exposure",
            ctx.exposure.deployed + added_notional,
            self.max_total_exposure_pct,
            "total exposure / capital",
        );
        if let Some(theme) = ctx.theme {
            let current = ctx
                .exposure
                .theme_exposures
                .get(theme)
                .copied()
                .unwrap_or_default();
            pct_check(
                "theme_exposure",
                current + added_notional,
                self.max_theme_exposure_pct,
                &format!("exposure to theme {theme:?} / capital"),
            );
        }
        pct_check(
            "daily_loss",
            (-ctx.exposure.daily_pnl).max(Decimal::ZERO),
            self.max_daily_loss_pct,
            "today's loss / capital",
        );

        if self.max_drawdown_pct != Self::OFF {
            checks.push(gate(
                "drawdown",
                ctx.exposure.drawdown,
                self.max_drawdown_pct,
                "drawdown from peak",
            ));
        }
        if let Some(max) = self.max_drawdown_value {
            checks.push(gate(
                "drawdown_value",
                ctx.exposure.drawdown_value(),
                max,
                "drawdown from peak (quote units)",
            ));
        }

        RiskReport::from_checks(checks)
    }

    fn gate(
        &self,
        name: &str,
        value: Decimal,
        limit: Decimal,
        reducing: bool,
        what: &str,
    ) -> RiskCheck {
        let result = if value > limit {
            if reducing {
                RiskCheckResult::Warn {
                    reason: format!(
                        "{what} is {value}, over the limit of {limit}; allowed because the trade reduces risk"
                    ),
                }
            } else {
                RiskCheckResult::Reject {
                    reason: format!("{what} would be {value}, over the limit of {limit}"),
                }
            }
        } else if !reducing && limit > Decimal::ZERO && value >= limit * self.warn_fraction {
            RiskCheckResult::Warn {
                reason: format!(
                    "{what} would be {value}, within {} of the limit of {limit}",
                    Decimal::ONE - self.warn_fraction
                ),
            }
        } else {
            RiskCheckResult::Pass
        };
        RiskCheck::new(name, result, value, limit)
    }
}

#[cfg(test)]
mod tests {
    use rust_decimal_macros::dec;

    use super::*;

    fn limits() -> RiskLimits {
        RiskLimits {
            max_position_pct: dec!(0.5),
            max_total_exposure_pct: dec!(1),
            max_theme_exposure_pct: dec!(0.6),
            max_daily_loss_pct: dec!(0.1),
            max_drawdown_pct: dec!(0.2),
            max_position_quantity: Some(dec!(10)),
            max_drawdown_value: Some(dec!(500)),
            ..RiskLimits::default()
        }
    }

    fn exposure() -> Exposure {
        let mut e = Exposure {
            deployed: dec!(0),
            available: dec!(10000),
            ..Exposure::default()
        };
        e.mark_equity(dec!(10000));
        e
    }

    fn ctx<'a>(
        side: Side,
        quantity: Decimal,
        position: Decimal,
        exposure: &'a Exposure,
    ) -> TradeContext<'a> {
        TradeContext {
            side,
            quantity,
            price: dec!(100),
            position,
            capital: dec!(10000),
            exposure,
            theme: None,
        }
    }

    fn result<'a>(r: &'a RiskReport, name: &str) -> &'a RiskCheckResult {
        &r.check(name)
            .unwrap_or_else(|| panic!("no check {name}"))
            .result
    }

    #[test]
    fn small_trade_is_clear() {
        let e = exposure();
        let r = limits().evaluate(&ctx(Side::Buy, dec!(2), dec!(0), &e));
        assert_eq!(
            r.verdict,
            Verdict::Clear,
            "{:?}",
            r.findings().collect::<Vec<_>>()
        );
        let names: Vec<&str> = r.checks.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "position_quantity",
                "position_size",
                "total_exposure",
                "daily_loss",
                "drawdown",
                "drawdown_value"
            ]
        );
        assert_eq!(r.check("position_size").unwrap().current_value, dec!(0.02));
    }

    #[test]
    fn position_quantity_rejects_over_and_warns_near_the_limit() {
        let e = exposure();
        let r = limits().evaluate(&ctx(Side::Sell, dec!(4), dec!(-7), &e));
        assert!(result(&r, "position_quantity").is_reject());
        assert_eq!(
            r.check("position_quantity").unwrap().current_value,
            dec!(11)
        );
        assert_eq!(r.verdict, Verdict::Blocked);
        assert!(!r.allows_proceeding());

        let r = limits().evaluate(&ctx(Side::Buy, dec!(8), dec!(0), &e));
        assert!(result(&r, "position_quantity").is_warn());
        assert_eq!(r.verdict, Verdict::Warnings);
        assert!(r.allows_proceeding());
    }

    #[test]
    fn notional_limits_use_capital() {
        let mut e = exposure();
        e.deployed = dec!(9500);
        // 6 × 100 = 600 notional: 6% position, but 101% total exposure.
        let r = limits().evaluate(&ctx(Side::Buy, dec!(6), dec!(0), &e));
        assert!(matches!(result(&r, "position_size"), RiskCheckResult::Pass));
        assert!(result(&r, "total_exposure").is_reject());

        let mut no_capital = ctx(Side::Buy, dec!(1), dec!(0), &e);
        no_capital.capital = dec!(0);
        let r = limits().evaluate(&no_capital);
        assert!(result(&r, "position_size").is_reject());
    }

    #[test]
    fn theme_exposure_is_checked_when_a_theme_is_given() {
        let mut e = exposure();
        e.theme_exposures.insert("sol".into(), dec!(5500));
        let mut c = ctx(Side::Buy, dec!(6), dec!(0), &e);
        c.theme = Some("sol");
        let r = limits().evaluate(&c);
        assert!(result(&r, "theme_exposure").is_reject());
    }

    #[test]
    fn losses_block_new_risk_but_not_reductions() {
        let mut e = exposure();
        e.mark_equity(dec!(9400)); // 600 below the peak: over the 500 limit.
        e.daily_pnl = dec!(-600);
        assert_eq!(e.drawdown_value(), dec!(600));
        assert_eq!(e.drawdown, dec!(0.06));

        let open = limits().evaluate(&ctx(Side::Buy, dec!(1), dec!(3), &e));
        assert!(result(&open, "drawdown_value").is_reject());
        assert!(matches!(result(&open, "daily_loss"), RiskCheckResult::Pass));

        let close = limits().evaluate(&ctx(Side::Sell, dec!(3), dec!(3), &e));
        assert!(result(&close, "drawdown_value").is_warn());
        assert!(close.allows_proceeding());
    }

    #[test]
    fn reducing_detects_flips_as_new_risk() {
        let e = exposure();
        assert!(ctx(Side::Sell, dec!(2), dec!(3), &e).is_reducing());
        assert!(ctx(Side::Sell, dec!(3), dec!(3), &e).is_reducing());
        assert!(
            !ctx(Side::Sell, dec!(7), dec!(3), &e).is_reducing(),
            "flip to -4"
        );
        assert!(!ctx(Side::Buy, dec!(1), dec!(0), &e).is_reducing());
        assert!(ctx(Side::Buy, dec!(1), dec!(-1), &e).is_reducing());

        // A flip past the quantity limit is rejected even though it starts
        // by reducing.
        let r = limits().evaluate(&ctx(Side::Sell, dec!(14), dec!(3), &e));
        assert!(result(&r, "position_quantity").is_reject());
    }

    #[test]
    fn absolute_limits_skip_fraction_checks() {
        let limits = RiskLimits::absolute(Some(dec!(50)), Some(dec!(500)));
        let e = exposure();
        let mut c = ctx(Side::Buy, dec!(60), dec!(0), &e);
        c.capital = dec!(0); // No capital needed when no fraction is checked.
        let r = limits.evaluate(&c);
        let names: Vec<&str> = r.checks.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["position_quantity", "drawdown_value"]);
        assert!(result(&r, "position_quantity").is_reject());
        assert!(matches!(
            result(&r, "drawdown_value"),
            RiskCheckResult::Pass
        ));
    }

    #[test]
    fn mark_equity_tracks_peak() {
        let mut e = Exposure::default();
        e.mark_equity(dec!(100));
        e.mark_equity(dec!(120));
        e.mark_equity(dec!(90));
        assert_eq!(e.peak_equity, dec!(120));
        assert_eq!(e.drawdown_value(), dec!(30));
        assert_eq!(e.drawdown, dec!(0.25));
    }

    #[test]
    fn verdict_display_and_serde() {
        assert_eq!(Verdict::Clear.to_string(), "clear");
        assert_eq!(Verdict::Blocked.to_string(), "blocked");
        let json = serde_json::to_string(&RiskCheckResult::Warn { reason: "x".into() }).unwrap();
        assert_eq!(json, r#"{"result":"warn","reason":"x"}"#);
    }
}
