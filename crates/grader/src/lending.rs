//! Lending sessions: a bot with a wallet and an obligation in Kamino's
//! lending markets, as `specs/venues/kamino.md` defines them, among
//! borrowers who may be liquidated and liquidators who compete for them.
//!
//! A scenario with a [`Lending`] section runs here. The session is the same
//! as any other (see [`crate::session`]); what differs is the world and the
//! account:
//!
//! 1. **Setup**, before the cheatcodes lock. The bot's wallet gets the
//!    scenario's assets and its obligation in each lending market, and opens
//!    any position the scenario gives it. Synthetic borrowers each deposit
//!    collateral and borrow to a health drawn from the scenario's range: a
//!    borrow at the reserve's loan-to-value limit, made while the
//!    collateral's price (and its TWAP) were briefly higher, which leaves
//!    the obligation as close to liquidation as one that borrowed before a
//!    fall. Competing liquidators get wallets of the debt asset.
//! 2. **Oracles.** Every reserve prices through Scope. The scenario's feeds
//!    move the prices it names, with their TWAPs; every other reserve the
//!    session uses gets a feed that holds its price, so no price ages out
//!    unless a scenario makes it.
//! 3. **Every step**, after the blocks: the keeper refreshes every reserve
//!    on the session's clock, where interest accrues, and checks each
//!    refresh landed on the step's slot ([`KaminoCrank`]); then the
//!    borrowers react and the liquidators act, in an order drawn from the
//!    scenario's seed ([`KaminoActors`]); then the bot's turn.
//! 4. **The account** is the bot's obligations, refreshed in the slot
//!    measured by simulations the keeper pays for, and its wallet.
//!    Holdings are the program's; prices are the scenario's path for each
//!    reserve, so a feed's failure changes what the bot can do, not what
//!    its account is worth.
//!
//! Liquidation races are settled by the actors' seeded order and delays.

use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
    sync::{Arc, Mutex},
};

use async_trait::async_trait;
use mtx_core::{
    mandate::{InstrumentRef, Kind, Mandate, Observed, PositionRange, Term},
    turns::Step,
};
use mtx_kit::{
    BotEnv,
    chain::{ChainClient, RpcChainClient},
    token::TokenAccountInfo,
};
use serde::{Deserialize, Serialize};
use solana_keypair::Keypair;
use solana_pubkey::Pubkey;
use solana_rpc_client::rpc_client::RpcClient;
use solana_rpc_client_api::filter::{Memcmp, RpcFilterType};
use solana_signer::Signer;
use solana_transaction::Instruction;
use surfpool_sdk::{CheatcodeFilter, SimnetCommand, Surfnet};
use tracing::{info, warn};

use crate::{
    Error, Result,
    actors::{Actor, ActorsReport, Cast, Moment},
    crank::{CrankOutcome, CrankStep, VenueCrank},
    error::io_error,
    guard::Guard,
    oracles::{Feed, Oracles, ScopeOracle, Source, TwapMode},
    scenario::{Scenario, SplitMix64},
    session::{
        self, Account, BOT_USERS, Done, IxRecord, Prepared, SLOTS_PER_STEP, SessionDir,
        SessionOptions, SessionReport, StepRecord, Timeline, World,
    },
    snapshot,
    spot::Wallet,
    venue::{
        Venue,
        kamino::{self, Books, Kamino, Reading, klend},
    },
    wasm::{Desk, Reader},
};

/// The venue every lending session trades.
pub const VENUE: &str = "kamino";
/// SOL each wallet the session makes holds for fees and rent.
const FEE_SOL: f64 = 1.0;
/// Most synthetic borrowers and liquidators a scenario may ask for.
const MAX_BORROWERS: u32 = 32;
const MAX_LIQUIDATORS: u32 = 5;
/// A synthetic borrower borrows this share of what the reserve's loan to
/// value allows, so rounding never refuses it.
const BORROW_SHARE: f64 = 0.99;
/// Health below which a borrower with a policy reacts.
const WARNING_HEALTH: f64 = 1.05;
/// The longest clock jump a scenario may make, in slots: thirty days.
pub const MAX_WARP_SLOTS: u64 = 30 * 24 * 3_600 * 4;

/// A lending scenario's reserves, the bot's wallet, and the other accounts
/// in its markets.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Lending {
    /// What the synthetic accounts' draws come from.
    pub seed: u64,
    /// The reserves the bot may use, by the venue's symbol
    /// (`kamino:money-market:<SYMBOL>`). The mandate's universe is exactly
    /// these.
    pub reserves: Vec<String>,
    /// What the bot's wallet starts with: each reserve's asset, in whole
    /// tokens.
    #[serde(default)]
    pub wallet: BTreeMap<String, f64>,
    /// A position the bot holds when the session starts, opened in order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub open: Vec<Opening>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub borrowers: Option<Borrowers>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub liquidators: Option<Liquidators>,
    /// What the reward divides the session's PnL by, as for spot sessions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bound_usd: Option<f64>,
}

/// One action of the bot's opening position.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub enum Opening {
    Deposit { reserve: String, amount: f64 },
    Borrow { reserve: String, amount: f64 },
}

/// Synthetic borrowers, each with collateral at one reserve and debt at
/// another of the same market.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Borrowers {
    pub count: u32,
    pub collateral: String,
    pub debt: String,
    /// The health each starts at, drawn uniformly from `[low, high]`.
    pub health: [f64; 2],
    /// The collateral each deposits, in USD, drawn log-uniformly.
    pub size_usd: [f64; 2],
    /// The share that add a fifth to their collateral once their health
    /// falls below 1.05, and the share that repay half their debt then.
    /// The rest do nothing.
    #[serde(default)]
    pub top_up_share: f64,
    #[serde(default)]
    pub repay_share: f64,
}

/// Liquidators that compete with the bot.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Liquidators {
    pub count: u32,
    /// Steps each waits once it has seen an obligation it can liquidate,
    /// drawn per liquidator from `[low, high]`: 0 acts in the step the
    /// obligation turns, before the bot's turn.
    pub delay_steps: [u64; 2],
    /// Each one's wallet of the debt asset, in USD.
    #[serde(default = "default_liquidator_usd")]
    pub wallet_usd: f64,
}

fn default_liquidator_usd() -> f64 {
    1_000_000.0
}

/// A jump of the session's clock: before step `step`'s blocks, the clock
/// moves `slots` slots ahead (and the time with it, at the slot time).
/// Interest for the whole gap accrues at the next refresh, as on mainnet.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Warp {
    pub step: u64,
    pub slots: u64,
}

/// The reserve instrument a symbol names.
pub fn instrument(symbol: &str) -> InstrumentRef {
    InstrumentRef {
        venue: VENUE.into(),
        kind: Kind::MoneyMarket,
        symbol: symbol.into(),
    }
}

fn range_ok(range: [f64; 2], min: f64) -> bool {
    range.iter().all(|v| v.is_finite()) && range[0] >= min && range[0] <= range[1]
}

impl Lending {
    pub fn instruments(&self) -> Vec<InstrumentRef> {
        self.reserves.iter().map(|s| instrument(s)).collect()
    }

