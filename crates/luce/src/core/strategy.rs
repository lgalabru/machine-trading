//! Strategies and the cash flows that feed them.
//!
//! A [`Strategy`] is a named, long-lived allocation of capital with a purpose:
//! "lend SOL on Kamino's main market", "trade the SOL perpetual on Phoenix".
//! It belongs to one [`Account`](crate::core::account::Account)
//! and is either active or closed. Its history is a list of
//! [`StrategyEvent`]s — deposits into and withdrawals out of the position —
//! each tied to the on-chain transaction that made it happen.
//!
//! Strategies are the unit `luce status` reports on. Everything needed to
//! value one later (which provider, which market, which reserve, which asset)
//! is captured at open time so that reporting never depends on re-discovering
//! chain state.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

use crate::core::{
    account::AccountId, asset::Asset, instrument::InstrumentId, performance::CashFlow,
};

/// Identifies a yield provider, e.g. `"kamino"`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ProviderId(pub String);

impl ProviderId {
    /// Creates a provider identifier.
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }
}

impl std::fmt::Display for ProviderId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Unique identifier for a strategy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct StrategyId(pub Uuid);

impl StrategyId {
    /// Generates a fresh random identifier.
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }

    /// Returns `true` if the hyphenated lowercase rendering starts with
    /// `prefix` (case-insensitive). Lets users refer to strategies by the first
    /// few characters, as with git commits.
    pub fn matches_prefix(&self, prefix: &str) -> bool {
        !prefix.is_empty()
            && self
                .0
                .hyphenated()
                .to_string()
                .starts_with(&prefix.to_ascii_lowercase())
    }
}

impl Default for StrategyId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for StrategyId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.hyphenated().fmt(f)
    }
}

/// Unique identifier for a strategy event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct EventId(pub Uuid);

impl EventId {
    /// Generates a fresh random identifier.
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for EventId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for EventId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.hyphenated().fmt(f)
    }
}

/// Supplying an asset to a lending market to earn interest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LendStrategy {
    /// Which protocol.
    pub provider: ProviderId,
    /// Provider-specific market identifier (a lending market address on Kamino).
    pub market: String,
    /// Provider-specific pool identifier (a reserve address on Kamino).
    pub reserve: String,
    /// The asset being lent.
    pub asset: Asset,
}

/// Trading instruments on a venue with capital held in one asset.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TradeStrategy {
    /// The venue traded on, e.g. `"phoenix"`.
    pub provider: ProviderId,
    /// The instruments the strategy trades.
    pub instruments: Vec<InstrumentId>,
    /// The asset the capital (collateral) is held in, e.g. USDC.
    pub capital: Asset,
}

/// What a strategy does with its capital.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[non_exhaustive]
pub enum StrategyKind {
    /// Lend an asset to a money market.
    Lend(LendStrategy),
    /// Trade instruments; deposits and withdrawals move collateral.
    Trade(TradeStrategy),
}

impl StrategyKind {
    /// The asset the strategy is denominated in.
    pub fn asset(&self) -> &Asset {
        match self {
            Self::Lend(lend) => &lend.asset,
            Self::Trade(trade) => &trade.capital,
        }
    }

    /// The provider operating the strategy.
    pub fn provider(&self) -> &ProviderId {
        match self {
            Self::Lend(lend) => &lend.provider,
            Self::Trade(trade) => &trade.provider,
        }
    }

    /// Short machine-readable name of the kind, for display and storage.
    pub fn name(&self) -> &'static str {
        match self {
            Self::Lend(_) => "lend",
            Self::Trade(_) => "trade",
        }
    }
}

/// Lifecycle state of a strategy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StrategyStatus {
    /// Capital is deployed; the strategy can receive deposits and withdrawals.
    Active,
    /// Fully exited. No further events are expected.
    Closed,
}

impl std::fmt::Display for StrategyStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Active => f.write_str("active"),
            Self::Closed => f.write_str("closed"),
        }
    }
}

/// Errors from strategy state transitions.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum StrategyError {
    /// Closing a strategy twice is a bug in the caller.
    #[error("strategy {0} is already closed")]
    AlreadyClosed(StrategyId),
    /// Time does not run backwards.
    #[error("strategy {id} cannot close at {closed_at}, before it opened at {opened_at}")]
    ClosedBeforeOpened {
        id: StrategyId,
        opened_at: DateTime<Utc>,
        closed_at: DateTime<Utc>,
    },
}

/// A long-lived allocation of one account's capital.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Strategy {
    /// Stable identifier.
    pub id: StrategyId,
    /// Owning account.
    pub account_id: AccountId,
    /// What the strategy does.
    pub kind: StrategyKind,
    /// Lifecycle state.
    pub status: StrategyStatus,
    /// Optional free-text label chosen by the user.
    pub label: Option<String>,
    /// When the strategy was opened.
    pub opened_at: DateTime<Utc>,
    /// When the strategy was closed, if it has been.
    pub closed_at: Option<DateTime<Utc>>,
}

