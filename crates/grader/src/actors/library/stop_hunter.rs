//! A stop hunter: it waits for the price to come near an obvious level (a
//! round number on a `level_step_usd` grid), pushes it through with one
//! large take, and trades back out `hold_slots` later, once the stops it
//! was after have fired and the price has had time to return.

use async_trait::async_trait;
use mtx_kit::Side;
use serde::{Deserialize, Serialize};

use super::{Scale, Wallet, Window, finite_positive};
use crate::actors::{Actor, ActorsReport, Moment};

fn ten() -> f64 {
    10.0
}

fn twenty_slots() -> u64 {
    20
}

fn hundred_slots() -> u64 {
    100
}

fn three_ticks() -> u64 {
    3
}

fn fifty() -> f64 {
    50.0
}

/// A stop hunter's settings.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct StopHunter {
    /// The grid of obvious levels, in USD: every whole dollar at 1.0.
    pub level_step_usd: f64,
    /// How near the mid must come to a level, in basis points, before it
    /// pushes.
    #[serde(default = "ten")]
    pub trigger_bps: f64,
    /// Its push, in market trade sizes.
    #[serde(default = "ten")]
    pub push_mult: f64,
    /// How far through the level the push may fill.
    #[serde(default = "three_ticks")]
    pub overshoot_ticks: u64,
    #[serde(default = "twenty_slots")]
    pub hold_slots: u64,
    /// Slots after a reversal before it hunts again.
    #[serde(default = "hundred_slots")]
    pub cooldown_slots: u64,
    /// How far past the price its reversal may fill, in basis points.
    #[serde(default = "fifty")]
    pub limit_bps: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active: Option<Window>,
}

impl StopHunter {
    pub fn validate(&self) -> Result<(), String> {
        finite_positive("level_step_usd", self.level_step_usd)?;
        finite_positive("trigger_bps", self.trigger_bps)?;
        finite_positive("push_mult", self.push_mult)?;
        finite_positive("limit_bps", self.limit_bps)
    }
}

enum State {
    Waiting {
        from: u64,
    },
    /// It pushed `side` for `size` at `at`.
    Pushed {
        side: Side,
        size: f64,
        at: u64,
    },
}

pub(super) struct Hunter {
    spec: StopHunter,
    scale: Scale,
    wallet: Wallet,
    state: State,
}

impl Hunter {
    pub fn new(spec: StopHunter, scale: Scale, wallet: Wallet) -> Self {
        Self {
            spec,
            scale,
            wallet,
            state: State::Waiting { from: 0 },
        }
    }

    async fn look(&mut self, now: &Moment) {
        let tick = self.wallet.seat().tick();
        match self.state {
            State::Pushed { side, size, at } => {
                if now.at < at + self.spec.hold_slots {
                    return;
                }
                let back = side.opposite();
                let reach = self.spec.limit_bps / 10_000.0;
                let limit = match back {
                    Side::Bid => now.book_usd * (1.0 + reach),
                    Side::Ask => now.book_usd * (1.0 - reach),
                };
                self.wallet.take(now.at, back, size, limit).await;
                self.wallet.count("reversals", 1);
                self.state = State::Waiting {
                    from: now.at + self.spec.cooldown_slots,
                };
            }
            State::Waiting { from } => {
                if now.at < from || !Window::holds(&self.spec.active, now.at) {
                    return;
                }
                let Ok((Some(bid), Some(ask))) = self.wallet.seat().top().await else {
                    return;
                };
                let mid = (bid + ask) / 2.0;
                let step = self.spec.level_step_usd;
                let below = (mid / step).floor() * step;
                let above = below + step;
                let near =
                    |level: f64| (mid - level).abs() / mid * 10_000.0 <= self.spec.trigger_bps;
                let overshoot = self.spec.overshoot_ticks as f64 * tick;
                // The nearer level, if near enough: push through it.
                let (side, limit) = if mid - below <= above - mid {
                    if !near(below) {
                        return;
                    }
                    (Side::Ask, below - overshoot)
                } else {
                    if !near(above) {
                        return;
                    }
                    (Side::Bid, above + overshoot)
                };
                let size = self.scale.trade_size * self.spec.push_mult;
                if self.wallet.take(now.at, side, size, limit).await {
                    self.wallet.count("pushes", 1);
                    self.state = State::Pushed {
                        side,
                        size,
                        at: now.at,
                    };
                }
            }
        }
    }
}

#[async_trait]
impl Actor for Hunter {
    async fn after_blocks(&mut self, now: &Moment) {
        self.look(now).await;
    }

    async fn report(&mut self, _: &mut ActorsReport) {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::actors::library::tests::{Book, at, scale, wallet};

    #[tokio::test]
    async fn it_pushes_through_a_round_number_then_trades_back() {
        let (book, log) = Book::new("h", 100.20, 100.30);
        let top = book.top.clone();
        let (wallet, ledger) = wallet(book);
        let spec = StopHunter {
            level_step_usd: 1.0,
            trigger_bps: 10.0,
            push_mult: 10.0,
            overshoot_ticks: 3,
            hold_slots: 10,
            cooldown_slots: 100,
            limit_bps: 50.0,
            active: None,
        };
        let mut hunter = Hunter::new(spec, scale(), wallet);
        // 25 bps over 100: too far.
        hunter.after_blocks(&at(2, 100.0)).await;
        assert!(log.lock().unwrap().is_empty());
        *top.lock().unwrap() = (Some(100.04), Some(100.08));
        hunter.after_blocks(&at(4, 100.0)).await;
        hunter.after_blocks(&at(8, 100.0)).await;
        hunter.after_blocks(&at(14, 99.9)).await;
        // Cooling down.
        hunter.after_blocks(&at(20, 99.9)).await;
        assert_eq!(
            *log.lock().unwrap(),
            ["h take Ask 10.000 at 99.97", "h take Bid 10.000 at 100.40"]
        );
        let tally = &ledger.report().actors[0];
        assert_eq!((tally.counts["pushes"], tally.counts["reversals"]), (1, 1));
    }
}