    /// The section on its own: known reserves, listed once, and a wallet,
    /// opening, borrowers and liquidators that use them sensibly. What ties
    /// it to the rest of the scenario is checked there (see
    /// [`crate::scenario_v2::ScenarioV2::validate`]).
    pub fn validate(&self) -> std::result::Result<(), String> {
        if self.reserves.is_empty() {
            return Err("lending: no reserves".into());
        }
        for (i, symbol) in self.reserves.iter().enumerate() {
            if kamino::reserve_of(symbol).is_none() {
                return Err(format!(
                    "lending: {symbol:?} is neither a reserve the Kamino profile names nor an address"
                ));
            }
            if self.reserves[..i].contains(symbol) {
                return Err(format!("lending: {symbol} is listed twice"));
            }
        }
        let known = |name: &str, what: &str| {
            if self.reserves.iter().any(|r| r == name) {
                Ok(())
            } else {
                Err(format!("lending: {what} {name} isn't one of the reserves"))
            }
        };
        for (symbol, amount) in &self.wallet {
            known(symbol, "the wallet's")?;
            if !(amount.is_finite() && *amount >= 0.0) {
                return Err(format!("lending: the wallet's {symbol} must be at least 0"));
            }
        }
        for opening in &self.open {
            let (Opening::Deposit { reserve, amount } | Opening::Borrow { reserve, amount }) =
                opening;
            known(reserve, "the opening's")?;
            if !(amount.is_finite() && *amount > 0.0) {
                return Err("lending: an opening's amount must be positive".into());
            }
        }
        if let Some(b) = &self.borrowers {
            known(&b.collateral, "the borrowers' collateral")?;
            known(&b.debt, "the borrowers' debt")?;
            if b.collateral == b.debt {
                return Err(
                    "lending: borrowers borrow another reserve than their collateral".into(),
                );
            }
            if b.count == 0 || b.count > MAX_BORROWERS {
                return Err(format!("lending: borrowers.count is 1 to {MAX_BORROWERS}"));
            }
            if !range_ok(b.health, 1.0) || b.health[1] > 2.0 {
                return Err("lending: borrowers.health is a range within [1, 2]".into());
            }
            if !range_ok(b.size_usd, f64::MIN_POSITIVE) {
                return Err("lending: borrowers.size_usd is a positive range".into());
            }
            let shares = [b.top_up_share, b.repay_share];
            if shares.iter().any(|s| !(0.0..=1.0).contains(s)) || shares.iter().sum::<f64>() > 1.0 {
                return Err(
                    "lending: borrower policy shares are within [0, 1] and sum to at most 1".into(),
                );
            }
        }
        if let Some(l) = &self.liquidators {
            if l.count == 0 || l.count > MAX_LIQUIDATORS {
                return Err(format!(
                    "lending: liquidators.count is 1 to {MAX_LIQUIDATORS}"
                ));
            }
            if l.delay_steps[0] > l.delay_steps[1] {
                return Err("lending: liquidators.delay_steps is a range".into());
            }
            if !(l.wallet_usd.is_finite() && l.wallet_usd > 0.0) {
                return Err("lending: liquidators.wallet_usd must be positive".into());
            }
            if self.borrowers.is_none() {
                return Err(
                    "lending: liquidators need borrowers to name the debt they repay".into(),
                );
            }
        }
        if self.bound_usd.is_some_and(|b| !(b.is_finite() && b > 0.0)) {
            return Err("lending: bound_usd must be positive".into());
        }
        Ok(())
    }
}

/// A scenario's clock jumps: inside the session's `duration_slots`, after
/// its first step, in order, and each a positive jump of at most
/// [`MAX_WARP_SLOTS`].
pub fn validate_warps(warps: &[Warp], duration_slots: u64) -> std::result::Result<(), String> {
    let steps = duration_slots.div_ceil(SLOTS_PER_STEP);
    for (i, warp) in warps.iter().enumerate() {
        if warp.step == 0 || warp.step >= steps {
            return Err(format!("warps: step {} is outside 1..{steps}", warp.step));
        }
        if warp.slots == 0 || warp.slots > MAX_WARP_SLOTS {
            return Err(format!("warps: a jump is 1 to {MAX_WARP_SLOTS} slots"));
        }
        if i > 0 && warps[i - 1].step >= warp.step {
            return Err("warps: steps increase".into());
        }
    }
    Ok(())
}

/// What a lending session reports beyond the common fields.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct LendingReport {
    /// The bot's supply and debt at each reserve, at the start and the end.
    pub start: Vec<ReserveHolding>,
    pub end: Vec<ReserveHolding>,
    /// The lowest health any of its obligations had at a step.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min_health: Option<f64>,
    /// Liquidations of the bot's own obligations, by anyone.
    pub liquidated: u32,
    /// The bot's liquidations that landed.
    pub liquidations: u32,
    /// The synthetic borrowers, as they started.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub borrowers: Vec<BorrowerRecord>,
    /// What the actors did: borrowers' reactions and the liquidators'
    /// liquidations, in the order they happened.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub actions: Vec<ActorAction>,
}

/// One reserve of the account.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ReserveHolding {
    pub instrument: InstrumentRef,
    pub supplied: f64,
    pub borrowed: f64,
    /// The scenario's price for the asset there, and the wallet's holding.
    pub price_usd: f64,
    pub wallet: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BorrowerRecord {
    pub obligation: String,
    pub collateral_usd: f64,
    pub health: f64,
    #[serde(deserialize_with = "session::static_name")]
    pub policy: session::StaticName,
}

/// Something an actor did.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ActorAction {
    pub step: u64,
    /// `kamino-liquidator/0`, `kamino-borrower/3`.
    pub by: String,
    /// `liquidate`, `top-up`, `repay`.
    #[serde(deserialize_with = "session::static_name")]
    pub action: session::StaticName,
    pub obligation: String,
    /// Units of the asset repaid or deposited.
    pub amount: f64,
    /// Why it failed, if it did.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

type ActionLog = Arc<Mutex<Vec<ActorAction>>>;

fn venue_error(label: impl Into<String>, reason: impl ToString) -> Error {
    Error::Transaction {
        label: label.into(),
        reason: reason.to_string(),
    }
}

/// Send `instructions` as `signer` and wait for them to land: setup's and
/// the actors' own transactions, through the grader's endpoint.
async fn land(
    chain: &dyn ChainClient,
    books: &Books,
    market: &Pubkey,
    signer: &Keypair,
    label: &str,
    instructions: Vec<Instruction>,
) -> std::result::Result<(), String> {
    let signature = kamino::send(chain, books, market, signer, label, instructions).await?;
    for _ in 0..200 {
        match chain.get_transaction(&signature).await {
            Ok(Some(summary)) => {
                return match summary.error {
                    None => Ok(()),
                    Some(error) => Err(format!("{label}: {error}")),
                };
            }
            Ok(None) => tokio::time::sleep(std::time::Duration::from_millis(25)).await,
            Err(error) => return Err(format!("{label}: {error}")),
        }
    }
    Err(format!("{label}: never landed"))
}

/// The Scope `OraclePrices` layout (see [`crate::oracles`]): an entry's
/// value, exponent, slot and time.
const SCOPE_ENTRIES_AT: usize = 8 + 32;
const SCOPE_ENTRY_LEN: usize = 56;

fn scope_entry(data: &[u8], index: u16) -> Option<(u64, u64)> {
    let at = SCOPE_ENTRIES_AT + index as usize * SCOPE_ENTRY_LEN;
    let entry = data.get(at..at + 16)?;
    Some((
        u64::from_le_bytes(entry[..8].try_into().ok()?),
        u64::from_le_bytes(entry[8..16].try_into().ok()?),
    ))
}

/// Rewrite Scope entries in place, before the lock: each to `scale` times
/// its value (1 leaves it), all stamped `slot` and `unix`.
fn write_scope(
    surfnet: &Surfnet,
    rpc: &RpcClient,
    account: &Pubkey,
    entries: &[(u16, f64)],
    slot: u64,
    unix: i64,
) -> Result<()> {
    let found = rpc.get_account(account)?;
    let mut data = found.data.clone();
    for &(index, scale) in entries {
        let at = SCOPE_ENTRIES_AT + index as usize * SCOPE_ENTRY_LEN;
        let Some(entry) = data.get_mut(at..at + 32) else {
            return Err(venue_error(
                "scope",
                format!("no entry {index} in {account}"),
            ));
        };
        if scale != 1.0 {
            let value = u64::from_le_bytes(entry[..8].try_into().expect("8"));
            let scaled = (value as f64 * scale).round() as u64;
            entry[..8].copy_from_slice(&scaled.to_le_bytes());
        }
        entry[16..24].copy_from_slice(&slot.to_le_bytes());
        entry[24..32].copy_from_slice(&(unix as u64).to_le_bytes());
    }
    surfnet
        .cheatcodes()
        .set_account(account, found.lamports, &data, &found.owner)?;
    Ok(())
}

/// The chain's clock: its slot and Unix time.
fn clock(rpc: &RpcClient) -> Result<(u64, i64)> {
    let data = rpc
        .get_account(&solana_pubkey::pubkey!(
            "SysvarC1ock11111111111111111111111111111111"
        ))?
        .data;
    let read = |at: usize| -> Result<[u8; 8]> {
        data.get(at..at + 8)
            .and_then(|b| b.try_into().ok())
            .ok_or_else(|| venue_error("clock", "the clock sysvar is short"))
    };
    Ok((u64::from_le_bytes(read(0)?), i64::from_le_bytes(read(32)?)))
}

