//! The toxic-day family: TSLA's opening half hour, when the price jumps
//! most. Two funds work the tape in one-sided bursts: one sees the jumps
//! coming, the other is an execution algo that knows nothing of the price.
//! On a toxic morning the informed fund is the busy one; on a quiet morning
//! the algo is. The bursts look the same either way; only what the price
//! does after them tells the mornings apart.
//!
//! - **The price** walks TSLA's calibration from the 09:30 open, with the
//!   opening hour's measured jump rate and sizes (3.9 an hour, median
//!   0.382%, p90 0.652%) and its diffusive volatility (a stated
//!   `vol_mult`): about one session in four has no jump at all.
//! - **The fund** (the library's [`Informed`]) sees the path
//!   `horizon_slots` ahead and takes its way while it is more than
//!   `edge_bps` off.
//! - **The algo** (the library's [`Algo`]) works parent orders on random
//!   sides, each about as long as the fund's horizon, with pauses that make
//!   it busy as often as the fund is.
//! - The busy one trades at a pace drawn per session, the other at a
//!   quarter of it: the kind of morning only swaps which is which.
//! - **Noise** takes at random and **chasers** take the way the book moved,
//!   after it moved, the same on either morning.
//!
//! Every role is seated in every session, from wallets of its own drawn from
//! one range, with one size distribution and one limit for all. The
//! informed bot is told the fund's wallets (an identity, [`crate::latent`]),
//! never the kind of morning, the path, or when anyone trades.

use std::collections::BTreeMap;

use mtx_core::mandate::Mandate;
use serde::Serialize;

use crate::{
    actors::{
        Library,
        library::{Algo, Chaser, Informed, Noise, Scale},
    },
    market::{
        Calibration, MarketModel, ModelSpec,
        calibration::sha256_hex,
        model::{EventKind, JumpSize, ModelPath, Start},
    },
    orders::CloseOut,
    scenario::{CloseMode, Scenario, SplitMix64, StopOut, StopRule, VENUE},
};

/// TSLA's market calibration, built in so hidden sets need no file.
const TSLA_CALIBRATION: &str =
    include_str!("../../../calibration/market/tsla.20261113.8b7bca8e.toml");

/// Where the opening hour's numbers come from.
const OPENING_SOURCE: &str = "TSLA Phoenix 1 m mark candles 2026-06-17 to 2026-11-12, 09:31-10:30 New York, all days; the calibration's jump rule (1 m return beyond 5 robust sigmas of the trailing hour)";

/// What a session is, beneath its path.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Shape {
    /// `toxic` (the fund is the busy one) or `quiet` (the algo is).
    pub day: String,
    /// Paces while trading, as multiples of the market's trade rate.
    pub fund_intensity: f64,
    pub algo_intensity: f64,
    pub noise_flow: f64,
    /// The share of the session's slots the fund's edge holds in.
    pub fund_active: f64,
    pub horizon_slots: u64,
    pub edge_bps: f64,
    /// The algo's parents' lengths and the pauses between, in slots.
    pub algo_length_slots: [u64; 2],
    pub algo_gap_slots: [u64; 2],
    pub fund_wallets: u32,
    pub algo_wallets: u32,
    pub noise_wallets: u32,
    pub chaser_wallets: u32,
    pub chaser_trigger_bps: f64,
    /// The path's jumps: slot and size in percent.
    pub jumps: Vec<(u64, f64)>,
}

/// The family's parameters. Every range is inclusive.
#[derive(Debug, Clone)]
pub struct ToxicDayFamily {
    pub symbol: String,
    pub duration: (u64, u64),
    pub waypoint_slots: u64,
    pub vol_mult: f64,
    pub jump_rate_per_hour: f64,
    pub jump_median_pct: f64,
    pub jump_p90_pct: f64,
    /// The busy one's pace while trading; the other's is `quiet_share` of
    /// it.
    pub busy_intensity: (f64, f64),
    pub quiet_share: f64,
    pub noise_flow: (f64, f64),
    pub horizon_slots: (u64, u64),
    pub edge_bps: (f64, f64),
    /// Wallets of the fund, the algo and noise, each drawn from this range.
    pub wallets: (u64, u64),
    pub chaser_wallets: (u64, u64),
    pub chaser_trigger_bps: (f64, f64),
    pub chaser_lookback_slots: u64,
    pub chaser_intensity: f64,
    pub size_mult: f64,
    pub limit_bps: f64,
    pub max_position: f64,
    pub max_drawdown_usd: f64,
    pub close_haircut_bps: f64,
}

