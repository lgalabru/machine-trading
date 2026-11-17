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
//!
//! The informed and decoys may be [`Shy`]: they hide their trading for a
//! while once one account keeps following it.

use std::sync::Arc;

use async_trait::async_trait;
use mtx_kit::Side;
use serde::{Deserialize, Serialize};

use super::{
    Rng, Scale, Wallet, Window, finite_at_least_zero, finite_positive,
    shy::{Copier, Shy, ShyWatch},
    within,
};
use crate::actors::{Actor, ActorsReport, Moment, StepFlow};

fn one() -> f64 {
    1.0
}

fn one_wallet() -> u32 {
    1
}

fn fifty() -> f64 {
    50.0
}

fn most() -> f64 {
    0.85
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
/// way at `intensity_mult` times the market's trade rate. Their view is not
/// perfect: it is off by `view_noise_bps` (a normal draw's sigma) at each
/// look, and for one `horizon_slots` in `1 - hit_rate` of them it is wrong
/// outright and they trade the other way (see [`View`]).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Informed {
    #[serde(default = "one_wallet")]
    pub wallets: u32,
    pub horizon_slots: u64,
    pub edge_bps: f64,
    #[serde(default = "most")]
    pub hit_rate: f64,
    #[serde(default)]
    pub view_noise_bps: f64,
    #[serde(default = "one")]
    pub intensity_mult: f64,
    #[serde(default = "one")]
    pub size_mult: f64,
    #[serde(default = "fifty")]
    pub limit_bps: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active: Option<Window>,
    /// How it reacts to being copied, if it minds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shy: Option<Shy>,
}

impl Informed {
    pub fn validate(&self) -> Result<(), String> {
        if self.wallets == 0 {
            return Err("wallets must be at least 1".into());
        }
        if self.horizon_slots == 0 {
            return Err("horizon_slots must be at least 1".into());
        }
        if let Some(shy) = &self.shy {
            shy.validate()?;
        }
        finite_at_least_zero("edge_bps", self.edge_bps)?;
        within("hit_rate", self.hit_rate, 0.5, 1.0)?;
        finite_at_least_zero("view_noise_bps", self.view_noise_bps)?;
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
/// level `horizon_slots` ahead, as imperfectly, by `hit_rate` and
/// `view_noise_bps`) until a slot drawn per session in
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
    #[serde(default = "most")]
    pub hit_rate: f64,
    #[serde(default)]
    pub view_noise_bps: f64,
    /// The window its wallets stop seeing ahead in, in slots into the
    /// session.
    pub informed_for: Window,
    #[serde(default = "one")]
    pub intensity_mult: f64,
    #[serde(default = "one")]
    pub size_mult: f64,
    #[serde(default = "fifty")]
    pub limit_bps: f64,
    /// How it reacts to being copied, if it minds: its wallets together.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shy: Option<Shy>,
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
        if let Some(shy) = &self.shy {
            shy.validate()?;
        }
        finite_at_least_zero("edge_bps", self.edge_bps)?;
        within("hit_rate", self.hit_rate, 0.5, 1.0)?;
        finite_at_least_zero("view_noise_bps", self.view_noise_bps)?;
        finite_positive("intensity_mult", self.intensity_mult)?;
        finite_positive("size_mult", self.size_mult)?;
        finite_positive("limit_bps", self.limit_bps)
    }
}

/// What a trader that sees ahead makes of it. Its view is off by a normal
/// draw at each look, and once a `horizon_slots` it is wrong altogether
/// with chance `1 - hit_rate`, until the next draw: it takes the move it
/// sees the wrong way, as a trader whose read of the market is mistaken
/// does, and not a step at a time. Right on average, then, and not always.
#[derive(Debug, Clone, Copy)]
struct View {
    horizon: u64,
    edge_bps: f64,
    hit_rate: f64,
    noise_bps: f64,
    wrong: bool,
    /// The slot the current draw of right or wrong lasts to.
    until: u64,
}

impl View {
    fn new(horizon: u64, edge_bps: f64, hit_rate: f64, noise_bps: f64) -> Self {
        Self {
            horizon,
            edge_bps,
            hit_rate,
            noise_bps,
            wrong: false,
            until: 0,
        }
    }

