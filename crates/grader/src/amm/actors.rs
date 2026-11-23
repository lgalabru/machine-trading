//! An `amm` session's actors in the transaction layer's auction, each
//! under its own name, as the bot is (ADR 0004):
//!
//! - **The calm flow** ([`Flow`]): small swaps of either side on each
//!   pool, at the research's rate and sizes, ordinary transactions with a
//!   small priority fee.
//! - **Cycle searchers** ([`CycleSearchers`]): each step, each reads every
//!   pool, finds the best cycle from SOL back to SOL of at most its
//!   `max_legs` legs on exact quotes ([`super::cycle`]), and if it pays
//!   sends it as one atomic transaction, its last swap's threshold its
//!   least profit, bidding a share of what it expects as a priority fee or
//!   a tip, after a latency of its own.
//! - **Pool searchers** ([`PoolSearchers`]): each step, each reads every
//!   pool and trades one that strayed past its fee from its reference price
//!   back to it ([`super::prices`]), as a trader with a deeper venue
//!   outside the session does, bidding most of what it expects. Milestone
//!   D's pools arbitrageur, competing.
//!
//! Every draw comes from the scenario's seed through
//! [`crate::txlayer::draw`], keyed by the actor, the slot or the step, so
//! a session replays its competition exactly. A searcher sends again only
//! once its last send is no longer pending. In a trigger step, what they
//! send reaches the trigger's open slot with no route latency, after their
//! own (see [`crate::txlayer::TxEngine::set_trigger_open`]).

use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};

use async_trait::async_trait;
use mtx_kit::chain::{AddressLookupTableAccount, ChainClient, InstructionGroup, TransactionPlan};
use solana_keypair::Keypair;
use solana_transaction::Instruction;
use tracing::warn;

use super::{
    SearcherAttempt, cycle,
    prices::Prices,
    spec::{CycleSearcher, Pays, PoolFlow, PoolSearcher},
};
use crate::{
    actors::{Actor, ActorsReport, Moment},
    lending::auction::{simulate, submit},
    txlayer::{TxEngine, draw, engine::Fate, jito, normal},
    venue::amm::{Direction, Mode, Pool, PoolSeat, State, SwapOrder},
};

/// Slots a minute, at mainnet's 400 ms a slot: the research's rates are a
/// minute's.
const SLOTS_PER_MINUTE: f64 = 150.0;
/// The standard normal's 90th percentile: a log-normal size's p90 is its
/// median times `exp(Z_90 * sigma)`.
const Z_90: f64 = 1.281_551_565_545;
/// What a calm swap asks and pays per unit: a small swap's units with room,
/// and a price that makes its fee about mainnet's 15,000 lamports.
const FLOW_UNITS: u32 = 300_000;
const FLOW_CU_PRICE: u64 = 30_000;
/// How much less than its quote each of a cycle's legs but the last may
/// give: 10 bp, about a calm slot's moves on the pools (the research's calm
/// gap: p99 4.8 bp).
const SALE_SLIPPAGE: f64 = 0.001;
/// Units a searcher's simulation may use: up to four swaps that cross many
/// ticks.
const SEARCHER_UNITS_MAX: u32 = 1_400_000;
/// The margin a searcher's limit adds to its simulation's units
/// (as the liquidators', [`crate::lending::auction`]).
const UNITS_MARGIN: f64 = 0.1;
const UNITS_FLOOR: u64 = 1_000;
/// The largest swap a pool searcher sends, in SOL or its worth: far past a
/// session's chunk.
const POOL_SEARCHER_SOL_MAX: f64 = 20_000.0;

/// What a searcher's sends share: the chain it reads, the layer it bids
/// in, the session's lookup table and seed, and the log the report reads.
#[derive(Clone)]
pub(crate) struct Bidding {
    pub chain: Arc<dyn ChainClient>,
    pub engine: Option<Arc<TxEngine>>,
    pub tables: Vec<AddressLookupTableAccount>,
    pub seed: u64,
    pub log: Arc<Mutex<Vec<SearcherAttempt>>>,
}

