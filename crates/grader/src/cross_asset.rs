//! The cross-asset family: the flow-reading family's NVDA tape, read, and a
//! QQQ perp beside it on the same cross-margin account, traded.
//!
//! - **NVDA** is [`FlowFamily::nvda`]'s session as it stands: its sharp
//!   moves, and its informed, decoy, chaser and noise wallets (the actor
//!   library trades the primary market, NVDA). The mandate lets the account
//!   hold next to none of it: the desk reads NVDA and trades QQQ.
//! - **QQQ**'s own path is drawn jointly with NVDA's
//!   ([`draw_joint`](crate::market::model::draw_joint)) at the two
//!   calibrations' fitted correlation, and carries a share `beta` of each of
//!   NVDA's moves, at the same slots: news that moves NVDA moves the index
//!   it is in. `beta` is drawn per session and never told.
//! - **QQQ's tape** is noise, and chasers that take the way QQQ moved once
//!   it moved past their trigger: flow that carries nothing ahead of a move.
//!
//! The informed bot is told NVDA's roster alone, as the flow-reading
//! family's is ([`Shape::roster`](crate::flow_reading::Shape)): who is
//! informed and who is a decoy, not the moves, nor `beta`.

use std::collections::BTreeMap;

use mtx_core::mandate::{Allowance, InstrumentRef, Kind, Mandate};
use serde::Serialize;

use crate::{
    actors::Pricing,
    flow_reading::{FlowFamily, Shape as FlowShape, lay_move},
    making::{TakerOrder, TakerSide},
    market::{
        Calibration, MarketModel, ModelSpec,
        calibration::sha256_hex,
        model::{ModelPath, Start, draw_joint},
    },
    orders::CloseOut,
    scenario::{JointDraw, MarketScenario, Scenario, SplitMix64, VENUE},
};

/// QQQ's market calibration, built in so hidden sets need no file.
const QQQ_CALIBRATION: &str =
    include_str!("../../../calibration/market/qqq.20261113.60ee1c12.toml");

/// The correlation file the two markets' entry comes from.
const CORRELATION_ID: &str = "us-eq.20261113.617e47d8";
const CORRELATION: &str =
    include_str!("../../../calibration/correlation/us-eq.20261113.617e47d8.toml");

/// A session's draws: the NVDA session's, and what QQQ carries of it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Shape {
    #[serde(flatten)]
    pub flow: FlowShape,
    /// The share of each NVDA move QQQ makes, at the same slots.
    pub beta: f64,
    /// Chaser takes on QQQ's tape, and noise takes.
    pub qqq_chases: u32,
    pub qqq_noise: u32,
}

/// The family's parameters. Every range is inclusive.
#[derive(Debug, Clone)]
pub struct CrossFamily {
    /// The market read, with its flow and moves.
    pub read: FlowFamily,
    /// The market traded.
    pub symbol: String,
    pub calibration: &'static str,
    /// The two markets' correlation, from [`CORRELATION_ID`].
    pub correlation: f64,
    /// What QQQ carries of each NVDA move.
    pub beta: (f64, f64),
    /// QQQ's tape: noise takes a minute, and the chasers' trigger, lookback
    /// and takes a chase, with every take's size multiple and limit.
    pub noise_per_minute: f64,
    pub trigger_bps: (f64, f64),
    pub chaser_lookback: u64,
    pub chase_takes: u64,
    pub size_mult: f64,
    pub limit_bps: f64,
    /// The mandate: QQQ's cap and the read market's (the smallest Phoenix
    /// holds, as a mandate's cap must be positive), and the loss limit.
    pub max_position: f64,
    pub read_max_position: f64,
    pub close_haircut_bps: f64,
}

impl CrossFamily {
    /// QQQ traded on NVDA's flow, `cross-asset-lead-qqq`.
    pub fn qqq() -> Self {
        Self {
            read: FlowFamily::nvda(),
            symbol: "QQQ".into(),
            calibration: QQQ_CALIBRATION,
            correlation: correlation("nvda", "qqq"),
            beta: (0.35, 0.75),
            noise_per_minute: 6.0,
            trigger_bps: (4.0, 10.0),
            chaser_lookback: 40,
            chase_takes: 3,
            size_mult: 10.0,
            limit_bps: 50.0,
            max_position: 10.0,
            read_max_position: 0.001,
            close_haircut_bps: 5.0,
        }
    }

