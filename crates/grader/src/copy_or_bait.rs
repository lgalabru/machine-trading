//! The copy-or-bait family: NVDA's opening minutes, where a fund trades its
//! views through a wallet the whole market knows, and on some days turns
//! that wallet into bait once it sees one account copy it.
//!
//! - **The price** walks NVDA's market calibration from the 09:30 New York
//!   open for 4,500 of its 0.25 s slots, at the opening window's volatility
//!   (batch 5's template, `vol_mult` 2.4) and measured jumps: 2.75 an hour
//!   from 09:31 to 10:30, median 0.305% and p90 0.533%. Sessions come in
//!   mirrored pairs: the second is the first's path turned upside down.
//! - **The leader** (the library's [`Informed`](crate::actors::library::Informed),
//!   one wallet, public: every bot is told it) sees the path `horizon_slots`
//!   ahead and takes its way while it is more than `edge_bps` off, right
//!   for a horizon at a time with chance 0.85. It sets [`Bait`] every
//!   session: once one account has followed `matches` of its last eight
//!   runs, its views from then on are right with chance `hit_rate_after`.
//!   On a bait day that is 0.5, a coin flip at the same pace, sizes and
//!   timing; on a genuine day it is 0.85, as before. Half the sessions of a
//!   set are each.
//! - **Decoys** trade as the leader does for the first part of the session,
//!   then on random views of their own; **chasers** take the way the book
//!   moved; **noise** takes at random; **a refill** puts back the depth.
//!   Every role trades the same sizes, so no take's size names its wallet.
//!
//! What a session draws is its [`Shape`]. The informed bot is told the bait
//! rule (how the leader judges following and its runs' length), a process
//! fact the instruction states the ranges of; never the day's kind, which is
//! the session's draw, nor the path, nor when the leader trades.

use std::collections::BTreeMap;

use mtx_core::mandate::Mandate;
use serde::Serialize;
use serde_json::json;

use crate::{
    actors::{Library, library::Bait},
    market::{
        Calibration, MarketModel, ModelSpec,
        calibration::sha256_hex,
        model::{JumpSize, ModelPath, Start},
    },
    orders::CloseOut,
    scenario::{CloseMode, Scenario, SplitMix64, StopOut, StopRule, VENUE},
};

/// NVDA's market calibration, built in so hidden sets need no file.
const NVDA_CALIBRATION: &str =
    include_str!("../../../calibration/market/nvda.20261113.903d1120.toml");

/// NVDA's opening hour (09:31 to 10:30 New York), measured on the
/// calibration's own Phoenix 1 m mark candles (2026-06-10 to 2026-11-12, all
/// days) with its jump rule (5 robust sigmas of the trailing hour).
pub const OPEN_JUMPS_PER_HOUR: f64 = 2.75;
pub const OPEN_JUMP_MEDIAN_PCT: f64 = 0.305;
pub const OPEN_JUMP_P90_PCT: f64 = 0.533;
/// The opening window's volatility multiple, batch 5's template for NVDA.
pub const OPEN_VOL_MULT: f64 = 2.4;
const OPEN_SOURCE: &str = "NVDA Phoenix 1 m mark, 09:31-10:30 New York, 2026-06-10 to 2026-11-12, the calibration's 5-robust-sigma jump rule (JUMP_REGIME_MEASUREMENT.md, table 2)";

/// The day's kind: whether the leader turns its wallet into bait once
/// copied.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Day {
    Genuine,
    Bait,
}

/// A session's draws beneath its path and tape: what the ladder's oracles
/// may be told. The informed bot is told [`Shape::rule`] only.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Shape {
    pub day: Day,
    /// The leader's bait rule as the session's library sets it.
    pub bait: Bait,
    pub horizon_slots: u64,
    pub edge_bps: f64,
    pub decoys: u32,
    pub chasers: u32,
    pub noise: u32,
    pub trigger_bps: f64,
    /// Whether the session's path is its pair's turned upside down.
    pub mirrored: bool,
}

impl Shape {
    /// What the informed bot is told: how the leader judges following, and
    /// how long its runs are. Not the day.
    pub fn rule(&self) -> serde_json::Value {
        json!({
            "follow_slots": self.bait.follow_slots,
            "match_share": self.bait.match_share,
            "matches": self.bait.matches,
            "episodes": self.bait.episodes,
            "ref_fills": self.bait.ref_fills,
            "horizon_slots": self.horizon_slots,
        })
    }
}

