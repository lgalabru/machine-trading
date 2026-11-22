//! `amm` sessions: a bot with a wallet of SOL and USDC among concentrated
//! pools, the actors that move them, and the searchers that compete for
//! what their moves open (RFD 0005, tasks 2 and 3: `backrun-sol-usdc` on two
//! SOL/USDC pools, `backrun-thin-pools` on a graph of ten pools over five
//! tokens).
//!
//! A scenario whose venues are `venues.amm` alone runs here. The session is
//! the same as any other (see [`crate::session`]); what differs:
//!
//! 1. **Setup**, before the cheatcodes lock: the bot's wallet holds the
//!    scenario's SOL (wrapped) and USDC, and an empty account of every
//!    other token its pools trade; the whale's accounts, the calm flow's
//!    and each searcher's get theirs; the session's lookup tables hold
//!    every account a swap through the pools names, the bot's token
//!    accounts with them, 256 a table ([`crate::lending::route`]).
//! 2. **Triggers.** Each of the whale's chunks ends a step in the slot it
//!    lands in ([`crate::trigger`], [`whale`]), with that slot still open.
//! 3. **Every step**, after the blocks: the calm flow, then the cycle
//!    searchers, then the pool searchers send into the transaction layer's
//!    auction under their own names ([`actors`]); then the bot's turn.
//! 4. **The account** is the wallet's wrapped SOL, USDC and other tokens,
//!    valued at the scenario's reference prices ([`prices`]: SOL's
//!    `reference_usd`, moved by `market_model.path`, and each other
//!    token's `reference_sol`), never at the pools' own prices, which a
//!    trade moves. Where the mandate's numeraire is SOL, the account is
//!    measured in SOL (its USDC at the reference then) and priced at the
//!    starting reference, so holding SOL while the reference moves makes
//!    nothing.
//!    The wallet's native SOL pays fees, which the transaction layer counts
//!    in PnL (`fees_in_pnl`).
//! 5. **The audit** (mandate v0.3.0): every landed or failed transaction of
//!    the bot's is read back from the chain for its costs (rule 13), what
//!    the account held beside SOL right after it (rule 11, from the
//!    transaction's own token balances), and its fills, from the pools'
//!    vaults (rule 1).

mod actors;
pub mod cycle;
mod prices;
mod spec;
mod whale;

use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::{Arc, Mutex},
};

use async_trait::async_trait;
use mtx_core::{
    Side,
    mandate::{
        InstrumentRef, Mandate, Observed, ObservedFill, ObservedPool, ObservedTransaction,
        TransactionOutcome,
    },
    turns::Step,
};
use mtx_kit::{
    BotEnv,
    chain::{AddressLookupTableAccount, ChainClient, RpcChainClient},
};
use serde::{Deserialize, Serialize};
use solana_keypair::Keypair;
use solana_pubkey::Pubkey;
use solana_signer::Signer;
use surfpool_sdk::{CheatcodeFilter, SimnetCommand};
use tracing::info;

use self::prices::Prices;
pub use self::spec::*;
use crate::{
    Error, Result,
    actors::{Cast, Moment},
    error::io_error,
    guard::Guard,
    lending::{FEE_SOL, observe, venue_error},
    scenario::Scenario,
    session::{
        self, Account, BOT_USERS, Done, IxRecord, Prepared, SLOTS_PER_STEP, SessionDir,
        SessionOptions, SessionReport, StepRecord, Timeline, World,
    },
    snapshot,
    spot::{VenueReading, Wallet},
    txlayer::report::TxLayerReport,
    venue::{
        SeatChain, Venue,
        amm::{Amm, Pool, State},
    },
    wasm::{Desk, Reader},
};

/// What an `amm` session's wallet may hold, by asset.
pub const WALLET_ASSETS: [&str; 2] = ["SOL", "USDC"];
/// The most of either a wallet may hold, in whole units: far past a cycle's
/// size.
pub const WALLET_UNITS_MAX: f64 = 1_000_000.0;
/// Searchers a session may have in all: the research's busiest slot had up
/// to 44 failing signers, most of them one-off.
pub const SEARCHERS_MAX: usize = 16;
/// The largest cycle a searcher (or a report's optimum) sizes, in SOL: the
/// research's largest arb put in 996.
pub const CYCLE_SOL_MAX: f64 = 5_000.0;
/// What each searcher's and the flow's accounts hold of each token: far
/// more than any trade of theirs takes (the research's largest arb, 996
/// SOL).
const ACTOR_SOL: f64 = 200_000.0;
const ACTOR_USDC: f64 = 20_000_000.0;
/// What each actor holds of native SOL for its fees and bids: a pool
/// searcher bids most of a gross that reached 1 SOL a backrun, and pays it
/// whether its transaction runs or fails.
const ACTOR_NATIVE_SOL: f64 = 1_000.0;
/// How long after a trigger a backrun is counted as its, in slots: the
/// research's late responses landed within a slot or two.
pub const BACKRUN_WINDOW_SLOTS: u64 = 4;

/// What an `amm` session reports beyond the common fields.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AmmReport {
    /// SOL's reference price at the start and the end, in USD.
    pub reference_start_usd: f64,
    pub reference_end_usd: f64,
    /// The wallet at the start and the end, at the reference.
    pub start: Wallet,
    pub end: Wallet,
    /// What the wallet held at the end of every token beside SOL and USDC,
    /// by mint, in units.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub end_others: BTreeMap<String, f64>,
    /// The bot's fills, from the pools' vaults, in the order it sent them.
    pub fills: Vec<AmmFill>,
    /// Each of the bot's transactions, as rules 11 to 13 read it.
    pub transactions: Vec<ObservedTransaction>,
    pub pools: Vec<ObservedPool>,
    /// Each trigger, and who backran it.
    pub triggers: Vec<TriggerRecord>,
    /// Each searcher's send, and what became of it.
    pub attempts: Vec<SearcherAttempt>,
}

