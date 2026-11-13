//! A spoofer: a wall of size near the touch that is never meant to trade.
//!
//! After each step's blocks it looks at the book. With no wall up, it rests
//! one `distance_ticks` behind the touch on its side, `wall_mult` times the
//! market's trade size. Once the touch comes within `cancel_ticks` of the
//! wall, or the wall has stood `lifetime_slots`, it pulls it, and puts a new
//! one up at its next look. While a wall stands it may sell into the other
//! side (`harvest_mult`), which is what the wall is for. It trades from one
//! wallet throughout, so a reader of the chain can learn it: many orders,
//! nearly all cancelled, none filled.
//!
//! With `ahead_slots`, it puts a wall up only while it sees the level that
//! far ahead going the other way: support painted under a price it expects
//! to fall (a bid wall), as a seller unloading would. A wall that looks
//! like support then says the opposite, to whoever has learnt the wallet.

use std::sync::Arc;

use async_trait::async_trait;
use mtx_kit::Side;
use serde::{Deserialize, Serialize};

use super::{
    BookSide, Scale, Wallet, Window, finite_at_least_zero, finite_positive, round_down, round_up,
};
use crate::actors::{Actor, ActorsReport, Moment};

fn five() -> u64 {
    5
}

fn one_tick() -> u64 {
    1
}

fn eight() -> f64 {
    8.0
}

fn forty() -> u64 {
    40
}

/// A spoofer's settings.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Spoofer {
    /// The side its walls go on: bids hold the price up.
    #[serde(default)]
    pub side: BookSide,
    #[serde(default = "five")]
    pub distance_ticks: u64,
    #[serde(default = "one_tick")]
    pub cancel_ticks: u64,
    #[serde(default = "eight")]
    pub wall_mult: f64,
    #[serde(default = "forty")]
    pub lifetime_slots: u64,
    /// What it takes on the other side at each look while its wall stands,
    /// in market trade sizes: none by default.
    #[serde(default)]
    pub harvest_mult: f64,
    /// How far ahead it sees, if it walls only against the move it sees.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ahead_slots: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active: Option<Window>,
}

impl Default for Spoofer {
    fn default() -> Self {
        Self {
            side: BookSide::Bid,
            distance_ticks: five(),
            cancel_ticks: one_tick(),
            wall_mult: eight(),
            lifetime_slots: forty(),
            harvest_mult: 0.0,
            ahead_slots: None,
            active: None,
        }
    }
}

impl Spoofer {
    pub fn validate(&self) -> Result<(), String> {
        if self.cancel_ticks >= self.distance_ticks {
            return Err("cancel_ticks must be under distance_ticks".into());
        }
        if self.lifetime_slots == 0 {
            return Err("lifetime_slots must be at least 1".into());
        }
        if self.ahead_slots == Some(0) {
            return Err("ahead_slots must be at least 1".into());
        }
        finite_positive("wall_mult", self.wall_mult)?;
        finite_at_least_zero("harvest_mult", self.harvest_mult)
    }
}

/// A wall it has up: its price, and the slot it went up at.
#[derive(Debug, Clone, Copy)]
struct Wall {
    price: f64,
    since: u64,
}

pub(super) struct SpooferActor {
    spec: Spoofer,
    scale: Scale,
    wallet: Wallet,
    path: Arc<dyn Fn(u64) -> Moment + Send + Sync>,
    wall: Option<Wall>,
}

impl SpooferActor {
    pub fn new(
        spec: Spoofer,
        scale: Scale,
        wallet: Wallet,
        path: Arc<dyn Fn(u64) -> Moment + Send + Sync>,
    ) -> Self {
        Self {
            spec,
            scale,
            wallet,
            path,
            wall: None,
        }
    }

    /// Whether it walls `now`: always, or, with `ahead_slots`, while the
    /// level it sees ahead goes against its wall's side.
    fn walls_now(&self, now: &Moment) -> bool {
        let Some(ahead) = self.spec.ahead_slots else {
            return true;
        };
        let later = (self.path)(now.at + ahead).book_usd;
        match Side::from(self.spec.side) {
            Side::Bid => later < now.book_usd,
            Side::Ask => later > now.book_usd,
        }
    }

