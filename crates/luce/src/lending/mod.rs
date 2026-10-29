//! The lending provider contract.
//!
//! A [`LendingProvider`] is a money market that takes deposits of one asset
//! and pays interest on them: Kamino, Jupiter Lend, Marginfi. Luce talks to
//! all of them through this trait so a strategy can be opened, valued, risk
//! assessed, and exited without knowing which protocol is underneath.
//!
//! Providers **describe**; Luce **acts**. A provider reads chain state into
//! [`ReserveSnapshot`]s and [`LendPosition`]s and turns an intent (deposit
//! this much, withdraw everything) into
//! [`InstructionGroup`](crate::chain::InstructionGroup)s. Packing into
//! transactions, simulation, signing and sending all happen in
//! [`crate::chain::tx`], under the user's signer.
//!
//! No provider ships in this crate yet: Kamino's interface crate targets the
//! Solana 2.x crates, and this crate builds on 3.x/4.x. The contract, the
//! reserve arithmetic and the [`risk`] checks are protocol-agnostic and
//! tested on their own.

pub mod risk;

use async_trait::async_trait;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use solana_pubkey::Pubkey;
use thiserror::Error;

use crate::{
    chain::{
        error::{ChainError, PlanError},
        tx::ExecutionPlan,
    },
    core::{asset::Asset, rates::Yield, strategy::ProviderId},
};

/// Errors from a lending provider.
#[derive(Debug, Error)]
pub enum LendingError {
    /// Chain access failed.
    #[error(transparent)]
    Chain(#[from] ChainError),
    /// Instructions could not be packed.
    #[error(transparent)]
    Plan(#[from] PlanError),
    /// No reserve for the requested asset in the market.
    #[error("no reserve for {asset} in market {market}")]
    ReserveNotFound { market: Pubkey, asset: String },
    /// Nothing to withdraw.
    #[error("no position in reserve {reserve} for {owner}")]
    NoPosition { owner: Pubkey, reserve: Pubkey },
    /// The amount is not acceptable (zero, or more than the position holds).
    #[error("invalid amount: {0}")]
    InvalidAmount(String),
    /// The provider cannot do this on this reserve.
    #[error("unsupported: {0}")]
    Unsupported(String),
    /// On-chain data did not parse.
    #[error("decode {what}: {detail}")]
    Decode { what: &'static str, detail: String },
}

/// A lending market: a set of reserves governed together.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MarketRef {
    /// Which provider.
    pub provider: ProviderId,
    /// Market account.
    pub address: Pubkey,
    /// Human-readable name.
    pub name: String,
}

/// Whether a reserve accepts activity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReserveStatus {
    /// Normal operation.
    Active,
    /// Being wound down; deposits are refused.
    Obsolete,
    /// Not shown; typically pre-launch.
    Hidden,
}

impl std::fmt::Display for ReserveStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Active => f.write_str("active"),
            Self::Obsolete => f.write_str("obsolete"),
            Self::Hidden => f.write_str("hidden"),
        }
    }
}

/// One point of a piecewise-linear borrow-rate curve.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RateCurvePoint {
    /// Utilization at this point, as a fraction in `[0, 1]`.
    pub utilization: Decimal,
    /// Borrow APR at this utilization, as a fraction.
    pub borrow_apr: Decimal,
}

/// Everything known about a reserve at one moment.
///
/// Amounts are UI amounts of the reserve's asset. Rates are fractions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReserveSnapshot {
    /// Which provider.
    pub provider: ProviderId,
    /// Market the reserve belongs to.
    pub market: Pubkey,
    /// Reserve account.
    pub address: Pubkey,
    /// The asset lent and borrowed here.
    pub asset: Asset,
    /// Operational status.
    pub status: ReserveStatus,
    /// Reserve-level emergency switch.
    pub emergency_mode: bool,
    /// Market-level emergency switch.
    pub market_emergency_mode: bool,
    /// Yield paid to depositors from borrow interest.
    pub supply: Yield,
    /// Rate paid by borrowers.
    pub borrow: Yield,
    /// Extra depositor yield from protocol-funded rewards in the same asset.
    pub rewards_apr: Decimal,
    /// `borrowed / supplied`.
    pub utilization: Decimal,
    /// Total deposits (including what is lent out).
    pub total_supplied: Decimal,
    /// Total outstanding borrows.
    pub total_borrowed: Decimal,
    /// Liquidity that can be withdrawn right now.
    pub available_liquidity: Decimal,
    /// Cap on total deposits.
    pub deposit_limit: Decimal,
    /// Liquidity per share: how many native asset units one native share redeems for.
    pub exchange_rate: Decimal,
    /// Fraction of borrow interest kept by the protocol.
    pub protocol_take_rate: Decimal,
    /// Borrow-rate curve, sorted by utilization.
    pub borrow_curve: Vec<RateCurvePoint>,
    /// Max loan-to-value when used as collateral, in percent.
    pub max_ltv_pct: u8,
    /// Liquidation threshold when used as collateral, in percent.
    pub liquidation_threshold_pct: u8,
    /// Slot the data was read at, if known.
    pub observed_slot: Option<u64>,
}

