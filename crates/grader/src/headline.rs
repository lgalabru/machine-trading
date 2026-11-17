//! The headline family: a scheduled TSLA headline jumps the price, the
//! market pauses, and a verdict (the call after the release) then carries
//! it further or takes part of it back. Two funds trade every session; one
//! knows which.
//!
//! - **The price** walks a market's calibration (GARCH-t on one-minute
//!   blocks, its tails and volatility states), inside regular hours.
//! - **The headline** prints at a slot drawn in a stated window: the level
//!   jumps 1-4%, up or down with equal odds, in a few uneven steps a few
//!   slots apart.
//! - **The verdict** comes a drawn while after the jump, in one to three
//!   quick steps: a follow-through carries the level on the jump's way by a
//!   share of the jump, a fade takes the same share back. The two are
//!   equally likely and draw their share, delay and steps alike, so a
//!   policy that rides (or fades) every headline nets its costs.
//! - **Two funds** trade from wallets of their own in every session, at
//!   the same pace and sizes, both ahead of the path until the jump is
//!   done. Then the informed fund keeps trading ahead of it (the noise
//!   ahead, and the verdict once it is within its horizon), and the other
//!   trades against what it sees: each fund's flow is the other's mirror,
//!   so the market's net flow says nothing of the verdict. Only each
//!   fund's record since the jump tells them apart.
//! - **Chasers and noise** trade in every session too.
//!
//! No tells: every session seats the same actors, both verdicts draw from
//! the same ranges, and the roster the informed bot is told (the informed
//! fund's wallets) never depends on the verdict.

use std::collections::BTreeMap;

use mtx_core::mandate::Mandate;
use serde::Serialize;

use crate::{
    actors::{
        Library,
        library::{Chaser, Decoy, DecoyAfter, Informed, Noise, Scale, Window},
    },
    market::{
        Calibration, MarketModel, ModelSpec,
        calibration::sha256_hex,
        model::{ModelPath, Start},
    },
    orders::CloseOut,
    scenario::{CloseMode, Scenario, SplitMix64, StopOut, StopRule, VENUE, Waypoint, move_slots},
};

/// TSLA's market calibration, built in so hidden sets need no file.
const TSLA_CALIBRATION: &str =
    include_str!("../../../calibration/market/tsla.20261113.8b7bca8e.toml");

/// What a session is, beneath its path.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Shape {
    /// The headline's first step, and the slot its last one lands.
    pub event_slot: u64,
    pub jump_end_slot: u64,
    /// `1` the headline jumps up, `-1` down.
    pub side: i8,
    /// The jump, in percent of the level before it, and its steps' slots.
    pub jump_pct: f64,
    pub jump_steps: Vec<u64>,
    /// `follow` or `fade`.
    pub verdict: String,
    /// The verdict's first step, and its size as a share of the jump.
    pub verdict_slot: u64,
    pub verdict_share: f64,
    pub verdict_steps: Vec<u64>,
    /// How far ahead each fund sees, in slots, and how many wallets each
    /// trades from.
    pub informed_horizon_slots: u64,
    pub decoy_horizon_slots: u64,
    pub fund_wallets: u32,
}

/// The family's parameters. Every range is inclusive.
#[derive(Debug, Clone)]
pub struct HeadlineFamily {
    pub symbol: String,
    /// The snapshot's mark, which the session starts at, in USD.
    pub start_usd: f64,
    pub duration: (u64, u64),
    pub waypoint_slots: u64,
    /// The window the headline prints in, in slots into the session.
    pub event_window: (u64, u64),
    pub jump_pct: (f64, f64),
    pub jump_steps: (u64, u64),
    /// Slots between two steps of the jump or the verdict.
    pub step_gap_slots: (u64, u64),
    /// Slots from the jump's last step to the verdict's first.
    pub verdict_delay_slots: (u64, u64),
    /// The verdict, as a share of the jump.
    pub verdict_share: (f64, f64),
    pub verdict_steps: (u64, u64),
    /// The two funds: wallets each, how far ahead they see, the move they
    /// trade on, and their pace and sizes.
    pub fund_wallets: (u64, u64),
    pub horizon_slots: (u64, u64),
    pub fund_edge_bps: f64,
    pub fund_intensity: f64,
    pub fund_size: f64,
    pub noise_wallets: u32,
    pub noise_flow: f64,
    pub noise_size: f64,
    pub chaser_wallets: u32,
    pub chaser_trigger_bps: f64,
    pub chaser_intensity: f64,
    pub chaser_size: f64,
    /// The mandate: the position cap and the loss limit; and the broker's
    /// haircut on a position left at the close.
    pub max_position: f64,
    pub max_drawdown_usd: f64,
    pub close_haircut_bps: f64,
}

