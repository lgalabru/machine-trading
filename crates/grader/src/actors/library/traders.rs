//! Noise and informed traders: takers at the market's pace, one at random,
//! the other ahead of the scenario's path.
//!
//! Both take at the start of a step, against what the bot left after its
//! last turn, as the scenario's takers do. Each trade goes from one of the
//! actor's wallets, drawn at random, at a limit `limit_bps` past the price
//! the book trades around.

use std::sync::Arc;

use async_trait::async_trait;
use mtx_kit::Side;
use serde::{Deserialize, Serialize};

use super::{Rng, Scale, Wallet, Window, finite_at_least_zero, finite_positive};
use crate::actors::{Actor, ActorsReport, Moment};

fn one() -> f64 {
    1.0
}

fn one_wallet() -> u32 {
    1
}

fn fifty() -> f64 {
    50.0
}

/// Noise traders: Poisson arrivals at `flow_mult` times the market's trade
/// rate, each a coin-flip side and a lognormal size at `size_mult` times
/// the market's.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Noise {
    #[serde(default = "one_wallet")]
    pub wallets: u32,
    #[serde(default = "one")]
    pub flow_mult: f64,
    #[serde(default = "one")]
    pub size_mult: f64,
    /// How far past the book's price a take may fill, in basis points.
    #[serde(default = "fifty")]
    pub limit_bps: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active: Option<Window>,
}

impl Noise {
    pub fn validate(&self) -> Result<(), String> {
        if self.wallets == 0 {
            return Err("wallets must be at least 1".into());
        }
        finite_at_least_zero("flow_mult", self.flow_mult)?;
        finite_positive("size_mult", self.size_mult)?;
        finite_positive("limit_bps", self.limit_bps)
    }
}

/// Informed traders: they see the scenario's level `horizon_slots` ahead
/// and, while it is more than `edge_bps` from the book's price, take its
/// way at `intensity_mult` times the market's trade rate.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Informed {
    #[serde(default = "one_wallet")]
    pub wallets: u32,
    pub horizon_slots: u64,
    pub edge_bps: f64,
    #[serde(default = "one")]
    pub intensity_mult: f64,
    #[serde(default = "one")]
    pub size_mult: f64,
    #[serde(default = "fifty")]
    pub limit_bps: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active: Option<Window>,
}

impl Informed {
    pub fn validate(&self) -> Result<(), String> {
        if self.wallets == 0 {
            return Err("wallets must be at least 1".into());
        }
        if self.horizon_slots == 0 {
            return Err("horizon_slots must be at least 1".into());
        }
        finite_at_least_zero("edge_bps", self.edge_bps)?;
        finite_positive("intensity_mult", self.intensity_mult)?;
        finite_positive("size_mult", self.size_mult)?;
        finite_positive("limit_bps", self.limit_bps)
    }
}

/// Takes `size` on `side` from a random one of `wallets`, around `price`.
async fn take_one(
    wallets: &[Wallet],
    rng: &mut Rng,
    at: u64,
    side: Side,
    size: f64,
    price: f64,
    limit_bps: f64,
) {
    let wallet = &wallets[rng.below(wallets.len())];
    let reach = limit_bps / 10_000.0;
    let limit = match side {
        Side::Bid => price * (1.0 + reach),
        Side::Ask => price * (1.0 - reach),
    };
    wallet.take(at, side, size, limit).await;
}

pub(super) struct NoiseTrader {
    spec: Noise,
    scale: Scale,
    wallets: Vec<Wallet>,
    rng: Rng,
    /// The slot it last traded up to.
    last: Option<u64>,
}

impl NoiseTrader {
    pub fn new(spec: Noise, scale: Scale, wallets: Vec<Wallet>, rng: Rng) -> Self {
        Self {
            spec,
            scale,
            wallets,
            rng,
            last: None,
        }
    }
}

#[async_trait]
impl Actor for NoiseTrader {
    async fn before_blocks(&mut self, last: &Moment) {
        let slots = last.at - self.last.unwrap_or(0);
        self.last = Some(last.at);
        if !Window::holds(&self.spec.active, last.at) {
            return;
        }
        let n = self
            .rng
            .poisson(self.scale.per_slot() * self.spec.flow_mult * slots as f64);
        for _ in 0..n {
            let side = if self.rng.chance(0.5) {
                Side::Bid
            } else {
                Side::Ask
            };
            let size = self.scale.draw_size(&mut self.rng, self.spec.size_mult);
            take_one(
                &self.wallets,
                &mut self.rng,
                last.at,
                side,
                size,
                last.book_usd,
                self.spec.limit_bps,
            )
            .await;
        }
    }

