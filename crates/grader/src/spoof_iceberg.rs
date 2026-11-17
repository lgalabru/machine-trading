//! The spoof-or-iceberg family: a desk must sell an inherited long in QQQ
//! perps by the close, while large orders stand on the book ahead of a jump.
//!
//! - **The price** walks QQQ's market calibration inside regular hours and
//!   jumps once, up or down with equal odds, by a multiple of the
//!   calibration's median jump, over a slot or two, at an onset drawn in
//!   the middle of the session.
//! - **The walls.** Two wallets see the jump coming, each its own number of
//!   slots ahead, and rest a large order behind the touch from then on. The
//!   spoofer (the library's [`Spoofer`](crate::actors::library::Spoofer))
//!   paints the side the jump goes against, a bid wall ahead of a fall, and
//!   pulls the wall when the touch comes near it. The iceberg (the library's
//!   [`Iceberg`](crate::actors::library::Iceberg)) is a genuine parent order
//!   on the jump's side, a bid ahead of a rise, shown a slice at a time: it
//!   reloads where it stands after each fill and never pulls on approach.
//!   Both draw their distance from the touch, their size and their lead
//!   from the same ranges, so a snapshot of the book can't tell them apart;
//!   only what each does when the price comes to it can.
//! - **The flow.** Noise takes at random from wallets of its own, and a
//!   refill puts back the depth takes consume.
//! - **The order.** The desk starts long `Q` and must be flat by the close,
//!   sells only; a broker sells whatever is left at the closing index less
//!   a haircut (`order`, `mandate`).
//!
//! No tells: every session seats the same actors from fresh wallets of the
//! session's keyring, and up and down are mirrored. Who is the spoofer and
//! who the iceberg is the session's latent identity, told to the informed
//! bot as wallets (`crate::latent`); never the jump itself.

use std::collections::BTreeMap;

use mtx_core::{Side, mandate::Mandate};
use serde::Serialize;
use serde_json::json;

use crate::{
    actors::Library,
    market::{
        Calibration, MarketModel, ModelSpec,
        calibration::sha256_hex,
        model::{ModelPath, Start},
    },
    orders::ExecutionOrder,
    scenario::{Scenario, SplitMix64, VENUE},
};

/// QQQ's market calibration, built in so hidden sets need no file.
const QQQ_CALIBRATION: &str =
    include_str!("../../../calibration/market/qqq.20261113.60ee1c12.toml");

/// A session's draws beneath its path and book: what the ladder's oracles
/// may be told. The informed bot is told only who the spoofer and the
/// iceberg are.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Shape {
    /// `up` or `down`: the session's future, never told to the informed bot.
    pub direction: String,
    /// The jump's first slot, its ramp in slots and its size in percent
    /// (signed).
    pub onset: u64,
    pub ramp: u64,
    pub move_pct: f64,
    /// How far ahead of the onset each wall goes up, in slots.
    pub spoofer_lead: u64,
    pub iceberg_lead: u64,
    /// Each wall's distance behind the touch, in ticks, and size, in the
    /// market's trade sizes.
    pub spoofer_ticks: u64,
    pub iceberg_ticks: u64,
    pub wall_mult: f64,
    pub display_mult: f64,
}

/// The family's parameters. Every range is inclusive.
#[derive(Debug, Clone)]
pub struct SpoofIcebergFamily {
    pub symbol: String,
    /// The snapshot's mark, which sessions start at.
    pub arrival_usd: f64,
    pub calibration: &'static str,
    pub duration: (u64, u64),
    /// The jump: its onset as a share of the session, its size as a
    /// multiple of the calibration's median jump, and its ramp in slots.
    pub onset_share: (f64, f64),
    pub move_mult: (f64, f64),
    pub ramp_slots: (u64, u64),
    /// Slots between the path's waypoints.
    pub waypoint_slots: u64,
    /// How far ahead of the jump a wall goes up, in slots; its distance
    /// behind the touch, in ticks; and its size, in the market's trade
    /// sizes. One range for the spoofer's wall and the iceberg's slice.
    pub lead_slots: (u64, u64),
    pub distance_ticks: (u64, u64),
    pub size_mult: (f64, f64),
    /// The spoofer pulls its wall once the touch is this many ticks from it.
    pub cancel_ticks: (u64, u64),
    /// The iceberg's parent, as a multiple of its slice, and how long it
    /// keeps working after the jump, in slots.
    pub parent_slices: (f64, f64),
    pub iceberg_tail: (u64, u64),
    /// The market's own taker pace and size, and the noise.
    pub trades_per_minute: f64,
    pub trade_size: f64,
    pub trade_size_sigma: f64,
    pub noise_wallets: (u64, u64),
    pub noise_mult: f64,
    pub noise_limit_bps: f64,
    /// The refill's half-life range, in slots.
    pub refill_half_life: (f64, f64),
    /// The order: its quantity, and the broker's haircut in basis points.
    pub quantity: (u64, u64),
    pub haircut_bps: (f64, f64),
    /// The mandate's loss limit, as a share of the order's notional.
    pub drawdown_share: f64,
}

