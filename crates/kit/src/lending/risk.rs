//! Risk assessment for lending deposits and positions.
//!
//! Lending the same asset you hold has no price risk — SOL in, SOL out — so
//! the risks that remain are operational and structural, and they can be
//! read off the reserve itself:
//!
//! | Check | Question it answers |
//! |-------|---------------------|
//! | `reserve_status` | Is the reserve accepting deposits at all? |
//! | `emergency_mode` | Has the protocol or market pulled the emergency switch? |
//! | `deposit_limit` | Does this deposit fit under the cap, with room to spare? |
//! | `wallet_balance` | Can the wallet fund it? |
//! | `fee_reserve` | Will enough SOL remain to pay for the exit transaction? |
//! | `utilization` | Is the pool so heavily borrowed that exits may be delayed? |
//! | `exit_liquidity` | Could the whole position be withdrawn right now? |
//! | `concentration` | Is this position a large share of the pool? |
//! | `rate_sensitivity` | What happens to the yield if utilization drops? |
//! | `benchmark` | Does lending beat simply staking? |
//!
//! Smart-contract and governance risk are real and not quantified here; the
//! report says so rather than pretending otherwise.
//!
//! Each check is a [`RiskCheck`] with the same pass/warn/reject vocabulary the
//! trading side uses. A deposit with any rejection must not be signed.

use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use super::ReserveSnapshot;
pub use crate::core::risk::Verdict;
use crate::core::risk::{RiskCheck, RiskCheckResult};

/// Utilization above which exits may be delayed.
pub const UTILIZATION_WARN: Decimal = Decimal::from_parts(90, 0, 0, false, 2);
/// Pool share above which a position is "large".
pub const CONCENTRATION_WARN: Decimal = Decimal::from_parts(5, 0, 0, false, 2);
/// Post-deposit headroom under the deposit limit, as a fraction of the limit,
/// below which the cap is "close".
pub const HEADROOM_WARN_FRACTION: Decimal = Decimal::from_parts(10, 0, 0, false, 2);
/// Utilization drop applied for the rate-sensitivity stress.
pub const RATE_STRESS_UTILIZATION_DROP: Decimal = Decimal::from_parts(10, 0, 0, false, 2);
/// Stressed supply APR below this fraction of the current APR is "fragile".
pub const RATE_FRAGILITY_FRACTION: Decimal = Decimal::from_parts(50, 0, 0, false, 2);
/// Exit-liquidity coverage is reported up to this multiple of the position.
const COVERAGE_CAP: Decimal = Decimal::from_parts(1000, 0, 0, false, 0);

/// The checks, their combined verdict, and the risks no check can measure.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RiskAssessment {
    /// Individual checks, in evaluation order.
    pub checks: Vec<RiskCheck>,
    /// Combined verdict.
    pub verdict: Verdict,
    /// Risks acknowledged but not quantified.
    pub unquantified: Vec<String>,
}

impl RiskAssessment {
    fn from_checks(checks: Vec<RiskCheck>) -> Self {
        let verdict = Verdict::of(&checks);
        Self {
            checks,
            verdict,
            unquantified: vec![
                "smart-contract risk: the lending program could have a bug or be exploited".into(),
                "governance risk: the protocol can change reserve parameters without notice".into(),
            ],
        }
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
}

fn check(name: &str, result: RiskCheckResult, current: Decimal, limit: Decimal) -> RiskCheck {
    RiskCheck {
        name: name.to_owned(),
        result,
        current_value: current,
        limit_value: limit,
    }
}

fn warn(reason: String) -> RiskCheckResult {
    RiskCheckResult::Warn { reason }
}

fn reject(reason: String) -> RiskCheckResult {
    RiskCheckResult::Reject { reason }
}

/// Inputs for assessing a prospective deposit. UI amounts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DepositContext<'a> {
    /// The reserve being deposited into.
    pub reserve: &'a ReserveSnapshot,
    /// Value already held in this reserve by the depositor.
    pub existing_position: Decimal,
    /// Amount to deposit.
    pub amount: Decimal,
    /// Depositor's balance of the reserve asset (for SOL: the SOL balance).
    pub wallet_asset_balance: Decimal,
    /// Depositor's SOL balance, for fees.
    pub wallet_sol_balance: Decimal,
    /// SOL that must remain after the deposit to pay for the exit.
    pub fee_reserve_sol: Decimal,
    /// Whether the reserve asset is native SOL (then the deposit and fees
    /// draw from the same balance).
    pub asset_is_sol: bool,
    /// Benchmark APY (staking), if known.
    pub benchmark_apy: Option<Decimal>,
}

