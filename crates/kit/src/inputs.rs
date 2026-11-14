//! What a task may tell a perp session's bot beyond the runtime contract,
//! as typed values: the positions it starts with ([`StartPositions`]), an
//! order to work ([`SessionOrder`]) and the broker's haircut on what it
//! leaves undone ([`BrokerHaircut`]), and the markets a broker closes out
//! ([`CloseOuts`]).
//!
//! Each travels as one of the session's variables ([`SESSION_VARS`]), as
//! JSON, and has a constant naming it, a `parse` for the variable's text and
//! a `to_var` that writes it: the grader writes them with `to_var`, and a
//! bot reads them with [`Session::start_positions`], [`Session::order`],
//! [`Session::broker_haircut`] and [`Session::close_outs`]. Markets are
//! named by the venue's symbol (`NVDA`), quantities are in base units and
//! haircuts in basis points.
//!
//! [`SESSION_VARS`]: crate::SESSION_VARS

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::{Session, Side, mandate::InstrumentRef};

/// A session variable that is set but doesn't read as its type.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{var}: {detail}")]
pub struct SessionVarError {
    /// The variable.
    pub var: &'static str,
    /// What is wrong with it.
    pub detail: String,
}

impl SessionVarError {
    fn new(var: &'static str, detail: impl Into<String>) -> Self {
        Self {
            var,
            detail: detail.into(),
        }
    }
}

/// The positions the bot's account starts the session with, in base units
/// by market symbol, positive long: `MTX_START_POSITIONS`, written
/// `{"NVDA": -480.0}`. A market it doesn't name starts flat.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct StartPositions(pub BTreeMap<String, f64>);

impl StartPositions {
    /// The variable it travels in.
    pub const VAR: &'static str = "MTX_START_POSITIONS";

    /// Reads the variable's text: every quantity finite.
    pub fn parse(text: &str) -> Result<Self, SessionVarError> {
        let positions: Self = from_json(Self::VAR, text)?;
        for (symbol, quantity) in &positions.0 {
            if !quantity.is_finite() {
                return Err(SessionVarError::new(
                    Self::VAR,
                    format!("{symbol}: {quantity} is not a quantity"),
                ));
            }
        }
        Ok(positions)
    }

    /// The variable's text.
    pub fn to_var(&self) -> String {
        to_json(self)
    }

    /// The starting position in `symbol`, in base units: 0 if not named.
    pub fn get(&self, symbol: &str) -> f64 {
        self.0.get(symbol).copied().unwrap_or(0.0)
    }
}

/// A quantity the bot is to trade by the end of the session, one way:
/// `MTX_ORDER`, written
/// `{"instrument": "phoenix:perp:NVDA", "side": "buy", "quantity": 62.0}`,
/// with `"reduce_only": true` when no fill may grow the position. What it
/// leaves undone a broker trades at the close, worse by the
/// [`BrokerHaircut`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionOrder {
    /// The market, as the mandate names it.
    pub instrument: InstrumentRef,
    /// [`Side::Bid`] to buy the quantity, [`Side::Ask`] to sell it.
    pub side: Side,
    /// Base units to trade by the end. Positive.
    pub quantity: f64,
    /// Whether any fill that grows the position breaks the order.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub reduce_only: bool,
}

impl SessionOrder {
    /// The variable it travels in.
    pub const VAR: &'static str = "MTX_ORDER";

    /// Reads the variable's text: a positive, finite quantity.
    pub fn parse(text: &str) -> Result<Self, SessionVarError> {
        let order: Self = from_json(Self::VAR, text)?;
        if !(order.quantity.is_finite() && order.quantity > 0.0) {
            return Err(SessionVarError::new(
                Self::VAR,
                format!("quantity {} must be positive", order.quantity),
            ));
        }
        Ok(order)
    }

    /// The variable's text.
    pub fn to_var(&self) -> String {
        to_json(self)
    }
}

