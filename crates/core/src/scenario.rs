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
    /// The most, in USD, the account's value may fall below its running peak
    /// during the session: a prop desk's loss limit. PnL alone pays a bot
    /// that goes deep underwater and gets lucky; the limit makes surviving
    /// the path part of the score.
    pub max_drawdown_usd: f64,
    pub path: Vec<Waypoint>,
    /// PnL the reference bot makes on this scenario; a score of 1 matches it.
    /// Written by `mt calibrate`.
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

    /// Total variation of the path, in percent: the sum of every move's size,
    /// from the starting mark through each waypoint.
    pub fn total_variation_pct(&self) -> f64 {
        let mut level = 0.0;
        let mut total = 0.0;
        for waypoint in &self.path {
            total += (waypoint.level_pct - level).abs();
            level = waypoint.level_pct;
        }
        total
    }

    /// The most a bot could make on this scenario with perfect hindsight:
    /// holding the full position cap the right way through every move, at no
    /// cost. No bot reaches it (fills cost fees and slippage, and the path is
    /// unknown), so a score against it never saturates.
    pub fn hindsight_bound_usd(&self, start_mark_usd: f64) -> f64 {
        self.max_position * start_mark_usd * self.total_variation_pct() / 100.0
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

/// Which way the shock goes. Half the family's scenarios crash, half
/// squeeze, so a blind bet at the open has no edge: a family where every
/// scenario moved the same way would pay a bot for knowing the family, not
/// for reading the market.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    /// The market drops: forced selling.
    Crash,
    /// The market spikes: forced buying.
    Squeeze,
}

impl Direction {
    fn sign(self) -> f64 {
        match self {
            Self::Crash => -1.0,
            Self::Squeeze => 1.0,
        }
    }
}

/// What the market does once the shock has held.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Aftermath {
    /// It reverts, giving back part or all of the shock.
    Revert,
    /// It barely moves back: trading the reversion doesn't pay.
    Stall,
    /// It keeps going the shock's way: fading the shock loses.
    Extend,
}

/// Parameters for a family of shock scenarios: a crash or a squeeze, then
/// its aftermath.
#[derive(Debug, Clone)]
pub struct ShockFamily {
    pub symbol: String,
    /// Range of the shock's size, in percent of the starting mark.
    pub depth_pct: (f64, f64),
    /// Range of the slot the shock starts at.
    pub shock_slot: (u64, u64),
    /// Range of how many slots the market holds the shock before it moves again.
    pub hold_slots: (u64, u64),
    /// Odds of a crash; a squeeze takes the rest.
    pub crash_odds: f64,
    /// Odds of a reversion and of a stall; an extension takes the rest.
    pub revert_odds: f64,
    pub stall_odds: f64,
    /// Range of how much of the shock a reversion gives back, 0 to 1.
    pub revert_recovery: (f64, f64),
    /// Range of how much of the shock a stall gives back, 0 to 1.
    pub stall_recovery: (f64, f64),
    /// Range of how much further an extension goes, in percent of the starting mark.
    pub extend_pct: (f64, f64),
    /// Slots the session continues after the last move.
    pub tail_slots: u64,
    /// The position cap, in base units, every scenario in the family carries.
    pub max_position: f64,
    /// The drawdown limit, in USD, every scenario in the family carries.
    pub max_drawdown_usd: f64,
}

impl ShockFamily {
    pub fn spcx() -> Self {
        Self {
            symbol: "SPCX".into(),
            depth_pct: (5.0, 12.0),
            shock_slot: (60, 120),
            hold_slots: (60, 160),
            crash_odds: 0.5,
            revert_odds: 0.65,
            stall_odds: 0.2,
            revert_recovery: (0.5, 1.0),
            stall_recovery: (0.0, 0.2),
            extend_pct: (2.0, 5.0),
            tail_slots: 80,
            // ~1x the 10,000 USDC of collateral at the snapshot's ~$143 mark.
            max_position: 70.0,
            // 10% of the collateral: the reference's worst path (both
            // tranches on, then stopped out at 13.5%) stays inside it; a
            // blind full-size bet caught by a 12% shock breaks it.
            max_drawdown_usd: 1_000.0,
        }
    }

