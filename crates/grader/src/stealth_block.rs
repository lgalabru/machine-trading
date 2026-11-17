//! The stealth-or-block family: TSLA's opening half hour, where a fast news
//! reader trades ahead of the opening jumps either in small clips or in
//! large blocks, and a rebalancer works the other style.
//!
//! - **The price** walks TSLA's market calibration from 09:31 New York on a
//!   drawn trading day, at the opening hour's measured volatility, jump rate
//!   and jump sizes (`JUMP_REGIME_MEASUREMENT.md` Table 2 and its caveat).
//!   Sessions come in pairs: the second of a pair runs the first's path
//!   mirrored, with the other class informed.
//! - **The informed** (the library's [`Informed`](crate::actors::library::Informed))
//!   see the path `horizon_slots` ahead and take its way while it is more
//!   than `edge_bps` off. The session's class regime says their size: small
//!   clips (`stealth`) or large blocks (`block`).
//! - **The rebalancers** are faded decoys from the first slot: one-sided
//!   bursts on random views, at the informed's pace and rhythm. One works
//!   the informed's own size class at half the informed's activity, the
//!   other the other class at one and a half times it, so each class holds
//!   the same directional flow whichever is informed: trade counts and sizes
//!   by class don't change with the regime, only what the price does after.
//! - **Chasers** take the way the book moved once it moved over
//!   `trigger_bps`, **noise** takes at random at the calibrated size, and a
//!   **refill** puts back the depth takes consume.
//!
//! The informed, the rebalancers and the noise share one wallet pool, so an
//! address says nothing. The informed bot is told the class regime (a fact
//! of the session's process), never the path, the jumps, a side or a
//! wallet.

use std::collections::BTreeMap;

use mtx_core::mandate::Mandate;
use serde::Serialize;
use serde_json::json;

use crate::{
    actors::Library,
    market::{
        Calibration, MarketModel, ModelSpec,
        calibration::sha256_hex,
        model::{EventKind, JumpSize, ModelPath, Start},
    },
    orders::CloseOut,
    scenario::{Scenario, SplitMix64, VENUE},
};

/// TSLA's market calibration, built in so hidden sets need no file.
const TSLA_CALIBRATION: &str =
    include_str!("../../../calibration/market/tsla.20261113.8b7bca8e.toml");

const JUMP_SOURCE: &str = "JUMP_REGIME_MEASUREMENT.md Table 2: Phoenix 1 m marks, 09:31-10:30, \
     calibration 5-sigma rule; stated divided by vol_mult 2.2, which still scales stated sizes";

/// Which size class the informed trade in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Class {
    /// Small clips: stealth.
    Small,
    /// Large blocks.
    Large,
}

impl Class {
    fn other(self) -> Self {
        match self {
            Self::Small => Self::Large,
            Self::Large => Self::Small,
        }
    }
}

/// A session's draws beneath its path and tape: what the ladder's oracles
/// may be told. The informed bot is told only [`Shape::informed_class`].
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Shape {
    pub informed_class: Class,
    /// The session's start, Unix seconds (09:31 New York).
    pub start_unix: i64,
    pub mirrored: bool,
    pub horizon_slots: u64,
    pub edge_bps: f64,
    /// The share of slots the informed see a move past their edge, and the
    /// mean length of those runs: what the rebalancers' rhythm is set from.
    pub informed_duty: f64,
    pub informed_run_slots: f64,
    pub noise_wallets: u32,
    pub trigger_bps: f64,
    pub refill_half_life: f64,
    /// The path's jumps: slot and log move in percent. The session's
    /// future: never told to the informed bot.
    pub jumps: Vec<(u64, f64)>,
}

/// The family's parameters. Every range is inclusive.
#[derive(Debug, Clone)]
pub struct StealthBlockFamily {
    pub symbol: String,
    pub duration_slots: u64,
    /// The opening window's stated departures from the calibration.
    pub vol_mult: f64,
    pub jump_rate_per_hour: f64,
    pub jump_median_pct: f64,
    pub jump_p90_pct: f64,
    pub waypoint_slots: u64,
    /// The calibration's taker pace times the opening hour's share of the
    /// day's trades (hour 9 over 1/24).
    pub hour_scale: f64,
    /// Every actor's sizes over the calibration's (Phoenix trades TSLA in
    /// 0.001 lots; the calibrated median is 0.008).
    pub size_scale: f64,
    /// The small and large classes' sizes, as multiples of the median.
    pub small_size: f64,
    pub large_size: f64,
    pub informed_wallets: u32,
    pub informed_intensity: f64,
    pub horizon_slots: (u64, u64),
    pub edge_bps: (f64, f64),
    /// The rebalancers' activity, as multiples of the informed's: in the
    /// informed's class and in the other.
    pub own_class_hedger: f64,
    pub other_class_hedger: f64,
    pub noise_wallets: (u64, u64),
    pub noise_flow: f64,
    pub chaser_wallets: u32,
    pub chaser_intensity: f64,
    pub trigger_bps: (f64, f64),
    pub refill_half_life: (f64, f64),
    pub pool_wallets: u32,
    pub limit_bps: f64,
    pub max_position: f64,
    pub max_drawdown_usd: f64,
    pub close_haircut_bps: f64,
}