/// One of the bot's swaps, as a pool's vaults recorded it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AmmFill {
    pub signature: String,
    pub at: u64,
    pub instrument: InstrumentRef,
    /// The bot's side: `buy` bought the pool's base.
    pub side: Side,
    /// Base units, and quote units per base unit, the pool's fee included.
    pub base: f64,
    pub price: f64,
}

/// One trigger: the whale's chunk, the gap it opened, and what landed
/// behind it in its slot.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TriggerRecord {
    /// The step it ended, and its slot from the session's start.
    pub k: u64,
    pub at: u64,
    pub pool: String,
    pub side: WhaleSide,
    pub sol: f64,
    /// Which of the whale's accounts sent it.
    pub account: u32,
    pub signature: String,
    /// The gap between the two pools before and after it, in basis points,
    /// where the session has two pools, and what the best cycle of up to
    /// [`cycle::LEGS_MAX`] legs would have made right after it, in SOL.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gap_before_bps: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gap_after_bps: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cycle_optimum_sol: Option<f64>,
    /// What the layer landed in the rest of its slot, in order: each
    /// sender and whether it ran.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub landed_after: Vec<(String, bool)>,
    /// The first of the bot and the searchers to land and run behind it,
    /// within [`BACKRUN_WINDOW_SLOTS`], and how many slots after it: 0 is
    /// its own slot.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub winner: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub winner_after_slots: Option<u64>,
}

/// One searcher's send.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SearcherAttempt {
    pub k: u64,
    pub at: u64,
    pub sender: String,
    pub signature: String,
    /// The pool a pool searcher traded; none for a cycle.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pool: Option<String>,
    /// What it expected to make, in SOL, and what it bid of it.
    pub expected_sol: f64,
    pub bid_lamports: u64,
    pub tip: bool,
    pub delay_slots: u64,
    /// `landed`, `failed` (on chain, or without running), `expired` or
    /// `pending`, once the session ends.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub outcome: Option<String>,
}

/// SOL's reference price `at` slots in: the start, moved by the scenario's
/// path.
fn reference_at(start_usd: f64, path: &[crate::scenario::Waypoint], at: u64) -> f64 {
    start_usd * (1.0 + crate::making::level_at(path, at) / 100.0)
}

