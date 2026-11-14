//! The levered-carry family: SPCX's perp trades at a persistent discount to
//! its index, so Phoenix's funding pays the longs, and a carry desk sizes a
//! long against the index's volatility and the desk's stop-out.
//!
//! - **The index** is drawn from SPCX's market calibration (GARCH-t on
//!   one-minute blocks, its fitted tails and smoothed mark), in regular
//!   hours, with two stated departures (see [`regimes`]): SPCX's fitted
//!   session-to-session volatility spread split into a calm and a stressed
//!   state, and their switching run at session timescale.
//! - **The discount** is drawn per session and drifts slowly around its
//!   level; in the stressed state the crowd leans in: it widens toward 1.5
//!   times its level and churns faster.
//! - **The flush.** At most once a session, the crowd that holds the
//!   discount sells into the book: it drops 1-3% within 2-10 seconds, the
//!   index with it, and the discount resets and rebuilds. Its odds per
//!   second are 120 times higher in the stressed state than in the calm one.
//! - **Funding** is Phoenix's, on the book's mark, with the session's
//!   interval, period and cap set on chain.
//!
//! The mandate is long-only (a carry book holds the side funding pays) with
//! a 700-share cap and a $1,000 loss limit that the grader's risk desk
//! enforces by closing the position at the market (`scoring.stop_out`).
//!
//! No tells: every session draws its states from the same chain, opens the
//! same way and runs the same tape; a flush can happen in either state; the
//! level draw overlaps the states.

use mtx_kit::Mandate;
use serde::Serialize;

use crate::{
    carry::BOOK_MARK,
    making::{TAKER_ACCOUNTS, TakerOrder, TakerSide},
    market::{
        Calibration, MarketModel, ModelSpec,
        calibration::{RegimeModel, Regimes, sha256_hex},
        model::Start,
    },
    scenario::{
        CloseMode, Funding, SLOT_SECONDS, Scenario, SplitMix64, StopOut, StopRule, VENUE, Waypoint,
    },
};

/// SPCX's market calibration, built in so hidden sets need no file.
const SPCX_CALIBRATION: &str =
    include_str!("../../../calibration/market/spcx.20261112.d1f48be2.toml");

/// Quote lots per base lot of one USD per SPCX: 10^6 quote lots per USD over
/// 10^3 base lots per SPCX.
const QUOTE_LOTS_PER_BASE_LOT_PER_USD: f64 = 1_000.0;

/// The stated regime chain: SPCX's one fitted state (its session level's
/// log-sd, 0.54) split into a calm and a stressed state 2x apart, each with
/// the rest of the spread as its own level draw, switching (with
/// [`REGIME_CLOCK`]) about every 12 minutes out of calm and every 5 out of
/// stress.
pub fn regimes() -> Regimes {
    Regimes {
        model: RegimeModel::Markov,
        names: vec!["calm".into(), "stressed".into()],
        vol_mult: vec![0.85, 1.7],
        log_sd_within: vec![0.4, 0.4],
        share: vec![0.7, 0.3],
        transition_per_hour: vec![vec![0.9, 0.1], vec![0.25, 0.75]],
        source: "stated for levered-carry-sizing-spcx: SPCX's fitted session level spread \
                 (log-sd 0.5425) split into two states 2x apart with log-sd 0.4 each, \
                 shares 70/30"
            .into(),
    }
}

/// How much faster than the stated hourly rates the states switch.
pub const REGIME_CLOCK: f64 = 51.0;

/// One session, beneath its path: what the ladder's told rungs are told.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Shape {
    /// The state from each slot on.
    pub regimes: Vec<(u64, String)>,
    /// The session's level in each state, as a multiple of the calibration's
    /// median session.
    pub regime_vol: Vec<f64>,
    /// The discount's level, in percent of the index.
    pub discount_pct: f64,
    pub funding: Funding,
    /// The cap on funding as a premium, in percent of the price.
    pub cap_pct: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub flush: Option<Flush>,
}

/// The session's flush.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct Flush {
    /// The slot it starts, the slot the book has fallen by, and how far, in
    /// percent.
    pub start: u64,
    pub end: u64,
    pub drop_pct: f64,
    /// The slot the discount is back to its new level.
    pub rebuilt: u64,
    pub discount_after_pct: f64,
}

