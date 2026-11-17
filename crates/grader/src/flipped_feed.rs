//! The flipped-feed family: NVDA's opening half hour, traded off a house
//! model's calls, one of whose two gateways reports them the wrong way
//! round.
//!
//! - **The price** walks NVDA's market calibration from the 09:30 New York
//!   open for 30 minutes (7,200 of its 0.25 s slots), at the opening
//!   window's stated volatility (`vol_mult` 2.4, the opening 1 m sd net of
//!   jumps over the calibration's) and jumps: 2.75 an hour from 09:31 to
//!   10:30, median 0.305% and p90 0.533% (`OPEN_*`). Sessions come in
//!   mirrored pairs: the second of a pair walks the first's path upside
//!   down.
//! - **The feed** (the library's [`Feed`](crate::actors::library::Feed)):
//!   the house model sees the level `horizon_slots` ahead and calls its way
//!   while it is more than `edge_bps` off, right for a whole horizon at a
//!   time with chance `hit_rate`. Each call goes out as a memo from one of
//!   two gateway wallets, drawn per call. One gateway reports the model's
//!   side; the other, after an upstream change, the side a maker would be
//!   filled on: the opposite. Which one is drawn per session.
//! - **The flow.** Noise takes at random, chasers take the way the book
//!   moved, and a refill puts back the depth. No one trades on the model,
//!   so the tape can't confirm a call.
//!
//! No tells: every session seats the same actors and both gateways from
//! fresh wallets of the session's keyring, and calls through either carry
//! the same horizon, pace and confidence. Which gateway reports which side
//! is the session's latent identity, told to the informed bot as wallets
//! (`crate::latent`); never a call's outcome, the path or when a call
//! comes.
//!
//! The flipped gateway is a stated stylisation: a real desk meets a feed
//! whose convention changed upstream now and then, not every session.

use std::collections::BTreeMap;

use mtx_core::mandate::Mandate;
use serde::Serialize;
use serde_json::json;

use crate::{
    actors::{
        Library,
        library::{MAKER_SIDE, TAKER_SIDE},
    },
    market::{
        Calibration, MarketModel, ModelSpec,
        calibration::sha256_hex,
        model::{JumpSize, ModelPath, Start},
    },
    orders::CloseOut,
    scenario::{Scenario, SplitMix64, VENUE},
};

/// NVDA's market calibration, built in so hidden sets need no file.
const NVDA_CALIBRATION: &str =
    include_str!("../../../calibration/market/nvda.20261113.903d1120.toml");

/// NVDA's opening window (09:31 to 10:30 New York), measured on the
/// calibration's own Phoenix 1 m mark candles (2026-06-10 to 2026-11-12, all
/// days) with its jump rule (5 robust sigmas of the trailing hour).
pub const OPEN_JUMPS_PER_HOUR: f64 = 2.75;
pub const OPEN_JUMP_MEDIAN_PCT: f64 = 0.305;
pub const OPEN_JUMP_P90_PCT: f64 = 0.533;
/// The opening window's 1 m sd with the jumps' variance taken out (about
/// 15.7 bp, 09:31 to 10:00) over the calibration's (6.4 bp).
pub const OPEN_VOL_MULT: f64 = 2.4;
const OPEN_SOURCE: &str = "NVDA Phoenix 1 m mark, 09:31-10:30 New York, 2026-06-10 to 2026-11-12, the calibration's 5-robust-sigma jump rule (JUMP_REGIME_MEASUREMENT.md, table 2)";

/// A session's draws beneath its path, feed and tape: what the ladder's
/// oracles may be told. The informed bot is told only which gateway is
/// which.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Shape {
    /// Whether the session walks its pair's path upside down.
    pub mirrored: bool,
    pub horizon_slots: u64,
    pub edge_bps: f64,
    pub calls_per_minute: f64,
    pub noise: u32,
    pub chasers: u32,
}

