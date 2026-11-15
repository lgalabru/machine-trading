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
//! named as the mandate names them, by [`InstrumentRef`]
//! (`phoenix:perp:NVDA`), so two venues' markets of one symbol stay apart;
//! quantities are in base units and haircuts in basis points. Reading
//! refuses a market given twice, and writing refuses a value that isn't a
//! finite number, so neither side can lose or garble an entry.
//!
//! The helpers are `#[inline]`. That keeps them out of the kit's own
//! compiled code, so their `serde_json` code doesn't change how the
//! compiler optimises a bot's mandate parse when the bot doesn't call them
//! (code a bot never calls is dropped at link time either way).
//!
//! [`SESSION_VARS`]: crate::SESSION_VARS

use std::{collections::BTreeMap, fmt};

use serde::{
    Deserialize, Deserializer, Serialize,
    de::{Error as _, MapAccess, Visitor},
};

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
    #[inline]
    fn new(var: &'static str, detail: impl Into<String>) -> Self {
        Self {
            var,
            detail: detail.into(),
        }
    }
}

/// The positions the bot's account starts the session with, in base units
/// by market, positive long: `MTX_START_POSITIONS`, written
/// `{"phoenix:perp:NVDA": -480.0}`. A market it doesn't name starts flat.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[serde(transparent)]
pub struct StartPositions(pub BTreeMap<InstrumentRef, f64>);

impl<'de> Deserialize<'de> for StartPositions {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_map(ByInstrument).map(Self)
    }
}

impl StartPositions {
    /// The variable it travels in.
    pub const VAR: &'static str = "MTX_START_POSITIONS";

    /// Reads the variable's text: each market once, every quantity finite.
    #[inline]
    pub fn parse(text: &str) -> Result<Self, SessionVarError> {
        let positions: Self = from_json(Self::VAR, text)?;
        positions.check()?;
        Ok(positions)
    }

    /// The variable's text: an error if a quantity isn't finite.
    #[inline]
    pub fn to_var(&self) -> Result<String, SessionVarError> {
        self.check()?;
        Ok(to_json(self))
    }

    /// The starting position in `instrument`, in base units: 0 if not
    /// named.
    #[inline]
    pub fn get(&self, instrument: &InstrumentRef) -> f64 {
        self.0.get(instrument).copied().unwrap_or(0.0)
    }

    #[inline]
    fn check(&self) -> Result<(), SessionVarError> {
        for (instrument, quantity) in &self.0 {
            if !quantity.is_finite() {
                return Err(SessionVarError::new(
                    Self::VAR,
                    format!("{instrument}: {quantity} is not a quantity"),
                ));
            }
        }
        Ok(())
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

    /// Reads the variable's text: a positive, finite quantity. A field
    /// given twice, or one it doesn't know, is an error.
    #[inline]
    pub fn parse(text: &str) -> Result<Self, SessionVarError> {
        let order: Self = from_json(Self::VAR, text)?;
        order.check()?;
        Ok(order)
    }

    /// The variable's text: an error if the quantity isn't positive and
    /// finite.
    #[inline]
    pub fn to_var(&self) -> Result<String, SessionVarError> {
        self.check()?;
        Ok(to_json(self))
    }

    #[inline]
    fn check(&self) -> Result<(), SessionVarError> {
        if self.quantity.is_finite() && self.quantity > 0.0 {
            Ok(())
        } else {
            Err(SessionVarError::new(
                Self::VAR,
                format!("quantity {} must be positive", self.quantity),
            ))
        }
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
    #[inline]
    pub fn parse(text: &str) -> Result<Self, SessionVarError> {
        let bps: f64 = text
            .trim()
            .parse()
            .map_err(|e| SessionVarError::new(Self::VAR, format!("{text:?}: {e}")))?;
        let haircut = Self(bps);
        haircut.check(Self::VAR, "")?;
        Ok(haircut)
    }

    /// The variable's text: an error if the haircut isn't finite and at
    /// least 0.
    #[inline]
    pub fn to_var(&self) -> Result<String, SessionVarError> {
        self.check(Self::VAR, "")?;
        Ok(self.0.to_string())
    }

    #[inline]
    fn check(&self, var: &'static str, of: &str) -> Result<(), SessionVarError> {
        if self.0.is_finite() && self.0 >= 0.0 {
            Ok(())
        } else {
            Err(SessionVarError::new(
                var,
                format!("{of}{} bps must be at least 0", self.0),
            ))
        }
    }

    /// The haircut as a fraction: 30 bps is 0.003.
    #[inline]
    pub fn fraction(&self) -> f64 {
        self.0 / 10_000.0
    }
}

/// The markets whose leftover position a broker closes at the end of the
/// session, each with its haircut in basis points: `MTX_CLOSE_OUT`, written
/// `{"phoenix:perp:SPY": 10.0}`.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[serde(transparent)]
pub struct CloseOuts(pub BTreeMap<InstrumentRef, f64>);

impl<'de> Deserialize<'de> for CloseOuts {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_map(ByInstrument).map(Self)
    }
}

impl CloseOuts {
    /// The variable it travels in.
    pub const VAR: &'static str = "MTX_CLOSE_OUT";

