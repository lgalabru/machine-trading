//! A task family's own actors, built from the scenario.
//!
//! A scenario names its family and carries the family's plan, opaque to the
//! engine ([`FamilyPlan`]). A family registers a builder under its name
//! ([`register`]); at setup, while the cheatcodes still work, a spot session
//! hands the builder the plan and the cast, and the builder onboards its own
//! seats ([`super::Seat::spot`], [`super::Cast::add_seat`]) and adds its
//! actors ([`super::Cast::add`], [`super::Cast::add_first`]). It may return
//! a [`SendHook`] for the bot's guard.

use std::{
    collections::BTreeMap,
    sync::{Arc, LazyLock, RwLock},
};

use futures::future::BoxFuture;
use serde::{Deserialize, Serialize};
use surfpool_sdk::Surfnet;

use super::Cast;
use crate::{Error, Result, guard::SendHook, scenario::Scenario};

/// A family's plan for its own actors.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct FamilyPlan {
    /// The name the family registered its builder under.
    pub family: String,
    /// Whatever the family's builder reads.
    pub plan: serde_json::Value,
}

/// What a family's builder gets at setup.
pub struct FamilySetup<'a> {
    pub surfnet: &'a Surfnet,
    /// The grader's own RPC, which seats trade through.
    pub rpc_url: &'a str,
    pub scenario: &'a Scenario,
    pub plan: &'a serde_json::Value,
    /// The scenario's level at the start, in USD.
    pub reference_usd: f64,
    pub cast: &'a mut Cast,
}

/// A family's builder: onboards its seats, adds its actors to the cast, and
/// may return a hook for the bot's sends.
pub type BuildFamily =
    for<'a> fn(FamilySetup<'a>) -> BoxFuture<'a, Result<Option<Arc<dyn SendHook>>>>;

static FAMILIES: LazyLock<RwLock<BTreeMap<String, BuildFamily>>> = LazyLock::new(Default::default);

/// Register `build` for scenarios whose plan names `family`. A later
/// registration under the same name replaces it.
pub fn register(family: &str, build: BuildFamily) {
    FAMILIES
        .write()
        .expect("family registry")
        .insert(family.to_string(), build);
}

/// The builder registered as `family`.
pub fn builder(family: &str) -> Result<BuildFamily> {
    FAMILIES
        .read()
        .expect("family registry")
        .get(family)
        .copied()
        .ok_or_else(|| Error::Transaction {
            label: "family actors".into(),
            reason: format!("no family registered as {family:?}"),
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::actors::{Actor, ActorsReport, Moment, StepFlow};

    struct Quiet;

    impl Actor for Quiet {
        fn before_flow<'a>(&'a mut self, _: &'a Moment) -> BoxFuture<'a, ()> {
            Box::pin(async {})
        }
        fn after_blocks<'a>(&'a mut self, _: &'a Moment, _: &'a StepFlow) -> BoxFuture<'a, ()> {
            Box::pin(async {})
        }
        fn report<'a>(&'a self, report: &'a mut ActorsReport) -> BoxFuture<'a, ()> {
            Box::pin(async move {
                report
                    .extra
                    .insert("quiet".into(), serde_json::json!({ "acted": false }));
            })
        }
    }

    fn quiet(setup: FamilySetup<'_>) -> BoxFuture<'_, Result<Option<Arc<dyn SendHook>>>> {
        Box::pin(async move {
            let walls = setup.plan["walls"].as_u64().unwrap_or(0);
            for _ in 0..walls {
                setup.cast.add_first(Box::new(Quiet));
            }
            Ok(None)
        })
    }

    #[test]
    fn a_plan_round_trips_opaque() {
        let plan: FamilyPlan =
            serde_json::from_str(r#"{"family":"quiet","plan":{"walls":2,"any":[1,"x"]}}"#).unwrap();
        assert_eq!(plan.plan["any"][1], "x");
        let back: FamilyPlan =
            serde_json::from_value(serde_json::to_value(&plan).unwrap()).unwrap();
        assert_eq!(back, plan);
    }

    #[test]
    fn a_family_is_found_by_the_name_it_registered() {
        register("quiet-test", quiet);
        assert!(builder("quiet-test").is_ok());
        let missing = builder("nobody").err().unwrap().to_string();
        assert!(
            missing.contains("no family registered as \"nobody\""),
            "{missing}"
        );
    }
}