/// Set an `amm` session up, up to the bot's start; see the module docs.
pub(crate) async fn prepare(opts: &SessionOptions<'_>) -> Result<Prepared> {
    let scenario = opts.scenario;
    let venue = scenario
        .amm
        .as_ref()
        .expect("an amm session's scenario has an amm section");
    let seed = venue.seed.expect("checked: an amm session has a seed");
    let snapshot_dir = opts
        .snapshot_dir
        .canonicalize()
        .map_err(io_error(opts.snapshot_dir))?;
    let (surfnet, _) = snapshot::start_snapshot_stepped(&snapshot_dir).await?;
    let internal = Guard::internal(surfnet.rpc_url()).await?;
    let rpc = mtx_kit::rpc_client_with_timeout(internal.url(), session::INTERNAL_RPC_TIMEOUT);
    let setup_from = rpc.get_slot()?;
    let chain: Arc<dyn ChainClient> = Arc::new(RpcChainClient::with_timeout(
        internal.url(),
        session::INTERNAL_RPC_TIMEOUT,
    ));
    let amm = crate::lending::connect_pools(scenario, &chain).await?;
    let pools: Vec<Pool> = amm.iter().flat_map(|a| a.pools().to_vec()).collect();
    let first = check_first_pool(&pools)?;
    let reference_start_usd = match venue.reference_usd {
        Some(price) => price,
        None => State::read(chain.as_ref(), first).await?.mid(first),
    };
    assert!(reference_start_usd.is_finite() && reference_start_usd > 0.0);
    let prices = Prices {
        sol: first.base.mint,
        usdc: first.quote.mint,
        start_usd: reference_start_usd,
        path: scenario.path.clone(),
        others: venue
            .reference_sol
            .iter()
            .map(|(mint, price)| (mint.parse().expect("checked: a mint"), *price))
            .collect(),
        other_paths: venue
            .reference_paths
            .iter()
            .map(|(mint, path)| (mint.parse().expect("checked: a mint"), path.clone()))
            .collect(),
    };
    check_pools(&pools, &scenario.mandate, &prices)?;

    let cheats = surfnet.cheatcodes();
    let (sol, usdc) = (&pools[0].base, &pools[0].quote);
    // Every token beside SOL and USDC the pools trade, once, in the pools'
    // order.
    let mut others: Vec<&crate::venue::amm::Token> = Vec::new();
    for token in pools.iter().flat_map(|p| [&p.base, &p.quote]) {
        let listed = others.iter().any(|t| t.mint == token.mint);
        if token.mint != sol.mint && token.mint != usdc.mint && !listed {
            others.push(token);
        }
    }
    // `others_sol` of each other token's worth; an account of each even
    // when it holds none, so a swap into it opens nothing.
    let fund = |owner: &Pubkey, sol_units: f64, usdc_units: f64, others_sol: f64| -> Result<()> {
        cheats.fund_sol(owner, (FEE_SOL * 1e9) as u64)?;
        for (token, units) in [(sol, sol_units), (usdc, usdc_units)] {
            let atoms = crate::lending::atoms(units, u32::from(token.asset.decimals));
            if atoms > 0 {
                crate::spot::fund_mint(&surfnet, owner, &token.mint, atoms)?;
            }
        }
        for token in &others {
            let units = others_sol / prices.in_sol(&token.mint, 1.0, reference_start_usd, 0);
            let atoms = crate::lending::atoms(units, u32::from(token.asset.decimals));
            crate::spot::fund_mint(&surfnet, owner, &token.mint, atoms)?;
        }
        Ok(())
    };
    let bot = opts.keys.keypair("bot", 0);
    fund(
        &bot.pubkey(),
        venue.wallet.get("SOL").copied().unwrap_or_default(),
        venue.wallet.get("USDC").copied().unwrap_or_default(),
        0.0,
    )?;
    // The layer's `native_sol` replaces the grant above, and Jito's tip
    // accounts exist as on mainnet.
    if let Some(layer) = &scenario.tx_layer {
        crate::txlayer::setup(&surfnet, layer, &bot.pubkey())?;
    }
    let keys = ActorKeys::fund(opts, venue, &|owner: &Pubkey| {
        fund(owner, ACTOR_SOL, ACTOR_USDC, ACTOR_SOL)?;
        cheats.fund_sol(owner, (ACTOR_NATIVE_SOL * 1e9) as u64)?;
        Ok(())
    })?;
    let tables = {
        let accounts = crate::lending::route::route_accounts(
            chain.as_ref(),
            &rpc,
            None,
            &pools,
            &bot.pubkey(),
        )
        .await?;
        let address = |i: u32| opts.keys.keypair("session-lookup-table", i).pubkey();
        crate::lending::route::write_tables(&surfnet, &address, &accounts)?
    };

    let guard = Guard::start(surfnet.rpc_url()).await?;
    let workdir = SessionDir::create(&bot, opts.slot.map(|slot| BOT_USERS[slot]))?;
    let mut slot = rpc.get_slot()?;
    let first_slot = setup_from + scenario.setup_slots();
    if slot > first_slot {
        return Err(Error::SetupSlots {
            scenario: scenario.name.clone(),
            used: slot - setup_from,
            budget: scenario.setup_slots(),
        });
    }
    while slot < first_slot {
        surfnet.send_command(SimnetCommand::SlotForward(None))?;
        slot = session::wait_for_slot(&rpc, slot + 1).await?;
    }
    if let Some(observer) = &opts.observer {
        observer.before_lock(surfnet.rpc_url(), &bot.pubkey().to_string());
    }
    cheats.lock(CheatcodeFilter::all())?;
    let base_slot = rpc.get_slot()?;
    let end_slot = base_slot + scenario.duration_slots;
    let mandate = scenario.mandate.clone().with_term(base_slot, end_slot);
    let triggers: Arc<Mutex<Vec<TriggerRecord>>> = Arc::default();
    let attempts: Arc<Mutex<Vec<SearcherAttempt>>> = Arc::default();
    let mut account = AmmAccount {
        scenario: scenario.clone(),
        chain: chain.clone(),
        internal_url: internal.url().to_string(),
        amm,
        pools: pools.clone(),
        tables: tables.clone(),
        bot: bot.pubkey(),
        prices: prices.clone(),
        reference_start_usd,
        start: None,
        last: None,
        start_value_usd: 0.0,
        peak_value_usd: 0.0,
        max_drawdown_usd: 0.0,
        inventory: Vec::new(),
        triggers: triggers.clone(),
        attempts: attempts.clone(),
    };
    let start = account.read(0).await?;
    account.start_value_usd = start.value_usd;
    account.peak_value_usd = start.value_usd;
    account.start = Some(start);
    let env = BotEnv {
        rpc_url: guard.url().to_string(),
        keypair_path: workdir.keypair_path(),
        exchange_path: PathBuf::new(),
        symbol: scenario.symbol.clone(),
        end_slot,
        max_position: scenario.max_position(),
        max_drawdown_usd: mandate
            .limits
            .max_drawdown_usd
            .unwrap_or(account.start_value_usd),
        mandate_path: Some(workdir.write_mandate(&mandate)?),
    };
    let (cast, triggers) = cast(
        venue,
        &pools,
        keys,
        &actors::Bidding {
            chain: chain.clone(),
            engine: None,
            tables,
            seed,
            log: attempts,
        },
        &prices,
        triggers,
    );
    info!(
        scenario = scenario.name,
        pools = pools.len(),
        reference_start_usd,
        "amm session set up"
    );
    Ok(Prepared {
        world: World::Snapshot(Box::new(surfnet)),
        internal,
        rpc,
        guard,
        workdir,
        bot,
        base_slot,
        end_slot,
        mandate,
        env,
        reports: None,
        reopen_slot: None,
        vars: Vec::new(),
        cast,
        roster: Default::default(),
        cranks: Vec::new(),
        account: Box::new(account),
        triggers,
    })
}

/// The keys of an `amm` session's actors, each funded.
struct ActorKeys {
    whale: Vec<Keypair>,
    flow: Vec<Keypair>,
    cycle: Vec<Keypair>,
    pool: Vec<Keypair>,
}

impl ActorKeys {
    /// A key for each of `venue`'s actors' accounts, each given what
    /// `fund` gives it.
    fn fund(
        opts: &SessionOptions<'_>,
        venue: &crate::pools::AmmVenue,
        fund: &dyn Fn(&Pubkey) -> Result<()>,
    ) -> Result<Self> {
        let keys = |role: &str, count: usize| -> Result<Vec<Keypair>> {
            let count = u32::try_from(count).expect("an actor count is small");
            (0..count)
                .map(|i| {
                    let keypair = opts.keys.keypair(role, i);
                    fund(&keypair.pubkey())?;
                    Ok(keypair)
                })
                .collect()
        };
        Ok(Self {
            whale: keys(
                "amm-whale",
                venue.whale.as_ref().map_or(0, |w| w.accounts as usize),
            )?,
            flow: keys(
                "amm-flow",
                venue.background.as_ref().map_or(0, |b| b.pools.len()),
            )?,
            cycle: keys("amm-cycle-searcher", venue.searchers.len())?,
            pool: keys("amm-pool-searcher", venue.pool_searchers.len())?,
        })
    }
}

