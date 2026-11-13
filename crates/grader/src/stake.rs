//! Stake-pool sessions: a bot staking SOL into liquid staking pools and
//! unstaking it, as `specs/instruments/stake-pool.md` and the venue
//! profiles (`specs/venues/jito-stake-pool.md`, `marinade.md`) define them.
//!
//! A scenario with a [`Stake`] section runs here. The session is a spot
//! session's (see [`crate::spot`]): a fresh offline world from a snapshot
//! that holds the pools, the RPC guard, the bot's turns in steps of
//! [`SLOTS_PER_STEP`] slots. What differs:
//!
//! 1. The bot's wallet starts with the scenario's native SOL: what a stake
//!    pool takes and pays. It holds a token account for each pool's token.
//! 2. Every step, the session reads what each pool holds for it: the
//!    tokens in its wallet, its SOL, and the delayed exits not yet paid (a
//!    stake account it may withdraw, a Marinade ticket). The position in
//!    each instrument is its token; the value is the tokens at their pool's
//!    rate and the SOL at the Pyth SOL/USD price, each counted once.
//! 3. The scenario's epoch boundaries ([`crate::epochs`]) move the clock a
//!    day ahead between two steps, and the pools pay the epoch's rewards.
//! 4. At the end it reads the bot's instant stakes and unstakes from the
//!    pools' own transactions, and audits them against the mandate.

use std::{path::PathBuf, sync::Arc};

use async_trait::async_trait;
use mtx_core::{
    Decimal, Holding, Holdings,
    mandate::{InstrumentRef, Kind, Mandate, Observed, PositionRange, Term},
    turns::Step,
};
use mtx_kit::{
    BotEnv,
    chain::{ChainClient, RpcChainClient, token},
    protocols::spot::MarketFill,
};
use serde::{Deserialize, Serialize};
use solana_keypair::Keypair;
use solana_pubkey::Pubkey;
use solana_rpc_client::rpc_client::RpcClient;
use solana_signer::Signer;
use surfpool_sdk::{CheatcodeFilter, SimnetCommand, Surfnet};

use crate::{
    Error, Result,
    actors::{Cast, Moment},
    epochs::{EpochEvent, EpochRecord, Epochs},
    error::io_error,
    guard::Guard,
    scenario::Scenario,
    session::{
        self, Account, BOT_USERS, Done, IxRecord, Prepared, SLOTS_PER_STEP, SessionDir,
        SessionOptions, SessionReport, StepRecord, Timeline, World, grader_f64,
    },
    snapshot,
    spot::{SpotFill, Wallet},
    venue::{Market, price_usd, stake_venue, value_usd},
    wasm::{Desk, Reader},
};

/// SOL the bot's wallet holds for fees and account rent, on top of the
/// scenario's. Its value counts: staking spends the same SOL.
const FEE_SOL: f64 = 1.0;
/// SOL the grader's crank account holds for its fees.
const CRANK_SOL: u64 = 10_000_000_000;

/// A stake-pool scenario's pools and starting wallet.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Stake {
    /// The stake pools the bot may trade, as the mandate names them
    /// (`jito-stake-pool:stake-pool:JITOSOL`, `marinade:stake-pool:MSOL`).
    /// The mandate's universe is exactly these.
    pub pools: Vec<InstrumentRef>,
    /// Native SOL the wallet starts with.
    pub sol: f64,
    /// The epoch boundaries the session crosses, in order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub epochs: Vec<EpochEvent>,
    /// What the reward divides the session's PnL by. Without it the reward
    /// is 0 and only `skill` against the reference is reported.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bound_usd: Option<f64>,
}

