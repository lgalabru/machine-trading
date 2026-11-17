//! The copy-shy family: NVDA's opening half hour, where one fund trades
//! ahead of the moves and hides its trading once it sees it is copied.
//!
//! - **The price** walks NVDA's market calibration from the 09:30 New York
//!   open for 20 to 30 minutes (4,800 to 7,200 of its 0.25 s slots),
//!   at the opening hour's measured volatility and jumps: the
//!   calibration's own rule finds 2.75 jumps an hour from 09:31 to 10:30,
//!   median 0.305% and p90 0.533%, and a robust 1 m sd twice the regular
//!   session's (`OPEN_*`).
//! - **The fund** (the library's [`Informed`](crate::actors::library::Informed),
//!   one wallet) sees the path `horizon_slots` ahead and takes its way
//!   while it is more than `edge_bps` off. It is [`Shy`]: once a single
//!   account's flow its way after its fills comes to `copy_share` of them,
//!   it trades `throttle` of its size, `throttle` of its horizon ahead,
//!   until `cooldown_slots` after it last found itself copied.
//! - **Decoys** trade as the fund does until a slot each draws, then on
//!   random views of their own; they are as shy as the fund, so a throttle
//!   names no one.
//! - **Chasers** take the way the book moved; their takes count as copying
//!   too, so the fund sometimes hides with no bot at all.
//! - **Noise** takes at random, and **a refill** puts back the depth.
//!
//! Every role is seated every session from fresh wallets, with one size
//! distribution and one limit. Who is who and the shy rule's parameters are
//! the session's latent state ([`Shape`]); the path, and when the fund
//! trades, never are.

use std::collections::BTreeMap;

use mtx_core::mandate::Mandate;
use serde::Serialize;
use serde_json::json;

