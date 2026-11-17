//! The scenario generators, by name.
//!
//! A task's `[metadata.mtx] generator` names one of these. Each draws its
//! practice set (`mtx scenario generate`) and its hidden set (`mtx grade
//! --draw`); to add a generator, write its family and add one [`Generator`]
//! to [`GENERATORS`].
//!
//! A hidden set says each session's latent state ([`Hidden::latent`]): what
//! the task's informed bot, scoring v2's 1, is told. It is the session's
//! regime as a perfect reader of the task's data could know it, never the
//! session's future (where a move ends, when an order comes): the generator
//! picks those fields, so the informed bot is held to them by construction.

use serde_json::{Value, json};

use crate::{
    carry::{CarryFamily, Shape as CarryShape},
    error::Result,
    making::MakingFamily,
    scenario::{Scenario, Shape, ShockFamily},
    weekend::{WeekendFamily, WeekendShape},
};

/// A hidden set as `mtx grade --draw` runs it (and a practice set as
/// `mtx calibrate --practice` runs the informed bot on it).
pub struct Hidden {
    pub scenarios: Vec<Scenario>,
    /// What each scenario was drawn as, for its detail file.
    pub annotations: Vec<Value>,
    /// Each scenario's regime as MTX_REGIME, when the draw was told to hand
    /// it out: everything it was drawn as, for the ladder's oracles.
    pub regimes: Option<Vec<String>>,
    /// Each scenario's latent state, as JSON: what the task's informed bot
    /// is handed as MTX_REGIME (`mtx grade --informed`). Drawn the same
    /// whether or not anyone is told, so telling changes no session.
    pub latent: Vec<String>,
}

pub struct Generator {
    pub name: &'static str,
    /// `count` practice scenarios named `<prefix>-<n>`, drawn with each
    /// one's latent state, for `mtx calibrate --practice` to run the informed
    /// bot on them. Host-side only: the latent state is never written to a
    /// practice file. [`Generator::practice`] is its scenarios.
    pub practice_drawn: fn(prefix: &str, count: usize, seed: u64) -> Result<Hidden>,
    /// `count` hidden scenarios named `hidden-<n>`.
    pub hidden: fn(count: usize, seed: u64, tell_regime: bool) -> Result<Hidden>,
    /// Every parameter of the families this generator draws from, as text
    /// (their `Debug`): part of a task's fingerprint, so a parameter no
    /// drawn scenario shows still moves it.
    pub params: fn() -> String,
}

impl Generator {
    /// `count` practice scenarios named `<prefix>-<n>`: the scenarios of
    /// [`Generator::practice_drawn`], so the two are one draw.
    pub fn practice(&self, prefix: &str, count: usize, seed: u64) -> Vec<Scenario> {
        (self.practice_drawn)(prefix, count, seed)
            .expect("a practice draw is serialisable")
            .scenarios
    }

    /// The description a shipped practice file of a v2 or guarded task
    /// carries: the symbol and nothing the generator knows of the session,
    /// not even the family's name (`funding-carry` holds a latent field's
    /// name). A generator's own description states the session's latent
    /// state in prose (who is informed, from which account, the process's
    /// parameters), and the file goes into the agent's image.
    pub fn neutral_description(&self, symbol: &str) -> String {
        format!("{symbol} practice session")
    }
}