/// The family's parameters. Every range is inclusive.
#[derive(Debug, Clone)]
pub struct CopyOrBaitFamily {
    pub symbol: String,
    pub calibration: &'static str,
    pub duration: u64,
    pub waypoint_slots: u64,
    pub trades_per_minute: f64,
    pub trade_size: f64,
    pub trade_size_sigma: f64,
    /// Every actor's size multiple and limit.
    pub size_mult: f64,
    pub limit_bps: f64,
    pub decoys: (u64, u64),
    pub chasers: (u64, u64),
    pub noise: (u64, u64),
    pub horizon_slots: (u64, u64),
    pub edge_bps: (f64, f64),
    pub hit_rate: f64,
    /// The leader's pace while it trades, each decoy's, the chasers'
    /// together, and each noise wallet's, as multiples of the market's. A
    /// decoy trades all session once it has faded, so its pace is about the
    /// leader's over the session, not while it trades.
    pub informed_mult: f64,
    pub decoy_mult: f64,
    pub chaser_mult: f64,
    pub noise_mult: f64,
    /// Where decoys fade, as shares of the session.
    pub fade_share: (f64, f64),
    pub trigger_bps: (f64, f64),
    pub chaser_lookback: u64,
    pub refill_half_life: (f64, f64),
    /// The bait rule's ranges.
    pub follow_slots: (u64, u64),
    pub match_share: (f64, f64),
    pub matches: (u64, u64),
    pub episodes: u32,
    pub ref_fills: u32,
    /// The leader's views once the bait is set, on a bait day.
    pub hit_rate_bait: f64,
    pub max_position: f64,
    pub max_drawdown_usd: f64,
    pub close_haircut_bps: f64,
}

impl CopyOrBaitFamily {
    /// NVDA perps, `copy-or-bait-nvda`.
    pub fn nvda() -> Self {
        Self {
            symbol: "NVDA".into(),
            calibration: NVDA_CALIBRATION,
            duration: 4_500,
            waypoint_slots: 3,
            trades_per_minute: 1.24,
            trade_size: 0.015,
            trade_size_sigma: 1.04,
            size_mult: 40.0,
            limit_bps: 50.0,
            decoys: (1, 2),
            chasers: (2, 2),
            noise: (3, 5),
            horizon_slots: (60, 150),
            edge_bps: (12.0, 18.0),
            hit_rate: 0.85,
            informed_mult: 48.0,
            decoy_mult: 4.0,
            chaser_mult: 4.0,
            noise_mult: 2.0,
            fade_share: (0.2, 0.5),
            trigger_bps: (8.0, 15.0),
            chaser_lookback: 60,
            refill_half_life: (4.0, 40.0),
            follow_slots: (10, 40),
            match_share: (0.6, 1.0),
            matches: (3, 4),
            episodes: 8,
            ref_fills: 3,
            hit_rate_bait: 0.5,
            max_position: 25.0,
            max_drawdown_usd: 400.0,
            close_haircut_bps: 5.0,
        }
    }

    /// The price's model: the calibration from the open, at the opening
    /// window's volatility and jumps.
    pub fn model(&self) -> MarketModel {
        let mut calibration =
            Calibration::parse(self.calibration).expect("the family's calibration is valid");
        calibration.sha256 = sha256_hex(self.calibration.as_bytes());
        MarketModel::new(
            calibration,
            ModelSpec {
                vol_mult: OPEN_VOL_MULT,
                start: Start::Before {
                    kind: "regular".into(),
                    lead_seconds: 0.0,
                },
                jumps: true,
                jump_rate_per_hour: Some(OPEN_JUMPS_PER_HOUR),
                jump_size: Some(JumpSize {
                    abs_median_pct: OPEN_JUMP_MEDIAN_PCT,
                    abs_p90_pct: OPEN_JUMP_P90_PCT,
                    source: OPEN_SOURCE.into(),
                }),
                gaps: false,
                ..ModelSpec::default()
            },
        )
        .expect("the model's spec is valid")
    }