    fn model(&self) -> MarketModel {
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
        self.draw_with(&self.read.model(), &self.model(), name, seed)
    }

    fn draw_with(
        &self,
        read_model: &MarketModel,
        model: &MarketModel,
        name: &str,
        seed: u64,
    ) -> (Scenario, Shape) {
        let matrix = vec![vec![1.0, self.correlation], vec![self.correlation, 1.0]];
        let mut traded: Option<ModelPath> = None;
        let (mut scenario, flow) = self.read.draw_on(read_model, name, seed, |seed, duration| {
            let mut paths = draw_joint(&[read_model, model], &matrix, seed, duration)
                .expect("the family's matrix and calibrations draw jointly");
            traded = paths.pop();
            paths.pop().expect("two paths")
        });
        let mut path = traded.expect("the joint draw ran");
        let duration = scenario.duration_slots;

        let mut rng = SplitMix64(seed ^ 0x7171_715f_6c65_6164);
        let beta = round3(rng.range_f64(self.beta));
        for (onset, ramp, size_pct) in &flow.moves {
            let lift = ((1.0 + size_pct / 100.0).ln() * beta).exp_m1() * 100.0;
            lay_move(&mut path, *onset, *ramp, lift, &[0.25, 0.5, 0.75, 1.0]);
        }

        let trigger_bps = rng.range_f64(self.trigger_bps);
        let (tape, qqq_chases, qqq_noise) = self.tape(&mut rng, &path, duration, trigger_bps);

        let read_record = scenario
            .market_model
            .take()
            .expect("the flow family records its draw");
        let read_symbol = scenario.symbol.clone();
        let read_market = MarketScenario {
            symbol: read_symbol.clone(),
            path: std::mem::take(&mut scenario.path),
            fair_path: None,
            funding: None,
            making: None,
            tape: Vec::new(),
            takers_pricing: None,
            market_model: None,
        };
        let traded_market = MarketScenario {
            symbol: self.symbol.clone(),
            path: path.waypoints(self.read.waypoint_slots),
            fair_path: None,
            funding: None,
            making: None,
            tape,
            takers_pricing: Some(Pricing::Book),
            market_model: None,
        };
        scenario.markets = vec![read_market, traded_market];
        scenario.markets.sort_by(|a, b| a.symbol.cmp(&b.symbol));
        scenario.joint = Some(JointDraw {
            matrix_source: CORRELATION_ID.into(),
            matrix_sha256: Some(sha256_hex(CORRELATION.as_bytes())),
            assets: vec![read_symbol.clone(), self.symbol.clone()],
            matrix,
            records: BTreeMap::from([
                (read_symbol.clone(), read_record),
                (self.symbol.clone(), path.record.clone()),
            ]),
        });
        let mut mandate = Mandate::perp(
            VENUE,
            &read_symbol,
            self.read_max_position,
            self.read.max_drawdown_usd,
        );
        mandate.universe.push(Allowance::new(
            InstrumentRef {
                venue: VENUE.into(),
                kind: Kind::Perp,
                symbol: self.symbol.clone(),
            },
            self.max_position,
        ));
        scenario.mandate = mandate;
        scenario.close_out = [&read_symbol, &self.symbol]
            .into_iter()
            .map(|symbol| {
                (
                    symbol.clone(),
                    CloseOut {
                        haircut_bps: self.close_haircut_bps,
                    },
                )
            })
            .collect();
        scenario.description = format!(
            "{}: trade the perp on {}'s tape, within {} {} and a ${} loss limit",
            self.symbol, read_symbol, self.max_position, self.symbol, self.read.max_drawdown_usd
        );
        let shape = Shape {
            flow,
            beta,
            qqq_chases,
            qqq_noise,
        };
        (scenario, shape)
    }