    /// The side it trades at `at`, given the move ahead in bps.
    fn side(&mut self, rng: &mut Rng, at: u64, ahead_bps: f64) -> Option<Side> {
        if at >= self.until {
            self.wrong = self.hit_rate < 1.0 && !rng.chance(self.hit_rate);
            self.until = at + self.horizon;
        }
        let seen = if self.noise_bps > 0.0 {
            ahead_bps + self.noise_bps * rng.normal()
        } else {
            ahead_bps
        };
        let side = side_of(seen, self.edge_bps)?;
        Some(if self.wrong { side.opposite() } else { side })
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
/// Whether it landed.
async fn take_one(
    wallets: &[Wallet],
    rng: &mut Rng,
    at: u64,
    side: Side,
    size: f64,
    price: f64,
    limit_bps: f64,
) -> bool {
    let wallet = &wallets[rng.below(wallets.len())];
    let reach = limit_bps / 10_000.0;
    let limit = match side {
        Side::Bid => price * (1.0 + reach),
        Side::Ask => price * (1.0 - reach),
    };
    wallet.take(at, side, size, limit).await
}

/// A shy trader's watch over `wallets`, if `shy` says it is one.
fn watch(shy: Option<Shy>, wallets: &[Wallet]) -> Option<ShyWatch> {
    let shy = shy?;
    let own = wallets.iter().map(Wallet::pubkey).collect();
    Some(ShyWatch::new(shy, own, wallets.first()?.ledger.clone()))
}

/// Start a shy trader's step at `at`: what it keeps of its size and
/// horizon, all of them if it isn't shy.
fn begin_shy(shy: &mut Option<ShyWatch>, wallets: &[Wallet], at: u64) -> f64 {
    let Some(watch) = shy.as_mut() else {
        return 1.0;
    };
    if let Some(copier) = watch.begin(at) {
        wallets[0].count("throttles", 1);
        if copier == Copier::Bot {
            wallets[0].count("throttles_by_bot", 1);
        }
    }
    if watch.throttled(at) {
        wallets[0].count("throttled_steps", 1);
    }
    watch.keep(at)
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
    /// One view a wallet, as a decoy's: the wallets err on their own.
    views: Vec<View>,
    last: Option<u64>,
    shy: Option<ShyWatch>,
}

impl InformedTrader {
    pub fn new(
        spec: Informed,
        scale: Scale,
        wallets: Vec<Wallet>,
        path: Arc<dyn Fn(u64) -> Moment + Send + Sync>,
        rng: Rng,
    ) -> Self {
        let shy = watch(spec.shy, &wallets);
        let views = vec![
            View::new(
                spec.horizon_slots,
                spec.edge_bps,
                spec.hit_rate,
                spec.view_noise_bps
            );
            wallets.len()
        ];
        Self {
            spec,
            scale,
            wallets,
            path,
            rng,
            views,
            last: None,
            shy,
        }
    }

    /// The side `wallet` trades `at`, if the move it sees `horizon` slots
    /// ahead is past its edge.
    fn side(&mut self, wallet: usize, now: &Moment, horizon: u64) -> Option<Side> {
        let ahead = (self.path)(now.at + horizon).book_usd;
        self.views[wallet].side(
            &mut self.rng,
            now.at,
            (ahead / now.book_usd - 1.0) * 10_000.0,
        )
    }
}

/// `share` of `horizon`, at least a slot.
fn kept(horizon: u64, share: f64) -> u64 {
    ((horizon as f64 * share).round() as u64).max(1)
}

#[async_trait]
impl Actor for InformedTrader {
    async fn before_blocks(&mut self, last: &Moment) {
        let slots = last.at - self.last.unwrap_or(0);
        self.last = Some(last.at);
        let keep = begin_shy(&mut self.shy, &self.wallets, last.at);
        if !Window::holds(&self.spec.active, last.at) {
            return;
        }
        let horizon = kept(self.spec.horizon_slots, keep);
        let rate =
            self.scale.per_slot() * self.spec.intensity_mult * keep / self.wallets.len() as f64;
        let mut stepped = false;
        for wallet in 0..self.wallets.len() {
            let Some(side) = self.side(wallet, last, horizon) else {
                continue;
            };
            stepped = true;
            for _ in 0..self.rng.poisson(rate * slots as f64) {
                let size = self.scale.draw_size(&mut self.rng, self.spec.size_mult) * keep;
                let reach = self.spec.limit_bps / 10_000.0;
                let limit = match side {
                    Side::Bid => last.book_usd * (1.0 + reach),
                    Side::Ask => last.book_usd * (1.0 - reach),
                };
                let landed = self.wallets[wallet].take(last.at, side, size, limit).await;
                if landed && let Some(shy) = self.shy.as_mut() {
                    shy.filled(last.at, side, size);
                }
            }
        }
        if stepped {
            self.wallets[0].count("informed_steps", 1);
        }
    }

    fn wants_flow(&self) -> bool {
        self.shy.is_some()
    }

    async fn flow(&mut self, flow: &StepFlow) {
        if let Some(shy) = self.shy.as_mut() {
            shy.bot(flow);
        }
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
    /// Each wallet's read of the path while it still sees ahead.
    reads: Vec<View>,
    path: Arc<dyn Fn(u64) -> Moment + Send + Sync>,
    rng: Rng,
    last: Option<u64>,
    shy: Option<ShyWatch>,
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
        let shy = watch(spec.shy, &wallets);
        let reads = vec![
            View::new(
                spec.horizon_slots,
                spec.edge_bps,
                spec.hit_rate,
                spec.view_noise_bps
            );
            wallets.len()
        ];
        Self {
            spec,
            scale,
            wallets,
            fades,
            views,
            reads,
            path,
            rng,
            last: None,
            shy,
        }
    }

    /// The side `wallet` trades `at`: ahead of the path before it fades,
    /// looking `keep` of its horizon ahead, its view after.
    fn side(&mut self, wallet: usize, now: &Moment, keep: f64) -> Option<Side> {
        let horizon = self.spec.horizon_slots;
        if now.at < self.fades[wallet] {
            let ahead = (self.path)(now.at + kept(horizon, keep)).book_usd;
            return self.reads[wallet].side(
                &mut self.rng,
                now.at,
                (ahead / now.book_usd - 1.0) * 10_000.0,
            );
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
        let keep = begin_shy(&mut self.shy, &self.wallets, last.at);
        let rate =
            self.scale.per_slot() * self.spec.intensity_mult * keep / self.wallets.len() as f64;
        for wallet in 0..self.wallets.len() {
            let Some(side) = self.side(wallet, last, keep) else {
                continue;
            };
            for _ in 0..self.rng.poisson(rate * slots as f64) {
                let size = self.scale.draw_size(&mut self.rng, self.spec.size_mult) * keep;
                let reach = self.spec.limit_bps / 10_000.0;
                let limit = match side {
                    Side::Bid => last.book_usd * (1.0 + reach),
                    Side::Ask => last.book_usd * (1.0 - reach),
                };
                let landed = self.wallets[wallet].take(last.at, side, size, limit).await;
                if landed && let Some(shy) = self.shy.as_mut() {
                    shy.filled(last.at, side, size);
                }
            }
        }
    }

    fn wants_flow(&self) -> bool {
        self.shy.is_some()
    }

    async fn flow(&mut self, flow: &StepFlow) {
        if let Some(shy) = self.shy.as_mut() {
            shy.bot(flow);
        }
    }

    async fn report(&mut self, _: &mut ActorsReport) {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::actors::{
        VenueFlow,
        library::tests::{Book, at, scale, wallet},
    };

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
            hit_rate: 1.0,
            view_noise_bps: 0.0,
            intensity_mult: 10.0,
            size_mult: 1.0,
            limit_bps: 50.0,
            active: None,
            shy: None,
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
    async fn a_shy_informed_trader_trades_smaller_and_later_once_copied() {
        let (book, log) = Book::new("i", 99.0, 101.0);
        let (wallet, ledger) = wallet(book);
        // Up 1% from slot 1,000.
        let path: Arc<dyn Fn(u64) -> Moment + Send + Sync> =
            Arc::new(|s| at(s, if s >= 1_000 { 101.0 } else { 100.0 }));
        let spec = Informed {
            wallets: 1,
            horizon_slots: 1_000,
            edge_bps: 50.0,
            hit_rate: 1.0,
            view_noise_bps: 0.0,
            intensity_mult: 0.1,
            size_mult: 1.0,
            limit_bps: 50.0,
            active: None,
            shy: Some(Shy {
                detect_slots: 2,
                copy_share: 0.5,
                fills: 3,
                throttle: 0.2,
                cooldown_slots: 10_000,
            }),
        };
        spec.validate().unwrap();
        let mut informed =
            InformedTrader::new(spec, scale(), vec![wallet], path, Rng::new([8; 32]));
        assert!(informed.wants_flow());
        // It buys ahead of the rise, and the bot buys as much as it the step
        // after each time it does.
        let copying = StepFlow {
            bot: vec![VenueFlow {
                market: "test".into(),
                bought: 1.0,
                sold: 0.0,
            }],
        };
        let mut bought = false;
        for s in (2..=400).step_by(2) {
            informed.before_blocks(&at(s, 100.0)).await;
            if bought {
                informed.flow(&copying).await;
            }
            let step = std::mem::take(&mut *log.lock().unwrap());
            bought = !step.is_empty();
            assert!(step.iter().all(|l| l.starts_with("i take Bid")), "{step:?}");
        }
        let counts = &ledger.report().actors[0].counts;
        assert!(counts["throttles_by_bot"] >= 1, "{counts:?}");
        // Throttled, it trades as often a fifth of the time and looks a fifth
        // of its horizon ahead: at slot 500 the rise is out of sight, and
        // from 800 it buys a fifth of the size.
        informed.before_blocks(&at(500, 100.0)).await;
        assert!(log.lock().unwrap().is_empty());
        informed.before_blocks(&at(900, 100.0)).await;
        let late = log.lock().unwrap().clone();
        assert!(!late.is_empty());
        assert!(
            late.iter().all(|l| l == "i take Bid 0.200 at 100.50"),
            "{late:?}"
        );
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
            hit_rate: 1.0,
            view_noise_bps: 0.0,
            informed_for: Window {
                from: 150,
                until: Some(200),
            },
            intensity_mult: 1.0,
            size_mult: 1.0,
            limit_bps: 50.0,
            shy: None,
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
