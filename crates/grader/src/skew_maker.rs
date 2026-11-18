//! The skew-maker family: TSLA's opening half hour, where a desk must work
//! an order (build a long, or unwind one) while three rival makers quote the
//! perp around its index and lean their quotes, one of them on a real view
//! of the price ahead.
//!
//! - **The order.** Buy Q TSLA from flat, or sell an inherited long of Q, by
//!   the session's end, the side drawn per pair of sessions; a broker does
//!   whatever is left at the closing index worse by a haircut (`order`).
//!   Reading the makers only changes when and how hard the desk trades:
//!   ahead of a move the order's way, now; ahead of one against it, later.
//! - **The price** walks TSLA's market calibration from 09:31 New York on a
//!   drawn trading day, at the opening hour's measured volatility, jump rate
//!   and jump sizes (`JUMP_REGIME_MEASUREMENT.md` Table 2 and its caveat).
//!   Sessions come in pairs: the second of a pair runs the first's path
//!   mirrored, with its own draws of everything else.
//! - **The skewers** (the library's [`Skewer`](crate::actors::library::Skewer))
//!   each rest a two-sided quote around the index and lean it. One sees the
//!   path `horizon_slots` ahead and leans its way while the move is over
//!   `edge_bps`, right 85% of the time (`hit_rate`, #114). Two lean on
//!   random views, held as long as the informed one's runs and as often in
//!   the session, and quote flat between. All three draw their width, lean,
//!   noise, refresh and size from one range each, and sit in a shuffled
//!   order.
//! - **The flow.** Noise takes at random at the opening hour's pace, two
//!   chasers take the way the book moved, and an arbitrageur ties the book
//!   to the index, taking any quote a move left through it. No taker is
//!   informed: the quotes carry all the information.
//!
//! The informed bot is told who is who (the informed skewer's wallet and
//! the decoys'), never a view, a window, a jump's time or its sign.

use std::collections::BTreeMap;

use mtx_core::{Side, mandate::Mandate};
use serde::Serialize;
use serde_json::json;

use crate::{
    actors::Library,
    market::{
        Calibration, MarketModel, ModelSpec,
        calibration::sha256_hex,
        model::{EventKind, JumpSize, ModelPath, Start},
    },
    orders::ExecutionOrder,
    scenario::{Scenario, SplitMix64, VENUE},
};

/// TSLA's market calibration, built in so hidden sets need no file.
const TSLA_CALIBRATION: &str =
    include_str!("../../../calibration/market/tsla.20261113.8b7bca8e.toml");

const JUMP_SOURCE: &str =
    "JUMP_REGIME_MEASUREMENT.md Table 2: Phoenix 1 m marks, 09:31-10:30, calibration 5-sigma rule";

/// One skewer's draws.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Quoting {
    pub informed: bool,
    pub half_bps: f64,
    pub skew_bps: f64,
    pub quote_noise_bps: f64,
    pub react_slots: u64,
    pub size_mult: f64,
}

/// A session's draws beneath its path and quotes: what the ladder's oracles
/// may be told. The informed bot is told only who is who.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Shape {
    /// The order: `buy` or `sell`, its quantity and the broker's haircut.
    pub side: String,
    pub quantity: f64,
    pub haircut_bps: f64,
    /// The session's start, Unix seconds (09:31 New York).
    pub start_unix: i64,
    pub mirrored: bool,
    /// The skewers in their seats' order.
    pub skewers: Vec<Quoting>,
    pub horizon_slots: u64,
    pub edge_bps: f64,
    /// The share of slots the informed skewer sees a move past its edge,
    /// the mean length of those runs and the longest: what the decoys'
    /// rhythm is set from.
    pub informed_duty: f64,
    pub informed_run_slots: f64,
    pub informed_longest_run: u64,
    pub decoy_hold_slots: [u64; 2],
    pub decoy_gap_slots: [u64; 2],
    pub noise_wallets: u32,
    pub trigger_bps: f64,
    /// The path's jumps: slot and log move in percent. The session's
    /// future: never told to the informed bot.
    pub jumps: Vec<(u64, f64)>,
}