    /// Reads the variable's text: each market once, every haircut finite
    /// and at least 0.
    #[inline]
    pub fn parse(text: &str) -> Result<Self, SessionVarError> {
        let close_outs: Self = from_json(Self::VAR, text)?;
        close_outs.check()?;
        Ok(close_outs)
    }

    /// The variable's text: an error if a haircut isn't finite and at
    /// least 0.
    #[inline]
    pub fn to_var(&self) -> Result<String, SessionVarError> {
        self.check()?;
        Ok(to_json(self))
    }

    /// The haircut a broker closes `instrument` out at, if it does.
    #[inline]
    pub fn haircut(&self, instrument: &InstrumentRef) -> Option<BrokerHaircut> {
        self.0.get(instrument).copied().map(BrokerHaircut)
    }

    #[inline]
    fn check(&self) -> Result<(), SessionVarError> {
        for (instrument, &bps) in &self.0 {
            BrokerHaircut(bps).check(Self::VAR, &format!("{instrument}: "))?;
        }
        Ok(())
    }
}

/// Reads a JSON object of `instrument: number` entries, refusing a key that
/// isn't an instrument (the empty one included) and a market given twice,
/// which a plain map would let the last one win.
struct ByInstrument;

impl<'de> Visitor<'de> for ByInstrument {
    type Value = BTreeMap<InstrumentRef, f64>;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("an object of numbers keyed by <venue>:<kind>:<symbol>")
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
        let mut entries = BTreeMap::new();
        while let Some((key, value)) = map.next_entry::<String, f64>()? {
            if key.is_empty() {
                return Err(A::Error::custom("a market with no name"));
            }
            let instrument: InstrumentRef = key.parse().map_err(A::Error::custom)?;
            if entries.insert(instrument, value).is_some() {
                return Err(A::Error::custom(format!("{key} is given twice")));
            }
        }
        Ok(entries)
    }
}

impl Session {
    /// The positions the session starts the account with: none if the
    /// task sets none.
    #[inline]
    pub fn start_positions(&self) -> Result<StartPositions, SessionVarError> {
        self.var(StartPositions::VAR)
            .map_or(Ok(StartPositions::default()), StartPositions::parse)
    }

    /// The order the session asks the bot to work, if any.
    #[inline]
    pub fn order(&self) -> Result<Option<SessionOrder>, SessionVarError> {
        self.var(SessionOrder::VAR)
            .map(SessionOrder::parse)
            .transpose()
    }

    /// The broker's haircut on what the order leaves undone, if the
    /// session has an order.
    #[inline]
    pub fn broker_haircut(&self) -> Result<Option<BrokerHaircut>, SessionVarError> {
        self.var(BrokerHaircut::VAR)
            .map(BrokerHaircut::parse)
            .transpose()
    }

    /// The markets a broker closes out at the end: none if the task sets
    /// none.
    #[inline]
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

    fn market(name: &str) -> InstrumentRef {
        name.parse().unwrap()
    }