impl HeadlineFamily {
    pub fn tsla() -> Self {
        Self {
            symbol: "TSLA".into(),
            start_usd: 388.95,
            duration: (1_400, 2_200),
            waypoint_slots: 4,
            event_window: (300, 600),
            jump_pct: (1.0, 4.0),
            jump_steps: (2, 5),
            step_gap_slots: (3, 8),
            verdict_delay_slots: (20, 260),
            verdict_share: (0.3, 0.8),
            verdict_steps: (1, 3),
            fund_wallets: (2, 3),
            horizon_slots: (30, 90),
            fund_edge_bps: 4.0,
            fund_intensity: 15.0,
            fund_size: 40.0,
            noise_wallets: 4,
            noise_flow: 15.0,
            noise_size: 25.0,
            chaser_wallets: 3,
            chaser_trigger_bps: 15.0,
            chaser_intensity: 30.0,
            chaser_size: 30.0,
            // About 0.47x of the 10,000 USDC of collateral at $388.95.
            max_position: 12.0,
            max_drawdown_usd: 600.0,
            close_haircut_bps: 5.0,
        }
    }

    /// The price's model: TSLA's calibration inside regular hours.
    pub fn model() -> MarketModel {
        let mut calibration =
            Calibration::parse(TSLA_CALIBRATION).expect("TSLA's calibration is valid");
        calibration.sha256 = sha256_hex(TSLA_CALIBRATION.as_bytes());
        MarketModel::new(
            calibration,
            ModelSpec {
                start: Start::Session {
                    kind: "regular".into(),
                },
                ..ModelSpec::default()
            },
        )
        .expect("the model's spec is valid")
    }

    /// The market's own taker flow, from its calibration.
    fn scale(model: &MarketModel) -> Scale {
        let venue = model
            .calibration
            .venue("phoenix_perp")
            .expect("TSLA's calibration has its Phoenix perp");
        Scale {
            trades_per_minute: venue.trades_per_minute,
            trade_size: venue.trade_size.median,
            trade_size_sigma: venue.trade_size.sigma,
        }
    }

    pub fn sample(&self, name: &str, seed: u64) -> Scenario {
        self.draw(name, seed).0
    }

    pub fn draw(&self, name: &str, seed: u64) -> (Scenario, Shape) {
        self.draw_with(&Self::model(), name, seed)
    }

    fn draw_with(&self, model: &MarketModel, name: &str, seed: u64) -> (Scenario, Shape) {
        let mut rng = SplitMix64(seed);
        let duration = rng.range_u64(self.duration);
        let mut walk = model.draw(rng.next_u64(), duration);

        // The headline: its slot, way, size and steps, each step a random
        // share of the jump.
        let event_slot = rng.range_u64(self.event_window);
        let side: i8 = if rng.next_f64() < 0.5 { 1 } else { -1 };
        let jump_pct = round3(rng.range_f64(self.jump_pct));
        let jump_steps = self.steps(&mut rng, event_slot, self.jump_steps);
        let jump_end_slot = *jump_steps.last().expect("a jump has steps");
        let s = f64::from(side);
        let jump_log = (1.0 + s * jump_pct / 100.0).ln();
        self.add_steps(&mut rng, &mut walk, &jump_steps, jump_log);

        // The verdict: the same share and delay either way.
        let follow = rng.next_f64() < 0.5;
        let verdict_share = round3(rng.range_f64(self.verdict_share));
        let verdict_slot = jump_end_slot + rng.range_u64(self.verdict_delay_slots);
        let verdict_steps = self.steps(&mut rng, verdict_slot, self.verdict_steps);
        let way = if follow { s } else { -s };
        let verdict_log = (1.0 + way * verdict_share * jump_pct / 100.0).ln();
        self.add_steps(&mut rng, &mut walk, &verdict_steps, verdict_log);

        let fund_wallets = rng.range_u64(self.fund_wallets) as u32;
        let informed_horizon_slots = rng.range_u64(self.horizon_slots);
        let decoy_horizon_slots = rng.range_u64(self.horizon_slots);

        let forced: Vec<u64> = jump_steps.iter().chain(&verdict_steps).copied().collect();
        let path = self.waypoints(&walk, &forced);
        let actors = self.actors(
            model,
            jump_end_slot,
            fund_wallets,
            informed_horizon_slots,
            decoy_horizon_slots,
        );
        let mandate = Mandate::perp(VENUE, &self.symbol, self.max_position, self.max_drawdown_usd);
        let shape = Shape {
            event_slot,
            jump_end_slot,
            side,
            jump_pct,
            jump_steps,
            verdict: if follow { "follow" } else { "fade" }.into(),
            verdict_slot,
            verdict_share,
            verdict_steps,
            informed_horizon_slots,
            decoy_horizon_slots,
            fund_wallets,
        };
        let description = format!(
            "{}: a headline between slots {} and {}; position within {} either way",
            self.symbol, self.event_window.0, self.event_window.1, self.max_position
        );
        let scenario = Scenario {
            name: name.to_string(),
            description,
            symbol: self.symbol.clone(),
            duration_slots: duration,
            mandate,
            path,
            making: None,
            offhours: None,
            funding: None,
            tape: Vec::new(),
            spot: None,
            faults: None,
            actors: Some(actors),
            takers_pricing: None,
            turn_data: None,
            oracles: None,
            lending: None,
            warps: Vec::new(),
            order: None,
            participation: None,
            net: None,
            reference_pnl_usd: None,
            informed_pnl_usd: None,
            reference_max_drawdown_usd: None,
            informed_max_drawdown_usd: None,
            fair_path: None,
            bound_on: Default::default(),
            market_model: Some(walk.record.clone()),
            raw_sends: None,
            markets: Vec::new(),
            joint: None,
            start_positions: Default::default(),
            close_out: BTreeMap::from([(
                self.symbol.clone(),
                CloseOut {
                    haircut_bps: self.close_haircut_bps,
                },
            )]),
            tx_layer: None,
            stop_out: Some(StopOut {
                on: vec![StopRule::Drawdown],
                close: CloseMode::Market,
                slice_base: None,
            }),
            liquidator: None,
        };
        (scenario, shape)
    }