/// `venue`'s actors on `pools`, in the order they act after a step's
/// blocks (the calm flow, the cycle searchers, the pool searchers), each
/// bidding as `bidding` says; and its whale, which lands the triggers and
/// logs them in `log`.
fn cast(
    venue: &crate::pools::AmmVenue,
    pools: &[Pool],
    keys: ActorKeys,
    bidding: &actors::Bidding,
    prices: &Prices,
    log: Arc<Mutex<Vec<TriggerRecord>>>,
) -> (Cast, Option<Box<dyn crate::trigger::Triggers>>) {
    let mut cast = Cast::default();
    let graph = cycle::Graph::new(pools, &prices.sol);
    if let Some(background) = &venue.background {
        let flows = background
            .pools
            .iter()
            .zip(keys.flow)
            .map(|(flow, keypair)| (pool_named(pools, &flow.pool), flow.clone(), keypair))
            .collect();
        cast.add(Box::new(actors::Flow {
            bidding: bidding.clone(),
            prices: prices.clone(),
            pools: flows,
            drawn_to: 0,
        }));
    }
    if !venue.searchers.is_empty() {
        cast.add(Box::new(actors::CycleSearchers {
            bidding: bidding.clone(),
            graph: graph.clone(),
            pools: pools.to_vec(),
            searchers: venue.searchers.iter().cloned().zip(keys.cycle).collect(),
            last: BTreeMap::new(),
            looks: 0,
            step: 0,
        }));
    }
    if !venue.pool_searchers.is_empty() {
        cast.add(Box::new(actors::PoolSearchers {
            bidding: bidding.clone(),
            prices: prices.clone(),
            pools: pools.to_vec(),
            searchers: venue
                .pool_searchers
                .iter()
                .cloned()
                .zip(keys.pool)
                .collect(),
            last: BTreeMap::new(),
            looks: 0,
            step: 0,
        }));
    }
    let whale = venue.whale.as_ref().map(|spec| {
        Box::new(whale::Whale {
            chain: bidding.chain.clone(),
            accounts: keys.whale,
            chunks: whale::Whale::chunks_of(spec, pools),
            pools: pools.to_vec(),
            graph,
            prices: prices.clone(),
            tables: bidding.tables.clone(),
            seed: bidding.seed,
            log,
        }) as Box<dyn crate::trigger::Triggers>
    });
    (cast, whale)
}

/// The pool `name` names, among the session's: the scenario's check found
/// it there.
fn pool_named(pools: &[Pool], name: &str) -> Pool {
    pools
        .iter()
        .find(|p| p.instrument.to_string() == name)
        .expect("checked: an actor's pool is one of the session's")
        .clone()
}

/// An `amm` session's first pool trades SOL for USDC: the two assets the
/// account and the reference price start from.
fn check_first_pool(pools: &[Pool]) -> Result<&Pool> {
    let first = pools
        .first()
        .ok_or_else(|| venue_error("amm", "no pools"))?;
    if first.base.asset.symbol != "SOL" || first.quote.asset.symbol != "USDC" {
        return Err(venue_error(
            first.instrument.to_string(),
            "an amm session's first pool trades SOL for USDC",
        ));
    }
    Ok(first)
}

/// Every pool of an `amm` session trades two tokens the session prices
/// (SOL, USDC, or one of `reference_sol`'s), and the mandate admits each
/// with them.
fn check_pools(pools: &[Pool], mandate: &Mandate, prices: &Prices) -> Result<()> {
    for pool in pools {
        let label = pool.instrument.to_string();
        for token in [&pool.base, &pool.quote] {
            if !prices.knows(&token.mint) {
                return Err(venue_error(
                    &label,
                    format!(
                        "{} has no reference price: venues.amm.reference_sol names it",
                        token.mint
                    ),
                ));
            }
        }
        let assets = [pool.base.mint.to_string(), pool.quote.mint.to_string()];
        if mandate
            .allowance_of(&pool.instrument, Some(&assets))
            .is_none()
        {
            return Err(venue_error(
                &label,
                "the mandate's universe doesn't admit the pool with its assets",
            ));
        }
    }
    Ok(())
}

/// One reading of the bot's wallet, and of the pools for its turn.
#[derive(Debug, Clone)]
struct AmmReading {
    wallet: Wallet,
    value_usd: f64,
    /// Every other token's units, by mint, where the session has any.
    others: BTreeMap<Pubkey, f64>,
    /// What the wallet holds beside SOL, in USD: rule 11's measure.
    other_usd: f64,
}

/// An `amm` session's account: the bot's wallet, at the reference price.
struct AmmAccount {
    scenario: Scenario,
    chain: Arc<dyn ChainClient>,
    /// The grader's own endpoint, for reading transactions back.
    internal_url: String,
    amm: Vec<Amm>,
    pools: Vec<Pool>,
    tables: Vec<AddressLookupTableAccount>,
    bot: Pubkey,
    prices: Prices,
    reference_start_usd: f64,
    start: Option<AmmReading>,
    last: Option<AmmReading>,
    start_value_usd: f64,
    peak_value_usd: f64,
    max_drawdown_usd: f64,
    /// What the wallet held beside SOL at each step's reading, in USD, by
    /// slots into the session: rule 11's measurement for a landed
    /// transaction whose own balances don't list the wallet's.
    inventory: Vec<(u64, Option<f64>)>,
    triggers: Arc<Mutex<Vec<TriggerRecord>>>,
    attempts: Arc<Mutex<Vec<SearcherAttempt>>>,
}

impl AmmAccount {
    fn reference_at(&self, at: u64) -> f64 {
        self.prices.sol_usd(at)
    }

    /// Whether the mandate's numeraire is the pools' base, SOL: the account
    /// is then valued in it.
    fn values_in_base(&self) -> bool {
        let base = self.pools[0].base.mint.to_string();
        self.scenario
            .mandate
            .limits
            .inventory
            .as_ref()
            .is_some_and(|inventory| inventory.numeraire == base)
    }

