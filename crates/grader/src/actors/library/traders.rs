//! Takers at the market's pace: noise traders at random, informed traders
//! ahead of the scenario's path, chasers behind it, and decoys that trade
//! ahead of it for a while and then on random views of their own.
//!
//! All take at the start of a step, against what the bot left after its
//! last turn, as the scenario's takers do. Each trade goes from one of the
//! actor's wallets, drawn at random, at a limit `limit_bps` past the price
//! the book trades around.
//!
//! Chasers and decoys are directional flow that knows nothing: summed with
//! the informed, they blur what the market's net flow says, so only a
//! reader who keeps each wallet's record can tell the informed apart.

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

/// Chasers: they take the way the book has moved, once it has moved over
/// `trigger_bps` in the last `lookback_slots`, at `intensity_mult` times
/// the market's trade rate. They know nothing the chart doesn't show.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Chaser {
    #[serde(default = "one_wallet")]
    pub wallets: u32,
    #[serde(default = "forty")]
    pub lookback_slots: u64,
    #[serde(default = "ten")]
    pub trigger_bps: f64,
    #[serde(default = "one")]
    pub intensity_mult: f64,
    #[serde(default = "one")]
    pub size_mult: f64,
    #[serde(default = "fifty")]
    pub limit_bps: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active: Option<Window>,
}

impl Chaser {
    pub fn validate(&self) -> Result<(), String> {
        if self.wallets == 0 {
            return Err("wallets must be at least 1".into());
        }
        if self.lookback_slots == 0 {
            return Err("lookback_slots must be at least 1".into());
        }
        finite_at_least_zero("trigger_bps", self.trigger_bps)?;
        finite_positive("intensity_mult", self.intensity_mult)?;
        finite_positive("size_mult", self.size_mult)?;
        finite_positive("limit_bps", self.limit_bps)
    }
}

/// Decoys: each wallet trades as an informed trader does (it sees the
/// level `horizon_slots` ahead) until a slot drawn per session in
/// `informed_for`, and from then on keeps trading one way at the same
/// pace, on a view it draws at random and holds for `horizon_slots`: a
/// record that looks informed, and then isn't.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Decoy {
    #[serde(default = "one_wallet")]
    pub wallets: u32,
    pub horizon_slots: u64,
    pub edge_bps: f64,
    /// The window its wallets stop seeing ahead in, in slots into the
    /// session.
    pub informed_for: Window,
    #[serde(default = "one")]
    pub intensity_mult: f64,
    #[serde(default = "one")]
    pub size_mult: f64,
    #[serde(default = "fifty")]
    pub limit_bps: f64,
}

impl Decoy {
    pub fn validate(&self) -> Result<(), String> {
        if self.wallets == 0 {
            return Err("wallets must be at least 1".into());
        }
        if self.horizon_slots == 0 {
            return Err("horizon_slots must be at least 1".into());
        }
        if self
            .informed_for
            .until
            .is_none_or(|until| until <= self.informed_for.from)
        {
            return Err("informed_for needs an until after its from".into());
        }
        finite_at_least_zero("edge_bps", self.edge_bps)?;
        finite_positive("intensity_mult", self.intensity_mult)?;
        finite_positive("size_mult", self.size_mult)?;
        finite_positive("limit_bps", self.limit_bps)
    }
}

fn forty() -> u64 {
    40
}

fn ten() -> f64 {
    10.0
}

/// The side a move of `edge` bps calls for, past `trigger` bps either way.
fn side_of(edge: f64, trigger: f64) -> Option<Side> {
    if edge > trigger {
        Some(Side::Bid)
    } else if edge < -trigger {
        Some(Side::Ask)
    } else {
        None
    }
}

