//! What a session's latent state is, and how the informed bot is told it.
//!
//! A family that draws per-session roles (which actors are informed, which
//! are decoys, who hunts) describes what a perfect reader of the task's data
//! could know of the session as a [`Latent`]: a set of fields, each declared
//! [`Knowable`] as an identity or a process. A session's realised draws (a
//! shock's direction, when an order comes, a path) have no way in: a field
//! can't be added without saying why it is knowable in principle.
//!
//! Who is who needs the session's own addresses, which no one knows before
//! it starts (they come from the grade's secret). So a latent names the
//! *kinds* of actor to tell ([`Latent::roster`]), and the session fills in
//! their wallets as it starts ([`resolve`]): `{"roster": ["informed"]}` is
//! handed over as `{"wallets": {"informed": ["<pubkey>", ..]}}`.
//!
//! The grader hands the latent to the informed bot as `MTX_REGIME`, and to no
//! other (see `docs/gates.md`). A regime with no `roster` is handed over
//! byte for byte as it is.

use serde::Serialize;
use serde_json::{Map, Value};

use crate::actors::library::Roster;

/// Why a field of a session's latent state is knowable in principle, and so
/// may be told to the informed bot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Knowable {
    /// Who the actors are: which wallets are informed, decoys or hunters,
    /// which RFQ clients are informed.
    Identity,
    /// The process the session's randomness is drawn from: lags, shares,
    /// half-lives, latencies, hold times.
    Process,
}

/// The key a latent names the kinds to tell the wallets of under, and the
/// key their wallets come back under.
pub const ROSTER: &str = "roster";
pub const WALLETS: &str = "wallets";

/// A session's latent state, as the informed bot is told it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Latent {
    values: Map<String, Value>,
    fields: Vec<(String, Knowable)>,
}

impl Latent {
    pub fn new() -> Self {
        Self::default()
    }

    /// An identity fact: `field` is `value`.
    pub fn identity(self, field: &str, value: impl Serialize) -> Result<Self, String> {
        self.with(field, Knowable::Identity, value)
    }

    /// A fact of the session's process: `field` is `value`.
    pub fn process(self, field: &str, value: impl Serialize) -> Result<Self, String> {
        self.with(field, Knowable::Process, value)
    }

    /// Tell the wallets of these kinds of actor (`informed`, `decoy`,
    /// `stop-hunter`, `rfq-informed`, …), as the session sets them. An
    /// identity fact; without one, nothing says who is who.
    pub fn roster(self, kinds: &[&str]) -> Result<Self, String> {
        self.with(ROSTER, Knowable::Identity, kinds)
    }

    fn with(mut self, field: &str, why: Knowable, value: impl Serialize) -> Result<Self, String> {
        if field.is_empty() || field == WALLETS {
            return Err(format!("`{field}` can't be a latent field"));
        }
        if self.values.contains_key(field) {
            return Err(format!("latent field `{field}` is given twice"));
        }
        let value = serde_json::to_value(value).map_err(|e| format!("latent `{field}`: {e}"))?;
        if !sound(&value) {
            return Err(format!("latent `{field}` isn't a finite value"));
        }
        self.values.insert(field.to_string(), value);
        self.fields.push((field.to_string(), why));
        Ok(self)
    }

    /// Each field and why it is knowable, in the order given.
    pub fn fields(&self) -> &[(String, Knowable)] {
        &self.fields
    }

    pub fn to_json(&self) -> String {
        Value::Object(self.values.clone()).to_string()
    }
}

/// Whether `value` holds only what a regime can say: no null (what a
/// non-finite number serialises as), anywhere in it.
fn sound(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Array(items) => items.iter().all(sound),
        Value::Object(fields) => fields.values().all(sound),
        _ => true,
    }
}

