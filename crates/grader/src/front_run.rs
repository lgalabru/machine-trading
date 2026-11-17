//! The front-run family: a desk must sell an inherited long in SPY perps by
//! the close, while a predator watches its position and trades ahead of a
//! seller it has found.
//!
//! - **The price** walks SPY's market calibration inside regular hours.
//! - **The book.** The snapshot's SPY makers are widened to a backstop well
//!   out; near the touch the depth is the grader's maker alone, a small
//!   quote a side, requoted every few slots and leaning against what it
//!   holds. Selling at its bid is cheaper than the broker; selling into the
//!   backstop is dearer.
//! - **The flow.** Takers trade small orders either way from the grader's
//!   taker accounts.
//! - **The predator** (the library's
//!   [`FrontRunner`](crate::actors::library::FrontRunner)) reads the bot's
//!   position step by step. Once the bot's net selling over its window
//!   reaches its threshold, it has found a seller: it stays on it for a
//!   while, selling ahead of each of the bot's turns into the maker's bid,
//!   and buys back later. Its type is drawn per session from a fixed menu
//!   ([`PREDATORS`]); one of them never trades ahead of anyone.
//! - **The order.** The desk starts long `Q` and must be flat by the close,
//!   sells only; a broker sells whatever is left at the closing index less
//!   a haircut (`order`, `mandate`).
//!
//! No tells: every session seats the predator from a fresh wallet of the
//! session's keyring, whatever its type. Which type it is and its wallet are
//! the session's latent identity, told to the informed bot (`crate::latent`);
//! each type's settings are fixed process facts of the menu.

use std::collections::BTreeMap;

use mtx_core::{Side, mandate::Mandate};
use serde::Serialize;
use serde_json::json;

use crate::{
    actors::Library,
    making::{Competition, Making, Reactive, TAKER_ACCOUNTS, TakerOrder, TakerSide},
    market::{
        Calibration, MarketModel, ModelSpec,
        calibration::sha256_hex,
        model::Start,
    },
    orders::ExecutionOrder,
    scenario::{Scenario, SplitMix64, VENUE},
};

/// SPY's market calibration, built in so hidden sets need no file.
const SPY_CALIBRATION: &str =
    include_str!("../../../calibration/market/spy.20261113.2b088307.toml");

/// One kind of predator: how it reads the bot and how it trades ahead.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct Predator {
    pub name: &'static str,
    /// The steps whose net selling it reads, and the net, in the library's
    /// trade sizes, that makes the bot a seller to it.
    pub window_steps: u64,
    pub min_mult: f64,
    /// What it sells ahead each turn, as a share of what the bot's pace
    /// over the window says it will sell while a sale is held; how long it
    /// stays on a seller it has found, and holds each sale, in steps.
    pub share: f64,
    pub persist_steps: u64,
    pub hold_steps: u64,
}

/// The menu a session's predator is drawn from, with equal odds. A
/// `dormant` one is seated and funded like the others and never trades
/// ahead.
pub const PREDATORS: &[Predator] = &[
    Predator {
        name: "hair-trigger",
        window_steps: 8,
        min_mult: 1.0,
        share: 1.2,
        persist_steps: 80,
        hold_steps: 20,
    },
    Predator {
        name: "quick",
        window_steps: 12,
        min_mult: 2.0,
        share: 1.2,
        persist_steps: 100,
        hold_steps: 30,
    },
    Predator {
        name: "steady",
        window_steps: 20,
        min_mult: 6.0,
        share: 1.2,
        persist_steps: 120,
        hold_steps: 40,
    },
    Predator {
        name: "patient",
        window_steps: 30,
        min_mult: 16.0,
        share: 1.2,
        persist_steps: 150,
        hold_steps: 50,
    },
    Predator {
        name: "dormant",
        window_steps: 1,
        min_mult: 1.0e6,
        share: 1.2,
        persist_steps: 0,
        hold_steps: 1,
    },
];

/// A session's draws beneath its path and book.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Shape {
    /// The predator's type: a [`PREDATORS`] name.
    pub predator: String,
    /// The order's quantity and the broker's haircut.
    pub quantity: f64,
    pub haircut_bps: f64,
    /// The grader's maker: half-spread, quote size and look delay; the
    /// backstop, in basis points.
    pub maker_bps: f64,
    pub maker_size: f64,
    pub react_slots: u64,
    pub backstop_bps: f64,
}

