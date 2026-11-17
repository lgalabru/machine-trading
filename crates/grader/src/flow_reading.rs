//! The flow-reading family: a market whose price makes a few sharp moves a
//! session, and whose tape mixes wallets that trade ahead of them with
//! wallets that only look as if they do.
//!
//! - **The price** walks the asset's market calibration inside regular
//!   hours, and makes `moves` sharp moves a session, each a multiple of the
//!   calibration's median jump, up or down with equal odds, over a short ramp
//!   of uneven steps. Between them the calibration's own path runs.
//! - **The informed** (the library's [`Informed`](crate::actors::library::Informed))
//!   see the path `horizon_slots` ahead and take its way while it is more
//!   than `edge_bps` off: a burst before each move, and little else.
//! - **Decoys** trade exactly as the informed do until a slot each draws in
//!   `fade_share` of the session, and from then on on random views of their
//!   own, at the same pace: a record that is right, then isn't.
//! - **Chasers** take the way the book moved once it moved over
//!   `trigger_bps`: they trade with every move, after it.
//! - **Noise** takes at random, and **a refill** puts back the depth takes
//!   consume.
//!
//! Every role is seated in every session, from fresh wallets of the
//! session's keyring, with one size distribution and one limit for all;
//! only the counts, the horizon, the edge and the fades are drawn. Who is
//! who is the session's latent identity ([`Shape::roster`]), told to the
//! informed bot as wallets (`crate::latent`), never the moves themselves.

use mtx_core::mandate::Mandate;
use serde::Serialize;
use serde_json::json;

use crate::{
    actors::Library,
    market::{
        Calibration, MarketModel, ModelSpec,
        calibration::sha256_hex,
        model::{ModelPath, Start},
    },
    scenario::{Scenario, SplitMix64, VENUE},
};

/// NVDA's market calibration, built in so hidden sets need no file.
const NVDA_CALIBRATION: &str =
    include_str!("../../../calibration/market/nvda.20261113.903d1120.toml");

/// A session's draws beneath its path and tape: what the ladder's oracles
/// may be told. The informed bot is told only [`Shape::roster`]'s kinds.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Shape {
    /// Wallets of each role.
    pub informed: u32,
    pub decoys: u32,
    pub chasers: u32,
    pub noise: u32,
    /// The informed's and decoys' horizon and edge.
    pub horizon_slots: u64,
    pub edge_bps: f64,
    /// The window decoys fade in, in slots into the session.
    pub fade_from: u64,
    pub fade_until: u64,
    /// The chasers' trigger.
    pub trigger_bps: f64,
    /// Each move: its first slot, its ramp in slots and its size in percent
    /// (signed). The session's future: never told to the informed bot.
    pub moves: Vec<(u64, u64, f64)>,
}

/// The family's parameters. Every range is inclusive.
#[derive(Debug, Clone)]
pub struct FlowFamily {
    pub symbol: String,
    /// The snapshot's mark, which sessions start at.
    pub arrival_usd: f64,
    pub calibration: &'static str,
    pub duration: (u64, u64),
    /// Sharp moves a session, their size as a multiple of the calibration's
    /// median jump, and their ramp in slots. Onsets fall one to a stretch of
    /// the session, between `margin_slots` from either end.
    pub moves: (u64, u64),
    pub move_mult: (f64, f64),
    pub ramp_slots: (u64, u64),
    pub margin_slots: u64,
    /// Slots between the path's waypoints.
    pub waypoint_slots: u64,
    /// The market's own taker pace and size (its calibration's venue
    /// numbers), and every actor's size multiple and limit.
    pub trades_per_minute: f64,
    pub trade_size: f64,
    pub trade_size_sigma: f64,
    pub size_mult: f64,
    pub limit_bps: f64,
    /// Wallets of each role.
    pub informed: (u64, u64),
    pub decoys: (u64, u64),
    pub chasers: (u64, u64),
    pub noise: (u64, u64),
    /// The informed's (and decoys') horizon and edge, drawn per session.
    pub horizon_slots: (u64, u64),
    pub edge_bps: (f64, f64),
    /// Pace of the informed and of the decoys, and of each noise wallet and
    /// each chaser wallet, as multiples of the market's.
    pub informed_mult: f64,
    pub noise_mult: f64,
    pub chaser_mult: f64,
    /// Where decoys fade, as shares of the session.
    pub fade_share: (f64, f64),
    /// The chasers' trigger and lookback.
    pub trigger_bps: (f64, f64),
    pub chaser_lookback: u64,
    /// The refill's half-life range, in slots.
    pub refill_half_life: (f64, f64),
    /// The mandate: position cap in base units and loss limit in USD.
    pub max_position: f64,
    pub max_drawdown_usd: f64,
}