pub const GENERATORS: &[Generator] = &[
    Generator {
        name: "flash-crash",
        practice_drawn: |prefix, count, seed| {
            shaped(
                ShockFamily::spcx().draw_set(prefix, count, seed),
                false,
                shock_latent,
            )
        },
        hidden: |count, seed, tell| {
            shaped(
                ShockFamily::spcx().draw_set("hidden", count, seed),
                tell,
                shock_latent,
            )
        },
        params: || format!("{:?}", ShockFamily::spcx()),
    },
    Generator {
        name: "market-making",
        practice_drawn: |prefix, count, seed| {
            making_drawn(&MakingFamily::spcx(), prefix, count, seed, false)
        },
        hidden: |count, seed, tell| {
            making_drawn(&MakingFamily::spcx(), "hidden", count, seed, tell)
        },
        params: || format!("{:?}", MakingFamily::spcx()),
    },
    Generator {
        name: "market-making-v2",
        practice_drawn: |prefix, count, seed| {
            making_drawn(&MakingFamily::v2(), prefix, count, seed, false)
        },
        hidden: |count, seed, tell| making_drawn(&MakingFamily::v2(), "hidden", count, seed, tell),
        params: || format!("{:?}", MakingFamily::v2()),
    },
    Generator {
        name: "funding-carry",
        practice_drawn: |prefix, count, seed| {
            shaped(
                CarryFamily::practice().draw_set(prefix, count, seed),
                false,
                carry_latent,
            )
        },
        hidden: |count, seed, tell| {
            shaped(
                CarryFamily::spcx().draw_set("hidden", count, seed),
                tell,
                carry_latent,
            )
        },
        params: || {
            format!(
                "{:?} practice {:?}",
                CarryFamily::spcx(),
                CarryFamily::practice()
            )
        },
    },
    Generator {
        name: "weekend-gap",
        practice_drawn: |prefix, count, seed| {
            shaped(
                WeekendFamily::spcx().draw_set(prefix, count, seed),
                false,
                weekend_latent,
            )
        },
        hidden: |count, seed, tell| {
            shaped(
                WeekendFamily::spcx().draw_set("hidden", count, seed),
                tell,
                weekend_latent,
            )
        },
        params: || format!("{:?}", WeekendFamily::spcx()),
    },
];

/// The generator called `name`.
pub fn generator(name: &str) -> Option<&'static Generator> {
    GENERATORS.iter().find(|g| g.name == name)
}

/// Every generator's name.
pub fn names() -> Vec<&'static str> {
    GENERATORS.iter().map(|g| g.name).collect()
}

/// A market-making hidden set, each scenario annotated with its informed
/// orders and, when told, the regime as those orders by slot and side: what a
/// perfect reader of the tape would know.
fn making_drawn(
    family: &MakingFamily,
    prefix: &str,
    count: usize,
    seed: u64,
    tell: bool,
) -> Result<Hidden> {
    let drawn = family.sample_set_labeled(prefix, count, seed);
    let annotations = drawn
        .iter()
        .map(|(_, informed)| json!({ "informed": informed }))
        .collect();
    let regimes = if tell {
        Some(
            drawn
                .iter()
                .map(|(scenario, informed)| {
                    let orders: Vec<_> = scenario
                        .making
                        .iter()
                        .flat_map(|m| m.takers.iter())
                        .zip(informed)
                        .filter(|(_, informed)| **informed)
                        .map(|(o, _)| json!({"slot": o.slot, "side": o.side}))
                        .collect();
                    Ok(serde_json::to_string(&orders)?)
                })
                .collect::<Result<_>>()?,
        )
    } else {
        None
    };
    // No account is informed for the session: whether an order is informed
    // is the order's own draw, the session's future.
    let latent = drawn.iter().map(|_| "{}".to_string()).collect();
    Ok(Hidden {
        scenarios: drawn.into_iter().map(|(scenario, _)| scenario).collect(),
        annotations,
        regimes,
        latent,
    })
}

/// A hidden set drawn with each scenario's shape: annotated `{"shape": ..}`,
/// with its `latent` state and, when told, the shape as the regime.
fn shaped<S: serde::Serialize>(
    drawn: Vec<(Scenario, S)>,
    tell: bool,
    latent: fn(&S) -> Value,
) -> Result<Hidden> {
    let annotations = drawn
        .iter()
        .map(|(_, shape)| json!({ "shape": shape }))
        .collect();
    let regimes = if tell {
        Some(
            drawn
                .iter()
                .map(|(_, shape)| Ok(serde_json::to_string(shape)?))
                .collect::<Result<_>>()?,
        )
    } else {
        None
    };
    let latent = drawn
        .iter()
        .map(|(_, shape)| Ok(serde_json::to_string(&latent(shape))?))
        .collect::<Result<_>>()?;
    Ok(Hidden {
        scenarios: drawn.into_iter().map(|(scenario, _)| scenario).collect(),
        annotations,
        regimes,
        latent,
    })
}

