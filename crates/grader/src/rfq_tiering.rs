//! The RFQ client-tiering family: a desk answers SPCX requests for quotes
//! from a book of clients, a few of whom know where SPCX is going.
//!
//! - **The price** walks SPCX's market calibration (GARCH-t on one-minute
//!   blocks, its fitted tails, volatility level and jumps), inside regular
//!   hours. The desk values its inventory at Phoenix's index, which follows
//!   it to the cent.
//! - **The clients.** 10 to 16 of them ask on one Poisson schedule, each 3
//!   to 10 times a session on average, one side at a time, in sizes from one
//!   distribution. A share of them (0.15 to 0.45, drawn per session, and
//!   which ones drawn by the session's own seed) see the level `lead_slots`
//!   ahead and ask the way it is going; the rest ask either way at random.
//! - **The reach.** A client takes a quote no worse than its value by
//!   `tolerance_bps` for one trade size, times the square root of the size in
//!   trade sizes. Past it, it takes nothing.
//! - **The desk** starts with SPCX and USDC so it can sell first, and is
//!   scored over the idle desk: what its starting SPCX would have made held
//!   through the session is left out, so a desk that answers nothing scores
//!   0. It may not trade the perp.
//!
//! No tells: every client is seated and funded in every session, asks on the
//! same schedule, in the same sizes, with the same deadline; ids carry
//! nothing (`c<n>-<k>`), and which clients are informed is redrawn every
//! session.

use mtx_core::mandate::Mandate;
use serde::Serialize;

use crate::{
    actors::library::{Library, RfqClients, Scale},
    market::{
        Calibration, MarketModel, ModelSpec,
        calibration::sha256_hex,
        model::{ModelPath, Start},
    },
    scenario::{Scenario, SplitMix64, VENUE},
};

/// SPCX's market calibration, built in so hidden sets need no file.
const SPCX_CALIBRATION: &str =
    include_str!("../../../calibration/market/spcx.20261112.d1f48be2.toml");

/// What a session is drawn as, beneath its path: the clients' process. Which
/// clients are informed is the session's own draw (the RFQ clients' seed),
/// told to the informed bot through the roster.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Shape {
    pub clients: u32,
    pub informed_share: f64,
    pub lead_slots: u64,
    pub correlation: f64,
    pub tolerance_bps: f64,
    /// Requests per client per session, on average, and over all clients
    /// per 100 slots.
    pub asks_per_client: f64,
    pub per_100_slots: f64,
}

/// The family's parameters. Every range is inclusive.
#[derive(Debug, Clone)]
pub struct RfqTieringFamily {
    pub symbol: String,
    pub duration: (u64, u64),
    pub waypoint_slots: u64,
    pub clients: (u64, u64),
    pub informed_share: (f64, f64),
    pub lead_slots: (u64, u64),
    pub correlation: (f64, f64),
    pub asks_per_client: (f64, f64),
    pub tolerance_bps: (f64, f64),
    /// A request's size: lognormal, its median in trade sizes and its
    /// log-sd; one trade size in SPCX.
    pub size_mult: f64,
    pub size_sigma: f64,
    pub trade_size: f64,
    pub deadline_slots: u64,
    /// The desk's start: SPCX and USDC.
    pub desk_base: f64,
    pub desk_usdc: f64,
    pub max_drawdown_usd: f64,
}

impl RfqTieringFamily {
    pub fn spcx() -> Self {
        Self {
            symbol: "SPCX".into(),
            duration: (1_200, 2_000),
            waypoint_slots: 4,
            clients: (10, 16),
            informed_share: (0.15, 0.45),
            lead_slots: (30, 90),
            correlation: (0.6, 0.95),
            asks_per_client: (3.0, 10.0),
            tolerance_bps: (0.2, 0.5),
            size_mult: 20.0,
            size_sigma: 0.5,
            // SPCX's median Phoenix trade (calibration, 30 days to 2026-11-11).
            trade_size: 0.021,
            deadline_slots: 8,
            desk_base: 20.0,
            desk_usdc: 3_000.0,
            max_drawdown_usd: 400.0,
        }
    }

    /// The price's model: SPCX's calibration inside regular hours.
    pub fn model() -> MarketModel {
        let mut calibration =
            Calibration::parse(SPCX_CALIBRATION).expect("SPCX's calibration is valid");
        calibration.sha256 = sha256_hex(SPCX_CALIBRATION.as_bytes());
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
        self.draw_with(&Self::model(), name, seed)
    }

    fn draw_with(&self, model: &MarketModel, name: &str, seed: u64) -> (Scenario, Shape) {
        let mut rng = SplitMix64(seed);
        let duration = rng.range_u64(self.duration);
        let index: ModelPath = model.draw(rng.next_u64(), duration);
        let clients = rng.range_u64(self.clients) as u32;
        let informed_share = round3(rng.range_f64(self.informed_share));
        let lead_slots = rng.range_u64(self.lead_slots);
        let correlation = round3(rng.range_f64(self.correlation));
        let asks_per_client = round2(rng.range_f64(self.asks_per_client));
        let tolerance_bps = round3(rng.range_f64(self.tolerance_bps));
        let per_100_slots = round3(f64::from(clients) * asks_per_client * 100.0 / duration as f64);
        let shape = Shape {
            clients,
            informed_share,
            lead_slots,
            correlation,
            tolerance_bps,
            asks_per_client,
            per_100_slots,
        };
        let rfq = RfqClients {
            clients,
            informed_share,
            lead_slots,
            correlation,
            per_100_slots,
            size_mult: self.size_mult,
            deadline_slots: self.deadline_slots,
            tolerance_bps,
            two_way_share: 0.0,
            desk_base: self.desk_base,
            desk_usdc: self.desk_usdc,
            over_idle_desk: true,
        };
        let library = Library {
            scale: Scale {
                trades_per_minute: 1.463,
                trade_size: self.trade_size,
                trade_size_sigma: self.size_sigma,
            },
            rfq_clients: Some(rfq),
            record: true,
            ..Library::default()
        };
        // The perp is read-only: its index values the desk.
        let mandate = Mandate::perp(VENUE, &self.symbol, 0.001, self.max_drawdown_usd);
        let description = format!(
            "{}: answer {clients} clients' requests for quotes; the desk is scored over holding its start",
            self.symbol
        );
        let scenario = serde_json::from_value::<Scenario>(serde_json::json!({
            "name": name,
            "description": description,
            "symbol": self.symbol,
            "duration_slots": duration,
            "mandate": mandate,
            "path": index.waypoints(self.waypoint_slots),
            "actors": library,
            "market_model": index.record,
        }))
        .expect("an RFQ tiering scenario is a valid scenario");
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
        let model = Self::model();
        (0..count)
            .map(|i| {
                let draw_seed = seed.wrapping_mul(1_000_003).wrapping_add(i as u64 + 1);
                self.draw_with(&model, &format!("{prefix}-{}", i + 1), draw_seed)
            })
            .collect()
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
        let family = RfqTieringFamily::spcx();
        for seed in 0..8 {
            let (a, shape) = family.draw("s", seed);
            let (b, _) = family.draw("s", seed);
            assert_eq!(a, b);
            a.validate().unwrap();
            let rfq = a.actors.as_ref().unwrap().rfq_clients.as_ref().unwrap();
            assert_eq!(rfq.two_way_share, 0.0);
            assert!(rfq.over_idle_desk);
            assert!((10..=16).contains(&shape.clients));
            assert!((30..=90).contains(&shape.lead_slots));
        }
    }
}