    /// QQQ's taker tape: noise at random, and a chase of `chase_takes`
    /// takes whenever the path moved past `trigger_bps` over the lookback,
    /// the way it moved, once per move.
    fn tape(
        &self,
        rng: &mut SplitMix64,
        path: &ModelPath,
        duration: u64,
        trigger_bps: f64,
    ) -> (Vec<TakerOrder>, u32, u32) {
        let calibration =
            Calibration::parse(self.calibration).expect("the family's calibration is valid");
        let venue = &calibration.venues["phoenix_perp"];
        let median = venue.trade_size.median;
        let sigma = venue.trade_size.sigma;
        let slots_per_minute = 150.0;
        let per_slot = self.noise_per_minute / slots_per_minute;
        let size = |rng: &mut SplitMix64| {
            round4(median * self.size_mult * (sigma * rng.normal()).exp()).max(0.001)
        };
        let mut orders = Vec::new();
        let (mut chases, mut noise) = (0u32, 0u32);
        let mut quiet_until = 0;
        let lookback = self.chaser_lookback;
        for slot in 1..duration.saturating_sub(4) {
            if rng.next_f64() < per_slot {
                let side = if rng.next_f64() < 0.5 {
                    TakerSide::Buy
                } else {
                    TakerSide::Sell
                };
                orders.push(TakerOrder {
                    slot,
                    side,
                    size: size(rng),
                    limit_bps: self.limit_bps,
                    account: (rng.next_u64() % 8) as u8,
                });
                noise += 1;
            }
            if slot < lookback || slot < quiet_until {
                continue;
            }
            let moved = (path.log_levels[slot as usize]
                - path.log_levels[(slot - lookback) as usize])
                * 1e4;
            if moved.abs() >= trigger_bps {
                let side = if moved > 0.0 {
                    TakerSide::Buy
                } else {
                    TakerSide::Sell
                };
                for k in 0..self.chase_takes {
                    orders.push(TakerOrder {
                        slot: slot + 1 + 2 * k,
                        side,
                        size: size(rng),
                        limit_bps: self.limit_bps,
                        account: (rng.next_u64() % 8) as u8,
                    });
                }
                chases += 1;
                quiet_until = slot + lookback;
            }
        }
        orders.sort_by_key(|o| o.slot);
        orders.retain(|o| o.slot + 4 < duration);
        (orders, chases, noise)
    }

    pub fn sample_set(&self, prefix: &str, count: usize, seed: u64) -> Vec<Scenario> {
        self.draw_set(prefix, count, seed)
            .into_iter()
            .map(|(scenario, _)| scenario)
            .collect()
    }

    /// `count` sessions named `<prefix>-<n>`.
    pub fn draw_set(&self, prefix: &str, count: usize, seed: u64) -> Vec<(Scenario, Shape)> {
        let (read_model, model) = (self.read.model(), self.model());
        (1..=count as u64)
            .map(|n| {
                let draw_seed = seed.wrapping_mul(1_000_003).wrapping_add(n);
                self.draw_with(&read_model, &model, &format!("{prefix}-{n}"), draw_seed)
            })
            .collect()
    }
}

/// The fitted correlation between two assets of [`CORRELATION`].
fn correlation(a: &str, b: &str) -> f64 {
    let file: toml::Value = toml::from_str(CORRELATION).expect("the correlation file is TOML");
    let assets: Vec<&str> = file["meta"]["assets"]
        .as_array()
        .expect("assets")
        .iter()
        .filter_map(toml::Value::as_str)
        .collect();
    let at = |s: &str| assets.iter().position(|x| *x == s).expect("a listed asset");
    file["correlation"]["matrix"][at(a)][at(b)]
        .as_float()
        .expect("a number")
}

fn round3(x: f64) -> f64 {
    (x * 1000.0).round() / 1000.0
}

fn round4(x: f64) -> f64 {
    (x * 10_000.0).round() / 10_000.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sessions_are_valid_and_deterministic() {
        let family = CrossFamily::qqq();
        let a = family.sample_set("t", 4, 9);
        let b = family.sample_set("t", 4, 9);
        assert_eq!(
            serde_json::to_string(&a).unwrap(),
            serde_json::to_string(&b).unwrap()
        );
        for scenario in &a {
            scenario.validate().unwrap();
            assert_eq!(scenario.markets.len(), 2);
            assert!(!scenario.markets[1].tape.is_empty());
        }
    }

    #[test]
    fn qqq_carries_each_move() {
        let family = CrossFamily::qqq();
        for (scenario, shape) in family.draw_set("t", 6, 3) {
            assert!((0.35..=0.75).contains(&shape.beta));
            let qqq = &scenario.markets[1];
            for (onset, ramp, size) in &shape.flow.moves {
                let at = |slot: u64| crate::making::level_at(&qqq.path, slot);
                let jump = at(onset + ramp + 2) - at(onset.saturating_sub(2));
                assert!(jump * size.signum() > 0.0, "{jump} vs {size}");
            }
        }
    }
}
