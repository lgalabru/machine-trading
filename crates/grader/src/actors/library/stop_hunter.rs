//! A stop hunter: it waits for the price to come near an obvious level (a
//! round number on a `level_step_usd` grid), pushes it through with one
//! large take, and trades back out `hold_slots` later, once the stops it
//! was after have fired and the price has had time to return. A scenario
//! may schedule its hunts (`at_slots`): a generator that moves the market
//! through the run puts the hunter where the run starts.

use async_trait::async_trait;
use mtx_kit::Side;
use serde::{Deserialize, Serialize};

use super::{Rng, Scale, Wallet, Window, finite_positive};
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

fn one_wallet() -> u32 {
    1
}

fn is_one(wallets: &u32) -> bool {
    *wallets == 1
}

/// A stop hunter's settings.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct StopHunter {
    /// The wallets it trades from. With several it pushes from one of them,
    /// drawn each time, and trades back from the same one: a reader sees no
    /// single address that hunts.
    #[serde(default = "one_wallet", skip_serializing_if = "is_one")]
    pub wallets: u32,
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
    /// When set, it hunts only from these slots (ascending, into the
    /// session): once each, the first time from that slot on that the price
    /// is near a level, and never again for that slot once the next has
    /// come. Without them it hunts whenever it is near one and cooled down.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub at_slots: Vec<u64>,
    /// How far past the price its reversal may fill, in basis points.
    #[serde(default = "fifty")]
    pub limit_bps: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active: Option<Window>,
}

impl StopHunter {
    pub fn validate(&self) -> Result<(), String> {
        if self.wallets == 0 {
            return Err("wallets must be at least 1".into());
        }
        finite_positive("level_step_usd", self.level_step_usd)?;
        finite_positive("trigger_bps", self.trigger_bps)?;
        finite_positive("push_mult", self.push_mult)?;
        if self.at_slots.windows(2).any(|w| w[0] >= w[1]) {
            return Err("at_slots must be ascending, each once".into());
        }
        finite_positive("limit_bps", self.limit_bps)
    }
}

enum State {
    Waiting {
        from: u64,
    },
    /// It pushed `side` for `size` at `at`, from its `wallet`th wallet.
    Pushed {
        side: Side,
        size: f64,
        at: u64,
        wallet: usize,
    },
}

pub(super) struct Hunter {
    spec: StopHunter,
    scale: Scale,
    wallets: Vec<Wallet>,
    rng: Rng,
    state: State,
    /// How many of `at_slots` are spent: hunted, or passed by the next.
    spent: usize,
}

impl Hunter {
    pub fn new(spec: StopHunter, scale: Scale, wallets: Vec<Wallet>, rng: Rng) -> Self {
        Self {
            spec,
            scale,
            wallets,
            rng,
            state: State::Waiting { from: 0 },
            spent: 0,
        }
    }

    /// Whether a scheduled hunt is due at `at`, skipping those the next one
    /// has passed; always, without a schedule.
    fn due(&mut self, at: u64) -> bool {
        let slots = &self.spec.at_slots;
        if slots.is_empty() {
            return true;
        }
        while self.spent + 1 < slots.len() && slots[self.spent + 1] <= at {
            self.spent += 1;
        }
        self.spent < slots.len() && slots[self.spent] <= at
    }

