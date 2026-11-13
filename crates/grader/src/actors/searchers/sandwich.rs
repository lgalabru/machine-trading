//! A sandwicher: around a swap of the bot's it can see pending on its pool,
//! a bundle of three, all in one block at its top: its own trade the swap's
//! way, the bot's swap, and its trade back. The swap then fills at the
//! price the front-run left, and the sandwicher sells back into the price
//! the swap made.
//!
//! The front-run's size is the largest the swap's own limit still lets
//! through, found by bisection over a fixed number of bundle simulations
//! (`iterations`); of it, a half and a quarter, the one that pays most is
//! sent, if it pays more than `min_profit_usd` after its fees and its tip
//! (`tip_share` of what it makes). A tight limit leaves no room: nothing is
//! sent. It sees only what a leaky route or a sandwiching leader shows it,
//! never a bundle, and a bundle around a `jitodontfront` transaction is
//! refused, so it leaves those alone. It needs a Jito leader to land.

use std::{collections::BTreeSet, sync::Arc};

use async_trait::async_trait;
use mtx_kit::{AmountUnit, Side};
use serde::{Deserialize, Serialize};

use super::{Account, PoolSide, lamports_of, positive, share, token_amount};
use crate::{
    actors::{Actor, ActorsReport, Moment},
    txlayer::{
        TxEngine,
        engine::{Pending, PoolTurn},
        jito,
        pool::BASE_FEE,
    },
};

fn eight() -> u32 {
    8
}

fn max_tip() -> u64 {
    100_000_000
}

fn half() -> f64 {
    0.5
}

fn one() -> f64 {
    1.0
}

/// A sandwicher's settings.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Sandwicher {
    /// The pool it works: `raydium` or `meteora`.
    pub venue: String,
    /// The least swap it sandwiches, in base units.
    #[serde(default = "one")]
    pub min_base: f64,
    /// The largest front-run, in base units.
    #[serde(default = "fifty")]
    pub max_base: f64,
    /// The share of its profit it tips.
    #[serde(default = "half")]
    pub tip_share: f64,
    /// The most it tips, in lamports: its wallet holds 1 SOL for fees.
    #[serde(default = "max_tip")]
    pub max_tip_lamports: u64,
    /// The least it sends for, in USD after fees and tip.
    #[serde(default = "half")]
    pub min_profit_usd: f64,
    /// Bisection steps on the front-run's size.
    #[serde(default = "eight")]
    pub iterations: u32,
    /// What its wallet starts with, base and quote units.
    #[serde(default = "thousand")]
    pub base: f64,
    #[serde(default = "two_hundred_thousand")]
    pub quote: f64,
}

fn fifty() -> f64 {
    50.0
}

fn thousand() -> f64 {
    1_000.0
}

fn two_hundred_thousand() -> f64 {
    200_000.0
}

impl Sandwicher {
    pub(super) fn validate(&self) -> Result<(), String> {
        positive("min_base", self.min_base)?;
        positive("max_base", self.max_base)?;
        share("tip_share", self.tip_share)?;
        positive("base", self.base)?;
        positive("quote", self.quote)?;
        if !(self.min_profit_usd.is_finite() && self.min_profit_usd >= 0.0) {
            return Err("min_profit_usd must be at least 0".into());
        }
        if !(1..=16).contains(&self.iterations) {
            return Err("iterations is 1 to 16".into());
        }
        Ok(())
    }
}

/// A running sandwicher.
pub struct Sandwich {
    spec: Sandwicher,
    account: Account,
    pool: PoolSide,
    /// The bot's sends it has looked at, by pool entry.
    seen: BTreeSet<u64>,
}

impl Sandwich {
    pub fn new(spec: Sandwicher, account: Account, pool: PoolSide) -> Self {
        Self {
            spec,
            account,
            pool,
            seen: BTreeSet::new(),
        }
    }

    /// The front-run of `x` base units the way of a swap taking `side`,
    /// and the trade back.
    async fn legs(
        &mut self,
        side: Side,
        x: f64,
        tip: u64,
        turn: &PoolTurn,
    ) -> Option<(Vec<u8>, Vec<u8>)> {
        let owner = self.account.pubkey();
        let (front, back) = match side {
            // The bot sells: sell first, buy the same back after.
            Side::Ask => (
                self.pool
                    .swap(&owner, Side::Ask, x, AmountUnit::Base, 0.0)
                    .await,
                self.pool
                    .swap(&owner, Side::Bid, x, AmountUnit::Base, 1e12)
                    .await,
            ),
            Side::Bid => (
                self.pool
                    .swap(&owner, Side::Bid, x, AmountUnit::Base, 1e12)
                    .await,
                self.pool
                    .swap(&owner, Side::Ask, x, AmountUnit::Base, 0.0)
                    .await,
            ),
        };
        let front = self
            .account
            .sign(front.ok()?, super::SWAP_UNITS, 0, 0, turn.latest)
            .ok()?;
        let back = self
            .account
            .sign(back.ok()?, super::SWAP_UNITS, 0, tip, turn.latest)
            .ok()?;
        Some((front, back))
    }