impl Stake {
    /// A stake-pool scenario trades known pools under a mandate whose
    /// universe is exactly them, and has none of the other kinds' sections.
    pub fn validate(&self, scenario: &Scenario) -> std::result::Result<(), String> {
        if self.pools.is_empty() {
            return Err("stake: no pools".into());
        }
        for (i, pool) in self.pools.iter().enumerate() {
            if pool.kind != Kind::StakePool {
                return Err(format!("stake: {pool} is not a stake-pool instrument"));
            }
            let offered = stake_venue(&pool.venue, "http://127.0.0.1:1")
                .map_err(|e| format!("stake: {e}"))?
                .instruments();
            if !offered.contains(pool) {
                return Err(format!("stake: {} offers no {pool}", pool.venue));
            }
            if self.pools[..i].contains(pool) {
                return Err(format!("stake: {pool} is listed twice"));
            }
        }
        if !(self.sol.is_finite() && self.sol >= 0.0) {
            return Err("stake: sol must be a number of at least 0".into());
        }
        if self.bound_usd.is_some_and(|bound| !bound.is_finite()) {
            return Err("stake: bound_usd must be a number".into());
        }
        let mut wanted = self.pools.clone();
        let mut universe: Vec<InstrumentRef> = scenario
            .mandate
            .universe
            .iter()
            .map(|a| a.instrument.clone())
            .collect();
        wanted.sort();
        universe.sort();
        if wanted != universe {
            return Err("mandate: the universe must be exactly the stake pools".into());
        }
        if scenario.mandate.term.is_some() {
            return Err("mandate: a scenario's mandate has no term; the session sets it".into());
        }
        if scenario.symbol != self.pools[0].symbol {
            return Err(format!(
                "stake: the symbol is the first pool's, {}",
                self.pools[0].symbol
            ));
        }
        if !scenario.path.is_empty()
            || scenario.making.is_some()
            || scenario.offhours.is_some()
            || scenario.funding.is_some()
            || scenario.fair_path.is_some()
            || !scenario.tape.is_empty()
            || scenario.faults.is_some()
            || scenario.turn_data.is_some()
            || scenario.order.is_some()
            || scenario.participation.is_some()
            || scenario.net.is_some()
        {
            return Err(
                "stake: a stake-pool scenario has only its pools, wallet and epochs".into(),
            );
        }
        if scenario.duration_slots == 0 {
            return Err("stake: the session has no slots".into());
        }
        crate::epochs::validate(
            &self.epochs,
            scenario.duration_slots.div_ceil(SLOTS_PER_STEP),
        )
    }
}

/// What a stake-pool session's account holds, and what it's worth.
#[derive(Debug, Clone, Default, Serialize, PartialEq)]
pub struct StakeWallet {
    /// SOL in the wallet, and SOL waiting in delayed exits.
    pub sol: f64,
    pub sol_waiting: f64,
    /// Each pool's token held, and the pool's SOL per token.
    pub tokens: Vec<Held>,
    /// The Pyth SOL/USD price it was valued at, and its value.
    pub sol_usd: f64,
    pub value_usd: f64,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Held {
    pub instrument: InstrumentRef,
    pub amount: f64,
    pub rate: f64,
}

/// A stake-pool session's account of the bot's wallet and trading.
#[derive(Debug, Clone, Default, Serialize, PartialEq)]
pub struct StakeReport {
    pub start: StakeWallet,
    pub end: StakeWallet,
    /// The bot's instant stakes and unstakes, as the pools recorded them:
    /// a stake buys the token, an unstake sells it, at SOL per token.
    pub fills: Vec<SpotFill>,
    /// The epoch boundaries the session crossed.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub epochs: Vec<EpochRecord>,
}

/// The session's pools, read through the grader's own client.
struct Pools {
    chain: Arc<dyn ChainClient>,
    owner: Pubkey,
    markets: Vec<Box<dyn Market>>,
}

impl Pools {
    async fn connect(
        chain: Arc<dyn ChainClient>,
        rpc_url: &str,
        pools: &[InstrumentRef],
        owner: Pubkey,
    ) -> Result<Self> {
        let mut markets = Vec::new();
        for pool in pools {
            markets.push(stake_venue(&pool.venue, rpc_url)?.market(pool).await?);
        }
        Ok(Self {
            chain,
            owner,
            markets,
        })
    }