/// Why a field of a session's latent state is knowable in principle
/// (WAVE2_11 Part B, change 8), and so may be told to the informed bot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Knowable {
    /// Who the actors are: whether an informed trader or a one-sided worker
    /// is seated, and from which of the grader's accounts.
    Identity,
    /// The process the session's randomness is drawn from: the funding
    /// process, the informed share and lags, the impact and noise curves.
    Process,
}

/// Every field a family's latent state holds, and why it is knowable. A
/// session's realised draws (where a shock goes, which way flow trades, when
/// an episode or an order comes) are its future, and never here.
///
/// A family `"*"` names a field any family may tell (see
/// [`crate::latent::Latent`]): who is who, resolved to the session's wallets
/// as it starts.
pub const LATENT_FIELDS: &[(&str, &str, Knowable)] = &[
    ("*", crate::latent::ROSTER, Knowable::Identity),
    ("funding-carry", "informed_share", Knowable::Process),
    ("funding-carry", "lag_slots", Knowable::Process),
    ("funding-carry", "hold_slots", Knowable::Process),
    ("funding-carry", "funding", Knowable::Process),
    ("funding-carry", "cap_pct", Knowable::Process),
    ("funding-carry", "noise_pct", Knowable::Process),
    ("weekend-gap", "informed", Knowable::Identity),
    ("weekend-gap", "informed_account", Knowable::Identity),
    ("weekend-gap", "worker", Knowable::Identity),
    ("weekend-gap", "worker_account", Knowable::Identity),
    ("weekend-gap", "impact_pct", Knowable::Process),
    ("weekend-gap", "half_life_slots", Knowable::Process),
];

/// The members of a latent state itself, which no shipped practice file may
/// hold outside `actors`: the latent object, the roster and the resolved
/// wallets. Identity fields such as `informed` and `worker` are not here:
/// they are also the actor library's own configuration keys
/// (`actors.informed`).
pub const RESERVED_OUTSIDE_ACTORS: [&str; 4] = ["latent", "roster", "wallets", "roles"];

/// Remove the per-account role maps (`roles`, as `making.roles` of a family
/// that names its informed accounts) a scenario holds outside `actors`.
/// Only a host-side informed run needs them; a practice file saved with one
/// would name the informed account. `actors.*.roles` (a list of role names)
/// stays.
pub fn strip_identity_roles(value: &mut Value) {
    if let Value::Object(map) = value {
        map.remove("roles");
        for (key, member) in map.iter_mut() {
            if key != "actors" {
                strip_identity_roles(member);
            }
        }
    } else if let Value::Array(items) = value {
        items.iter_mut().for_each(strip_identity_roles);
    }
}

/// The reserved members ([`RESERVED_OUTSIDE_ACTORS`]) a practice file's JSON
/// holds as a key anywhere outside its `actors`.
pub fn latent_keys_in(text: &str) -> Vec<&'static str> {
    fn walk(value: &Value, found: &mut Vec<&'static str>) {
        match value {
            Value::Object(map) => {
                for (key, member) in map {
                    if let Some(reserved) = RESERVED_OUTSIDE_ACTORS.iter().find(|r| **r == key)
                        && !found.contains(reserved)
                    {
                        found.push(reserved);
                    }
                    if key != "actors" {
                        walk(member, found);
                    }
                }
            }
            Value::Array(items) => items.iter().for_each(|i| walk(i, found)),
            _ => {}
        }
    }
    let mut found = Vec::new();
    if let Ok(value) = serde_json::from_str::<Value>(text) {
        walk(&value, &mut found);
    }
    found
}

