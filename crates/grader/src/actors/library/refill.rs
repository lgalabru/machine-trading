//! A depth refill: the book's makers coming back after a take.
//!
//! The snapshot's makers are splines, curves around the oracle that only
//! move when the oracle does: a taker that eats the top of the book leaves
//! the gap there until the next path move re-centres them. A real book
//! refills, as makers requote behind a trade, at a pace that is a property
//! of the market (its resilience). This actor supplies that pace.
//!
//! At its first look it notes where the touch sits on each side relative to
//! the scenario's book level: the makers' profile. Afterwards, every
//! `react_slots`, a side whose touch has moved out from there by a gap of
//! `g` ticks gets `g (1 - 2^(-react / half_life))` of them refilled: post-only
//! orders of `size_mult` market trade sizes a tick, from the old touch back
//! toward the profile, so the gap halves every `half_life_slots`. The half-life
//! is drawn once per session from the range, from the actor's seed. It never
//! crosses the spread, never goes past the profile, and stops adding to its
//! side once its position reaches `max_position_mult` trade sizes. When the
//! book level moves, it pulls its orders: the makers have re-centred, and
//! stale quotes would be picked off.

use async_trait::async_trait;
use mtx_kit::Side;
use serde::{Deserialize, Serialize};

use super::{Rng, Scale, Wallet, Window, finite_positive};
use crate::actors::{Actor, ActorsReport, Moment};

fn five() -> u64 {
    5
}

fn two() -> u64 {
    2
}

fn half() -> f64 {
    0.5
}

fn twenty() -> f64 {
    20.0
}

fn two_ticks() -> f64 {
    2.0
}

/// A refill's settings.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Refill {
    /// The range the session's half-life is drawn from, in slots (equal
    /// ends fix it): how long a gap takes to close by half.
    pub half_life_slots: [f64; 2],
    /// The most orders it sends a side at a look: a bigger refill goes in
    /// bundles of ticks, each at the bundle's inner price.
    #[serde(default = "five")]
    pub levels: u64,
    /// Its size at each tick it refills, in the market's trade sizes.
    #[serde(default = "half")]
    pub size_mult: f64,
    /// Slots between looks.
    #[serde(default = "two")]
    pub react_slots: u64,
    /// The position, in trade sizes, past which it stops adding to a side.
    #[serde(default = "twenty")]
    pub max_position_mult: f64,
    /// A gap under this many ticks is the book's own rounding, not a take.
    #[serde(default = "two_ticks")]
    pub min_gap_ticks: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active: Option<Window>,
}

impl Refill {
    pub fn validate(&self) -> Result<(), String> {
        let [lo, hi] = self.half_life_slots;
        finite_positive("half_life_slots", lo)?;
        finite_positive("half_life_slots", hi)?;
        if lo > hi {
            return Err("half_life_slots must run from low to high".into());
        }
        if self.levels == 0 || self.react_slots == 0 {
            return Err("levels and react_slots must be at least 1".into());
        }
        finite_positive("size_mult", self.size_mult)?;
        finite_positive("max_position_mult", self.max_position_mult)?;
        finite_positive("min_gap_ticks", self.min_gap_ticks)
    }
}

/// Where the makers' touch sits on each side, as a share of the book level.
#[derive(Debug, Clone, Copy)]
struct Profile {
    bid: f64,
    ask: f64,
}

pub(super) struct RefillActor {
    spec: Refill,
    scale: Scale,
    wallet: Wallet,
    half_life: f64,
    profile: Option<Profile>,
    /// The book level its orders were placed against, while it has any.
    placed_at: Option<f64>,
    /// Ticks owed to each side that a look couldn't yet send whole.
    owed: [f64; 2],
    next_look: u64,
}

impl RefillActor {
    pub fn new(spec: Refill, scale: Scale, wallet: Wallet, mut rng: Rng) -> Self {
        let [lo, hi] = spec.half_life_slots;
        let half_life = lo + (hi - lo) * rng.uniform();
        Self {
            spec,
            scale,
            wallet,
            half_life,
            profile: None,
            placed_at: None,
            owed: [0.0; 2],
            next_look: 0,
        }
    }