    /// Whether the bot's swap still runs after a front-run of `x`.
    async fn fits(
        &mut self,
        engine: &TxEngine,
        victim: &Pending,
        side: Side,
        x: f64,
        turn: &PoolTurn,
    ) -> bool {
        let Some((front, _)) = self.legs(side, x, 0, turn).await else {
            return false;
        };
        engine
            .simulate_bundle(&[front, victim.tx.wire.clone()], &[vec![], vec![]])
            .await
            .is_ok_and(|r| r.len() == 2 && r.iter().all(|t| t.ok))
    }

    /// What the bundle of `x` makes, in quote units, before fees and tip.
    async fn profit(
        &mut self,
        engine: &TxEngine,
        victim: &Pending,
        side: Side,
        x: f64,
        turn: &PoolTurn,
    ) -> Option<f64> {
        let quote_account = self.pool.wallet_keys(&self.account.pubkey())[1];
        let before = self.pool.balance(&quote_account).await?;
        let (front, back) = self.legs(side, x, 0, turn).await?;
        let ran = engine
            .simulate_bundle(
                &[front, victim.tx.wire.clone(), back],
                &[vec![], vec![], vec![quote_account]],
            )
            .await
            .ok()?;
        if ran.len() != 3 || !ran.iter().all(|t| t.ok) {
            return None;
        }
        let after = token_amount(ran[2].accounts.first()?.as_ref()?)?;
        let units = 10f64.powi(i32::from(self.pool.assets().quote.decimals));
        Some((after as f64 - before as f64) / units)
    }

    async fn try_victim(&mut self, engine: Arc<TxEngine>, victim: &Pending, turn: &PoolTurn) {
        let Some((side, size)) = self.pool.read_swap(&engine, victim).await else {
            return;
        };
        if size < self.spec.min_base {
            return;
        }
        self.account.count("swaps_seen");
        if victim.tx.dont_front {
            self.account.count("dont_front");
            return;
        }
        // The largest front-run the swap's limit lets through.
        let (mut lo, mut hi) = (0.0, self.spec.max_base);
        for _ in 0..self.spec.iterations {
            let mid = (lo + hi) / 2.0;
            if self.fits(&engine, victim, side, mid, turn).await {
                lo = mid;
            } else {
                hi = mid;
            }
        }
        if lo <= 0.0 {
            self.account.count("no_room");
            return;
        }
        let mut best: Option<(f64, f64)> = None;
        for x in [lo, lo / 2.0, lo / 4.0] {
            if let Some(profit) = self.profit(&engine, victim, side, x, turn).await
                && best.is_none_or(|(_, b)| profit > b)
            {
                best = Some((x, profit));
            }
        }
        if let Some(engine) = self.account.engine() {
            engine.note(&self.account.sender, "last_room_base", lo);
        }
        let Some((x, profit)) = best else {
            self.account.count("no_room");
            return;
        };
        if let Some(engine) = self.account.engine() {
            engine.note(&self.account.sender, "last_front_base", x);
            engine.note(&self.account.sender, "last_profit_quote", profit);
        }
        let sol_usd = turn.sol_usd.unwrap_or(DEFAULT_SOL_USD);
        let tip = lamports_of(profit * self.spec.tip_share, sol_usd)
            .min(self.spec.max_tip_lamports)
            .max(jito::MIN_TIP);
        let fees = 2 * BASE_FEE;
        let net = profit - (tip + fees) as f64 * sol_usd / 1e9;
        if net < self.spec.min_profit_usd {
            self.account.count("unprofitable");
            return;
        }
        let Some((front, back)) = self.legs(side, x, tip, turn).await else {
            return;
        };
        match self
            .account
            .send(vec![front, victim.tx.wire.clone(), back], true, true)
            .await
        {
            Ok(_) => self.account.count("sandwiches_sent"),
            Err(refusal) => {
                tracing::warn!(sender = %self.account.sender, reason = %refusal.message, "a sandwich was refused");
                self.account.count("refused");
            }
        }
    }
}

/// What a SOL is worth when the scenario doesn't say: tips and fees are
/// priced in it.
pub(super) const DEFAULT_SOL_USD: f64 = 150.0;

#[async_trait]
impl Actor for Sandwich {
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
        if turn.skipped || !turn.jito {
            return;
        }
        for victim in &turn.visible {
            if !self.seen.insert(victim.id) {
                continue;
            }
            self.try_victim(engine.clone(), victim, turn).await;
        }
    }

    async fn end(&mut self, _last: &Moment) {
        self.account.closing(&self.pool).await;
    }

    async fn report(&mut self, _: &mut ActorsReport) {}
}