/// How far the book moved in bps over the `back` slots to `now`.
fn moved_bps(path: &(dyn Fn(u64) -> Moment + Send + Sync), now: &Moment, back: u64) -> f64 {
    let before = path(now.at.saturating_sub(back)).book_usd;
    (now.book_usd / before - 1.0) * 10_000.0
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
        side_of((ahead / now.book_usd - 1.0) * 10_000.0, self.spec.edge_bps)
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

pub(super) struct ChaserTrader {
    spec: Chaser,
    scale: Scale,
    wallets: Vec<Wallet>,
    path: Arc<dyn Fn(u64) -> Moment + Send + Sync>,
    rng: Rng,
    last: Option<u64>,
}

impl ChaserTrader {
    pub fn new(
        spec: Chaser,
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
}

#[async_trait]
impl Actor for ChaserTrader {
    async fn before_blocks(&mut self, last: &Moment) {
        let slots = last.at - self.last.unwrap_or(0);
        self.last = Some(last.at);
        if !Window::holds(&self.spec.active, last.at) {
            return;
        }
        let moved = moved_bps(self.path.as_ref(), last, self.spec.lookback_slots);
        let Some(side) = side_of(moved, self.spec.trigger_bps) else {
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
    }

    async fn report(&mut self, _: &mut ActorsReport) {}
}

pub(super) struct DecoyTrader {
    spec: Decoy,
    scale: Scale,
    wallets: Vec<Wallet>,
    /// The slot each wallet stops seeing ahead at, and once it has, its
    /// view and the slot it holds it until.
    fades: Vec<u64>,
    views: Vec<Option<(Side, u64)>>,
    path: Arc<dyn Fn(u64) -> Moment + Send + Sync>,
    rng: Rng,
    last: Option<u64>,
}

impl DecoyTrader {
    pub fn new(
        spec: Decoy,
        scale: Scale,
        wallets: Vec<Wallet>,
        path: Arc<dyn Fn(u64) -> Moment + Send + Sync>,
        mut rng: Rng,
    ) -> Self {
        let from = spec.informed_for.from;
        let span = spec.informed_for.until.unwrap_or(from + 1) - from;
        let fades = wallets
            .iter()
            .map(|_| from + rng.below(span as usize) as u64)
            .collect();
        let views = vec![None; wallets.len()];
        Self {
            spec,
            scale,
            wallets,
            fades,
            views,
            path,
            rng,
            last: None,
        }
    }

    /// The side `wallet` trades `at`: ahead of the path before it fades,
    /// its view after.
    fn side(&mut self, wallet: usize, now: &Moment) -> Option<Side> {
        let horizon = self.spec.horizon_slots;
        if now.at < self.fades[wallet] {
            let ahead = (self.path)(now.at + horizon).book_usd;
            return side_of((ahead / now.book_usd - 1.0) * 10_000.0, self.spec.edge_bps);
        }
        match self.views[wallet] {
            Some((side, until)) if now.at < until => Some(side),
            _ => {
                let side = if self.rng.chance(0.5) {
                    Side::Bid
                } else {
                    Side::Ask
                };
                self.views[wallet] = Some((side, now.at + horizon));
                Some(side)
            }
        }
    }
}

#[async_trait]
impl Actor for DecoyTrader {
    async fn before_blocks(&mut self, last: &Moment) {
        let slots = last.at - self.last.unwrap_or(0);
        self.last = Some(last.at);
        let rate = self.scale.per_slot() * self.spec.intensity_mult / self.wallets.len() as f64;
        for wallet in 0..self.wallets.len() {
            let Some(side) = self.side(wallet, last) else {
                continue;
            };
            for _ in 0..self.rng.poisson(rate * slots as f64) {
                let size = self.scale.draw_size(&mut self.rng, self.spec.size_mult);
                let reach = self.spec.limit_bps / 10_000.0;
                let limit = match side {
                    Side::Bid => last.book_usd * (1.0 + reach),
                    Side::Ask => last.book_usd * (1.0 - reach),
                };
                self.wallets[wallet].take(last.at, side, size, limit).await;
            }
        }
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

    #[tokio::test]
    async fn chasers_take_the_way_the_book_moved() {
        let (book, log) = Book::new("c", 99.0, 101.0);
        let (wallet, _) = wallet(book);
        // Up 1% at slot 100.
        let path: Arc<dyn Fn(u64) -> Moment + Send + Sync> =
            Arc::new(|s| at(s, if s >= 100 { 101.0 } else { 100.0 }));
        let spec = Chaser {
            wallets: 1,
            lookback_slots: 40,
            trigger_bps: 50.0,
            intensity_mult: 1.0,
            size_mult: 1.0,
            limit_bps: 50.0,
            active: None,
        };
        let mut chaser = ChaserTrader::new(spec, scale(), vec![wallet], path, Rng::new([4; 32]));
        chaser.before_blocks(&at(90, 100.0)).await;
        assert!(log.lock().unwrap().is_empty(), "nothing has moved yet");
        chaser.before_blocks(&at(110, 101.0)).await;
        let taken = log.lock().unwrap().clone();
        assert!(!taken.is_empty());
        assert!(
            taken.iter().all(|l| l.starts_with("c take Bid")),
            "{taken:?}"
        );
        // Forty slots on, the move is old: nothing.
        log.lock().unwrap().clear();
        chaser.before_blocks(&at(150, 101.0)).await;
        assert!(log.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn decoys_trade_ahead_until_they_fade_then_hold_views_of_their_own() {
        let (book, log) = Book::new("d", 99.0, 101.0);
        let (wallet, _) = wallet(book);
        // Up 1% at slot 100, and again at slot 300.
        let path: Arc<dyn Fn(u64) -> Moment + Send + Sync> = Arc::new(|s| {
            at(
                s,
                match s {
                    0..100 => 100.0,
                    100..300 => 101.0,
                    _ => 102.0,
                },
            )
        });
        let spec = Decoy {
            wallets: 1,
            horizon_slots: 20,
            edge_bps: 50.0,
            informed_for: Window {
                from: 150,
                until: Some(200),
            },
            intensity_mult: 1.0,
            size_mult: 1.0,
            limit_bps: 50.0,
        };
        let mut decoy = DecoyTrader::new(spec, scale(), vec![wallet], path, Rng::new([6; 32]));
        assert!((150..200).contains(&decoy.fades[0]));
        // Ahead of the first rise it buys, as the informed do.
        decoy.before_blocks(&at(90, 100.0)).await;
        let early = std::mem::take(&mut *log.lock().unwrap());
        assert!(
            !early.is_empty() && early.iter().all(|l| l.contains("Bid")),
            "{early:?}"
        );
        // Once it has faded it trades whether or not a move is ahead, one
        // way for a horizon at a time, and both ways over many.
        let mut sides: Vec<(u64, bool)> = Vec::new();
        for s in (210..2_000).step_by(2) {
            decoy.before_blocks(&at(s, 101.0)).await;
            let step = std::mem::take(&mut *log.lock().unwrap());
            let buys = step.iter().filter(|l| l.contains("Bid")).count();
            assert!(buys == 0 || buys == step.len(), "{step:?}");
            if !step.is_empty() {
                sides.push((s, buys > 0));
            }
        }
        assert!(sides.iter().any(|(_, b)| *b) && sides.iter().any(|(_, b)| !*b));
        // Its view changes at most once a horizon.
        let changes = sides.windows(2).filter(|p| p[0].1 != p[1].1).count();
        assert!(changes <= (2_000 - 210) / 20, "{changes}");
    }
}