impl Bidding {
    /// Simulate `instructions` as `signer`'s, price them by `bid_lamports`
    /// as `pays` says, and send them into the layer `delay_slots` late, as
    /// `sender`'s. Returns the signature.
    async fn send(
        &self,
        signer: &Keypair,
        sender: &str,
        instructions: Vec<Instruction>,
        pays: Pays,
        bid_lamports: u64,
        delay_slots: u64,
    ) -> Result<String, String> {
        let engine = self.engine.as_ref().ok_or("an amm session has a layer")?;
        let group = InstructionGroup::new(sender, instructions);
        let simulated = TransactionPlan::from_group(&group, SEARCHER_UNITS_MAX)
            .with_lookup_tables(&self.tables);
        let units = simulate(self.chain.as_ref(), signer, &simulated).await?;
        let limit = ((units as f64 * (1.0 + UNITS_MARGIN)) as u64 + UNITS_FLOOR)
            .min(u64::from(SEARCHER_UNITS_MAX)) as u32;
        let plan = TransactionPlan::from_group(&group, limit).with_lookup_tables(&self.tables);
        let (plan, tip) = match pays {
            Pays::Tip => (plan, Some(bid_lamports.max(jito::MIN_TIP))),
            Pays::PriorityFee => {
                let price = u64::try_from(u128::from(bid_lamports) * 1_000_000 / u128::from(limit))
                    .unwrap_or(u64::MAX);
                (plan.with_compute_unit_price(price), None)
            }
        };
        submit(
            engine,
            self.chain.as_ref(),
            signer,
            sender,
            &plan,
            tip,
            delay_slots,
        )
        .await
    }

    /// Whether `signature`'s send is still waiting for a block.
    fn pending(&self, signature: &str) -> bool {
        self.engine
            .as_ref()
            .and_then(|engine| engine.fate(signature))
            .is_some_and(|fate| fate == Fate::Pending)
    }

    /// A latency in `range`, drawn for `sender`'s `n`th look.
    fn latency(&self, sender: &str, range: [u64; 2], n: u64) -> u64 {
        let key = crate::txlayer::hash(0, "sender", &[], sender.as_bytes());
        let u = draw(self.seed, "searcher-latency", &[key, n]);
        let span = range[1] - range[0] + 1;
        range[0] + ((u * span as f64) as u64).min(span - 1)
    }

    fn record(&self, attempt: SearcherAttempt) {
        self.log.lock().expect("attempts log").push(attempt);
    }
}

/// The calm flow: one account per pool, swapping as the draws say.
pub(crate) struct Flow {
    pub bidding: Bidding,
    pub prices: Prices,
    /// Each pool with its flow and the account that trades it.
    pub pools: Vec<(Pool, PoolFlow, Keypair)>,
    /// The last slot drawn, from the session's start.
    pub drawn_to: u64,
}

impl Flow {
    /// The swap pool `i` draws at `slot`, if any: its side (a sale, or
    /// not) and size in SOL's worth.
    fn swap_at(&self, i: usize, slot: u64) -> Option<(bool, f64)> {
        let flow = &self.pools[i].1;
        let keys = [i as u64, slot];
        if draw(self.bidding.seed, "flow", &keys) >= flow.swaps_per_minute / SLOTS_PER_MINUTE {
            return None;
        }
        let sigma = (flow.p90_sol / flow.median_sol).ln() / Z_90;
        let size = flow.median_sol * (sigma * normal(self.bidding.seed, "flow-size", &keys)).exp();
        let sells = draw(self.bidding.seed, "flow-side", &keys) < 0.5;
        Some((sells, size.min(flow.max_sol)))
    }

