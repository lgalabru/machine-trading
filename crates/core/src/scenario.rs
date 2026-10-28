//! Market scenarios: the price path a trading session replays.
//!
//! A scenario is a list of waypoints, each a mark price relative to the mark
//! at the start of the session. It is independent of any one snapshot: the
//! same file replays on any snapshot of the same market. Waypoints compile to
//! Surfpool overrides that walk Phoenix's oracle and book to each level in
//! steps Phoenix accepts.

use std::path::Path;

use serde::{Deserialize, Serialize};
use solana_pubkey::Pubkey;

use crate::{
    Error, Result,
    chain::Override,
    error::io_error,
    phoenix::{SLOTS_BETWEEN_STEPS, market_move},
};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Scenario {
    pub name: String,
    pub description: String,
    pub symbol: String,
    /// Session length, counted from when the bot starts. The account is
    /// marked to market at this slot.
    pub duration_slots: u64,
    /// The largest position, long or short, in base units, a bot may hold.
    /// Without a cap, PnL in dollars rewards size as much as skill: a bot
    /// betting several times the reference's size beats it on any profitable
    /// family. The reference trades up to the cap, so the reward compares
    /// strategies at equal size.
    pub max_position: f64,
    pub path: Vec<Waypoint>,
    /// PnL the reference bot makes on this scenario; a score of 1 matches it.
    /// Written by `ott calibrate`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reference_pnl_usd: Option<f64>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Waypoint {
    /// Slot, relative to the session start, at which the move begins.
    pub slot: u64,
    /// Target mark relative to the session's starting mark, in percent:
    /// `-8.0` is 8% below it, `0.0` is back to where it started.
    pub level_pct: f64,
}

/// Lowest level a waypoint may target. Past this, Phoenix's risk checks and
/// the market's resting liquidity stop behaving like a real market.
const MIN_LEVEL_PCT: f64 = -40.0;

impl Scenario {
    pub fn load(path: &Path) -> Result<Self> {
        let contents = std::fs::read_to_string(path).map_err(io_error(path))?;
        let scenario: Self = serde_json::from_str(&contents)?;
        scenario
            .validate()
            .map_err(|reason| Error::InvalidScenario {
                path: path.to_path_buf(),
                reason,
            })?;
        Ok(scenario)
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        let contents = serde_json::to_string_pretty(self)? + "\n";
        std::fs::write(path, contents).map_err(io_error(path))
    }

    /// Waypoints must be in order, each move must finish before the next one
    /// starts, and the last must finish before the session ends.
    pub fn validate(&self) -> std::result::Result<(), String> {
        let mut level = 0.0;
        let mut free_from = 0;
        for (i, waypoint) in self.path.iter().enumerate() {
            if !(MIN_LEVEL_PCT..=100.0).contains(&waypoint.level_pct) {
                return Err(format!(
                    "waypoint {i}: level {}% is outside [{MIN_LEVEL_PCT}, 100]",
                    waypoint.level_pct
                ));
            }
            if waypoint.slot < free_from {
                return Err(format!(
                    "waypoint {i} starts at slot {} while the previous move runs until {free_from}",
                    waypoint.slot
                ));
            }
            free_from = waypoint.slot + move_slots(level, waypoint.level_pct) + 1;
            level = waypoint.level_pct;
        }
        if free_from > self.duration_slots {
            return Err(format!(
                "the last move runs until slot {free_from}, past the session end at {}",
                self.duration_slots
            ));
        }
        Ok(())
    }

    /// Surfpool overrides for the whole path, with slots relative to when
    /// they are registered, given the mark at that moment.
    pub fn overrides(&self, perp_asset_map: Pubkey, start_mark_ticks: u64) -> Vec<Override> {
        let ticks_at =
            |level_pct: f64| (start_mark_ticks as f64 * (1.0 + level_pct / 100.0)).round() as u64;
        let mut from = start_mark_ticks;
        let mut overrides = Vec::new();
        for waypoint in &self.path {
            let to = ticks_at(waypoint.level_pct);
            let (moves, _) = market_move(perp_asset_map, &self.symbol, from, to, waypoint.slot);
            overrides.extend(moves);
            from = to;
        }
        overrides
    }
}

/// Slots a move between two levels spans, uncross included. Mirrors
/// [`market_move`]'s stepping.
fn move_slots(from_pct: f64, to_pct: f64) -> u64 {
    let ratio = (1.0 + to_pct / 100.0) / (1.0 + from_pct / 100.0);
    let steps = (((ratio - 1.0).abs() * 100.0 / 4.0).ceil() as u64).max(1);
    steps * SLOTS_BETWEEN_STEPS
}

/// What the market does once the crash has held.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Aftermath {
    /// It rebounds, recovering part or all of the drop.
    Rebound,
    /// It barely recovers: buying the dip doesn't pay.
    Stall,
    /// It keeps falling: buying the dip loses.
    Slide,
}