    /// The wallet `at` slots in: its wrapped SOL and USDC at the
    /// reference, and each pool's prices for the bot's turn.
    async fn read(&self, at: u64) -> Result<AmmReading> {
        let (sol, usdc) = (&self.pools[0].base, &self.pools[0].quote);
        let units = |atoms: u64, decimals: u8| atoms as f64 / 10f64.powi(decimals.into());
        let owned = |mint: Pubkey| {
            let chain = self.chain.clone();
            let bot = self.bot;
            async move { mtx_kit::chain::token::owned_amount(chain.as_ref(), &bot, &mint).await }
        };
        let base = units(
            owned(sol.mint)
                .await
                .map_err(|e| venue_error("wallet", e))?,
            sol.asset.decimals,
        );
        let quote = units(
            owned(usdc.mint)
                .await
                .map_err(|e| venue_error("wallet", e))?,
            usdc.asset.decimals,
        );
        let price_usd = self.reference_at(at);
        let mut others = BTreeMap::new();
        for mint in self.prices.others.keys() {
            let decimals = self
                .pools
                .iter()
                .flat_map(|p| [&p.base, &p.quote])
                .find(|t| t.mint == *mint)
                .map(|t| t.asset.decimals)
                .expect("checked: a priced token is a pool's");
            let atoms = owned(*mint).await.map_err(|e| venue_error("wallet", e))?;
            others.insert(*mint, units(atoms, decimals));
        }
        // A mandate whose numeraire is SOL measures the account in SOL: its
        // USDC and other tokens at the reference now, the sum at the
        // reference the session started at, so the reference's moves alone
        // move no PnL.
        let others_sol: f64 = others
            .iter()
            .map(|(mint, held)| self.prices.in_sol(mint, *held, price_usd, at))
            .sum();
        let others_usd: f64 = others
            .iter()
            .map(|(mint, held)| self.prices.in_usd(mint, *held, price_usd, at))
            .sum();
        let value_usd = match (self.values_in_base(), others.is_empty()) {
            (true, true) => (base + quote / price_usd) * self.reference_start_usd,
            (true, false) => (base + quote / price_usd + others_sol) * self.reference_start_usd,
            (false, true) => base * price_usd + quote,
            (false, false) => base * price_usd + quote + others_usd,
        };
        let other_usd = match others.is_empty() {
            true => quote,
            false => quote + others_usd,
        };
        let mut venues = Vec::with_capacity(self.pools.len());
        for pool in &self.pools {
            let state = State::read(self.chain.as_ref(), pool).await?;
            let (best_bid, best_ask) = state.top(pool);
            venues.push(VenueReading {
                id: pool.instrument.to_string(),
                supplied: 0.0,
                borrowed: 0.0,
                health: None,
                stats: vec![
                    ("price".to_string(), state.mid(pool)),
                    ("liquidity".to_string(), state.liquidity() as f64),
                    ("fee_bps".to_string(), state.fee_fraction() * 10_000.0),
                ],
                best_bid,
                best_ask,
            });
        }
        Ok(AmmReading {
            wallet: Wallet {
                base,
                quote,
                price_usd,
                value_usd,
                venues,
            },
            value_usd,
            others,
            other_usd,
        })
    }

    /// What the session names as the venue: each pool's keys and its
    /// tokens' decimals, the session's lookup tables (`lookup_table` is the
    /// first), SOL's reference price at the start, and every other token's
    /// in SOL whose price is fixed: one a path moves is the market's to
    /// read, not the session's to tell.
    fn metadata(&self) -> serde_json::Value {
        serde_json::json!({
            "pools": self.pools.iter().map(|p| serde_json::json!({
                "id": p.instrument.to_string(),
                "address": p.address.to_string(),
                "program": p.program.id().to_string(),
                "base_mint": p.base.mint.to_string(),
                "quote_mint": p.quote.mint.to_string(),
                "base_vault": p.base.vault.to_string(),
                "quote_vault": p.quote.vault.to_string(),
                "base_decimals": p.base.asset.decimals,
                "quote_decimals": p.quote.asset.decimals,
            })).collect::<Vec<_>>(),
            "lookup_table": self.tables[0].key.to_string(),
            "lookup_tables": self.tables.iter().map(|t| t.key.to_string()).collect::<Vec<_>>(),
            "reference_usd": self.reference_start_usd,
            "reference_sol": self.prices.others.iter()
                .filter(|(mint, _)| !self.prices.other_paths.contains_key(mint))
                .map(|(mint, price)| (mint.to_string(), *price))
                .collect::<BTreeMap<_, _>>(),
        })
    }