    /// The slots of a move's steps, from `first`, a drawn gap apart.
    fn steps(&self, rng: &mut SplitMix64, first: u64, count: (u64, u64)) -> Vec<u64> {
        let n = rng.range_u64(count);
        let mut slots = vec![first];
        for _ in 1..n {
            let last = *slots.last().expect("a first step");
            slots.push(last + rng.range_u64(self.step_gap_slots));
        }
        slots
    }

    /// Adds `total` (in log price) to the walk in steps at `slots`, each a
    /// random share of it, held after.
    fn add_steps(&self, rng: &mut SplitMix64, walk: &mut ModelPath, slots: &[u64], total: f64) {
        let weights: Vec<f64> = slots.iter().map(|_| 0.5 + rng.next_f64()).collect();
        let sum: f64 = weights.iter().sum();
        for (&slot, w) in slots.iter().zip(&weights) {
            let shift = total * w / sum;
            for level in walk.log_levels.iter_mut().skip(slot as usize) {
                *level += shift;
            }
        }
    }

    /// The path as waypoints: the walk every `waypoint_slots`, and a
    /// waypoint exactly at each step of the jump and the verdict.
    fn waypoints(&self, walk: &ModelPath, forced: &[u64]) -> Vec<Waypoint> {
        let duration = (walk.log_levels.len() - 1) as u64;
        let level_pct = |slot: u64| round3(walk.level_pct(slot));
        let mut candidates: Vec<(u64, bool)> = (1..)
            .map(|k| k * self.waypoint_slots)
            .take_while(|&s| s < duration)
            .filter(|s| {
                !forced
                    .iter()
                    .any(|&f| *s < f && f - *s < 2 * self.waypoint_slots)
            })
            .map(|s| (s, false))
            .collect();
        candidates.extend(forced.iter().map(|&s| (s, true)));
        candidates.sort_unstable();
        candidates.dedup_by_key(|(s, _)| *s);

        let mut out: Vec<Waypoint> = Vec::new();
        let mut free_from = 0;
        let mut last = 0.0;
        for (slot, forced) in candidates {
            if slot >= duration || (!forced && slot < free_from) {
                continue;
            }
            let level = level_pct(slot);
            if level == last {
                continue;
            }
            let done = slot + move_slots(last, level) + 1;
            if done > duration {
                break;
            }
            last = level;
            free_from = done;
            out.push(Waypoint {
                slot,
                level_pct: level,
                premium_pct: 0.0,
            });
        }
        out
    }