    async fn swap(
        &self,
        i: usize,
        sells: bool,
        sol: f64,
        sol_usd: f64,
        at: u64,
    ) -> Result<(), String> {
        let (pool, _, keypair) = &self.pools[i];
        let state = State::read(self.bidding.chain.as_ref(), pool)
            .await
            .map_err(|e| e.to_string())?;
        let base = self.prices.base_units(pool, sol, sol_usd, at);
        let order = match sells {
            true => SwapOrder {
                direction: Direction::BaseIn,
                mode: Mode::ExactIn,
                amount: (base * 10f64.powi(pool.base.asset.decimals.into())) as u64,
                other_amount_threshold: 0,
            },
            false => SwapOrder {
                direction: Direction::QuoteIn,
                mode: Mode::ExactIn,
                amount: (base * state.mid(pool) * 10f64.powi(pool.quote.asset.decimals.into()))
                    as u64,
                other_amount_threshold: 0,
            },
        };
        let quoted = state.quote(&pool.address, order.direction, order.mode, order.amount)?;
        let seat = PoolSeat::new(
            pool.clone(),
            self.bidding.chain.clone(),
            keypair.insecure_clone(),
        );
        let plan = TransactionPlan::from_group(
            &InstructionGroup::new(
                "background",
                seat.swap_instructions(&state, &order, &quoted),
            ),
            FLOW_UNITS,
        )
        .with_compute_unit_price(FLOW_CU_PRICE)
        .with_lookup_tables(&self.bidding.tables);
        let engine = self
            .bidding
            .engine
            .as_ref()
            .ok_or("an amm session has a layer")?;
        submit(
            engine,
            self.bidding.chain.as_ref(),
            keypair,
            "background",
            &plan,
            None,
            0,
        )
        .await
        .map(|_| ())
    }
}

#[async_trait]
impl Actor for Flow {
    /// The swaps drawn for the slots the step moved through, sent now:
    /// they land in the blocks that follow.
    async fn after_blocks(&mut self, now: &Moment) {
        for slot in self.drawn_to + 1..=now.at {
            for i in 0..self.pools.len() {
                let Some((sells, sol)) = self.swap_at(i, slot) else {
                    continue;
                };
                if let Err(error) = self.swap(i, sells, sol, now.level_usd, now.at).await {
                    warn!(pool = %self.pools[i].0.instrument, %error, "a calm swap wasn't sent");
                }
            }
        }
        self.drawn_to = self.drawn_to.max(now.at);
    }

    fn attach_tx(&mut self, engine: Arc<TxEngine>) {
        self.bidding.engine = Some(engine);
    }

    async fn report(&mut self, _report: &mut ActorsReport) {}
}

/// The session's cycle searchers.
pub(crate) struct CycleSearchers {
    pub bidding: Bidding,
    /// The cycles through the session's pools.
    pub graph: cycle::Graph,
    pub pools: Vec<Pool>,
    pub searchers: Vec<(CycleSearcher, Keypair)>,
    /// Each searcher's last send, while it may still be pending.
    pub last: BTreeMap<usize, String>,
    pub looks: u64,
    pub step: u64,
}

impl CycleSearchers {
    fn name(i: usize) -> String {
        format!("cycle-searcher/{i}")
    }

