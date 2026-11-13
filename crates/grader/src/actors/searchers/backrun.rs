//! A backrunner: it reads what landed in the last block, and after a swap of
//! the bot's of at least `min_base` on either of its two pools left them
//! more than `edge_bps` apart, it buys on the cheap one and sells on the
//! dear one, the same base, in one bundle with a tip. The size is the best
//! of a fixed ladder of simulations, from `max_base` down by halves; it
//! sends only if that pays over its fees and its tip (`tip_share` of what
//! it makes). Its trade pulls the pools back together: the dear pool's
//! price falls toward the cheap one's before anyone else trades it.

use std::{collections::BTreeSet, sync::Arc};

use async_trait::async_trait;
use mtx_kit::{AmountUnit, Side};
use serde::{Deserialize, Serialize};

use super::{
    Account, PoolSide, lamports_of, positive, sandwich::DEFAULT_SOL_USD, share, token_amount,
};
use crate::{
    actors::{Actor, ActorsReport, Moment},
    txlayer::{TxEngine, engine::PoolTurn, jito, pool::BASE_FEE},
};

fn twenty() -> f64 {
    20.0
}

fn fifty() -> f64 {
    50.0
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

fn thousand() -> f64 {
    1_000.0
}

fn two_hundred_thousand() -> f64 {
    200_000.0
}

/// Rungs of its size ladder.
const RUNGS: u32 = 6;

/// A backrunner's settings.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Backrunner {
    /// Its two pools: `raydium` and `meteora`.
    pub venue: String,
    pub other: String,
    /// The least swap of the bot's it acts after, in base units.
    #[serde(default = "one")]
    pub min_base: f64,
    /// How far apart the pools' prices must be, in basis points.
    #[serde(default = "twenty")]
    pub edge_bps: f64,
    /// Its largest trade, in base units.
    #[serde(default = "fifty")]
    pub max_base: f64,
    #[serde(default = "half")]
    pub tip_share: f64,
    /// The most it tips, in lamports: its wallet holds 1 SOL for fees.
    #[serde(default = "max_tip")]
    pub max_tip_lamports: u64,
    #[serde(default = "half")]
    pub min_profit_usd: f64,
    #[serde(default = "thousand")]
    pub base: f64,
    #[serde(default = "two_hundred_thousand")]
    pub quote: f64,
}

impl Backrunner {
    pub(super) fn validate(&self) -> Result<(), String> {
        if self.venue == self.other {
            return Err("venue and other are two pools".into());
        }
        positive("min_base", self.min_base)?;
        positive("edge_bps", self.edge_bps)?;
        positive("max_base", self.max_base)?;
        share("tip_share", self.tip_share)?;
        positive("base", self.base)?;
        positive("quote", self.quote)?;
        if !(self.min_profit_usd.is_finite() && self.min_profit_usd >= 0.0) {
            return Err("min_profit_usd must be at least 0".into());
        }
        Ok(())
    }
}

/// A running backrunner.
pub struct Backrun {
    spec: Backrunner,
    account: Account,
    pools: [PoolSide; 2],
    /// The bot's transactions it has acted after.
    seen: BTreeSet<String>,
}

impl Backrun {
    pub fn new(spec: Backrunner, account: Account, pools: [PoolSide; 2]) -> Self {
        Self {
            spec,
            account,
            pools,
            seen: BTreeSet::new(),
        }
    }

    /// Buy `x` base on `cheap`, sell it on `dear`: the two legs.
    async fn legs(
        &mut self,
        cheap: usize,
        x: f64,
        tip: u64,
        turn: &PoolTurn,
    ) -> Option<(Vec<u8>, Vec<u8>)> {
        let owner = self.account.pubkey();
        let buy = self.pools[cheap]
            .swap(&owner, Side::Bid, x, AmountUnit::Base, 1e12)
            .await
            .ok()?;
        let sell = self.pools[1 - cheap]
            .swap(&owner, Side::Ask, x, AmountUnit::Base, 0.0)
            .await
            .ok()?;
        let buy = self
            .account
            .sign(buy, super::SWAP_UNITS, 0, 0, turn.latest)
            .ok()?;
        let sell = self
            .account
            .sign(sell, super::SWAP_UNITS, 0, tip, turn.latest)
            .ok()?;
        Some((buy, sell))
    }

    /// What buying and selling `x` makes, in quote units, before fees.
    async fn profit(
        &mut self,
        engine: &TxEngine,
        cheap: usize,
        x: f64,
        turn: &PoolTurn,
    ) -> Option<f64> {
        let quote_account = self.pools[0].wallet_keys(&self.account.pubkey())[1];
        let before = self.pools[0].balance(&quote_account).await?;
        let (buy, sell) = self.legs(cheap, x, 0, turn).await?;
        let ran = engine
            .simulate_bundle(&[buy, sell], &[vec![], vec![quote_account]])
            .await
            .ok()?;
        if ran.len() != 2 || !ran.iter().all(|t| t.ok) {
            return None;
        }
        let after = token_amount(ran[1].accounts.first()?.as_ref()?)?;
        let units = 10f64.powi(i32::from(self.pools[0].assets().quote.decimals));
        Some((after as f64 - before as f64) / units)
    }
}

#[async_trait]
impl Actor for Backrun {
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
        self.account.opening(&self.pools[0]).await;
        if turn.skipped || !turn.jito {
            return;
        }
        // A swap of the bot's that landed on either pool in the last block,
        // large enough.
        let mut moved = false;
        for recent in turn.recent.iter().filter(|r| r.by_bot && r.ok) {
            if !self.pools.iter().any(|p| p.touched_by(&recent.tx))
                || !self.seen.insert(recent.tx.signature.clone())
            {
                continue;
            }
            moved = true;
        }
        if !moved {
            return;
        }
        let (Some(a), Some(b)) = (self.pools[0].price().await, self.pools[1].price().await) else {
            return;
        };
        let gap = (a / b - 1.0).abs() * 10_000.0;
        if gap < self.spec.edge_bps {
            return;
        }
        self.account.count("gaps_seen");
        let cheap = usize::from(b < a);
        let mut best: Option<(f64, f64)> = None;
        for rung in 0..RUNGS {
            let x = self.spec.max_base / f64::from(1u32 << rung);
            if x < self.spec.min_base / 4.0 {
                break;
            }
            if let Some(profit) = self.profit(&engine, cheap, x, turn).await
                && best.is_none_or(|(_, p)| profit > p)
            {
                best = Some((x, profit));
            }
        }
        let Some((x, profit)) = best else {
            self.account.count("unprofitable");
            return;
        };
        let sol_usd = turn.sol_usd.unwrap_or(DEFAULT_SOL_USD);
        let tip = lamports_of(profit * self.spec.tip_share, sol_usd)
            .min(self.spec.max_tip_lamports)
            .max(jito::MIN_TIP);
        let net = profit - (tip + 2 * BASE_FEE) as f64 * sol_usd / 1e9;
        if net < self.spec.min_profit_usd {
            self.account.count("unprofitable");
            return;
        }
        let Some((buy, sell)) = self.legs(cheap, x, tip, turn).await else {
            return;
        };
        match self.account.send(vec![buy, sell], true, true).await {
            Ok(_) => self.account.count("backruns_sent"),
            Err(_) => self.account.count("refused"),
        }
    }

    async fn end(&mut self, _last: &Moment) {
        self.account.closing(&self.pools[0]).await;
    }

    async fn report(&mut self, _: &mut ActorsReport) {}
}
