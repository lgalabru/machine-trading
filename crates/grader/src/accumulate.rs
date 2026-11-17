//! The accumulation family: a desk must buy a client's quantity of NVDA
//! perps by the close, against taker flow that may know where the price is
//! going, in a book that is thin near the touch.
//!
//! - **The price** walks NVDA's market calibration (GARCH-t on one-minute
//!   blocks, its fitted tails, volatility states and smoothed mark), inside
//!   regular hours.
//! - **The informed move.** A session is calm, or the price moves 2.5 to 4
//!   times the calibration's median jump up or down over a ramp of uneven
//!   steps, from an onset 15% to 70% into the session. Up and down are
//!   equally likely: a fixed schedule loses in one of them.
//! - **The flow.** Takers trade from a shared pool of accounts at a rate
//!   drawn per session, either side at random. Ahead of a move, a burst of
//!   orders trades its way; from the onset on, the noise leans its way.
//!   Fake-out bursts, drawn exactly like the informed ones, come in every
//!   regime and have nothing after them. Sizes, limits and accounts are
//!   drawn alike for all of them.
//! - **The book.** The snapshot's NVDA makers hold about 12 NVDA a tick, so
//!   they are widened to a backstop 15 to 30 bp out. Near the touch the
//!   depth is the grader's maker alone: a few units a side, requoted every
//!   few slots, leaning against what it holds. A buyer that lifts it faster
//!   than it recovers pays the backstop.
//! - **The order.** Buy `Q` by the close, never above the arrival price
//!   plus a cap, buys only; a broker buys whatever is left at the closing
//!   index plus a haircut (`order`, `mandate`).
//!
//! No tells: every regime runs the same machinery (the same actors, the same
//! bursts and fake-outs, the same order draws); only the move and the lean
//! differ.

use mtx_core::{Side, mandate::Mandate};
use serde::Serialize;

use crate::{
    making::{Competition, Making, Reactive, TAKER_ACCOUNTS, TakerOrder, TakerSide},
    market::{
        Calibration, MarketModel, ModelSpec,
        calibration::sha256_hex,
        model::{ModelPath, Start},
    },
    orders::ExecutionOrder,
    scenario::{Scenario, SplitMix64, VENUE},
};

/// Mixed into a session's seed for the stream its burst accounts are drawn
/// from.
const ACCOUNTS_STREAM: u64 = 0x5eed_acc0_0075;

/// NVDA's market calibration, built in so hidden sets need no file.
const NVDA_CALIBRATION: &str =
    include_str!("../../../calibration/market/nvda.20261113.903d1120.toml");

/// What a session is, beneath its path and flow: what the ladder's informed
/// rungs are told.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Shape {
    /// `calm`, `up` or `down`.
    pub regime: String,
    /// The move's start and its last step, in slots, and its size in percent
    /// (signed), in an informed session.
    pub onset: Option<u64>,
    pub end: Option<u64>,
    pub move_pct: Option<f64>,
    /// The first slot of each burst, informed first, and whether it is.
    pub bursts: Vec<(u64, bool)>,
    /// The taker account the informed trader sends from: seated in every
    /// session, informed or calm.
    pub informed_account: u8,
    /// The grader's maker: its half-spread, quote size and look delay, and
    /// the snapshot makers' backstop, in basis points.
    pub maker_bps: f64,
    pub maker_size: f64,
    pub react_slots: u64,
    pub backstop_bps: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Regime {
    Calm,
    Up,
    Down,
}

impl Regime {
    fn name(self) -> &'static str {
        match self {
            Self::Calm => "calm",
            Self::Up => "up",
            Self::Down => "down",
        }
    }

    fn sign(self) -> f64 {
        match self {
            Self::Calm => 0.0,
            Self::Up => 1.0,
            Self::Down => -1.0,
        }
    }
}