/// What a practice file's strings state of a latent state: a string value
/// outside `actors` that names one of the family's latent fields
/// (`informed_account`, or "informed account") or an account ("account 4").
/// The generator's own descriptions state the session's latent values this
/// way, so this finds them in prose.
pub fn latent_text_in(family: &str, text: &str) -> Vec<String> {
    fn strings<'a>(value: &'a Value, out: &mut Vec<&'a str>) {
        match value {
            Value::String(s) => out.push(s),
            Value::Object(map) => {
                for (key, member) in map {
                    if key != "actors" {
                        strings(member, out);
                    }
                }
            }
            Value::Array(items) => items.iter().for_each(|i| strings(i, out)),
            _ => {}
        }
    }
    let Ok(value) = serde_json::from_str::<Value>(text) else {
        return Vec::new();
    };
    let mut found = Vec::new();
    strings(&value, &mut found);
    let fields: Vec<&str> = LATENT_FIELDS
        .iter()
        .filter(|(f, ..)| *f == family)
        .map(|(_, field, _)| *field)
        .collect();
    let mut hits: Vec<String> = Vec::new();
    for string in found {
        let lower = string.to_lowercase();
        for field in &fields {
            let named = lower.contains(field) || lower.contains(&field.replace('_', " "));
            if named && !hits.iter().any(|h| h == field) {
                hits.push(field.to_string());
            }
        }
        let mut rest = lower.as_str();
        while let Some(at) = rest.find("account ") {
            rest = &rest[at + 8..];
            if rest.starts_with(|c: char| c.is_ascii_digit())
                && !hits.iter().any(|h| h == "an account")
            {
                hits.push("an account".to_string());
            }
        }
    }
    hits
}

/// Nothing: a session's shock (its direction, legs and aftermath) is the
/// one draw that makes it, so all of it is the session's future.
fn shock_latent(_: &Shape) -> Value {
    json!({})
}

/// The session's funding process and how its episodes behave: the informed
/// share, the index's lag, how long a crowded premium holds, funding's
/// settings and cap, and the index's noise. Not the episodes themselves:
/// when each comes, which kind it is and how far it goes.
fn carry_latent(shape: &CarryShape) -> Value {
    json!({
        "informed_share": shape.informed_share,
        "lag_slots": shape.lag_slots,
        "hold_slots": shape.hold_slots,
        "funding": shape.funding,
        "cap_pct": shape.cap_pct,
        "noise_pct": shape.noise_pct,
    })
}