    /// The account across the pools: each token, the SOL counted once,
    /// valued at the pools' rates and the SOL/USD price.
    async fn read(&self) -> Result<StakeWallet> {
        let chain = self.chain.as_ref();
        let sol_usd = price_usd(chain, &token::native_mint()).await?;
        let mut all: Vec<Holding> = Vec::new();
        let mut tokens = Vec::new();
        for market in &self.markets {
            let held = market.holdings(&self.owner).await?;
            let Some(Holding::Spot(token)) = held.first() else {
                return Err(Error::UnknownMarket(format!(
                    "{} holds no token",
                    market.instrument()
                )));
            };
            // The pool's rate: what one token is worth, in SOL.
            let one = Holding::Spot(Holdings {
                wallet: Decimal::ONE,
                ..Holdings::empty(token.base.clone())
            });
            let usd = market.value_usd(chain, &[one]).await?.unwrap_or_default();
            tokens.push(Held {
                instrument: market.instrument().clone(),
                amount: grader_f64(token.total()),
                rate: grader_f64(usd / sol_usd),
            });
            all.extend(held);
        }
        let markets: Vec<&dyn Market> = self.markets.iter().map(AsRef::as_ref).collect();
        let value = value_usd(chain, &markets, &all).await?;
        let (sol, waiting) = all
            .iter()
            .filter_map(|h| match h {
                Holding::Spot(asset) if asset.base.id.0 == token::native_mint().to_string() => {
                    Some((asset.wallet, asset.locked))
                }
                _ => None,
            })
            .fold(
                (Decimal::ZERO, Decimal::ZERO),
                |(_, waiting), (sol, locked)| (sol, waiting + locked),
            );
        Ok(StakeWallet {
            sol: grader_f64(sol),
            sol_waiting: grader_f64(waiting),
            tokens,
            sol_usd: grader_f64(sol_usd),
            value_usd: grader_f64(value),
        })
    }

    /// Each pool's token mint.
    async fn mints(&self) -> Result<Vec<Pubkey>> {
        let mut mints = Vec::new();
        for market in &self.markets {
            let held = market.holdings(&self.owner).await?;
            let Some(Holding::Spot(token)) = held.first() else {
                return Err(Error::UnknownMarket(format!(
                    "{} holds no token",
                    market.instrument()
                )));
            };
            mints.push(
                token.base.id.0.parse().map_err(|_| {
                    Error::UnknownMarket(format!("{} is not a mint", token.base.id))
                })?,
            );
        }
        Ok(mints)
    }

    /// The bot's own fills on every pool after `after_slot`, oldest first.
    async fn fills(&self, after_slot: u64, base_slot: u64) -> Result<Vec<SpotFill>> {
        let mut found: Vec<MarketFill> = Vec::new();
        for market in &self.markets {
            found.extend(
                market
                    .fills(after_slot, &self.owner)
                    .await?
                    .into_iter()
                    .map(|(fill, _)| fill),
            );
        }
        found.sort_by_key(|f| (f.slot, f.instrument.0.clone(), f.sequence));
        found.dedup_by_key(|f| (f.instrument.0.clone(), f.sequence));
        found
            .into_iter()
            .map(|fill| {
                Ok(SpotFill {
                    at: fill.slot.saturating_sub(base_slot),
                    instrument: fill.instrument.0.parse().map_err(|reason: String| {
                        Error::Transaction {
                            label: "a fill's market".into(),
                            reason,
                        }
                    })?,
                    side: fill.taker_side,
                    role: "taker",
                    price: grader_f64(fill.price),
                    base: grader_f64(fill.quantity),
                })
            })
            .collect()
    }
}

/// Set a stake-pool session up, up to the bot's start; see the module docs
/// and [`session::run`].
pub(crate) async fn prepare(opts: &SessionOptions<'_>) -> Result<Prepared> {
    let scenario = opts.scenario;
    let stake = scenario
        .stake
        .as_ref()
        .expect("a stake-pool session's scenario has a stake section");
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

    let bot = opts.keys.keypair("bot", 0);
    let pools = Pools::connect(chain.clone(), internal.url(), &stake.pools, bot.pubkey()).await?;
    fund(&surfnet, &rpc, &bot, stake.sol, &pools.mints().await?)?;
    let crank = opts.keys.keypair("crank", 0);
    surfnet.cheatcodes().fund_sol(&crank.pubkey(), CRANK_SOL)?;
    let start = pools.read().await?;

    let guard = Guard::start(surfnet.rpc_url()).await?;
    let workdir = SessionDir::create(&bot, BOT_USERS[opts.slot])?;
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
    let (epochs, added) = Epochs::schedule(&rpc, &stake.epochs, &stake.pools, slot, crank)?;
    surfnet.cheatcodes().lock(CheatcodeFilter::all())?;
    let base_slot = rpc.get_slot()?;
    let end_slot = base_slot + scenario.duration_slots + added;
    let mandate = Mandate {
        term: Some(Term {
            until_slot: end_slot,
        }),
        ..scenario.mandate.clone()
    };
    let env = BotEnv {
        rpc_url: guard.url().to_string(),
        keypair_path: workdir.keypair_path(),
        exchange_path: PathBuf::new(),
        symbol: scenario.symbol.clone(),
        end_slot,
        max_position: scenario.max_position(),
        max_drawdown_usd: mandate.limits.max_drawdown_usd.unwrap_or(start.value_usd),
        mandate_path: Some(workdir.write_mandate(&mandate)?),
    };
    let mut observed = Observed::default();
    for held in &start.tokens {
        observed.begin(&held.instrument, held.amount);
    }
    let account = StakeAccount {
        scenario: scenario.clone(),
        stake: stake.clone(),
        pools,
        epochs,
        base_slot,
        observed,
        start_value_usd: start.value_usd,
        peak_value_usd: start.value_usd,
        max_drawdown_usd: 0.0,
        start,
        last: None,
    };
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
        cast: Cast::default(),
        account: Box::new(account),
    })
}