/// The family's parameters. Every range is inclusive.
#[derive(Debug, Clone)]
pub struct AccumulateFamily {
    pub symbol: String,
    /// The snapshot's mark, which the session starts at: the arrival price
    /// the cap is set from.
    pub arrival_usd: f64,
    pub duration: (u64, u64),
    /// Odds of a calm session; up and down share the rest.
    pub calm_odds: f64,
    /// The move, as a multiple of the calibration's median jump.
    pub move_mult: (f64, f64),
    /// Its onset, as a share of the session, and its length in slots.
    pub onset_share: (f64, f64),
    pub ramp_slots: (u64, u64),
    /// Slots between the path's waypoints.
    pub waypoint_slots: u64,
    /// Noise orders per slot, and their sizes: lognormal around a median,
    /// clamped.
    pub noise_rate: (f64, f64),
    pub size_median: f64,
    pub size_sigma: f64,
    pub size_clamp: (f64, f64),
    /// Taker limits, as multiples of the maker's half-spread: lognormal
    /// around 1.5, clamped.
    pub limit_sigma: f64,
    pub limit_clamp: (f64, f64),
    /// Orders in a burst, and how far ahead of its slot they start.
    pub burst_orders: (u64, u64),
    pub burst_lead: (u64, u64),
    /// Fake-out bursts a session, and where they fall, as a share of it.
    pub fakeouts: (u64, u64),
    pub fakeout_share: (f64, f64),
    /// How far the noise leans the move's way from its onset: the odds a
    /// noise order goes with it, above one half.
    pub lean: (f64, f64),
    /// The grader's maker: its half-spread, floor, quote size and look
    /// delay; and the snapshot makers' backstop, in basis points.
    pub maker_bps: (f64, f64),
    pub floor_bps: (f64, f64),
    pub maker_size: (f64, f64),
    pub react_slots: (u64, u64),
    pub backstop_bps: (f64, f64),
    /// The order: its quantity, the price cap over arrival in percent, and
    /// the broker's haircut in basis points.
    pub quantity: (u64, u64),
    pub cap_pct: (f64, f64),
    pub haircut_bps: (f64, f64),
    /// The mandate's loss limit, as a share of the order's notional.
    pub drawdown_share: f64,
}

impl AccumulateFamily {
    pub fn nvda() -> Self {
        Self {
            symbol: "NVDA".into(),
            arrival_usd: 159.92,
            duration: (1_200, 2_000),
            calm_odds: 0.3,
            move_mult: (2.5, 4.0),
            onset_share: (0.15, 0.70),
            ramp_slots: (40, 240),
            waypoint_slots: 4,
            noise_rate: (0.06, 0.15),
            size_median: 0.15,
            size_sigma: 0.7,
            size_clamp: (0.03, 2.0),
            limit_sigma: 0.5,
            limit_clamp: (0.5, 6.0),
            burst_orders: (6, 14),
            burst_lead: (20, 100),
            fakeouts: (0, 2),
            fakeout_share: (0.15, 0.85),
            lean: (0.10, 0.25),
            maker_bps: (3.0, 6.0),
            floor_bps: (1.0, 2.0),
            maker_size: (2.0, 5.0),
            react_slots: (2, 12),
            backstop_bps: (15.0, 30.0),
            // About 0.3x of the 10,000 USDC of collateral at the low end and
            // 0.7x at the top, at the snapshot's $159.92.
            quantity: (20, 45),
            cap_pct: (0.8, 2.0),
            haircut_bps: (10.0, 30.0),
            drawdown_share: 0.05,
        }
    }