    async fn look(&mut self, now: &Moment) {
        let tick = self.wallets[0].seat().tick();
        match self.state {
            State::Pushed {
                side,
                size,
                at,
                wallet,
            } => {
                if now.at < at + self.spec.hold_slots {
                    return;
                }
                let back = side.opposite();
                let reach = self.spec.limit_bps / 10_000.0;
                let limit = match back {
                    Side::Bid => now.book_usd * (1.0 + reach),
                    Side::Ask => now.book_usd * (1.0 - reach),
                };
                let wallet = &self.wallets[wallet];
                wallet.take(now.at, back, size, limit).await;
                wallet.count("reversals", 1);
                self.state = State::Waiting {
                    from: now.at + self.spec.cooldown_slots,
                };
            }
            State::Waiting { from } => {
                if now.at < from || !Window::holds(&self.spec.active, now.at) || !self.due(now.at)
                {
                    return;
                }
                let Ok((Some(bid), Some(ask))) = self.wallets[0].seat().top().await else {
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
                let wallet = match self.wallets.len() {
                    1 => 0,
                    n => self.rng.below(n),
                };
                if self.wallets[wallet].take(now.at, side, size, limit).await {
                    self.wallets[wallet].count("pushes", 1);
                    if !self.spec.at_slots.is_empty() {
                        self.spent += 1;
                    }
                    self.state = State::Pushed {
                        side,
                        size,
                        at: now.at,
                        wallet,
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
            wallets: 1,
            level_step_usd: 1.0,
            trigger_bps: 10.0,
            push_mult: 10.0,
            overshoot_ticks: 3,
            hold_slots: 10,
            cooldown_slots: 100,
            at_slots: Vec::new(),
            limit_bps: 50.0,
            active: None,
        };
        let mut hunter = Hunter::new(spec, scale(), vec![wallet], Rng::new([1; 32]));
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

    #[tokio::test]
    async fn with_a_schedule_it_hunts_once_from_each_slot_and_skips_a_passed_one() {
        let (book, log) = Book::new("h", 100.04, 100.08);
        let (wallet, _) = wallet(book);
        let spec = StopHunter {
            wallets: 1,
            level_step_usd: 1.0,
            trigger_bps: 10.0,
            push_mult: 1.0,
            overshoot_ticks: 3,
            hold_slots: 2,
            cooldown_slots: 0,
            at_slots: vec![10, 30, 34, 60],
            limit_bps: 50.0,
            active: None,
        };
        let mut hunter = Hunter::new(spec, scale(), vec![wallet], Rng::new([1; 32]));
        let pushes = || {
            log.lock()
                .unwrap()
                .iter()
                .filter(|l| l.contains("take Ask"))
                .count()
        };
        // Near a level from the start, but nothing is due before slot 10.
        for slot in [0, 2, 8] {
            hunter.after_blocks(&at(slot, 100.0)).await;
        }
        assert_eq!(pushes(), 0);
        // Slot 10's hunt, and its reversal; then nothing until 30.
        for slot in [10, 12, 14, 20, 28] {
            hunter.after_blocks(&at(slot, 100.0)).await;
        }
        assert_eq!(pushes(), 1);
        // At 36 both 30 and 34 have come: one hunt, for 34.
        for slot in [36, 38, 40, 50] {
            hunter.after_blocks(&at(slot, 100.0)).await;
        }
        assert_eq!(pushes(), 2);
        for slot in [60, 62, 64, 90] {
            hunter.after_blocks(&at(slot, 100.0)).await;
        }
        assert_eq!(pushes(), 3);
        let bad = StopHunter {
            at_slots: vec![30, 30],
            ..hunter.spec.clone()
        };
        assert!(bad.validate().is_err());
    }

    #[tokio::test]
    async fn with_several_wallets_each_hunt_runs_from_one_drawn_wallet_and_back() {
        let ledger_seats = |name: &str| {
            let (book, log) = Book::new(name, 100.04, 100.08);
            (wallet(book).0, log)
        };
        let ((a, log_a), (b, log_b)) = (ledger_seats("a"), ledger_seats("b"));
        let spec = StopHunter {
            wallets: 2,
            level_step_usd: 1.0,
            trigger_bps: 10.0,
            push_mult: 1.0,
            overshoot_ticks: 3,
            hold_slots: 2,
            cooldown_slots: 0,
            at_slots: Vec::new(),
            limit_bps: 50.0,
            active: None,
        };
        let mut hunter = Hunter::new(spec, scale(), vec![a, b], Rng::new([9; 32]));
        for hunt in 0..16 {
            hunter.after_blocks(&at(10 * hunt, 100.0)).await;
            hunter.after_blocks(&at(10 * hunt + 2, 100.0)).await;
        }
        let (a, b) = (log_a.lock().unwrap(), log_b.lock().unwrap());
        assert!(
            !a.is_empty() && !b.is_empty(),
            "both wallets hunt: {a:?} {b:?}"
        );
        assert_eq!(a.len() + b.len(), 32);
        // Each wallet's hunts are a push and its own reversal, in turn.
        for log in [&*a, &*b] {
            for pair in log.chunks(2) {
                assert!(pair[0].contains("take Ask") && pair[1].contains("take Bid"));
            }
        }
    }
}