/// The family's parameters. Every range is inclusive.
#[derive(Debug, Clone)]
pub struct SkewMakerFamily {
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
    /// Each skewer's half-width, its lean as a share of it, the noise on
    /// each side's distance as a share of the lean, its refresh and size.
    pub half_bps: (f64, f64),
    pub skew_share: (f64, f64),
    pub quote_noise_share: f64,
    pub react_slots: (u64, u64),
    pub quote_size: (f64, f64),
    /// The informed skewer's view.
    pub horizon_slots: (u64, u64),
    pub edge_bps: (f64, f64),
    pub hit_rate: f64,
    pub noise_wallets: (u64, u64),
    pub noise_flow: f64,
    pub chaser_wallets: u32,
    pub chaser_intensity: f64,
    pub trigger_bps: (f64, f64),
    pub limit_bps: f64,
    /// The order's quantity in TSLA, the broker's haircut in basis points,
    /// and the loss limit as a share of the order's notional (out of reach
    /// of the moves: the order's value moves with the market either way).
    pub quantity: (u64, u64),
    pub haircut_bps: (f64, f64),
    pub drawdown_share: f64,
    /// The snapshot's mark, to size the loss limit.
    pub arrival_usd: f64,
}

impl SkewMakerFamily {
    /// TSLA perps, `skew-maker-follow-tsla`.
    pub fn tsla() -> Self {
        Self {
            symbol: "TSLA".into(),
            duration_slots: 4_500,
            vol_mult: 2.2,
            jump_rate_per_hour: 3.9,
            jump_median_pct: 0.382,
            jump_p90_pct: 0.652,
            waypoint_slots: 2,
            hour_scale: 1.28,
            size_scale: 5.0,
            half_bps: (4.0, 8.0),
            skew_share: (0.6, 1.0),
            quote_noise_share: 0.3,
            react_slots: (8, 14),
            quote_size: (1.0, 3.0),
            horizon_slots: (60, 150),
            edge_bps: (30.0, 40.0),
            hit_rate: 0.85,
            noise_wallets: (3, 5),
            noise_flow: 4.0,
            chaser_wallets: 2,
            chaser_intensity: 1.0,
            trigger_bps: (15.0, 25.0),
            limit_bps: 50.0,
            quantity: (4, 8),
            haircut_bps: (3.0, 8.0),
            drawdown_share: 0.05,
            arrival_usd: 389.0,
        }
    }