/// Jump the stepped surfnet's clock `slots` slots ahead, time with it, and
/// wait for the grader's endpoint to see it. Bypasses the cheatcode lock:
/// it is the engine's clock, not a cheat.
pub(crate) async fn warp(surfnet: &Surfnet, rpc: &RpcClient, slots: u64) -> Result<u64> {
    let data = rpc
        .get_account(&solana_pubkey::pubkey!(
            "SysvarC1ock11111111111111111111111111111111"
        ))?
        .data;
    let field = |at: usize| u64::from_le_bytes(data[at..at + 8].try_into().expect("8 bytes"));
    if data.len() < 40 {
        return Err(venue_error("warp", "the clock sysvar is short"));
    }
    let to = field(0) + slots;
    let clock = solana_clock::Clock {
        slot: to,
        epoch_start_timestamp: field(8) as i64,
        epoch: field(16),
        leader_schedule_epoch: field(24),
        unix_timestamp: field(32) as i64 + (slots * snapshot::SLOT_TIME_MS / 1000) as i64,
    };
    surfnet.send_command(SimnetCommand::UpdateInternalClock(None, clock))?;
    session::wait_for_slot(rpc, to).await
}

/// The gaps a scenario's warps leave in its session's slots, `(from, to)`,
/// and the slots they add.
pub fn gaps(warps: &[Warp]) -> (Vec<(u64, u64)>, u64) {
    let mut gaps = Vec::new();
    let mut added = 0;
    for w in warps {
        let from = w.step * SLOTS_PER_STEP + added;
        gaps.push((from, from + w.slots));
        added += w.slots;
    }
    (gaps, added)
}

/// A whole token amount in atoms.
fn atoms(units: f64, decimals: u32) -> u64 {
    (units * 10f64.powi(decimals as i32)).floor().max(0.0) as u64
}

/// One account's plain obligation in `market`, made (see
/// [`kamino::obligation_setup`]).
async fn open_obligation(
    chain: &dyn ChainClient,
    books: &Books,
    market: &Pubkey,
    owner: &Keypair,
) -> Result<()> {
    let instructions = kamino::obligation_setup(chain, books, market, &owner.pubkey()).await?;
    land(chain, books, market, owner, "open obligation", instructions)
        .await
        .map_err(|e| venue_error("open obligation", e))
}

/// The refresh and the action, for an obligation's own lending action.
async fn act(
    chain: &dyn ChainClient,
    books: &Books,
    owner: &Keypair,
    reserve: &kamino::Book,
    label: &str,
    action: Instruction,
) -> std::result::Result<(), String> {
    let market = reserve.keys.market;
    let own = klend::obligation(&owner.pubkey(), &market);
    let stored = chain
        .get_account(&own)
        .await
        .map_err(|e| e.to_string())?
        .and_then(|a| klend::Obligation::decode(&a.data));
    let touched = stored
        .as_ref()
        .map(klend::Obligation::reserves)
        .unwrap_or_default();
    let mut refresh: Vec<Pubkey> = touched.clone();
    if !refresh.contains(&reserve.keys.address) {
        refresh.push(reserve.keys.address);
    }
    let mut instructions = Vec::new();
    if stored.is_none() {
        instructions.extend(
            kamino::obligation_setup(chain, books, &market, &owner.pubkey())
                .await
                .map_err(|e| e.to_string())?,
        );
    }
    for address in &refresh {
        let book = books
            .at(address)
            .ok_or_else(|| format!("{address} isn't a session reserve"))?;
        instructions.push(klend::refresh_reserve(&book.keys));
    }
    instructions.push(klend::refresh_obligation(&market, &own, &touched));
    instructions.push(action);
    land(chain, books, &market, owner, label, instructions).await
}