    /// The price's model: NVDA's calibration inside regular hours.
    pub fn model() -> MarketModel {
        let mut calibration =
            Calibration::parse(NVDA_CALIBRATION).expect("NVDA's calibration is valid");
        calibration.sha256 = sha256_hex(NVDA_CALIBRATION.as_bytes());
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

    /// The regime a draw seed gives: read first, so a set can stratify.
    pub fn regime(&self, seed: u64) -> Regime {
        let draw = SplitMix64(seed).next_f64();
        if draw < self.calm_odds {
            Regime::Calm
        } else if draw < self.calm_odds + (1.0 - self.calm_odds) / 2.0 {
            Regime::Up
        } else {
            Regime::Down
        }
    }

    pub fn sample(&self, name: &str, seed: u64) -> Scenario {
        self.draw(name, seed).0
    }

    pub fn draw(&self, name: &str, seed: u64) -> (Scenario, Shape) {
        self.draw_with(&Self::model(), name, seed)
    }

    fn draw_with(&self, model: &MarketModel, name: &str, seed: u64) -> (Scenario, Shape) {
        let regime = self.regime(seed);
        let mut rng = SplitMix64(seed);
        rng.next_f64();
        let duration = rng.range_u64(self.duration);
        let mut index = model.draw(rng.next_u64(), duration);
        let median_jump_pct = model.calibration.fair_value.jumps.abs_size_median_pct;

        // The move: drawn in every regime, so the draws after it line up,
        // and laid on the path only in an informed one.
        let size_pct = rng.range_f64(self.move_mult) * median_jump_pct;
        let onset = (rng.range_f64(self.onset_share) * duration as f64) as u64;
        let ramp = rng.range_u64(self.ramp_slots);
        let steps = uneven_ramp(&mut rng, 8);
        let sign = regime.sign();
        let end = (onset + ramp).min(duration - 1);
        if sign != 0.0 {
            lay_move(&mut index, onset, ramp, sign * size_pct, &steps);
        }

        // The book: the grader's maker and the makers' backstop.
        let maker_bps = round2(rng.range_f64(self.maker_bps));
        let floor_bps = round2(rng.range_f64(self.floor_bps).min(0.5 * maker_bps));
        let maker_size = round2(rng.range_f64(self.maker_size));
        let react_slots = rng.range_u64(self.react_slots);
        let backstop_bps = round2(rng.range_f64(self.backstop_bps));

        // The flow: noise at the session's rate, leaning the move's way from
        // its onset; a burst ahead of the move; fake-outs in any regime.
        let rate = rng.range_f64(self.noise_rate);
        let lean = rng.range_f64(self.lean);
        let mut takers = Vec::new();
        for slot in 0..duration {
            if rng.next_f64() >= rate {
                continue;
            }
            let tilt = if slot >= onset { sign * lean } else { 0.0 };
            let side = if rng.next_f64() < 0.5 + tilt {
                TakerSide::Buy
            } else {
                TakerSide::Sell
            };
            takers.push(self.order(&mut rng, slot, side, maker_bps));
        }
        let mut bursts: Vec<(u64, f64, bool)> = Vec::new();
        if sign != 0.0 {
            bursts.push((onset, sign, true));
        }
        for _ in 0..rng.range_u64(self.fakeouts) {
            let at = (rng.range_f64(self.fakeout_share) * duration as f64) as u64;
            let side = if rng.next_f64() < 0.5 { 1.0 } else { -1.0 };
            bursts.push((at, side, false));
        }
        // Who sends each burst: one account a burst, the informed trader's
        // seated in every session (it trades noise like any account), each
        // fake-out from one of the others. Drawn on a stream of their own,
        // so the session's other draws don't move with them.
        let mut seats = SplitMix64(seed ^ ACCOUNTS_STREAM);
        let informed_account = (seats.next_u64() % u64::from(TAKER_ACCOUNTS)) as u8;
        let mut burst_starts = Vec::new();
        for &(at, side, informed) in &bursts {
            let account = if informed {
                informed_account
            } else {
                let other = seats.next_u64() % (u64::from(TAKER_ACCOUNTS) - 1);
                ((u64::from(informed_account) + 1 + other) % u64::from(TAKER_ACCOUNTS)) as u8
            };
            let lead = rng.range_u64(self.burst_lead).min(at.max(1));
            let mut first = at;
            for _ in 0..rng.range_u64(self.burst_orders) {
                let slot = at - rng.range_u64((1, lead.max(1))).min(at);
                first = first.min(slot);
                let side = if side > 0.0 {
                    TakerSide::Buy
                } else {
                    TakerSide::Sell
                };
                let order = self.order(&mut rng, slot, side, maker_bps);
                takers.push(TakerOrder { account, ..order });
            }
            burst_starts.push((first, informed));
        }
        // Stable: orders in one slot keep the order they were drawn in.
        takers.sort_by_key(|o| o.slot);

        // The order.
        let quantity = rng.range_u64(self.quantity) as f64;
        let cap_pct = round2(rng.range_f64(self.cap_pct));
        let haircut_bps = round2(rng.range_f64(self.haircut_bps));
        // A perp mandate has a loss limit; this one is out of reach of any
        // move the family makes, so it never decides a session: a buy
        // program's value moves with the market whatever the desk does.
        let max_drawdown = (quantity * self.arrival_usd * self.drawdown_share).round();
        let mut mandate = Mandate::perp(VENUE, &self.symbol, quantity, max_drawdown);
        let allowance = &mut mandate.universe[0];
        allowance.min_position = Some(0.0);
        allowance.sides = Some(vec![Side::Buy]);
        allowance.max_buy_price = Some(round2(self.arrival_usd * (1.0 + cap_pct / 100.0)));

        let record = index.record.clone();
        let path = index.waypoints(self.waypoint_slots);
        let shape = Shape {
            regime: regime.name().into(),
            onset: (sign != 0.0).then_some(onset),
            end: (sign != 0.0).then_some(end),
            move_pct: (sign != 0.0).then_some(round3(sign * size_pct)),
            bursts: burst_starts,
            informed_account,
            maker_bps,
            maker_size,
            react_slots,
            backstop_bps,
        };
        let description = format!(
            "{}: buy {quantity} by the close, at most {cap_pct}% over arrival; the broker takes the rest at {haircut_bps} bp",
            self.symbol
        );
        let scenario = Scenario {
            name: name.to_string(),
            description,
            symbol: self.symbol.clone(),
            duration_slots: duration,
            mandate,
            path,
            making: Some(Making {
                competitor_bps: maker_bps,
                competition: Competition {
                    floor_bps,
                    react_slots,
                    size: maker_size,
                    reactive: Some(Reactive::default()),
                },
                takers,
                makers_bps: Some(backstop_bps),
                roles: [("informed".to_string(), vec![informed_account])].into(),
            }),
            offhours: None,
            funding: None,
            tape: Vec::new(),
            spot: None,
            faults: None,
            actors: None,
            takers_pricing: None,
            turn_data: None,
            oracles: None,
            lending: None,
            warps: Vec::new(),
            order: Some(ExecutionOrder {
                instrument: None,
                side: Side::Buy,
                quantity,
                haircut_bps,
                reduce_only: false,
            }),
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
            markets: Vec::new(),
            joint: None,
        };
        (scenario, shape)
    }

    /// A taker order: size, limit and account drawn alike for noise, bursts
    /// and fake-outs.
    fn order(
        &self,
        rng: &mut SplitMix64,
        slot: u64,
        side: TakerSide,
        maker_bps: f64,
    ) -> TakerOrder {
        let (lo, hi) = self.size_clamp;
        let size = (self.size_median * (self.size_sigma * rng.normal()).exp()).clamp(lo, hi);
        let (llo, lhi) = self.limit_clamp;
        let limit = 1.5 * maker_bps * (self.limit_sigma * rng.normal()).exp().clamp(llo, lhi);
        TakerOrder {
            slot,
            side,
            size: round3(size),
            limit_bps: round2(limit),
            account: rng.range_u64((0, TAKER_ACCOUNTS as u64 - 1)) as u8,
        }
    }

    pub fn sample_set(&self, prefix: &str, count: usize, seed: u64) -> Vec<Scenario> {
        self.draw_set(prefix, count, seed)
            .into_iter()
            .map(|(scenario, _)| scenario)
            .collect()
    }

    /// `count` sessions named `<prefix>-<n>`, each regime in proportion to
    /// its odds (largest remainder), up and down mirrored.
    pub fn draw_set(&self, prefix: &str, count: usize, seed: u64) -> Vec<(Scenario, Shape)> {
        let model = Self::model();
        let calm = (self.calm_odds * count as f64).round() as usize;
        let up = (count - calm) / 2;
        let mut quotas = [calm, up, count - calm - up];
        let mut out = Vec::with_capacity(count);
        let mut draw = 0u64;
        while out.len() < count {
            draw += 1;
            let draw_seed = seed.wrapping_mul(1_000_003).wrapping_add(draw);
            let slot = match self.regime(draw_seed) {
                Regime::Calm => 0,
                Regime::Up => 1,
                Regime::Down => 2,
            };
            if quotas[slot] == 0 {
                continue;
            }
            quotas[slot] -= 1;
            let name = format!("{prefix}-{}", out.len() + 1);
            out.push(self.draw_with(&model, &name, draw_seed));
        }
        out
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
        let family = AccumulateFamily::nvda();
        for seed in 0..60 {
            let (scenario, shape) = family.draw("s", seed);
            assert_eq!(scenario.validate(), Ok(()), "seed {seed}");
            assert_eq!(scenario, family.sample("s", seed));
            let order = scenario.order.as_ref().unwrap();
            assert_eq!(scenario.mandate.universe[0].max_position, order.quantity);
            let limit = scenario.mandate.limits.max_drawdown_usd.unwrap();
            assert!(limit > order.quantity * 159.92 * 0.04, "{limit}");
            assert_eq!(shape.regime == "calm", shape.onset.is_none());
        }
    }

    /// Sets are stratified and mirrored, and every regime runs the same
    /// machinery: bursts and fake-outs in each.
    #[test]
    fn sets_are_stratified_mirrored_and_alike() {
        let family = AccumulateFamily::nvda();
        let set = family.draw_set("h", 100, 3);
        let count = |r: &str| set.iter().filter(|(_, s)| s.regime == r).count();
        assert_eq!((count("calm"), count("up"), count("down")), (30, 35, 35));
        let fakeouts = |r: &str| {
            set.iter()
                .filter(|(_, s)| s.regime == r)
                .map(|(_, s)| s.bursts.iter().filter(|(_, i)| !i).count())
                .sum::<usize>()
        };
        assert!(fakeouts("calm") > 10 && fakeouts("up") > 10 && fakeouts("down") > 10);
    }

    /// The move lands: an up session's path ends higher than it was at the
    /// onset by about the move, give or take the walk.
    #[test]
    fn the_move_lands_its_way() {
        let family = AccumulateFamily::nvda();
        let set = family.draw_set("h", 60, 9);
        let mut along = 0;
        let mut informed = 0;
        for (scenario, shape) in &set {
            let (Some(onset), Some(end), Some(size)) = (shape.onset, shape.end, shape.move_pct)
            else {
                continue;
            };
            informed += 1;
            let at = |slot: u64| crate::making::level_at(&scenario.path, slot);
            let moved = at(end + 8) - at(onset.saturating_sub(4));
            if moved * size > 0.0 && moved.abs() > 0.4 * size.abs() {
                along += 1;
            }
        }
        assert!(along * 10 >= informed * 8, "{along} of {informed}");
    }

    /// The backstop is the snapshot makers' own widening, apart from the
    /// grader's maker's half-spread; it round-trips through v2 and can't
    /// sit inside the maker.
    #[test]
    fn the_backstop_round_trips_and_sits_behind_the_maker() {
        let scenario = AccumulateFamily::nvda().sample("s", 4);
        let making = scenario.making.clone().unwrap();
        assert!(making.makers_bps() > making.competitor_bps);
        let v2 = crate::scenario_v2::ScenarioV2::try_from(&scenario).unwrap();
        assert_eq!(v2.into_scenario().unwrap(), scenario);
        let inside = Scenario {
            making: Some(Making {
                makers_bps: Some(making.competitor_bps / 2.0),
                ..making.clone()
            }),
            ..scenario.clone()
        };
        assert!(inside.validate().unwrap_err().contains("makers_bps"));
        let none = Making {
            makers_bps: None,
            ..making
        };
        assert_eq!(none.makers_bps(), none.competitor_bps);
    }
}