#[derive(Debug, Clone)]
pub struct LeveredCarryFamily {
    pub symbol: String,
    /// The session's length, in slots, drawn first.
    pub duration: (u64, u64),
    /// The discount's level, in percent, log-uniform.
    pub discount_pct: (f64, f64),
    /// The discount's pull toward its target, as a time constant in
    /// seconds.
    pub discount_tau_seconds: f64,
    /// Its noise per second, as a share of its level, in calm and stress.
    pub discount_noise: [f64; 2],
    /// The stressed state's target, as a multiple of the level.
    pub stressed_widening: f64,
    /// Odds per second of a flush in calm and in stress.
    pub flush_per_second: [f64; 2],
    pub flush_drop_pct: (f64, f64),
    pub flush_seconds: (u64, u64),
    pub rebuild_seconds: (u64, u64),
    /// Slots between the path's waypoints.
    pub tick_gap: (u64, u64),
    pub funding_interval: (u64, u64),
    pub funding_period: (u64, u64),
    pub funding_cap_pct: (f64, f64),
    pub nominal_price_usd: f64,
    pub tape_gap: (u64, u64),
    pub tape_size_median: f64,
    pub tape_size_sigma: f64,
    pub tape_size_clamp: (f64, f64),
    pub tape_limit_bps: f64,
    pub max_position: f64,
    pub max_drawdown_usd: f64,
    /// The risk desk's largest order.
    pub stop_slice: f64,
}

impl LeveredCarryFamily {
    pub fn spcx() -> Self {
        Self {
            symbol: "SPCX".into(),
            duration: (1_200, 2_000),
            discount_pct: (0.3, 1.2),
            discount_tau_seconds: 60.0,
            discount_noise: [0.02, 0.05],
            stressed_widening: 1.5,
            flush_per_second: [0.0001, 0.012],
            flush_drop_pct: (1.0, 3.0),
            flush_seconds: (2, 10),
            rebuild_seconds: (30, 90),
            tick_gap: (4, 10),
            funding_interval: (2, 6),
            funding_period: (150, 600),
            funding_cap_pct: (0.6, 1.6),
            nominal_price_usd: 143.0,
            tape_gap: (3, 8),
            tape_size_median: 0.4,
            tape_size_sigma: 0.6,
            tape_size_clamp: (0.05, 2.0),
            tape_limit_bps: 60.0,
            // 10x the 10,000 USDC of collateral at the snapshot's ~$143.
            max_position: 700.0,
            max_drawdown_usd: 1_000.0,
            stop_slice: 100.0,
        }
    }

    /// The index's model: SPCX's calibration in regular hours, with the
    /// stated regime chain and clock.
    pub fn model() -> MarketModel {
        let mut calibration =
            Calibration::parse(SPCX_CALIBRATION).expect("SPCX's calibration is valid");
        calibration.sha256 = sha256_hex(SPCX_CALIBRATION.as_bytes());
        MarketModel::new(
            calibration,
            ModelSpec {
                regimes: Some(regimes()),
                regime_clock: REGIME_CLOCK,
                start: Start::Session {
                    kind: "regular".into(),
                },
                ..ModelSpec::default()
            },
        )
        .expect("the stated departures are valid")
    }

    pub fn sample(&self, name: &str, seed: u64) -> Scenario {
        self.draw(name, seed).0
    }

    pub fn draw(&self, name: &str, seed: u64) -> (Scenario, Shape) {
        self.draw_with(&Self::model(), name, seed)
    }