/// Set a lending session up, up to the bot's start; see the module docs.
pub(crate) async fn prepare(opts: &SessionOptions<'_>) -> Result<Prepared> {
    let scenario = opts.scenario;
    let lending = scenario
        .lending
        .as_ref()
        .expect("a lending session's scenario has a lending section");
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
    let instruments = lending.instruments();
    let venue = Kamino::connect(internal.url(), &instruments).await?;
    let books = venue.books().clone();

    // A reserve stamped after the clock can't be refreshed (klend's
    // `MathOverflow`): a snapshot whose accounts were fetched out of order.
    let (now_slot, now_unix) = clock(&rpc)?;
    let mut stored = BTreeMap::new();
    for book in &books.reserves {
        let reserve = rpc
            .get_account(&book.keys.address)
            .ok()
            .and_then(|a| klend::Reserve::decode(&a.data))
            .ok_or_else(|| venue_error(book.instrument.to_string(), "not a reserve"))?;
        if reserve.last_update_slot > now_slot {
            return Err(venue_error(
                book.instrument.to_string(),
                format!(
                    "refreshed at slot {}, after the snapshot's clock ({now_slot}): rebuild the snapshot",
                    reserve.last_update_slot
                ),
            ));
        }
        stored.insert(book.keys.address, reserve);
    }
    // Every price the session's reserves read, fresh as of the start.
    let mut scope: BTreeMap<Pubkey, BTreeSet<u16>> = BTreeMap::new();
    for reserve in stored.values() {
        let entries = scope.entry(reserve.scope_prices).or_default();
        for i in reserve.price_chain.iter().chain(&reserve.twap_chain) {
            if *i != klend::NO_ENTRY {
                entries.insert(*i);
            }
        }
    }
    for (account, entries) in &scope {
        let entries: Vec<(u16, f64)> = entries.iter().map(|i| (*i, 1.0)).collect();
        write_scope(&surfnet, &rpc, account, &entries, now_slot, now_unix)?;
    }

    let cheats = surfnet.cheatcodes();
    let fund = |owner: &Pubkey, book: &kamino::Book, units: f64| -> Result<()> {
        let atoms = atoms(units, book.decimals);
        if atoms > 0 {
            crate::spot::fund_mint(&surfnet, owner, &book.keys.mint, atoms)?;
        }
        Ok(())
    };
    let book = |symbol: &str| -> Result<&kamino::Book> {
        books
            .get(&instrument(symbol).to_string())
            .ok_or_else(|| venue_error("lending", format!("no reserve {symbol}")))
    };

    let bot = opts.keys.keypair("bot", 0);
    cheats.fund_sol(&bot.pubkey(), (FEE_SOL * 1e9) as u64)?;
    for (symbol, units) in &lending.wallet {
        fund(&bot.pubkey(), book(symbol)?, *units)?;
    }
    for market in books.markets() {
        open_obligation(chain.as_ref(), &books, &market, &bot).await?;
    }
    for opening in &lending.open {
        let (label, reserve, ix) = match opening {
            Opening::Deposit { reserve, amount } => {
                let b = book(reserve)?;
                (
                    "open: deposit",
                    b,
                    klend::deposit(&bot.pubkey(), &b.keys, atoms(*amount, b.decimals)),
                )
            }
            Opening::Borrow { reserve, amount } => {
                let b = book(reserve)?;
                (
                    "open: borrow",
                    b,
                    klend::borrow(&bot.pubkey(), &b.keys, atoms(*amount, b.decimals)),
                )
            }
        };
        act(chain.as_ref(), &books, &bot, reserve, label, ix)
            .await
            .map_err(|e| venue_error(label, e))?;
    }

    let log: ActionLog = Arc::default();
    let mut borrowers = Vec::new();
    let mut records = Vec::new();
    if let Some(spec) = &lending.borrowers {
        let collateral = book(&spec.collateral)?;
        let debt = book(&spec.debt)?;
        if collateral.keys.market != debt.keys.market {
            return Err(venue_error(
                "borrowers",
                "the collateral and the debt are in different lending markets",
            ));
        }
        let c = &stored[&collateral.keys.address];
        let d = &stored[&debt.keys.address];
        for i in 0..spec.count {
            let mut rng = SplitMix64(lending.seed ^ 0x6b61_6d69_6e6f ^ (u64::from(i) << 20));
            rng.next_u64();
            let health = rng.range_f64((spec.health[0], spec.health[1]));
            let size_usd = rng.log_range_f64((spec.size_usd[0], spec.size_usd[1]));
            let draw = rng.next_f64();
            let policy = if draw < spec.top_up_share {
                Policy::TopUp
            } else if draw < spec.top_up_share + spec.repay_share {
                Policy::Repay
            } else {
                Policy::Hold
            };
            let keypair = opts.keys.keypair("kamino-borrower", i);
            let key = keypair.pubkey();
            let units = size_usd / c.price;
            // Twice what it deposits: the rest tops it up later.
            cheats.fund_sol(&key, (FEE_SOL * 1e9) as u64)?;
            fund(&key, collateral, units * 1.25)?;
            open_obligation(chain.as_ref(), &books, &collateral.keys.market, &keypair).await?;
            act(
                chain.as_ref(),
                &books,
                &keypair,
                collateral,
                "borrower deposit",
                klend::deposit(&key, &collateral.keys, atoms(units, collateral.decimals)),
            )
            .await
            .map_err(|e| venue_error("borrower deposit", e))?;
            // At `bump` times the price, a borrow at the loan-to-value limit
            // leaves the obligation at `health` once the price is back.
            let liq = f64::from(c.liquidation_threshold_pct) / 100.0;
            let ltv = f64::from(c.loan_to_value_pct) / 100.0;
            if ltv <= 0.0 {
                return Err(venue_error(
                    "borrowers",
                    format!("{} can't be borrowed against", spec.collateral),
                ));
            }
            let bump = liq / (BORROW_SHARE * ltv * health);
            let borrowed_usd =
                BORROW_SHARE * size_usd * bump * ltv / (d.borrow_factor_pct as f64 / 100.0);
            let debt_units = borrowed_usd / d.price;
            let entries: Vec<(u16, f64)> = [c.price_chain[0], c.twap_chain[0]]
                .into_iter()
                .filter(|e| *e != klend::NO_ENTRY)
                .map(|e| (e, bump))
                .collect();
            write_scope(
                &surfnet,
                &rpc,
                &c.scope_prices,
                &entries,
                now_slot,
                now_unix,
            )?;
            let borrowed = act(
                chain.as_ref(),
                &books,
                &keypair,
                debt,
                "borrower borrow",
                klend::borrow(&key, &debt.keys, atoms(debt_units, debt.decimals)),
            )
            .await;
            let back: Vec<(u16, f64)> = entries.iter().map(|(e, _)| (*e, 1.0 / bump)).collect();
            write_scope(&surfnet, &rpc, &c.scope_prices, &back, now_slot, now_unix)?;
            borrowed.map_err(|e| venue_error("borrower borrow", e))?;
            // What it repays with, if its policy is to.
            fund(&key, debt, debt_units * 0.5)?;
            let obligation = klend::obligation(&key, &collateral.keys.market);
            records.push(BorrowerRecord {
                obligation: obligation.to_string(),
                collateral_usd: size_usd,
                health,
                policy: policy.name(),
            });
            borrowers.push(Borrower {
                name: format!("kamino-borrower/{i}"),
                keypair,
                collateral: collateral.clone(),
                debt: debt.clone(),
                top_up: units * 0.2,
                policy,
                done: false,
            });
        }
    }
    let mut liquidators = Vec::new();
    if let (Some(spec), Some(b)) = (&lending.liquidators, &lending.borrowers) {
        let debt = book(&b.debt)?;
        let d = &stored[&debt.keys.address];
        let mut rng = SplitMix64(lending.seed ^ 0x6c69_7175_6964);
        rng.next_u64();
        for i in 0..spec.count {
            let keypair = opts.keys.keypair("kamino-liquidator", i);
            cheats.fund_sol(&keypair.pubkey(), (FEE_SOL * 1e9) as u64)?;
            fund(&keypair.pubkey(), debt, spec.wallet_usd / d.price)?;
            let span = spec.delay_steps[1] - spec.delay_steps[0] + 1;
            let delay = spec.delay_steps[0] + rng.next_u64() % span;
            liquidators.push(Liquidator {
                name: format!("kamino-liquidator/{i}"),
                keypair,
                delay,
                seen: BTreeMap::new(),
                // A key for the race's order, drawn per liquidator.
                order: rng.next_u64(),
            });
        }
        liquidators.sort_by_key(|l| l.order);
    }

    // The scenario's feeds, completed from the reserves they write, and a
    // feed that holds each other reserve's price.
    let (gaps, warped) = gaps(&scenario.warps);
    let mut oracles = scenario.oracles.clone().unwrap_or(Oracles {
        seed: lending.seed,
        feeds: Vec::new(),
    });
    for reserve in stored.values() {
        let head = reserve.price_chain[0];
        let account = reserve.scope_prices.to_string();
        let found = oracles.feeds.iter_mut().position(
            |f| matches!(&f.source, Source::Scope(s) if s.account == account && s.index == head),
        );
        let index = match found {
            Some(i) => i,
            None => {
                oracles.feeds.push(Feed {
                    name: format!("kamino {head}"),
                    anchor: None,
                    source: Source::Scope(ScopeOracle {
                        account: account.clone(),
                        index: head,
                        exp: scope_entry(&rpc.get_account(&reserve.scope_prices)?.data, head)
                            .map_or(8, |(_, exp)| exp),
                        twap_index: None,
                        chain: Vec::new(),
                        twap: TwapMode::Hold,
                    }),
                    cadence_slots: crate::oracles::DEFAULT_CADENCE_SLOTS,
                    path: Vec::new(),
                    conf_bps: 0.0,
                    failures: Vec::new(),
                });
                oracles.feeds.len() - 1
            }
        };
        if let Source::Scope(s) = &mut oracles.feeds[index].source {
            if s.twap_index.is_none() && reserve.twap_chain[0] != klend::NO_ENTRY {
                s.twap_index = Some(reserve.twap_chain[0]);
            }
            if s.chain.is_empty() {
                s.chain = reserve.price_chain[1..]
                    .iter()
                    .copied()
                    .filter(|e| *e != klend::NO_ENTRY)
                    .collect();
            }
        }
    }
    oracles
        .validate(scenario.duration_slots + warped)
        .map_err(|reason| venue_error("oracles", reason))?;

    let keeper = opts.keys.keypair("kamino-keeper", 0);
    surfnet
        .cheatcodes()
        .fund_sol(&keeper.pubkey(), (FEE_SOL * 1e9) as u64)?;

    // The account as the session starts, priced where each feed starts.
    let start_prices: BTreeMap<Pubkey, f64> = {
        let mut prices = BTreeMap::new();
        for market in books.markets() {
            let reading = kamino::read(
                chain.as_ref(),
                &books,
                &bot.pubkey(),
                &market,
                &keeper.pubkey(),
            )
            .await?;
            for (address, reserve) in reading.reserves {
                prices.insert(address, reserve.price);
            }
        }
        prices
    };
    let feeds: BTreeMap<Pubkey, Feed> = stored
        .iter()
        .filter_map(|(address, reserve)| {
            let account = reserve.scope_prices.to_string();
            oracles
                .feeds
                .iter()
                .find(|f| {
                    matches!(&f.source, Source::Scope(s) if s.account == account && s.index == reserve.price_chain[0])
                })
                .map(|f| (*address, f.clone()))
        })
        .collect();

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
    crate::oracles::register_around(
        &rpc,
        &oracles,
        scenario.duration_slots + warped,
        &crate::oracles::World::default(),
        &gaps,
    )?;
    surfnet.cheatcodes().lock(CheatcodeFilter::all())?;
    let base_slot = rpc.get_slot()?;
    let end_slot = base_slot + scenario.duration_slots + warped;
    let mandate = Mandate {
        term: Some(Term {
            until_slot: end_slot,
        }),
        ..scenario.mandate.clone()
    };
    let mut account = LendingAccount {
        scenario: scenario.clone(),
        chain: chain.clone(),
        books: books.clone(),
        venue,
        bot: bot.pubkey(),
        keeper: keeper.pubkey(),
        instruments: instruments.clone(),
        start_prices,
        feeds,
        observed: Observed::default(),
        start: None,
        last: None,
        start_value_usd: 0.0,
        peak_value_usd: 0.0,
        max_drawdown_usd: 0.0,
        min_health: None,
        liquidated: 0,
        last_debt: BTreeMap::new(),
        log: log.clone(),
        borrowers: records,
    };
    let start = account.read(0).await?;
    account.start_value_usd = start.value_usd;
    account.peak_value_usd = start.value_usd;
    for (instrument, holding) in instruments.iter().zip(&start.holdings) {
        account
            .observed
            .begin(instrument, holding.supplied - holding.borrowed);
    }
    account.last_debt = start.debt.clone();
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
    let mut cast = Cast::default();
    if !borrowers.is_empty() || !liquidators.is_empty() {
        cast.add(Box::new(KaminoActors {
            chain: chain.clone(),
            books: books.clone(),
            keeper: keeper.pubkey(),
            borrowers,
            liquidators,
            step: 0,
            log,
        }));
    }
    info!(
        scenario = scenario.name,
        reserves = books.reserves.len(),
        "lending session set up"
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
        cast,
        cranks: vec![Box::new(KaminoCrank { books, keeper })],
        account: Box::new(account),
    })
}

/// A synthetic borrower's reaction to a falling health.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Policy {
    Hold,
    TopUp,
    Repay,
}