    /// The share of a gap that closes at each look.
    fn closing(&self) -> f64 {
        1.0 - 0.5f64.powf(self.spec.react_slots as f64 / self.half_life)
    }

    async fn look(&mut self, now: &Moment) {
        if now.at < self.next_look {
            return;
        }
        self.next_look = now.at + self.spec.react_slots;
        let seat = self.wallet.seat();
        let tick = seat.tick();
        let book = now.book_usd;
        if let Some(placed) = self.placed_at
            && (book - placed).abs() >= 2.0 * tick
        {
            if !self.wallet.cancel_all(now.at).await {
                return;
            }
            self.placed_at = None;
            self.owed = [0.0; 2];
        }
        let Ok((Some(bid), Some(ask))) = seat.top().await else {
            return;
        };
        let Some(profile) = self.profile else {
            self.profile = Some(Profile {
                bid: (book - bid) / book,
                ask: (ask - book) / book,
            });
            return;
        };
        if !Window::holds(&self.spec.active, now.at) {
            return;
        }
        let held = seat.position().await.unwrap_or(0.0);
        let cap = self.spec.max_position_mult * self.scale.trade_size;
        let size = self.scale.trade_size * self.spec.size_mult;
        let closing = self.closing();
        // Asks come back down toward the profile's, bids up.
        for (n, side) in [Side::Ask, Side::Bid].into_iter().enumerate() {
            let room = (ask - bid) / tick - 1.0;
            let (touch, target) = match side {
                Side::Ask => (ask, book * (1.0 + profile.ask)),
                Side::Bid => (bid, book * (1.0 - profile.bid)),
            };
            let gap = match side {
                Side::Ask => (touch - target) / tick,
                Side::Bid => (target - touch) / tick,
            }
            .round();
            let full = match side {
                Side::Ask => held <= -cap,
                Side::Bid => held >= cap,
            };
            if gap < self.spec.min_gap_ticks || full {
                self.owed[n] = 0.0;
                continue;
            }
            self.owed[n] += gap * closing;
            let ticks = (self.owed[n].floor()).min(gap).min(room.floor()).max(0.0) as u64;
            if ticks == 0 {
                continue;
            }
            self.owed[n] -= ticks as f64;
            // Ticks 1..=ticks off the old touch, in at most `levels` bundles.
            let per = ticks.div_ceil(self.spec.levels);
            let mut done = 0;
            while done < ticks {
                let count = per.min(ticks - done);
                let inner = done + count;
                let n = (touch / tick).round();
                let n = match side {
                    Side::Ask => n - inner as f64,
                    Side::Bid => n + inner as f64,
                };
                // The grid price as a clean decimal: the seat reads it as
                // text and rounds asks up, so a stray digit costs a tick.
                let price = (n * tick * 1e8).round() / 1e8;
                if self
                    .wallet
                    .rest(now.at, side, price, size * count as f64)
                    .await
                {
                    self.placed_at = Some(book);
                    self.wallet.count("ticks_refilled", count);
                }
                done += count;
            }
        }
    }
}

#[async_trait]
impl Actor for RefillActor {
    async fn after_blocks(&mut self, now: &Moment) {
        self.look(now).await;
    }

    async fn end(&mut self, last: &Moment) {
        if self.placed_at.is_some() && self.wallet.cancel_all(last.at).await {
            self.placed_at = None;
        }
    }

