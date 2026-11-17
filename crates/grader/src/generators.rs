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
    basket_unwind::{BasketFamily, Shape as BasketShape, informed_kind},
    carry::{CarryFamily, Shape as CarryShape},
    error::Result,
    making::MakingFamily,
    scenario::{Scenario, Shape, ShockFamily},
    weekend::{WeekendFamily, WeekendShape},
};

/// A hidden set as `mtx grade --draw` runs it.
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
    /// `count` practice scenarios named `<prefix>-<n>`.
    pub practice: fn(prefix: &str, count: usize, seed: u64) -> Vec<Scenario>,
    /// `count` hidden scenarios named `hidden-<n>`.
    pub hidden: fn(count: usize, seed: u64, tell_regime: bool) -> Result<Hidden>,
}

pub const GENERATORS: &[Generator] = &[
    Generator {
        name: "flash-crash",
        practice: |prefix, count, seed| ShockFamily::spcx().sample_set(prefix, count, seed),
        hidden: |count, seed, tell| {
            shaped(
                ShockFamily::spcx().draw_set("hidden", count, seed),
                tell,
                shock_latent,
            )
        },
    },
    Generator {
        name: "market-making",
        practice: |prefix, count, seed| MakingFamily::spcx().sample_set(prefix, count, seed),
        hidden: |count, seed, tell| making_hidden(&MakingFamily::spcx(), count, seed, tell),
    },
    Generator {
        name: "market-making-v2",
        practice: |prefix, count, seed| MakingFamily::v2().sample_set(prefix, count, seed),
        hidden: |count, seed, tell| making_hidden(&MakingFamily::v2(), count, seed, tell),
    },
    Generator {
        name: "funding-carry",
        practice: |prefix, count, seed| CarryFamily::practice().sample_set(prefix, count, seed),
        hidden: |count, seed, tell| {
            shaped(
                CarryFamily::spcx().draw_set("hidden", count, seed),
                tell,
                carry_latent,
            )
        },
    },
    Generator {
        name: "basket-unwind",
        practice: |prefix, count, seed| BasketFamily::nvda_tsla().sample_set(prefix, count, seed),
        hidden: |count, seed, tell| {
            shaped(
                BasketFamily::nvda_tsla().draw_set("hidden", count, seed),
                tell,
                basket_latent,
            )
        },
    },
    Generator {
        name: "weekend-gap",
        practice: |prefix, count, seed| WeekendFamily::spcx().sample_set(prefix, count, seed),
        hidden: |count, seed, tell| {
            shaped(
                WeekendFamily::spcx().draw_set("hidden", count, seed),
                tell,
                weekend_latent,
            )
        },
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
fn making_hidden(family: &MakingFamily, count: usize, seed: u64, tell: bool) -> Result<Hidden> {
    let drawn = family.sample_set_labeled("hidden", count, seed);
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

/// Who is who: each market's informed trader's wallet, which the session
/// seats in every session, calm or not. Not which market moves, which way,
/// when or how far, nor who sends the fake-outs.
fn basket_latent(shape: &BasketShape) -> Value {
    let kinds: Vec<String> = shape
        .informed_accounts
        .keys()
        .map(|s| informed_kind(s))
        .collect();
    json!({ "roster": kinds })
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
            let a = (g.practice)("practice", 4, 7);
            let b = (g.practice)("practice", 4, 7);
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
