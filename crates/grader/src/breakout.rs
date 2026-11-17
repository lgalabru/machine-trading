//! The breakout family: the price runs through round levels, and each run is
//! either a real break, which the level follows, or a stop hunt, which it
//! doesn't.
//!
//! - **The price** walks a market's calibration (GARCH-t on one-minute
//!   blocks, its tails and volatility states), inside regular hours.
//! - **The levels** are a grid (`level_step_usd`). Before each event the walk
//!   drifts, over up to a few hundred slots, to a point just short of the
//!   grid level nearest it (`approach_bps` of it, under the hunter's
//!   trigger), so runs go up and down with equal odds.
//! - **The run.** At the event the book jumps through the level by an
//!   excursion: the makers re-centre `excursion_pct` past it while the oracle
//!   (the scenario's level) stays where it was. Hunts and breaks draw it from
//!   one distribution, so the first look at the book can't tell them apart.
//! - **A hunt** holds the book there for the hunter's `hold_slots`, then the
//!   makers go back to the level. The stop hunter pushes from its wallet a
//!   couple of slots before the run starts, and trades back out while the
//!   book is still through the level. One wallet each: a reader who has
//!   seen a session's first hunt and first break knows who is who.
//! - **A break** leaves the book there for a lead drawn from the same range
//!   as the hunts' hold, while the oracle starts to follow (a creep, drawn,
//!   under the walk's noise at first); then the level jumps on through the
//!   book to the break's continuation and stays. An informed breaker sweeps
//!   through the level a couple of slots before, from a wallet of its own,
//!   exactly as the hunter pushes (sizes and holds from the same
//!   ranges, from a wallet that was flat), and trades back out a hold later;
//!   informed wallets, who see the level ahead, trade it before it goes.
//! - **Mirrored.** What a follower makes from the excursion on in a break is
//!   drawn to match what it loses in a hunt (the excursion plus the
//!   approach), so a policy that trades every run the same way nets about
//!   its costs.
//!
//! No tells: every session seats the same actors (noise, chasers, the
//! informed wallets and the hunter's), has both hunts and breaks, and draws
//! their approach, excursion and timing alike.

use std::collections::BTreeMap;

use mtx_core::mandate::Mandate;
use serde::Serialize;

use crate::{
    actors::{
        Library,
        library::{Chaser, Informed, Noise, Scale, Sees, StopHunter},
    },
    market::{
        Calibration, MarketModel, ModelSpec,
        calibration::sha256_hex,
        model::{ModelPath, Start},
    },
    orders::CloseOut,
    scenario::{Scenario, SplitMix64, VENUE, Waypoint, move_slots},
};

/// TSLA's market calibration, built in so hidden sets need no file.
const TSLA_CALIBRATION: &str =
    include_str!("../../../calibration/market/tsla.20261113.8b7bca8e.toml");

/// How often an informed trader's view is right: the library's default.
const INFORMED_HIT_RATE: f64 = 0.85;

/// Slots before the run that the hunter is due: it pushes into the book as
/// it stands, and the run follows.
const HUNTER_EARLY_SLOTS: u64 = 2;

/// One run through a level, as drawn.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Event {
    /// `hunt` or `break`.
    pub kind: String,
    /// The slot the book jumps through the level.
    pub slot: u64,
    /// `1` up through the level, `-1` down.
    pub side: i8,
    /// The grid level, in USD.
    pub level_usd: f64,
    /// How far short of the level the walk is brought, in basis points.
    pub approach_bps: f64,
    /// How far through the level the book jumps, in percent of the level.
    pub excursion_pct: f64,
    /// Slots the book holds there: the hunter's hold, or the break's lead.
    pub hold_slots: u64,
    /// A break's continuation past the level, in percent, and how far the
    /// oracle creeps toward the level during its lead, as a share of the
    /// approach.
    pub continuation_pct: Option<f64>,
    pub creep: Option<f64>,
    /// The slots a break's level takes to jump through the book.
    pub leg_slots: Option<u64>,
}

/// What a session is, beneath its path: the events and the actors'
/// settings.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Shape {
    pub events: Vec<Event>,
    /// The stop hunter's settings: its hold, its trigger and the grid.
    pub hold_slots: u64,
    pub trigger_bps: f64,
    pub level_step_usd: f64,
    /// How far ahead the informed wallets see the level, and how many there
    /// are.
    pub horizon_slots: u64,
    pub informed_wallets: u32,
    /// The informed breakers' sweep, in the market's trade sizes, and how
    /// long they hold it.
    pub sweep_mult: f64,
    pub sweep_hold_slots: u64,
}