/// The family's parameters. Every range is inclusive.
#[derive(Debug, Clone)]
pub struct FrontRunFamily {
    pub symbol: String,
    /// The snapshot's mark, which sessions start at.
    pub arrival_usd: f64,
    pub calibration: &'static str,
    pub duration: (u64, u64),
    pub waypoint_slots: u64,
    /// The library's trade size, in SPY: the predator's unit.
    pub trade_size: f64,
    pub trade_size_sigma: f64,
    pub trades_per_minute: f64,
    /// The predator's reach past the book's price, in basis points, and
    /// the most it holds, in trade sizes.
    pub predator_limit_bps: f64,
    pub predator_cap_mult: f64,
    /// Taker orders a slot, and their sizes: lognormal around a median,
    /// clamped; their limits, as multiples of the maker's half-spread.
    pub noise_rate: (f64, f64),
    pub size_median: f64,
    pub size_sigma: f64,
    pub size_clamp: (f64, f64),
    pub limit_sigma: f64,
    pub limit_clamp: (f64, f64),
    /// The grader's maker and the backstop.
    pub maker_bps: (f64, f64),
    pub floor_bps: (f64, f64),
    pub maker_size: (f64, f64),
    pub react_slots: (u64, u64),
    pub backstop_bps: (f64, f64),
    /// The order: its quantity, and the broker's haircut in basis points.
    pub quantity: (u64, u64),
    pub haircut_bps: (f64, f64),
    /// The mandate's loss limit, as a share of the order's notional.
    pub drawdown_share: f64,
}