impl Strategy {
    /// Opens a new, active strategy.
    pub fn open(
        account_id: AccountId,
        kind: StrategyKind,
        label: Option<String>,
        opened_at: DateTime<Utc>,
    ) -> Self {
        Self {
            id: StrategyId::new(),
            account_id,
            kind,
            status: StrategyStatus::Active,
            label,
            opened_at,
            closed_at: None,
        }
    }

    /// Returns `true` while the strategy is active.
    pub fn is_active(&self) -> bool {
        self.status == StrategyStatus::Active
    }

    /// Marks the strategy closed at `at`.
    pub fn close(&mut self, at: DateTime<Utc>) -> Result<(), StrategyError> {
        if self.status == StrategyStatus::Closed {
            return Err(StrategyError::AlreadyClosed(self.id));
        }
        if at < self.opened_at {
            return Err(StrategyError::ClosedBeforeOpened {
                id: self.id,
                opened_at: self.opened_at,
                closed_at: at,
            });
        }
        self.status = StrategyStatus::Closed;
        self.closed_at = Some(at);
        Ok(())
    }

    /// The asset the strategy is denominated in.
    pub fn asset(&self) -> &Asset {
        self.kind.asset()
    }
}

/// Direction of a cash flow between the wallet and the strategy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventKind {
    /// Capital moved from the wallet into the position.
    Deposit,
    /// Capital moved from the position back to the wallet.
    Withdraw,
}

impl std::fmt::Display for EventKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Deposit => f.write_str("deposit"),
            Self::Withdraw => f.write_str("withdraw"),
        }
    }
}

/// One on-chain movement of capital attributed to a strategy.
///
/// Amounts are in the strategy asset's **native** units and are always
/// non-negative; direction comes from [`EventKind`]. For a deposit the amount
/// is what left the wallet; for a withdrawal it is what came back. Transaction
/// fees paid in the chain's gas token are deliberately *not* folded in here —
/// they belong to the wallet's ledger, not the strategy's yield.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StrategyEvent {
    /// Stable identifier.
    pub id: EventId,
    /// The strategy this event belongs to.
    pub strategy_id: StrategyId,
    /// Deposit or withdrawal.
    pub kind: EventKind,
    /// Asset amount moved, in native units.
    pub amount_native: u64,
    /// Provider share units minted (deposit) or burned (withdraw), if the
    /// provider has shares (Kamino cTokens). Lets later valuation be exact.
    pub shares_native: Option<u64>,
    /// Transaction signature that executed the movement.
    pub tx_signature: String,
    /// Slot the transaction landed in, if known.
    pub slot: Option<u64>,
    /// When the movement happened.
    pub occurred_at: DateTime<Utc>,
}

impl StrategyEvent {
    /// Builds an event with a fresh identifier.
    pub fn new(
        strategy_id: StrategyId,
        kind: EventKind,
        amount_native: u64,
        shares_native: Option<u64>,
        tx_signature: impl Into<String>,
        slot: Option<u64>,
        occurred_at: DateTime<Utc>,
    ) -> Self {
        Self {
            id: EventId::new(),
            strategy_id,
            kind,
            amount_native,
            shares_native,
            tx_signature: tx_signature.into(),
            slot,
            occurred_at,
        }
    }
}

/// Converts strategy events into signed UI cash flows for performance math.
///
/// Deposits are positive, withdrawals negative. Events are returned in
/// chronological order regardless of input order.
pub fn cash_flows(events: &[StrategyEvent], asset: &Asset) -> Vec<CashFlow> {
    let mut flows: Vec<CashFlow> = events
        .iter()
        .map(|event| {
            let magnitude = asset.to_ui(event.amount_native);
            let amount = match event.kind {
                EventKind::Deposit => magnitude,
                EventKind::Withdraw => -magnitude,
            };
            CashFlow {
                at: event.occurred_at,
                amount,
            }
        })
        .collect();
    flows.sort_by_key(|flow| flow.at);
    flows
}

