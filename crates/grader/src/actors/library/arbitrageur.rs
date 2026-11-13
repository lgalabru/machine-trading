//! The arbitrageur: it ties the market to its reference.
//!
//! Every `latency_slots` it reads the market's best bid and ask through its
//! seat. A bid more than `edge_bps` over the reference, it sells into, down
//! to that edge; an ask more than `edge_bps` under it, it buys, up to that
//! edge: at most `size_mult` market trade sizes at a time, and never past
//! `max_inventory_mult` of them held. The reference is the market's own
//! index where it has one (a perp's, read from the chain), so the perp
//! follows its index; elsewhere it is the scenario's level. On a pool the
//! same seat swaps, so the pool follows the level too.

use async_trait::async_trait;
use mtx_kit::Side;
use serde::{Deserialize, Serialize};

use super::{Scale, Wallet, finite_positive};
use crate::actors::{Actor, ActorsReport, Moment};

fn two() -> u64 {
    2
}

fn five() -> f64 {
    5.0
}

fn ten() -> f64 {
    10.0
}

fn hundred() -> f64 {
    100.0
}

/// The arbitrageur's settings.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Arbitrageur {
    #[serde(default = "two")]
    pub latency_slots: u64,
    #[serde(default = "five")]
    pub edge_bps: f64,
    #[serde(default = "ten")]
    pub size_mult: f64,
    #[serde(default = "hundred")]
    pub max_inventory_mult: f64,
}

impl Default for Arbitrageur {
    fn default() -> Self {
        Self {
            latency_slots: two(),
            edge_bps: five(),
            size_mult: ten(),
            max_inventory_mult: hundred(),
        }
    }
}

impl Arbitrageur {
    pub fn validate(&self) -> Result<(), String> {
        if self.latency_slots == 0 {
            return Err("latency_slots must be at least 1".into());
        }
        finite_positive("edge_bps", self.edge_bps)?;
        finite_positive("size_mult", self.size_mult)?;
        finite_positive("max_inventory_mult", self.max_inventory_mult)
    }
}

pub(super) struct Arb {
    spec: Arbitrageur,
    scale: Scale,
    wallet: Wallet,
    next: u64,
    /// What it holds, as its takes add up.
    held: f64,
}

impl Arb {
    pub fn new(spec: Arbitrageur, scale: Scale, wallet: Wallet) -> Self {
        Self {
            spec,
            scale,
            wallet,
            next: 0,
            held: 0.0,
        }
    }
}

#[async_trait]
impl Actor for Arb {
    async fn after_blocks(&mut self, now: &Moment) {
        if now.at < self.next {
            return;
        }
        self.next = now.at + self.spec.latency_slots;
        let Ok((bid, ask)) = self.wallet.seat().top().await else {
            return;
        };
        let reference = match self.wallet.seat().index().await {
            Ok(Some(index)) => index,
            _ => now.level_usd,
        };
        let edge = self.spec.edge_bps / 10_000.0;
        let max = self.scale.trade_size * self.spec.max_inventory_mult;
        let size = self.scale.trade_size * self.spec.size_mult;
        let (side, limit, room) = match (bid, ask) {
            (Some(bid), _) if bid > reference * (1.0 + edge) => {
                (Side::Ask, reference * (1.0 + edge), max + self.held)
            }
            (_, Some(ask)) if ask < reference * (1.0 - edge) => {
                (Side::Bid, reference * (1.0 - edge), max - self.held)
            }
            _ => return,
        };
        let size = size.min(room);
        if size <= 0.0 {
            return;
        }
        if self.wallet.take(now.at, side, size, limit).await {
            // What filled, as the seat reads its position.
            match self.wallet.seat().position().await {
                Ok(position) => self.held = position,
                Err(_) => {
                    self.held += match side {
                        Side::Bid => size,
                        Side::Ask => -size,
                    }
                }
            }
            self.wallet.count("arbs", 1);
        }
    }

    async fn report(&mut self, _: &mut ActorsReport) {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::actors::library::tests::{Book, at, scale, wallet};

    #[tokio::test]
    async fn it_sells_a_rich_market_and_buys_a_cheap_one_back_to_the_reference() {
        let (book, log) = Book::new("a", 100.20, 100.30);
        let top = book.top.clone();
        let (wallet, _) = wallet(book);
        let mut arb = Arb::new(
            Arbitrageur {
                latency_slots: 4,
                edge_bps: 5.0,
                size_mult: 3.0,
                max_inventory_mult: 4.0,
            },
            scale(),
            wallet,
        );
        arb.after_blocks(&at(2, 100.0)).await;
        // Not due yet.
        arb.after_blocks(&at(4, 100.0)).await;
        // Still rich: only 1 more fits under the inventory limit.
        arb.after_blocks(&at(6, 100.0)).await;
        arb.after_blocks(&at(10, 100.0)).await;
        *top.lock().unwrap() = (Some(99.80), Some(99.90));
        arb.after_blocks(&at(14, 100.0)).await;
        *top.lock().unwrap() = (Some(99.98), Some(100.02));
        arb.after_blocks(&at(18, 100.0)).await;
        assert_eq!(
            *log.lock().unwrap(),
            [
                "a take Ask 3.000 at 100.05",
                "a take Ask 1.000 at 100.05",
                "a take Bid 3.000 at 99.95",
            ]
        );
    }
}