impl Policy {
    fn name(self) -> &'static str {
        match self {
            Policy::Hold => "hold",
            Policy::TopUp => "top-up",
            Policy::Repay => "repay",
        }
    }
}

struct Borrower {
    name: String,
    keypair: Keypair,
    collateral: kamino::Book,
    debt: kamino::Book,
    /// Units of collateral a top-up adds.
    top_up: f64,
    policy: Policy,
    /// It reacts once.
    done: bool,
}

struct Liquidator {
    name: String,
    keypair: Keypair,
    delay: u64,
    /// The step it first saw each obligation liquidatable.
    seen: BTreeMap<Pubkey, u64>,
    order: u64,
}

/// The session's borrowers and competing liquidators, acting after the
/// blocks: borrowers first, then each liquidator in its drawn order.
struct KaminoActors {
    chain: Arc<dyn ChainClient>,
    books: Arc<Books>,
    /// Pays for the refreshes the actors simulate to read obligations.
    keeper: Pubkey,
    borrowers: Vec<Borrower>,
    liquidators: Vec<Liquidator>,
    step: u64,
    log: ActionLog,
}

impl KaminoActors {
    fn record(
        &self,
        by: &str,
        action: &'static str,
        obligation: &Pubkey,
        amount: f64,
        result: std::result::Result<(), String>,
    ) {
        if let Err(error) = &result {
            warn!(by, action, %error, "a lending actor's transaction failed");
        }
        self.log.lock().expect("log").push(ActorAction {
            step: self.step,
            by: by.into(),
            action,
            obligation: obligation.to_string(),
            amount,
            error: result.err(),
        });
    }

    /// Every obligation in the session's markets, by address.
    async fn obligations(&self) -> Vec<(Pubkey, Pubkey)> {
        let mut all = Vec::new();
        for market in self.books.markets() {
            let filters = vec![
                RpcFilterType::DataSize(klend::OBLIGATION_LEN as u64),
                RpcFilterType::Memcmp(Memcmp::new_raw_bytes(
                    klend::OBLIGATION_MARKET_AT,
                    market.to_bytes().to_vec(),
                )),
            ];
            match self
                .chain
                .get_program_accounts(&klend::PROGRAM_ID, filters)
                .await
            {
                Ok(found) => all.extend(found.into_iter().map(|(key, _)| (key, market))),
                Err(error) => warn!(%error, "couldn't list the market's obligations"),
            }
        }
        all.sort();
        all
    }

    async fn borrowers_act(&mut self) {
        for i in 0..self.borrowers.len() {
            let b = &self.borrowers[i];
            if b.done || b.policy == Policy::Hold {
                continue;
            }
            let key = b.keypair.pubkey();
            let market = b.collateral.keys.market;
            let Ok(reading) = kamino::read(
                self.chain.as_ref(),
                &self.books,
                &key,
                &market,
                &self.keeper,
            )
            .await
            else {
                continue;
            };
            let Some(health) = reading
                .obligation
                .as_ref()
                .and_then(klend::Obligation::health)
            else {
                continue;
            };
            if health >= WARNING_HEALTH {
                continue;
            }
            let obligation = klend::obligation(&key, &market);
            let (action, amount, result) = match b.policy {
                Policy::TopUp => {
                    let ix = klend::deposit(
                        &key,
                        &b.collateral.keys,
                        atoms(b.top_up, b.collateral.decimals),
                    );
                    let r = act(
                        self.chain.as_ref(),
                        &self.books,
                        &b.keypair,
                        &b.collateral,
                        "top up",
                        ix,
                    )
                    .await;
                    ("top-up", b.top_up, r)
                }
                Policy::Repay => {
                    let (_, owed) = reading.balance(&b.debt);
                    let half = owed / 2.0;
                    let ix = klend::repay(&key, &b.debt.keys, atoms(half, b.debt.decimals));
                    let r = act(
                        self.chain.as_ref(),
                        &self.books,
                        &b.keypair,
                        &b.debt,
                        "repay",
                        ix,
                    )
                    .await;
                    ("repay", half, r)
                }
                Policy::Hold => continue,
            };
            let name = b.name.clone();
            self.borrowers[i].done = true;
            self.record(&name, action, &obligation, amount, result);
        }
    }

    async fn liquidators_act(&mut self) {
        let obligations = self.obligations().await;
        let mut readings: BTreeMap<Pubkey, Reading> = BTreeMap::new();
        for l in 0..self.liquidators.len() {
            let payer = self.liquidators[l].keypair.pubkey();
            for (address, market) in &obligations {
                if !readings.contains_key(address) {
                    match kamino::read_obligation(
                        self.chain.as_ref(),
                        &self.books,
                        address,
                        market,
                        &self.keeper,
                    )
                    .await
                    {
                        Ok(reading) => {
                            readings.insert(*address, reading);
                        }
                        Err(error) => {
                            warn!(%address, %error, "couldn't read an obligation");
                            continue;
                        }
                    }
                }
                let reading = &readings[address];
                let Some(obligation) = &reading.obligation else {
                    continue;
                };
                if obligation.health().is_none_or(|h| h >= 1.0) {
                    self.liquidators[l].seen.remove(address);
                    continue;
                }
                let seen = *self.liquidators[l]
                    .seen
                    .entry(*address)
                    .or_insert(self.step);
                if self.step < seen + self.liquidators[l].delay {
                    continue;
                }
                // The largest debt and the largest deposit the session knows.
                let repay = obligation
                    .borrows
                    .iter()
                    .filter_map(|b| self.books.at(&b.reserve).map(|book| (b.market_value, book)))
                    .max_by(|a, b| a.0.total_cmp(&b.0))
                    .map(|(_, book)| book.clone());
                let withdraw = obligation
                    .deposits
                    .iter()
                    .filter_map(|d| self.books.at(&d.reserve).map(|book| (d.market_value, book)))
                    .max_by(|a, b| a.0.total_cmp(&b.0))
                    .map(|(_, book)| book.clone());
                let (Some(repay), Some(withdraw)) = (repay, withdraw) else {
                    continue;
                };
                let close_factor = self
                    .chain
                    .get_account(market)
                    .await
                    .ok()
                    .flatten()
                    .and_then(|a| klend::LendingMarket::decode(&a.data))
                    .map_or(0.2, |m| f64::from(m.close_factor_pct) / 100.0);
                let (_, owed) = reading.balance(&repay);
                let amount = owed * close_factor;
                let liquidator = &self.liquidators[l];
                let mut instructions =
                    klend::liquidator_accounts(&payer, &repay.keys, &withdraw.keys);
                let mut reserves = obligation.reserves();
                for extra in [repay.keys.address, withdraw.keys.address] {
                    if !reserves.contains(&extra) {
                        reserves.push(extra);
                    }
                }
                for address in &reserves {
                    if let Some(book) = self.books.at(address) {
                        instructions.push(klend::refresh_reserve(&book.keys));
                    }
                }
                instructions.push(klend::refresh_obligation(
                    market,
                    address,
                    &obligation.reserves(),
                ));
                instructions.push(klend::liquidate(
                    &payer,
                    address,
                    &repay.keys,
                    &withdraw.keys,
                    atoms(amount, repay.decimals),
                    0,
                ));
                let result = land(
                    self.chain.as_ref(),
                    &self.books,
                    market,
                    &liquidator.keypair,
                    "liquidate",
                    instructions,
                )
                .await;
                let name = liquidator.name.clone();
                // Whatever happened, the obligation changed or will be read
                // again.
                readings.remove(address);
                self.record(&name, "liquidate", address, amount, result);
            }
        }
    }
}

#[async_trait]
impl Actor for KaminoActors {
    async fn after_blocks(&mut self, _now: &Moment) {
        self.borrowers_act().await;
        self.liquidators_act().await;
        self.step += 1;
    }

    async fn report(&mut self, _report: &mut ActorsReport) {}
}

/// Kamino's keeper: every step, a refresh of every reserve the session
/// uses, on the session's clock. Interest accrues here, by the slots since
/// each reserve's last refresh. Each refresh must land on the step's slot:
/// a reserve stamped elsewhere ran on a stray clock, and the session fails.
pub struct KaminoCrank {
    books: Arc<Books>,
    keeper: Keypair,
}

#[async_trait]
impl VenueCrank for KaminoCrank {
    fn name(&self) -> &str {
        "kamino"
    }

