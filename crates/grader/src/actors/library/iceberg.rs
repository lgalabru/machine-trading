//! An iceberg: a large parent order shown a slice at a time. It rests
//! `display_mult` trade sizes at a fixed price, and when the slice trades it
//! puts the same size back at the same price until the parent
//! (`parent_mult` trade sizes) is done. Where a spoofer's wall is pulled as
//! the touch comes near, this one stands: a reader of the chain sees an
//! order that refills after each fill, at one price, and never leaves.
//!
//! It trades from one wallet of its own, and reads its fills from that
//! wallet's position, so the wallet must be nobody else's. It puts a slice
//! up `distance_ticks` behind the touch on its side; if the touch moves
//! more than `max_away_ticks` from it, it takes the slice down and puts it
//! up again behind the new touch.

use async_trait::async_trait;
use mtx_kit::Side;
use serde::{Deserialize, Serialize};

use super::{BookSide, Scale, Wallet, Window, finite_positive, round_down, round_up};
use crate::actors::{Actor, ActorsReport, Moment};

fn one() -> f64 {
    1.0
}

fn six() -> f64 {
    6.0
}

fn one_tick() -> u64 {
    1
}

fn ten_ticks() -> u64 {
    10
}

/// An iceberg's settings.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Iceberg {
    /// The side its parent order is on: bids buy.
    #[serde(default)]
    pub side: BookSide,
    /// Ticks behind the touch its slice goes up at.
    #[serde(default = "one_tick")]
    pub distance_ticks: u64,
    /// What it shows at a time, in market trade sizes.
    #[serde(default = "one")]
    pub display_mult: f64,
    /// The whole parent, in market trade sizes.
    #[serde(default = "six")]
    pub parent_mult: f64,
    /// How far the touch may move from its slice, in ticks, before the slice
    /// is moved.
    #[serde(default = "ten_ticks")]
    pub max_away_ticks: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active: Option<Window>,
}

impl Default for Iceberg {
    fn default() -> Self {
        Self {
            side: BookSide::Bid,
            distance_ticks: one_tick(),
            display_mult: one(),
            parent_mult: six(),
            max_away_ticks: ten_ticks(),
            active: None,
        }
    }
}

impl Iceberg {
    pub fn validate(&self) -> Result<(), String> {
        finite_positive("display_mult", self.display_mult)?;
        finite_positive("parent_mult", self.parent_mult)?;
        if self.parent_mult < self.display_mult {
            return Err("parent_mult must be at least display_mult".into());
        }
        if self.max_away_ticks <= self.distance_ticks {
            return Err("max_away_ticks must be over distance_ticks".into());
        }
        Ok(())
    }
}

pub(super) struct IcebergActor {
    spec: Iceberg,
    scale: Scale,
    wallet: Wallet,
    /// The price its slice stands at, once one does.
    standing: Option<f64>,
    /// The wallet's position at its last look, once it has looked.
    position: Option<f64>,
    /// What is left of the parent, in base units.
    left: f64,
}

impl IcebergActor {
    pub fn new(spec: Iceberg, scale: Scale, wallet: Wallet) -> Self {
        let left = scale.trade_size * spec.parent_mult;
        Self {
            spec,
            scale,
            wallet,
            standing: None,
            position: None,
            left,
        }
    }

    fn slice(&self) -> f64 {
        (self.scale.trade_size * self.spec.display_mult).min(self.left)
    }

    async fn show(&mut self, at: u64, side: Side, price: f64) {
        if self.wallet.rest(at, side, price, self.slice()).await {
            self.standing = Some(price);
        }
    }

    async fn look(&mut self, now: &Moment) {
        let seat = self.wallet.seat();
        let tick = seat.tick();
        let side: Side = self.spec.side.into();
        let (Ok((bid, ask)), Ok(position)) = (seat.top().await, seat.position().await) else {
            return;
        };
        // What its slices have filled since the last look.
        let filled = self
            .position
            .replace(position)
            .map_or(0.0, |before| match side {
                Side::Bid => position - before,
                Side::Ask => before - position,
            });
        let filled = filled.max(0.0);
        if filled > 0.0 {
            self.left = (self.left - filled).max(0.0);
        }
        let done = self.left <= self.scale.trade_size * 1e-9;
        if done || !Window::holds(&self.spec.active, now.at) {
            if self.standing.take().is_some() {
                self.wallet.cancel_all(now.at).await;
            }
            return;
        }
        let touch = match side {
            Side::Bid => bid,
            Side::Ask => ask,
        };
        if let Some(price) = self.standing {
            let away = touch.map_or(0.0, |t| match side {
                Side::Bid => (t - price) / tick,
                Side::Ask => (price - t) / tick,
            });
            if away > self.spec.max_away_ticks as f64 + 0.5 {
                self.standing = None;
                if self.wallet.cancel_all(now.at).await {
                    self.wallet.count("moves", 1);
                }
                return;
            }
            if filled > 0.0 {
                // The slice traded: take what is left of it down and put a
                // whole one back at the same price.
                self.standing = None;
                if self.wallet.cancel_all(now.at).await {
                    self.show(now.at, side, price).await;
                    if self.standing.is_some() {
                        self.wallet.count("reloads", 1);
                    }
                }
            }
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
        self.show(now.at, side, price).await;
        if self.standing.is_some() {
            self.wallet.count("slices", 1);
        }
    }
}

#[async_trait]
impl Actor for IcebergActor {
    async fn after_blocks(&mut self, now: &Moment) {
        self.look(now).await;
    }

    async fn end(&mut self, last: &Moment) {
        if self.standing.take().is_some() {
            self.wallet.cancel_all(last.at).await;
        }
    }