/// The family's parameters. Every range is inclusive.
#[derive(Debug, Clone)]
pub struct FlippedFeedFamily {
    pub symbol: String,
    pub arrival_usd: f64,
    pub calibration: &'static str,
    pub duration: u64,
    pub waypoint_slots: u64,
    pub trades_per_minute: f64,
    pub trade_size: f64,
    pub trade_size_sigma: f64,
    pub size_mult: f64,
    pub limit_bps: f64,
    pub noise: (u64, u64),
    pub noise_mult: f64,
    pub chasers: (u64, u64),
    pub chaser_mult: f64,
    pub chaser_lookback: u64,
    pub trigger_bps: (f64, f64),
    pub refill_half_life: (f64, f64),
    /// The model's horizon, in slots, and its edge, in basis points.
    pub horizon_slots: (u64, u64),
    pub edge_bps: (f64, f64),
    pub hit_rate: f64,
    /// Calls a minute while the move ahead is past the edge, and the least
    /// gap between two calls, in slots.
    pub calls_per_minute: (f64, f64),
    pub min_gap_slots: u64,
    pub max_position: f64,
    pub max_drawdown_usd: f64,
    pub close_haircut_bps: f64,
}

impl FlippedFeedFamily {
    /// NVDA perps, `flipped-feed-nvda`.
    pub fn nvda() -> Self {
        Self {
            symbol: "NVDA".into(),
            arrival_usd: 159.92,
            calibration: NVDA_CALIBRATION,
            duration: 7_200,
            waypoint_slots: 3,
            trades_per_minute: 1.24,
            trade_size: 0.015,
            trade_size_sigma: 1.04,
            size_mult: 40.0,
            limit_bps: 50.0,
            noise: (3, 5),
            noise_mult: 2.0,
            chasers: (1, 2),
            chaser_mult: 2.0,
            chaser_lookback: 40,
            trigger_bps: (8.0, 15.0),
            refill_half_life: (4.0, 40.0),
            horizon_slots: (96, 240),
            edge_bps: (20.0, 30.0),
            hit_rate: 0.8,
            calls_per_minute: (5.0, 7.0),
            min_gap_slots: 16,
            max_position: 4.0,
            max_drawdown_usd: 150.0,
            close_haircut_bps: 5.0,
        }
    }

    /// The price's model: the calibration from the open, at the opening
    /// window's stated volatility and jumps.
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

    pub fn draw(&self, name: &str, seed: u64) -> (Scenario, Shape) {
        self.draw_with(&self.model(), name, seed, false)
    }

    fn draw_with(
        &self,
        model: &MarketModel,
        name: &str,
        seed: u64,
        mirrored: bool,
    ) -> (Scenario, Shape) {
        let mut rng = SplitMix64(seed);
        let mut index = model.draw(rng.next_u64(), self.duration);
        if mirrored {
            mirror(&mut index);
        }
        let horizon_slots = rng.range_u64(self.horizon_slots);
        let edge_bps = round2(rng.range_f64(self.edge_bps));
        let calls_per_minute = round2(rng.range_f64(self.calls_per_minute));
        let noise = rng.range_u64(self.noise) as u32;
        let chasers = rng.range_u64(self.chasers) as u32;
        let trigger_bps = round2(rng.range_f64(self.trigger_bps));
        let half_life = round2(rng.range_f64(self.refill_half_life));

        let actors: Library = serde_json::from_value(json!({
            "scale": {
                "trades_per_minute": self.trades_per_minute,
                "trade_size": self.trade_size,
                "trade_size_sigma": self.trade_size_sigma,
            },
            "noise": [{
                "wallets": noise,
                "flow_mult": self.noise_mult * f64::from(noise),
                "size_mult": self.size_mult,
                "limit_bps": self.limit_bps,
            }],
            "chasers": [{
                "wallets": chasers,
                "lookback_slots": self.chaser_lookback,
                "trigger_bps": trigger_bps,
                "intensity_mult": self.chaser_mult * f64::from(chasers),
                "size_mult": self.size_mult,
                "limit_bps": self.limit_bps,
            }],
            "refill": { "half_life_slots": [half_life, half_life], "levels": 5 },
            "feed": {
                "horizon_slots": horizon_slots,
                "edge_bps": edge_bps,
                "hit_rate": self.hit_rate,
                "calls_per_minute": calls_per_minute,
                "min_gap_slots": self.min_gap_slots,
                "active": { "from": 0, "until": self.duration.saturating_sub(horizon_slots) },
            },
        }))
        .expect("the family's actors are a valid library");

        let mandate = Mandate::perp(
            VENUE,
            &self.symbol,
            self.max_position,
            self.max_drawdown_usd,
        );
        let record = index.record.clone();
        let path = index.waypoints(self.waypoint_slots);
        let shape = Shape {
            mirrored,
            horizon_slots,
            edge_bps,
            calls_per_minute,
            noise,
            chasers,
        };
        let description = format!(
            "{}: trade the perp off the house model's calls at the open, within {} {} and a ${} loss limit",
            self.symbol, self.max_position, self.symbol, self.max_drawdown_usd
        );
        let scenario = Scenario {
            name: name.to_string(),
            description,
            symbol: self.symbol.clone(),
            duration_slots: self.duration,
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
            market_model: Some(record),
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
            stop_out: None,
            liquidator: None,
        };
        (scenario, shape)
    }