    async fn look(&mut self, now: &Moment) {
        let seat = self.wallet.seat();
        let tick = seat.tick();
        let Ok((bid, ask)) = seat.top().await else {
            return;
        };
        let side: Side = self.spec.side.into();
        // The touch on its side, and how near it is to the wall.
        let touch = match side {
            Side::Bid => bid,
            Side::Ask => ask,
        };
        if let Some(wall) = self.wall {
            let gap = touch.map(|t| match side {
                Side::Bid => (t - wall.price) / tick,
                Side::Ask => (wall.price - t) / tick,
            });
            let near = gap.is_none_or(|g| g <= self.spec.cancel_ticks as f64 + 0.5);
            let old = now.at >= wall.since + self.spec.lifetime_slots;
            if near || old || !Window::holds(&self.spec.active, now.at) {
                if self.wallet.cancel_all(now.at).await {
                    self.wall = None;
                    self.wallet.count(
                        if near {
                            "walls_pulled"
                        } else {
                            "walls_expired"
                        },
                        1,
                    );
                }
                return;
            }
            if self.spec.harvest_mult > 0.0 {
                // Sell into the other side the wall props up, one tick deep.
                let size = self.scale.trade_size * self.spec.harvest_mult;
                let (other, limit) = match side {
                    Side::Bid => (Side::Ask, bid.map(|b| b - tick)),
                    Side::Ask => (Side::Bid, ask.map(|a| a + tick)),
                };
                if let Some(limit) = limit {
                    self.wallet.take(now.at, other, size, limit).await;
                }
            }
            return;
        }
        if !Window::holds(&self.spec.active, now.at) || !self.walls_now(now) {
            return;
        }
        let Some(touch) = touch else {
            return;
        };
        let away = self.spec.distance_ticks as f64 * tick;
        let price = match side {
            Side::Bid => round_down(touch - away, tick),
            Side::Ask => round_up(touch + away, tick),
        };
        if price <= 0.0 {
            return;
        }
        let size = self.scale.trade_size * self.spec.wall_mult;
        if self.wallet.rest(now.at, side, price, size).await {
            self.wall = Some(Wall {
                price,
                since: now.at,
            });
            self.wallet.count("walls", 1);
        }
    }
}

#[async_trait]
impl Actor for SpooferActor {
    async fn after_blocks(&mut self, now: &Moment) {
        self.look(now).await;
    }

    async fn end(&mut self, last: &Moment) {
        if self.wall.is_some() && self.wallet.cancel_all(last.at).await {
            self.wall = None;
        }
    }

    async fn report(&mut self, _: &mut ActorsReport) {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::actors::library::tests::{Book, at, scale, wallet};

    #[tokio::test]
    async fn it_walls_behind_the_touch_and_pulls_the_wall_as_the_price_comes() {
        let (book, log) = Book::new("s", 100.00, 100.10);
        let top = book.top.clone();
        let (wallet, ledger) = wallet(book);
        let mut spoofer = SpooferActor::new(
            Spoofer {
                lifetime_slots: 100,
                ..Spoofer::default()
            },
            scale(),
            wallet,
            Arc::new(|s| at(s, 100.0)),
        );
        spoofer.after_blocks(&at(2, 100.0)).await;
        // The bid falls to 2 ticks over the wall: still up.
        *top.lock().unwrap() = (Some(99.97), Some(100.05));
        spoofer.after_blocks(&at(4, 100.0)).await;
        // One tick over it: pulled, then back up behind the new touch.
        *top.lock().unwrap() = (Some(99.96), Some(100.05));
        spoofer.after_blocks(&at(6, 100.0)).await;
        spoofer.after_blocks(&at(8, 100.0)).await;
        spoofer.end(&at(10, 100.0)).await;
        assert_eq!(
            *log.lock().unwrap(),
            [
                "s rest Bid 8.000 at 99.95",
                "s cancel",
                "s rest Bid 8.000 at 99.91",
                "s cancel",
            ]
        );
        let tally = &ledger.report().actors[0];
        assert_eq!(
            (tally.counts["walls"], tally.counts["walls_pulled"]),
            (2, 1)
        );
    }

    #[tokio::test]
    async fn an_old_wall_comes_down_and_a_harvest_sells_into_the_bids() {
        let (book, log) = Book::new("s", 100.00, 100.10);
        let (wallet, _) = wallet(book);
        let mut spoofer = SpooferActor::new(
            Spoofer {
                lifetime_slots: 4,
                harvest_mult: 0.5,
                ..Spoofer::default()
            },
            scale(),
            wallet,
            Arc::new(|s| at(s, 100.0)),
        );
        for s in [2, 4, 6] {
            spoofer.after_blocks(&at(s, 100.0)).await;
        }
        assert_eq!(
            *log.lock().unwrap(),
            [
                "s rest Bid 8.000 at 99.95",
                "s take Ask 0.500 at 99.99",
                "s cancel",
            ]
        );
    }

    #[tokio::test]
    async fn a_seller_walls_only_ahead_of_a_fall() {
        let (book, log) = Book::new("s", 100.00, 100.10);
        let (wallet, _) = wallet(book);
        // Down 1% at slot 50.
        let mut spoofer = SpooferActor::new(
            Spoofer {
                ahead_slots: Some(20),
                ..Spoofer::default()
            },
            scale(),
            wallet,
            Arc::new(|s| at(s, if s >= 50 { 99.0 } else { 100.0 })),
        );
        spoofer.after_blocks(&at(10, 100.0)).await;
        assert!(log.lock().unwrap().is_empty(), "nothing falls in sight");
        spoofer.after_blocks(&at(40, 100.0)).await;
        assert_eq!(*log.lock().unwrap(), ["s rest Bid 8.000 at 99.95"]);
    }
}