    /// A scenario drawn from the family. The same seed always yields the same
    /// scenario. About a third of shocks arrive in two legs with a pause in
    /// between, so the extreme isn't always the first print.
    pub fn sample(&self, name: &str, seed: u64) -> Scenario {
        let mut rng = SplitMix64(seed);
        let depth = rng.range_f64(self.depth_pct);
        let shock_slot = rng.range_u64(self.shock_slot);
        let two_legs = rng.next_f64() < 1.0 / 3.0;
        let hold = rng.range_u64(self.hold_slots);
        let aftermath = self.pick_aftermath(rng.next_f64());
        let direction = self.pick_direction(rng.next_f64());
        let sign = direction.sign();

        let round = |x: f64| (x * 100.0).round() / 100.0;
        let extreme = round(sign * depth);
        let mut path = Vec::new();
        let mut slot = shock_slot;
        if two_legs {
            let first = round(extreme * rng.range_f64((0.4, 0.7)));
            path.push(Waypoint {
                slot,
                level_pct: first,
            });
            slot += move_slots(0.0, first) + 1 + rng.range_u64((20, 60));
            path.push(Waypoint {
                slot,
                level_pct: extreme,
            });
            slot += move_slots(first, extreme) + 1;
        } else {
            path.push(Waypoint {
                slot,
                level_pct: extreme,
            });
            slot += move_slots(0.0, extreme) + 1;
        }
        slot += hold;
        let end = match aftermath {
            Aftermath::Revert => round(extreme * (1.0 - rng.range_f64(self.revert_recovery))),
            Aftermath::Stall => round(extreme * (1.0 - rng.range_f64(self.stall_recovery))),
            Aftermath::Extend => round(extreme + sign * rng.range_f64(self.extend_pct)),
        };
        if end != extreme {
            path.push(Waypoint {
                slot,
                level_pct: end,
            });
            slot += move_slots(extreme, end) + 1;
        }

        let legs = if two_legs { "in two legs" } else { "at once" };
        let (moves, then) = match (direction, aftermath) {
            (Direction::Crash, Aftermath::Revert) => ("drops", "rebounds"),
            (Direction::Crash, Aftermath::Stall) => ("drops", "stalls"),
            (Direction::Crash, Aftermath::Extend) => ("drops", "slides further"),
            (Direction::Squeeze, Aftermath::Revert) => ("spikes", "fades"),
            (Direction::Squeeze, Aftermath::Stall) => ("spikes", "stalls"),
            (Direction::Squeeze, Aftermath::Extend) => ("spikes", "keeps running"),
        };
        Scenario {
            name: name.to_string(),
            description: format!(
                "{} {moves} {depth:.1}% {legs} around slot {shock_slot}, then {then} to {end:+.2}% of its starting mark",
                self.symbol
            ),
            symbol: self.symbol.clone(),
            duration_slots: slot + self.tail_slots,
            max_position: self.max_position,
            max_drawdown_usd: self.max_drawdown_usd,
            path,
            reference_pnl_usd: None,
        }
    }

    /// `count` scenarios named `<prefix>-<n>`, stratified: each direction and
    /// aftermath appears in proportion to its odds (largest remainder), so a
    /// small set isn't skewed by luck of the draw. Seeds derive from `seed`;
    /// scenarios keep their draw order.
    pub fn sample_set(&self, prefix: &str, count: usize, seed: u64) -> Vec<Scenario> {
        let mut cells: Vec<((Direction, Aftermath), f64)> = Vec::new();
        for (direction, p_dir) in [
            (Direction::Crash, self.crash_odds),
            (Direction::Squeeze, 1.0 - self.crash_odds),
        ] {
            for (aftermath, p_after) in [
                (Aftermath::Revert, self.revert_odds),
                (Aftermath::Stall, self.stall_odds),
                (Aftermath::Extend, 1.0 - self.revert_odds - self.stall_odds),
            ] {
                cells.push(((direction, aftermath), p_dir * p_after));
            }
        }
        let mut quotas: Vec<((Direction, Aftermath), usize, f64)> = cells
            .iter()
            .map(|&(cell, p)| {
                let exact = p * count as f64;
                (cell, exact.floor() as usize, exact - exact.floor())
            })
            .collect();
        let short = count - quotas.iter().map(|q| q.1).sum::<usize>();
        let mut by_remainder: Vec<usize> = (0..quotas.len()).collect();
        // Stable: ties go to the earlier cell.
        by_remainder.sort_by(|&a, &b| quotas[b].2.total_cmp(&quotas[a].2));
        for &i in by_remainder.iter().take(short) {
            quotas[i].1 += 1;
        }

        let mut scenarios = Vec::with_capacity(count);
        let mut draw = 0u64;
        while scenarios.len() < count {
            draw += 1;
            let draw_seed = seed.wrapping_mul(1_000_003).wrapping_add(draw);
            let cell = self.kind(draw_seed);
            let quota = quotas.iter_mut().find(|q| q.0 == cell).expect("every cell");
            if quota.1 == 0 {
                continue;
            }
            quota.1 -= 1;
            let name = format!("{prefix}-{}", scenarios.len() + 1);
            scenarios.push(self.sample(&name, draw_seed));
        }
        scenarios
    }

