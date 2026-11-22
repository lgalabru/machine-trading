//! The whale: a trader working a large order into one pool, a chunk at a
//! time. Each chunk is a trigger (ADR 0004): the session lands it through
//! its own endpoint, as the ordinary transaction mainnet's chunks were (a
//! 15,000 lamport fee, no tip, `docs/RESEARCH.md`), in the slot the
//! scenario names, and ends the step right behind it.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use mtx_core::turns::Trigger;
use mtx_kit::{
    SendState,
    chain::{AddressLookupTableAccount, ChainClient, InstructionGroup, TransactionPlan},
};
use solana_keypair::Keypair;

use super::{
    TriggerRecord, cycle,
    prices::Prices,
    spec::{Chunk, Whale as WhaleSpec, WhaleSide},
};
use crate::{
    Error, Result,
    txlayer::draw,
    venue::{
        amm::{Direction, Mode, Pool, PoolSeat, State, SwapOrder},
        sent_plan,
    },
};

/// What a chunk pays beyond its base fee, in lamports: mainnet's chunks
/// paid 15,000 lamports in all, the 5,000 base fee and this.
const PRIORITY_LAMPORTS: u64 = 10_000;

/// The session's whale, landing its chunks as the steps reach them.
pub(crate) struct Whale {
    pub chain: Arc<dyn ChainClient>,
    /// Its accounts, which each chunk draws one of.
    pub accounts: Vec<Keypair>,
    /// Every chunk in the order it lands, with its pool.
    pub chunks: Vec<(Pool, WhaleSide, Chunk)>,
    pub pools: Vec<Pool>,
    /// The cycles through the pools, for the report's optimum.
    pub graph: cycle::Graph,
    pub prices: Prices,
    pub tables: Vec<AddressLookupTableAccount>,
    pub seed: u64,
    pub log: Arc<Mutex<Vec<TriggerRecord>>>,
}

impl Whale {
    /// The whale of `spec` on `pools`: its chunks in order, each with its
    /// pool, which the scenario's check found among them.
    pub fn chunks_of(spec: &WhaleSpec, pools: &[Pool]) -> Vec<(Pool, WhaleSide, Chunk)> {
        spec.chunks()
            .into_iter()
            .map(|(order, chunk)| {
                let order = &spec.orders[order];
                let pool = pools
                    .iter()
                    .find(|p| p.instrument.to_string() == order.pool)
                    .expect("a whale's pool is one of the session's");
                (pool.clone(), order.side, chunk)
            })
            .collect()
    }

    /// The two pools' gap now, where the session has two.
    async fn gap_bps(&self) -> Option<f64> {
        if self.pools.len() != 2 {
            return None;
        }
        let states = self.states().await?;
        Some(cycle::gap_bps(&self.pools, &states))
    }

    /// What the best cycle of up to [`cycle::LEGS_MAX`] legs would make now,
    /// in SOL.
    async fn cycle_optimum_sol(&self) -> Option<f64> {
        let states = self.states().await?;
        let max_in = (super::CYCLE_SOL_MAX * 1e9) as u64;
        let best = self
            .graph
            .best(&self.pools, &states, cycle::LEGS_MAX, max_in, None)?;
        Some(best.profit_atoms() as f64 / 1e9)
    }

    async fn states(&self) -> Option<Vec<State>> {
        let mut states = Vec::with_capacity(self.pools.len());
        for pool in &self.pools {
            states.push(State::read(self.chain.as_ref(), pool).await.ok()?);
        }
        Some(states)
    }
}

#[async_trait]
impl crate::trigger::Triggers for Whale {
    fn slots(&self) -> Vec<u64> {
        self.chunks.iter().map(|(_, _, chunk)| chunk.slot).collect()
    }

