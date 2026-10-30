//! Market scenarios: the price path a trading session replays.
//!
//! A scenario is a list of waypoints, each a mark price relative to the mark
//! at the start of the session. It is independent of any one snapshot: the
//! same file replays on any snapshot of the same market. Waypoints compile to
//! Surfpool overrides that walk Phoenix's oracle and book to each level in
//! steps Phoenix accepts.

use std::path::Path;

use mth_botkit::mandate::{InstrumentRef, Kind, Mandate};
use serde::{Deserialize, Serialize};
use solana_pubkey::Pubkey;

use crate::{
    Error, Result,
    chain::Override,
    error::io_error,
    making::Making,
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
    /// What the bot may trade and the limits it must stay within: the
    /// scenario's market, with a position cap and a drawdown limit (see
    /// `specs/mandate.md`). Without a cap, PnL in dollars rewards size as
    /// much as skill: a bot betting several times the reference's size beats
    /// it on any profitable family. The reference trades up to the cap, so
    /// the reward compares strategies at equal size. The drawdown limit is a
    /// prop desk's loss limit: PnL alone pays a bot that goes deep underwater
    /// and gets lucky; the limit makes surviving the path part of the score.
    pub mandate: Mandate,
    pub path: Vec<Waypoint>,
    /// For a market-making scenario: the competing makers and the taker
    /// flow the grader sends. Without it, nobody trades but the bot.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub making: Option<Making>,
    /// PnL the reference bot makes on this scenario; a score of 1 matches it.
    /// Written by `mth calibrate`.
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

/// The venue every scenario trades on.
pub const VENUE: &str = "phoenix";

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

    /// The market the scenario moves, as the mandate names it.
    pub fn instrument(&self) -> InstrumentRef {
        InstrumentRef {
            venue: VENUE.into(),
            kind: Kind::Perp,
            symbol: self.symbol.clone(),
        }
    }

    /// The mandate's position cap on the scenario's market.
    pub fn max_position(&self) -> f64 {
        self.mandate
            .allowance(&self.instrument())
            .map_or(0.0, |a| a.max_position)
    }

    /// The mandate must be valid and cover exactly the scenario's market: a
    /// session measures only that one, so a wider universe would go
    /// unenforced. Waypoints must be in order, each move must finish before
    /// the next one starts, and the last must finish before the session ends.
    pub fn validate(&self) -> std::result::Result<(), String> {
        self.mandate
            .validate()
            .map_err(|reason| format!("mandate: {reason}"))?;
        if self.mandate.universe.len() != 1 || self.mandate.allowance(&self.instrument()).is_none()
        {
            return Err(format!(
                "mandate: the universe must be exactly {}, the scenario's market",
                self.instrument()
            ));
        }
        if self.mandate.term.is_some() {
            return Err("mandate: a scenario's mandate has no term; the session sets it".into());
        }
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
        if let Some(making) = &self.making {
            making
                .validate(self.duration_slots)
                .map_err(|reason| format!("making: {reason}"))?;
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
        self.max_position() * start_mark_usd * self.total_variation_pct() / 100.0
    }

    /// What the reward divides PnL by: the hindsight bound, plus, for a
    /// market-making scenario, the spread bound (see
    /// [`Making::spread_bound_usd`]). A maker can earn both: the spread on
    /// every taker order, and the moves it trades or leans into, so a bound
    /// on the spread alone is one a bot can pass.
    pub fn bound_usd(&self, start_mark_usd: f64) -> f64 {
        let spread = self.making.as_ref().map_or(0.0, |making| {
            making.spread_bound_usd(&self.path, start_mark_usd)
        });
        spread + self.hindsight_bound_usd(start_mark_usd)
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

/// Parameters for a family of shock scenarios: a crash or a squeeze in one
/// or more legs, then its aftermath.
///
/// The path is built so that its shape gives nothing away beyond what a
/// trader would see in a real market, and so that what it does give away
/// doesn't pay for certain (the exploit checks in `bench/checks/exploits`
/// hold it to that):
/// - Leg sizes are log-uniform over a wide range, and most legs land at
///   once, so a move's first print says little about how far it goes and
///   leaves little to ride. The rest ramp in unequal steps.
/// - Quiet stretches carry fizzles: moves that start like a leg and give it
///   all back, so not every move away is a shock.
/// - Each leg is drawn independently and another follows with fixed odds, so
///   a leg's size says nothing about whether another is coming; pauses and
///   the hold before the aftermath share one range.
/// - The session's length is drawn before the path and independently of it.
///   The path is cut where the session ends, so the end slot dates nothing:
///   a session may end mid-shock.
/// - Between moves the mark wobbles, so quiet stretches aren't flat lines.
#[derive(Debug, Clone)]
pub struct ShockFamily {
    pub symbol: String,
    /// Range of the session's length, in slots. Drawn before the path.
    pub duration: (u64, u64),
    /// Range of the slot the shock starts at.
    pub shock_slot: (u64, u64),
    /// Range of each leg's size, in percent of the starting mark. Sizes are
    /// log-uniform over it: small legs are as common as large ones.
    pub leg_pct: (f64, f64),
    /// Odds a leg or the aftermath lands at once, as one move; the rest ramp.
    pub gap_odds: f64,
    /// Odds that another leg follows a leg, up to `max_legs`.
    pub extra_leg_odds: f64,
    pub max_legs: u32,
    /// Range of how many slots the market holds still after a leg, before
    /// the next leg or the aftermath.
    pub pause_slots: (u64, u64),
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
    /// Largest single step of a ramp, in percent of the starting mark. Phoenix
    /// moves at most 4% of the current price per step; 3% of the start stays
    /// under that down to a 25% drop.
    pub max_step_pct: f64,
    /// Range of slots between a ramp's steps.
    pub step_gap: (u64, u64),
    /// Odds a quiet stretch carries a fizzle, its size in percent (either
    /// way), and how many slots it holds before giving it all back.
    pub fizzle_odds: f64,
    pub fizzle_pct: (f64, f64),
    pub fizzle_hold: (u64, u64),
    /// Largest change of the wobble per tick, in percent of the starting mark.
    pub noise_pct: f64,
    /// Range of slots between wobble ticks.
    pub noise_gap: (u64, u64),
    /// The position cap, in base units, every scenario in the family carries.
    pub max_position: f64,
    /// The drawdown limit, in USD, every scenario in the family carries.
    pub max_drawdown_usd: f64,
}

/// What a draw from a [`ShockFamily`] is, beneath its path.
#[derive(Debug, Clone, PartialEq)]
pub struct Shape {
    pub direction: Direction,
    pub aftermath: Aftermath,
    pub legs: u32,
    /// Size of the first leg, in percent of the starting mark.
    pub first_leg_pct: f64,
    /// Level of the shock's extreme, before the aftermath, in percent.
    pub extreme_pct: f64,
    /// Level the aftermath ends at, in percent.
    pub end_pct: f64,
    /// Slots from the last waypoint to the session's end.
    pub tail_slots: u64,
    /// Whether the session ended before the aftermath had landed.
    pub cut: bool,
}

impl ShockFamily {
    pub fn spcx() -> Self {
        Self {
            symbol: "SPCX".into(),
            duration: (320, 720),
            shock_slot: (40, 200),
            leg_pct: (0.6, 8.0),
            gap_odds: 0.7,
            extra_leg_odds: 0.35,
            max_legs: 3,
            pause_slots: (30, 140),
            crash_odds: 0.5,
            revert_odds: 0.65,
            stall_odds: 0.2,
            revert_recovery: (0.5, 1.0),
            stall_recovery: (0.0, 0.2),
            extend_pct: (1.0, 5.0),
            max_step_pct: 3.0,
            step_gap: (3, 6),
            fizzle_odds: 0.5,
            fizzle_pct: (0.4, 2.0),
            fizzle_hold: (6, 40),
            noise_pct: 0.12,
            noise_gap: (4, 12),
            // ~1x the 10,000 USDC of collateral at the snapshot's ~$143 mark.
            max_position: 70.0,
            // 10% of the collateral: the reference's worst path (both
            // tranches on, then stopped out at 13.5%) stays inside it; a
            // blind full-size bet caught by a deep shock breaks it.
            max_drawdown_usd: 1_000.0,
        }
    }

    /// A scenario drawn from the family. The same seed always yields the same
    /// scenario.
    pub fn sample(&self, name: &str, seed: u64) -> Scenario {
        self.draw(name, seed).0
    }

    /// A scenario and the shape it was drawn from.
    pub fn draw(&self, name: &str, seed: u64) -> (Scenario, Shape) {
        let mut rng = SplitMix64(seed);
        // Direction and aftermath come first, so `kind` can read them alone.
        let direction = self.pick_direction(rng.next_f64());
        let aftermath = self.pick_aftermath(rng.next_f64());
        let sign = direction.sign();
        // The session's length comes before the path, so the path can't
        // date it.
        let duration_slots = rng.range_u64(self.duration);
        let shock_slot = rng.range_u64(self.shock_slot);
        let mut legs = 1;
        while legs < self.max_legs && rng.next_f64() < self.extra_leg_odds {
            legs += 1;
        }

        let mut path = PathBuilder::new(self, rng);
        path.quiet(0.0, shock_slot);
        let mut base = 0.0;
        let mut first_leg_pct = 0.0;
        for leg in 0..legs {
            let size = path.rng.log_range_f64(self.leg_pct);
            if leg == 0 {
                first_leg_pct = size;
            }
            base = round(base + sign * size);
            path.travel(base);
            let pause = path.rng.range_u64(self.pause_slots);
            path.quiet(base, path.slot + pause);
        }
        let extreme = base;
        let end = match aftermath {
            Aftermath::Revert => round(extreme * (1.0 - path.rng.range_f64(self.revert_recovery))),
            Aftermath::Stall => round(extreme * (1.0 - path.rng.range_f64(self.stall_recovery))),
            Aftermath::Extend => round(extreme + sign * path.rng.range_f64(self.extend_pct)),
        };
        path.travel(end);
        let aftermath_landed = path.slot;
        path.quiet(end, duration_slots);
        let cut = aftermath_landed > duration_slots;
        path.cut(duration_slots);
        let tail = duration_slots - path.waypoints.last().map_or(0, |w| w.slot);

        let depth = extreme.abs();
        let legs_text = if legs == 1 {
            "at once".to_string()
        } else {
            format!("in {legs} legs")
        };
        let (moves, then) = match (direction, aftermath) {
            (Direction::Crash, Aftermath::Revert) => ("drops", "rebounds"),
            (Direction::Crash, Aftermath::Stall) => ("drops", "stalls"),
            (Direction::Crash, Aftermath::Extend) => ("drops", "slides further"),
            (Direction::Squeeze, Aftermath::Revert) => ("spikes", "fades"),
            (Direction::Squeeze, Aftermath::Stall) => ("spikes", "stalls"),
            (Direction::Squeeze, Aftermath::Extend) => ("spikes", "keeps running"),
        };
        let scenario = Scenario {
            name: name.to_string(),
            description: if cut {
                format!(
                    "{} {moves} {depth:.1}% {legs_text} from around slot {shock_slot}; the session ends before it {then}",
                    self.symbol
                )
            } else {
                format!(
                    "{} {moves} {depth:.1}% {legs_text} from around slot {shock_slot}, then {then} to {end:+.2}% of its starting mark",
                    self.symbol
                )
            },
            symbol: self.symbol.clone(),
            duration_slots,
            mandate: Mandate::perp(
                VENUE,
                &self.symbol,
                self.max_position,
                self.max_drawdown_usd,
            ),
            path: path.waypoints,
            making: None,
            reference_pnl_usd: None,
        };
        let shape = Shape {
            direction,
            aftermath,
            legs,
            first_leg_pct,
            extreme_pct: extreme,
            end_pct: end,
            tail_slots: tail,
            cut,
        };
        (scenario, shape)
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
    /// scenario: the first two draws of [`Self::draw`].
    pub fn kind(&self, seed: u64) -> (Direction, Aftermath) {
        let mut rng = SplitMix64(seed);
        let direction = self.pick_direction(rng.next_f64());
        (direction, self.pick_aftermath(rng.next_f64()))
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

fn round(level_pct: f64) -> f64 {
    (level_pct * 1000.0).round() / 1000.0
}

/// Builds a family's waypoints in slot order. Each waypoint is one step
/// Phoenix accepts in a single move, so the path compiles to exactly the
/// steps drawn here.
struct PathBuilder<'a> {
    family: &'a ShockFamily,
    rng: SplitMix64,
    waypoints: Vec<Waypoint>,
    /// Current level, wobble included.
    level: f64,
    /// First slot free for the next waypoint.
    slot: u64,
}

impl<'a> PathBuilder<'a> {
    fn new(family: &'a ShockFamily, rng: SplitMix64) -> Self {
        Self {
            family,
            rng,
            waypoints: Vec::new(),
            level: 0.0,
            slot: 0,
        }
    }

    fn push(&mut self, level_pct: f64, gap: u64) {
        self.waypoints.push(Waypoint {
            slot: self.slot,
            level_pct,
        });
        self.level = level_pct;
        self.slot += gap;
    }

    /// Go to `to`: at once with the family's gap odds, else in a ramp.
    fn travel(&mut self, to: f64) {
        if self.rng.next_f64() < self.family.gap_odds {
            self.jump(to);
        } else {
            self.ramp(to);
        }
    }

    /// Go to `to` in one move: Phoenix steps it as fast as it can.
    fn jump(&mut self, to: f64) {
        let slots = move_slots(self.level, to) + 1;
        self.push(to, slots);
    }

    /// Walk to `to` in unequal steps at uneven intervals.
    fn ramp(&mut self, to: f64) {
        let total = to - self.level;
        // Enough steps that random weights rarely need redrawing.
        let min_steps = (total.abs() / (0.6 * self.family.max_step_pct))
            .ceil()
            .max(1.0) as u64;
        let steps = min_steps + self.rng.range_u64((0, 2));
        // Random weights, redrawn until no step is too large.
        let weights = loop {
            let w: Vec<f64> = (0..steps).map(|_| 0.25 + self.rng.next_f64()).collect();
            let sum: f64 = w.iter().sum();
            let w: Vec<f64> = w.iter().map(|x| x / sum).collect();
            if w.iter()
                .all(|x| (x * total).abs() <= self.family.max_step_pct)
            {
                break w;
            }
        };
        let from = self.level;
        let mut done = 0.0;
        for (i, weight) in weights.iter().enumerate() {
            done += weight;
            let level = if i + 1 == weights.len() {
                to
            } else {
                round(from + total * done)
            };
            let gap = self.rng.range_u64(self.family.step_gap);
            self.push(level, gap);
        }
    }

    /// Wobble around `base` until `until`, leaving room for a move there.
    /// With the family's fizzle odds, somewhere in the stretch the market
    /// moves away as a leg would, holds, and gives it all back.
    fn quiet(&mut self, base: f64, until: u64) {
        let family = self.family;
        let fizzle = if self.rng.next_f64() < family.fizzle_odds {
            let sign = if self.rng.next_f64() < 0.5 { -1.0 } else { 1.0 };
            let size = sign * self.rng.range_f64(family.fizzle_pct);
            let hold = self.rng.range_u64(family.fizzle_hold);
            // Room for the move out, the hold and the move back.
            let needs = 2 * move_slots(0.0, family.fizzle_pct.1) + hold + 12;
            let room = until.saturating_sub(self.slot);
            (room > needs + 8).then(|| {
                let at = self.slot + 4 + self.rng.range_u64((0, room - needs - 8));
                (at, size, hold)
            })
        } else {
            None
        };
        if let Some((at, size, hold)) = fizzle {
            self.wobble(base, at);
            let away = round(base + size);
            self.travel(away);
            self.wobble(away, self.slot + hold);
            self.travel(base);
        }
        self.wobble(base, until);
    }

    fn wobble(&mut self, base: f64, until: u64) {
        let mut offset = self.level - base;
        loop {
            let gap = self.rng.range_u64(self.family.noise_gap);
            if self.slot + gap + 3 > until {
                break;
            }
            self.slot += gap;
            let step = self.family.noise_pct * (2.0 * self.rng.next_f64() - 1.0);
            offset = 0.5 * offset + step;
            self.push(round(base + offset), 0);
        }
        self.slot = self.slot.max(until);
    }

    /// End the path at `duration`: drop every waypoint whose move wouldn't
    /// finish by then.
    fn cut(&mut self, duration: u64) {
        let mut level = 0.0;
        let mut keep = 0;
        for (i, w) in self.waypoints.iter().enumerate() {
            if w.slot + move_slots(level, w.level_pct) + 1 > duration {
                break;
            }
            level = w.level_pct;
            keep = i + 1;
        }
        self.waypoints.truncate(keep);
    }
}

/// Small, dependency-free, and stable across platforms and releases, which a
/// generator for checked-in task data needs more than statistical quality.
pub(crate) struct SplitMix64(pub(crate) u64);

impl SplitMix64 {
    pub(crate) fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    pub(crate) fn next_f64(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }

    pub(crate) fn range_f64(&mut self, (lo, hi): (f64, f64)) -> f64 {
        lo + (hi - lo) * self.next_f64()
    }

    /// Log-uniform over `[lo, hi]`, both positive.
    pub(crate) fn log_range_f64(&mut self, (lo, hi): (f64, f64)) -> f64 {
        (lo.ln() + (hi.ln() - lo.ln()) * self.next_f64()).exp()
    }

    pub(crate) fn range_u64(&mut self, (lo, hi): (u64, u64)) -> u64 {
        lo + self.next_u64() % (hi - lo + 1)
    }

    /// A standard normal draw (Box-Muller).
    pub(crate) fn normal(&mut self) -> f64 {
        let u = self.next_f64().max(f64::MIN_POSITIVE);
        let v = self.next_f64();
        (-2.0 * u.ln()).sqrt() * (2.0 * std::f64::consts::PI * v).cos()
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
        // A fizzle can take the mark past the shock's levels, and back.
        let slack = family.fizzle_pct.1 + 2.0 * family.noise_pct;
        for seed in 0..200 {
            let (scenario, shape) = family.draw("s", seed);
            assert_eq!((shape.direction, shape.aftermath), family.kind(seed));
            let sign = shape.direction.sign();
            let extreme = sign * shape.extreme_pct;
            let end = sign * shape.end_pct;
            assert!(extreme >= family.leg_pct.0 - 0.01, "seed {seed}: {shape:?}");
            match shape.aftermath {
                Aftermath::Extend => assert!(end > extreme, "seed {seed}"),
                _ => assert!(end <= extreme && end >= 0.0, "seed {seed}"),
            }
            if shape.cut {
                continue;
            }
            let levels: Vec<f64> = scenario.path.iter().map(|w| w.level_pct * sign).collect();
            let peak = levels.iter().cloned().fold(f64::MIN, f64::max);
            assert!(peak < extreme.max(end) + slack, "seed {seed}");
            assert!((levels.last().unwrap() - end).abs() < slack, "seed {seed}");
        }
    }

    /// Most moves land at once, so a move's first print leaves little to
    /// ride; the rest ramp in unequal steps, none larger than Phoenix takes.
    #[test]
    fn most_moves_land_at_once() {
        let family = ShockFamily::spcx();
        let (mut at_once, mut uneven, mut ramps) = (0, 0, 0);
        for seed in 0..400 {
            let mut path = PathBuilder::new(&family, SplitMix64(seed));
            path.travel(5.0);
            let steps: Vec<f64> = std::iter::once(0.0)
                .chain(path.waypoints.iter().map(|w| w.level_pct))
                .collect::<Vec<_>>()
                .windows(2)
                .map(|w| w[1] - w[0])
                .collect();
            if steps.len() == 1 {
                at_once += 1;
                continue;
            }
            ramps += 1;
            assert!(
                steps.iter().all(|d| *d <= family.max_step_pct + 1e-9),
                "seed {seed}"
            );
            let (lo, hi) = steps
                .iter()
                .fold((f64::MAX, f64::MIN), |(lo, hi), d| (lo.min(*d), hi.max(*d)));
            if hi - lo > 0.2 {
                uneven += 1;
            }
        }
        let share = at_once as f64 / 400.0;
        assert!(
            (share - family.gap_odds).abs() < 0.07,
            "{share} land at once"
        );
        assert!(
            uneven * 4 > ramps * 3,
            "only {uneven} of {ramps} ramps are uneven"
        );
    }

    /// Quiet stretches carry fizzles: a move away that comes all the way back.
    #[test]
    fn fizzles_come_all_the_way_back() {
        let family = ShockFamily::spcx();
        let mut fizzled = 0;
        for seed in 0..200 {
            let mut path = PathBuilder::new(&family, SplitMix64(seed));
            path.quiet(0.0, 300);
            let away = path
                .waypoints
                .iter()
                .any(|w| w.level_pct.abs() >= family.fizzle_pct.0 - 1e-9);
            if away {
                fizzled += 1;
                // The last move back lands on the base, before the wobble.
                assert!(
                    path.waypoints
                        .iter()
                        .all(|w| w.level_pct.abs() <= family.fizzle_pct.1 + 1.0),
                    "seed {seed}"
                );
                assert!(path.waypoints.last().unwrap().level_pct.abs() < 2.0 * family.noise_pct);
            }
        }
        let share = fizzled as f64 / 200.0;
        assert!(
            (share - family.fizzle_odds).abs() < 0.1,
            "{share} of stretches fizzle"
        );
    }

    /// The session's length is drawn before the path: how long a session
    /// runs says nothing about its shock, and some end before the aftermath.
    #[test]
    fn the_end_dates_nothing() {
        let family = ShockFamily::spcx();
        let draws: Vec<(Scenario, Shape)> = (0..600).map(|seed| family.draw("s", seed)).collect();
        let mean = |legs: u32| {
            let d: Vec<f64> = draws
                .iter()
                .filter(|(_, s)| s.legs == legs)
                .map(|(sc, _)| sc.duration_slots as f64)
                .collect();
            d.iter().sum::<f64>() / d.len() as f64
        };
        assert!(
            (mean(1) - mean(3)).abs() / mean(1) < 0.08,
            "{} vs {}",
            mean(1),
            mean(3)
        );
        let cut = draws.iter().filter(|(_, s)| s.cut).count();
        assert!(cut > 0 && cut < 120, "{cut} of 600 sessions end mid-shock");
    }

    /// A shock's legs are drawn independently: how far the first leg went
    /// says nothing about whether another follows.
    #[test]
    fn a_legs_size_says_nothing_about_the_next() {
        let family = ShockFamily::spcx();
        let mid = (family.leg_pct.0 + family.leg_pct.1) / 2.0;
        let (mut small, mut small_more, mut large, mut large_more) = (0, 0, 0, 0);
        for seed in 0..4_000 {
            let (_, shape) = family.draw("s", seed);
            let first = shape.first_leg_pct;
            if first < mid {
                small += 1;
                small_more += (shape.legs > 1) as u32;
            } else {
                large += 1;
                large_more += (shape.legs > 1) as u32;
            }
        }
        let share = |more: u32, n: u32| more as f64 / n as f64;
        assert!((share(small_more, small) - share(large_more, large)).abs() < 0.05);
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
            mandate: Mandate::perp(VENUE, "SPCX", 70.0, 1_000.0),
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
            making: None,
            reference_pnl_usd: None,
        };
        assert!(scenario.validate().unwrap_err().contains("previous move"));

        let elsewhere = Scenario {
            mandate: Mandate::perp(VENUE, "SOL", 70.0, 1_000.0),
            path: vec![],
            ..scenario.clone()
        };
        assert!(
            elsewhere
                .validate()
                .unwrap_err()
                .contains("phoenix:perp:SPCX")
        );

        let mut wide = Scenario {
            path: vec![],
            ..scenario
        };
        wide.mandate
            .universe
            .extend(Mandate::perp(VENUE, "SOL", 1.0, 1.0).universe);
        assert!(wide.validate().unwrap_err().contains("exactly"));
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
