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

/// A shock's kind: which way, in how many legs, and whether its aftermath
/// reverts, stalls or extends. Not how far a leg goes, when it comes or
/// where the aftermath ends.
fn shock_latent(shape: &Shape) -> Value {
    json!({
        "direction": shape.direction,
        "aftermath": shape.aftermath,
        "legs": shape.legs,
    })
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

/// Who is who: whether an informed trader and a one-sided worker trade,
/// from which accounts and (the worker) which way, and the impact curve.
/// Not where the weekend took the market, when anyone trades, or the path.
fn weekend_latent(shape: &WeekendShape) -> Value {
    json!({
        "informed": shape.informed,
        "informed_account": shape.informed_account,
        "worker": shape.worker,
        "worker_account": shape.worker_account,
        "worker_side": shape.worker_side,
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
    /// its future: each family's latent fields are exactly these, and every
    /// field it leaves out is one that says where the session goes.
    #[test]
    fn the_latent_state_is_the_regime_and_never_the_future() {
        let fields = |name: &str| -> (Vec<String>, Vec<String>) {
            let hidden = (generator(name).unwrap().hidden)(6, 11, true).unwrap();
            let keys = |text: &str| -> Vec<String> {
                let mut keys: Vec<String> =
                    serde_json::from_str::<serde_json::Map<String, Value>>(text)
                        .unwrap()
                        .keys()
                        .cloned()
                        .collect();
                keys.sort();
                keys
            };
            let latent = keys(&hidden.latent[0]);
            for other in &hidden.latent {
                assert_eq!(keys(other), latent, "{name}");
            }
            let regime = keys(&hidden.regimes.unwrap()[0]);
            let withheld = regime.into_iter().filter(|k| !latent.contains(k)).collect();
            (latent, withheld)
        };
        let sorted = |keys: &[&str]| -> Vec<String> {
            let mut keys: Vec<String> = keys.iter().map(|k| k.to_string()).collect();
            keys.sort();
            keys
        };
        assert_eq!(
            fields("flash-crash"),
            (
                sorted(&["aftermath", "direction", "legs"]),
                sorted(&[
                    "cut",
                    "end_pct",
                    "extreme_pct",
                    "first_leg_pct",
                    "tail_slots"
                ])
            )
        );
        assert_eq!(
            fields("funding-carry"),
            (
                sorted(&[
                    "cap_pct",
                    "funding",
                    "hold_slots",
                    "informed_share",
                    "lag_slots",
                    "noise_pct"
                ]),
                sorted(&["episodes"])
            )
        );
        assert_eq!(
            fields("weekend-gap"),
            (
                sorted(&[
                    "half_life_slots",
                    "impact_pct",
                    "informed",
                    "informed_account",
                    "worker",
                    "worker_account",
                    "worker_side"
                ]),
                sorted(&[
                    "informed_slots",
                    "open_pct",
                    "path",
                    "permanent_pct",
                    "weekend_pct",
                    "worker_slots"
                ])
            )
        );
        // Market making's regime is the informed orders, each its own draw:
        // nothing of it is latent.
        for name in ["market-making", "market-making-v2"] {
            let hidden = (generator(name).unwrap().hidden)(6, 11, false).unwrap();
            assert!(hidden.latent.iter().all(|l| l == "{}"), "{name}");
        }
    }
}