    #[test]
    fn each_input_reads_back_what_was_written() {
        // A perp and a spot market of one symbol stay apart.
        let positions = StartPositions(
            [
                (market("phoenix:perp:NVDA"), -480.0),
                (market("phoenix:spot:NVDA"), 3.0),
            ]
            .into(),
        );
        let order = SessionOrder {
            instrument: market("phoenix:perp:NVDA"),
            side: Side::Bid,
            quantity: 62.0,
            reduce_only: true,
        };
        let haircut = BrokerHaircut(30.0);
        let close_outs = CloseOuts([(market("phoenix:perp:SPY"), 10.0)].into());
        assert_eq!(
            order.to_var().unwrap(),
            r#"{"instrument":"phoenix:perp:NVDA","side":"buy","quantity":62.0,"reduce_only":true}"#
        );
        assert_eq!(
            positions.to_var().unwrap(),
            r#"{"phoenix:perp:NVDA":-480.0,"phoenix:spot:NVDA":3.0}"#
        );
        let s = session(&[
            (StartPositions::VAR, &positions.to_var().unwrap()),
            (SessionOrder::VAR, &order.to_var().unwrap()),
            (BrokerHaircut::VAR, &haircut.to_var().unwrap()),
            (CloseOuts::VAR, &close_outs.to_var().unwrap()),
        ]);
        let read = s.start_positions().unwrap();
        assert_eq!(read, positions);
        assert_eq!(read.get(&market("phoenix:perp:NVDA")), -480.0);
        assert_eq!(read.get(&market("phoenix:spot:NVDA")), 3.0);
        assert_eq!(read.get(&market("phoenix:perp:SPY")), 0.0);
        assert_eq!(s.order().unwrap(), Some(order));
        assert_eq!(s.broker_haircut().unwrap(), Some(haircut));
        assert_eq!(haircut.fraction(), 0.003);
        assert_eq!(
            s.close_outs().unwrap().haircut(&market("phoenix:perp:SPY")),
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
            r#"{"instrument":"phoenix:perp:NVDA","side":"buy","quantity":1,"quantity":2}"#,
        ] {
            let error = SessionOrder::parse(bad).unwrap_err();
            assert_eq!(error.var, "MTX_ORDER", "{bad}");
        }
        assert!(BrokerHaircut::parse("-1").is_err());
        assert!(BrokerHaircut::parse("thirty").is_err());
        assert_eq!(BrokerHaircut::parse(" 12.5 ").unwrap(), BrokerHaircut(12.5));
        assert!(CloseOuts::parse(r#"{"phoenix:perp:SPY":-1}"#).is_err());
        // A market given twice, the empty name, a bare symbol, a value that
        // isn't a number: errors, never the last one winning.
        for bad in [
            r#"{"phoenix:perp:NVDA":1,"phoenix:perp:NVDA":2}"#,
            r#"{"":1}"#,
            r#"{"NVDA":1}"#,
            r#"{"phoenix:perp:NVDA":"long"}"#,
        ] {
            assert_eq!(
                StartPositions::parse(bad).unwrap_err().var,
                StartPositions::VAR,
                "{bad}"
            );
            assert_eq!(
                CloseOuts::parse(bad).unwrap_err().var,
                CloseOuts::VAR,
                "{bad}"
            );
        }
        let s = session(&[(SessionOrder::VAR, "{")]);
        assert!(s.order().is_err());
    }

    #[test]
    fn writing_refuses_what_json_cant_carry() {
        let nvda = market("phoenix:perp:NVDA");
        for bad in [f64::NAN, f64::INFINITY] {
            assert!(
                StartPositions([(nvda.clone(), bad)].into())
                    .to_var()
                    .is_err()
            );
            assert!(CloseOuts([(nvda.clone(), bad)].into()).to_var().is_err());
            assert!(BrokerHaircut(bad).to_var().is_err());
            let order = SessionOrder {
                instrument: nvda.clone(),
                side: Side::Ask,
                quantity: bad,
                reduce_only: false,
            };
            assert!(order.to_var().is_err());
        }
        assert!(CloseOuts([(nvda, -1.0)].into()).to_var().is_err());
        assert!(BrokerHaircut(-1.0).to_var().is_err());
    }
}