    /// The direction and aftermath a seed draws, without building the
    /// scenario: the fifth and sixth draws of [`Self::sample`].
    pub fn kind(&self, seed: u64) -> (Direction, Aftermath) {
        let mut rng = SplitMix64(seed);
        for _ in 0..4 {
            rng.next_u64();
        }
        let aftermath = self.pick_aftermath(rng.next_f64());
        (self.pick_direction(rng.next_f64()), aftermath)
    }

    fn pick_aftermath(&self, draw: f64) -> Aftermath {
        if draw < self.revert_odds {
            Aftermath::Revert
        } else if draw < self.revert_odds + self.stall_odds {
            Aftermath::Stall
        } else {
            Aftermath::Extend
        }
    }

    fn pick_direction(&self, draw: f64) -> Direction {
        if draw < self.crash_odds {
            Direction::Crash
        } else {
            Direction::Squeeze
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
    fn sets_are_stratified_by_direction_and_aftermath() {
        let family = ShockFamily::spcx();
        let set = family.sample_set("h", 12, 77);
        assert_eq!(set.len(), 12);
        let n = |a: &str, b: &str| {
            set.iter()
                .filter(|s| s.description.contains(a) && s.description.contains(b))
                .count()
        };
        assert_eq!(
            [
                n("drops", "rebounds"),
                n("drops", "stalls"),
                n("drops", "slides")
            ],
            [4, 1, 1]
        );
        assert_eq!(
            [
                n("spikes", "fades"),
                n("spikes", "stalls"),
                n("spikes", "keeps running")
            ],
            [4, 1, 1]
        );
        assert_eq!(set, family.sample_set("h", 12, 77));
    }

    #[test]
    fn the_family_draws_every_kind_at_roughly_its_odds() {
        let family = ShockFamily::spcx();
        let draws: Vec<(Direction, Aftermath)> = (0..4_000).map(|seed| family.kind(seed)).collect();
        let share = |f: &dyn Fn(&(Direction, Aftermath)) -> bool| {
            draws.iter().filter(|d| f(d)).count() as f64 / draws.len() as f64
        };
        assert!((share(&|d| d.0 == Direction::Crash) - 0.5).abs() < 0.03);
        assert!((share(&|d| d.1 == Aftermath::Revert) - 0.65).abs() < 0.03);
        assert!((share(&|d| d.1 == Aftermath::Stall) - 0.2).abs() < 0.03);
        assert!((share(&|d| d.1 == Aftermath::Extend) - 0.15).abs() < 0.03);
    }

    #[test]
    fn shocks_go_their_way_and_aftermaths_follow() {
        let family = ShockFamily::spcx();
        for seed in 0..200 {
            let scenario = family.sample("s", seed);
            let (direction, aftermath) = family.kind(seed);
            let sign = direction.sign();
            let levels: Vec<f64> = scenario.path.iter().map(|w| w.level_pct * sign).collect();
            let extreme = levels.iter().cloned().fold(f64::MIN, f64::max);
            let end = *levels.last().unwrap();
            assert!(extreme >= 5.0 && levels[0] > 0.0, "seed {seed}: {levels:?}");
            match aftermath {
                Aftermath::Extend => assert!(end == extreme, "seed {seed}"),
                _ => assert!(end <= extreme && end >= 0.0, "seed {seed}"),
            }
        }
    }

    /// Across the family, a position taken blind at the open and held
    /// through the shock gains on half the draws and loses on the other half.
    #[test]
    fn a_blind_bet_at_the_open_has_no_edge() {
        let family = ShockFamily::spcx();
        let set = family.sample_set("s", 12, 5);
        let short_to_extreme: f64 = set
            .iter()
            .map(|s| {
                -s.path
                    .iter()
                    .map(|w| w.level_pct)
                    .fold(0.0_f64, |acc, l| if l.abs() > acc.abs() { l } else { acc })
            })
            .sum();
        assert!(
            short_to_extreme.abs() < 15.0,
            "a short at the open nets {short_to_extreme:.1}%"
        );
    }

    #[test]
    fn sampled_scenarios_are_valid_and_deterministic() {
        let family = ShockFamily::spcx();
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
            symbol: "SPCX".into(),
            duration_slots: 400,
            max_position: 70.0,
            max_drawdown_usd: 1_000.0,
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
        let scenario = ShockFamily::spcx().sample("s", 7);
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
