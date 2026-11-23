//! The pools' arbitrageur in a lending session with pools: after each
//! step's blocks, before the other actors and the bot act, it swaps each
//! pool back to the scenario's price, as the CEX-DEX arbitrage that keeps
//! mainnet's SOL/USDC pools on the reference does every slot.
//!
//! Without it nothing would move the pools but the session's own swaps
//! (RFD 0005, "Moving the pools"), and a liquidator selling seized SOL
//! into a pool still at the session's opening price after a 4% fall would
//! earn the fall, not the bonus. Its swaps are real ones, stopped by the
//! program at the reference ([`crate::venue::amm::State::quote_to_price`]),
//! so each pool's ticks and liquidity stay consistent. They land at once
//! through the grader's endpoint, like the borrowers' own actions: the
//! pools are on the reference when a step's turns begin.

use std::{collections::BTreeMap, sync::Arc};

use async_trait::async_trait;
use mtx_kit::{
    SendState,
    chain::{AddressLookupTableAccount, ChainClient, InstructionGroup, TransactionPlan},
};
use solana_keypair::Keypair;
use solana_pubkey::Pubkey;
use tracing::warn;

use crate::{
    actors::{Actor, ActorsReport, Moment},
    oracles::Feed,
    venue::{
        amm::{Pool, PoolSeat, State},
        kamino::Books,
        sent_plan,
    },
};

/// How far a pool may sit from the reference before the arbitrageur moves
/// it, in basis points: the SOL/USDC pools' 4 bp fee, which an arbitrage
/// inside it can't pay for.
pub const BAND_BPS: f64 = 4.0;

/// What the arbitrageur holds of each token, in whole units, for its
/// swaps: far more than a session's 5% move takes through either pool
/// (500 SOL moved Whirlpool 3 bp and Raydium 15 bp, README of
/// `snapshots/atomic-arb-2026-11-21`).
pub const BASE_UNITS: f64 = 200_000.0;
pub const QUOTE_UNITS: f64 = 20_000_000.0;

/// The scenario's price of a pool's base in its quote, `at` slots into the
/// session.
pub type Reference = Arc<dyn Fn(&Pool, u64) -> Option<f64> + Send + Sync>;

/// The scenario's price of each pool's base in its quote: the ratio of the
/// paths of the reserves of its two assets (`start_prices` and `feeds` by
/// reserve, as the account values them).
pub fn reference(
    books: &Books,
    start_prices: BTreeMap<Pubkey, f64>,
    feeds: BTreeMap<Pubkey, Feed>,
) -> Reference {
    let reserve_of: BTreeMap<Pubkey, Pubkey> = books
        .reserves
        .iter()
        .map(|b| (b.keys.mint, b.keys.address))
        .collect();
    Arc::new(move |pool: &Pool, at: u64| {
        let price = |mint: &Pubkey| {
            let reserve = reserve_of.get(mint)?;
            let start = *start_prices.get(reserve)?;
            Some(
                feeds
                    .get(reserve)
                    .map_or(start, |f| f.true_price(start, at)),
            )
        };
        Some(price(&pool.base.mint)? / price(&pool.quote.mint)?)
    })
}

pub struct PoolArbitrageur {
    pub chain: Arc<dyn ChainClient>,
    pub keypair: Keypair,
    pub pools: Vec<Pool>,
    pub tables: Vec<AddressLookupTableAccount>,
    pub reference: Reference,
}

impl PoolArbitrageur {
    /// Swap `pool` back to its reference, if it strayed past the band.
    async fn rebalance(&self, pool: &Pool, at: u64) -> Result<(), String> {
        let Some(reference) = (self.reference)(pool, at) else {
            return Ok(());
        };
        let state = State::read(self.chain.as_ref(), pool)
            .await
            .map_err(|e| e.to_string())?;
        let mid = state.mid(pool);
        let off_bps = (mid / reference - 1.0) * 10_000.0;
        if off_bps.abs() <= BAND_BPS {
            return Ok(());
        }
        let max_in = match off_bps > 0.0 {
            true => (BASE_UNITS * 10f64.powi(pool.base.asset.decimals.into())) as u64,
            false => (QUOTE_UNITS * 10f64.powi(pool.quote.asset.decimals.into())) as u64,
        };
        let Some((quoted, limit)) = state.quote_to_price(pool, reference, max_in)? else {
            return Ok(());
        };
        let seat = PoolSeat::new(
            pool.clone(),
            self.chain.clone(),
            self.keypair.insecure_clone(),
        );
        let instructions = seat.swap_to_price(&state, &quoted, limit);
        let plan = TransactionPlan::from_group(
            &InstructionGroup::new("rebalance", instructions),
            pool.program.compute_unit_limit(),
        )
        .with_lookup_tables(&self.tables);
        match sent_plan(self.chain.as_ref(), &self.keypair, &plan)
            .await
            .state
        {
            SendState::Landed(_) => Ok(()),
            other => Err(format!("{other:?}")),
        }
    }
}

#[async_trait]
impl Actor for PoolArbitrageur {
    async fn after_blocks(&mut self, now: &Moment) {
        for pool in &self.pools {
            if let Err(error) = self.rebalance(pool, now.at).await {
                warn!(pool = %pool.instrument, %error, "the pools' arbitrageur couldn't rebalance");
            }
        }
    }

    async fn report(&mut self, _report: &mut ActorsReport) {}
}