/// Assesses a prospective deposit.
pub fn assess_deposit(ctx: &DepositContext<'_>) -> RiskAssessment {
    let r = ctx.reserve;
    let mut checks = Vec::with_capacity(10);

    checks.extend(status_checks(r));

    // Deposit limit.
    let post_supplied = r.total_supplied + ctx.amount;
    let result = if post_supplied > r.deposit_limit {
        reject(format!(
            "deposit would bring supply to {post_supplied} {}, above the limit of {} (headroom {})",
            r.asset.symbol,
            r.deposit_limit,
            r.deposit_headroom()
        ))
    } else if r.deposit_limit > Decimal::ZERO
        && (r.deposit_limit - post_supplied) / r.deposit_limit < HEADROOM_WARN_FRACTION
    {
        warn(format!(
            "after this deposit the reserve is within {} of its deposit limit; further deposits may be refused",
            pct(HEADROOM_WARN_FRACTION)
        ))
    } else {
        RiskCheckResult::Pass
    };
    checks.push(check(
        "deposit_limit",
        result,
        post_supplied,
        r.deposit_limit,
    ));

    // Wallet balance.
    let needed = if ctx.asset_is_sol {
        ctx.amount + ctx.fee_reserve_sol
    } else {
        ctx.amount
    };
    let result = if ctx.wallet_asset_balance < needed {
        reject(format!(
            "wallet holds {} {}, needs {}{}",
            n(ctx.wallet_asset_balance),
            r.asset.symbol,
            n(needed),
            if ctx.asset_is_sol {
                format!(
                    " (deposit plus a {} SOL fee reserve)",
                    n(ctx.fee_reserve_sol)
                )
            } else {
                String::new()
            }
        ))
    } else {
        RiskCheckResult::Pass
    };
    checks.push(check(
        "wallet_balance",
        result,
        ctx.wallet_asset_balance,
        needed,
    ));

    // Fee reserve (only meaningful when the asset is not SOL; otherwise folded above).
    if !ctx.asset_is_sol {
        let result = if ctx.wallet_sol_balance < ctx.fee_reserve_sol {
            reject(format!(
                "wallet holds {} SOL, below the {} SOL needed to pay for transactions and the exit",
                n(ctx.wallet_sol_balance),
                n(ctx.fee_reserve_sol)
            ))
        } else {
            RiskCheckResult::Pass
        };
        checks.push(check(
            "fee_reserve",
            result,
            ctx.wallet_sol_balance,
            ctx.fee_reserve_sol,
        ));
    }

    let post_position = ctx.existing_position + ctx.amount;
    checks.extend(pool_checks(
        r,
        post_supplied,
        r.available_liquidity + ctx.amount,
        post_position,
        ctx.benchmark_apy,
    ));

    RiskAssessment::from_checks(checks)
}

/// Inputs for assessing an existing position.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PositionContext<'a> {
    /// The reserve the position is in.
    pub reserve: &'a ReserveSnapshot,
    /// Current value of the position, UI amount.
    pub position_value: Decimal,
    /// Benchmark APY (staking), if known.
    pub benchmark_apy: Option<Decimal>,
}

/// Assesses an existing position.
pub fn assess_position(ctx: &PositionContext<'_>) -> RiskAssessment {
    let r = ctx.reserve;
    let mut checks = status_checks(r);
    checks.extend(pool_checks(
        r,
        r.total_supplied,
        r.available_liquidity,
        ctx.position_value,
        ctx.benchmark_apy,
    ));
    RiskAssessment::from_checks(checks)
}

