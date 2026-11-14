//! Searchers: the actors that see a transaction layer's pool and act inside
//! each slot's block, as a scenario's `tx_layer.searchers` lists them (see
//! `docs/tx-layer.md`).
//!
//! - **Fee bidders** ([`FeeBidder`]) swap on a pool at random, paying a
//!   priority fee over what clears the block: launch-day buyers, landing
//!   ahead of a cheaper send in the same block.
//! - **Sandwichers** ([`Sandwicher`]) take a swap of the bot's they can see
//!   pending (sent by a leaky route, or to a sandwiching leader) and land a
//!   bundle around it: a trade the swap's way first, the swap, and the trade
//!   back. The front-run is sized by a fixed number of bundle simulations,
//!   the largest the swap's own limit still lets through, and taken only if
//!   it pays more than its fees and tip.
//! - **JIT liquidity** ([`JitLp`]) fills a narrow position at the price just
//!   before a large swap it can see, in the same bundle, and empties it in
//!   the next block: the swap's fee goes mostly to it.
//! - **Backrunners** ([`Backrunner`]) read what landed in the last block:
//!   after a large swap of the bot's moved one pool away from the other,
//!   they buy where it is cheap and sell where it is dear, in one bundle
//!   with a tip, as long as that pays.
//! - **Racing liquidators** ([`LiquidatorRace`]) are a lending session's
//!   liquidators sending through the pool: each bids a share of the bonus,
//!   in priority fees or as a bundle's tip. Only the first lands; the rest
//!   fail and pay their fees, or, as bundles, pay nothing.
//!
//! None of them sees a bundle of the bot's: what goes privately to the block
//! engine stays private. Each trades from a wallet of its own, through the
//! pool and the builder like anyone, paying its own fees and tips: the
//! layer's report gives each one's ledger, checked against its wallet.
//! What each decides is a pure function of the session's seed, the chain's
//! state and what the bot sent: searches run a fixed number of simulations.

mod backrun;
mod fee_bidder;
mod jit;
#[cfg(test)]
mod live;
mod sandwich;

use std::sync::Arc;

use mtx_kit::{AmountUnit, Side};
use serde::{Deserialize, Serialize};
use solana_keypair::Keypair;
use solana_message::{Message, VersionedMessage};
use solana_pubkey::Pubkey;
use solana_signer::Signer;
use solana_transaction::{AccountMeta, Hash, Instruction, versioned::VersionedTransaction};

pub use self::{
    backrun::{Backrun, Backrunner},
    fee_bidder::{FeeBid, FeeBidder},
    jit::{Jit, JitLp},
    sandwich::{Sandwich, Sandwicher},
};
use crate::{
    txlayer::{
        RouteKind, TxEngine,
        engine::{Accepted, Pending, Refusal, Submission},
        jito,
        pool::{COMPUTE_BUDGET, SYSTEM},
    },
    venue::pools::{Pool, PoolAssets, SwapOrder},
};

/// A scenario's searchers, by kind.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Searchers {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fee_bidders: Vec<FeeBidder>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sandwichers: Vec<Sandwicher>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub jit: Vec<JitLp>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub backrunners: Vec<Backrunner>,
    /// A lending session's liquidators race through the pool.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub liquidators: Option<LiquidatorRace>,
}

impl Searchers {
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }

    /// Whether any of them trades a pool.
    pub fn on_pools(&self) -> bool {
        !(self.fee_bidders.is_empty()
            && self.sandwichers.is_empty()
            && self.jit.is_empty()
            && self.backrunners.is_empty())
    }

    pub fn validate(&self) -> Result<(), String> {
        let each = |kind: &str, checks: Vec<Result<(), String>>| {
            checks.into_iter().enumerate().try_for_each(|(i, c)| {
                c.map_err(|e| format!("tx_layer.searchers.{kind}[{i}]: {e}"))
            })
        };
        each(
            "fee_bidders",
            self.fee_bidders.iter().map(FeeBidder::validate).collect(),
        )?;
        each(
            "sandwichers",
            self.sandwichers.iter().map(Sandwicher::validate).collect(),
        )?;
        each("jit", self.jit.iter().map(JitLp::validate).collect())?;
        each(
            "backrunners",
            self.backrunners.iter().map(Backrunner::validate).collect(),
        )?;
        if let Some(race) = &self.liquidators {
            race.validate()
                .map_err(|e| format!("tx_layer.searchers.liquidators: {e}"))?;
        }
        Ok(())
    }

    /// The pools they trade, by venue, each named once.
    pub fn venues(&self) -> Vec<&str> {
        let mut venues: Vec<&str> = self
            .fee_bidders
            .iter()
            .map(|f| f.venue.as_str())
            .chain(self.sandwichers.iter().map(|s| s.venue.as_str()))
            .chain(self.jit.iter().map(|j| j.venue.as_str()))
            .chain(
                self.backrunners
                    .iter()
                    .flat_map(|b| [b.venue.as_str(), b.other.as_str()]),
            )
            .collect();
        venues.sort();
        venues.dedup();
        venues
    }
}