impl FlowFamily {
    /// NVDA perps, `smart-money-decay-nvda`.
    pub fn nvda() -> Self {
        Self {
            symbol: "NVDA".into(),
            arrival_usd: 159.92,
            calibration: NVDA_CALIBRATION,
            duration: (1_200, 2_000),
            moves: (3, 6),
            move_mult: (0.9, 2.1),
            ramp_slots: (2, 6),
            margin_slots: 100,
            waypoint_slots: 2,
            trades_per_minute: 1.24,
            trade_size: 0.015,
            trade_size_sigma: 1.04,
            size_mult: 10.0,
            limit_bps: 50.0,
            informed: (1, 3),
            decoys: (1, 3),
            chasers: (2, 4),
            noise: (3, 6),
            horizon_slots: (20, 80),
            edge_bps: (15.0, 40.0),
            informed_mult: 24.0,
            noise_mult: 4.0,
            chaser_mult: 8.0,
            fade_share: (0.2, 0.6),
            trigger_bps: (8.0, 20.0),
            chaser_lookback: 40,
            refill_half_life: (4.0, 40.0),
            max_position: 30.0,
            max_drawdown_usd: 500.0,
        }
    }

    /// The price's model: the calibration inside regular hours.
    pub fn model(&self) -> MarketModel {
        let mut calibration =
            Calibration::parse(self.calibration).expect("the family's calibration is valid");
        calibration.sha256 = sha256_hex(self.calibration.as_bytes());
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

    pub fn sample(&self, name: &str, seed: u64) -> Scenario {
        self.draw(name, seed).0
    }

    pub fn draw(&self, name: &str, seed: u64) -> (Scenario, Shape) {
        self.draw_with(&self.model(), name, seed)
    }

    fn draw_with(&self, model: &MarketModel, name: &str, seed: u64) -> (Scenario, Shape) {
        let mut rng = SplitMix64(seed);
        let duration = rng.range_u64(self.duration);
        let mut index = model.draw(rng.next_u64(), duration);
        let median_jump_pct = model.calibration.fair_value.jumps.abs_size_median_pct;

        let horizon_slots = rng.range_u64(self.horizon_slots);
        let edge_bps = round2(rng.range_f64(self.edge_bps));

        // The moves: one to each stretch of the session, each at least ten
        // basis points past the edge, so the informed see every one.
        let count = rng.range_u64(self.moves);
        let span = duration - 2 * self.margin_slots;
        let stretch = span / count;
        let mut moves = Vec::new();
        for k in 0..count {
            let from = self.margin_slots + k * stretch;
            let onset = from + rng.range_u64((stretch / 5, stretch * 4 / 5));
            let ramp = rng.range_u64(self.ramp_slots);
            let floor = (edge_bps + 10.0) / 100.0 / median_jump_pct;
            let mult = rng.range_f64(self.move_mult).max(floor);
            let sign = if rng.next_f64() < 0.5 { 1.0 } else { -1.0 };
            let size_pct = round3(sign * mult * median_jump_pct);
            let steps = uneven_ramp(&mut rng, 4);
            lay_move(&mut index, onset, ramp, size_pct, &steps);
            moves.push((onset, ramp, size_pct));
        }

        let informed = rng.range_u64(self.informed) as u32;
        let decoys = rng.range_u64(self.decoys) as u32;
        let chasers = rng.range_u64(self.chasers) as u32;
        let noise = rng.range_u64(self.noise) as u32;
        let fade_from = (self.fade_share.0 * duration as f64) as u64;
        let fade_until = (self.fade_share.1 * duration as f64) as u64;
        let trigger_bps = round2(rng.range_f64(self.trigger_bps));
        let half_life = round2(rng.range_f64(self.refill_half_life));

        // Every wallet of a pace-sharing role trades at the same pace: the
        // informed and the decoys split theirs among their wallets, noise
        // and chasers have it per wallet.
        let size = self.size_mult;
        let limit = self.limit_bps;
        let actors: Library = serde_json::from_value(json!({
            "scale": {
                "trades_per_minute": self.trades_per_minute,
                "trade_size": self.trade_size,
                "trade_size_sigma": self.trade_size_sigma,
            },
            "noise": [{
                "wallets": noise,
                "flow_mult": self.noise_mult * f64::from(noise),
                "size_mult": size,
                "limit_bps": limit,
            }],
            "informed": [{
                "wallets": informed,
                "horizon_slots": horizon_slots,
                "edge_bps": edge_bps,
                "intensity_mult": self.informed_mult * f64::from(informed),
                "size_mult": size,
                "limit_bps": limit,
            }],
            "chasers": [{
                "wallets": chasers,
                "lookback_slots": self.chaser_lookback,
                "trigger_bps": trigger_bps,
                "intensity_mult": self.chaser_mult * f64::from(chasers),
                "size_mult": size,
                "limit_bps": limit,
            }],
            "decoys": [{
                "wallets": decoys,
                "horizon_slots": horizon_slots,
                "edge_bps": edge_bps,
                "informed_for": { "from": fade_from, "until": fade_until },
                "intensity_mult": self.informed_mult * f64::from(decoys),
                "size_mult": size,
                "limit_bps": limit,
            }],
            "refill": { "half_life_slots": [half_life, half_life], "levels": 5 },
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
            informed,
            decoys,
            chasers,
            noise,
            horizon_slots,
            edge_bps,
            fade_from,
            fade_until,
            trigger_bps,
            moves,
        };
        let description = format!(
            "{}: trade the perp off the tape, within {} {} and a ${} loss limit",
            self.symbol, self.max_position, self.symbol, self.max_drawdown_usd
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
            market_model: Some(record),
            raw_sends: None,
            start_positions: Default::default(),
            close_out: Default::default(),
            tx_layer: None,
            stop_out: None,
            liquidator: None,
        };
        (scenario, shape)
    }

    pub fn sample_set(&self, prefix: &str, count: usize, seed: u64) -> Vec<Scenario> {
        self.draw_set(prefix, count, seed)
            .into_iter()
            .map(|(scenario, _)| scenario)
            .collect()
    }

    /// `count` sessions named `<prefix>-<n>`.
    pub fn draw_set(&self, prefix: &str, count: usize, seed: u64) -> Vec<(Scenario, Shape)> {
        let model = self.model();
        (1..=count as u64)
            .map(|n| {
                let draw_seed = seed.wrapping_mul(1_000_003).wrapping_add(n);
                self.draw_with(&model, &format!("{prefix}-{n}"), draw_seed)
            })
            .collect()
    }
}

/// `n` cumulative shares rising from 0 to 1 in uneven steps.
fn uneven_ramp(rng: &mut SplitMix64, n: usize) -> Vec<f64> {
    let mut steps: Vec<f64> = (0..n).map(|_| 0.2 + rng.next_f64()).collect();
    let total: f64 = steps.iter().sum();
    let mut acc = 0.0;
    for step in &mut steps {
        acc += *step / total;
        *step = acc;
    }
    steps
}

/// Lays a move of `size_pct` on the path from `onset` over `ramp` slots, in
/// the ramp's uneven steps, and holds it after.
fn lay_move(path: &mut ModelPath, onset: u64, ramp: u64, size_pct: f64, steps: &[f64]) {
    let lift = (1.0 + size_pct / 100.0).ln();
    let n = steps.len() as u64;
    for (slot, level) in path.log_levels.iter_mut().enumerate() {
        let slot = slot as u64;
        if slot < onset {
            continue;
        }
        let done = ((slot - onset) * n / ramp.max(1)).min(n);
        let share = if done == 0 {
            0.0
        } else {
            steps[done as usize - 1]
        };
        *level += lift * share;
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

    #[test]
    fn sessions_are_valid_and_deterministic() {
        let family = FlowFamily::nvda();
        let a = family.sample_set("t", 6, 9);
        let b = family.sample_set("t", 6, 9);
        assert_eq!(
            serde_json::to_string(&a).unwrap(),
            serde_json::to_string(&b).unwrap()
        );
        for scenario in &a {
            scenario.validate().unwrap();
            scenario.actors.as_ref().unwrap().validate().unwrap();
        }
    }

    #[test]
    fn every_move_clears_the_edge_and_both_ways_come() {
        let family = FlowFamily::nvda();
        let mut signs = (0, 0);
        for (_, shape) in family.draw_set("t", 40, 3) {
            assert!((3..=6).contains(&shape.moves.len()));
            for (_, _, size) in &shape.moves {
                assert!(size.abs() * 100.0 >= shape.edge_bps + 9.9, "{shape:?}");
                if *size > 0.0 {
                    signs.0 += 1;
                } else {
                    signs.1 += 1;
                }
            }
        }
        assert!(signs.0 > 40 && signs.1 > 40, "{signs:?}");
    }
}