    async fn crank(&mut self, rpc: &RpcClient, step: &CrankStep) -> Result<CrankOutcome> {
        let instructions: Vec<Instruction> = self
            .books
            .reserves
            .iter()
            .map(|b| klend::refresh_reserve(&b.keys))
            .collect();
        match mtx_kit::send(rpc, &self.keeper, instructions, "kamino refresh") {
            Ok(_) => {}
            Err(mtx_kit::Error::Transaction { reason, .. }) => {
                return Ok(CrankOutcome::Refused(reason));
            }
            Err(error) => return Err(venue_error("kamino refresh", error)),
        }
        let keys: Vec<Pubkey> = self.books.reserves.iter().map(|b| b.keys.address).collect();
        for (key, account) in keys.iter().zip(rpc.get_multiple_accounts(&keys)?) {
            let slot = account
                .and_then(|a| klend::Reserve::decode(&a.data))
                .map(|r| r.last_update_slot);
            if slot != Some(step.slot) {
                return Err(venue_error(
                    "kamino clock witness",
                    format!(
                        "reserve {key} was refreshed at slot {slot:?}, not the step's {}",
                        step.slot
                    ),
                ));
            }
        }
        Ok(CrankOutcome::Done)
    }
}

/// One reading of the bot's account.
#[derive(Debug, Clone)]
struct LendingReading {
    holdings: Vec<ReserveHolding>,
    value_usd: f64,
    health: Option<f64>,
    /// The bot's debt per reserve, in atoms: a fall without a repay is a
    /// liquidation.
    debt: BTreeMap<Pubkey, f64>,
    wallet: Wallet,
}

/// A lending session's account: the bot's obligations, refreshed in the
/// slot measured, and its wallet, at the scenario's prices.
struct LendingAccount {
    scenario: Scenario,
    chain: Arc<dyn ChainClient>,
    books: Arc<Books>,
    venue: Kamino,
    bot: Pubkey,
    /// Pays for every refresh the grader simulates, so a bot with no SOL
    /// left is still measured.
    keeper: Pubkey,
    instruments: Vec<InstrumentRef>,
    /// Each reserve's price when the session starts, and the feed that
    /// moves it.
    start_prices: BTreeMap<Pubkey, f64>,
    feeds: BTreeMap<Pubkey, Feed>,
    observed: Observed,
    start: Option<LendingReading>,
    last: Option<LendingReading>,
    start_value_usd: f64,
    peak_value_usd: f64,
    max_drawdown_usd: f64,
    min_health: Option<f64>,
    liquidated: u32,
    last_debt: BTreeMap<Pubkey, f64>,
    log: ActionLog,
    borrowers: Vec<BorrowerRecord>,
}

impl LendingAccount {
    /// The scenario's price for a reserve's asset `at` slots in: the
    /// path's, whatever its oracle publishes.
    fn price_at(&self, reserve: &Pubkey, at: u64) -> f64 {
        let start = self.start_prices.get(reserve).copied().unwrap_or_default();
        match self.feeds.get(reserve) {
            Some(feed) => feed.true_price(start, at),
            None => start,
        }
    }

    async fn read(&self, at: u64) -> Result<LendingReading> {
        let mut holdings = Vec::new();
        let mut value = 0.0;
        let mut health: Option<f64> = None;
        let mut debt = BTreeMap::new();
        let mut views = Vec::new();
        let mut counted: BTreeSet<Pubkey> = BTreeSet::new();
        for market in self.books.markets() {
            let reading = self.venue.read(&self.bot, &market, &self.keeper).await?;
            if let Some(h) = reading
                .obligation
                .as_ref()
                .and_then(klend::Obligation::health)
            {
                health = Some(health.map_or(h, |was: f64| was.min(h)));
            }
            if let Some(obligation) = &reading.obligation {
                for b in &obligation.borrows {
                    debt.insert(b.reserve, b.amount);
                }
            }
            for book in self.books.of_market(&market) {
                let (supplied, borrowed) = reading.balance(book);
                let price = self.price_at(&book.keys.address, at);
                let reserve = reading.reserves.get(&book.keys.address);
                let wallet = self.wallet_units(book, reserve).await?;
                // A mint in two reserves' wallets is one wallet.
                let wallet_counted = if counted.insert(book.keys.mint) {
                    wallet
                } else {
                    0.0
                };
                value += (supplied - borrowed + wallet_counted) * price;
                holdings.push(ReserveHolding {
                    instrument: book.instrument.clone(),
                    supplied,
                    borrowed,
                    price_usd: price,
                    wallet,
                });
                if let Some(reserve) = reserve {
                    views.push(self.view(book, reserve, supplied, borrowed, &reading).await);
                }
            }
        }
        let first = &holdings[0];
        Ok(LendingReading {
            wallet: Wallet {
                base: first.supplied - first.borrowed,
                quote: 0.0,
                price_usd: first.price_usd,
                value_usd: value,
                venues: views,
            },
            holdings,
            value_usd: value,
            health,
            debt,
        })
    }

    /// The wallet's asset and collateral tokens for one reserve, in asset
    /// units.
    async fn wallet_units(
        &self,
        book: &kamino::Book,
        reserve: Option<&klend::Reserve>,
    ) -> Result<f64> {
        let asset = mtx_kit::token::associated_token_address(
            &self.bot,
            &book.keys.mint,
            &book.keys.token_program,
        );
        let ctoken = klend::collateral_ata(&self.bot, &book.keys);
        let found = self
            .chain
            .get_multiple_accounts(&[asset, ctoken])
            .await
            .map_err(|e| venue_error("wallet", e))?;
        let amount = |i: usize| -> f64 {
            found[i]
                .as_ref()
                .and_then(|a| TokenAccountInfo::decode(&a.owner, &a.data).ok())
                .map_or(0.0, |t| t.amount as f64)
        };
        let rate = reserve.map_or(1.0, klend::Reserve::exchange_rate);
        Ok((amount(0) + amount(1) * rate) / 10f64.powi(book.decimals as i32))
    }

    /// A reserve as a component's step shows it.
    async fn view(
        &self,
        book: &kamino::Book,
        reserve: &klend::Reserve,
        supplied: f64,
        borrowed: f64,
        reading: &Reading,
    ) -> crate::spot::VenueReading {
        let twap = match reserve.twap_chain[0] {
            klend::NO_ENTRY => None,
            entry => self
                .chain
                .get_account(&reserve.scope_prices)
                .await
                .ok()
                .flatten()
                .and_then(|a| crate::oracles::scope_price(&a.data, entry)),
        };
        let left = |limit: u64, used: f64| {
            (limit as f64 - used).max(0.0) / 10f64.powi(book.decimals as i32)
        };
        let mut stats = vec![
            ("utilization".to_string(), reserve.utilization()),
            ("supply_apy".to_string(), reserve.supply_apr().exp_m1()),
            ("borrow_apy".to_string(), reserve.borrow_apr().exp_m1()),
            (
                "ltv".to_string(),
                f64::from(reserve.loan_to_value_pct) / 100.0,
            ),
            (
                "liquidation_ltv".to_string(),
                f64::from(reserve.liquidation_threshold_pct) / 100.0,
            ),
            (
                "borrow_factor".to_string(),
                reserve.borrow_factor_pct as f64 / 100.0,
            ),
            ("price".to_string(), reserve.price),
            ("price_status".to_string(), f64::from(reserve.price_status)),
            (
                "deposit_limit_left".to_string(),
                left(reserve.deposit_limit, reserve.total_supply()),
            ),
            (
                "borrow_limit_left".to_string(),
                left(reserve.borrow_limit, reserve.borrowed),
            ),
        ];
        if let Some(twap) = twap {
            stats.push(("twap".to_string(), twap));
        }
        crate::spot::VenueReading {
            id: book.instrument.to_string(),
            supplied,
            borrowed,
            health: reading
                .obligation
                .as_ref()
                .and_then(klend::Obligation::health),
            stats,
        }
    }
}

#[async_trait]
impl Account for LendingAccount {
    fn start_value_usd(&self) -> f64 {
        self.start_value_usd
    }

    fn moment(&self, at: u64) -> Moment {
        let price = self
            .books
            .reserves
            .first()
            .map_or(0.0, |b| self.price_at(&b.keys.address, at));
        Moment {
            at,
            level_usd: price,
            book_usd: price,
            fair_usd: None,
        }
    }

