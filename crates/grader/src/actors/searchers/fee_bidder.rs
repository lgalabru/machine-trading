//! A fee bidder: a buyer or seller on a pool who pays to land first, as
//! launch-day traders do. In each slot it swaps with chance `per_slot`, a
//! coin-flip side and a lognormal size around `size`, paying `premium`
//! times what clears the pool's block, and at least `min_cu_price`. The
//! builder orders a block by price, so its swap moves the pool ahead of any
//! send that pays less in the same block.

use std::sync::Arc;

use async_trait::async_trait;
use mtx_kit::{AmountUnit, Side};
use serde::{Deserialize, Serialize};

use super::{Account, PoolSide, positive};
use crate::{
    actors::{Actor, ActorsReport, Moment},
    txlayer::{TxEngine, draw, engine::PoolTurn, normal},
};

fn two() -> f64 {
    2.0
}

fn half() -> f64 {
    0.5
}

fn min_price() -> u64 {
    50_000
}

fn thousand() -> f64 {
    1_000.0
}

fn two_hundred_thousand() -> f64 {
    200_000.0
}

/// A fee bidder's settings.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct FeeBidder {
    /// The pool it trades: `raydium` or `meteora`.
    pub venue: String,
    /// The chance it swaps in a slot.
    pub per_slot: f64,
    /// Its median swap, in base units, and the sigma of its lognormal.
    pub size: f64,
    #[serde(default = "half")]
    pub size_sigma: f64,
    /// What it pays over the price that clears the pool's block.
    #[serde(default = "two")]
    pub premium: f64,
    /// The least it pays, in micro-lamports per compute unit.
    #[serde(default = "min_price")]
    pub min_cu_price: u64,
    /// What its wallet starts with, base and quote units.
    #[serde(default = "thousand")]
    pub base: f64,
    #[serde(default = "two_hundred_thousand")]
    pub quote: f64,
}

impl FeeBidder {
    pub(super) fn validate(&self) -> Result<(), String> {
        if !(0.0..=1.0).contains(&self.per_slot) {
            return Err("per_slot is a chance, 0 to 1".into());
        }
        positive("size", self.size)?;
        positive("premium", self.premium)?;
        positive("base", self.base)?;
        positive("quote", self.quote)?;
        if !(self.size_sigma.is_finite() && self.size_sigma >= 0.0) {
            return Err("size_sigma must be at least 0".into());
        }
        Ok(())
    }
}

/// A running fee bidder.
pub struct FeeBid {
    spec: FeeBidder,
    account: Account,
    pool: PoolSide,
    /// Its draws: the layer's seed and its number.
    seed: u64,
    n: u64,
}

impl FeeBid {
    pub fn new(spec: FeeBidder, account: Account, pool: PoolSide, seed: u64, n: u32) -> Self {
        Self {
            spec,
            account,
            pool,
            seed,
            n: u64::from(n),
        }
    }
}

#[async_trait]
impl Actor for FeeBid {
    fn bind_tx(&mut self, engine: &Arc<TxEngine>) {
        self.account.bind(engine);
    }

    fn wants_pool(&self) -> bool {
        true
    }

    async fn on_pool(&mut self, turn: &PoolTurn) {
        let Some(engine) = self.account.engine().cloned() else {
            return;
        };
        self.account.opening(&self.pool).await;
        let keys = [self.n, turn.slot];
        if draw(self.seed, "fee-bidder", &keys) >= self.spec.per_slot {
            return;
        }
        let side = if draw(self.seed, "fee-bidder-side", &keys) < 0.5 {
            Side::Bid
        } else {
            Side::Ask
        };
        let size = self.spec.size
            * (self.spec.size_sigma * normal(self.seed, "fee-bidder-size", &keys)).exp();
        let limit = match side {
            Side::Bid => 1e12,
            Side::Ask => 0.0,
        };
        let owner = self.account.pubkey();
        let Ok(swap) = self
            .pool
            .swap(&owner, side, size, AmountUnit::Base, limit)
            .await
        else {
            self.account.count("unbuilt");
            return;
        };
        let pool = self.pool.assets().address;
        let clearing = engine.price_to_land(turn.slot, &[pool]);
        let price = ((clearing as f64 * self.spec.premium) as u64).max(self.spec.min_cu_price);
        let Ok(wire) = self
            .account
            .sign(swap, super::SWAP_UNITS, price, 0, turn.latest)
        else {
            return;
        };
        match self.account.send(vec![wire], false, true).await {
            Ok(_) => self.account.count("swaps_sent"),
            Err(_) => self.account.count("refused"),
        }
    }

    async fn end(&mut self, _last: &Moment) {
        self.account.closing(&self.pool).await;
    }

    async fn report(&mut self, _: &mut ActorsReport) {}
}