    /// Searcher `i`'s look at `states`: its cycle's instructions, the
    /// profit it expects in SOL, and its bid in lamports, if it sends.
    fn plan(&self, i: usize, states: &[State]) -> Option<(Vec<Instruction>, f64, u64)> {
        let (spec, keypair) = &self.searchers[i];
        let fixed = spec.fixed_sol.map(|sol| (sol * 1e9) as u64);
        let max_in = (super::CYCLE_SOL_MAX * 1e9) as u64;
        let max_legs = usize::try_from(spec.max_legs).expect("checked: 2 to 4 legs");
        let cycle = self
            .graph
            .best(&self.pools, states, max_legs, max_in, fixed)?;
        let profit_sol = cycle.profit_atoms() as f64 / 1e9;
        if profit_sol < spec.min_profit_sol.max(f64::MIN_POSITIVE) {
            return None;
        }
        let min_profit_atoms = (spec.min_profit_sol * 1e9) as u64;
        // Each leg but the last may give a little less than it was quoted
        // (the pool may move first, and the searcher's own tokens cover the
        // difference), and the last must give back the SOL put in and the
        // least profit: the cycle lands whole or not.
        let last = cycle.legs.len() - 1;
        let mut instructions = Vec::new();
        for (n, (leg, quoted)) in cycle.legs.iter().zip(&cycle.quotes).enumerate() {
            let threshold = match n == last {
                true => cycle.sol_in_atoms() + min_profit_atoms,
                false => (quoted.amount_out as f64 * (1.0 - SALE_SLIPPAGE)) as u64,
            };
            let order = SwapOrder {
                direction: leg.direction,
                mode: Mode::ExactIn,
                amount: quoted.amount_in,
                other_amount_threshold: threshold,
            };
            let seat = PoolSeat::new(
                self.pools[leg.pool].clone(),
                self.bidding.chain.clone(),
                keypair.insecure_clone(),
            );
            instructions.extend(seat.swap_instructions(&states[leg.pool], &order, quoted));
        }
        let bid = (profit_sol * spec.bid_share * 1e9) as u64;
        Some((instructions, profit_sol, bid))
    }
}

#[async_trait]
impl Actor for CycleSearchers {
    async fn after_blocks(&mut self, now: &Moment) {
        let mut states = Vec::with_capacity(self.pools.len());
        for pool in &self.pools {
            match State::read(self.bidding.chain.as_ref(), pool).await {
                Ok(state) => states.push(state),
                Err(error) => {
                    warn!(%error, "a cycle searcher couldn't read a pool");
                    return;
                }
            }
        }
        for i in 0..self.searchers.len() {
            if self.last.get(&i).is_some_and(|s| self.bidding.pending(s)) {
                continue;
            }
            let Some((instructions, profit_sol, bid)) = self.plan(i, &states) else {
                continue;
            };
            let name = Self::name(i);
            let (spec, keypair) = &self.searchers[i];
            self.looks += 1;
            let delay = self.bidding.latency(&name, spec.latency_slots, self.looks);
            match self
                .bidding
                .send(keypair, &name, instructions, spec.pays, bid, delay)
                .await
            {
                Ok(signature) => {
                    self.bidding.record(SearcherAttempt {
                        k: self.step,
                        at: now.at,
                        sender: name,
                        signature: signature.clone(),
                        pool: None,
                        expected_sol: profit_sol,
                        bid_lamports: bid,
                        tip: spec.pays == Pays::Tip,
                        delay_slots: delay,
                        outcome: None,
                    });
                    self.last.insert(i, signature);
                }
                Err(error) => warn!(searcher = name, %error, "a cycle searcher's send failed"),
            }
        }
        self.step += 1;
    }

    fn attach_tx(&mut self, engine: Arc<TxEngine>) {
        self.bidding.engine = Some(engine);
    }

    async fn report(&mut self, _report: &mut ActorsReport) {}
}

/// The session's pool searchers.
pub(crate) struct PoolSearchers {
    pub bidding: Bidding,
    pub prices: Prices,
    pub pools: Vec<Pool>,
    pub searchers: Vec<(PoolSearcher, Keypair)>,
    /// Each searcher's last send on each pool, while it may be pending.
    pub last: BTreeMap<(usize, usize), String>,
    pub looks: u64,
    pub step: u64,
}

impl PoolSearchers {
    fn name(i: usize) -> String {
        format!("pool-searcher/{i}")
    }