    async fn report(&mut self, _: &mut ActorsReport) {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::actors::library::tests::{Book, at, scale, wallet};

    fn iceberg(spec: Iceberg) -> (IcebergActor, Book) {
        let (book, _) = Book::new("i", 100.00, 100.10);
        let handle = Book {
            top: book.top.clone(),
            log: book.log.clone(),
            position: book.position.clone(),
            name: book.name.clone(),
        };
        let (wallet, _) = wallet(book);
        (IcebergActor::new(spec, scale(), wallet), handle)
    }

    #[tokio::test]
    async fn it_reloads_its_slice_at_the_same_price_until_the_parent_is_done() {
        let (book, log) = Book::new("i", 100.00, 100.10);
        let (top, position) = (book.top.clone(), book.position.clone());
        let (wallet, ledger) = wallet(book);
        let spec = Iceberg {
            display_mult: 1.0,
            parent_mult: 2.5,
            ..Iceberg::default()
        };
        let mut iceberg = IcebergActor::new(spec, scale(), wallet);
        iceberg.after_blocks(&at(2, 100.0)).await;
        // Nothing traded: it stands.
        iceberg.after_blocks(&at(4, 100.0)).await;
        // The slice trades; the touch drops with it. The slice is back at 99.99.
        *position.lock().unwrap() = 1.0;
        *top.lock().unwrap() = (Some(99.90), Some(100.00));
        iceberg.after_blocks(&at(6, 100.0)).await;
        // Traded again: 0.5 of the parent left, shown as 0.5.
        *position.lock().unwrap() = 2.0;
        iceberg.after_blocks(&at(8, 100.0)).await;
        // The rest trades: done, nothing stands.
        *position.lock().unwrap() = 2.5;
        iceberg.after_blocks(&at(10, 100.0)).await;
        iceberg.after_blocks(&at(12, 100.0)).await;
        assert_eq!(
            *log.lock().unwrap(),
            [
                "i rest Bid 1.000 at 99.99",
                "i cancel",
                "i rest Bid 1.000 at 99.99",
                "i cancel",
                "i rest Bid 0.500 at 99.99",
                "i cancel",
            ]
        );
        let tally = &ledger.report().actors[0];
        assert_eq!((tally.counts["slices"], tally.counts["reloads"]), (1, 2));
    }

    #[tokio::test]
    async fn an_ask_iceberg_reads_its_fills_as_a_falling_position() {
        let (book, log) = Book::new("i", 100.00, 100.10);
        let position = book.position.clone();
        let (wallet, _) = wallet(book);
        let spec = Iceberg {
            side: BookSide::Ask,
            distance_ticks: 0,
            ..Iceberg::default()
        };
        let mut iceberg = IcebergActor::new(spec, scale(), wallet);
        iceberg.after_blocks(&at(2, 100.0)).await;
        *position.lock().unwrap() = -1.0;
        iceberg.after_blocks(&at(4, 100.0)).await;
        // A position rising on an ask side is not a fill.
        *position.lock().unwrap() = 3.0;
        iceberg.after_blocks(&at(6, 100.0)).await;
        assert_eq!(
            *log.lock().unwrap(),
            [
                "i rest Ask 1.000 at 100.10",
                "i cancel",
                "i rest Ask 1.000 at 100.10",
            ]
        );
    }

    #[tokio::test]
    async fn a_slice_the_touch_has_left_behind_is_put_up_again_behind_the_new_touch() {
        let (mut iceberg, handle) = iceberg(Iceberg {
            max_away_ticks: 5,
            ..Iceberg::default()
        });
        iceberg.after_blocks(&at(2, 100.0)).await;
        // Four ticks away: it stays.
        *handle.top.lock().unwrap() = (Some(100.03), Some(100.13));
        iceberg.after_blocks(&at(4, 100.0)).await;
        // Eleven ticks: pulled, then put up again behind the new touch.
        *handle.top.lock().unwrap() = (Some(100.10), Some(100.20));
        iceberg.after_blocks(&at(6, 100.0)).await;
        iceberg.after_blocks(&at(8, 100.0)).await;
        assert_eq!(
            *handle.log.lock().unwrap(),
            [
                "i rest Bid 1.000 at 99.99",
                "i cancel",
                "i rest Bid 1.000 at 100.09",
            ]
        );
    }

    #[tokio::test]
    async fn it_shows_nothing_outside_its_window_and_takes_its_slice_down_at_the_end() {
        let (mut iceberg, handle) = iceberg(Iceberg {
            active: Some(Window {
                from: 4,
                until: Some(8),
            }),
            ..Iceberg::default()
        });
        iceberg.after_blocks(&at(2, 100.0)).await;
        assert!(handle.log.lock().unwrap().is_empty());
        iceberg.after_blocks(&at(4, 100.0)).await;
        iceberg.after_blocks(&at(8, 100.0)).await;
        iceberg.after_blocks(&at(10, 100.0)).await;
        iceberg.end(&at(12, 100.0)).await;
        assert_eq!(
            *handle.log.lock().unwrap(),
            ["i rest Bid 1.000 at 99.99", "i cancel"]
        );
    }

    #[test]
    fn its_settings_are_checked() {
        assert!(Iceberg::default().validate().is_ok());
        let bad = |f: fn(&mut Iceberg)| {
            let mut spec = Iceberg::default();
            f(&mut spec);
            spec.validate().is_err()
        };
        assert!(bad(|s| s.display_mult = 0.0));
        assert!(bad(|s| s.parent_mult = f64::NAN));
        assert!(bad(|s| s.parent_mult = 0.5));
        assert!(bad(|s| s.max_away_ticks = 1));
    }
}
