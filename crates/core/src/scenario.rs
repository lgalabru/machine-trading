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

/// Parameters for a family of flash-crash scenarios.
#[derive(Debug, Clone)]
pub struct CrashFamily {
    pub symbol: String,
    /// Range of the crash's depth, in percent below the starting mark.
    pub depth_pct: (f64, f64),
    /// Range of the slot the crash starts at.
    pub crash_slot: (u64, u64),
    /// Range of how many slots the market stays down before rebounding.
    pub hold_slots: (u64, u64),
    /// Range of how much of the drop the rebound recovers, 0 to 1.
    pub recovery: (f64, f64),
    /// Slots the session continues after the rebound.
    pub tail_slots: u64,
}

impl CrashFamily {
    pub fn sol() -> Self {
        Self {
            symbol: "SOL".into(),
            depth_pct: (5.0, 12.0),
            crash_slot: (60, 120),
            hold_slots: (60, 160),
            recovery: (0.5, 1.0),
            tail_slots: 80,
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
        let recovery = rng.range_f64(self.recovery);
        let hold = rng.range_u64(self.hold_slots);

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
        let rebound = round(bottom * (1.0 - recovery));
        path.push(Waypoint {
            slot,
            level_pct: rebound,
        });
        slot += move_slots(bottom, rebound) + 1;

        let legs = if two_legs { "in two legs" } else { "at once" };
        Scenario {
            name: name.to_string(),
            description: format!(
                "{} drops {depth:.1}% {legs} around slot {crash_slot}, then rebounds to {rebound:+.2}% of its starting mark",
                self.symbol
            ),
            symbol: self.symbol.clone(),
            duration_slots: slot + self.tail_slots,
            path,
            reference_pnl_usd: None,
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