/// `regime`, the latent as a hidden set drew it, with its `roster` (the
/// kinds to tell) replaced by those kinds' wallets in `session`. A regime
/// that isn't a JSON object, or has no `roster`, is returned as it is.
pub fn resolve(regime: &str, session: &Roster) -> Result<String, String> {
    let Ok(Value::Object(mut latent)) = serde_json::from_str::<Value>(regime) else {
        return Ok(regime.to_string());
    };
    let Some(kinds) = latent.remove(ROSTER) else {
        return Ok(regime.to_string());
    };
    if latent.contains_key(WALLETS) {
        return Err(format!("the latent has `{WALLETS}` and a `{ROSTER}`"));
    }
    let Value::Array(kinds) = kinds else {
        return Err(format!("`{ROSTER}` must be a list of kinds"));
    };
    let mut told = Map::new();
    for kind in kinds {
        let Value::String(kind) = kind else {
            return Err(format!("`{ROSTER}` must be a list of kinds"));
        };
        let Some(names) = session.get(&kind) else {
            return Err(format!("the session has no `{kind}` to tell"));
        };
        told.insert(kind, names.iter().cloned().map(Value::String).collect());
    }
    latent.insert(WALLETS.to_string(), Value::Object(told));
    Ok(Value::Object(latent).to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session() -> Roster {
        Roster::from([
            (
                "informed".to_string(),
                vec!["A".to_string(), "B".to_string()],
            ),
            ("decoy".to_string(), vec!["C".to_string()]),
            ("rfq-informed".to_string(), vec!["c3".to_string()]),
            ("noise".to_string(), Vec::new()),
        ])
    }

    #[test]
    fn every_field_says_why_it_is_knowable_and_nothing_else_gets_in() {
        let latent = Latent::new()
            .process("horizon_slots", 40)
            .unwrap()
            .identity("hunters", 2)
            .unwrap()
            .roster(&["informed", "decoy"])
            .unwrap();
        assert_eq!(
            latent.fields(),
            [
                ("horizon_slots".to_string(), Knowable::Process),
                ("hunters".to_string(), Knowable::Identity),
                ("roster".to_string(), Knowable::Identity),
            ]
        );
        let json: Value = serde_json::from_str(&latent.to_json()).unwrap();
        assert_eq!(json["horizon_slots"], 40);
        assert_eq!(json["roster"], serde_json::json!(["informed", "decoy"]));
    }

    #[test]
    fn a_bad_field_is_refused() {
        let new = Latent::new;
        assert!(new().process("", 1).is_err());
        assert!(new().process("wallets", 1).is_err());
        assert!(new().process("x", f64::NAN).is_err());
        assert!(new().process("x", f64::INFINITY).is_err());
        assert!(new().process("x", [1.0, f64::NAN]).is_err());
        assert!(new().process("x", Option::<u8>::None).is_err());
        assert!(
            new().process("x", 1).unwrap().identity("x", 2).is_err(),
            "twice"
        );
        assert!(new().roster(&["a"]).unwrap().roster(&["b"]).is_err());
    }

    #[test]
    fn the_roster_comes_back_as_the_sessions_wallets() {
        let latent = Latent::new()
            .process("horizon_slots", 40)
            .unwrap()
            .roster(&["informed", "rfq-informed", "noise"])
            .unwrap()
            .to_json();
        let told: Value = serde_json::from_str(&resolve(&latent, &session()).unwrap()).unwrap();
        assert_eq!(told["horizon_slots"], 40);
        assert!(told.get("roster").is_none());
        assert_eq!(
            told["wallets"],
            serde_json::json!({ "informed": ["A", "B"], "rfq-informed": ["c3"], "noise": [] })
        );
    }

    #[test]
    fn a_regime_without_a_roster_is_handed_over_as_it_is() {
        for regime in [
            r#"{"shape":{"kind":"x"}}"#,
            "not json",
            "[1, 2]",
            "  { \"a\" : 1 }  ",
        ] {
            assert_eq!(resolve(regime, &session()).unwrap(), regime);
        }
    }

    #[test]
    fn a_roster_the_session_cannot_fill_is_an_error_not_a_silent_gap() {
        let missing = Latent::new().roster(&["spoofer"]).unwrap().to_json();
        let error = resolve(&missing, &session()).unwrap_err();
        assert!(error.contains("spoofer"), "{error}");
        for bad in [
            r#"{"roster": "informed"}"#,
            r#"{"roster": [1]}"#,
            r#"{"roster": ["informed"], "wallets": {}}"#,
        ] {
            assert!(resolve(bad, &session()).is_err(), "{bad}");
        }
    }
}