    /// Each of the bot's transactions that landed or failed, read back from
    /// the chain: its costs at the reference then (rule 13), what the
    /// wallet held beside SOL right after a landed one (rule 11), from its
    /// own token balances, and its fills from the pools' vaults (rule 1).
    fn observed_transactions(
        &self,
        records: &[session::TxRecord],
    ) -> Result<(Vec<ObservedTransaction>, Vec<AmmFill>)> {
        let rpc =
            mtx_kit::rpc_client_with_timeout(&self.internal_url, session::INTERNAL_RPC_TIMEOUT);
        let numeraire = self.pools[0].base.mint;
        let mut transactions = Vec::new();
        let mut fills = Vec::new();
        for record in records.iter().filter(|r| !r.duplicate) {
            let (Some(signature), Some(at)) = (&record.signature, record.at) else {
                continue;
            };
            let found = crate::chain::call(
                &rpc,
                "getTransaction",
                serde_json::json!([signature, { "encoding": "json", "commitment": "confirmed", "maxSupportedTransactionVersion": 0 }]),
            )?;
            let unreadable = || {
                venue_error(
                    "mandate",
                    format!("transaction {signature} doesn't read back"),
                )
            };
            let costs = observe::costs(&found).ok_or_else(unreadable)?;
            let changes = observe::token_changes(&found).ok_or_else(unreadable)?;
            let reference = self.reference_at(at);
            let usd = |lamports: u64| lamports as f64 / 1e9 * reference;
            let other_holdings_usd = match record.ok {
                true => self.held_after(&changes, at, &numeraire),
                false => None,
            };
            if !costs.flash_borrows.is_empty() {
                return Err(venue_error(
                    "mandate",
                    format!(
                        "transaction {signature} flash borrows, and an amm session lends nothing"
                    ),
                ));
            }
            transactions.push(ObservedTransaction {
                signature: signature.clone(),
                slot: at,
                outcome: match record.ok {
                    true => TransactionOutcome::Landed,
                    false => TransactionOutcome::Failed,
                },
                fee_usd: usd(costs.base_lamports),
                priority_fee_usd: usd(costs.priority_lamports),
                tip_usd: usd(costs.tip_lamports),
                other_holdings_usd,
                flash_borrows: Vec::new(),
            });
            if record.ok {
                fills.extend(
                    self.pools
                        .iter()
                        .filter_map(|pool| fill(pool, &changes, signature, at)),
                );
            }
        }
        Ok((transactions, fills))
    }

    /// What the wallet held beside `numeraire` right after a landed
    /// transaction, in USD: its own token balances, where they list the
    /// wallet's accounts, or the step's reading that followed it. USDC is
    /// worth a dollar, the session's other tokens their reference; a token
    /// it doesn't price can't be valued, and breaks rule 11.
    fn held_after(
        &self,
        changes: &[observe::TokenChange],
        at: u64,
        numeraire: &Pubkey,
    ) -> Option<f64> {
        let usdc = self.pools[0].quote.mint;
        let decimals = self.pools[0].quote.asset.decimals;
        let sol_usd = self.reference_at(at);
        let own: Vec<&observe::TokenChange> = changes
            .iter()
            .filter(|c| c.owner == Some(self.bot) && c.mint != *numeraire)
            .collect();
        if own.is_empty() {
            return self
                .inventory
                .iter()
                .find(|(measured, _)| *measured >= at)
                .and_then(|(_, other)| *other);
        }
        let mut held = 0.0;
        for change in own {
            if change.mint == usdc {
                held += change.post as f64 / 10f64.powi(decimals.into());
                continue;
            }
            if !self.prices.others.contains_key(&change.mint) {
                return Some(f64::NAN);
            }
            let token_decimals = self
                .pools
                .iter()
                .flat_map(|p| [&p.base, &p.quote])
                .find(|t| t.mint == change.mint)
                .map(|t| t.asset.decimals)
                .expect("checked: a priced token is a pool's");
            let units = change.post as f64 / 10f64.powi(token_decimals.into());
            held += self.prices.in_usd(&change.mint, units, sol_usd, at);
        }
        Some(held)
    }
}

/// The swap a landed transaction made through `pool`, from its vaults: the
/// base vault's change is the size, the quote vault's the price's. Vaults
/// that moved the same way, or not at all, are no swap.
fn fill(
    pool: &Pool,
    changes: &[observe::TokenChange],
    signature: &str,
    at: u64,
) -> Option<AmmFill> {
    let change = |vault: &Pubkey| -> i128 {
        changes
            .iter()
            .filter(|c| c.account == *vault)
            .map(|c| i128::from(c.post) - i128::from(c.pre))
            .sum()
    };
    let (base, quote) = (change(&pool.base.vault), change(&pool.quote.vault));
    // The pool took base: the account sold it.
    let side = match (base.signum(), quote.signum()) {
        (1, -1) => Side::Sell,
        (-1, 1) => Side::Buy,
        _ => return None,
    };
    let base_units = base.unsigned_abs() as f64 / 10f64.powi(pool.base.asset.decimals.into());
    let quote_units = quote.unsigned_abs() as f64 / 10f64.powi(pool.quote.asset.decimals.into());
    assert!(base_units > 0.0, "a swap moves the base vault");
    Some(AmmFill {
        signature: signature.to_string(),
        at,
        instrument: pool.instrument.clone(),
        side,
        base: base_units,
        price: quote_units / base_units,
    })
}

#[async_trait]
impl Account for AmmAccount {
    fn start_value_usd(&self) -> f64 {
        self.start_value_usd
    }

    fn moment(&self, at: u64) -> Moment {
        let price = self.reference_at(at);
        Moment {
            at,
            level_usd: price,
            book_usd: price,
            fair_usd: None,
        }
    }

    async fn step(&mut self, k: u64, _slot: u64, at: u64) -> Result<StepRecord> {
        let now = self.read(at).await?;
        if self.scenario.mandate.limits.inventory.is_some() {
            self.inventory.push((at, Some(now.other_usd)));
        }
        let value_usd = now.value_usd;
        self.peak_value_usd = self.peak_value_usd.max(value_usd);
        self.max_drawdown_usd = self.max_drawdown_usd.max(self.peak_value_usd - value_usd);
        let price = now.wallet.price_usd;
        let record = StepRecord {
            k,
            at,
            mark_usd: price,
            index_usd: price,
            level_usd: price,
            book_usd: None,
            best_bid_usd: None,
            best_ask_usd: None,
            impact_bps: None,
            trades: None,
            position: now.wallet.base,
            value_usd,
            pnl_usd: value_usd - self.start_value_usd,
            hold_usd: 0.0,
            turn_ms: None,
            answered: None,
            fair_usd: None,
            markets: Vec::new(),
        };
        self.last = Some(now);
        Ok(record)
    }

    async fn turn_data(&mut self, _step: &mut Step) -> Result<()> {
        Ok(())
    }

    fn wallet(&self) -> Option<&Wallet> {
        self.last.as_ref().map(|r| &r.wallet)
    }