fn status_checks(r: &ReserveSnapshot) -> Vec<RiskCheck> {
    let status = if r.status == super::ReserveStatus::Active {
        RiskCheckResult::Pass
    } else {
        reject(format!("reserve status is {}", r.status))
    };
    let emergency = match (r.emergency_mode, r.market_emergency_mode) {
        (false, false) => RiskCheckResult::Pass,
        (true, _) => reject("reserve is in emergency mode".into()),
        (_, true) => reject("market is in emergency mode".into()),
    };
    let emergency_value = Decimal::from(u8::from(r.emergency_mode || r.market_emergency_mode));
    vec![
        check(
            "reserve_status",
            status,
            Decimal::from(u8::from(r.status == super::ReserveStatus::Active)),
            Decimal::ONE,
        ),
        check("emergency_mode", emergency, emergency_value, Decimal::ZERO),
    ]
}

fn pool_checks(
    r: &ReserveSnapshot,
    supplied: Decimal,
    available: Decimal,
    position: Decimal,
    benchmark_apy: Option<Decimal>,
) -> Vec<RiskCheck> {
    let mut checks = Vec::with_capacity(5);

    // Utilization.
    let utilization = if supplied > Decimal::ZERO {
        (r.total_borrowed / supplied).min(Decimal::ONE)
    } else {
        Decimal::ZERO
    };
    let result = if utilization > UTILIZATION_WARN {
        warn(format!(
            "utilization is {}; above {} withdrawals compete for thin liquidity and may be delayed",
            pct(utilization),
            pct(UTILIZATION_WARN)
        ))
    } else {
        RiskCheckResult::Pass
    };
    checks.push(check("utilization", result, utilization, UTILIZATION_WARN));

    // Exit liquidity.
    let coverage = if position > Decimal::ZERO {
        available / position
    } else {
        Decimal::MAX
    };
    let result = if position > Decimal::ZERO && coverage < Decimal::ONE {
        warn(format!(
            "only {} {} is withdrawable right now against a position of {}; a full exit would wait for repayments",
            n(available),
            r.asset.symbol,
            n(position)
        ))
    } else {
        RiskCheckResult::Pass
    };
    checks.push(check(
        "exit_liquidity",
        result,
        coverage.min(COVERAGE_CAP),
        Decimal::ONE,
    ));

    // Concentration.
    let share = if supplied > Decimal::ZERO {
        position / supplied
    } else {
        Decimal::ZERO
    };
    let result = if share > CONCENTRATION_WARN {
        warn(format!(
            "position is {} of the pool; your own exit would move utilization and rates",
            pct(share)
        ))
    } else {
        RiskCheckResult::Pass
    };
    checks.push(check("concentration", result, share, CONCENTRATION_WARN));

    // Rate sensitivity: supply APR if utilization fell by the stress amount.
    if let (Some(current), Some(stressed)) = (
        r.supply_apr_at(utilization),
        r.supply_apr_at(utilization - RATE_STRESS_UTILIZATION_DROP),
    ) {
        let result = if current > Decimal::ZERO && stressed < current * RATE_FRAGILITY_FRACTION {
            warn(format!(
                "a {} drop in utilization would cut the supply APR from {} to {}; this yield depends on borrowers staying",
                pct(RATE_STRESS_UTILIZATION_DROP),
                pct(current),
                pct(stressed)
            ))
        } else {
            RiskCheckResult::Pass
        };
        checks.push(check("rate_sensitivity", result, stressed, current));
    }

    // Benchmark.
    if let Some(benchmark) = benchmark_apy {
        let total = r.total_supply_apy();
        let result = if total < benchmark {
            warn(format!(
                "lending pays {} APY while staking pays about {}; staking would earn more",
                pct(total),
                pct(benchmark)
            ))
        } else {
            RiskCheckResult::Pass
        };
        checks.push(check("benchmark", result, total, benchmark));
    }

    checks
}