    fn actors(
        &self,
        model: &MarketModel,
        jump_end_slot: u64,
        fund_wallets: u32,
        informed_horizon_slots: u64,
        decoy_horizon_slots: u64,
    ) -> Library {
        Library {
            scale: Self::scale(model),
            noise: vec![Noise {
                wallets: self.noise_wallets,
                flow_mult: self.noise_flow,
                size_mult: self.noise_size,
                limit_bps: 50.0,
                active: None,
            }],
            informed: vec![Informed {
                wallets: fund_wallets,
                horizon_slots: informed_horizon_slots,
                edge_bps: self.fund_edge_bps,
                intensity_mult: self.fund_intensity,
                size_mult: self.fund_size,
                limit_bps: 80.0,
                active: None,
            }],
            decoys: vec![Decoy {
                wallets: fund_wallets,
                horizon_slots: decoy_horizon_slots,
                edge_bps: self.fund_edge_bps,
                informed_for: Window {
                    from: jump_end_slot,
                    until: Some(jump_end_slot + 1),
                },
                after: DecoyAfter::Contrary,
                intensity_mult: self.fund_intensity,
                size_mult: self.fund_size,
                limit_bps: 80.0,
            }],
            chasers: vec![Chaser {
                wallets: self.chaser_wallets,
                lookback_slots: 20,
                trigger_bps: self.chaser_trigger_bps,
                intensity_mult: self.chaser_intensity,
                size_mult: self.chaser_size,
                limit_bps: 80.0,
                active: None,
            }],
            ..Library::default()
        }
    }

    pub fn sample_set(&self, prefix: &str, count: usize, seed: u64) -> Vec<Scenario> {
        self.draw_set(prefix, count, seed)
            .into_iter()
            .map(|(scenario, _)| scenario)
            .collect()
    }

    /// `count` sessions named `<prefix>-<n>`.
    pub fn draw_set(&self, prefix: &str, count: usize, seed: u64) -> Vec<(Scenario, Shape)> {
        let model = Self::model();
        (0..count)
            .map(|i| {
                let draw_seed = seed.wrapping_mul(1_000_003).wrapping_add(i as u64 + 1);
                self.draw_with(&model, &format!("{prefix}-{}", i + 1), draw_seed)
            })
            .collect()
    }
}

fn round3(x: f64) -> f64 {
    (x * 1000.0).round() / 1000.0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::making::level_at;

    #[test]
    fn sessions_are_valid_and_deterministic() {
        let family = HeadlineFamily::tsla();
        for seed in 0..40 {
            let (scenario, shape) = family.draw("s", seed);
            assert_eq!(scenario.validate(), Ok(()), "seed {seed}");
            assert_eq!(scenario, family.sample("s", seed));
            assert!(shape.verdict_steps.last().unwrap() + 200 < scenario.duration_slots);
            let v2 = crate::scenario_v2::ScenarioV2::try_from(&scenario).unwrap();
            assert_eq!(v2.into_scenario().unwrap(), scenario);
        }
    }

    /// The level jumps the headline's way at the event, and the verdict
    /// carries it on or takes its share back.
    #[test]
    fn the_jump_and_the_verdict_land_on_the_path() {
        let family = HeadlineFamily::tsla();
        let (mut follows, mut ups) = (0, 0);
        for (scenario, shape) in family.draw_set("h", 60, 5) {
            let path = &scenario.path;
            let s = f64::from(shape.side);
            let before = level_at(path, shape.event_slot - 1);
            let jumped = level_at(path, shape.jump_end_slot + 2);
            let moved = s * (jumped - before);
            assert!(
                (moved - shape.jump_pct).abs() < 0.3 * shape.jump_pct + 0.1,
                "{shape:?}: jumped {moved}"
            );
            let settled = level_at(path, shape.verdict_steps.last().unwrap() + 2);
            let pre = level_at(path, shape.verdict_slot - 1);
            let verdict = s * (settled - pre);
            let want = shape.verdict_share * shape.jump_pct;
            let way = if shape.verdict == "follow" { 1.0 } else { -1.0 };
            assert!(
                (way * verdict - want).abs() < 0.3 * want + 0.1,
                "{shape:?}: verdict {verdict}"
            );
            follows += usize::from(shape.verdict == "follow");
            ups += usize::from(shape.side > 0);
        }
        assert!((18..=42).contains(&follows), "{follows} follow-throughs");
        assert!((18..=42).contains(&ups), "{ups} up");
    }

    /// Both funds are seated in every session, at the same pace and sizes;
    /// the decoy sees ahead until the jump is done, then trades against it.
    #[test]
    fn both_funds_are_seated_alike() {
        let family = HeadlineFamily::tsla();
        for (scenario, shape) in family.draw_set("h", 20, 8) {
            let actors = scenario.actors.as_ref().unwrap();
            let (informed, decoy) = (&actors.informed[0], &actors.decoys[0]);
            assert_eq!(informed.wallets, decoy.wallets);
            assert_eq!(informed.intensity_mult, decoy.intensity_mult);
            assert_eq!(informed.size_mult, decoy.size_mult);
            assert_eq!(decoy.informed_for.from, shape.jump_end_slot);
            assert_eq!(decoy.after, DecoyAfter::Contrary);
            assert!(!actors.noise.is_empty() && !actors.chasers.is_empty());
        }
    }
}