/// Net share balance implied by a sequence of events: minted minus burned.
///
/// Returns `None` if any event lacks share information, since a partial sum
/// would be silently wrong.
pub fn net_shares(events: &[StrategyEvent]) -> Option<i128> {
    events.iter().try_fold(0_i128, |acc, event| {
        let shares = i128::from(event.shares_native?);
        Some(match event.kind {
            EventKind::Deposit => acc + shares,
            EventKind::Withdraw => acc - shares,
        })
    })
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;
    use rust_decimal_macros::dec;

    use super::*;

    fn sol() -> Asset {
        Asset::new("So11111111111111111111111111111111111111112", "SOL", 9).unwrap()
    }

    fn lend_kind() -> StrategyKind {
        StrategyKind::Lend(LendStrategy {
            provider: ProviderId::new("kamino"),
            market: "market".into(),
            reserve: "reserve".into(),
            asset: sol(),
        })
    }

    fn at(day: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 1, day, 12, 0, 0).unwrap()
    }

    #[test]
    fn open_is_active_and_close_transitions() {
        let mut s = Strategy::open(AccountId::new(), lend_kind(), Some("test".into()), at(1));
        assert!(s.is_active());
        assert_eq!(s.closed_at, None);
        assert_eq!(s.kind.name(), "lend");
        assert_eq!(s.asset().symbol, "SOL");

        s.close(at(5)).unwrap();
        assert!(!s.is_active());
        assert_eq!(s.status, StrategyStatus::Closed);
        assert_eq!(s.closed_at, Some(at(5)));
    }

    #[test]
    fn close_twice_fails() {
        let mut s = Strategy::open(AccountId::new(), lend_kind(), None, at(1));
        s.close(at(2)).unwrap();
        assert_eq!(
            s.close(at(3)).unwrap_err(),
            StrategyError::AlreadyClosed(s.id)
        );
    }

    #[test]
    fn close_before_open_fails() {
        let mut s = Strategy::open(AccountId::new(), lend_kind(), None, at(5));
        let err = s.close(at(1)).unwrap_err();
        assert!(matches!(err, StrategyError::ClosedBeforeOpened { .. }));
        assert!(s.is_active(), "failed close must not change state");
    }

    #[test]
    fn id_prefix_matching() {
        let id = StrategyId(Uuid::parse_str("3f2504e0-4f89-11d3-9a0c-0305e82c3301").unwrap());
        assert!(id.matches_prefix("3f25"));
        assert!(id.matches_prefix("3F25"));
        assert!(id.matches_prefix("3f2504e0-4f89"));
        assert!(!id.matches_prefix("4f89"));
        assert!(
            !id.matches_prefix(""),
            "empty prefix must not match everything"
        );
    }

    #[test]
    fn cash_flows_are_signed_and_sorted() {
        let sid = StrategyId::new();
        let events = vec![
            StrategyEvent::new(
                sid,
                EventKind::Withdraw,
                500_000_000,
                None,
                "sig2",
                None,
                at(3),
            ),
            StrategyEvent::new(
                sid,
                EventKind::Deposit,
                2_000_000_000,
                None,
                "sig1",
                None,
                at(1),
            ),
        ];
        let flows = cash_flows(&events, &sol());
        assert_eq!(flows.len(), 2);
        assert_eq!(flows[0].at, at(1));
        assert_eq!(flows[0].amount, dec!(2.000000000));
        assert_eq!(flows[1].at, at(3));
        assert_eq!(flows[1].amount, dec!(-0.500000000));
    }

    #[test]
    fn net_shares_requires_complete_information() {
        let sid = StrategyId::new();
        let with = vec![
            StrategyEvent::new(sid, EventKind::Deposit, 10, Some(100), "a", None, at(1)),
            StrategyEvent::new(sid, EventKind::Withdraw, 4, Some(40), "b", None, at(2)),
        ];
        assert_eq!(net_shares(&with), Some(60));
        let partial = vec![
            StrategyEvent::new(sid, EventKind::Deposit, 10, Some(100), "a", None, at(1)),
            StrategyEvent::new(sid, EventKind::Withdraw, 4, None, "b", None, at(2)),
        ];
        assert_eq!(net_shares(&partial), None);
        assert_eq!(net_shares(&[]), Some(0));
    }

    #[test]
    fn trade_kind_is_denominated_in_its_capital() {
        let usdc = Asset::new("EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v", "USDC", 6).unwrap();
        let kind = StrategyKind::Trade(TradeStrategy {
            provider: ProviderId::new("phoenix"),
            instruments: vec![InstrumentId::new("phoenix:SOL")],
            capital: usdc.clone(),
        });
        assert_eq!(kind.asset(), &usdc);
        assert_eq!(kind.name(), "trade");
        assert_eq!(kind.provider().0, "phoenix");
        let json = serde_json::to_string(&kind).unwrap();
        assert!(json.starts_with(r#"{"type":"trade""#), "{json}");
    }

    #[test]
    fn kind_serde_is_tagged() {
        let json = serde_json::to_string(&lend_kind()).unwrap();
        assert!(json.starts_with(r#"{"type":"lend""#), "{json}");
        let back: StrategyKind = serde_json::from_str(&json).unwrap();
        assert_eq!(back, lend_kind());
    }
}