    /// The price's model: TSLA's calibration with the opening window's
    /// volatility, jump rate and jump sizes.
    pub fn model(&self) -> MarketModel {
        let mut calibration =
            Calibration::parse(TSLA_CALIBRATION).expect("TSLA's calibration is valid");
        calibration.sha256 = sha256_hex(TSLA_CALIBRATION.as_bytes());
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
                    abs_median_pct: self.jump_median_pct,
                    abs_p90_pct: self.jump_p90_pct,
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
    /// first's path mirrored.
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
            // One side a pair: the mirror is the same order on the mirrored
            // path, so a policy blind to the makers nets about nothing.
            let side = if rng.next_f64() < 0.5 {
                Side::Buy
            } else {
                Side::Sell
            };
            let name = format!("{prefix}-{}", out.len() + 1);
            out.push(self.build(
                &model,
                &name,
                &mut rng,
                path.clone(),
                start_unix,
                false,
                side,
            ));
            if out.len() < count {
                let mut mirror = SplitMix64(draw_seed ^ 0x6d69_7272_6f72);
                let mut flipped = path;
                mirror_path(&mut flipped);
                let name = format!("{prefix}-{}", out.len() + 1);
                out.push(self.build(&model, &name, &mut mirror, flipped, start_unix, true, side));
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
        start_unix: i64,
        mirrored: bool,
        side: Side,
    ) -> (Scenario, Shape) {
        let duration = self.duration_slots;
        let horizon_slots = rng.range_u64(self.horizon_slots);
        let edge_bps = round2(rng.range_f64(self.edge_bps));
        let noise_wallets = rng.range_u64(self.noise_wallets) as u32;
        let trigger_bps = round2(rng.range_f64(self.trigger_bps));
        let (duty, run, longest) = informed_activity(&index, horizon_slots, edge_bps);
        let (hold, gap) = rhythm(duty, run, longest, horizon_slots);

        // Three skewers drawn alike; one of them, in a drawn seat, informed.
        let informed_seat = (rng.next_f64() * 3.0) as usize % 3;
        let skewers: Vec<Quoting> = (0..3)
            .map(|seat| {
                let half_bps = round2(rng.range_f64(self.half_bps));
                let skew_bps = round2(half_bps * rng.range_f64(self.skew_share));
                Quoting {
                    informed: seat == informed_seat,
                    half_bps,
                    skew_bps,
                    quote_noise_bps: round2(skew_bps * self.quote_noise_share),
                    react_slots: rng.range_u64(self.react_slots),
                    size_mult: round2(rng.range_f64(self.quote_size)),
                }
            })
            .collect();
        let view = |q: &Quoting| {
            if q.informed {
                json!({
                    "kind": "informed",
                    "horizon_slots": horizon_slots,
                    "edge_bps": edge_bps,
                    "hit_rate": self.hit_rate,
                })
            } else {
                json!({ "kind": "decoy", "hold_slots": hold, "gap_slots": gap })
            }
        };
        let skewer_specs: Vec<_> = skewers
            .iter()
            .map(|q| {
                json!({
                    "view": view(q),
                    "half_bps": q.half_bps,
                    "skew_bps": q.skew_bps,
                    "quote_noise_bps": q.quote_noise_bps,
                    "react_slots": q.react_slots,
                    "size_mult": q.size_mult,
                })
            })
            .collect();

        let (trades_per_minute, median, sigma) = Self::venue(model);
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
            "chasers": [{
                "wallets": self.chaser_wallets,
                "trigger_bps": trigger_bps,
                "intensity_mult": self.chaser_intensity,
                "limit_bps": self.limit_bps,
            }],
            "arbitrageur": {},
            "skewers": skewer_specs,
        }))
        .expect("the family's actors are a valid library");

        // The order: buy Q from flat, or sell an inherited long of Q.
        let quantity = rng.range_u64(self.quantity) as f64;
        let haircut_bps = round2(rng.range_f64(self.haircut_bps));
        let max_drawdown = (quantity * self.arrival_usd * self.drawdown_share).round();
        let mut mandate = Mandate::perp(VENUE, &self.symbol, quantity, max_drawdown);
        let allowance = &mut mandate.universe[0];
        allowance.min_position = Some(0.0);
        allowance.sides = Some(vec![side]);
        let start_positions = match side {
            Side::Sell => BTreeMap::from([(self.symbol.clone(), quantity)]),
            Side::Buy => BTreeMap::new(),
        };

        let jumps = index
            .record
            .events
            .iter()
            .filter(|e| e.kind == EventKind::Jump)
            .map(|e| (e.slot, round3(e.pct)))
            .collect();
        let side_name = match side {
            Side::Buy => "buy",
            Side::Sell => "sell",
        };
        let shape = Shape {
            side: side_name.into(),
            quantity,
            haircut_bps,
            start_unix,
            mirrored,
            skewers,
            horizon_slots,
            edge_bps,
            informed_duty: round4(duty),
            informed_run_slots: round2(run),
            informed_longest_run: longest,
            decoy_hold_slots: hold,
            decoy_gap_slots: gap,
            noise_wallets,
            trigger_bps,
            jumps,
        };
        let description = format!(
            "{}: {side_name} {quantity} by the end of the opening half hour among three rival makers; the broker does the rest at {haircut_bps} bp",
            self.symbol
        );
        let record = index.record.clone();
        let scenario = Scenario {
            name: name.to_string(),
            description,
            symbol: self.symbol.clone(),
            duration_slots: duration,
            mandate,
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
            order: Some(ExecutionOrder {
                instrument: None,
                side,
                quantity,
                haircut_bps,
                reduce_only: side == Side::Sell,
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
            markets: Vec::new(),
            joint: None,
            start_positions,
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
}

/// The share of slots the informed skewer sees a move past `edge_bps`
/// within `horizon` slots, the mean length of those runs, and the longest.
fn informed_activity(path: &ModelPath, horizon: u64, edge_bps: f64) -> (f64, f64, u64) {
    let levels = &path.log_levels;
    let last = levels.len() - 1;
    let (mut active, mut runs, mut run, mut longest) = (0u64, 0u64, 0u64, 0u64);
    for t in 0..last {
        let ahead = levels[(t + horizon as usize).min(last)];
        let on = ((ahead - levels[t]).exp() - 1.0).abs() * 10_000.0 > edge_bps;
        if on {
            active += 1;
            if run == 0 {
                runs += 1;
            }
            run += 1;
            longest = longest.max(run);
        } else {
            run = 0;
        }
    }
    let duty = active as f64 / last as f64;
    let mean = if runs == 0 {
        horizon as f64 / 2.0
    } else {
        active as f64 / runs as f64
    };
    (duty, mean, longest)
}

/// A decoy's holds and flat spells: holds drawn across the informed
/// skewer's own runs, from half their mean to their longest (a run before a
/// jump lasts about a horizon), spaced so a decoy leans as often as the
/// informed skewer does. A session the informed skewer sits out keeps a
/// floor of leaning, a twentieth of the session.
fn rhythm(duty: f64, run: f64, longest: u64, horizon: u64) -> ([u64; 2], [u64; 2]) {
    let lo = (run * 0.5).round().max(2.0);
    let hi = (longest as f64).max(horizon as f64 / 2.0).max(lo + 1.0);
    let mean = (lo + hi) / 2.0;
    let duty = duty.clamp(0.05, 0.6);
    let gap = mean * (1.0 - duty) / duty;
    (
        [lo as u64, hi.round() as u64],
        [
            (gap * 0.5).round().max(1.0) as u64,
            (gap * 1.5).round().max(2.0) as u64,
        ],
    )
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
        let family = SkewMakerFamily::tsla();
        let a = family.draw_set("t", 6, 9);
        let b = family.draw_set("t", 6, 9);
        assert_eq!(
            serde_json::to_string(&a).unwrap(),
            serde_json::to_string(&b).unwrap()
        );
        for pair in a.chunks(2) {
            assert!(!pair[0].1.mirrored && pair[1].1.mirrored);
            assert_eq!(pair[0].1.side, pair[1].1.side);
            assert_eq!(pair[0].1.start_unix, pair[1].1.start_unix);
        }
        for (scenario, shape) in &a {
            scenario.validate().unwrap();
            assert_eq!(scenario.valuation(), "oracle");
            let actors = scenario.actors.as_ref().unwrap();
            actors.validate().unwrap();
            let kinds: Vec<&str> = actors.skewers.iter().map(|s| s.kind()).collect();
            assert_eq!(kinds.iter().filter(|k| **k == "skewer-informed").count(), 1);
            assert_eq!(kinds.iter().filter(|k| **k == "skewer-decoy").count(), 2);
            assert_eq!(shape.skewers.iter().filter(|q| q.informed).count(), 1);
        }
    }

    /// The informed skewer sits in any seat, and its quoting is drawn from
    /// the decoys' ranges.
    #[test]
    fn the_informed_seat_is_a_draw_and_its_quoting_no_label() {
        let family = SkewMakerFamily::tsla();
        let set = family.draw_set("t", 60, 4);
        let mut seats = [0usize; 3];
        for (_, shape) in &set {
            let seat = shape.skewers.iter().position(|q| q.informed).unwrap();
            seats[seat] += 1;
            for q in &shape.skewers {
                assert!((family.half_bps.0..=family.half_bps.1).contains(&q.half_bps));
                assert!(q.skew_bps <= q.half_bps);
            }
        }
        assert!(seats.iter().all(|n| *n >= 10), "{seats:?}");
    }

    /// A sell order unwinds an inherited long, a buy order builds one from
    /// flat, and the mandate allows only the order's side, within Q.
    #[test]
    fn each_session_works_one_order_the_mandate_holds_to() {
        let family = SkewMakerFamily::tsla();
        let set = family.draw_set("t", 24, 6);
        let sides: Vec<&str> = set.iter().map(|(_, s)| s.side.as_str()).collect();
        assert!(
            sides.contains(&"buy") && sides.contains(&"sell"),
            "{sides:?}"
        );
        for (scenario, shape) in &set {
            let order = scenario.order.as_ref().unwrap();
            assert_eq!(order.quantity, shape.quantity);
            assert!((4.0..=8.0).contains(&order.quantity));
            let held = scenario.start_positions.get("TSLA").copied().unwrap_or(0.0);
            let allowance = &scenario.mandate.universe[0];
            assert_eq!(allowance.max_position, order.quantity);
            assert_eq!(allowance.min_position, Some(0.0));
            assert_eq!(allowance.sides.as_deref(), Some(&[order.side][..]));
            match order.side {
                Side::Sell => assert!(held == order.quantity && order.reduce_only),
                Side::Buy => assert!(held == 0.0 && !order.reduce_only),
            }
        }
    }

    #[test]
    fn sessions_start_at_09_31_new_york_on_a_trading_day() {
        let family = SkewMakerFamily::tsla();
        let model = family.model();
        for (_, shape) in family.draw_set("t", 6, 2) {
            assert_eq!(model.calendar().kind_at(shape.start_unix), Some("regular"));
            assert_ne!(
                model.calendar().kind_at(shape.start_unix - 61),
                Some("regular")
            );
        }
    }
}