/// Parameters for a family of flash-crash scenarios.
#[derive(Debug, Clone)]
pub struct CrashFamily {
    pub symbol: String,
    /// Range of the crash's depth, in percent below the starting mark.
    pub depth_pct: (f64, f64),
    /// Range of the slot the crash starts at.
    pub crash_slot: (u64, u64),
    /// Range of how many slots the market stays down before it moves again.
    pub hold_slots: (u64, u64),
    /// Odds of a rebound and of a stall; a slide takes the rest.
    pub rebound_odds: f64,
    pub stall_odds: f64,
    /// Range of how much of the drop a rebound recovers, 0 to 1.
    pub rebound_recovery: (f64, f64),
    /// Range of how much of the drop a stall recovers, 0 to 1.
    pub stall_recovery: (f64, f64),
    /// Range of how much further a slide falls, in percent of the starting mark.
    pub slide_pct: (f64, f64),
    /// Slots the session continues after the last move.
    pub tail_slots: u64,
    /// The position cap, in base units, every scenario in the family carries.
    pub max_position: f64,
}

impl CrashFamily {
    pub fn sol() -> Self {
        Self {
            symbol: "SOL".into(),
            depth_pct: (5.0, 12.0),
            crash_slot: (60, 120),
            hold_slots: (60, 160),
            rebound_odds: 0.65,
            stall_odds: 0.2,
            rebound_recovery: (0.5, 1.0),
            stall_recovery: (0.0, 0.2),
            slide_pct: (2.0, 5.0),
            tail_slots: 80,
            // ~1x the 10,000 USDC of collateral at the snapshot's ~$101 mark.
            max_position: 100.0,
        }
    }

    /// A scenario drawn from the family. The same seed always yields the same
    /// scenario. About a third of crashes arrive in two legs with a pause in
    /// between, so the bottom isn't always the first print.
    pub fn sample(&self, name: &str, seed: u64) -> Scenario {
        let mut rng = SplitMix64(seed);
        let depth = rng.range_f64(self.depth_pct);
        let crash_slot = rng.range_u64(self.crash_slot);
        let two_legs = rng.next_f64() < 1.0 / 3.0;
        let hold = rng.range_u64(self.hold_slots);
        let aftermath = self.pick_aftermath(rng.next_f64());

        let round = |x: f64| (x * 100.0).round() / 100.0;
        let bottom = round(-depth);
        let mut path = Vec::new();
        let mut slot = crash_slot;
        if two_legs {
            let first = round(bottom * rng.range_f64((0.4, 0.7)));
            path.push(Waypoint {
                slot,
                level_pct: first,
            });
            slot += move_slots(0.0, first) + 1 + rng.range_u64((20, 60));
            path.push(Waypoint {
                slot,
                level_pct: bottom,
            });
            slot += move_slots(first, bottom) + 1;
        } else {
            path.push(Waypoint {
                slot,
                level_pct: bottom,
            });
            slot += move_slots(0.0, bottom) + 1;
        }
        slot += hold;
        let end = match aftermath {
            Aftermath::Rebound => round(bottom * (1.0 - rng.range_f64(self.rebound_recovery))),
            Aftermath::Stall => round(bottom * (1.0 - rng.range_f64(self.stall_recovery))),
            Aftermath::Slide => round(bottom - rng.range_f64(self.slide_pct)),
        };
        if end != bottom {
            path.push(Waypoint {
                slot,
                level_pct: end,
            });
            slot += move_slots(bottom, end) + 1;
        }

        let legs = if two_legs { "in two legs" } else { "at once" };
        let then = match aftermath {
            Aftermath::Rebound => "rebounds",
            Aftermath::Stall => "stalls",
            Aftermath::Slide => "slides further",
        };
        Scenario {
            name: name.to_string(),
            description: format!(
                "{} drops {depth:.1}% {legs} around slot {crash_slot}, then {then} to {end:+.2}% of its starting mark",
                self.symbol
            ),
            symbol: self.symbol.clone(),
            duration_slots: slot + self.tail_slots,
            max_position: self.max_position,
            path,
            reference_pnl_usd: None,
        }
    }

    /// `count` scenarios named `<prefix>-<n>`, stratified: each aftermath
    /// appears in proportion to its odds (largest remainder), so a small set
    /// isn't skewed by luck of the draw. Seeds derive from `seed`; scenarios
    /// keep their draw order.
    pub fn sample_set(&self, prefix: &str, count: usize, seed: u64) -> Vec<Scenario> {
        let odds = [
            (Aftermath::Rebound, self.rebound_odds),
            (Aftermath::Stall, self.stall_odds),
            (Aftermath::Slide, 1.0 - self.rebound_odds - self.stall_odds),
        ];
        let mut quotas: Vec<(Aftermath, usize, f64)> = odds
            .iter()
            .map(|&(a, p)| {
                let exact = p * count as f64;
                (a, exact.floor() as usize, exact - exact.floor())
            })
            .collect();
        let short = count - quotas.iter().map(|q| q.1).sum::<usize>();
        let mut by_remainder: Vec<usize> = (0..quotas.len()).collect();
        by_remainder.sort_by(|&a, &b| quotas[b].2.total_cmp(&quotas[a].2));
        for &i in by_remainder.iter().take(short) {
            quotas[i].1 += 1;
        }

        let mut scenarios = Vec::with_capacity(count);
        let mut draw = 0u64;
        while scenarios.len() < count {
            draw += 1;
            let draw_seed = seed.wrapping_mul(1_000_003).wrapping_add(draw);
            let aftermath = self.aftermath(draw_seed);
            let quota = quotas
                .iter_mut()
                .find(|q| q.0 == aftermath)
                .expect("every aftermath");
            if quota.1 == 0 {
                continue;
            }
            quota.1 -= 1;
            let name = format!("{prefix}-{}", scenarios.len() + 1);
            scenarios.push(self.sample(&name, draw_seed));
        }
        scenarios
    }