/// Give `owner` `sol` native SOL and SOL for fees, and a token account for
/// each pool's token. Setup only.
fn fund(
    surfnet: &Surfnet,
    rpc: &RpcClient,
    owner: &Keypair,
    sol: f64,
    mints: &[Pubkey],
) -> Result<()> {
    let lamports = ((sol + FEE_SOL) * 1e9).round() as u64;
    surfnet.cheatcodes().fund_sol(&owner.pubkey(), lamports)?;
    let instructions = mints
        .iter()
        .map(|mint| token::create_ata_idempotent(&owner.pubkey(), &owner.pubkey(), mint))
        .collect();
    mtx_kit::send(rpc, owner, instructions, "token accounts")?;
    Ok(())
}

/// A stake-pool session's account: the bot's wallet across the pools.
struct StakeAccount {
    scenario: Scenario,
    stake: Stake,
    pools: Pools,
    epochs: Epochs,
    base_slot: u64,
    observed: Observed,
    start: StakeWallet,
    start_value_usd: f64,
    peak_value_usd: f64,
    max_drawdown_usd: f64,
    last: Option<(StakeWallet, Wallet)>,
}

impl StakeReport {
    /// The fills, as the mandate's audit reads them.
    pub fn observed_fills(&self) -> Vec<mtx_core::mandate::ObservedFill> {
        self.fills
            .iter()
            .map(|fill| mtx_core::mandate::ObservedFill {
                instrument: fill.instrument.clone(),
                side: fill.side,
                price: fill.price,
            })
            .collect()
    }
}

impl StakeAccount {
    fn note(&mut self, reading: &StakeWallet) {
        for held in &reading.tokens {
            self.observed.record(&held.instrument, held.amount);
        }
    }
}

/// A stake-pool reading as a component's measured wallet: the first pool's
/// token as its base, its SOL as its quote.
fn as_wallet(reading: &StakeWallet) -> Wallet {
    Wallet {
        base: reading.tokens.first().map_or(0.0, |h| h.amount),
        quote: reading.sol + reading.sol_waiting,
        price_usd: reading.sol_usd,
        value_usd: reading.value_usd,
    }
}

#[async_trait]
impl Account for StakeAccount {
    fn start_value_usd(&self) -> f64 {
        self.start_value_usd
    }

    fn moment(&self, at: u64) -> Moment {
        Moment {
            at,
            level_usd: self.start.sol_usd,
            book_usd: self.start.sol_usd,
        }
    }

    async fn warp(&mut self, surfnet: &Surfnet, rpc: &RpcClient, k: u64, slot: u64) -> Result<u64> {
        self.epochs.warp(surfnet, rpc, k, slot).await
    }

    async fn step(&mut self, k: u64, _slot: u64, at: u64) -> Result<StepRecord> {
        let now = self.pools.read().await?;
        self.note(&now);
        let value_usd = now.value_usd;
        self.peak_value_usd = self.peak_value_usd.max(value_usd);
        self.max_drawdown_usd = self.max_drawdown_usd.max(self.peak_value_usd - value_usd);
        let wallet = as_wallet(&now);
        let record = StepRecord {
            k,
            at,
            mark_usd: now.sol_usd,
            index_usd: now.sol_usd,
            level_usd: now.sol_usd,
            book_usd: None,
            best_bid_usd: None,
            best_ask_usd: None,
            position: wallet.base,
            value_usd,
            pnl_usd: value_usd - self.start_value_usd,
            turn_ms: None,
            answered: None,
            fair_usd: None,
        };
        self.last = Some((now, wallet));
        Ok(record)
    }