impl StealthBlockFamily {
    /// TSLA perps, `stealth-or-block-tsla`.
    pub fn tsla() -> Self {
        Self {
            symbol: "TSLA".into(),
            duration_slots: 3_000,
            vol_mult: 2.2,
            jump_rate_per_hour: 3.9,
            jump_median_pct: 0.382,
            jump_p90_pct: 0.652,
            waypoint_slots: 2,
            hour_scale: 1.28,
            size_scale: 5.0,
            small_size: 0.5,
            large_size: 4.0,
            informed_wallets: 2,
            informed_intensity: 16.0,
            horizon_slots: (60, 150),
            edge_bps: (20.0, 30.0),
            own_class_hedger: 0.5,
            other_class_hedger: 1.5,
            noise_wallets: (3, 5),
            noise_flow: 4.0,
            chaser_wallets: 2,
            chaser_intensity: 1.0,
            trigger_bps: (15.0, 25.0),
            refill_half_life: (4.0, 40.0),
            pool_wallets: 8,
            limit_bps: 50.0,
            max_position: 10.0,
            max_drawdown_usd: 250.0,
            close_haircut_bps: 5.0,
        }
    }

    /// The price's model: TSLA's calibration with the opening window's
    /// volatility, jump rate and jump sizes.
    pub fn model(&self) -> MarketModel {
        let mut calibration =
            Calibration::parse(TSLA_CALIBRATION).expect("TSLA's calibration is valid");
        calibration.sha256 = sha256_hex(TSLA_CALIBRATION.as_bytes());
        let size = |pct: f64| (pct / self.vol_mult * 10_000.0).round() / 10_000.0;
        MarketModel::new(
            calibration,
            ModelSpec {
                vol_mult: self.vol_mult,
                start: Start::Session {
                    kind: "regular".into(),
                },
                jumps: true,
                jump_rate_per_hour: Some(self.jump_rate_per_hour),
                jump_size: Some(JumpSize {
                    abs_median_pct: size(self.jump_median_pct),
                    abs_p90_pct: size(self.jump_p90_pct),
                    source: JUMP_SOURCE.into(),
                }),
                ..ModelSpec::default()
            },
        )
        .expect("the model's spec is valid")
    }

    fn venue(model: &MarketModel) -> (f64, f64, f64) {
        let venue = model
            .calibration
            .venue("phoenix_perp")
            .expect("TSLA's calibration has its Phoenix perp");
        (
            venue.trades_per_minute,
            venue.trade_size.median,
            venue.trade_size.sigma,
        )
    }

    /// A trading day's 09:31 New York, drawn from the calendar's range.
    fn opening(model: &MarketModel, rng: &mut SplitMix64) -> i64 {
        let calendar = model.calendar();
        let (from, to) = calendar.draw_days();
        let span = ((to - from + 1) * 86_400) as u64;
        let any = from * 86_400 + (rng.next_u64() % span) as i64;
        let (open, _) = calendar
            .next_session("regular", any)
            .expect("the regular session recurs within a year");
        open + 60
    }

    pub fn sample(&self, name: &str, seed: u64) -> Scenario {
        self.draw_set(name, 1, seed).remove(0).0
    }

