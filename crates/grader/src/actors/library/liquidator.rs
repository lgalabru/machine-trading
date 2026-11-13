//! A competing liquidator: it races the bot for liquidations on a lending
//! venue. Every `latency_slots` it asks the venue which positions can be
//! liquidated ([`Liquidations::candidates`]) and liquidates each whose bonus
//! is at least `min_bonus_bps`, best first.
//!
//! No session has a lending venue yet (the Kamino venue brings one), so a
//! session builds it with none and it stays idle, saying so in its counts.
//! A lending venue implements [`Liquidations`] and hands it over through
//! [`super::Setting::liquidations`].

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use super::{Wallet, finite_at_least_zero};
use crate::actors::{Actor, ActorsReport, Moment};

fn two() -> u64 {
    2
}

/// The liquidator's settings.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Liquidator {
    #[serde(default = "two")]
    pub latency_slots: u64,
    #[serde(default)]
    pub min_bonus_bps: f64,
}

impl Liquidator {
    pub fn validate(&self) -> Result<(), String> {
        if self.latency_slots == 0 {
            return Err("latency_slots must be at least 1".into());
        }
        finite_at_least_zero("min_bonus_bps", self.min_bonus_bps)
    }
}

/// A position a lending venue would let anyone liquidate.
#[derive(Debug, Clone, PartialEq)]
pub struct Candidate {
    /// The position, as the venue names it (an obligation's address).
    pub position: String,
    /// The liquidation bonus, in basis points of what is repaid.
    pub bonus_bps: f64,
}

/// What a lending venue offers a liquidator.
#[async_trait]
pub trait Liquidations: Send + Sync {
    /// The positions that can be liquidated now.
    async fn candidates(&self) -> Vec<Candidate>;
    /// Liquidate `candidate` from `wallet`.
    async fn liquidate(&self, wallet: &Wallet, at: u64, candidate: &Candidate) -> bool;
}

pub(super) struct Competing {
    spec: Liquidator,
    wallet: Wallet,
    venue: Option<Box<dyn Liquidations>>,
    next: u64,
    idle_noted: bool,
}

impl Competing {
    pub fn new(spec: Liquidator, wallet: Wallet, venue: Option<Box<dyn Liquidations>>) -> Self {
        Self {
            spec,
            wallet,
            venue,
            next: 0,
            idle_noted: false,
        }
    }
}

#[async_trait]
impl Actor for Competing {
    async fn after_blocks(&mut self, now: &Moment) {
        let Some(venue) = &self.venue else {
            if !self.idle_noted {
                self.wallet.count("no_lending_venue", 1);
                self.idle_noted = true;
            }
            return;
        };
        if now.at < self.next {
            return;
        }
        self.next = now.at + self.spec.latency_slots;
        let mut candidates: Vec<Candidate> = venue
            .candidates()
            .await
            .into_iter()
            .filter(|c| c.bonus_bps >= self.spec.min_bonus_bps)
            .collect();
        // Best bonus first; ties by name, so the order replays.
        candidates.sort_by(|a, b| {
            b.bonus_bps
                .total_cmp(&a.bonus_bps)
                .then_with(|| a.position.cmp(&b.position))
        });
        for candidate in &candidates {
            if venue.liquidate(&self.wallet, now.at, candidate).await {
                self.wallet.count("liquidations", 1);
            }
        }
    }

    async fn report(&mut self, _: &mut ActorsReport) {}
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::actors::library::tests::{Book, at, wallet};

    struct Venue(Arc<Mutex<Vec<String>>>);

    #[async_trait]
    impl Liquidations for Venue {
        async fn candidates(&self) -> Vec<Candidate> {
            vec![
                Candidate {
                    position: "small".into(),
                    bonus_bps: 20.0,
                },
                Candidate {
                    position: "big".into(),
                    bonus_bps: 500.0,
                },
                Candidate {
                    position: "dust".into(),
                    bonus_bps: 1.0,
                },
            ]
        }
        async fn liquidate(&self, _: &Wallet, at: u64, candidate: &Candidate) -> bool {
            self.0
                .lock()
                .unwrap()
                .push(format!("{at} {}", candidate.position));
            true
        }
    }

    #[tokio::test]
    async fn it_liquidates_the_best_first_and_idles_without_a_venue() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let (book, _) = Book::new("l", 99.0, 101.0);
        let (w, ledger) = wallet(book);
        let spec = Liquidator {
            latency_slots: 4,
            min_bonus_bps: 10.0,
        };
        let mut liquidator = Competing::new(spec.clone(), w, Some(Box::new(Venue(seen.clone()))));
        liquidator.after_blocks(&at(2, 100.0)).await;
        liquidator.after_blocks(&at(4, 100.0)).await;
        assert_eq!(*seen.lock().unwrap(), ["2 big", "2 small"]);
        assert_eq!(ledger.report().actors[0].counts["liquidations"], 2);

        let (book, _) = Book::new("l", 99.0, 101.0);
        let (w, ledger) = wallet(book);
        let mut idle = Competing::new(spec, w, None);
        idle.after_blocks(&at(2, 100.0)).await;
        idle.after_blocks(&at(4, 100.0)).await;
        assert_eq!(ledger.report().actors[0].counts["no_lending_venue"], 1);
    }
}