impl SpoofIcebergFamily {
    /// QQQ perps, `spoof-or-iceberg-exit-qqq`.
    pub fn qqq() -> Self {
        Self {
            symbol: "QQQ".into(),
            arrival_usd: 674.92,
            calibration: QQQ_CALIBRATION,
            duration: (1_400, 2_000),
            onset_share: (0.3, 0.8),
            move_mult: (2.0, 4.0),
            ramp_slots: (1, 2),
            waypoint_slots: 2,
            lead_slots: (30, 90),
            distance_ticks: (20, 60),
            size_mult: (20.0, 80.0),
            cancel_ticks: (1, 3),
            parent_slices: (3.0, 8.0),
            iceberg_tail: (100, 400),
            trades_per_minute: 1.408,
            trade_size: 0.05,
            trade_size_sigma: 0.705,
            noise_wallets: (3, 5),
            noise_mult: 3.0,
            noise_limit_bps: 4.0,
            refill_half_life: (4.0, 40.0),
            quantity: (6, 14),
            haircut_bps: (5.0, 15.0),
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

    /// The jump's sign a draw seed gives: read first, so a set can mirror.
    pub fn sign(seed: u64) -> f64 {
        if SplitMix64(seed).next_f64() < 0.5 {
            1.0
        } else {
            -1.0
        }
    }

    pub fn sample(&self, name: &str, seed: u64) -> Scenario {
        self.draw(name, seed).0
    }

    pub fn draw(&self, name: &str, seed: u64) -> (Scenario, Shape) {
        self.draw_with(&self.model(), name, seed)
    }

    fn draw_with(&self, model: &MarketModel, name: &str, seed: u64) -> (Scenario, Shape) {
        let sign = Self::sign(seed);
        let mut rng = SplitMix64(seed);
        rng.next_f64();
        let duration = rng.range_u64(self.duration);
        let mut index = model.draw(rng.next_u64(), duration);
        let median_jump_pct = model.calibration.fair_value.jumps.abs_size_median_pct;

        let onset = (rng.range_f64(self.onset_share) * duration as f64) as u64;
        let ramp = rng.range_u64(self.ramp_slots);
        let size_pct = round3(sign * rng.range_f64(self.move_mult) * median_jump_pct);
        lay_move(&mut index, onset, ramp, size_pct);

        // The walls: drawn alike, then put on the sides the jump gives them.
        let spoofer_lead = rng.range_u64(self.lead_slots);
        let iceberg_lead = rng.range_u64(self.lead_slots);
        let spoofer_ticks = rng.range_u64(self.distance_ticks);
        let iceberg_ticks = rng.range_u64(self.distance_ticks);
        let wall_mult = round2(rng.range_f64(self.size_mult));
        let display_mult = round2(rng.range_f64(self.size_mult));
        let cancel_ticks = rng
            .range_u64(self.cancel_ticks)
            .min(spoofer_ticks.saturating_sub(1));
        let parent_mult = round2(display_mult * rng.range_f64(self.parent_slices));
        let tail = rng.range_u64(self.iceberg_tail);
        let (with, against) = if sign > 0.0 {
            ("bid", "ask")
        } else {
            ("ask", "bid")
        };

        let noise = rng.range_u64(self.noise_wallets) as u32;
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
                "limit_bps": self.noise_limit_bps,
            }],
            "spoofers": [{
                "side": against,
                "distance_ticks": spoofer_ticks,
                "cancel_ticks": cancel_ticks,
                "wall_mult": wall_mult,
                "lifetime_slots": duration,
                "active": { "from": onset.saturating_sub(spoofer_lead), "until": onset + ramp },
            }],
            "icebergs": [{
                "side": with,
                "distance_ticks": iceberg_ticks,
                "display_mult": display_mult,
                "parent_mult": parent_mult,
                "max_away_ticks": iceberg_ticks + self.distance_ticks.1,
                "active": { "from": onset.saturating_sub(iceberg_lead), "until": onset + tail },
            }],
            "refill": { "half_life_slots": [half_life, half_life], "levels": 5 },
        }))
        .expect("the family's actors are a valid library");

        // The order: sell the inherited long by the close.
        let quantity = rng.range_u64(self.quantity) as f64;
        let haircut_bps = round2(rng.range_f64(self.haircut_bps));
        // Out of reach of the jump: the long's value moves with the market
        // whatever the desk does.
        let max_drawdown = (quantity * self.arrival_usd * self.drawdown_share).round();
        let mut mandate = Mandate::perp(VENUE, &self.symbol, quantity, max_drawdown);
        let allowance = &mut mandate.universe[0];
        allowance.min_position = Some(0.0);
        allowance.sides = Some(vec![Side::Sell]);

        let record = index.record.clone();
        let path = index.waypoints(self.waypoint_slots);
        let shape = Shape {
            direction: if sign > 0.0 { "up" } else { "down" }.into(),
            onset,
            ramp,
            move_pct: size_pct,
            spoofer_lead,
            iceberg_lead,
            spoofer_ticks,
            iceberg_ticks,
            wall_mult,
            display_mult,
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
        };
        (scenario, shape)
    }

    pub fn sample_set(&self, prefix: &str, count: usize, seed: u64) -> Vec<Scenario> {
        self.draw_set(prefix, count, seed)
            .into_iter()
            .map(|(scenario, _)| scenario)
            .collect()
    }

    /// `count` sessions named `<prefix>-<n>`, half of them up and half down
    /// (the odd one either way).
    pub fn draw_set(&self, prefix: &str, count: usize, seed: u64) -> Vec<(Scenario, Shape)> {
        let model = self.model();
        let up = count / 2 + (count % 2) * usize::from(Self::sign(seed) > 0.0);
        let mut quotas = [up, count - up];
        let mut out = Vec::with_capacity(count);
        let mut draw = 0u64;
        while out.len() < count {
            draw += 1;
            let draw_seed = seed.wrapping_mul(1_000_003).wrapping_add(draw);
            let slot = usize::from(Self::sign(draw_seed) < 0.0);
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

/// Lays a jump of `size_pct` on the path from `onset` over `ramp` slots, in
/// equal steps, and holds it after.
fn lay_move(path: &mut ModelPath, onset: u64, ramp: u64, size_pct: f64) {
    let lift = (1.0 + size_pct / 100.0).ln();
    let ramp = ramp.max(1);
    for (slot, level) in path.log_levels.iter_mut().enumerate() {
        let slot = slot as u64;
        if slot < onset {
            continue;
        }
        let share = ((slot - onset + 1) as f64 / ramp as f64).min(1.0);
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
        let family = SpoofIcebergFamily::qqq();
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

    /// Sets are mirrored, and the walls sit on the sides the jump gives
    /// them: the spoofer against it, the iceberg with it.
    #[test]
    fn sets_are_mirrored_and_the_walls_face_the_jump() {
        let family = SpoofIcebergFamily::qqq();
        let set = family.draw_set("h", 40, 3);
        let ups = set.iter().filter(|(_, s)| s.direction == "up").count();
        assert_eq!(ups, 20);
        for (scenario, shape) in &set {
            let actors = scenario.actors.as_ref().unwrap();
            let spoof = serde_json::to_value(&actors.spoofers[0]).unwrap();
            let ice = serde_json::to_value(&actors.icebergs[0]).unwrap();
            let up = shape.move_pct > 0.0;
            assert_eq!(spoof["side"], if up { "ask" } else { "bid" });
            assert_eq!(ice["side"], if up { "bid" } else { "ask" });
            let at = |slot: u64| crate::making::level_at(&scenario.path, slot);
            let moved = at(shape.onset + 10) - at(shape.onset.saturating_sub(4));
            assert!(moved * shape.move_pct > 0.0, "{}", scenario.name);
        }
    }
}