impl ReserveSnapshot {
    /// Borrow APR the curve implies at `utilization` (clamped to `[0, 1]`).
    ///
    /// Linear interpolation between curve points; `None` if the curve is empty.
    pub fn borrow_apr_at(&self, utilization: Decimal) -> Option<Decimal> {
        let u = utilization.clamp(Decimal::ZERO, Decimal::ONE);
        let (first, last) = (self.borrow_curve.first()?, self.borrow_curve.last()?);
        if u <= first.utilization {
            return Some(first.borrow_apr);
        }
        if u >= last.utilization {
            return Some(last.borrow_apr);
        }
        for pair in self.borrow_curve.windows(2) {
            let (a, b) = (pair[0], pair[1]);
            if u >= a.utilization && u <= b.utilization {
                if u == a.utilization || b.utilization == a.utilization {
                    return Some(a.borrow_apr);
                }
                if u == b.utilization {
                    return Some(b.borrow_apr);
                }
                let slope = (b.borrow_apr - a.borrow_apr) / (b.utilization - a.utilization);
                return Some(a.borrow_apr + slope * (u - a.utilization));
            }
        }
        None
    }

    /// Supply APR at `utilization`: `borrow_apr(u) × u × (1 − take_rate)`.
    pub fn supply_apr_at(&self, utilization: Decimal) -> Option<Decimal> {
        let u = utilization.clamp(Decimal::ZERO, Decimal::ONE);
        Some(self.borrow_apr_at(u)? * u * (Decimal::ONE - self.protocol_take_rate))
    }

    /// Room left under the deposit limit (never negative).
    pub fn deposit_headroom(&self) -> Decimal {
        (self.deposit_limit - self.total_supplied).max(Decimal::ZERO)
    }

    /// Native liquidity `shares` redeem for, rounded down.
    pub fn shares_to_liquidity(&self, shares: u64) -> u64 {
        (Decimal::from(shares) * self.exchange_rate)
            .floor()
            .try_into()
            .unwrap_or(u64::MAX)
    }

    /// Native shares needed to withdraw `liquidity`, rounded up so the caller
    /// never receives less than asked. Zero if the exchange rate is zero.
    pub fn liquidity_to_shares_ceil(&self, liquidity: u64) -> u64 {
        if self.exchange_rate <= Decimal::ZERO {
            return 0;
        }
        (Decimal::from(liquidity) / self.exchange_rate)
            .ceil()
            .try_into()
            .unwrap_or(u64::MAX)
    }

    /// Total depositor yield, interest plus rewards, as an APY.
    pub fn total_supply_apy(&self) -> Decimal {
        self.supply.apy + self.rewards_apr
    }
}

/// A depositor's holding in one reserve.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LendPosition {
    /// Reserve account.
    pub reserve: Pubkey,
    /// The asset.
    pub asset: Asset,
    /// Native share units held.
    pub shares: u64,
    /// Native asset units the shares redeem for now.
    pub value_native: u64,
    /// UI amount of `value_native`.
    pub value: Decimal,
}

/// How much to withdraw.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WithdrawAmount {
    /// Everything, closing the position.
    All,
    /// A specific amount of the asset, in native units.
    Liquidity(u64),
}

/// A money market Luce can lend to.
#[async_trait]
pub trait LendingProvider: Send + Sync {
    /// Stable provider identifier.
    fn id(&self) -> ProviderId;

    /// Markets this provider instance knows about.
    async fn markets(&self) -> Result<Vec<MarketRef>, LendingError>;

    /// All reserves of a market.
    async fn reserves(&self, market: &Pubkey) -> Result<Vec<ReserveSnapshot>, LendingError>;

    /// One reserve by address.
    async fn reserve(&self, reserve: &Pubkey) -> Result<ReserveSnapshot, LendingError>;

    /// The reserve of a market for an asset symbol (case-insensitive), if any.
    async fn find_reserve(
        &self,
        market: &Pubkey,
        symbol: &str,
    ) -> Result<ReserveSnapshot, LendingError> {
        self.reserves(market)
            .await?
            .into_iter()
            .find(|r| r.asset.symbol.eq_ignore_ascii_case(symbol))
            .ok_or_else(|| LendingError::ReserveNotFound {
                market: *market,
                asset: symbol.to_owned(),
            })
    }

    /// `owner`'s position in `reserve`, if any.
    async fn position(
        &self,
        owner: &Pubkey,
        reserve: &Pubkey,
    ) -> Result<Option<LendPosition>, LendingError>;