    /// Searcher `i`'s trade of pool `p` back to its reference price, SOL
    /// being worth `sol_usd`, as its instructions, the profit it expects in
    /// SOL at the reference, and its bid in lamports, if it trades.
    fn plan(
        &self,
        i: usize,
        p: usize,
        state: &State,
        sol_usd: f64,
        at: u64,
    ) -> Option<(Vec<Instruction>, f64, u64)> {
        let (spec, keypair) = &self.searchers[i];
        let pool = &self.pools[p];
        let reference = self.prices.pool_price(pool, sol_usd, at);
        let fee = state.fee_fraction();
        let off = state.mid(pool) / reference - 1.0;
        if off.abs() <= fee + spec.margin_bps / 10_000.0 {
            return None;
        }
        // Where trading more stops paying: the pool's price at the
        // reference, past the fee.
        let base_max = self
            .prices
            .base_units(pool, POOL_SEARCHER_SOL_MAX, sol_usd, at);
        let (target, max_in) = match off > 0.0 {
            true => (
                reference * (1.0 + fee),
                (base_max * 10f64.powi(pool.base.asset.decimals.into())) as u64,
            ),
            false => (
                reference * (1.0 - fee),
                (base_max * reference * 10f64.powi(pool.quote.asset.decimals.into())) as u64,
            ),
        };
        let (quoted, limit) = state.quote_to_price(pool, target, max_in).ok()??;
        let base = |atoms: u64| atoms as f64 / 10f64.powi(pool.base.asset.decimals.into());
        let quote = |atoms: u64| atoms as f64 / 10f64.powi(pool.quote.asset.decimals.into());
        let sol =
            |mint: &solana_pubkey::Pubkey, units: f64| self.prices.in_sol(mint, units, sol_usd, at);
        let (base_mint, quote_mint) = (&pool.base.mint, &pool.quote.mint);
        let profit_sol = match quoted.direction {
            Direction::BaseIn => {
                sol(quote_mint, quote(quoted.amount_out)) - sol(base_mint, base(quoted.amount_in))
            }
            Direction::QuoteIn => {
                sol(base_mint, base(quoted.amount_out)) - sol(quote_mint, quote(quoted.amount_in))
            }
        };
        if profit_sol < spec.min_profit_sol.max(f64::MIN_POSITIVE) {
            return None;
        }
        let seat = PoolSeat::new(
            pool.clone(),
            self.bidding.chain.clone(),
            keypair.insecure_clone(),
        );
        let instructions = seat.swap_to_price(state, &quoted, limit);
        let bid = (profit_sol * spec.bid_share * 1e9) as u64;
        Some((instructions, profit_sol, bid))
    }
}

#[async_trait]
impl Actor for PoolSearchers {
    async fn after_blocks(&mut self, now: &Moment) {
        let sol_usd = now.level_usd;
        for p in 0..self.pools.len() {
            let state = match State::read(self.bidding.chain.as_ref(), &self.pools[p]).await {
                Ok(state) => state,
                Err(error) => {
                    warn!(%error, "a pool searcher couldn't read a pool");
                    continue;
                }
            };
            for i in 0..self.searchers.len() {
                if self
                    .last
                    .get(&(i, p))
                    .is_some_and(|s| self.bidding.pending(s))
                {
                    continue;
                }
                let Some((instructions, profit_sol, bid)) =
                    self.plan(i, p, &state, sol_usd, now.at)
                else {
                    continue;
                };
                let name = Self::name(i);
                let (spec, keypair) = &self.searchers[i];
                self.looks += 1;
                let delay = self.bidding.latency(&name, spec.latency_slots, self.looks);
                match self
                    .bidding
                    .send(keypair, &name, instructions, spec.pays, bid, delay)
                    .await
                {
                    Ok(signature) => {
                        self.bidding.record(SearcherAttempt {
                            k: self.step,
                            at: now.at,
                            sender: name,
                            signature: signature.clone(),
                            pool: Some(self.pools[p].instrument.to_string()),
                            expected_sol: profit_sol,
                            bid_lamports: bid,
                            tip: spec.pays == Pays::Tip,
                            delay_slots: delay,
                            outcome: None,
                        });
                        self.last.insert((i, p), signature);
                    }
                    Err(error) => warn!(searcher = name, %error, "a pool searcher's send failed"),
                }
            }
        }
        self.step += 1;
    }

    fn attach_tx(&mut self, engine: Arc<TxEngine>) {
        self.bidding.engine = Some(engine);
    }

    async fn report(&mut self, _report: &mut ActorsReport) {}
}