impl ToxicDayFamily {
    pub fn tsla() -> Self {
        Self {
            symbol: "TSLA".into(),
            // 20 to 30 minutes from the open.
            duration: (3_000, 4_500),
            waypoint_slots: 4,
            vol_mult: 2.0,
            jump_rate_per_hour: 3.9,
            jump_median_pct: 0.382,
            jump_p90_pct: 0.652,
            busy_intensity: (60.0, 90.0),
            quiet_share: 0.25,
            noise_flow: (8.0, 12.0),
            horizon_slots: (60, 150),
            edge_bps: (20.0, 30.0),
            wallets: (6, 10),
            chaser_wallets: (2, 3),
            chaser_trigger_bps: (10.0, 15.0),
            chaser_lookback_slots: 30,
            chaser_intensity: 15.0,
            size_mult: 25.0,
            limit_bps: 80.0,
            // About 0.31x of the 10,000 USDC of collateral at $388.95.
            max_position: 8.0,
            max_drawdown_usd: 400.0,
            close_haircut_bps: 5.0,
        }
    }

    /// The price's model: TSLA's calibration from the regular open, with
    /// the opening hour's jumps and volatility stated.
    pub fn model(&self) -> MarketModel {
        let mut calibration =
            Calibration::parse(TSLA_CALIBRATION).expect("TSLA's calibration is valid");
        calibration.sha256 = sha256_hex(TSLA_CALIBRATION.as_bytes());
        MarketModel::new(
            calibration,
            ModelSpec {
                vol_mult: self.vol_mult,
                start: Start::Before {
                    kind: "regular".into(),
                    lead_seconds: 0.0,
                },
                waypoint_slots: self.waypoint_slots,
                jumps: true,
                jump_rate_per_hour: Some(self.jump_rate_per_hour),
                jump_size: Some(JumpSize {
                    abs_median_pct: self.jump_median_pct,
                    abs_p90_pct: self.jump_p90_pct,
                    source: OPENING_SOURCE.into(),
                }),
                gaps: false,
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
        self.draw_with(&self.model(), name, seed)
    }

    fn draw_with(&self, model: &MarketModel, name: &str, seed: u64) -> (Scenario, Shape) {
        let mut rng = SplitMix64(seed);
        let duration = rng.range_u64(self.duration);
        let walk = model.draw(rng.next_u64(), duration);

        let toxic = rng.next_f64() < 0.5;
        let busy = round3(rng.range_f64(self.busy_intensity));
        let light = round3(busy * self.quiet_share);
        let (fund_intensity, algo_intensity) = if toxic { (busy, light) } else { (light, busy) };
        let noise_flow = round3(rng.range_f64(self.noise_flow));
        let horizon_slots = rng.range_u64(self.horizon_slots);
        let edge_bps = round3(rng.range_f64(self.edge_bps));
        let chaser_trigger_bps = round3(rng.range_f64(self.chaser_trigger_bps));
        let fund_wallets = rng.range_u64(self.wallets) as u32;
        let algo_wallets = rng.range_u64(self.wallets) as u32;
        let noise_wallets = rng.range_u64(self.wallets) as u32;
        let chaser_wallets = rng.range_u64(self.chaser_wallets) as u32;

        // The algo's parents last about the fund's horizon, and its pauses
        // keep it working about as often as the fund's edge holds.
        let fund_active = active_ahead(&walk, horizon_slots, edge_bps);
        let length = horizon_slots;
        let gap = (length as f64 * (1.0 / fund_active.max(0.02) - 1.0)).round() as u64;
        let algo_length_slots = [length / 2, length * 3 / 2];
        let algo_gap_slots = [(gap / 2).max(1), (gap * 3 / 2).max(1)];

        let path = walk.waypoints(self.waypoint_slots);
        let jumps = walk
            .record
            .events
            .iter()
            .filter(|e| e.kind == EventKind::Jump)
            .map(|e| (e.slot, round3(e.pct)))
            .collect();
        let actors = Library {
            scale: Self::scale(model),
            noise: vec![Noise {
                wallets: noise_wallets,
                flow_mult: noise_flow,
                size_mult: self.size_mult,
                limit_bps: self.limit_bps,
                active: None,
            }],
            informed: vec![Informed {
                wallets: fund_wallets,
                horizon_slots,
                edge_bps,
                intensity_mult: fund_intensity,
                size_mult: self.size_mult,
                limit_bps: self.limit_bps,
                active: None,
            }],
            algos: vec![Algo {
                wallets: algo_wallets,
                intensity_mult: algo_intensity,
                size_mult: self.size_mult,
                limit_bps: self.limit_bps,
                length_slots: algo_length_slots,
                gap_slots: algo_gap_slots,
                active: None,
            }],
            chasers: vec![Chaser {
                wallets: chaser_wallets,
                lookback_slots: self.chaser_lookback_slots,
                trigger_bps: chaser_trigger_bps,
                intensity_mult: self.chaser_intensity,
                size_mult: self.size_mult,
                limit_bps: self.limit_bps,
                active: None,
            }],
            ..Library::default()
        };
        let mandate = Mandate::perp(
            VENUE,
            &self.symbol,
            self.max_position,
            self.max_drawdown_usd,
        );
        let shape = Shape {
            day: if toxic { "toxic" } else { "quiet" }.into(),
            fund_intensity,
            algo_intensity,
            noise_flow,
            fund_active: round3(fund_active),
            horizon_slots,
            edge_bps,
            algo_length_slots,
            algo_gap_slots,
            fund_wallets,
            algo_wallets,
            noise_wallets,
            chaser_wallets,
            chaser_trigger_bps,
            jumps,
        };
        let description = format!(
            "{}: the opening half hour; position within {} either way",
            self.symbol, self.max_position
        );
        let scenario = Scenario {
            name: name.to_string(),
            description,
            symbol: self.symbol.clone(),
            duration_slots: duration,
            mandate,
            path: path.clone(),
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
            fair_path: Some(path),
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

    pub fn sample_set(&self, prefix: &str, count: usize, seed: u64) -> Vec<Scenario> {
        self.draw_set(prefix, count, seed)
            .into_iter()
            .map(|(scenario, _)| scenario)
            .collect()
    }

    /// `count` sessions named `<prefix>-<n>`.
    pub fn draw_set(&self, prefix: &str, count: usize, seed: u64) -> Vec<(Scenario, Shape)> {
        let model = self.model();
        (0..count)
            .map(|i| {
                let draw_seed = seed.wrapping_mul(1_000_003).wrapping_add(i as u64 + 1);
                self.draw_with(&model, &format!("{prefix}-{}", i + 1), draw_seed)
            })
            .collect()
    }
}

/// The share of slots at which the level `horizon` slots ahead is more than
/// `edge_bps` away: when an informed trader with that view trades.
fn active_ahead(walk: &ModelPath, horizon: u64, edge_bps: f64) -> f64 {
    let l = &walk.log_levels;
    let n = l.len();
    let h = horizon as usize;
    let edge = edge_bps / 10_000.0;
    let hits = (0..n)
        .filter(|&t| ((l[(t + h).min(n - 1)] - l[t]).exp() - 1.0).abs() > edge)
        .count();
    hits as f64 / n as f64
}

fn round3(x: f64) -> f64 {
    (x * 1000.0).round() / 1000.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sessions_are_valid_and_deterministic() {
        let family = ToxicDayFamily::tsla();
        for seed in 0..30 {
            let (scenario, _) = family.draw("s", seed);
            assert_eq!(scenario.validate(), Ok(()), "seed {seed}");
            assert_eq!(scenario, family.sample("s", seed));
            let v2 = crate::scenario_v2::ScenarioV2::try_from(&scenario).unwrap();
            assert_eq!(v2.into_scenario().unwrap(), scenario);
        }
    }

    /// Both kinds of morning come up, every session seats every role, and
    /// the morning only swaps which of the fund and the algo is busy.
    #[test]
    fn mornings_swap_who_is_busy() {
        let family = ToxicDayFamily::tsla();
        let drawn = family.draw_set("t", 200, 3);
        let toxic = drawn.iter().filter(|(_, s)| s.day == "toxic").count();
        assert!((70..=130).contains(&toxic), "{toxic} toxic");
        let no_jump = drawn.iter().filter(|(_, s)| s.jumps.is_empty()).count();
        assert!((20..=90).contains(&no_jump), "{no_jump} sessions without a jump");
        for (scenario, shape) in &drawn {
            let actors = scenario.actors.as_ref().unwrap();
            assert_eq!(actors.informed.len(), 1);
            assert_eq!(actors.algos.len(), 1);
            assert_eq!(actors.noise.len(), 1);
            assert_eq!(actors.chasers.len(), 1);
            let (busy, light) = if shape.day == "toxic" {
                (shape.fund_intensity, shape.algo_intensity)
            } else {
                (shape.algo_intensity, shape.fund_intensity)
            };
            assert!((busy * family.quiet_share - light).abs() < 0.01, "{shape:?}");
            assert_eq!(actors.algos[0].size_mult, actors.informed[0].size_mult);
        }
    }
}