/// Who is who: whether an informed trader and a one-sided worker are
/// seated, from which accounts, and the impact curve. Not which way the
/// worker trades, where the weekend took the market, when anyone trades, or
/// the path.
fn weekend_latent(shape: &WeekendShape) -> Value {
    json!({
        "informed": shape.informed,
        "informed_account": shape.informed_account,
        "worker": shape.worker,
        "worker_account": shape.worker_account,
        "impact_pct": shape.impact_pct,
        "half_life_slots": shape.half_life_slots,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_unique_and_found() {
        let names = names();
        for name in &names {
            assert_eq!(names.iter().filter(|n| *n == name).count(), 1, "{name}");
            assert!(generator(name).is_some());
        }
        assert!(generator("nope").is_none());
    }

    #[test]
    fn every_generator_draws_both_sets_the_same_way_twice() {
        for g in GENERATORS {
            let a = g.practice("practice", 4, 7);
            let b = g.practice("practice", 4, 7);
            assert_eq!(a.len(), 4, "{}", g.name);
            assert_eq!(
                serde_json::to_string(&a).unwrap(),
                serde_json::to_string(&b).unwrap(),
                "{}",
                g.name
            );
            let hidden = (g.hidden)(4, 7, true).unwrap();
            assert_eq!(hidden.scenarios.len(), 4, "{}", g.name);
            assert_eq!(hidden.annotations.len(), 4, "{}", g.name);
            assert_eq!(hidden.regimes.as_ref().map(Vec::len), Some(4), "{}", g.name);
            let untold = (g.hidden)(4, 7, false).unwrap();
            assert!(untold.regimes.is_none());
            // Telling changes no session, and the latent state is drawn
            // either way.
            assert_eq!(
                serde_json::to_string(&hidden.scenarios).unwrap(),
                serde_json::to_string(&untold.scenarios).unwrap(),
                "{}",
                g.name
            );
            assert_eq!(hidden.latent, untold.latent, "{}", g.name);
            assert_eq!(hidden.latent.len(), 4, "{}", g.name);
        }
    }

    #[test]
    fn a_family_parameter_a_small_draw_never_reaches_still_shows_in_the_params() {
        let spcx = ShockFamily::spcx();
        let wider = ShockFamily {
            extend_pct: (1.0, 9.0),
            ..ShockFamily::spcx()
        };
        let draw = |family: &ShockFamily, count| {
            serde_json::to_string(
                &family
                    .draw_set("hidden", count, 1)
                    .into_iter()
                    .map(|d| d.0)
                    .collect::<Vec<_>>(),
            )
            .unwrap()
        };
        // Six scenarios have no extension in them: the draw alone can't see it.
        assert_eq!(draw(&spcx, 6), draw(&wider, 6));
        // The verifier's own 72 do, and the params text always does.
        assert_ne!(draw(&spcx, 72), draw(&wider, 72));
        assert_ne!(format!("{spcx:?}"), format!("{wider:?}"));
        for g in GENERATORS {
            assert!(!(g.params)().is_empty(), "{}", g.name);
            assert_eq!((g.params)(), (g.params)(), "{}", g.name);
        }
    }

    /// A practice set has each scenario's latent state beside it for the
    /// informed bot, and the practice set `mtx scenario generate` writes is
    /// its scenarios (`practice` is derived from `practice_drawn`).
    #[test]
    fn a_practice_set_is_drawn_with_its_latent_state() {
        for g in GENERATORS {
            let drawn = (g.practice_drawn)("practice", 4, 7).unwrap();
            assert_eq!(drawn.latent.len(), 4, "{}", g.name);
            assert_eq!(drawn.scenarios.len(), 4, "{}", g.name);
            assert!(drawn.regimes.is_none());
        }
    }

    /// A latent state is in a practice file as its members or in prose. The
    /// reserved members are caught outside `actors` and only there: the actor
    /// library's own `informed` and `worker` keys are configuration.
    #[test]
    fn a_latent_in_a_practice_file_is_caught_and_an_actors_config_is_not() {
        assert_eq!(latent_keys_in(r#"{"roster": ["informed"]}"#), ["roster"]);
        assert_eq!(latent_keys_in(r#"{"x": {"latent" : {}}}"#), ["latent"]);
        assert_eq!(latent_keys_in(r#"{"wallets": {}}"#), ["wallets"]);
        assert!(latent_keys_in(r#"{"note": "latent"}"#).is_empty());
        // A per-account role map outside `actors` (a c1-shaped
        // `making.roles`) names the informed account; `actors.pool.roles`
        // is a list of role names.
        let c1 = r#"{"making": {"takers": [], "roles": {"informed": [7]}}}"#;
        assert_eq!(latent_keys_in(c1), ["roles"]);
        assert!(
            latent_keys_in(r#"{"actors": {"pool": {"wallets": 3, "roles": ["informed"]}}}"#)
                .is_empty()
        );
        let mut value: Value = serde_json::from_str(
            r#"{"making": {"roles": {"informed": [7]}, "x": 1}, "actors": {"pool": {"roles": ["informed"]}}}"#,
        )
        .unwrap();
        strip_identity_roles(&mut value);
        assert_eq!(
            value,
            serde_json::json!({"making": {"x": 1}, "actors": {"pool": {"roles": ["informed"]}}})
        );
        let pooled = r#"{"actors": {"informed": {"share": 0.2}, "worker": {"qty": 3},
            "noise": {"wallets": 2}}, "description": "a pool"}"#;
        assert!(latent_keys_in(pooled).is_empty());
        assert!(latent_text_in("weekend-gap", pooled).is_empty());
        // A renamed identity key is no loophole: the structural check
        // (`Scenario` refuses unknown members) is what rejects it.
        let weekend = generator("weekend-gap").unwrap().practice("practice", 6, 5);
        let told = weekend.iter().any(|s| {
            let text = serde_json::to_string(s).unwrap();
            !latent_text_in("weekend-gap", &text).is_empty()
        });
        assert!(told, "the generator's description states a latent");
        assert_eq!(
            latent_text_in(
                "weekend-gap",
                r#"{"description": "closed, informed flow from account 4"}"#
            ),
            ["informed", "an account"]
        );
        assert!(
            latent_text_in("funding-carry", r#"{"description": "informed share 0.22"}"#)
                .contains(&"informed_share".to_string())
        );
    }

    /// The neutral description names the symbol and states nothing a latent
    /// field names.
    #[test]
    fn the_neutral_description_states_no_latent() {
        for g in GENERATORS {
            let description = g.neutral_description("SPCX");
            let json = serde_json::json!({ "description": description }).to_string();
            assert!(latent_text_in(g.name, &json).is_empty(), "{description}");
        }
    }

    /// The informed bot is told the session's latent state and nothing of
    /// its future. Every latent field is either classified as knowable in
    /// principle ([`LATENT_FIELDS`]) or a family constant, the same in every
    /// session: a field that changes from session to session and isn't an
    /// actor's identity or the process is one of the session's draws.
    #[test]
    fn the_latent_state_is_the_regime_and_never_the_future() {
        for g in GENERATORS {
            let hidden = (g.hidden)(64, 11, true).unwrap();
            let latent: Vec<serde_json::Map<String, Value>> = hidden
                .latent
                .iter()
                .map(|l| serde_json::from_str(l).unwrap())
                .collect();
            let classified: Vec<&str> = LATENT_FIELDS
                .iter()
                .filter(|(family, ..)| *family == g.name || *family == "*")
                .map(|(_, field, _)| *field)
                .collect();
            for session in &latent {
                for (field, value) in session {
                    let constant = latent.iter().all(|other| other.get(field) == Some(value));
                    assert!(
                        classified.contains(&field.as_str()) || constant,
                        "{}: `{field}` is a draw of the session, not knowable in principle",
                        g.name
                    );
                }
            }
            // Each classified field is in the latent state, and none of the
            // regime's other fields (the oracles' tell) is.
            // (Market making's regime is a list of orders: no fields.)
            let regime: serde_json::Map<String, Value> = hidden
                .regimes
                .as_ref()
                .and_then(|r| serde_json::from_str(&r[0]).ok())
                .unwrap_or_default();
            for field in classified.iter().filter(|f| **f != crate::latent::ROSTER) {
                assert!(latent[0].contains_key(*field), "{}: {field}", g.name);
            }
            for field in regime.keys() {
                assert!(
                    classified.contains(&field.as_str()) || !latent[0].contains_key(field),
                    "{}: {field}",
                    g.name
                );
            }
        }
        // The draws themselves are withheld: a shock's direction, legs and
        // aftermath, and which way weekend-gap's worker trades.
        let latent = |name: &str| {
            (generator(name).unwrap().hidden)(8, 3, false)
                .unwrap()
                .latent
        };
        assert!(latent("flash-crash").iter().all(|l| l == "{}"));
        assert!(
            latent("weekend-gap")
                .iter()
                .all(|l| !l.contains("worker_side"))
        );
        for name in ["market-making", "market-making-v2"] {
            assert!(latent(name).iter().all(|l| l == "{}"), "{name}");
        }
    }

    /// The check above would catch a draw handed over: a shock's direction
    /// varies from session to session and is classified as nothing.
    #[test]
    fn a_shock_s_direction_is_a_draw() {
        let drawn = ShockFamily::spcx().draw_set("hidden", 64, 11);
        let first = serde_json::to_value(drawn[0].1.direction).unwrap();
        assert!(
            drawn
                .iter()
                .any(|(_, shape)| serde_json::to_value(shape.direction).unwrap() != first)
        );
        assert!(
            !LATENT_FIELDS
                .iter()
                .any(|(family, ..)| *family == "flash-crash")
        );
    }
}