    /// The aftermath a seed draws, without building the scenario: the fifth
    /// draw of [`Self::sample`].
    pub fn aftermath(&self, seed: u64) -> Aftermath {
        let mut rng = SplitMix64(seed);
        for _ in 0..4 {
            rng.next_u64();
        }
        self.pick_aftermath(rng.next_f64())
    }

    fn pick_aftermath(&self, draw: f64) -> Aftermath {
        if draw < self.rebound_odds {
            Aftermath::Rebound
        } else if draw < self.rebound_odds + self.stall_odds {
            Aftermath::Stall
        } else {
            Aftermath::Slide
        }
    }
}

/// Small, dependency-free, and stable across platforms and releases, which a
/// generator for checked-in task data needs more than statistical quality.
struct SplitMix64(u64);

impl SplitMix64 {
    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    fn next_f64(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }

    fn range_f64(&mut self, (lo, hi): (f64, f64)) -> f64 {
        lo + (hi - lo) * self.next_f64()
    }

    fn range_u64(&mut self, (lo, hi): (u64, u64)) -> u64 {
        lo + self.next_u64() % (hi - lo + 1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sets_are_stratified_by_aftermath() {
        let family = CrashFamily::sol();
        for (count, expected) in [(12, [8, 2, 2]), (6, [4, 1, 1])] {
            let set = family.sample_set("h", count, 77);
            assert_eq!(set.len(), count);
            let n = |word: &str| set.iter().filter(|s| s.description.contains(word)).count();
            assert_eq!(
                [n("rebounds"), n("stalls"), n("slides")],
                expected,
                "{count}"
            );
            assert_eq!(set, family.sample_set("h", count, 77));
        }
    }

    #[test]
    fn the_family_draws_every_aftermath_at_roughly_its_odds() {
        let family = CrashFamily::sol();
        let draws: Vec<Aftermath> = (0..2_000).map(|seed| family.aftermath(seed)).collect();
        let share = |a| draws.iter().filter(|d| **d == a).count() as f64 / draws.len() as f64;
        assert!((share(Aftermath::Rebound) - 0.65).abs() < 0.05);
        assert!((share(Aftermath::Stall) - 0.2).abs() < 0.05);
        assert!((share(Aftermath::Slide) - 0.15).abs() < 0.05);
        for seed in 0..50 {
            let scenario = family.sample("s", seed);
            let end = scenario.path.last().unwrap().level_pct;
            let bottom = scenario
                .path
                .iter()
                .map(|w| w.level_pct)
                .fold(0.0, f64::min);
            match family.aftermath(seed) {
                Aftermath::Slide => assert_eq!(end, bottom, "seed {seed}"),
                _ => assert!(end >= bottom, "seed {seed}"),
            }
        }
    }

    #[test]
    fn sampled_scenarios_are_valid_and_deterministic() {
        let family = CrashFamily::sol();
        for seed in 0..200 {
            let scenario = family.sample("s", seed);
            assert_eq!(scenario.validate(), Ok(()), "seed {seed}: {scenario:?}");
            assert_eq!(scenario, family.sample("s", seed));
        }
    }

    #[test]
    fn overlapping_moves_are_rejected() {
        let scenario = Scenario {
            name: "overlap".into(),
            description: String::new(),
            symbol: "SOL".into(),
            duration_slots: 400,
            max_position: 100.0,
            path: vec![
                Waypoint {
                    slot: 10,
                    level_pct: -12.0,
                },
                Waypoint {
                    slot: 12,
                    level_pct: 0.0,
                },
            ],
            reference_pnl_usd: None,
        };
        assert!(scenario.validate().unwrap_err().contains("previous move"));
    }

    #[test]
    fn overrides_walk_to_each_level_and_uncross() {
        let scenario = CrashFamily::sol().sample("s", 7);
        let overrides = scenario.overrides(Pubkey::new_unique(), 10_000);
        let last_move = overrides
            .iter()
            .rev()
            .find(|o| o.template_id == "phoenix-market-move")
            .unwrap();
        let expected =
            (10_000.0 * (1.0 + scenario.path.last().unwrap().level_pct / 100.0)).round() as u64;
        assert_eq!(last_move.values["target_ticks"], expected.to_string());
        assert_eq!(
            overrides.last().unwrap().template_id,
            "phoenix-market-uncross"
        );
    }
}