    /// `count` sessions named `<prefix>-<n>`, in mirrored pairs: the second
    /// of each pair walks the first's path upside down (its feed and flow
    /// are drawn afresh in the session).
    pub fn draw_set(&self, prefix: &str, count: usize, seed: u64) -> Vec<(Scenario, Shape)> {
        let model = self.model();
        (0..count as u64)
            .map(|i| {
                let draw_seed = seed.wrapping_mul(1_000_003).wrapping_add(i / 2 + 1);
                let name = format!("{prefix}-{}", i + 1);
                self.draw_with(&model, &name, draw_seed, i % 2 == 1)
            })
            .collect()
    }
}

/// The latent state every session has: who the two gateways are.
pub fn latent() -> serde_json::Value {
    json!({ "roster": [TAKER_SIDE, MAKER_SIDE] })
}

/// Turns a path upside down about its start, its drawn moves with it.
fn mirror(path: &mut ModelPath) {
    for level in &mut path.log_levels {
        *level = -*level;
    }
    for event in &mut path.record.events {
        event.pct = -event.pct;
    }
}

fn round2(x: f64) -> f64 {
    (x * 100.0).round() / 100.0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::making::level_at;

    #[test]
    fn sessions_are_valid_deterministic_and_mirrored_in_pairs() {
        let family = FlippedFeedFamily::nvda();
        let a = family.draw_set("t", 6, 9);
        let b = family.draw_set("t", 6, 9);
        assert_eq!(
            serde_json::to_string(&a.iter().map(|(s, _)| s).collect::<Vec<_>>()).unwrap(),
            serde_json::to_string(&b.iter().map(|(s, _)| s).collect::<Vec<_>>()).unwrap()
        );
        for (scenario, _) in &a {
            scenario.validate().unwrap();
            let actors = scenario.actors.as_ref().unwrap();
            actors.validate().unwrap();
            assert!(actors.feed.is_some());
            assert!(actors.informed.is_empty() && actors.decoys.is_empty());
        }
        for pair in a.chunks(2) {
            let (up, down) = (&pair[0].0, &pair[1].0);
            assert!(pair[1].1.mirrored && !pair[0].1.mirrored);
            for slot in [600, 3_000, 6_000] {
                let (u, d) = (level_at(&up.path, slot), level_at(&down.path, slot));
                let log = |pct: f64| (1.0 + pct / 100.0).ln();
                assert!((log(u) + log(d)).abs() < 1e-4, "{slot}: {u} {d}");
            }
        }
    }

    /// How often the model calls, on the family's paths, with the engine's
    /// pacing: what `calls_per_minute` is tuned to (12 to 15 a session).
    #[test]
    fn the_model_calls_a_dozen_times_a_session() {
        let family = FlippedFeedFamily::nvda();
        let set = family.draw_set("t", 200, 5);
        let mut calls = Vec::new();
        let mut rng = SplitMix64(17);
        for (scenario, shape) in &set {
            let level = |slot: u64| 1.0 + level_at(&scenario.path, slot) / 100.0;
            let mut last_call: Option<u64> = None;
            let mut n = 0u32;
            let until = scenario.duration_slots - shape.horizon_slots;
            let per_step =
                shape.calls_per_minute * crate::scenario::SLOT_SECONDS / 60.0 * 2.0;
            let mut at = 0;
            while at < until {
                let ahead = (level(at + shape.horizon_slots) / level(at) - 1.0) * 10_000.0;
                let spaced = last_call.is_none_or(|c| at >= c + family.min_gap_slots);
                if ahead.abs() > shape.edge_bps
                    && spaced
                    && rng.next_f64() < 1.0 - (-per_step).exp()
                {
                    n += 1;
                    last_call = Some(at);
                }
                at += 2;
            }
            calls.push(f64::from(n));
        }
        let mean = calls.iter().sum::<f64>() / calls.len() as f64;
        let none = calls.iter().filter(|c| **c == 0.0).count();
        eprintln!("calls a session: mean {mean:.1}, none in {none} of {}", calls.len());
        assert!(mean > 4.0, "{mean}");
    }
}