    /// Plan to deposit `amount_native` of the reserve's asset for `owner`.
    async fn plan_deposit(
        &self,
        owner: &Pubkey,
        reserve: &Pubkey,
        amount_native: u64,
    ) -> Result<ExecutionPlan, LendingError>;

    /// Plan to withdraw from `owner`'s position in `reserve`.
    async fn plan_withdraw(
        &self,
        owner: &Pubkey,
        reserve: &Pubkey,
        amount: WithdrawAmount,
    ) -> Result<ExecutionPlan, LendingError>;

    /// Human-readable meaning of a custom program error code, if known.
    fn explain_error(&self, code: u32) -> Option<String>;
}

#[cfg(test)]
pub(crate) mod test_support {
    use rust_decimal_macros::dec;

    use super::*;

    /// A plausible SOL reserve for tests: 70% utilized, 2-point curve.
    pub fn sol_reserve() -> ReserveSnapshot {
        ReserveSnapshot {
            provider: ProviderId::new("test"),
            market: Pubkey::new_unique(),
            address: Pubkey::new_unique(),
            asset: Asset::new("So11111111111111111111111111111111111111112", "SOL", 9).unwrap(),
            status: ReserveStatus::Active,
            emergency_mode: false,
            market_emergency_mode: false,
            supply: Yield::from_apr_continuous(dec!(0.042)).unwrap(),
            borrow: Yield::from_apr_continuous(dec!(0.07)).unwrap(),
            rewards_apr: dec!(0),
            utilization: dec!(0.70),
            total_supplied: dec!(1000000),
            total_borrowed: dec!(700000),
            available_liquidity: dec!(300000),
            deposit_limit: dec!(1500000),
            exchange_rate: dec!(1.05),
            protocol_take_rate: dec!(0.15),
            borrow_curve: vec![
                RateCurvePoint {
                    utilization: dec!(0),
                    borrow_apr: dec!(0),
                },
                RateCurvePoint {
                    utilization: dec!(0.8),
                    borrow_apr: dec!(0.08),
                },
                RateCurvePoint {
                    utilization: dec!(1),
                    borrow_apr: dec!(1.0),
                },
            ],
            max_ltv_pct: 75,
            liquidation_threshold_pct: 80,
            observed_slot: Some(1),
        }
    }
}

#[cfg(test)]
mod tests {
    use rust_decimal_macros::dec;

    use super::test_support::sol_reserve;

    #[test]
    fn borrow_curve_interpolates_and_clamps() {
        let r = sol_reserve();
        assert_eq!(r.borrow_apr_at(dec!(0)), Some(dec!(0)));
        assert_eq!(r.borrow_apr_at(dec!(0.4)), Some(dec!(0.04)));
        assert_eq!(r.borrow_apr_at(dec!(0.8)), Some(dec!(0.08)));
        assert_eq!(r.borrow_apr_at(dec!(0.9)), Some(dec!(0.54)));
        assert_eq!(r.borrow_apr_at(dec!(1)), Some(dec!(1.0)));
        assert_eq!(
            r.borrow_apr_at(dec!(1.7)),
            Some(dec!(1.0)),
            "clamped above 1"
        );
        assert_eq!(r.borrow_apr_at(dec!(-1)), Some(dec!(0)), "clamped below 0");

        let mut empty = sol_reserve();
        empty.borrow_curve.clear();
        assert_eq!(empty.borrow_apr_at(dec!(0.5)), None);
    }

    #[test]
    fn supply_apr_follows_utilization_and_take_rate() {
        let r = sol_reserve();
        // 0.07 × 0.7 × 0.85 = 0.04165
        assert_eq!(r.supply_apr_at(dec!(0.7)), Some(dec!(0.04165)));
        assert_eq!(r.supply_apr_at(dec!(0)), Some(dec!(0)));
    }

    #[test]
    fn headroom_and_share_conversions() {
        let r = sol_reserve();
        assert_eq!(r.deposit_headroom(), dec!(500000));
        let mut full = sol_reserve();
        full.total_supplied = dec!(2000000);
        assert_eq!(full.deposit_headroom(), dec!(0));

        assert_eq!(r.shares_to_liquidity(1_000_000_000), 1_050_000_000);
        assert_eq!(r.shares_to_liquidity(1), 1); // 1.05 floors to 1
        assert_eq!(r.liquidity_to_shares_ceil(1_050_000_000), 1_000_000_000);
        assert_eq!(r.liquidity_to_shares_ceil(1), 1); // 0.95 ceils to 1
        assert_eq!(r.liquidity_to_shares_ceil(0), 0);
        let mut zero = sol_reserve();
        zero.exchange_rate = dec!(0);
        assert_eq!(zero.liquidity_to_shares_ceil(5), 0);
    }

    #[test]
    fn total_supply_apy_includes_rewards() {
        let mut r = sol_reserve();
        r.rewards_apr = dec!(0.01);
        assert_eq!(r.total_supply_apy(), r.supply.apy + dec!(0.01));
    }
}