/// A lending session's liquidators, racing through the pool: each bids a
/// share of the liquidation's bonus drawn from `shade` (the rest is its
/// margin), as priority fees on its compute units, or, with `bundles`, as
/// a Jito tip on a bundle of its liquidation.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct LiquidatorRace {
    pub shade: [f64; 2],
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub bundles: bool,
}

impl LiquidatorRace {
    fn validate(&self) -> Result<(), String> {
        let [low, high] = self.shade;
        if !(0.0..=1.0).contains(&low) || !(low..=1.0).contains(&high) {
            return Err("shade is a range of shares, low to high".into());
        }
        Ok(())
    }

    /// The share liquidator `i` of a session seeded `seed` bids.
    pub fn share(&self, seed: u64, i: u32) -> f64 {
        let [low, high] = self.shade;
        low + (high - low) * crate::txlayer::draw(seed, "liquidator-shade", &[u64::from(i)])
    }
}

fn positive(what: &str, v: f64) -> Result<(), String> {
    if v.is_finite() && v > 0.0 {
        Ok(())
    } else {
        Err(format!("{what} must be positive"))
    }
}

fn share(what: &str, v: f64) -> Result<(), String> {
    if (0.0..=1.0).contains(&v) {
        Ok(())
    } else {
        Err(format!("{what} is a share, 0 to 1"))
    }
}

/// The compute units a searcher's pool transaction asks for: a swap across
/// a few tick or bin arrays.
const SWAP_UNITS: u32 = 400_000;

/// A searcher's account: its wallet's key, its name in the layer, and the
/// layer once the session binds it.
pub struct Account {
    pub sender: String,
    pub keypair: Keypair,
    engine: Option<Arc<TxEngine>>,
    /// Transactions signed so far: each asks for a few units fewer, so no
    /// two of its own are alike.
    signed: u32,
    /// Its pool's base and quote in its wallet when it first looked, in
    /// atoms.
    start: Option<(u64, u64)>,
}

impl Account {
    pub fn new(kind: &str, i: u32, keypair: Keypair) -> Self {
        Self {
            sender: format!("actor:{kind}/{i}"),
            keypair,
            engine: None,
            signed: 0,
            start: None,
        }
    }

    /// Its wallet's base and quote of `pool`'s, in atoms.
    async fn holdings(&self, pool: &PoolSide) -> Option<(u64, u64)> {
        let keys = pool.wallet_keys(&self.pubkey());
        Some((pool.balance(&keys[0]).await?, pool.balance(&keys[1]).await?))
    }

    /// Note what it holds before it first acts.
    pub async fn opening(&mut self, pool: &PoolSide) {
        if self.start.is_none() {
            self.start = self.holdings(pool).await;
        }
    }

    /// Note, into the report, how its base and quote changed since
    /// [`Account::opening`], and that change valued at the pool's price
    /// now, in quote units: what it made, before fees.
    pub async fn closing(&self, pool: &PoolSide) {
        let (Some(engine), Some((base0, quote0)), Some((base1, quote1))) =
            (&self.engine, self.start, self.holdings(pool).await)
        else {
            return;
        };
        let a = pool.assets();
        let base = (base1 as f64 - base0 as f64) / 10f64.powi(i32::from(a.base.decimals));
        let quote = (quote1 as f64 - quote0 as f64) / 10f64.powi(i32::from(a.quote.decimals));
        engine.note(&self.sender, "base_change", base);
        engine.note(&self.sender, "quote_change", quote);
        if let Some(price) = pool.price().await {
            engine.note(&self.sender, "value_change_quote", quote + base * price);
        }
    }

    pub fn bind(&mut self, engine: &Arc<TxEngine>) {
        engine.register_actor(&self.sender, self.keypair.pubkey());
        self.engine = Some(engine.clone());
    }

    pub fn engine(&self) -> Option<&Arc<TxEngine>> {
        self.engine.as_ref()
    }

    pub fn pubkey(&self) -> Pubkey {
        self.keypair.pubkey()
    }

    pub fn count(&self, what: &str) {
        if let Some(engine) = &self.engine {
            engine.count(&self.sender, what, 1);
        }
    }