/// What a broker's price for an order's remainder is worse than the
/// session's close by, in basis points: `MTX_BROKER_HAIRCUT_BPS`, written as
/// a plain number (`30`). Set with every [`SessionOrder`].
#[derive(Debug, Clone, Copy, Default, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(transparent)]
pub struct BrokerHaircut(pub f64);

impl BrokerHaircut {
    /// The variable it travels in.
    pub const VAR: &'static str = "MTX_BROKER_HAIRCUT_BPS";

    /// Reads the variable's text: a finite number of basis points, at
    /// least 0.
    pub fn parse(text: &str) -> Result<Self, SessionVarError> {
        let bps: f64 = text
            .trim()
            .parse()
            .map_err(|e| SessionVarError::new(Self::VAR, format!("{text:?}: {e}")))?;
        if !(bps.is_finite() && bps >= 0.0) {
            return Err(SessionVarError::new(
                Self::VAR,
                format!("{bps} bps must be at least 0"),
            ));
        }
        Ok(Self(bps))
    }

    /// The variable's text.
    pub fn to_var(&self) -> String {
        self.0.to_string()
    }

    /// The haircut as a fraction: 30 bps is 0.003.
    pub fn fraction(&self) -> f64 {
        self.0 / 10_000.0
    }
}

/// The markets whose leftover position a broker closes at the end of the
/// session, each with its haircut in basis points, by market symbol:
/// `MTX_CLOSE_OUT`, written `{"SPY": 10.0}`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct CloseOuts(pub BTreeMap<String, f64>);

impl CloseOuts {
    /// The variable it travels in.
    pub const VAR: &'static str = "MTX_CLOSE_OUT";

    /// Reads the variable's text: every haircut finite and at least 0.
    pub fn parse(text: &str) -> Result<Self, SessionVarError> {
        let close_outs: Self = from_json(Self::VAR, text)?;
        for (symbol, bps) in &close_outs.0 {
            if !(bps.is_finite() && *bps >= 0.0) {
                return Err(SessionVarError::new(
                    Self::VAR,
                    format!("{symbol}: {bps} bps must be at least 0"),
                ));
            }
        }
        Ok(close_outs)
    }

    /// The variable's text.
    pub fn to_var(&self) -> String {
        to_json(self)
    }

    /// The haircut a broker closes `symbol` out at, if it does.
    pub fn haircut(&self, symbol: &str) -> Option<BrokerHaircut> {
        self.0.get(symbol).copied().map(BrokerHaircut)
    }
}

impl Session {
    /// The positions the session starts the account with: none if the
    /// task sets none.
    pub fn start_positions(&self) -> Result<StartPositions, SessionVarError> {
        self.var(StartPositions::VAR)
            .map_or(Ok(StartPositions::default()), StartPositions::parse)
    }

    /// The order the session asks the bot to work, if any.
    pub fn order(&self) -> Result<Option<SessionOrder>, SessionVarError> {
        self.var(SessionOrder::VAR)
            .map(SessionOrder::parse)
            .transpose()
    }

    /// The broker's haircut on what the order leaves undone, if the
    /// session has an order.
    pub fn broker_haircut(&self) -> Result<Option<BrokerHaircut>, SessionVarError> {
        self.var(BrokerHaircut::VAR)
            .map(BrokerHaircut::parse)
            .transpose()
    }

    /// The markets a broker closes out at the end: none if the task sets
    /// none.
    pub fn close_outs(&self) -> Result<CloseOuts, SessionVarError> {
        self.var(CloseOuts::VAR)
            .map_or(Ok(CloseOuts::default()), CloseOuts::parse)
    }
}

fn from_json<T: serde::de::DeserializeOwned>(
    var: &'static str,
    text: &str,
) -> Result<T, SessionVarError> {
    serde_json::from_str(text).map_err(|e| SessionVarError::new(var, e.to_string()))
}