    fn draw_with(&self, model: &MarketModel, name: &str, seed: u64) -> (Scenario, Shape) {
        let mut rng = SplitMix64(seed);
        let duration = rng.range_u64(self.duration);
        let index = model.draw(rng.next_u64(), duration);
        let record = index.record.clone();
        let stressed_at = |slot: u64| -> bool {
            record
                .regimes
                .iter()
                .rev()
                .find(|m| m.slot <= slot)
                .is_some_and(|m| m.name == "stressed")
        };

        let interval = rng.range_u64(self.funding_interval);
        let period = rng.range_u64(self.funding_period).max(interval);
        let cap_pct = round3(rng.range_f64(self.funding_cap_pct));
        let max_rate = (cap_pct / 100.0
            * self.nominal_price_usd
            * QUOTE_LOTS_PER_BASE_LOT_PER_USD
            * interval as f64
            / period as f64)
            .round()
            .max(1.0) as i64;
        let funding = Funding {
            interval_seconds: interval,
            period_seconds: period,
            max_rate,
            mark: Some(BOOK_MARK),
        };
        let level = rng.log_range_f64(self.discount_pct);

        // Walk the ticks: the discount's drift, the flush's odds, and the
        // index the flush moves.
        let mut path = Vec::new();
        let mut discount = level;
        let mut flush: Option<Flush> = None;
        // The flush's lasting move on the index, in log terms, at its end.
        let mut flush_log = 0.0;
        let mut discount_before = level;
        let mut slot = 0;
        loop {
            let gap = rng.range_u64(self.tick_gap);
            slot += gap;
            if slot + 3 > duration {
                break;
            }
            let dt = gap as f64 * SLOT_SECONDS;
            let stressed = stressed_at(slot);
            let state = usize::from(stressed);
            if flush.is_none() {
                let odds = 1.0 - (-self.flush_per_second[state] * dt).exp();
                if rng.next_f64() < odds {
                    let drop_pct = rng.range_f64(self.flush_drop_pct);
                    let end = slot + rng.range_u64(self.flush_seconds) * 4;
                    let rebuilt = end + rng.range_u64(self.rebuild_seconds) * 4;
                    let after = rng.log_range_f64(self.discount_pct);
                    // The book falls `drop_pct` while the discount closes:
                    // the index falls by the drop and the discount.
                    flush_log = (1.0 - drop_pct / 100.0).ln() + (1.0 - discount / 100.0).ln();
                    discount_before = discount;
                    flush = Some(Flush {
                        start: slot,
                        end,
                        drop_pct: round3(drop_pct),
                        rebuilt,
                        discount_after_pct: round3(after),
                    });
                }
            }
            let (offset, book_discount) = match flush {
                Some(f) if slot >= f.start => {
                    if slot < f.end {
                        let x = (slot - f.start) as f64 / (f.end - f.start) as f64;
                        (flush_log * x, discount_before * (1.0 - x))
                    } else {
                        let x = ((slot - f.end) as f64 / (f.rebuilt - f.end) as f64).min(1.0);
                        discount = f.discount_after_pct * x;
                        (flush_log, discount)
                    }
                }
                _ => {
                    let target = if stressed {
                        level * self.stressed_widening
                    } else {
                        level
                    };
                    let noise = self.discount_noise[state] * level * dt.sqrt();
                    discount += (target - discount) * (dt / self.discount_tau_seconds).min(1.0)
                        + noise * rng.normal();
                    discount = discount.clamp(0.0, 2.5);
                    (0.0, discount)
                }
            };
            let level_pct = ((index.log_levels[slot as usize] + offset).exp() - 1.0) * 100.0;
            let waypoint = Waypoint {
                slot,
                level_pct: round3(level_pct),
                premium_pct: -round3(book_discount),
            };
            if path.last().is_some_and(|w: &Waypoint| {
                w.level_pct == waypoint.level_pct && w.premium_pct == waypoint.premium_pct
            }) {
                continue;
            }
            path.push(waypoint);
        }

        let tape = self.tape(&mut rng, duration);
        let mut mandate = Mandate::perp(
            VENUE,
            &self.symbol,
            self.max_position,
            self.max_drawdown_usd,
        );
        // A carry book holds the side funding pays: long against a discount.
        mandate.universe[0].min_position = Some(0.0);
        let shape = Shape {
            regimes: record
                .regimes
                .iter()
                .map(|m| (m.slot, m.name.clone()))
                .collect(),
            regime_vol: record.regime_vol.iter().map(|v| round3(*v)).collect(),
            discount_pct: round3(level),
            funding,
            cap_pct,
            flush,
        };
        let description = format!(
            "{}: a {:.2}% discount; funding every {interval} s over {period} s, capped at {cap_pct}%",
            self.symbol, level
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
            funding: Some(funding),
            tape,
            spot: None,
            faults: None,
            actors: None,
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
            market_model: Some(record),
            raw_sends: None,
            stop_out: Some(StopOut {
                on: vec![StopRule::Drawdown],
                close: CloseMode::Market,
                slice_base: Some(self.stop_slice),
            }),
        };
        (scenario, shape)
    }