use crate::{
    actors::{Library, library::Shy},
    market::{
        Calibration, MarketModel, ModelSpec,
        calibration::sha256_hex,
        model::{JumpSize, Start},
    },
    orders::CloseOut,
    scenario::{Scenario, SplitMix64, VENUE},
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
/// The opening hour's robust 1 m sd (1.4826 times the median absolute
/// return, 10.70 bp) over all regular hours' (5.38 bp).
pub const OPEN_VOL_MULT: f64 = 1.99;
const OPEN_SOURCE: &str = "NVDA Phoenix 1 m mark, 09:31-10:30 New York, 2026-06-10 to 2026-11-12, the calibration's 5-robust-sigma jump rule (JUMP_REGIME_MEASUREMENT.md, table 2)";

/// A session's draws beneath its path and tape: what the ladder's oracles
/// may be told. The informed bot is told the roster and [`Shape::shy`].
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Shape {
    pub decoys: u32,
    pub chasers: u32,
    pub noise: u32,
    pub horizon_slots: u64,
    pub edge_bps: f64,
    /// The fund's and the decoys' reaction to being copied.
    pub shy: Shy,
    pub trigger_bps: f64,
}

/// The family's parameters. Every range is inclusive.
#[derive(Debug, Clone)]
pub struct CopyShyFamily {
    pub symbol: String,
    pub arrival_usd: f64,
    pub calibration: &'static str,
    pub duration: (u64, u64),
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
    /// The fund's and each decoy's pace, and each noise and chaser wallet's,
    /// as multiples of the market's.
    pub informed_mult: f64,
    pub noise_mult: f64,
    pub chaser_mult: f64,
    /// Where decoys fade, as shares of the session.
    pub fade_share: (f64, f64),
    pub trigger_bps: (f64, f64),
    pub chaser_lookback: u64,
    pub refill_half_life: (f64, f64),
    /// The shy rule's ranges.
    pub detect_slots: (u64, u64),
    pub copy_share: (f64, f64),
    pub shy_fills: u32,
    pub throttle: (f64, f64),
    pub cooldown_slots: (u64, u64),
    pub max_position: f64,
    pub max_drawdown_usd: f64,
    pub close_haircut_bps: f64,
}

impl CopyShyFamily {
    /// NVDA perps, `copy-shy-informed-nvda`.
    pub fn nvda() -> Self {
        Self {
            symbol: "NVDA".into(),
            arrival_usd: 159.92,
            calibration: NVDA_CALIBRATION,
            duration: (4_800, 7_200),
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
            edge_bps: (15.0, 20.0),
            informed_mult: 24.0,
            noise_mult: 2.0,
            chaser_mult: 8.0,
            fade_share: (0.2, 0.6),
            trigger_bps: (8.0, 15.0),
            chaser_lookback: 40,
            refill_half_life: (4.0, 40.0),
            detect_slots: (2, 8),
            copy_share: (0.3, 0.6),
            shy_fills: 3,
            throttle: (0.15, 0.3),
            cooldown_slots: (600, 1_200),
            max_position: 10.0,
            max_drawdown_usd: 200.0,
            close_haircut_bps: 5.0,
        }
    }

    /// The price's model: the calibration from the open, at the opening
    /// hour's measured volatility and jumps.
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
        self.draw_with(&self.model(), name, seed)
    }

    fn draw_with(&self, model: &MarketModel, name: &str, seed: u64) -> (Scenario, Shape) {
        let mut rng = SplitMix64(seed);
        let duration = rng.range_u64(self.duration);
        let index = model.draw(rng.next_u64(), duration);
        let horizon_slots = rng.range_u64(self.horizon_slots);
        let edge_bps = round2(rng.range_f64(self.edge_bps));
        let decoys = rng.range_u64(self.decoys) as u32;
        let chasers = rng.range_u64(self.chasers) as u32;
        let noise = rng.range_u64(self.noise) as u32;
        let trigger_bps = round2(rng.range_f64(self.trigger_bps));
        let half_life = round2(rng.range_f64(self.refill_half_life));
        let shy = Shy {
            detect_slots: rng.range_u64(self.detect_slots),
            copy_share: round2(rng.range_f64(self.copy_share)),
            fills: self.shy_fills,
            throttle: round2(rng.range_f64(self.throttle)),
            cooldown_slots: rng.range_u64(self.cooldown_slots),
        };
        let fade_from = (self.fade_share.0 * duration as f64) as u64;
        let fade_until = (self.fade_share.1 * duration as f64) as u64;

        let size = self.size_mult;
        let limit = self.limit_bps;
        // Each decoy is an actor of its own, so each minds its own copiers.
        let decoy = json!({
            "wallets": 1,
            "horizon_slots": horizon_slots,
            "edge_bps": edge_bps,
            "informed_for": { "from": fade_from, "until": fade_until },
            "intensity_mult": self.informed_mult,
            "size_mult": size,
            "limit_bps": limit,
            "shy": shy,
        });
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
                "wallets": 1,
                "horizon_slots": horizon_slots,
                "edge_bps": edge_bps,
                "intensity_mult": self.informed_mult,
                "size_mult": size,
                "limit_bps": limit,
                "shy": shy,
            }],
            "chasers": [{
                "wallets": chasers,
                "lookback_slots": self.chaser_lookback,
                "trigger_bps": trigger_bps,
                "intensity_mult": self.chaser_mult * f64::from(chasers),
                "size_mult": size,
                "limit_bps": limit,
            }],
            "decoys": vec![decoy; decoys as usize],
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
            decoys,
            chasers,
            noise,
            horizon_slots,
            edge_bps,
            shy,
            trigger_bps,
        };
        let description = format!(
            "{}: trade the perp off the tape at the open, within {} {} and a ${} loss limit",
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

fn round2(x: f64) -> f64 {
    (x * 100.0).round() / 100.0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{market::model::EventKind, scenario::SLOT_SECONDS};

    #[test]
    fn sessions_are_valid_and_deterministic() {
        let family = CopyShyFamily::nvda();
        let a = family.draw_set("t", 6, 9);
        let b = family.draw_set("t", 6, 9);
        assert_eq!(
            serde_json::to_string(&a.iter().map(|(s, _)| s).collect::<Vec<_>>()).unwrap(),
            serde_json::to_string(&b.iter().map(|(s, _)| s).collect::<Vec<_>>()).unwrap()
        );
        for (scenario, shape) in &a {
            scenario.validate().unwrap();
            assert_eq!(scenario.valuation(), "oracle");
            let actors = scenario.actors.as_ref().unwrap();
            actors.validate().unwrap();
            assert!(actors.has_shy() && actors.watches_bot());
            assert_eq!(actors.decoys.len() as u32, shape.decoys);
            assert!((4_800..=7_200).contains(&scenario.duration_slots));
        }
    }

    #[test]
    fn the_open_has_its_measured_jumps_both_ways() {
        let family = CopyShyFamily::nvda();
        let model = family.model();
        let (mut jumps, mut up, mut hours) = (0, 0, 0.0);
        for seed in 0..200 {
            let path = model.draw(seed, 4_000);
            hours += 4_000.0 * SLOT_SECONDS / 3_600.0;
            for event in path
                .record
                .events
                .iter()
                .filter(|e| e.kind == EventKind::Jump)
            {
                jumps += 1;
                up += usize::from(event.pct > 0.0);
            }
        }
        let rate = jumps as f64 / hours;
        assert!((2.2..3.3).contains(&rate), "{rate}");
        assert!(
            up * 10 > jumps * 4 && up * 10 < jumps * 6,
            "{up} of {jumps}"
        );
    }
}