fn to_json(value: &impl Serialize) -> String {
    serde_json::to_string(value).expect("plain maps and numbers serialize")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(vars: &[(&str, &str)]) -> Session {
        Session {
            steps: 1,
            slots_per_step: 1,
            start_slot: 0,
            end_slot: 1,
            symbol: "NVDA".into(),
            start_mark: 1.0,
            start_oracle: 1.0,
            cap: 1.0,
            max_drawdown: 1.0,
            mandate: crate::Mandate::perp("phoenix", "NVDA", 1.0, 1.0),
            venue: String::new(),
            authority: String::new(),
            vars: vars
                .iter()
                .map(|(n, v)| (n.to_string(), v.to_string()))
                .collect(),
            seed: 0,
            venues: Vec::new(),
            reports: crate::Reports::default(),
            applies_tx_options: false,
        }
    }

    #[test]
    fn each_input_reads_back_what_was_written() {
        let positions = StartPositions([("NVDA".to_string(), -480.0)].into());
        let order = SessionOrder {
            instrument: "phoenix:perp:NVDA".parse().unwrap(),
            side: Side::Bid,
            quantity: 62.0,
            reduce_only: true,
        };
        let haircut = BrokerHaircut(30.0);
        let close_outs = CloseOuts([("SPY".to_string(), 10.0)].into());
        assert_eq!(
            order.to_var(),
            r#"{"instrument":"phoenix:perp:NVDA","side":"buy","quantity":62.0,"reduce_only":true}"#
        );
        let s = session(&[
            (StartPositions::VAR, &positions.to_var()),
            (SessionOrder::VAR, &order.to_var()),
            (BrokerHaircut::VAR, &haircut.to_var()),
            (CloseOuts::VAR, &close_outs.to_var()),
        ]);
        assert_eq!(s.start_positions().unwrap(), positions);
        assert_eq!(s.start_positions().unwrap().get("NVDA"), -480.0);
        assert_eq!(s.start_positions().unwrap().get("SPY"), 0.0);
        assert_eq!(s.order().unwrap(), Some(order));
        assert_eq!(s.broker_haircut().unwrap(), Some(haircut));
        assert_eq!(haircut.fraction(), 0.003);
        assert_eq!(
            s.close_outs().unwrap().haircut("SPY"),
            Some(BrokerHaircut(10.0))
        );
    }

    #[test]
    fn an_unset_input_is_empty_and_a_bad_one_an_error() {
        let s = session(&[]);
        assert_eq!(s.start_positions().unwrap(), StartPositions::default());
        assert_eq!(s.order().unwrap(), None);
        assert_eq!(s.broker_haircut().unwrap(), None);
        assert_eq!(s.close_outs().unwrap(), CloseOuts::default());

        let sell = r#"{"instrument":"phoenix:perp:NVDA","side":"sell","quantity":1}"#;
        assert_eq!(SessionOrder::parse(sell).unwrap().side, Side::Ask);
        for bad in [
            r#"{"instrument":"phoenix:perp:NVDA","side":"bid","quantity":1}"#,
            r#"{"instrument":"phoenix:perp:NVDA","side":"buy","quantity":0}"#,
            r#"{"instrument":"phoenix:perp:NVDA","side":"buy","quantity":1,"haircut_bps":3}"#,
            r#"{"instrument":"NVDA","side":"buy","quantity":1}"#,
        ] {
            let error = SessionOrder::parse(bad).unwrap_err();
            assert_eq!(error.var, "MTX_ORDER", "{bad}");
        }
        assert!(BrokerHaircut::parse("-1").is_err());
        assert!(BrokerHaircut::parse("thirty").is_err());
        assert_eq!(BrokerHaircut::parse(" 12.5 ").unwrap(), BrokerHaircut(12.5));
        assert!(CloseOuts::parse(r#"{"SPY":-1}"#).is_err());
        assert!(StartPositions::parse(r#"{"NVDA":"long"}"#).is_err());
        let s = session(&[(SessionOrder::VAR, "{")]);
        assert!(s.order().is_err());
    }
}