/// The family's parameters. Every range is inclusive.
#[derive(Debug, Clone)]
pub struct BreakoutFamily {
    pub symbol: String,
    /// The snapshot's mark, which the session starts at, in USD: where the
    /// grid is laid from.
    pub start_usd: f64,
    pub duration: (u64, u64),
    pub waypoint_slots: u64,
    pub level_step_usd: f64,
    /// Hunts and breaks a session, each.
    pub hunts: (u64, u64),
    pub breaks: (u64, u64),
    /// No event in the first and last slots of a session, and at least
    /// `gap_slots` between two.
    pub head_slots: u64,
    pub tail_slots: u64,
    pub gap_slots: u64,
    /// Slots the walk takes to drift to the level, at most.
    pub approach_slots: u64,
    /// The hunter's trigger, in basis points; the approach stops this far
    /// inside it, at least, and at least `approach_floor_bps` short.
    pub trigger_bps: (f64, f64),
    pub approach_floor_bps: f64,
    pub approach_margin_bps: f64,
    pub excursion_pct: (f64, f64),
    /// A break's continuation beyond its excursion, in percent.
    pub follow_through_pct: (f64, f64),
    /// The hunter's hold, and a break's lead.
    pub hold_slots: (u64, u64),
    /// How far the oracle creeps during a break's lead, as a share of the
    /// approach.
    pub creep: (f64, f64),
    /// The slots a break's level jump takes, from the end of its lead.
    pub leg_slots: (u64, u64),
    /// The informed wallets: how many, how far ahead they see, and how far
    /// the level must go before they trade.
    pub informed_wallets: (u64, u64),
    pub horizon_slots: (u64, u64),
    pub informed_edge_bps: f64,
    pub informed_intensity: f64,
    pub informed_size: f64,
    /// The hunter's push, in the market's trade sizes.
    pub push_mult: (f64, f64),
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

impl BreakoutFamily {
    pub fn tsla() -> Self {
        Self {
            symbol: "TSLA".into(),
            start_usd: 388.95,
            duration: (1_400, 2_200),
            waypoint_slots: 4,
            level_step_usd: 2.0,
            hunts: (1, 3),
            breaks: (1, 3),
            head_slots: 200,
            tail_slots: 240,
            gap_slots: 180,
            approach_slots: 300,
            trigger_bps: (10.0, 30.0),
            approach_floor_bps: 3.0,
            approach_margin_bps: 3.0,
            excursion_pct: (0.15, 0.45),
            follow_through_pct: (0.15, 0.65),
            hold_slots: (10, 60),
            creep: (0.0, 1.5),
            leg_slots: (4, 12),
            informed_wallets: (2, 3),
            horizon_slots: (20, 60),
            informed_edge_bps: 25.0,
            informed_intensity: 40.0,
            informed_size: 60.0,
            push_mult: (300.0, 600.0),
            noise_wallets: 4,
            noise_flow: 15.0,
            noise_size: 25.0,
            chaser_wallets: 3,
            chaser_trigger_bps: 15.0,
            chaser_intensity: 30.0,
            chaser_size: 30.0,
            // About 0.47x of the 10,000 USDC of collateral at $388.95.
            max_position: 12.0,
            max_drawdown_usd: 500.0,
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

        // The session's process: the hunter's and the informed wallets'.
        let hold_slots = rng.range_u64(self.hold_slots);
        let trigger_bps = round2(rng.range_f64(self.trigger_bps));
        let push_mult = round2(rng.range_f64(self.push_mult));
        // The informed breakers' sweeps: the same sizes and holds as the
        // hunter's, drawn on their own.
        let sweep_mult = round2(rng.range_f64(self.push_mult));
        let sweep_hold_slots = rng.range_u64(self.hold_slots);
        let horizon_slots = rng.range_u64(self.horizon_slots);
        let informed_wallets = rng.range_u64(self.informed_wallets) as u32;

        // The events: hunts and breaks, shuffled, spaced through the session.
        let mut kinds: Vec<bool> = Vec::new();
        kinds.extend((0..rng.range_u64(self.hunts)).map(|_| true));
        kinds.extend((0..rng.range_u64(self.breaks)).map(|_| false));
        let room = duration.saturating_sub(self.head_slots + self.tail_slots);
        let fit = (room / self.gap_slots + 1) as usize;
        for i in (1..kinds.len()).rev() {
            kinds.swap(i, (rng.next_u64() % (i as u64 + 1)) as usize);
        }
        kinds.truncate(fit.max(2));
        let slack = room.saturating_sub(self.gap_slots * (kinds.len() as u64 - 1));
        let mut offsets: Vec<u64> = kinds.iter().map(|_| rng.next_u64() % (slack + 1)).collect();
        offsets.sort_unstable();
        let slots: Vec<u64> = offsets
            .iter()
            .enumerate()
            .map(|(i, o)| self.head_slots + o + i as u64 * self.gap_slots)
            .collect();

        let mut events = Vec::new();
        let mut free_from = 0u64;
        for (&hunt, &slot) in kinds.iter().zip(&slots) {
            let tie: i8 = if rng.next_f64() < 0.5 { 1 } else { -1 };
            let approach_bps = round2(rng.range_f64((
                self.approach_floor_bps,
                (trigger_bps - self.approach_margin_bps).max(self.approach_floor_bps),
            )));
            let excursion_pct = round3(rng.range_f64(self.excursion_pct));
            let follow = rng.range_f64(self.follow_through_pct);
            let lead = rng.range_u64(self.hold_slots);
            let creep = rng.range_f64(self.creep);
            let leg = rng.range_u64(self.leg_slots);

            // Bring the walk to just short of the grid level nearest it: the
            // run goes through it from the side the walk is on, up or down
            // with equal odds, and the drift there is at most half a step.
            let usd = |log: f64| self.start_usd * log.exp();
            let here = usd(walk.log_levels[slot as usize]);
            let step = self.level_step_usd;
            let level_usd = (here / step).round() * step;
            let side = match level_usd.partial_cmp(&here) {
                Some(std::cmp::Ordering::Greater) => 1,
                Some(std::cmp::Ordering::Less) => -1,
                _ => tie,
            };
            let s = f64::from(side);
            let target = level_usd * (1.0 - s * approach_bps / 10_000.0);
            let shift = (target / here).ln();
            let from = slot.saturating_sub(self.approach_slots).max(free_from);
            add_ramp(&mut walk, from, slot, shift);

            let (hold, continuation_pct, creep, leg) = if hunt {
                (hold_slots, None, None, None)
            } else {
                // The oracle creeps toward the level through the lead, then
                // jumps on to the continuation over the leg, and stays.
                let creep_log = (1.0 + s * creep * approach_bps / 10_000.0).ln();
                add_ramp(&mut walk, slot, slot + lead, creep_log);
                let continuation = excursion_pct + follow;
                let end_usd = level_usd * (1.0 + s * continuation / 100.0);
                let now_usd = usd(walk.log_levels[(slot + lead) as usize]);
                add_ramp(&mut walk, slot + lead, slot + lead + leg, (end_usd / now_usd).ln());
                (
                    lead,
                    Some(round3(continuation)),
                    Some(round3(creep)),
                    Some(leg),
                )
            };
            free_from = slot + hold + self.leg_slots.1;
            events.push(Event {
                kind: if hunt { "hunt" } else { "break" }.into(),
                slot,
                side,
                level_usd,
                approach_bps,
                excursion_pct,
                hold_slots: hold,
                continuation_pct,
                creep,
                leg_slots: leg,
            });
        }

        let path = self.waypoints(&walk, &events);
        let actors = self.actors(
            model,
            &events,
            trigger_bps,
            (push_mult, hold_slots),
            (sweep_mult, sweep_hold_slots),
            horizon_slots,
            informed_wallets,
        );
        let mandate = Mandate::perp(VENUE, &self.symbol, self.max_position, self.max_drawdown_usd);
        let shape = Shape {
            events,
            hold_slots,
            trigger_bps,
            level_step_usd: self.level_step_usd,
            horizon_slots,
            informed_wallets,
            sweep_mult,
            sweep_hold_slots,
        };
        let description = format!(
            "{}: round levels every ${}; position within {} either way",
            self.symbol, self.level_step_usd, self.max_position
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
            start_positions: Default::default(),
            close_out: BTreeMap::from([(
                self.symbol.clone(),
                CloseOut {
                    haircut_bps: self.close_haircut_bps,
                },
            )]),
            tx_layer: None,
            stop_out: None,
            liquidator: None,
            markets: Vec::new(),
            joint: None,
        };
        (scenario, shape)
    }

    /// The path as waypoints: the walk every `waypoint_slots`, with the
    /// book's excursion as a premium over the level through each run, and a
    /// waypoint exactly where each run starts and where its book comes back.
    fn waypoints(&self, walk: &ModelPath, events: &[Event]) -> Vec<Waypoint> {
        let duration = (walk.log_levels.len() - 1) as u64;
        let level_pct = |slot: u64| round3(walk.level_pct(slot));
        // The book's own level through a run: the excursion past the grid
        // level, until the hold ends (a hunt) or the level jumps on past it
        // (a break: the book never comes back before the level goes).
        let premium_pct = |slot: u64| -> f64 {
            let level = self.start_usd * (1.0 + level_pct(slot) / 100.0);
            events
                .iter()
                .find_map(|e| {
                    let end = e.slot + e.hold_slots + e.leg_slots.unwrap_or(0);
                    if slot < e.slot || slot >= end {
                        return None;
                    }
                    let s = f64::from(e.side);
                    let book = e.level_usd * (1.0 + s * e.excursion_pct / 100.0);
                    let premium = book / level - 1.0;
                    Some(if s * premium > 0.0 {
                        round3(premium * 100.0)
                    } else {
                        0.0
                    })
                })
                .unwrap_or(0.0)
        };
        let forced: Vec<u64> = events
            .iter()
            .flat_map(|e| [e.slot, e.slot + e.hold_slots + e.leg_slots.unwrap_or(0)])
            .collect();
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
        let mut last = (0.0, 0.0);
        for (slot, forced) in candidates {
            if slot >= duration || (!forced && slot < free_from) {
                continue;
            }
            let waypoint = Waypoint {
                slot,
                level_pct: level_pct(slot),
                premium_pct: premium_pct(slot),
            };
            if (waypoint.level_pct, waypoint.premium_pct) == last {
                continue;
            }
            let book_before = out.last().map_or(0.0, |w| w.book_pct());
            let done = slot + move_slots(book_before, waypoint.book_pct()) + 1;
            if done > duration {
                break;
            }
            last = (waypoint.level_pct, waypoint.premium_pct);
            free_from = done;
            out.push(waypoint);
        }
        out
    }

    #[allow(clippy::too_many_arguments)]
    fn actors(
        &self,
        model: &MarketModel,
        events: &[Event],
        trigger_bps: f64,
        (push_mult, hold_slots): (f64, u64),
        (sweep_mult, sweep_hold_slots): (f64, u64),
        horizon_slots: u64,
        informed_wallets: u32,
    ) -> Library {
        let pushes = |kind: &str| -> Vec<u64> {
            events
                .iter()
                .filter(|e| e.kind == kind)
                .map(|e| e.slot.saturating_sub(HUNTER_EARLY_SLOTS))
                .collect()
        };
        let push = |push_mult, hold_slots, at_slots| StopHunter {
            wallets: 1,
            level_step_usd: self.level_step_usd,
            trigger_bps,
            push_mult,
            size_sigma: None,
            overshoot_ticks: 100,
            hold_slots,
            cooldown_slots: 0,
            at_slots,
            limit_bps: 80.0,
            jitter: crate::actors::library::JITTER,
            active: None,
        };
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
                wallets: informed_wallets,
                horizon_slots,
                edge_bps: self.informed_edge_bps,
                sees: Sees::Level,
                sweeps: Some(push(sweep_mult, sweep_hold_slots, pushes("break"))),
                hit_rate: INFORMED_HIT_RATE,
                view_noise_bps: 0.0,
                intensity_mult: self.informed_intensity,
                size_mult: self.informed_size,
                limit_bps: 80.0,
                active: None,
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
            stop_hunters: vec![push(push_mult, hold_slots, pushes("hunt"))],
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

/// Adds `shift` (in log price) to the walk from `from` to `to`, linearly,
/// and holds it after `to`.
fn add_ramp(walk: &mut ModelPath, from: u64, to: u64, shift: f64) {
    let span = to.saturating_sub(from).max(1) as f64;
    for (slot, level) in walk.log_levels.iter_mut().enumerate() {
        let slot = slot as u64;
        if slot <= from {
            continue;
        }
        let share = ((slot - from) as f64 / span).min(1.0);
        *level += shift * share;
    }
}

fn round2(x: f64) -> f64 {
    (x * 100.0).round() / 100.0
}

fn round3(x: f64) -> f64 {
    (x * 1000.0).round() / 1000.0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::making::{book_at, level_at};

    #[test]
    fn sessions_are_valid_and_deterministic() {
        let family = BreakoutFamily::tsla();
        for seed in 0..40 {
            let (scenario, shape) = family.draw("s", seed);
            assert_eq!(scenario.validate(), Ok(()), "seed {seed}");
            assert_eq!(scenario, family.sample("s", seed));
            assert!(shape.events.iter().any(|e| e.kind == "hunt"), "seed {seed}");
            assert!(shape.events.iter().any(|e| e.kind == "break"), "seed {seed}");
            let v2 = crate::scenario_v2::ScenarioV2::try_from(&scenario).unwrap();
            assert_eq!(v2.into_scenario().unwrap(), scenario);
        }
    }

    /// Each run puts the book through its level; a hunt's oracle stays short
    /// of it and its book comes back, a break's oracle ends past it.
    #[test]
    fn hunts_come_back_and_breaks_follow_through() {
        let family = BreakoutFamily::tsla();
        let start = family.start_usd;
        let usd = |pct: f64| start * (1.0 + pct / 100.0);
        let (mut hunts, mut breaks, mut back) = (0, 0, 0);
        for (scenario, shape) in family.draw_set("h", 40, 5) {
            let path = &scenario.path;
            for e in &shape.events {
                let s = f64::from(e.side);
                let through = |p: f64| s * (p - e.level_usd) > 0.0;
                assert!(through(usd(book_at(path, e.slot))), "{e:?}");
                assert!(!through(usd(level_at(path, e.slot))), "{e:?}");
                let after = e.slot + e.hold_slots + family.leg_slots.1 + 8;
                if e.kind == "hunt" {
                    hunts += 1;
                    // The book holds through the level, and comes back
                    // unless the walk itself has crossed.
                    assert!(through(usd(book_at(path, e.slot + e.hold_slots - 1))));
                    back += usize::from(!through(usd(book_at(path, after))));
                } else {
                    breaks += 1;
                    // The book never comes back through a break.
                    for slot in e.slot..after {
                        assert!(through(usd(book_at(path, slot))), "{e:?} at {slot}");
                    }
                    assert!(through(usd(level_at(path, after))), "{e:?}");
                }
            }
        }
        assert!(hunts > 40 && breaks > 40, "{hunts} hunts, {breaks} breaks");
        assert!(back * 10 >= hunts * 9, "{back} of {hunts} hunts came back");
    }

    /// The hunter is due just before each hunt, and only then; every actor
    /// the roster tells is seated in every session.
    #[test]
    fn the_hunter_is_due_at_each_hunt_and_everyone_is_seated() {
        let family = BreakoutFamily::tsla();
        for (scenario, shape) in family.draw_set("h", 20, 8) {
            let actors = scenario.actors.as_ref().unwrap();
            let hunter = &actors.stop_hunters[0];
            let hunts: Vec<u64> = shape
                .events
                .iter()
                .filter(|e| e.kind == "hunt")
                .map(|e| e.slot - HUNTER_EARLY_SLOTS)
                .collect();
            assert_eq!(hunter.at_slots, hunts);
            assert_eq!(hunter.hold_slots, shape.hold_slots);
            assert_eq!(actors.informed.len(), 1);
            assert_eq!(actors.informed[0].sees, Sees::Level);
            // Every break is led by an informed sweep, drawn as a hunt's push.
            let sweeps = actors.informed[0].sweeps.as_ref().unwrap();
            let breaks: Vec<u64> = shape
                .events
                .iter()
                .filter(|e| e.kind == "break")
                .map(|e| e.slot - HUNTER_EARLY_SLOTS)
                .collect();
            assert_eq!(sweeps.at_slots, breaks);
            assert_eq!(sweeps.wallets, hunter.wallets);
            for push in [sweeps, hunter] {
                assert!((300.0..=600.0).contains(&push.push_mult), "{push:?}");
                assert!((10..=60).contains(&push.hold_slots), "{push:?}");
                assert_eq!(push.trigger_bps, shape.trigger_bps);
            }
            assert!(!actors.noise.is_empty() && !actors.chasers.is_empty());
        }
    }
}