    /// `instructions` as one transaction it signs on `blockhash`, asking
    /// for `units` (less a few, so each is unique) at `cu_price`, paying
    /// `tip` to a tip account after them.
    pub fn sign(
        &mut self,
        mut instructions: Vec<Instruction>,
        units: u32,
        cu_price: u64,
        tip: u64,
        blockhash: Hash,
    ) -> Result<Vec<u8>, String> {
        self.signed += 1;
        let limit = units.saturating_sub(self.signed % 4_096);
        let mut all = vec![compute_limit(limit)];
        if cu_price > 0 {
            all.push(compute_price(cu_price));
        }
        all.append(&mut instructions);
        if tip > 0 {
            all.push(tip_to(&self.keypair.pubkey(), self.signed, tip));
        }
        let message = Message::new_with_blockhash(&all, Some(&self.keypair.pubkey()), &blockhash);
        let tx = VersionedTransaction::try_new(VersionedMessage::Legacy(message), &[&self.keypair])
            .map_err(|e| e.to_string())?;
        wincode::serialize(&tx).map_err(|e| e.to_string())
    }

    /// Send `wires` through the pool: a bundle if `bundle`, on the Jito
    /// route; else a transaction on the RPC route. `now`: while a slot is
    /// built, to land in it.
    pub async fn send(
        &self,
        wires: Vec<Vec<u8>>,
        bundle: bool,
        now: bool,
    ) -> Result<Accepted, Refusal> {
        let Some(engine) = &self.engine else {
            return Err(Refusal {
                code: -32603,
                message: "no transaction layer".into(),
            });
        };
        engine
            .submit_actor(
                &self.sender,
                now,
                Submission {
                    wires,
                    route: if bundle {
                        RouteKind::Jito
                    } else {
                        RouteKind::Rpc
                    },
                    bundle,
                    retry_until_slot: None,
                    resign: false,
                    delay_slots: 0,
                    dropped: false,
                },
            )
            .await
    }
}

fn compute_limit(units: u32) -> Instruction {
    let mut data = vec![2];
    data.extend(units.to_le_bytes());
    Instruction::new_with_bytes(COMPUTE_BUDGET, &data, vec![])
}

fn compute_price(micro_lamports: u64) -> Instruction {
    let mut data = vec![3];
    data.extend(micro_lamports.to_le_bytes());
    Instruction::new_with_bytes(COMPUTE_BUDGET, &data, vec![])
}

/// A System transfer of `lamports` from `payer` to a tip account, the
/// `n`-th in turn.
fn tip_to(payer: &Pubkey, n: u32, lamports: u64) -> Instruction {
    let to: Pubkey = jito::TIP_ACCOUNTS[(n % 8) as usize]
        .parse()
        .expect("a tip account");
    let mut data = 2u32.to_le_bytes().to_vec();
    data.extend(lamports.to_le_bytes());
    Instruction::new_with_bytes(
        SYSTEM,
        &data,
        vec![AccountMeta::new(*payer, true), AccountMeta::new(to, false)],
    )
}

/// The amount a token account's data holds.
pub fn token_amount(data: &[u8]) -> Option<u64> {
    Some(u64::from_le_bytes(data.get(64..72)?.try_into().ok()?))
}

/// A searcher's view of one pool: the pool, and the chain it reads.
#[derive(Clone)]
pub struct PoolSide {
    pub pool: Arc<dyn Pool>,
    pub chain: Arc<dyn mtx_kit::chain::ChainClient>,
}

impl PoolSide {
    pub fn assets(&self) -> &PoolAssets {
        self.pool.assets()
    }

    /// Whether `tx` writes this pool.
    pub fn touched_by(&self, tx: &crate::txlayer::pool::PooledTx) -> bool {
        tx.writable.contains(&self.assets().address)
    }

    /// The swap instructions of `side` for `amount` of `unit`, with no
    /// worse than `limit` (0 or `f64::MAX` for none).
    pub async fn swap(
        &self,
        owner: &Pubkey,
        side: Side,
        amount: f64,
        unit: AmountUnit,
        limit: f64,
    ) -> Result<Vec<Instruction>, String> {
        self.pool
            .swap(
                self.chain.as_ref(),
                owner,
                &SwapOrder {
                    side,
                    amount,
                    unit,
                    limit,
                    partial: false,
                },
            )
            .await
    }

    /// The pool's price now, quote per base.
    pub async fn price(&self) -> Option<f64> {
        self.pool
            .price(self.chain.as_ref())
            .await
            .ok()
            .map(|p| p.price)
    }

    /// What `owner` holds of the pool's base and quote, in atoms, from a
    /// simulation's accounts (its two associated token accounts, in that
    /// order).
    pub fn wallet_keys(&self, owner: &Pubkey) -> Vec<Pubkey> {
        let a = self.assets();
        vec![
            mtx_kit::chain::token::ata_for(owner, &a.base_mint, &a.base_program),
            mtx_kit::chain::token::ata_for(owner, &a.quote_mint, &a.quote_program),
        ]
    }