    async fn report(&mut self, _: &mut ActorsReport) {}
}

pub(super) struct InformedTrader {
    spec: Informed,
    scale: Scale,
    wallets: Vec<Wallet>,
    path: Arc<dyn Fn(u64) -> Moment + Send + Sync>,
    rng: Rng,
    last: Option<u64>,
}

impl InformedTrader {
    pub fn new(
        spec: Informed,
        scale: Scale,
        wallets: Vec<Wallet>,
        path: Arc<dyn Fn(u64) -> Moment + Send + Sync>,
        rng: Rng,
    ) -> Self {
        Self {
            spec,
            scale,
            wallets,
            path,
            rng,
            last: None,
        }
    }

    /// The side it trades `at`, if the move ahead is past its edge.
    fn side(&self, now: &Moment) -> Option<Side> {
        let ahead = (self.path)(now.at + self.spec.horizon_slots).book_usd;
        let edge = (ahead / now.book_usd - 1.0) * 10_000.0;
        if edge > self.spec.edge_bps {
            Some(Side::Bid)
        } else if edge < -self.spec.edge_bps {
            Some(Side::Ask)
        } else {
            None
        }
    }
}

#[async_trait]
impl Actor for InformedTrader {
    async fn before_blocks(&mut self, last: &Moment) {
        let slots = last.at - self.last.unwrap_or(0);
        self.last = Some(last.at);
        if !Window::holds(&self.spec.active, last.at) {
            return;
        }
        let Some(side) = self.side(last) else {
            return;
        };
        let n = self
            .rng
            .poisson(self.scale.per_slot() * self.spec.intensity_mult * slots as f64);
        for _ in 0..n {
            let size = self.scale.draw_size(&mut self.rng, self.spec.size_mult);
            take_one(
                &self.wallets,
                &mut self.rng,
                last.at,
                side,
                size,
                last.book_usd,
                self.spec.limit_bps,
            )
            .await;
        }
        self.wallets[0].count("informed_steps", 1);
    }

    async fn report(&mut self, _: &mut ActorsReport) {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::actors::library::tests::{Book, at, scale, wallet};

    #[tokio::test]
    async fn informed_traders_take_the_way_the_path_goes_ahead_of_it() {
        let (book, log) = Book::new("i", 99.0, 101.0);
        let (wallet, ledger) = wallet(book);
        // Up 1% from slot 100.
        let path: Arc<dyn Fn(u64) -> Moment + Send + Sync> =
            Arc::new(|s| at(s, if s >= 100 { 101.0 } else { 100.0 }));
        let spec = Informed {
            wallets: 1,
            horizon_slots: 20,
            edge_bps: 50.0,
            intensity_mult: 10.0,
            size_mult: 1.0,
            limit_bps: 50.0,
            active: None,
        };
        let mut informed =
            InformedTrader::new(spec, scale(), vec![wallet], path, Rng::new([3; 32]));
        // 20 slots ahead of 60 is still flat: nothing.
        informed.before_blocks(&at(60, 100.0)).await;
        assert!(log.lock().unwrap().is_empty());
        // From 80 it sees the move: it buys, at 50 bps through.
        informed.before_blocks(&at(82, 100.0)).await;
        let log = log.lock().unwrap().clone();
        assert!(!log.is_empty());
        assert!(
            log.iter().all(|l| l == "i take Bid 1.000 at 100.50"),
            "{log:?}"
        );
        assert_eq!(ledger.report().actors[0].counts["informed_steps"], 1);
    }

    #[tokio::test]
    async fn noise_trades_at_the_markets_pace_both_ways() {
        let (book, log) = Book::new("n", 99.0, 101.0);
        let (wallet, _) = wallet(book);
        let spec = Noise {
            wallets: 1,
            flow_mult: 1.0,
            size_mult: 1.0,
            limit_bps: 50.0,
            active: Some(Window {
                from: 0,
                until: Some(2_000),
            }),
        };
        let mut noise = NoiseTrader::new(spec, scale(), vec![wallet], Rng::new([5; 32]));
        // 1 trade a slot (150 a minute at 0.4 s): about 2,000 by slot 2,000,
        // and none after.
        for s in (2..=3_000).step_by(2) {
            noise.before_blocks(&at(s, 100.0)).await;
        }
        let log = log.lock().unwrap();
        let buys = log.iter().filter(|l| l.contains("Bid")).count();
        assert!((1_850..2_150).contains(&log.len()), "{}", log.len());
        assert!(
            (log.len() / 2).abs_diff(buys) < 120,
            "{buys} of {}",
            log.len()
        );
    }
}