    async fn report(&mut self, _: &mut ActorsReport) {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::actors::library::tests::{Book, at, scale, wallet};

    fn spec(half_life: f64) -> Refill {
        Refill {
            half_life_slots: [half_life, half_life],
            levels: 5,
            size_mult: 0.5,
            react_slots: 2,
            max_position_mult: 20.0,
            min_gap_ticks: 2.0,
            active: None,
        }
    }

    fn actor(half_life: f64, book: Book) -> RefillActor {
        let (wallet, _) = wallet(book);
        RefillActor::new(spec(half_life), scale(), wallet, Rng::new([3; 32]))
    }

    #[tokio::test]
    async fn a_gap_closes_by_half_in_a_half_life_and_never_past_the_profile() {
        let (book, log) = Book::new("r", 99.90, 100.10);
        let top = book.top.clone();
        let mut refill = actor(4.0, book);
        refill.after_blocks(&at(0, 100.0)).await;
        // A take clears the ask 20 ticks out.
        *top.lock().unwrap() = (Some(99.90), Some(100.30));
        refill.after_blocks(&at(2, 100.0)).await;
        // 2 slots of a 4-slot half-life closes 29.3% of 20 ticks: 5, at
        // one order a tick.
        let sent = log.lock().unwrap().clone();
        assert_eq!(
            sent,
            [
                "r rest Ask 0.500 at 100.29",
                "r rest Ask 0.500 at 100.28",
                "r rest Ask 0.500 at 100.27",
                "r rest Ask 0.500 at 100.26",
                "r rest Ask 0.500 at 100.25",
            ]
        );
        // The profile's own touch, or within the rounding: nothing.
        log.lock().unwrap().clear();
        *top.lock().unwrap() = (Some(99.90), Some(100.11));
        refill.after_blocks(&at(4, 100.0)).await;
        assert!(log.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_big_refill_goes_in_bundles_and_never_crosses_the_spread() {
        let (book, log) = Book::new("r", 99.90, 100.10);
        let top = book.top.clone();
        let mut refill = actor(1.0, book);
        refill.after_blocks(&at(0, 100.0)).await;
        // 60 ticks out; the bid is 70 ticks below it.
        *top.lock().unwrap() = (Some(99.90), Some(100.70));
        refill.after_blocks(&at(2, 100.0)).await;
        let sent = log.lock().unwrap().clone();
        assert_eq!(sent.len(), 5, "{sent:?}");
        // 75% of 60 = 45 ticks, bundled by 9, the innermost at 100.25.
        assert_eq!(sent[4], "r rest Ask 4.500 at 100.25");
        // A book that runs one tick wide can't be refilled.
        log.lock().unwrap().clear();
        *top.lock().unwrap() = (Some(100.69), Some(100.70));
        refill.after_blocks(&at(4, 100.0)).await;
        assert!(
            log.lock().unwrap().iter().all(|l| !l.contains("Ask")),
            "{:?}",
            log.lock().unwrap()
        );
    }

    #[tokio::test]
    async fn it_pulls_its_orders_when_the_book_moves_and_stops_at_its_cap() {
        let (book, log) = Book::new("r", 99.90, 100.10);
        let top = book.top.clone();
        let position = book.position.clone();
        let mut refill = actor(4.0, book);
        refill.after_blocks(&at(0, 100.0)).await;
        *top.lock().unwrap() = (Some(99.90), Some(100.30));
        refill.after_blocks(&at(2, 100.0)).await;
        log.lock().unwrap().clear();
        // The makers re-centre 1% up.
        *top.lock().unwrap() = (Some(100.90), Some(101.10));
        refill.after_blocks(&at(4, 101.0)).await;
        assert_eq!(*log.lock().unwrap(), ["r cancel"]);
        // Short at its cap: no more asks.
        log.lock().unwrap().clear();
        *position.lock().unwrap() = -20.0;
        *top.lock().unwrap() = (Some(100.90), Some(101.40));
        refill.after_blocks(&at(6, 101.0)).await;
        assert!(log.lock().unwrap().is_empty());
    }

    #[test]
    fn the_half_life_is_drawn_from_the_range_by_seed() {
        let draw = |seed: u8| {
            let (book, _) = Book::new("r", 99.9, 100.1);
            let (wallet, _) = wallet(book);
            let mut s = spec(4.0);
            s.half_life_slots = [4.0, 40.0];
            RefillActor::new(s, scale(), wallet, Rng::new([seed; 32])).half_life
        };
        assert_eq!(draw(1), draw(1));
        assert_ne!(draw(1), draw(2));
        assert!((4.0..=40.0).contains(&draw(1)));
        let mut bad = spec(4.0);
        bad.half_life_slots = [10.0, 4.0];
        assert!(bad.validate().is_err());
    }
}
