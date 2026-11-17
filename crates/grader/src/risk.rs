//! Closes the grader makes on a bot's behalf, and what they record.
//!
//! A scenario that opts in (`scoring.stop_out`, see [`crate::scenario::StopOut`])
//! prices a breach of the limits it lists instead of disqualifying the
//! session: the grader's risk desk stops the bot, cancels its orders and
//! closes its position through the book with market orders signed with the
//! bot's key, and the session ends once the account is flat. The close pays
//! the book's impact and the taker fee like any order. Every such close is a
//! [`ForcedClose`] in the session's report; a venue's own liquidation, done
//! by a grader-run keeper ([`crate::keeper`]), records the same shape, one
//! per stretch of liquidations, and the session goes on.

use serde::{Deserialize, Serialize};

use crate::scenario::StopRule;

/// The most market orders the risk desk sends in one step. A position the
/// book can't absorb in them is closed further on the next steps.
pub const MAX_CLOSE_ORDERS_PER_STEP: usize = 8;

/// A close the grader made on the bot's behalf.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ForcedClose {
    /// Why it closed.
    pub reason: ForcedCloseReason,
    /// The step, and the slot counted from the session's start, whose
    /// measurement triggered it (a liquidation: the step it first fired at).
    pub step: u64,
    pub at: u64,
    /// The account's value as the session measured it at the trigger, and
    /// its running peak then.
    pub value_before_usd: f64,
    pub peak_usd: f64,
    /// The account's value once the close settled: at the session's end,
    /// which is the step the account was flat at, or its last step.
    pub value_after_usd: f64,
    /// The position when it triggered, in base units (positive is long):
    /// the primary market's, in a session on several, with each market's
    /// in `positions_before`, by symbol.
    pub position_before: f64,
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub positions_before: std::collections::BTreeMap<String, f64>,
    /// The close's own orders, apart from the bot's.
    pub orders: Vec<CloseOrder>,
    /// Whether the account was flat at the session's end, on every market.
    pub flat: bool,
}

/// Why the grader closed.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum ForcedCloseReason {
    /// The risk desk enforcing a mandate limit the scenario prices.
    StopOut { rule: StopRule },
    /// The venue's own liquidation of an account that crossed its
    /// maintenance margin, done by a grader-run keeper (see
    /// [`crate::keeper`]). The bot keeps running afterwards, with what is
    /// left.
    Liquidation { by: LiquidatedBy },
}

/// Who liquidated.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LiquidatedBy {
    /// The scenario's competing keeper.
    Keeper,
}

/// One of a close's orders.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct CloseOrder {
    /// Slots into the session it was sent at.
    pub at: u64,
    /// Its market, in a session on several markets; a session on one
    /// trades only its own.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub market: Option<mtx_core::mandate::InstrumentRef>,
    /// `buy` or `sell`, in base units.
    pub side: String,
    pub size: f64,
    /// Its worst price, in USD.
    pub limit_usd: f64,
    /// The position once it executed, in base units.
    pub position_after: f64,
    pub ok: bool,
    /// Its transaction, when it landed: the per-fill audit leaves its fills
    /// out, as the risk desk's or the keeper's, not the bot's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// What one step of closing did.
pub(crate) struct Closing {
    pub orders: Vec<CloseOrder>,
    /// The position after them, in base units: on several markets, the
    /// largest one left.
    pub position: f64,
}

/// Whether a position this small counts as flat: below one of Phoenix's
/// base lots on the markets the grader trades (10^-3 of a share).
pub(crate) fn is_flat(position: f64) -> bool {
    position.abs() < 5e-4
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_forced_close_round_trips_through_json() {
        let close = ForcedClose {
            reason: ForcedCloseReason::StopOut {
                rule: StopRule::Drawdown,
            },
            step: 12,
            at: 26,
            value_before_usd: 8_950.0,
            peak_usd: 10_000.0,
            value_after_usd: 8_900.0,
            position_before: 700.0,
            positions_before: Default::default(),
            orders: vec![CloseOrder {
                at: 26,
                market: None,
                side: "sell".into(),
                size: 700.0,
                limit_usd: 137.0,
                position_after: 0.0,
                ok: true,
                error: None,
                signature: None,
            }],
            flat: true,
        };
        let json = serde_json::to_string(&close).unwrap();
        assert!(json.contains(r#""reason":{"kind":"stop_out","rule":"drawdown"}"#));
        assert_eq!(serde_json::from_str::<ForcedClose>(&json).unwrap(), close);
    }

    #[test]
    fn a_liquidation_names_the_keeper_in_json() {
        let close = ForcedClose {
            reason: ForcedCloseReason::Liquidation {
                by: LiquidatedBy::Keeper,
            },
            step: 40,
            at: 82,
            value_before_usd: 3_100.0,
            peak_usd: 10_000.0,
            value_after_usd: 3_036.0,
            position_before: 900.0,
            positions_before: Default::default(),
            orders: Vec::new(),
            flat: false,
        };
        let json = serde_json::to_string(&close).unwrap();
        assert!(json.contains(r#""reason":{"kind":"liquidation","by":"keeper"}"#));
        assert_eq!(serde_json::from_str::<ForcedClose>(&json).unwrap(), close);
    }

    #[test]
    fn flat_is_below_a_base_lot() {
        assert!(is_flat(0.0));
        assert!(is_flat(-0.0004));
        assert!(!is_flat(0.001));
    }
}