    /// The vaults, base then quote.
    pub fn vaults(&self) -> Vec<Pubkey> {
        vec![self.assets().base_vault, self.assets().quote_vault]
    }

    /// What a pending swap of the bot's does to this pool, by simulating it
    /// alone: the side it takes and the base it moves, in whole units.
    pub async fn read_swap(&self, engine: &TxEngine, pending: &Pending) -> Option<(Side, f64)> {
        if !self.touched_by(&pending.tx) {
            return None;
        }
        let vaults = self.vaults();
        let base_before = self.balance(&vaults[0]).await?;
        let ran = engine
            .simulate_bundle(
                std::slice::from_ref(&pending.tx.wire),
                std::slice::from_ref(&vaults),
            )
            .await
            .ok()?;
        let after = ran.first().filter(|r| r.ok)?;
        let base_after = token_amount(after.accounts.first()?.as_ref()?)?;
        let units = 10f64.powi(i32::from(self.assets().base.decimals));
        if base_after > base_before {
            Some((Side::Ask, (base_after - base_before) as f64 / units))
        } else if base_after < base_before {
            Some((Side::Bid, (base_before - base_after) as f64 / units))
        } else {
            None
        }
    }

    /// A token account's amount now.
    pub async fn balance(&self, account: &Pubkey) -> Option<u64> {
        let found = self.chain.get_account(account).await.ok()??;
        token_amount(&found.data)
    }
}

/// The best of `candidates` by what `value` makes of each: each tried once,
/// in order, so the search is a fixed number of simulations.
pub async fn best_of<F, Fut>(candidates: &[f64], mut value: F) -> Option<(f64, f64)>
where
    F: FnMut(f64) -> Fut,
    Fut: std::future::Future<Output = Option<f64>>,
{
    let mut best: Option<(f64, f64)> = None;
    for &x in candidates {
        if let Some(v) = value(x).await
            && best.is_none_or(|(_, b)| v > b)
        {
            best = Some((x, v));
        }
    }
    best
}

/// The lamports `usd` buys at `sol_usd`.
pub fn lamports_of(usd: f64, sol_usd: f64) -> u64 {
    (usd.max(0.0) / sol_usd * 1e9).floor() as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn searchers_read_back_and_refuse_nonsense() {
        let json = r#"{
            "fee_bidders": [{ "venue": "raydium", "per_slot": 0.5, "size": 1.0 }],
            "sandwichers": [{ "venue": "raydium" }],
            "jit": [{ "venue": "meteora" }],
            "backrunners": [{ "venue": "raydium", "other": "meteora" }],
            "liquidators": { "shade": [0.2, 0.6], "bundles": true }
        }"#;
        let searchers: Searchers = serde_json::from_str(json).unwrap();
        assert_eq!(searchers.validate(), Ok(()));
        assert!(searchers.on_pools());
        assert_eq!(searchers.venues(), ["meteora", "raydium"]);
        let back: Searchers =
            serde_json::from_str(&serde_json::to_string(&searchers).unwrap()).unwrap();
        assert_eq!(back, searchers);
        assert!(Searchers::default().is_empty());
        let mut bad = searchers.clone();
        bad.liquidators = Some(LiquidatorRace {
            shade: [0.7, 0.2],
            bundles: false,
        });
        assert!(bad.validate().unwrap_err().contains("liquidators"));
        let mut bad = searchers;
        bad.sandwichers[0].tip_share = 2.0;
        assert!(bad.validate().unwrap_err().contains("sandwichers[0]"));
        assert!(serde_json::from_str::<Searchers>(r#"{ "nope": [] }"#).is_err());
    }

    #[test]
    fn a_race_draws_each_liquidators_share_in_its_range() {
        let race = LiquidatorRace {
            shade: [0.2, 0.6],
            bundles: false,
        };
        let shares: Vec<f64> = (0..20).map(|i| race.share(9, i)).collect();
        assert!(shares.iter().all(|s| (0.2..=0.6).contains(s)));
        assert_eq!(race.share(9, 3), race.share(9, 3));
        assert!(shares.windows(2).any(|w| w[0] != w[1]));
    }

    #[tokio::test]
    async fn the_best_candidate_wins_and_each_is_tried_once() {
        let mut tried = Vec::new();
        let best = best_of(&[4.0, 2.0, 1.0], |x| {
            tried.push(x);
            async move { (x < 4.0).then_some(10.0 - (x - 2.0).abs()) }
        })
        .await;
        assert_eq!(best, Some((2.0, 10.0)));
        assert_eq!(tried, [4.0, 2.0, 1.0]);
    }
}