    /// One pair's draws: the path's seed and everything but the side.
    fn draw_pair(&self, rng: &mut SplitMix64, day: Day) -> PairDraw {
        let path_seed = rng.next_u64();
        let horizon_slots = rng.range_u64(self.horizon_slots);
        let edge_bps = round2(rng.range_f64(self.edge_bps));
        let decoys = rng.range_u64(self.decoys) as u32;
        let chasers = rng.range_u64(self.chasers) as u32;
        let noise = rng.range_u64(self.noise) as u32;
        let trigger_bps = round2(rng.range_f64(self.trigger_bps));
        let half_life = round2(rng.range_f64(self.refill_half_life));
        let fade = (
            round2(rng.range_f64(self.fade_share)),
            round2(rng.range_f64(self.fade_share)),
        );
        let bait = Bait {
            follow_slots: rng.range_u64(self.follow_slots),
            match_share: round2(rng.range_f64(self.match_share)),
            matches: rng.range_u64(self.matches) as u32,
            episodes: self.episodes,
            ref_fills: self.ref_fills,
            hit_rate_after: match day {
                Day::Bait => self.hit_rate_bait,
                Day::Genuine => self.hit_rate,
            },
        };
        PairDraw {
            path_seed,
            shape: Shape {
                day,
                bait,
                horizon_slots,
                edge_bps,
                decoys,
                chasers,
                noise,
                trigger_bps,
                mirrored: false,
            },
            half_life,
            fade,
        }
    }

    fn scenario(&self, name: &str, path: &ModelPath, draw: &PairDraw) -> Scenario {
        let shape = &draw.shape;
        let duration = self.duration;
        let size = self.size_mult;
        let limit = self.limit_bps;
        let (lo, hi) = (
            draw.fade.0.min(draw.fade.1),
            draw.fade.0.max(draw.fade.1) + 0.01,
        );
        let decoy = json!({
            "wallets": 1,
            "horizon_slots": shape.horizon_slots,
            "edge_bps": shape.edge_bps,
            "hit_rate": self.hit_rate,
            "informed_for": {
                "from": (lo * duration as f64) as u64,
                "until": (hi * duration as f64) as u64,
            },
            "intensity_mult": self.decoy_mult,
            "size_mult": size,
            "limit_bps": limit,
        });
        let actors: Library = serde_json::from_value(json!({
            "scale": {
                "trades_per_minute": self.trades_per_minute,
                "trade_size": self.trade_size,
                "trade_size_sigma": self.trade_size_sigma,
            },
            "noise": [{
                "wallets": shape.noise,
                "flow_mult": self.noise_mult * f64::from(shape.noise),
                "size_mult": size,
                "limit_bps": limit,
            }],
            "informed": [{
                "wallets": 1,
                "horizon_slots": shape.horizon_slots,
                "edge_bps": shape.edge_bps,
                "hit_rate": self.hit_rate,
                "intensity_mult": self.informed_mult,
                "size_mult": size,
                "limit_bps": limit,
                "bait": shape.bait,
            }],
            "chasers": [{
                "wallets": shape.chasers,
                "lookback_slots": self.chaser_lookback,
                "trigger_bps": shape.trigger_bps,
                "intensity_mult": self.chaser_mult,
                "size_mult": size,
                "limit_bps": limit,
            }],
            "decoys": vec![decoy; shape.decoys as usize],
            "refill": { "half_life_slots": [draw.half_life, draw.half_life], "levels": 5 },
            "public": ["informed"],
        }))
        .expect("the family's actors are a valid library");
        let mandate = Mandate::perp(
            VENUE,
            &self.symbol,
            self.max_position,
            self.max_drawdown_usd,
        );
        let description = format!(
            "{}: copy a public wallet at the open, within {} {} and a ${} loss limit",
            self.symbol, self.max_position, self.symbol, self.max_drawdown_usd
        );
        Scenario {
            name: name.to_string(),
            description,
            symbol: self.symbol.clone(),
            duration_slots: duration,
            mandate,
            path: path.waypoints(self.waypoint_slots),
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
            market_model: Some(path.record.clone()),
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
        }
    }

