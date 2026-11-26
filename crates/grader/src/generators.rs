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

use serde_json::Value;
#[cfg(feature = "generators")]
use serde_json::json;

#[cfg(feature = "generators")]
use crate::{
    backrun::{BackrunFamily, BackrunShape},
    carry::{CarryFamily, Shape as CarryShape},
    liquidation_flash::{LiquidationFamily, LiquidationShape},
    making::{MakingFamily, measured::MeasuredFamily},
    scenario::{Shape, ShockFamily},
    thin_pools::{ThinPoolsFamily, ThinPoolsShape},
    weekend::{WeekendFamily, WeekendShape},
    weekend_measured::MeasuredWeekendShape,
};
use crate::{error::Result, scenario::Scenario};

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

/// Empty without the `generators` feature: the agent's `mtx` is built that
/// way, so nothing in it can draw a scenario from a task's family.
#[cfg(not(feature = "generators"))]
pub const GENERATORS: &[Generator] = &[];

#[cfg(feature = "generators")]
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
        name: "flash-crash-nvda",
        practice_drawn: |prefix, count, seed| {
            shaped(
                ShockFamily::nvda().draw_set(prefix, count, seed),
                false,
                nvda_shock_latent,
            )
        },
        hidden: |count, seed, tell| {
            shaped(
                ShockFamily::nvda().draw_set("hidden", count, seed),
                tell,
                nvda_shock_latent,
            )
        },
        params: || format!("{:?}", ShockFamily::nvda()),
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
        name: "market-making-measured-spcx",
        practice_drawn: |prefix, count, seed| {
            measured_drawn(&MeasuredFamily::spcx(), prefix, count, seed, false)
        },
        hidden: |count, seed, tell| {
            measured_drawn(&MeasuredFamily::spcx(), "hidden", count, seed, tell)
        },
        params: || format!("{:?}", MeasuredFamily::spcx()),
    },
    // ANT's measured flow on Phoenix's ANT: the profile the task was drawn
    // on before it moved to CBRS, kept to compare.
    Generator {
        name: "market-making-measured-ant",
        practice_drawn: |prefix, count, seed| {
            measured_drawn(&MeasuredFamily::ant(), prefix, count, seed, false)
        },
        hidden: |count, seed, tell| {
            measured_drawn(&MeasuredFamily::ant(), "hidden", count, seed, tell)
        },
        params: || format!("{:?}", MeasuredFamily::ant()),
    },
    // The task's: CBRS's measured flow on Phoenix's CBRS (market-making-cbrs).
    Generator {
        name: "market-making-cbrs",
        practice_drawn: |prefix, count, seed| {
            measured_drawn(&MeasuredFamily::cbrs(), prefix, count, seed, false)
        },
        hidden: |count, seed, tell| {
            measured_drawn(&MeasuredFamily::cbrs(), "hidden", count, seed, tell)
        },
        params: || format!("{:?}", MeasuredFamily::cbrs()),
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
    Generator {
        name: "weekend-gap-mainnet",
        practice_drawn: |prefix, count, seed| {
            shaped(
                WeekendFamily::mainnet().draw_set(prefix, count, seed),
                false,
                mainnet_weekend_latent,
            )
        },
        hidden: |count, seed, tell| {
            shaped(
                WeekendFamily::mainnet().draw_set("hidden", count, seed),
                tell,
                mainnet_weekend_latent,
            )
        },
        params: || format!("{:?}", WeekendFamily::mainnet()),
    },
    Generator {
        name: "liquidation-flash",
        practice_drawn: |prefix, count, seed| {
            shaped(
                LiquidationFamily::kamino_main().draw_set(prefix, count, seed),
                false,
                LiquidationShape::latent,
            )
        },
        hidden: |count, seed, tell| {
            shaped(
                LiquidationFamily::kamino_main().draw_set("hidden", count, seed),
                tell,
                LiquidationShape::latent,
            )
        },
        params: || format!("{:?}", LiquidationFamily::kamino_main()),
    },
    Generator {
        name: "backrun-sol-usdc",
        practice_drawn: |prefix, count, seed| {
            shaped(
                BackrunFamily::sol_usdc().draw_set(prefix, count, seed),
                false,
                BackrunShape::latent,
            )
        },
        hidden: |count, seed, tell| {
            shaped(
                BackrunFamily::sol_usdc().draw_set("hidden", count, seed),
                tell,
                BackrunShape::latent,
            )
        },
        params: || format!("{:?}", BackrunFamily::sol_usdc()),
    },
    Generator {
        name: "backrun-thin-pools",
        practice_drawn: |prefix, count, seed| {
            shaped(
                ThinPoolsFamily::snapshot().draw_set(prefix, count, seed),
                false,
                ThinPoolsShape::latent,
            )
        },
        hidden: |count, seed, tell| {
            shaped(
                ThinPoolsFamily::snapshot().draw_set("hidden", count, seed),
                tell,
                ThinPoolsShape::latent,
            )
        },
        params: || format!("{:?}", ThinPoolsFamily::snapshot()),
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

#[cfg(feature = "generators")]
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

#[cfg(feature = "generators")]
/// A measured market-making hidden set: each session annotated with its
/// kind, its latent state the informed takers' and the arbitrageur's
/// roster and the flow's process, and, when told, the kind as the regime.
fn measured_drawn(
    family: &MeasuredFamily,
    prefix: &str,
    count: usize,
    seed: u64,
    tell: bool,
) -> Result<Hidden> {
    let drawn = family.sample_set(prefix, count, seed);
    let annotations = drawn
        .iter()
        .map(|(_, shape)| json!({ "regime": shape.regime_name(), "latent": shape.latent() }))
        .collect();
    let regimes = if tell {
        Some(
            drawn
                .iter()
                .map(|(_, shape)| {
                    Ok(serde_json::to_string(
                        &json!({ "regime": shape.regime_name() }),
                    )?)
                })
                .collect::<Result<_>>()?,
        )
    } else {
        None
    };
    let latent = drawn
        .iter()
        .map(|(_, shape)| Ok(serde_json::to_string(&shape.latent())?))
        .collect::<Result<_>>()?;
    Ok(Hidden {
        scenarios: drawn.into_iter().map(|(scenario, _)| scenario).collect(),
        annotations,
        regimes,
        latent,
    })
}

#[cfg(feature = "generators")]
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
    ("flash-crash-nvda", "horizon_slots", Knowable::Process),
    ("flash-crash-nvda", "hit_rate", Knowable::Process),
    ("flash-crash-nvda", "quiet_odds", Knowable::Process),
    ("flash-crash-nvda", "aftermath_odds", Knowable::Process),
    ("flash-crash-nvda", "impact", Knowable::Process),
    ("market-making-cbrs", "informed_share", Knowable::Process),
    (
        "market-making-cbrs",
        "informed_horizon_slots",
        Knowable::Process,
    ),
    ("market-making-cbrs", "informed_hit_rate", Knowable::Process),
    ("market-making-cbrs", "informed_edge_bps", Knowable::Process),
    ("market-making-cbrs", "index_share", Knowable::Process),
    ("market-making-cbrs", "index_trigger_bps", Knowable::Process),
    ("market-making-cbrs", "book_lead_slots", Knowable::Process),
    ("market-making-cbrs", "orders_per_minute", Knowable::Process),
    ("market-making-cbrs", "competitor_bps", Knowable::Process),
    (
        "market-making-cbrs",
        "competitor_floor_bps",
        Knowable::Process,
    ),
    (
        "market-making-cbrs",
        "competitor_react_slots",
        Knowable::Process,
    ),
    (
        "market-making-cbrs",
        "arbitrageur_latency_slots",
        Knowable::Process,
    ),
    (
        "market-making-cbrs",
        "arbitrageur_edge_bps",
        Knowable::Process,
    ),
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
    ("weekend-gap-mainnet", "reopen_keep", Knowable::Process),
    (
        "weekend-gap-mainnet",
        "open_residual_sd_pct",
        Knowable::Process,
    ),
    ("weekend-gap-mainnet", "horizon_slots", Knowable::Process),
    ("weekend-gap-mainnet", "hit_rate", Knowable::Process),
    ("weekend-gap-mainnet", "edge_bps", Knowable::Process),
    ("weekend-gap-mainnet", "view_noise_bps", Knowable::Process),
    (
        "weekend-gap-mainnet",
        "orders_per_wallet",
        Knowable::Process,
    ),
    ("liquidation-flash", "liquidators", Knowable::Identity),
    ("liquidation-flash", "bids", Knowable::Identity),
    ("liquidation-flash", "bid_share", Knowable::Process),
    ("liquidation-flash", "tipping_share", Knowable::Process),
    ("liquidation-flash", "same_slot_share", Knowable::Process),
    ("liquidation-flash", "competition", Knowable::Process),
    ("liquidation-flash", "keeper_every_slots", Knowable::Process),
    ("backrun-sol-usdc", "searchers", Knowable::Identity),
    ("backrun-sol-usdc", "pool_searchers", Knowable::Identity),
    ("backrun-sol-usdc", "flow_scale", Knowable::Process),
    ("backrun-thin-pools", "searchers", Knowable::Identity),
    ("backrun-thin-pools", "pool_searchers", Knowable::Identity),
    ("backrun-thin-pools", "flow_scale", Knowable::Process),
    (
        "market-making-measured-spcx",
        "informed_share",
        Knowable::Process,
    ),
    (
        "market-making-measured-spcx",
        "informed_horizon_slots",
        Knowable::Process,
    ),
    (
        "market-making-measured-spcx",
        "informed_hit_rate",
        Knowable::Process,
    ),
    (
        "market-making-measured-spcx",
        "informed_edge_bps",
        Knowable::Process,
    ),
    (
        "market-making-measured-spcx",
        "index_share",
        Knowable::Process,
    ),
    (
        "market-making-measured-spcx",
        "index_trigger_bps",
        Knowable::Process,
    ),
    (
        "market-making-measured-spcx",
        "book_lead_slots",
        Knowable::Process,
    ),
    (
        "market-making-measured-spcx",
        "orders_per_minute",
        Knowable::Process,
    ),
    (
        "market-making-measured-spcx",
        "competitor_bps",
        Knowable::Process,
    ),
    (
        "market-making-measured-spcx",
        "competitor_floor_bps",
        Knowable::Process,
    ),
    (
        "market-making-measured-spcx",
        "competitor_react_slots",
        Knowable::Process,
    ),
    (
        "market-making-measured-spcx",
        "arbitrageur_latency_slots",
        Knowable::Process,
    ),
    (
        "market-making-measured-spcx",
        "arbitrageur_edge_bps",
        Knowable::Process,
    ),
    (
        "market-making-measured-ant",
        "informed_share",
        Knowable::Process,
    ),
    (
        "market-making-measured-ant",
        "informed_horizon_slots",
        Knowable::Process,
    ),
    (
        "market-making-measured-ant",
        "informed_hit_rate",
        Knowable::Process,
    ),
    (
        "market-making-measured-ant",
        "informed_edge_bps",
        Knowable::Process,
    ),
    (
        "market-making-measured-ant",
        "index_share",
        Knowable::Process,
    ),
    (
        "market-making-measured-ant",
        "index_trigger_bps",
        Knowable::Process,
    ),
    (
        "market-making-measured-ant",
        "book_lead_slots",
        Knowable::Process,
    ),
    (
        "market-making-measured-ant",
        "orders_per_minute",
        Knowable::Process,
    ),
    (
        "market-making-measured-ant",
        "competitor_bps",
        Knowable::Process,
    ),
    (
        "market-making-measured-ant",
        "competitor_floor_bps",
        Knowable::Process,
    ),
    (
        "market-making-measured-ant",
        "competitor_react_slots",
        Knowable::Process,
    ),
    (
        "market-making-measured-ant",
        "arbitrageur_latency_slots",
        Knowable::Process,
    ),
    (
        "market-making-measured-ant",
        "arbitrageur_edge_bps",
        Knowable::Process,
    ),
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

#[cfg(feature = "generators")]
/// Nothing: a session's shock (its direction, legs and aftermath) is the
/// one draw that makes it, so all of it is the session's future.
fn shock_latent(_: &Shape) -> Value {
    json!({})
}

#[cfg(feature = "generators")]
/// Who trades on information, and the process every session is drawn
/// from: the informed takers' wallets (resolved as the session starts),
/// how far ahead they see and how often they are right, the share of quiet
/// sessions and the aftermath's odds, and the impact's persistence. Never
/// the session's own draws: whether it is quiet, the shock's direction,
/// legs, size, timing or aftermath. Every session tells the same process;
/// only the wallets differ.
fn nvda_shock_latent(_: &Shape) -> Value {
    let family = ShockFamily::nvda();
    let informed = family
        .actors
        .as_ref()
        .and_then(|actors| actors.informed.first())
        .expect("the NVDA family seats informed takers");
    let impact = family.impact.as_ref().expect("the NVDA family has impact");
    let latent = crate::latent::Latent::new()
        .roster(&["informed"])
        .and_then(|l| l.process("horizon_slots", informed.horizon_slots))
        .and_then(|l| l.process("hit_rate", informed.hit_rate))
        .and_then(|l| l.process("quiet_odds", family.quiet_odds))
        .and_then(|l| {
            l.process(
                "aftermath_odds",
                json!({
                    "revert": family.revert_odds,
                    "stall": family.stall_odds,
                    "extend": 1.0 - family.revert_odds - family.stall_odds,
                }),
            )
        })
        .and_then(|l| {
            l.process(
                "impact",
                json!({
                    "permanent_share": impact.permanent_share,
                    "half_life_slots": impact.half_life_slots,
                }),
            )
        })
        .expect("the NVDA family's latent fields are sound");
    serde_json::from_str(&latent.to_json()).expect("a latent is JSON")
}

#[cfg(feature = "generators")]
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

#[cfg(feature = "generators")]
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

#[cfg(feature = "generators")]
/// Who positions into the reopen on information, and the process every
/// session is drawn from: the informed traders' wallets (resolved as the
/// session starts), how far ahead they see and how often they are right,
/// when they trade at all (past an edge, through a noisy view, at a pace),
/// and how much of the weekend's move the reopen keeps, give or take what.
/// Never the session's own draws: the weekend's move, the open, whether
/// an informed wallet's view is right this time. Every session tells the
/// same process; only the wallets differ.
fn mainnet_weekend_latent(_: &MeasuredWeekendShape) -> Value {
    let family = WeekendFamily::mainnet();
    let positioners = &family.positioners;
    let latent = crate::latent::Latent::new()
        .roster(&["informed"])
        .and_then(|l| l.process("reopen_keep", family.reopen_keep))
        .and_then(|l| l.process("open_residual_sd_pct", family.open_residual_sd_pct))
        .and_then(|l| l.process("horizon_slots", positioners.horizon_slots))
        .and_then(|l| l.process("hit_rate", positioners.hit_rate))
        .and_then(|l| l.process("edge_bps", positioners.edge_bps))
        .and_then(|l| l.process("view_noise_bps", positioners.view_noise_bps))
        .and_then(|l| l.process("orders_per_wallet", positioners.orders_per_wallet))
        .expect("the mainnet weekend's latent fields are sound");
    serde_json::from_str(&latent.to_json()).expect("a latent is JSON")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_agents_build_holds_no_generator() {
        assert_eq!(GENERATORS.is_empty(), !cfg!(feature = "generators"));
    }

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

    #[cfg(feature = "generators")]
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
    #[cfg(feature = "generators")]
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
    #[cfg(feature = "generators")]
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

    /// The mainnet weekend tells its informed bot who positions on
    /// information and the process, the same in every session, and nothing
    /// of the session's own draw: not the weekend's move nor the open.
    #[cfg(feature = "generators")]
    #[test]
    fn the_mainnet_weekend_latent_is_the_process_and_the_informed_wallets() {
        let hidden = (generator("weekend-gap-mainnet").unwrap().hidden)(24, 5, false).unwrap();
        let first: Value = serde_json::from_str(&hidden.latent[0]).unwrap();
        assert!(hidden.latent.iter().all(|l| *l == hidden.latent[0]));
        assert_eq!(first["roster"], json!(["informed"]));
        assert_eq!(first["reopen_keep"], 0.78);
        assert_eq!(first["open_residual_sd_pct"], 0.5);
        assert_eq!(first["horizon_slots"], 40);
        assert_eq!(first["hit_rate"], 0.95);
        assert_eq!(first["edge_bps"], 10.0);
        assert_eq!(first["view_noise_bps"], 10.0);
        assert_eq!(first["orders_per_wallet"], 3.0);
        for drawn in ["weekend_pct", "open_pct", "gap_pct", "path", "reopen_slot"] {
            assert!(first.get(drawn).is_none(), "{drawn} is the session's draw");
        }
        let annotation = &hidden.annotations[0]["shape"];
        assert!(annotation.get("gap_pct").is_some());
    }

    /// The NVDA shock family tells its informed bot who trades on
    /// information and the process, the same in every session, and nothing
    /// of the session's own draw: not whether it is quiet, nor the shock's
    /// direction, legs, size or aftermath.
    #[cfg(feature = "generators")]
    #[test]
    fn the_nvda_shock_latent_is_the_process_and_the_informed_wallets() {
        let hidden = (generator("flash-crash-nvda").unwrap().hidden)(24, 5, false).unwrap();
        let first: Value = serde_json::from_str(&hidden.latent[0]).unwrap();
        assert!(hidden.latent.iter().all(|l| *l == hidden.latent[0]));
        assert_eq!(first["roster"], json!(["informed"]));
        assert_eq!(first["horizon_slots"], 40);
        assert_eq!(first["quiet_odds"], 0.25);
        for drawn in [
            "quiet",
            "direction",
            "aftermath",
            "legs",
            "extreme_pct",
            "end_pct",
        ] {
            assert!(first.get(drawn).is_none(), "{drawn} is the session's draw");
        }
        let classified: Vec<&str> = LATENT_FIELDS
            .iter()
            .filter(|(family, ..)| *family == "flash-crash-nvda")
            .map(|(_, field, _)| *field)
            .collect();
        for field in first.as_object().unwrap().keys() {
            assert!(
                field == crate::latent::ROSTER || classified.contains(&field.as_str()),
                "{field}"
            );
        }
    }

    /// The check above would catch a draw handed over: a shock's direction
    /// varies from session to session and is classified as nothing.
    #[cfg(feature = "generators")]
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