    /// `count` sessions named `<prefix>-<n>`: pairs, the second of each the
    /// first's path mirrored with the other class informed.
    pub fn draw_set(&self, prefix: &str, count: usize, seed: u64) -> Vec<(Scenario, Shape)> {
        let model = self.model();
        let mut out = Vec::with_capacity(count);
        let mut n = 1u64;
        while out.len() < count {
            let draw_seed = seed.wrapping_mul(1_000_003).wrapping_add(n);
            let mut rng = SplitMix64(draw_seed);
            let start_unix = Self::opening(&model, &mut rng);
            let mut at = model.clone();
            at.spec.start = Start::At { unix: start_unix };
            let path = at.draw(rng.next_u64(), self.duration_slots);
            let class = if rng.next_f64() < 0.5 {
                Class::Small
            } else {
                Class::Large
            };
            let first = self.build(
                &model,
                &format!("{prefix}-{}", out.len() + 1),
                &mut rng,
                path.clone(),
                class,
                start_unix,
                false,
            );
            out.push(first);
            if out.len() < count {
                let mut mirror = SplitMix64(draw_seed ^ 0x6d69_7272_6f72);
                let mut flipped = path;
                mirror_path(&mut flipped);
                let second = self.build(
                    &model,
                    &format!("{prefix}-{}", out.len() + 1),
                    &mut mirror,
                    flipped,
                    class.other(),
                    start_unix,
                    true,
                );
                out.push(second);
            }
            n += 1;
        }
        out
    }