    async fn step(&mut self, k: u64, _slot: u64, at: u64) -> Result<StepRecord> {
        let now = self.read(at).await?;
        for (instrument, holding) in self.instruments.iter().zip(&now.holdings) {
            self.observed
                .record(instrument, holding.supplied - holding.borrowed);
        }
        if let Some(h) = now.health {
            self.min_health = Some(self.min_health.map_or(h, |m: f64| m.min(h)));
        }
        // Debt that fell between readings, at a reserve the bot didn't
        // repay this step, went to a liquidator: counted once per step.
        let fell = self.last_debt.iter().any(|(reserve, before)| {
            now.debt.get(reserve).copied().unwrap_or_default() < before * 0.999
        });
        if fell
            && now
                .holdings
                .iter()
                .any(|h| h.borrowed > 0.0 || h.supplied > 0.0)
        {
            let health_before = self.last.as_ref().and_then(|l| l.health);
            if health_before.is_some_and(|h| h < 1.0) {
                self.liquidated += 1;
            }
        }
        self.last_debt = now.debt.clone();
        let value_usd = now.value_usd;
        self.peak_value_usd = self.peak_value_usd.max(value_usd);
        self.max_drawdown_usd = self.max_drawdown_usd.max(self.peak_value_usd - value_usd);
        let first = &now.holdings[0];
        let record = StepRecord {
            k,
            at,
            mark_usd: first.price_usd,
            index_usd: first.price_usd,
            level_usd: first.price_usd,
            book_usd: None,
            best_bid_usd: None,
            best_ask_usd: None,
            position: first.supplied - first.borrowed,
            value_usd,
            pnl_usd: value_usd - self.start_value_usd,
            turn_ms: None,
            answered: None,
            fair_usd: None,
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
        _tx: Option<Arc<crate::txlayer::TxEngine>>,
    ) -> Result<Desk> {
        let mut seats = Vec::new();
        for instrument in &self.instruments {
            let market = self.venue.market(instrument).await?;
            seats.push(market.seat(bot.insecure_clone(), host_url).await?);
        }
        let mut venues = mtx_kit::VenueInfo::of_mandate(mandate, &self.instruments[0], |i| {
            self.instruments.contains(i)
        });
        for info in &mut venues {
            info.address = self.books.get(&info.id).map(|b| b.keys.address.to_string());
        }
        let start = self
            .start
            .as_ref()
            .map(|s| s.wallet.clone())
            .unwrap_or_default();
        Ok(Desk {
            markets: venues,
            seats,
            symbol: env.symbol.clone(),
            venue: serde_json::to_string(&self.metadata())?,
            authority: bot.pubkey().to_string(),
            reader: Reader::Lending { start },
            rfqs: None,
            pool: None,
            tx: None,
        })
    }

    fn decode(&self, ix: &serde_json::Value, keys: &[serde_json::Value]) -> IxRecord {
        decode_instruction(ix, keys)
    }

    async fn finish(mut self: Box<Self>, done: Done) -> Result<SessionReport> {
        let end_at = done.end_slot.saturating_sub(done.start_slot);
        let end = self.read(end_at).await?;
        for (instrument, holding) in self.instruments.iter().zip(&end.holdings) {
            self.observed
                .record(instrument, holding.supplied - holding.borrowed);
        }
        let start = self.start.clone().expect("a start reading");
        let liquidations = done
            .transactions
            .iter()
            .filter(|t| {
                t.ok && t
                    .instructions
                    .iter()
                    .any(|i| i.name.as_deref() == Some(LIQUIDATE))
            })
            .count() as u32;
        let instrument = self.instruments[0].clone();
        let observed = std::mem::take(&mut self.observed);
        let end_value_usd = end.value_usd;
        let start_price = start.holdings[0].price_usd;
        let end_price = end.holdings[0].price_usd;
        let actions = std::mem::take(&mut *self.log.lock().expect("log"));
        Ok(SessionReport {
            scenario: self.scenario.name.clone(),
            start_mark_usd: start_price,
            end_mark_usd: end_price,
            start_value_usd: self.start_value_usd,
            end_value_usd,
            pnl_usd: end_value_usd - self.start_value_usd,
            end_position: end.holdings[0].supplied - end.holdings[0].borrowed,
            liquidatable: end.health.is_some_and(|h| h < 1.0),
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
            valuation: "scenario",
            flow: done.actors.flow,
            competitor: done.actors.competitor,
            actors: done.actors.library,
            spot: None,
            faults: done.faults,
            order: None,
            participation: None,
            net: None,
            lending: Some(LendingReport {
                start: start.holdings.clone(),
                end: end.holdings.clone(),
                min_health: self.min_health,
                liquidated: self.liquidated,
                liquidations,
                borrowers: std::mem::take(&mut self.borrowers),
                actions,
            }),
            crank_refusals: done.crank_refusals,
            oracle_starts: Default::default(),
            tx_layer: None,
            turns: done.turns,
            wall_ms: done.began.elapsed().as_millis() as u64,
            timeline: Timeline {
                start_slot: done.start_slot,
                end_slot: done.end_slot,
                slots_per_step: SLOTS_PER_STEP,
                start_mark_usd: start_price,
                steps: done.steps,
                transactions: done.transactions,
                calls: done.calls,
                taker_orders: done.actors.taker_orders,
            },
        })
    }
}

impl LendingAccount {
    /// What a strategy's session names as the venue: each reserve's keys,
    /// so it can find its own obligation and others'.
    fn metadata(&self) -> serde_json::Value {
        serde_json::json!({
            "program": klend::PROGRAM_ID.to_string(),
            "reserves": self.books.reserves.iter().map(|b| serde_json::json!({
                "id": b.instrument.to_string(),
                "reserve": b.keys.address.to_string(),
                "market": b.keys.market.to_string(),
                "mint": b.keys.mint.to_string(),
                "decimals": b.decimals,
                "scope_prices": b.keys.scope_prices.to_string(),
            })).collect::<Vec<_>>(),
        })
    }
}

const LIQUIDATE: &str = "liquidate_obligation_and_redeem_reserve_collateral_v2";

/// klend's instructions a session's transactions use, by their tag.
const NAMES: [&str; 11] = [
    "refresh_reserve",
    "refresh_obligation",
    "init_user_metadata",
    "init_obligation",
    "init_obligation_farms_for_reserve",
    "deposit_reserve_liquidity_and_obligation_collateral_v2",
    "withdraw_obligation_collateral_and_redeem_reserve_collateral_v2",
    "borrow_obligation_liquidity_v2",
    "repay_obligation_liquidity_v2",
    LIQUIDATE,
    "deposit_reserve_liquidity",
];

/// One instruction of a bot's transaction: klend's by name.
fn decode_instruction(ix: &serde_json::Value, keys: &[serde_json::Value]) -> IxRecord {
    let program = ix["programIdIndex"]
        .as_u64()
        .and_then(|i| keys.get(i as usize))
        .and_then(|k| k.as_str())
        .unwrap_or_default();
    if program == klend::PROGRAM_ID.to_string() {
        let tag = ix["data"]
            .as_str()
            .and_then(|d| bs58::decode(d).into_vec().ok())
            .and_then(|d| d.get(..8).map(<[u8]>::to_vec));
        let name = NAMES
            .iter()
            .find(|n| tag.as_deref() == Some(&klend::instruction_tag(n)[..]))
            .map(|n| n.to_string());
        return IxRecord {
            program: VENUE.into(),
            name,
            order: None,
        };
    }
    let name = if program == "ComputeBudget111111111111111111111111111111" {
        "compute_budget"
    } else {
        program
    };
    IxRecord {
        program: name.into(),
        name: None,
        order: None,
    }
}

/// A snapshot recipe's Kamino markets (`kamino-markets`, see
/// [`crate::recipe`]): pairs of a collateral reserve and a debt reserve of
/// one lending market, by address.
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct MarketsSpec {
    pub pairs: Vec<[String; 2]>,
}

/// Load every account a lending session uses into a recipe's fork, and
/// leave it usable offline: every reserve fetched in one call (one fetched
/// later than the clock would be refreshed "in the future", which klend
/// refuses), the clock past their last refreshes, Scope's entries fresh,
/// any Token-2022 UI multiplier frozen at today's (a change scheduled
/// inside a session would move balances' meaning mid-session), and each
/// pair exercised by a throwaway account: deposit, borrow, repay, withdraw.
/// Returns the first collateral's price.
pub(crate) async fn exercise(
    surfnet: &Surfnet,
    rpc: &RpcClient,
    spec: &MarketsSpec,
) -> Result<f64> {
    let parse = |s: &String| -> Result<Pubkey> {
        s.parse()
            .map_err(|_| venue_error("kamino-markets", format!("{s} is not an address")))
    };
    let mut reserves: Vec<Pubkey> = Vec::new();
    for pair in &spec.pairs {
        for r in pair {
            let r = parse(r)?;
            if !reserves.contains(&r) {
                reserves.push(r);
            }
        }
    }
    let fetched = rpc.get_multiple_accounts(&reserves)?;
    let mut decoded = Vec::new();
    for (address, account) in reserves.iter().zip(fetched) {
        let reserve = account
            .and_then(|a| klend::Reserve::decode(&a.data))
            .ok_or_else(|| venue_error("kamino-markets", format!("{address} is not a reserve")))?;
        decoded.push(reserve);
    }
    let mut others: Vec<Pubkey> = Vec::new();
    for reserve in &decoded {
        let lut = kamino::NAMED
            .iter()
            .find(|n| n.market == reserve.lending_market)
            .map(|n| n.lookup_table);
        for key in [
            reserve.lending_market,
            reserve.mint,
            reserve.supply_vault,
            reserve.fee_vault,
            reserve.collateral_mint,
            reserve.collateral_vault,
            reserve.farm_collateral,
            reserve.farm_debt,
            reserve.scope_prices,
        ]
        .into_iter()
        .chain(lut)
        {
            if key != Pubkey::default() && !others.contains(&key) {
                others.push(key);
            }
        }
    }
    for chunk in others.chunks(100) {
        rpc.get_multiple_accounts(chunk)?;
    }
    let latest = decoded
        .iter()
        .map(|r| r.last_update_slot)
        .max()
        .unwrap_or(0);
    let (slot, unix) = clock(rpc)?;
    if slot <= latest {
        info!(
            slot,
            latest, "moving the fork's clock past the reserves' last refresh"
        );
        surfnet
            .cheatcodes()
            .time_travel_to_slot_at(latest + 1, (unix as u64 + 1) * 1_000)?;
    }
    let (slot, unix) = clock(rpc)?;
    let mut scope: BTreeMap<Pubkey, BTreeSet<u16>> = BTreeMap::new();
    for reserve in &decoded {
        let entries = scope.entry(reserve.scope_prices).or_default();
        for i in reserve.price_chain.iter().chain(&reserve.twap_chain) {
            if *i != klend::NO_ENTRY {
                entries.insert(*i);
            }
        }
    }
    for (account, entries) in &scope {
        let entries: Vec<(u16, f64)> = entries.iter().map(|i| (*i, 1.0)).collect();
        write_scope(surfnet, rpc, account, &entries, slot, unix)?;
    }
    for reserve in &decoded {
        freeze_ui_multiplier(surfnet, rpc, &reserve.mint, unix)?;
    }

    let instruments: Vec<InstrumentRef> = reserves
        .iter()
        .map(|r| instrument(&r.to_string()))
        .collect();
    let venue = Kamino::connect(surfnet.rpc_url(), &instruments).await?;
    let books = venue.books().clone();
    let chain = RpcChainClient::new(surfnet.rpc_url());
    let mut price = None;
    for pair in &spec.pairs {
        let collateral = books
            .at(&parse(&pair[0])?)
            .expect("a book per reserve")
            .clone();
        let debt = books
            .at(&parse(&pair[1])?)
            .expect("a book per reserve")
            .clone();
        let c = &decoded[reserves
            .iter()
            .position(|r| *r == collateral.keys.address)
            .expect("decoded")];
        let d = &decoded[reserves
            .iter()
            .position(|r| *r == debt.keys.address)
            .expect("decoded")];
        price.get_or_insert(c.price);
        let user = Keypair::new();
        let key = user.pubkey();
        surfnet
            .cheatcodes()
            .fund_sol(&key, (FEE_SOL * 1e9) as u64)?;
        let collateral_units = 200.0 / c.price;
        let debt_units = 10.0 / d.price;
        crate::spot::fund_mint(
            surfnet,
            &key,
            &collateral.keys.mint,
            atoms(collateral_units, collateral.decimals),
        )?;
        crate::spot::fund_mint(
            surfnet,
            &key,
            &debt.keys.mint,
            atoms(debt_units * 2.0, debt.decimals),
        )?;
        open_obligation(&chain, &books, &collateral.keys.market, &user).await?;
        let steps: [(&str, &kamino::Book, Instruction); 6] = [
            (
                "deposit collateral",
                &collateral,
                klend::deposit(
                    &key,
                    &collateral.keys,
                    atoms(collateral_units, collateral.decimals),
                ),
            ),
            (
                "borrow",
                &debt,
                klend::borrow(&key, &debt.keys, atoms(debt_units, debt.decimals)),
            ),
            ("repay", &debt, klend::repay(&key, &debt.keys, u64::MAX)),
            (
                "withdraw collateral",
                &collateral,
                klend::withdraw(&key, &collateral.keys, u64::MAX),
            ),
            (
                "deposit debt asset",
                &debt,
                klend::deposit(&key, &debt.keys, atoms(debt_units, debt.decimals)),
            ),
            (
                "withdraw debt asset",
                &debt,
                klend::withdraw(&key, &debt.keys, u64::MAX),
            ),
        ];
        for (label, book, ix) in steps {
            act(&chain, &books, &user, book, label, ix)
                .await
                .map_err(|e| venue_error(format!("kamino-markets: {label}"), e))?;
        }
    }
    price.ok_or_else(|| venue_error("kamino-markets", "no pairs"))
}

/// Fix a Token-2022 mint's UI multiplier (`ScaledUiAmount`) at the one in
/// force at `unix`, with no change pending.
fn freeze_ui_multiplier(
    surfnet: &Surfnet,
    rpc: &RpcClient,
    mint: &Pubkey,
    unix: i64,
) -> Result<()> {
    let account = rpc.get_account(mint)?;
    let Ok(decoded) = mtx_kit::token::Mint::decode(&account.owner, &account.data) else {
        return Ok(());
    };
    let Some(scaled) = decoded.scaled_ui_amount else {
        return Ok(());
    };
    let now = scaled.at(unix);
    let mut data = account.data.clone();
    // The extensions follow the account type byte, as type, length, value.
    let mut at = mtx_kit::token::ACCOUNT_LEN + 1;
    while let Some(header) = data.get(at..at + 4) {
        let ty = u16::from_le_bytes([header[0], header[1]]);
        let len = usize::from(u16::from_le_bytes([header[2], header[3]]));
        if ty == 25 {
            let value = &mut data[at + 4..at + 4 + len];
            value[32..40].copy_from_slice(&now.to_le_bytes());
            value[40..48].copy_from_slice(&0i64.to_le_bytes());
            value[48..56].copy_from_slice(&now.to_le_bytes());
            info!(%mint, multiplier = now, "UI multiplier frozen");
            surfnet
                .cheatcodes()
                .set_account(mint, account.lamports, &data, &account.owner)?;
            return Ok(());
        }
        at += 4 + len;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn warps_leave_gaps_in_the_session_slots() {
        let (gaps, added) = gaps(&[
            Warp {
                step: 3,
                slots: 100,
            },
            Warp { step: 5, slots: 50 },
        ]);
        assert_eq!(gaps, vec![(6, 106), (110, 160)]);
        assert_eq!(added, 150);
    }

    /// A grade rescored from its report reads the lending section back.
    #[test]
    fn a_lending_report_reads_back() {
        let report = LendingReport {
            liquidations: 1,
            actions: vec![ActorAction {
                step: 3,
                by: "kamino-liquidator/0".into(),
                action: "liquidate",
                obligation: Pubkey::new_unique().to_string(),
                amount: 2.5,
                error: None,
            }],
            ..LendingReport::default()
        };
        let text = serde_json::to_string(&report).unwrap();
        assert_eq!(
            serde_json::from_str::<LendingReport>(&text).unwrap(),
            report
        );
        let empty = serde_json::to_string(&LendingReport::default()).unwrap();
        assert!(!empty.contains("actions"));
        assert_eq!(
            serde_json::from_str::<LendingReport>(&empty).unwrap(),
            LendingReport::default()
        );
    }

    #[test]
    fn klend_instructions_decode_by_name() {
        let keys = vec![serde_json::json!(klend::PROGRAM_ID.to_string())];
        let ix = klend::refresh_obligation(&Pubkey::new_unique(), &Pubkey::new_unique(), &[]);
        let json = serde_json::json!({
            "programIdIndex": 0,
            "data": bs58::encode(&ix.data).into_string(),
        });
        let record = decode_instruction(&json, &keys);
        assert_eq!(record.program, "kamino");
        assert_eq!(record.name.as_deref(), Some("refresh_obligation"));
    }
}