impl FrontRunFamily {
    /// SPY perps, `front-run-aware-unwind-spy`.
    pub fn spy() -> Self {
        Self {
            symbol: "SPY".into(),
            arrival_usd: 750.47,
            calibration: SPY_CALIBRATION,
            duration: (1_000, 1_400),
            waypoint_slots: 4,
            trade_size: 0.05,
            trade_size_sigma: 0.6,
            trades_per_minute: 1.235,
            predator_limit_bps: 12.0,
            predator_cap_mult: 300.0,
            noise_rate: (0.04, 0.08),
            size_median: 0.03,
            size_sigma: 0.6,
            size_clamp: (0.005, 0.3),
            limit_sigma: 0.5,
            limit_clamp: (0.5, 4.0),
            maker_bps: (2.0, 4.0),
            floor_bps: (1.0, 1.5),
            maker_size: (0.15, 0.3),
            react_slots: (2, 6),
            backstop_bps: (30.0, 45.0),
            quantity: (6, 14),
            haircut_bps: (12.0, 25.0),
            drawdown_share: 0.05,
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

    /// The predator type a draw seed gives: read first, so a set can
    /// stratify.
    pub fn predator(seed: u64) -> usize {
        (SplitMix64(seed).next_u64() % PREDATORS.len() as u64) as usize
    }

    pub fn sample(&self, name: &str, seed: u64) -> Scenario {
        self.draw(name, seed).0
    }

    pub fn draw(&self, name: &str, seed: u64) -> (Scenario, Shape) {
        self.draw_with(&self.model(), name, seed)
    }

    fn draw_with(&self, model: &MarketModel, name: &str, seed: u64) -> (Scenario, Shape) {
        let predator = PREDATORS[Self::predator(seed)];
        let mut rng = SplitMix64(seed);
        rng.next_u64();
        let duration = rng.range_u64(self.duration);
        let index = model.draw(rng.next_u64(), duration);

        // The book: the grader's maker and the makers' backstop.
        let maker_bps = round2(rng.range_f64(self.maker_bps));
        let floor_bps = round2(rng.range_f64(self.floor_bps).min(0.5 * maker_bps));
        let maker_size = round3(rng.range_f64(self.maker_size));
        let react_slots = rng.range_u64(self.react_slots);
        let backstop_bps = round2(rng.range_f64(self.backstop_bps));

        // The flow: small orders either way, at the session's rate.
        let rate = rng.range_f64(self.noise_rate);
        let mut takers = Vec::new();
        for slot in 0..duration {
            if rng.next_f64() >= rate {
                continue;
            }
            let side = if rng.next_f64() < 0.5 {
                TakerSide::Buy
            } else {
                TakerSide::Sell
            };
            takers.push(self.order(&mut rng, slot, side, maker_bps));
        }

        let actors: Library = serde_json::from_value(json!({
            "scale": {
                "trades_per_minute": self.trades_per_minute,
                "trade_size": self.trade_size,
                "trade_size_sigma": self.trade_size_sigma,
            },
            "front_runners": [{
                "min_mult": predator.min_mult,
                "pattern_steps": 1,
                "window_steps": predator.window_steps,
                "persist_steps": predator.persist_steps,
                "share": predator.share,
                "hold_steps": predator.hold_steps,
                "limit_bps": self.predator_limit_bps,
                "max_inventory_mult": self.predator_cap_mult,
            }],
        }))
        .expect("the family's actors are a valid library");

        // The order: sell the inherited long by the close.
        let quantity = rng.range_u64(self.quantity) as f64;
        let haircut_bps = round2(rng.range_f64(self.haircut_bps));
        // Out of reach of the session's moves: the long's value moves with
        // the market whatever the desk does.
        let max_drawdown = (quantity * self.arrival_usd * self.drawdown_share).round();
        let mut mandate = Mandate::perp(VENUE, &self.symbol, quantity, max_drawdown);
        let allowance = &mut mandate.universe[0];
        allowance.min_position = Some(0.0);
        allowance.sides = Some(vec![Side::Sell]);

        let record = index.record.clone();
        let path = index.waypoints(self.waypoint_slots);
        let shape = Shape {
            predator: predator.name.into(),
            quantity,
            haircut_bps,
            maker_bps,
            maker_size,
            react_slots,
            backstop_bps,
        };
        let description = format!(
            "{}: sell the {quantity} held by the close; the broker takes the rest at {haircut_bps} bp",
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
            }),
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
            order: Some(ExecutionOrder {
                instrument: None,
                side: Side::Sell,
                quantity,
                haircut_bps,
                reduce_only: true,
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
            start_positions: BTreeMap::from([(self.symbol.clone(), quantity)]),
            close_out: Default::default(),
            tx_layer: None,
            stop_out: None,
            liquidator: None,
            markets: Vec::new(),
            joint: None,
        };
        (scenario, shape)
    }

    /// A taker order: size, limit and account drawn alike either way.
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

    /// `count` sessions named `<prefix>-<n>`, the predator types in equal
    /// shares (the remainder to the first types the draws reach).
    pub fn draw_set(&self, prefix: &str, count: usize, seed: u64) -> Vec<(Scenario, Shape)> {
        let model = self.model();
        let kinds = PREDATORS.len();
        let mut quotas: Vec<usize> = (0..kinds)
            .map(|i| count / kinds + usize::from(i < count % kinds))
            .collect();
        let mut out = Vec::with_capacity(count);
        let mut draw = 0u64;
        while out.len() < count {
            draw += 1;
            let draw_seed = seed.wrapping_mul(1_000_003).wrapping_add(draw);
            let kind = Self::predator(draw_seed);
            if quotas[kind] == 0 {
                continue;
            }
            quotas[kind] -= 1;
            let name = format!("{prefix}-{}", out.len() + 1);
            out.push(self.draw_with(&model, &name, draw_seed));
        }
        out
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
        let family = FrontRunFamily::spy();
        let a = family.sample_set("t", 8, 9);
        let b = family.sample_set("t", 8, 9);
        assert_eq!(
            serde_json::to_string(&a).unwrap(),
            serde_json::to_string(&b).unwrap()
        );
        for scenario in &a {
            assert_eq!(scenario.validate(), Ok(()), "{}", scenario.name);
            scenario.actors.as_ref().unwrap().validate().unwrap();
        }
    }

    /// Every type comes equally often, and each session seats one
    /// predator whatever its type.
    #[test]
    fn sets_are_stratified_and_seat_one_predator() {
        let family = FrontRunFamily::spy();
        let set = family.draw_set("h", 70, 3);
        for kind in PREDATORS {
            let n = set.iter().filter(|(_, s)| s.predator == kind.name).count();
            assert_eq!(n, 14, "{}", kind.name);
        }
        for (scenario, _) in &set {
            assert_eq!(scenario.actors.as_ref().unwrap().front_runners.len(), 1);
        }
    }
}