    /// Noise takers, either side at random, from the shared pool: they keep
    /// the book's mark fresh.
    fn tape(&self, rng: &mut SplitMix64, duration: u64) -> Vec<TakerOrder> {
        let mut orders = Vec::new();
        let mut slot = rng.range_u64(self.tape_gap);
        while slot < duration {
            let side = if rng.next_f64() < 0.5 {
                TakerSide::Buy
            } else {
                TakerSide::Sell
            };
            let (lo, hi) = self.tape_size_clamp;
            let size =
                (self.tape_size_median * (self.tape_size_sigma * rng.normal()).exp()).clamp(lo, hi);
            orders.push(TakerOrder {
                slot,
                side,
                size: (size * 100.0).round() / 100.0,
                limit_bps: self.tape_limit_bps,
                account: (rng.next_u64() % TAKER_ACCOUNTS as u64) as u8,
            });
            slot += rng.range_u64(self.tape_gap);
        }
        orders
    }

    pub fn sample_set(&self, prefix: &str, count: usize, seed: u64) -> Vec<Scenario> {
        self.draw_set(prefix, count, seed)
            .into_iter()
            .map(|(scenario, _)| scenario)
            .collect()
    }

    /// `count` sessions named `<prefix>-<n>`.
    pub fn draw_set(&self, prefix: &str, count: usize, seed: u64) -> Vec<(Scenario, Shape)> {
        let model = Self::model();
        (0..count)
            .map(|i| {
                let draw_seed = seed.wrapping_mul(1_000_003).wrapping_add(i as u64 + 1);
                self.draw_with(&model, &format!("{prefix}-{}", i + 1), draw_seed)
            })
            .collect()
    }
}

fn round3(x: f64) -> f64 {
    (x * 1000.0).round() / 1000.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sessions_are_valid_and_deterministic() {
        let family = LeveredCarryFamily::spcx();
        for seed in 0..60 {
            let (scenario, shape) = family.draw("s", seed);
            assert_eq!(scenario.validate(), Ok(()), "seed {seed}");
            assert_eq!(scenario, family.sample("s", seed));
            assert!(!scenario.tape.is_empty());
            assert!(scenario.path.iter().all(|w| w.premium_pct <= 0.0));
            assert!(!shape.regimes.is_empty());
        }
    }

    /// Sessions see 0 to 2 switches, both states start sessions, and
    /// flushes come mostly, not only, in stress.
    #[test]
    fn states_switch_at_session_timescale_and_flushes_follow_stress() {
        let family = LeveredCarryFamily::spcx();
        let set = family.draw_set("h", 300, 5);
        let mut switches = [0usize; 4];
        let (mut starts_stressed, mut flushes, mut in_stress) = (0, 0, 0);
        for (_, shape) in &set {
            switches[(shape.regimes.len() - 1).min(3)] += 1;
            starts_stressed += usize::from(shape.regimes[0].1 == "stressed");
            if let Some(f) = shape.flush {
                flushes += 1;
                let state = shape.regimes.iter().rev().find(|(s, _)| *s <= f.start).unwrap();
                in_stress += usize::from(state.1 == "stressed");
            }
        }
        assert!(switches[0] > 100 && switches[1] > 40 && switches[2] > 5, "{switches:?}");
        assert!((50..150).contains(&starts_stressed), "{starts_stressed}");
        assert!((90..240).contains(&flushes), "{flushes}");
        assert!(in_stress * 10 > flushes * 7 && in_stress < flushes, "{in_stress}/{flushes}");
    }

    /// A flush drops the book by its size while the discount closes.
    #[test]
    fn a_flush_drops_the_book() {
        let family = LeveredCarryFamily::spcx();
        let mut seen = 0;
        for seed in 0..200 {
            let (scenario, shape) = family.draw("s", seed);
            let Some(f) = shape.flush else { continue };
            if f.end + 16 >= scenario.duration_slots || f.start < 8 {
                continue;
            }
            let book = |slot: u64| crate::making::book_at(&scenario.path, slot);
            let fell = book(f.start - 4) - book(f.end + 12);
            assert!(
                fell > 0.6 * f.drop_pct && fell < 1.6 * f.drop_pct + 0.5,
                "seed {seed}: {f:?} fell {fell}"
            );
            seen += 1;
        }
        assert!(seen > 20);
    }
}