    async fn turn_data(&mut self, _step: &mut Step) -> Result<()> {
        Ok(())
    }

    fn wallet(&self) -> Option<&Wallet> {
        self.last.as_ref().map(|(_, wallet)| wallet)
    }

    async fn desk(
        &self,
        host_url: &str,
        bot: &Keypair,
        env: &BotEnv,
        mandate: &Mandate,
    ) -> Result<Desk> {
        let mut seats = Vec::new();
        let mut addresses = Vec::new();
        for market in &self.pools.markets {
            seats.push(market.seat(bot.insecure_clone(), host_url).await?);
            addresses.push((
                market.instrument().to_string(),
                market.address().to_string(),
            ));
        }
        let mut venues = mtx_kit::VenueInfo::of_mandate(mandate, &self.stake.pools[0], |i| {
            self.stake.pools.contains(i)
        });
        for info in &mut venues {
            info.address = addresses
                .iter()
                .find(|(id, _)| *id == info.id)
                .map(|(_, address)| address.clone());
        }
        Ok(Desk {
            markets: venues,
            seats,
            symbol: env.symbol.clone(),
            venue: serde_json::to_string(&serde_json::json!({
                "pools": addresses
                    .iter()
                    .map(|(id, address)| serde_json::json!({ "id": id, "pool": address }))
                    .collect::<Vec<_>>(),
            }))?,
            authority: bot.pubkey().to_string(),
            reader: Reader::Wallet {
                start: as_wallet(&self.start),
            },
        })
    }

    fn decode(&self, ix: &serde_json::Value, keys: &[serde_json::Value]) -> IxRecord {
        let program = ix["programIdIndex"]
            .as_u64()
            .and_then(|i| keys.get(i as usize))
            .and_then(|k| k.as_str())
            .unwrap_or_default();
        let name = if program == crate::venue::stake_pool::SPL_STAKE_POOL.to_string() {
            "spl-stake-pool"
        } else if program == crate::venue::marinade::PROGRAM.to_string() {
            "marinade"
        } else if program == "ComputeBudget111111111111111111111111111111" {
            "compute_budget"
        } else {
            program
        };
        IxRecord {
            program: name.to_string(),
            name: None,
            order: None,
        }
    }

    async fn finish(mut self: Box<Self>, done: Done) -> Result<SessionReport> {
        let scenario = self.scenario.clone();
        let base_slot = self.base_slot;
        let end = self.pools.read().await?;
        self.note(&end);
        let fills = self.pools.fills(base_slot, base_slot).await?;
        let (start_value_usd, end_value_usd) = (self.start_value_usd, end.value_usd);
        let instrument = self.stake.pools[0].clone();
        let observed = std::mem::take(&mut self.observed);
        Ok(SessionReport {
            scenario: scenario.name.clone(),
            start_mark_usd: self.start.sol_usd,
            end_mark_usd: end.sol_usd,
            start_value_usd,
            end_value_usd,
            pnl_usd: end_value_usd - start_value_usd,
            end_position: end.tokens.first().map_or(0.0, |h| h.amount),
            liquidatable: false,
            refused_calls: done.refused_calls,
            bot_exit: done.bot_exit,
            late_slots: done.late_slots,
            mandate: done.mandate,
            peak_position: observed.range(&instrument).map_or(0.0, PositionRange::peak),
            instrument,
            positions: observed.positions,
            positions_outside_universe: 0,
            max_drawdown_usd: self
                .max_drawdown_usd
                .max(self.peak_value_usd.max(end_value_usd) - end_value_usd),
            valuation: "pool-rate",
            flow: done.actors.flow,
            competitor: done.actors.competitor,
            spot: None,
            stake: Some(StakeReport {
                start: self.start.clone(),
                end,
                fills,
                epochs: self.epochs.records().to_vec(),
            }),
            faults: done.faults,
            order: None,
            participation: None,
            net: None,
            turns: done.turns,
            wall_ms: done.began.elapsed().as_millis() as u64,
            timeline: Timeline {
                start_slot: done.start_slot,
                end_slot: done.end_slot,
                slots_per_step: SLOTS_PER_STEP,
                start_mark_usd: self.start.sol_usd,
                steps: done.steps,
                transactions: done.transactions,
                calls: done.calls,
                taker_orders: done.actors.taker_orders,
            },
        })
    }
}