    async fn land(&mut self, i: usize, k: u64, slot: u64, at: u64) -> Result<Trigger> {
        let (pool, side, chunk) = self.chunks[i].clone();
        assert_eq!(chunk.slot, at, "a chunk lands in the step that ends on it");
        let account =
            (draw(self.seed, "whale-account", &[i as u64]) * self.accounts.len() as f64) as usize;
        let keypair = &self.accounts[account.min(self.accounts.len() - 1)];
        let label = pool.instrument.to_string();
        let state = State::read(self.chain.as_ref(), &pool).await?;
        let gap_before_bps = self.gap_bps().await;
        let base_units = self
            .prices
            .base_units(&pool, chunk.sol, self.prices.sol_usd(at));
        let atoms = (base_units * 10f64.powi(pool.base.asset.decimals.into())).round() as u64;
        let seat = PoolSeat::new(pool.clone(), self.chain.clone(), keypair.insecure_clone());
        let instructions = match chunk.impact_bps {
            None => market_order(&state, &seat, &pool, side, atoms)
                .map_err(|reason| crate::lending::venue_error(&label, reason))?,
            Some(impact_bps) => to_impact(&state, &seat, &pool, side, atoms, impact_bps)
                .map_err(|reason| crate::lending::venue_error(&label, reason))?,
        };
        let units = pool.program.compute_unit_limit();
        let plan =
            TransactionPlan::from_group(&InstructionGroup::new("whale chunk", instructions), units)
                .with_compute_unit_price(PRIORITY_LAMPORTS * 1_000_000 / u64::from(units))
                .with_lookup_tables(&self.tables);
        let signature = match sent_plan(self.chain.as_ref(), keypair, &plan).await.state {
            SendState::Landed(signature) => signature,
            other => {
                return Err(Error::Transaction {
                    label: format!("whale chunk {i} on {label}"),
                    reason: format!("{other:?}"),
                });
            }
        };
        // The ledger's slot for it: the open slot's, which the turns'
        // sends share.
        let landed_slot = match signature.parse() {
            Ok(parsed) => match self.chain.get_transaction(&parsed).await {
                Ok(Some(summary)) => summary.slot,
                Ok(None) | Err(_) => slot,
            },
            Err(_) => slot,
        };
        let record = TriggerRecord {
            k,
            at,
            pool: label.clone(),
            side,
            sol: chunk.sol,
            account: account as u32,
            signature: signature.clone(),
            gap_before_bps,
            gap_after_bps: self.gap_bps().await,
            cycle_optimum_sol: self.cycle_optimum_sol().await,
            landed_after: Vec::new(),
            winner: None,
            winner_after_slots: None,
        };
        self.log.lock().expect("trigger log").push(record);
        Ok(Trigger {
            signature,
            slot: landed_slot,
            market: label,
            side: match side {
                WhaleSide::Sell => mtx_core::Side::Sell,
                WhaleSide::Buy => mtx_core::Side::Buy,
            },
            base: chunk.sol,
        })
    }
}

/// A market order of `atoms` of the pool's base: a sale takes any price, a
/// buy pays any price.
fn market_order(
    state: &State,
    seat: &PoolSeat,
    pool: &Pool,
    side: WhaleSide,
    atoms: u64,
) -> std::result::Result<Vec<solana_transaction::Instruction>, String> {
    let order = match side {
        WhaleSide::Sell => SwapOrder {
            direction: Direction::BaseIn,
            mode: Mode::ExactIn,
            amount: atoms,
            other_amount_threshold: 0,
        },
        WhaleSide::Buy => SwapOrder {
            direction: Direction::QuoteIn,
            mode: Mode::ExactOut,
            amount: atoms,
            other_amount_threshold: u64::MAX,
        },
    };
    let quoted = state.quote(&pool.address, order.direction, order.mode, order.amount)?;
    Ok(seat.swap_instructions(state, &order, &quoted))
}

/// The swap that moves the pool's price by `impact_bps` (a sale divides it
/// by `1 + impact`, a buy multiplies it), trading at most `atoms` of the
/// base's worth: the program stops it at the price. Where the pool's
/// liquidity (or its cloned tick arrays) ends short of that price, the
/// impact is halved until the swap fills, as a trader's swap would stop
/// where the pool does.
fn to_impact(
    state: &State,
    seat: &PoolSeat,
    pool: &Pool,
    side: WhaleSide,
    atoms: u64,
    impact_bps: f64,
) -> std::result::Result<Vec<solana_transaction::Instruction>, String> {
    assert!(impact_bps > 0.0 && impact_bps.is_finite());
    let mid = state.mid(pool);
    let mut impact_bps = impact_bps;
    let mut last_error = String::new();
    for _ in 0..IMPACT_HALVINGS_MAX {
        let factor = 1.0 + impact_bps / 10_000.0;
        let (target, max_in) = match side {
            WhaleSide::Sell => (mid / factor, atoms),
            WhaleSide::Buy => {
                let base_units = atoms as f64 / 10f64.powi(pool.base.asset.decimals.into());
                let quote_units = base_units * mid * factor;
                (
                    mid * factor,
                    (quote_units * 10f64.powi(pool.quote.asset.decimals.into())) as u64,
                )
            }
        };
        match state.quote_to_price(pool, target, max_in) {
            Ok(Some((quoted, limit))) => return Ok(seat.swap_to_price(state, &quoted, limit)),
            Ok(None) => return Err("the pool is at the chunk's price already".into()),
            Err(error) => last_error = error,
        }
        impact_bps /= 2.0;
    }
    Err(last_error)
}

/// How many times a chunk's impact is halved before it gives up: 2^-12 of
/// the largest impact is under 3 bp.
const IMPACT_HALVINGS_MAX: u32 = 12;