    /// `count` sessions named `<prefix>-<n>`, in mirrored pairs (an odd
    /// last one alone), half the pairs bait days.
    pub fn draw_set(&self, prefix: &str, count: usize, seed: u64) -> Vec<(Scenario, Shape)> {
        let model = self.model();
        let mut rng = SplitMix64(seed.wrapping_mul(1_000_003) ^ 0x636f_7079_6261_6974);
        let pairs = count.div_ceil(2);
        let mut bait_left = pairs / 2 + (pairs % 2) * usize::from(rng.next_u64() % 2 == 1);
        let mut out = Vec::with_capacity(count);
        for pair in 0..pairs {
            let left = (pairs - pair) as u64;
            let day = if rng.next_u64() % left < bait_left as u64 {
                bait_left -= 1;
                Day::Bait
            } else {
                Day::Genuine
            };
            let draw = self.draw_pair(&mut rng, day);
            let path = model.draw(draw.path_seed, self.duration);
            for mirrored in [false, true] {
                let n = 2 * pair + usize::from(mirrored) + 1;
                if n > count {
                    break;
                }
                let path = if mirrored {
                    mirror(&path)
                } else {
                    path.clone()
                };
                let mut draw = draw.clone();
                draw.shape.mirrored = mirrored;
                let scenario = self.scenario(&format!("{prefix}-{n}"), &path, &draw);
                out.push((scenario, draw.shape));
            }
        }
        out
    }
}

/// One pair's draws.
#[derive(Debug, Clone)]
struct PairDraw {
    path_seed: u64,
    shape: Shape,
    half_life: f64,
    fade: (f64, f64),
}

/// `path` turned upside down: every move the other way, jumps too.
fn mirror(path: &ModelPath) -> ModelPath {
    let mut record = path.record.clone();
    for event in &mut record.events {
        event.pct = -event.pct;
    }
    ModelPath {
        log_levels: path.log_levels.iter().map(|l| -l).collect(),
        record,
    }
}

fn round2(x: f64) -> f64 {
    (x * 100.0).round() / 100.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sessions_are_valid_deterministic_mirrored_and_half_bait() {
        let family = CopyOrBaitFamily::nvda();
        let a = family.draw_set("t", 24, 9);
        let b = family.draw_set("t", 24, 9);
        assert_eq!(
            serde_json::to_string(&a.iter().map(|(s, _)| s).collect::<Vec<_>>()).unwrap(),
            serde_json::to_string(&b.iter().map(|(s, _)| s).collect::<Vec<_>>()).unwrap()
        );
        let baits = a.iter().filter(|(_, s)| s.day == Day::Bait).count();
        assert_eq!(baits, 12);
        for pair in a.chunks(2) {
            let (first, second) = (&pair[0], &pair[1]);
            assert_eq!(first.1.day, second.1.day);
            assert!(!first.1.mirrored && second.1.mirrored);
            // The pair's paths end on opposite sides of the start.
            let end = |s: &Scenario| s.path.last().map_or(0.0, |w| w.level_pct);
            assert!(end(&first.0) * end(&second.0) <= 0.0);
        }
        for (scenario, shape) in &a {
            scenario.validate().unwrap();
            assert_eq!(scenario.valuation(), "oracle");
            let actors = scenario.actors.as_ref().unwrap();
            actors.validate().unwrap();
            assert!(actors.has_bait() && actors.watches_bot());
            assert_eq!(actors.public, ["informed"]);
            assert_eq!(actors.decoys.len() as u32, shape.decoys);
            let bait = actors.informed[0].bait.unwrap();
            assert!((3..=4).contains(&bait.matches));
            assert!((10..=40).contains(&bait.follow_slots));
            assert!((0.6..=1.0).contains(&bait.match_share));
            let after = if shape.day == Day::Bait { 0.5 } else { 0.85 };
            assert_eq!(bait.hit_rate_after, after);
            // Every role trades the same sizes.
            assert!(actors.noise.iter().all(|a| a.size_mult == 40.0));
            assert!(actors.chasers.iter().all(|a| a.size_mult == 40.0));
            assert!(actors.decoys.iter().all(|a| a.size_mult == 40.0));
        }
    }

    #[test]
    fn the_rule_told_holds_no_day() {
        let family = CopyOrBaitFamily::nvda();
        for (_, shape) in family.draw_set("t", 8, 3) {
            let rule = shape.rule().to_string();
            assert!(
                !rule.contains("hit_rate") && !rule.contains("day"),
                "{rule}"
            );
        }
    }
}