    #[allow(clippy::too_many_arguments)]
    fn build(
        &self,
        model: &MarketModel,
        name: &str,
        rng: &mut SplitMix64,
        index: ModelPath,
        class: Class,
        start_unix: i64,
        mirrored: bool,
    ) -> (Scenario, Shape) {
        let duration = self.duration_slots;
        let horizon_slots = rng.range_u64(self.horizon_slots);
        let edge_bps = round2(rng.range_f64(self.edge_bps));
        let noise_wallets = rng.range_u64(self.noise_wallets) as u32;
        let trigger_bps = round2(rng.range_f64(self.trigger_bps));
        let half_life = round2(rng.range_f64(self.refill_half_life));
        let (duty, run) = informed_activity(&index, horizon_slots, edge_bps);

        let (trades_per_minute, median, sigma) = Self::venue(model);
        let size_of = |c: Class| match c {
            Class::Small => self.small_size,
            Class::Large => self.large_size,
        };
        let hedger = |share: f64, c: Class| {
            let (length, gap) = rhythm(duty, run, share, horizon_slots);
            json!({
                "wallets": 1,
                "horizon_slots": horizon_slots,
                "edge_bps": edge_bps,
                "informed_for": { "from": 0, "until": 1 },
                "bursts": { "length_slots": length, "gap_slots": gap },
                "intensity_mult": self.informed_intensity,
                "size_mult": size_of(c),
                "limit_bps": self.limit_bps,
            })
        };
        let actors: Library = serde_json::from_value(json!({
            "scale": {
                "trades_per_minute": round4(trades_per_minute * self.hour_scale),
                "trade_size": round4(median * self.size_scale),
                "trade_size_sigma": sigma,
            },
            "noise": [{
                "wallets": noise_wallets,
                "flow_mult": self.noise_flow,
                "limit_bps": self.limit_bps,
            }],
            "informed": [{
                "wallets": self.informed_wallets,
                "horizon_slots": horizon_slots,
                "edge_bps": edge_bps,
                "intensity_mult": self.informed_intensity,
                "size_mult": size_of(class),
                "limit_bps": self.limit_bps,
            }],
            "decoys": [
                hedger(self.own_class_hedger, class),
                hedger(self.other_class_hedger, class.other()),
            ],
            "chasers": [{
                "wallets": self.chaser_wallets,
                "trigger_bps": trigger_bps,
                "intensity_mult": self.chaser_intensity,
                "limit_bps": self.limit_bps,
            }],
            "refill": { "half_life_slots": [half_life, half_life], "levels": 5 },
            "pool": {
                "wallets": self.pool_wallets,
                "roles": ["noise", "informed", "decoys"],
            },
        }))
        .expect("the family's actors are a valid library");

        let jumps = index
            .record
            .events
            .iter()
            .filter(|e| e.kind == EventKind::Jump)
            .map(|e| (e.slot, round3(e.pct)))
            .collect();
        let shape = Shape {
            informed_class: class,
            start_unix,
            mirrored,
            horizon_slots,
            edge_bps,
            informed_duty: round4(duty),
            informed_run_slots: round2(run),
            noise_wallets,
            trigger_bps,
            refill_half_life: half_life,
            jumps,
        };
        let description = format!(
            "{}: trade the perp off the tape in the opening half hour, within {} {} and a ${} loss limit",
            self.symbol, self.max_position, self.symbol, self.max_drawdown_usd
        );
        let record = index.record.clone();
        let scenario = Scenario {
            name: name.to_string(),
            description,
            symbol: self.symbol.clone(),
            duration_slots: duration,
            mandate: Mandate::perp(
                VENUE,
                &self.symbol,
                self.max_position,
                self.max_drawdown_usd,
            ),
            path: index.waypoints(self.waypoint_slots),
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

    pub fn sample_set(&self, prefix: &str, count: usize, seed: u64) -> Vec<Scenario> {
        self.draw_set(prefix, count, seed)
            .into_iter()
            .map(|(scenario, _)| scenario)
            .collect()
    }
}

/// The share of slots the informed see a move past `edge_bps` within
/// `horizon` slots, and the mean length of those runs.
fn informed_activity(path: &ModelPath, horizon: u64, edge_bps: f64) -> (f64, f64) {
    let levels = &path.log_levels;
    let last = levels.len() - 1;
    let (mut active, mut runs, mut was) = (0u64, 0u64, false);
    for t in 0..last {
        let ahead = levels[(t + horizon as usize).min(last)];
        let on = ((ahead - levels[t]).exp() - 1.0).abs() * 10_000.0 > edge_bps;
        if on {
            active += 1;
            if !was {
                runs += 1;
            }
        }
        was = on;
    }
    let duty = active as f64 / last as f64;
    let run = if runs == 0 {
        horizon as f64 / 2.0
    } else {
        active as f64 / runs as f64
    };
    (duty, run)
}

/// A rebalancer's bursts: as long as the informed's runs, spaced so it is
/// active `share` times as often as they are. A session the informed sit
/// out keeps a floor of activity, a twentieth of the session.
fn rhythm(duty: f64, run: f64, share: f64, horizon: u64) -> ([u64; 2], [u64; 2]) {
    let run = run.max(4.0).min(horizon as f64 * 2.0);
    let duty = (duty.max(0.05) * share).min(0.6);
    let gap = run * (1.0 - duty) / duty;
    let length = [(run * 0.5).round().max(2.0) as u64, (run * 1.5).round() as u64];
    let gap = [(gap * 0.5).round().max(1.0) as u64, (gap * 1.5).round().max(2.0) as u64];
    (length, gap)
}

/// The same path, every move the other way.
fn mirror_path(path: &mut ModelPath) {
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

fn round3(x: f64) -> f64 {
    (x * 1_000.0).round() / 1_000.0
}

fn round4(x: f64) -> f64 {
    (x * 10_000.0).round() / 10_000.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sessions_are_valid_deterministic_and_paired() {
        let family = StealthBlockFamily::tsla();
        let a = family.draw_set("t", 6, 9);
        let b = family.draw_set("t", 6, 9);
        assert_eq!(
            serde_json::to_string(&a).unwrap(),
            serde_json::to_string(&b).unwrap()
        );
        for pair in a.chunks(2) {
            let (first, second) = (&pair[0], &pair[1]);
            assert_ne!(first.1.informed_class, second.1.informed_class);
            assert!(!first.1.mirrored && second.1.mirrored);
            assert_eq!(first.1.start_unix, second.1.start_unix);
            for (x, y) in first.0.path.iter().zip(&second.0.path) {
                assert_eq!(x.slot, y.slot);
                assert!((x.level_pct + y.level_pct).abs() < 0.01 + x.level_pct.abs() * 0.01);
            }
        }
        for (scenario, _) in &a {
            scenario.validate().unwrap();
            assert_eq!(scenario.valuation(), "oracle");
            scenario.actors.as_ref().unwrap().validate().unwrap();
        }
    }

    /// The class's directional flow is the same whichever class is
    /// informed: 1 + 0.5 in the informed's class and 1.5 in the other.
    #[test]
    fn each_class_holds_the_same_directional_activity() {
        let family = StealthBlockFamily::tsla();
        assert!(
            (1.0 + family.own_class_hedger - family.other_class_hedger).abs() < 1e-9,
            "counts by class would read the regime"
        );
        for (scenario, shape) in family.draw_set("t", 8, 4) {
            let actors = serde_json::to_value(scenario.actors.unwrap()).unwrap();
            let informed = actors["informed"][0]["size_mult"].as_f64().unwrap();
            let own = actors["decoys"][0]["size_mult"].as_f64().unwrap();
            let other = actors["decoys"][1]["size_mult"].as_f64().unwrap();
            assert_eq!(informed, own);
            assert_ne!(informed, other);
            let small = shape.informed_class == Class::Small;
            assert_eq!(informed < other, small);
        }
    }

    #[test]
    fn sessions_start_at_09_31_new_york_on_a_trading_day() {
        let family = StealthBlockFamily::tsla();
        let model = family.model();
        for (_, shape) in family.draw_set("t", 10, 2) {
            assert_eq!(
                model.calendar().kind_at(shape.start_unix),
                Some("regular")
            );
            assert_ne!(model.calendar().kind_at(shape.start_unix - 61), Some("regular"));
        }
    }
}