/// Formats a fraction as a percentage with two decimals.
pub(crate) fn pct(rate: Decimal) -> String {
    crate::core::rates::format_pct(rate, 2)
}

/// Formats an amount without trailing zeros.
fn n(value: Decimal) -> String {
    value.normalize().to_string()
}

#[cfg(test)]
mod tests {
    use rust_decimal_macros::dec;

    use super::{
        super::{RateCurvePoint, test_support::sol_reserve},
        *,
    };

    fn ctx(reserve: &ReserveSnapshot) -> DepositContext<'_> {
        DepositContext {
            reserve,
            existing_position: dec!(0),
            amount: dec!(10),
            wallet_asset_balance: dec!(50),
            wallet_sol_balance: dec!(50),
            fee_reserve_sol: dec!(0.05),
            asset_is_sol: true,
            benchmark_apy: Some(dec!(0.03)),
        }
    }

    fn result_of<'a>(a: &'a RiskAssessment, name: &str) -> &'a RiskCheckResult {
        &a.checks
            .iter()
            .find(|c| c.name == name)
            .unwrap_or_else(|| panic!("no check {name}"))
            .result
    }

    #[test]
    fn healthy_deposit_is_clear() {
        let r = sol_reserve();
        let a = assess_deposit(&ctx(&r));
        assert_eq!(
            a.verdict,
            Verdict::Clear,
            "{:?}",
            a.findings().collect::<Vec<_>>()
        );
        assert!(a.allows_proceeding());
        assert_eq!(a.findings().count(), 0);
        assert_eq!(a.unquantified.len(), 2);
        let names: Vec<&str> = a.checks.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(
            names,
            vec![
                "reserve_status",
                "emergency_mode",
                "deposit_limit",
                "wallet_balance",
                "utilization",
                "exit_liquidity",
                "concentration",
                "rate_sensitivity",
                "benchmark"
            ]
        );
    }

    #[test]
    fn inactive_or_emergency_reserve_blocks() {
        let mut r = sol_reserve();
        r.status = super::super::ReserveStatus::Obsolete;
        let a = assess_deposit(&ctx(&r));
        assert_eq!(a.verdict, Verdict::Blocked);
        assert!(matches!(
            result_of(&a, "reserve_status"),
            RiskCheckResult::Reject { .. }
        ));

        let mut r = sol_reserve();
        r.market_emergency_mode = true;
        let a = assess_deposit(&ctx(&r));
        assert!(
            matches!(result_of(&a, "emergency_mode"), RiskCheckResult::Reject { reason } if reason.contains("market"))
        );
        assert!(!a.allows_proceeding());
    }

    #[test]
    fn deposit_limit_rejects_and_warns() {
        let r = sol_reserve();
        let mut c = ctx(&r);
        c.amount = dec!(600000);
        c.wallet_asset_balance = dec!(700000);
        let a = assess_deposit(&c);
        assert!(matches!(
            result_of(&a, "deposit_limit"),
            RiskCheckResult::Reject { .. }
        ));

        // Limit 1.5M, supplied 1M: a 450k deposit leaves 50k = 3.3% headroom.
        c.amount = dec!(450000);
        let a = assess_deposit(&c);
        assert!(matches!(
            result_of(&a, "deposit_limit"),
            RiskCheckResult::Warn { .. }
        ));
        assert_eq!(a.verdict, Verdict::Warnings);
    }

    #[test]
    fn wallet_balance_includes_fee_reserve_for_sol() {
        let r = sol_reserve();
        let mut c = ctx(&r);
        c.wallet_asset_balance = dec!(10.04);
        c.wallet_sol_balance = dec!(10.04);
        let a = assess_deposit(&c);
        assert!(
            matches!(result_of(&a, "wallet_balance"), RiskCheckResult::Reject { reason } if reason.contains("fee reserve"))
        );

        c.wallet_asset_balance = dec!(10.05);
        c.wallet_sol_balance = dec!(10.05);
        let a = assess_deposit(&c);
        assert!(matches!(
            result_of(&a, "wallet_balance"),
            RiskCheckResult::Pass
        ));
    }

    #[test]
    fn non_sol_asset_checks_fee_reserve_separately() {
        let mut r = sol_reserve();
        r.asset = crate::core::asset::Asset::new("EPjF", "USDC", 6).unwrap();
        let mut c = ctx(&r);
        c.asset_is_sol = false;
        c.wallet_asset_balance = dec!(100);
        c.wallet_sol_balance = dec!(0.01);
        let a = assess_deposit(&c);
        assert!(matches!(
            result_of(&a, "wallet_balance"),
            RiskCheckResult::Pass
        ));
        assert!(matches!(
            result_of(&a, "fee_reserve"),
            RiskCheckResult::Reject { .. }
        ));
        assert_eq!(a.verdict, Verdict::Blocked);
    }

    #[test]
    fn utilization_and_exit_liquidity_warn_on_thin_pools() {
        let mut r = sol_reserve();
        r.total_borrowed = dec!(950000);
        r.available_liquidity = dec!(50000);
        r.utilization = dec!(0.95);
        let mut c = ctx(&r);
        c.amount = dec!(100000);
        c.wallet_asset_balance = dec!(200000);
        c.wallet_sol_balance = dec!(200000);
        let a = assess_deposit(&c);
        // post utilization = 950k / 1.1M = 86% → under 90%, passes; but a
        // position of 100k against 150k available is fine too.
        assert!(matches!(
            result_of(&a, "utilization"),
            RiskCheckResult::Pass
        ));

        let p = assess_position(&PositionContext {
            reserve: &r,
            position_value: dec!(80000),
            benchmark_apy: None,
        });
        assert!(matches!(
            result_of(&p, "utilization"),
            RiskCheckResult::Warn { .. }
        ));
        assert!(matches!(
            result_of(&p, "exit_liquidity"),
            RiskCheckResult::Warn { .. }
        ));
        assert!(matches!(
            result_of(&p, "concentration"),
            RiskCheckResult::Warn { .. }
        ));
        assert_eq!(p.verdict, Verdict::Warnings);
        assert!(
            p.checks.iter().all(|c| c.name != "benchmark"),
            "no benchmark given"
        );
    }

    #[test]
    fn rate_sensitivity_flags_fragile_yield() {
        // Steep curve: above the kink the rate collapses when utilization drops.
        let mut r = sol_reserve();
        r.borrow_curve = vec![
            RateCurvePoint {
                utilization: dec!(0),
                borrow_apr: dec!(0),
            },
            RateCurvePoint {
                utilization: dec!(0.9),
                borrow_apr: dec!(0.01),
            },
            RateCurvePoint {
                utilization: dec!(1),
                borrow_apr: dec!(2.0),
            },
        ];
        r.total_borrowed = dec!(980000);
        r.available_liquidity = dec!(20000);
        let p = assess_position(&PositionContext {
            reserve: &r,
            position_value: dec!(1),
            benchmark_apy: None,
        });
        assert!(matches!(
            result_of(&p, "rate_sensitivity"),
            RiskCheckResult::Warn { .. }
        ));
    }

    #[test]
    fn benchmark_warns_when_staking_pays_more() {
        let r = sol_reserve();
        let mut c = ctx(&r);
        c.benchmark_apy = Some(dec!(0.07));
        let a = assess_deposit(&c);
        assert!(
            matches!(result_of(&a, "benchmark"), RiskCheckResult::Warn { reason } if reason.contains("staking"))
        );
        assert_eq!(a.verdict, Verdict::Warnings);
        assert!(a.allows_proceeding());
    }

    #[test]
    fn verdict_display() {
        assert_eq!(Verdict::Clear.to_string(), "clear");
        assert_eq!(Verdict::Blocked.to_string(), "blocked");
    }
}
