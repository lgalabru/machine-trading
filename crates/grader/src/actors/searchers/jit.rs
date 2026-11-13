//! Just-in-time liquidity: a narrow position at the price, opened empty at
//! setup, which it fills just before a large swap it can see and empties
//! right after. On seeing a swap of the bot's of at least `min_base` on its
//! pool, pending where it can look, it sends a bundle: its deposit of
//! `deposit_base` and `deposit_quote` (and its tip), then the swap. The
//! swap trades mostly against its liquidity, so most of the swap's fee is
//! its. In the next block it takes everything out, fees included, paying
//! over what clears the block to go first. It needs a Jito leader to land
//! the bundle, and leaves a `jitodontfront` swap alone: a bundle with one
//! anywhere but first is refused.

use std::{collections::BTreeSet, sync::Arc};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use solana_pubkey::Pubkey;

use super::{Account, PoolSide, positive};
use crate::{
    actors::{Actor, ActorsReport, Moment},
    txlayer::{TxEngine, engine::PoolTurn},
    venue::pools::Deposit,
};

fn five() -> f64 {
    5.0
}

fn thirty() -> f64 {
    30.0
}

fn hundred() -> f64 {
    100.0
}

fn tip() -> u64 {
    10_000
}

fn thousand() -> f64 {
    1_000.0
}

fn two_hundred_thousand() -> f64 {
    200_000.0
}

fn twenty_thousand() -> f64 {
    20_000.0
}

/// What a withdrawal pays per compute unit at least, in micro-lamports.
const WITHDRAW_PRICE: u64 = 10_000;

/// A JIT liquidity provider's settings.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct JitLp {
    /// The pool it provides on: `raydium` or `meteora`.
    pub venue: String,
    /// The least swap it acts on, in base units.
    #[serde(default = "five")]
    pub min_base: f64,
    /// Its position's half-width around the price at setup, in basis points.
    #[serde(default = "thirty")]
    pub range_bps: f64,
    /// What it deposits before a swap.
    #[serde(default = "hundred")]
    pub deposit_base: f64,
    #[serde(default = "twenty_thousand")]
    pub deposit_quote: f64,
    /// Its tip on each bundle, in lamports.
    #[serde(default = "tip")]
    pub tip_lamports: u64,
    /// What its wallet starts with, base and quote units.
    #[serde(default = "thousand")]
    pub base: f64,
    #[serde(default = "two_hundred_thousand")]
    pub quote: f64,
}

impl JitLp {
    pub(super) fn validate(&self) -> Result<(), String> {
        positive("min_base", self.min_base)?;
        positive("range_bps", self.range_bps)?;
        positive("deposit_base", self.deposit_base)?;
        positive("deposit_quote", self.deposit_quote)?;
        positive("base", self.base)?;
        positive("quote", self.quote)?;
        if self.deposit_base > self.base || self.deposit_quote > self.quote {
            return Err("it deposits no more than its wallet holds".into());
        }
        if self.tip_lamports < crate::txlayer::jito::MIN_TIP {
            return Err(format!(
                "tip_lamports is at least the least tip, {}",
                crate::txlayer::jito::MIN_TIP
            ));
        }
        Ok(())
    }
}

/// A running JIT provider, with its position.
pub struct Jit {
    spec: JitLp,
    account: Account,
    pool: PoolSide,
    position: Pubkey,
    /// Whether its position may hold liquidity to take out.
    filled: bool,
    seen: BTreeSet<u64>,
}

impl Jit {
    pub fn new(spec: JitLp, account: Account, pool: PoolSide, position: Pubkey) -> Self {
        Self {
            spec,
            account,
            pool,
            position,
            filled: false,
            seen: BTreeSet::new(),
        }
    }

    /// Take everything out of its position, if it holds anything.
    async fn empty(&mut self, engine: &TxEngine, turn: &PoolTurn) {
        let owner = self.account.pubkey();
        let chain = self.pool.chain.clone();
        let Ok(Some(lp)) = self
            .pool
            .pool
            .position(chain.as_ref(), &owner, &self.position)
            .await
        else {
            return;
        };
        if !(lp.live || lp.fees_base > 0.0 || lp.fees_quote > 0.0) {
            self.filled = false;
            return;
        }
        let Ok(withdraw) = self
            .pool
            .pool
            .withdraw(chain.as_ref(), &owner, &lp, 1.0, None, None)
            .await
        else {
            return;
        };
        let pool = self.pool.assets().address;
        let price = engine
            .price_to_land(turn.slot, &[pool])
            .saturating_mul(2)
            .max(WITHDRAW_PRICE);
        let Ok(wire) = self
            .account
            .sign(withdraw, super::SWAP_UNITS, price, 0, turn.latest)
        else {
            return;
        };
        if self.account.send(vec![wire], false, true).await.is_ok() {
            self.account.count("withdrawals_sent");
        }
    }
}

#[async_trait]
impl Actor for Jit {
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
        if self.filled && !turn.skipped {
            self.empty(&engine, turn).await;
        }
        if turn.skipped || !turn.jito {
            return;
        }
        for victim in &turn.visible {
            if !self.seen.insert(victim.id) {
                continue;
            }
            let Some((_, size)) = self.pool.read_swap(&engine, victim).await else {
                continue;
            };
            if size < self.spec.min_base {
                continue;
            }
            self.account.count("swaps_seen");
            if victim.tx.dont_front {
                self.account.count("dont_front");
                continue;
            }
            let owner = self.account.pubkey();
            let chain = self.pool.chain.clone();
            let Ok((deposit, _)) = self
                .pool
                .pool
                .deposit(
                    chain.as_ref(),
                    &owner,
                    Some(&self.position),
                    &self.position,
                    &Deposit {
                        base: self.spec.deposit_base,
                        quote: self.spec.deposit_quote,
                        range: None,
                        shape: None,
                    },
                )
                .await
            else {
                self.account.count("unbuilt");
                continue;
            };
            let Ok(wire) = self.account.sign(
                deposit,
                super::SWAP_UNITS,
                0,
                self.spec.tip_lamports,
                turn.latest,
            ) else {
                continue;
            };
            match self
                .account
                .send(vec![wire, victim.tx.wire.clone()], true, true)
                .await
            {
                Ok(_) => {
                    self.filled = true;
                    self.account.count("jit_sent");
                }
                Err(_) => self.account.count("refused"),
            }
            // One a block: its position holds one deposit at a time.
            break;
        }
    }

    async fn end(&mut self, _last: &Moment) {
        self.account.closing(&self.pool).await;
    }

    async fn report(&mut self, _: &mut ActorsReport) {}
}