    async fn desk(
        &self,
        host_url: &str,
        bot: &Keypair,
        env: &BotEnv,
        mandate: &Mandate,
        tx: Option<Arc<crate::txlayer::TxEngine>>,
    ) -> Result<Desk> {
        // The bot's seats read through its guard's host side, and send
        // into the layer's pool.
        let pool = tx.as_ref().map(|_| {
            Arc::new(crate::txlayer::client::PoolClient::new(
                host_url,
                bot.insecure_clone(),
            ))
        });
        let chain: Arc<dyn ChainClient> = match &pool {
            Some(pool) => pool.clone(),
            None => Arc::new(RpcChainClient::with_timeout(
                host_url,
                session::INTERNAL_RPC_TIMEOUT,
            )),
        };
        let on = SeatChain {
            chain,
            pooled: pool.is_some(),
            lookup_tables: self.tables.clone(),
        };
        let mut seats = Vec::new();
        for amm in &self.amm {
            for p in amm.pools() {
                let market = amm.market(&p.instrument).await?;
                seats.push(market.seat_on(bot.insecure_clone(), on.clone()).await?);
            }
        }
        // The pools, each named by its instrument; the mandate's
        // wildcards cover them.
        let markets: Vec<mtx_kit::VenueInfo> = self
            .pools
            .iter()
            .map(|p| mtx_kit::VenueInfo {
                id: p.instrument.to_string(),
                kind: p.instrument.kind.into(),
                address: Some(p.address.to_string()),
                tradable: mandate
                    .allowance_of(
                        &p.instrument,
                        Some(&[p.base.mint.to_string(), p.quote.mint.to_string()]),
                    )
                    .is_some(),
            })
            .collect();
        let start = self
            .start
            .as_ref()
            .map(|s| s.wallet.clone())
            .unwrap_or_default();
        Ok(Desk {
            markets,
            seats,
            symbol: env.symbol.clone(),
            venue: serde_json::to_string(&self.metadata())?,
            authority: bot.pubkey().to_string(),
            reader: Reader::Lending { start },
            rfqs: None,
            pool,
            tx,
        })
    }

    fn decode(&self, ix: &serde_json::Value, keys: &[serde_json::Value]) -> IxRecord {
        decode_instruction(ix, keys)
    }

    async fn finish(mut self: Box<Self>, done: Done) -> Result<SessionReport> {
        let end_at = done.end_slot.saturating_sub(done.start_slot);
        let end = self.read(end_at).await?;
        if self.scenario.mandate.limits.inventory.is_some() {
            self.inventory.push((end_at, Some(end.other_usd)));
        }
        let start = self.start.clone().expect("a start reading");
        let (transactions, fills) = self.observed_transactions(&done.transactions)?;
        let pools: Vec<ObservedPool> = self
            .pools
            .iter()
            .map(|p| ObservedPool {
                instrument: p.instrument.clone(),
                assets: [p.base.mint.to_string(), p.quote.mint.to_string()],
            })
            .collect();
        let instrument = self.pools[0].instrument.clone();
        let end_value_usd = end.value_usd;
        let triggers = std::mem::take(&mut *self.triggers.lock().expect("trigger log"));
        let attempts = std::mem::take(&mut *self.attempts.lock().expect("attempts log"));
        Ok(SessionReport {
            scenario: self.scenario.name.clone(),
            start_mark_usd: self.reference_start_usd,
            base_lots_decimals: 3,
            end_mark_usd: end.wallet.price_usd,
            start_value_usd: self.start_value_usd,
            end_value_usd,
            pnl_usd: end_value_usd - self.start_value_usd,
            end_position: end.wallet.base,
            liquidatable: false,
            refused_calls: done.refused_calls,
            forced_closes: done.forced_closes,
            samples: Default::default(),
            term_start: Default::default(),
            liquidator: done.liquidator,
            bot_exit: done.bot_exit,
            late_slots: done.late_slots,
            mandate: done.mandate,
            peak_position: 0.0,
            instrument,
            positions: Vec::new(),
            positions_outside_universe: 0,
            max_drawdown_usd: self
                .max_drawdown_usd
                .max(self.peak_value_usd.max(end_value_usd) - end_value_usd),
            valuation: "reference",
            flow: done.actors.flow,
            competitor: done.actors.competitor,
            actors: done.actors.library,
            oracle_starts: BTreeMap::new(),
            spot: None,
            faults: done.faults,
            order: None,
            participation: None,
            net: None,
            start_position: 0.0,
            start_funded: false,
            close_outs: Vec::new(),
            perp_fills: Vec::new(),
            perp_open_orders: Vec::new(),
            making_fills: Vec::new(),
            takes_unjudged: Vec::new(),
            obligations: None,
            lending: None,
            amm: Some(AmmReport {
                reference_start_usd: self.reference_start_usd,
                reference_end_usd: end.wallet.price_usd,
                start: start.wallet,
                end: end.wallet,
                end_others: end
                    .others
                    .iter()
                    .map(|(mint, held)| (mint.to_string(), *held))
                    .collect(),
                fills,
                transactions,
                pools,
                triggers,
                attempts,
            }),
            tx_layer: None,
            crank_refusals: done.crank_refusals,
            markets: Vec::new(),
            turns: done.turns,
            wall_ms: done.began.elapsed().as_millis() as u64,
            timeline: Timeline {
                start_slot: done.start_slot,
                end_slot: done.end_slot,
                slots_per_step: SLOTS_PER_STEP,
                start_mark_usd: self.reference_start_usd,
                steps: done.steps,
                transactions: done.transactions,
                calls: done.calls,
                taker_orders: done.actors.taker_orders,
                market_taker_orders: Default::default(),
            },
        })
    }
}

