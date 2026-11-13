//! The scenario generators, by name.
//!
//! A task's `[metadata.mtx] generator` names one of these. Each draws its
//! practice set (`mtx scenario generate`) and its hidden set (`mtx grade
//! --draw`); to add a generator, write its family and add one [`Generator`]
//! to [`GENERATORS`].

use serde_json::{Value, json};

use crate::{
    carry::CarryFamily,
    error::Result,
    levcarry::LeveredCarryFamily,
    making::MakingFamily,
    scenario::{Scenario, ShockFamily},
    weekend::WeekendFamily,
};

/// A hidden set as `mtx grade --draw` runs it.
pub struct Hidden {
    pub scenarios: Vec<Scenario>,
    /// What each scenario was drawn as, for its detail file.
    pub annotations: Vec<Value>,
    /// Each scenario's regime as MTX_REGIME, when the draw was told to hand
    /// it out.
    pub regimes: Option<Vec<String>>,
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
            shaped(ShockFamily::spcx().draw_set("hidden", count, seed), tell)
        },
    },
    Generator {
        name: "market-making",
        practice: |prefix, count, seed| MakingFamily::spcx().sample_set(prefix, count, seed),
        hidden: |count, seed, tell| {
            let drawn = MakingFamily::spcx().sample_set_labeled("hidden", count, seed);
            let annotations = drawn
                .iter()
                .map(|(_, informed)| json!({ "informed": informed }))
                .collect();
            let regimes = if tell {
                // The informed orders, by slot and side: what a perfect
                // reader of the tape would know.
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
            Ok(Hidden {
                scenarios: drawn.into_iter().map(|(scenario, _)| scenario).collect(),
                annotations,
                regimes,
            })
        },
    },
    Generator {
        name: "funding-carry",
        practice: |prefix, count, seed| CarryFamily::practice().sample_set(prefix, count, seed),
        hidden: |count, seed, tell| {
            shaped(CarryFamily::spcx().draw_set("hidden", count, seed), tell)
        },
    },
    Generator {
        name: "levered-carry",
        practice: |prefix, count, seed| {
            LeveredCarryFamily::spcx().sample_set(prefix, count, seed)
        },
        hidden: |count, seed, tell| {
            shaped(
                LeveredCarryFamily::spcx().draw_set("hidden", count, seed),
                tell,
            )
        },
    },
    Generator {
        name: "weekend-gap",
        practice: |prefix, count, seed| WeekendFamily::spcx().sample_set(prefix, count, seed),
        hidden: |count, seed, tell| {
            shaped(WeekendFamily::spcx().draw_set("hidden", count, seed), tell)
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

/// A hidden set drawn with each scenario's shape: annotated `{"shape": ..}`
/// and, when told, the shape as the regime.
fn shaped<S: serde::Serialize>(drawn: Vec<(Scenario, S)>, tell: bool) -> Result<Hidden> {
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
    Ok(Hidden {
        scenarios: drawn.into_iter().map(|(scenario, _)| scenario).collect(),
        annotations,
        regimes,
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
            assert!((g.hidden)(4, 7, false).unwrap().regimes.is_none());
        }
    }
}