impl AmmReport {
    /// What the audit reads of it: the bot's fills, transactions and pools.
    pub fn observe(&self, observed: &mut Observed) {
        observed
            .fills
            .extend(self.fills.iter().map(|f| ObservedFill {
                instrument: f.instrument.clone(),
                side: f.side,
                price: f.price,
                worst_price: f.price,
            }));
        observed.transactions = self.transactions.clone();
        observed.pools = self.pools.clone();
    }

    /// Once the session ends, with its layer's report and what became of
    /// each send (`fate`): what landed behind each trigger in its slot and
    /// who won it, and each searcher's send's outcome.
    pub fn attribute(
        &mut self,
        layer: &TxLayerReport,
        fate: impl Fn(&str) -> Option<crate::txlayer::engine::Fate>,
    ) {
        use crate::txlayer::engine::Fate;
        for attempt in &mut self.attempts {
            attempt.outcome = Some(
                match fate(&attempt.signature) {
                    Some(Fate::Landed { ok: true, .. }) => "landed",
                    Some(Fate::Landed { ok: false, .. }) | Some(Fate::Failed(_)) => "failed",
                    Some(Fate::Expired) => "expired",
                    Some(Fate::Pending) | None => "pending",
                }
                .to_string(),
            );
        }
        let racing =
            |sender: &str| sender == crate::txlayer::engine::BOT || sender.contains("searcher/");
        for trigger in &mut self.triggers {
            // The block the session commits right after the trigger's step
            // closes the trigger's slot: the layer counts it one slot on.
            let first = trigger.at + 1;
            if let Some(block) = layer.blocks.iter().find(|b| b.at == first) {
                trigger.landed_after = block
                    .landed
                    .iter()
                    .map(|l| (l.sender.clone(), l.ok))
                    .collect();
            }
            let won = layer
                .blocks
                .iter()
                .filter(|b| b.at >= first && b.at < first + BACKRUN_WINDOW_SLOTS)
                .find_map(|b| {
                    let landed = b.landed.iter().find(|l| l.ok && racing(&l.sender))?;
                    Some((landed.sender.clone(), b.at - first))
                });
            (trigger.winner, trigger.winner_after_slots) = match won {
                Some((sender, after)) => (Some(sender), Some(after)),
                None => (None, None),
            };
        }
    }
}

/// One instruction of a bot's transaction, named by its program.
fn decode_instruction(ix: &serde_json::Value, keys: &[serde_json::Value]) -> IxRecord {
    let program = ix["programIdIndex"]
        .as_u64()
        .and_then(|i| keys.get(usize::try_from(i).ok()?))
        .and_then(|k| k.as_str())
        .unwrap_or_default();
    let named = match program {
        "whirLbMiicVdio4qvUfM5KAg6Ct8VwpYzGff3uctyCc" => "orca_whirlpool",
        "CAMMCzo5YL8w4VFF8KVHrK22GGUsp5VTaW7grrKgrWqK" => "raydium_clmm",
        "ComputeBudget111111111111111111111111111111" => "compute_budget",
        "ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL" => "associated_token",
        "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA" => "token",
        "11111111111111111111111111111111" => "system",
        other => other,
    };
    IxRecord {
        program: named.to_string(),
        name: None,
        order: None,
    }
}

#[cfg(test)]
mod tests {
    use mtx_core::Asset;

    use super::*;
    use crate::venue::amm::{Program, Token};

    fn pool() -> Pool {
        let token = |symbol: &str, decimals: u8| {
            let mint = Pubkey::new_unique();
            Token {
                mint,
                token_program: Pubkey::new_unique(),
                vault: Pubkey::new_unique(),
                asset: Asset::new(mint.to_string(), symbol.to_string(), decimals).unwrap(),
            }
        };
        Pool {
            instrument: "raydium:amm:3ucNos4NbumPLZNWztqGHNFFgkHeRMBQAVemeeomsUxv"
                .parse()
                .unwrap(),
            address: Pubkey::new_unique(),
            program: Program::RaydiumClmm,
            base: token("SOL", 9),
            quote: token("USDC", 6),
        }
    }

    fn change(account: Pubkey, pre: u64, post: u64) -> observe::TokenChange {
        observe::TokenChange {
            account,
            mint: Pubkey::new_unique(),
            owner: None,
            pre,
            post,
        }
    }

    #[test]
    fn a_fill_is_read_from_the_vaults_moving_apart() {
        let pool = pool();
        // The pool took 2 SOL and gave 140 USDC: the account sold at 70.
        let sold = [
            change(pool.base.vault, 10_000_000_000, 12_000_000_000),
            change(pool.quote.vault, 1_000_000_000, 860_000_000),
        ];
        let read = super::fill(&pool, &sold, "sig", 7).unwrap();
        assert_eq!((read.side, read.at), (Side::Sell, 7));
        assert!((read.base - 2.0).abs() < 1e-12 && (read.price - 70.0).abs() < 1e-9);
        let bought = [
            change(pool.base.vault, 12_000_000_000, 11_000_000_000),
            change(pool.quote.vault, 860_000_000, 931_000_000),
        ];
        assert_eq!(
            super::fill(&pool, &bought, "sig", 8).unwrap().side,
            Side::Buy
        );
        // Both vaults up is liquidity, not a swap; untouched vaults neither.
        let added = [
            change(pool.base.vault, 1, 2),
            change(pool.quote.vault, 1, 2),
        ];
        assert!(super::fill(&pool, &added, "sig", 9).is_none());
        assert!(super::fill(&pool, &[], "sig", 9).is_none());
    }

    #[test]
    fn the_reference_moves_with_the_path_alone() {
        let path = [crate::scenario::Waypoint::at(10, -1.0)];
        assert_eq!(reference_at(70.0, &path, 0), 70.0);
        assert!((reference_at(70.0, &path, 10) - 69.3).abs() < 1e-9);
        assert_eq!(reference_at(70.0, &[], 500), 70.0);
    }
}
