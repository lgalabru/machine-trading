//! Kamino Lending as a [`Venue`] (`specs/venues/kamino.md`): the reserves of
//! its lending markets, each a money-market instrument
//! (`specs/instruments/money-market.md`).
//!
//! An account's holding at a reserve is what its obligation supplied there
//! less what it borrowed, read the way the profile requires: from an
//! obligation refreshed in the slot measured. The grader simulates the
//! refresh ([`Kamino::read`]) rather than trusting what the accounts last
//! stored, so interest and prices are as of now. Values are the program's,
//! at each reserve's price from its oracle.
//!
//! A [`LendingSeat`] sends each lending action as one transaction: a refresh
//! of every reserve the obligation touches and of the obligation, then the
//! action, as klend requires, through the market's lookup table where the
//! snapshot holds it.

pub mod klend;

use std::{collections::BTreeMap, sync::Arc};

use async_trait::async_trait;
use mtx_core::{
    Asset, Decimal, Holding, LendingBalance, VenueId,
    mandate::{InstrumentRef, Kind},
};
use mtx_kit::{
    Action, Planned, SendState, Sent,
    chain::{
        AddressLookupTableAccount, ChainClient, InstructionGroup, RpcChainClient, TransactionPlan,
    },
};
use solana_keypair::Keypair;
use solana_pubkey::{Pubkey, pubkey};
use solana_signer::Signer;
use solana_transaction::{Instruction, versioned::VersionedTransaction};

use self::klend::{Obligation, Reserve, ReserveKeys};
use super::{Market, Seat, SeatResult, Venue, not_offered};
use crate::{Error, Result, session::INTERNAL_RPC_TIMEOUT};

/// A reserve the profile names, its lending market, and the market's lookup
/// table.
#[derive(Debug, Clone, Copy)]
pub struct Named {
    pub symbol: &'static str,
    pub reserve: Pubkey,
    pub market: Pubkey,
    pub lookup_table: Pubkey,
}

const MAIN: Pubkey = pubkey!("7u3HeHxYDLhnCoErrtycNokbQYbWGzLs6JSDqGAv5PfF");
const MAIN_LUT: Pubkey = pubkey!("FGMSBiyVE8TvZcdQnZETAAKw28tkQJ2ccZy6pyp95URb");
const BACKPACK: Pubkey = pubkey!("4iRHKGsTq3e4uut6e4PfyV9AEbNuXaMu9JaSP378p9qy");
const BACKPACK_LUT: Pubkey = pubkey!("DQUBKNkMfRjKsYD6fwrAjA34TNMS3P5ytxNcrSJiTqBy");
const XSTOCKS: Pubkey = pubkey!("5wJeMrUYECGq41fxRESKALVcHnNX26TAWy4W98yULsua");
const XSTOCKS_LUT: Pubkey = pubkey!("8ofreL6hKfEet1DnhHVGvCTnSdz4pg85PpbuCUHnEcKm");

/// The reserves the profile names, by symbol.
pub const NAMED: [Named; 6] = [
    Named {
        symbol: "SOL",
        reserve: pubkey!("d4A2prbA2whesmvHaL88BH6Ewn5N4bTSU2Ze8P6Bc4Q"),
        market: MAIN,
        lookup_table: MAIN_LUT,
    },
    Named {
        symbol: "USDC",
        reserve: pubkey!("D6q6wuQSrifJKZYpR1M8R4YawnLDtDsMmWM1NbBmgJ59"),
        market: MAIN,
        lookup_table: MAIN_LUT,
    },
    Named {
        symbol: "SPCX-BACKPACK",
        reserve: pubkey!("GrtBFz6BSky1PiyVBL7jy3w4Z1hKz2wvSjT4vd2iouC3"),
        market: BACKPACK,
        lookup_table: BACKPACK_LUT,
    },
    Named {
        symbol: "USDC-BACKPACK",
        reserve: pubkey!("7gHxajRCcU5sb9rQrpYDvkugSqgGsi9EG947pW7xJueT"),
        market: BACKPACK,
        lookup_table: BACKPACK_LUT,
    },
    Named {
        symbol: "SPYX-XSTOCKS",
        reserve: pubkey!("UvXjBuC7YZYaGB9Rn1PpBD1GySmjzunXgE8Zev9ua8d"),
        market: XSTOCKS,
        lookup_table: XSTOCKS_LUT,
    },
    Named {
        symbol: "USDC-XSTOCKS",
        reserve: pubkey!("97zoywd8mPZsGTg8q1wdD2Wgkdrs2tqusp1Qqcxbyj7E"),
        market: XSTOCKS,
        lookup_table: XSTOCKS_LUT,
    },
];

/// Compute a lending transaction may use: a refresh of every reserve it
/// touches, the farms' bookkeeping, and the action.
const COMPUTE_UNIT_LIMIT: u32 = 1_400_000;

/// One reserve of a session's: its instrument, its keys and its asset.
#[derive(Debug, Clone)]
pub struct Book {
    pub instrument: InstrumentRef,
    pub keys: ReserveKeys,
    pub asset: Asset,
    pub decimals: u32,
}

/// Every reserve a session reads, by the venue's reference, and each
/// lending market's lookup table.
#[derive(Debug, Clone, Default)]
pub struct Books {
    pub reserves: Vec<Book>,
    pub lookup_tables: BTreeMap<Pubkey, AddressLookupTableAccount>,
}

impl Books {
    pub fn get(&self, id: &str) -> Option<&Book> {
        self.reserves
            .iter()
            .find(|b| b.instrument.to_string() == id)
    }

    pub fn at(&self, reserve: &Pubkey) -> Option<&Book> {
        self.reserves.iter().find(|b| b.keys.address == *reserve)
    }

    /// The reserves of one lending market.
    pub fn of_market<'a>(&'a self, market: &'a Pubkey) -> impl Iterator<Item = &'a Book> + 'a {
        self.reserves
            .iter()
            .filter(move |b| b.keys.market == *market)
    }

    /// The lending markets, in the order their reserves come.
    pub fn markets(&self) -> Vec<Pubkey> {
        let mut markets: Vec<Pubkey> = Vec::new();
        for book in &self.reserves {
            if !markets.contains(&book.keys.market) {
                markets.push(book.keys.market);
            }
        }
        markets
    }
}

/// An account's refreshed state in one lending market: its obligation, if
/// it has one, and each of the session's reserves there.
#[derive(Debug, Clone, Default)]
pub struct Reading {
    pub reserves: BTreeMap<Pubkey, Reserve>,
    pub obligation: Option<Obligation>,
}

impl Reading {
    /// The obligation's supply and debt at `reserve`, in asset units.
    pub fn balance(&self, book: &Book) -> (f64, f64) {
        let Some(reserve) = self.reserves.get(&book.keys.address) else {
            return (0.0, 0.0);
        };
        let Some(obligation) = &self.obligation else {
            return (0.0, 0.0);
        };
        let supplied: f64 = obligation
            .deposits
            .iter()
            .filter(|d| d.reserve == book.keys.address)
            .map(|d| d.amount as f64 * reserve.exchange_rate())
            .fold(0.0, |a, b| a + b);
        let borrowed: f64 = obligation
            .borrows
            .iter()
            .filter(|b| b.reserve == book.keys.address)
            .map(|b| b.amount)
            .fold(0.0, |a, b| a + b);
        (
            reserve.atoms_to_units(supplied),
            reserve.atoms_to_units(borrowed),
        )
    }
}

/// Kamino, read and traded through one endpoint, offering the reserves of
/// `books`.
pub struct Kamino {
    id: VenueId,
    chain: Arc<dyn ChainClient>,
    books: Arc<Books>,
}

/// The reserve a reference names: a symbol the profile names, or an address.
pub fn reserve_of(symbol: &str) -> Option<Pubkey> {
    NAMED
        .iter()
        .find(|n| n.symbol == symbol)
        .map(|n| n.reserve)
        .or_else(|| symbol.parse().ok())
}

fn venue_error(label: impl Into<String>, reason: impl ToString) -> Error {
    Error::Transaction {
        label: label.into(),
        reason: reason.to_string(),
    }
}

impl Kamino {
    /// The venue with the reserves `instruments` name, read from the chain
    /// at `rpc_url`.
    pub async fn connect(rpc_url: &str, instruments: &[InstrumentRef]) -> Result<Self> {
        let id = VenueId::new("kamino");
        let chain: Arc<dyn ChainClient> =
            Arc::new(RpcChainClient::with_timeout(rpc_url, INTERNAL_RPC_TIMEOUT));
        let mut books = Books::default();
        for instrument in instruments {
            if instrument.venue != id.0 || instrument.kind != Kind::MoneyMarket {
                return Err(not_offered(&id, instrument));
            }
            let address =
                reserve_of(&instrument.symbol).ok_or_else(|| not_offered(&id, instrument))?;
            let account = chain
                .get_account(&address)
                .await
                .map_err(|e| venue_error(instrument.to_string(), e))?
                .ok_or_else(|| venue_error(instrument.to_string(), "no reserve at its address"))?;
            if account.owner != klend::PROGRAM_ID {
                return Err(venue_error(instrument.to_string(), "not a klend account"));
            }
            let reserve = Reserve::decode(&account.data)
                .ok_or_else(|| venue_error(instrument.to_string(), "not a reserve"))?;
            let symbol = instrument
                .symbol
                .split('-')
                .next()
                .unwrap_or(&instrument.symbol)
                .to_string();
            books.reserves.push(Book {
                instrument: instrument.clone(),
                keys: ReserveKeys::of(address, &reserve),
                asset: Asset::new(reserve.mint.to_string(), &symbol, reserve.decimals as u8)
                    .map_err(|e| venue_error(instrument.to_string(), e))?,
                decimals: reserve.decimals,
            });
        }
        for market in books.markets() {
            let table = NAMED
                .iter()
                .find(|n| n.market == market)
                .map(|n| n.lookup_table);
            if let Some(table) = table
                && let Ok(found) =
                    mtx_kit::chain::lookup::lookup_table(chain.as_ref(), &table).await
            {
                books.lookup_tables.insert(market, found);
            }
        }
        Ok(Self {
            id,
            chain,
            books: Arc::new(books),
        })
    }

    pub fn books(&self) -> &Arc<Books> {
        &self.books
    }

    /// `owner`'s plain obligation in `market`, and every session reserve
    /// there, refreshed in the current slot by a simulation that `payer`
    /// pays for.
    pub async fn read(&self, owner: &Pubkey, market: &Pubkey, payer: &Pubkey) -> Result<Reading> {
        read(self.chain.as_ref(), &self.books, owner, market, payer).await
    }
}

/// [`Kamino::read`], for any obligation: `owner`'s plain one in `market`.
pub async fn read(
    chain: &dyn ChainClient,
    books: &Books,
    owner: &Pubkey,
    market: &Pubkey,
    payer: &Pubkey,
) -> Result<Reading> {
    read_obligation(
        chain,
        books,
        &klend::obligation(owner, market),
        market,
        payer,
    )
    .await
}

/// The obligation at `address` in `market` and every session reserve there,
/// refreshed in the current slot by a simulation.
pub async fn read_obligation(
    chain: &dyn ChainClient,
    books: &Books,
    address: &Pubkey,
    market: &Pubkey,
    payer: &Pubkey,
) -> Result<Reading> {
    let label = || format!("reading obligation {address}");
    let stored = chain
        .get_account(address)
        .await
        .map_err(|e| venue_error(label(), e))?
        .and_then(|a| Obligation::decode(&a.data));
    let mut reserves: Vec<Pubkey> = books.of_market(market).map(|b| b.keys.address).collect();
    if let Some(obligation) = &stored {
        for reserve in obligation.reserves() {
            if !reserves.contains(&reserve) {
                reserves.push(reserve);
            }
        }
    }
    let mut instructions = Vec::new();
    for reserve in &reserves {
        instructions.push(klend::refresh_reserve(
            &keys_of(chain, books, reserve).await?,
        ));
    }
    let mut watch = reserves.clone();
    if let Some(obligation) = &stored {
        instructions.push(klend::refresh_obligation(
            market,
            address,
            &obligation.reserves(),
        ));
        watch.push(*address);
    }
    let plan = plan(books, market, "read", instructions);
    let blockhash = chain
        .get_latest_blockhash()
        .await
        .map_err(|e| venue_error(label(), e))?;
    let tx = plan
        .versioned(payer, blockhash)
        .map_err(|e| venue_error(label(), e))?;
    let simulation = chain
        .simulate(&tx, &watch)
        .await
        .map_err(|e| venue_error(label(), e))?;
    if let Some(error) = simulation.error {
        return Err(venue_error(
            label(),
            format!("{error}: {}", tail(&simulation.logs)),
        ));
    }
    let mut reading = Reading::default();
    for (key, account) in simulation.accounts {
        let Some(account) = account else { continue };
        if key == *address {
            reading.obligation = Obligation::decode(&account.data);
        } else if let Some(reserve) = Reserve::decode(&account.data) {
            reading.reserves.insert(key, reserve);
        }
    }
    Ok(reading)
}

/// A reserve's keys: from the session's books, or read from the chain for a
/// reserve the session doesn't name (one a counterparty's obligation uses).
async fn keys_of(chain: &dyn ChainClient, books: &Books, reserve: &Pubkey) -> Result<ReserveKeys> {
    if let Some(book) = books.at(reserve) {
        return Ok(book.keys);
    }
    let account = chain
        .get_account(reserve)
        .await
        .map_err(|e| venue_error(format!("reserve {reserve}"), e))?
        .ok_or_else(|| venue_error(format!("reserve {reserve}"), "missing"))?;
    let decoded = Reserve::decode(&account.data)
        .ok_or_else(|| venue_error(format!("reserve {reserve}"), "not a reserve"))?;
    Ok(ReserveKeys::of(*reserve, &decoded))
}

/// One transaction of `instructions`, through the market's lookup table
/// where there is one.
fn plan(
    books: &Books,
    market: &Pubkey,
    label: &str,
    instructions: Vec<Instruction>,
) -> TransactionPlan {
    let plan = TransactionPlan::from_group(
        &InstructionGroup::new(label, instructions),
        COMPUTE_UNIT_LIMIT,
    );
    match books.lookup_tables.get(market) {
        Some(table) => plan.with_lookup_tables(std::slice::from_ref(table)),
        None => plan,
    }
}

fn tail(logs: &[String]) -> String {
    logs[logs.len().saturating_sub(14)..].join(" | ")
}

/// The refresh klend wants right before an action on `obligation` (whose
/// stored state is `stored`, if it exists): every reserve it touches and
/// `extra` (the action's own, which it may not touch yet), then the
/// obligation.
async fn refreshed(
    chain: &dyn ChainClient,
    books: &Books,
    market: &Pubkey,
    obligation: &Pubkey,
    stored: Option<&Obligation>,
    extra: &[Pubkey],
) -> Result<Vec<Instruction>> {
    let touched = stored.map(Obligation::reserves).unwrap_or_default();
    let mut reserves = touched.clone();
    for reserve in extra {
        if !reserves.contains(reserve) {
            reserves.push(*reserve);
        }
    }
    let mut out = Vec::new();
    for reserve in &reserves {
        out.push(klend::refresh_reserve(
            &keys_of(chain, books, reserve).await?,
        ));
    }
    out.push(klend::refresh_obligation(market, obligation, &touched));
    Ok(out)
}

/// The instructions that make `owner`'s plain obligation in `market`: its
/// user metadata if it has none, the obligation, its accounts in the farms
/// of the market's session reserves, and its token accounts there. klend
/// closes an obligation a withdrawal empties, so an account may need them
/// again.
pub async fn obligation_setup(
    chain: &dyn ChainClient,
    books: &Books,
    market: &Pubkey,
    owner: &Pubkey,
) -> Result<Vec<Instruction>> {
    let mut instructions = Vec::new();
    if chain
        .get_account(&klend::user_metadata(owner))
        .await
        .map_err(|e| venue_error("user metadata", e))?
        .is_none()
    {
        instructions.push(klend::init_user_metadata(owner));
    }
    instructions.push(klend::init_obligation(owner, market));
    let obligation = klend::obligation(owner, market);
    for book in books.of_market(market) {
        instructions.push(mtx_kit::chain::token::create_ata_idempotent_for(
            owner,
            owner,
            &book.keys.mint,
            &book.keys.token_program,
        ));
        for (side, farm) in [
            (klend::FarmSide::Collateral, book.keys.farm_collateral),
            (klend::FarmSide::Debt, book.keys.farm_debt),
        ] {
            if !Reserve::has_farm(&farm) {
                continue;
            }
            // A farm account outlives the obligation it was made for.
            let made = chain
                .get_account(&klend::obligation_farm(&farm, &obligation))
                .await
                .map_err(|e| venue_error("obligation farm", e))?
                .is_some();
            if !made {
                instructions.extend(klend::init_obligation_farm(owner, &book.keys, side));
            }
        }
    }
    Ok(instructions)
}

/// Sign and send one lending transaction of `instructions` as `signer`:
/// simulated first, so a refusal comes back with klend's reason and never
/// reaches the chain.
pub async fn send(
    chain: &dyn ChainClient,
    books: &Books,
    market: &Pubkey,
    signer: &Keypair,
    label: &str,
    instructions: Vec<Instruction>,
) -> std::result::Result<mtx_kit::Signature, String> {
    let plan = plan(books, market, label, instructions);
    let blockhash = chain
        .get_latest_blockhash()
        .await
        .map_err(|e| e.to_string())?;
    let unsigned = plan
        .versioned(&signer.pubkey(), blockhash)
        .map_err(|e| e.to_string())?;
    let simulation = chain
        .simulate(&unsigned, &[])
        .await
        .map_err(|e| e.to_string())?;
    if let Some(error) = simulation.error {
        return Err(format!("{error}: {}", tail(&simulation.logs)));
    }
    let tx =
        VersionedTransaction::try_new(unsigned.message, &[signer]).map_err(|e| e.to_string())?;
    chain.send(&tx).await.map_err(|e| e.to_string())
}

#[async_trait]
impl Venue for Kamino {
    fn id(&self) -> &VenueId {
        &self.id
    }

    fn instruments(&self) -> Vec<InstrumentRef> {
        NAMED
            .iter()
            .map(|n| InstrumentRef {
                venue: self.id.0.clone(),
                kind: Kind::MoneyMarket,
                symbol: n.symbol.into(),
            })
            .collect()
    }

    async fn market(&self, instrument: &InstrumentRef) -> Result<Box<dyn Market>> {
        let book = self
            .books
            .get(&instrument.to_string())
            .ok_or_else(|| not_offered(&self.id, instrument))?
            .clone();
        Ok(Box::new(ReserveMarket {
            book,
            books: self.books.clone(),
            chain: self.chain.clone(),
        }))
    }
}

/// One reserve, as a session trades it.
pub struct ReserveMarket {
    book: Book,
    books: Arc<Books>,
    chain: Arc<dyn ChainClient>,
}

impl ReserveMarket {
    async fn reading(&self, owner: &Pubkey) -> Result<Reading> {
        read(
            self.chain.as_ref(),
            &self.books,
            owner,
            &self.book.keys.market,
            owner,
        )
        .await
    }
}

#[async_trait]
impl Market for ReserveMarket {
    fn instrument(&self) -> &InstrumentRef {
        &self.book.instrument
    }

    fn address(&self) -> Pubkey {
        self.book.keys.address
    }

    /// A reserve has no book.
    async fn top(&self) -> Result<(Option<f64>, Option<f64>)> {
        Ok((None, None))
    }

    async fn holdings(&self, owner: &Pubkey) -> Result<Vec<Holding>> {
        let (supplied, borrowed) = self.reading(owner).await?.balance(&self.book);
        Ok(vec![Holding::MoneyMarket(LendingBalance {
            asset: self.book.asset.clone(),
            supplied: decimal(supplied),
            borrowed: decimal(borrowed),
        })])
    }

    /// Supply less debt, at the reserve's price. A session's crank
    /// refreshes every reserve each step, so the stored price is the
    /// oracle's as of the step.
    async fn value_usd(
        &self,
        chain: &dyn ChainClient,
        holdings: &[Holding],
    ) -> Result<Option<Decimal>> {
        let reserve = chain
            .get_account(&self.book.keys.address)
            .await
            .map_err(|e| venue_error(self.book.instrument.to_string(), e))?
            .and_then(|a| Reserve::decode(&a.data))
            .ok_or_else(|| venue_error(self.book.instrument.to_string(), "not a reserve"))?;
        let mut value = Decimal::ZERO;
        for holding in holdings {
            let Holding::MoneyMarket(balance) = holding else {
                return Ok(None);
            };
            value += balance.net() * decimal(reserve.price);
        }
        Ok(Some(value))
    }

    async fn seat(&self, signer: Keypair, rpc_url: &str) -> Result<Box<dyn Seat>> {
        Ok(Box::new(LendingSeat {
            market: self.book.instrument.to_string(),
            book: self.book.clone(),
            books: self.books.clone(),
            chain: Arc::new(RpcChainClient::with_timeout(rpc_url, INTERNAL_RPC_TIMEOUT)),
            signer,
        }))
    }
}

fn decimal(value: f64) -> Decimal {
    Decimal::from_f64_retain(value).unwrap_or_default()
}

/// An account at one reserve, signed by the grader: it supplies, withdraws,
/// borrows, repays and liquidates there, and trades no book.
pub struct LendingSeat {
    market: String,
    book: Book,
    books: Arc<Books>,
    chain: Arc<dyn ChainClient>,
    signer: Keypair,
}

impl LendingSeat {
    fn atoms(&self, units: f64, decimals: u32) -> Option<u64> {
        let atoms = (units * 10f64.powi(decimals as i32)).floor();
        (atoms.is_finite() && atoms >= 1.0 && atoms < u64::MAX as f64).then_some(atoms as u64)
    }

    /// The instructions for a lending action, refresh first; `Err` is why it
    /// isn't sent.
    async fn instructions(&self, action: &Action) -> std::result::Result<Vec<Instruction>, String> {
        let owner = self.signer.pubkey();
        let keys = &self.book.keys;
        let market = keys.market;
        let own = klend::obligation(&owner, &market);
        let stored = self
            .chain
            .get_account(&own)
            .await
            .map_err(|e| e.to_string())?
            .and_then(|a| Obligation::decode(&a.data));
        let chain = self.chain.as_ref();
        let books = self.books.as_ref();
        let refresh = |extra: Vec<Pubkey>| {
            let stored = stored.clone();
            async move {
                refreshed(chain, books, &market, &own, stored.as_ref(), &extra)
                    .await
                    .map_err(|e| e.to_string())
            }
        };
        let amount = |units: f64| {
            self.atoms(units, self.book.decimals)
                .ok_or("an amount that rounds to nothing")
        };
        let mut out = Vec::new();
        if stored.is_none() && !matches!(action, Action::Liquidate { .. }) {
            out.extend(
                obligation_setup(chain, books, &market, &owner)
                    .await
                    .map_err(|e| e.to_string())?,
            );
        }
        match action {
            Action::Deposit { amount: units } => {
                let atoms = match units {
                    Some(units) => amount(*units)?,
                    None => self.wallet_atoms(&owner).await?,
                };
                if atoms == 0 {
                    return Err("nothing to deposit".into());
                }
                out.extend(refresh(vec![keys.address]).await?);
                out.push(klend::deposit(&owner, keys, atoms));
            }
            Action::Withdraw { amount: units } => {
                let collateral = match units {
                    None => u64::MAX,
                    Some(units) => {
                        let reserve = self.reserve().await?;
                        let atoms = amount(*units)? as f64;
                        ((atoms / reserve.exchange_rate()).floor() as u64).max(1)
                    }
                };
                out.extend(refresh(vec![keys.address]).await?);
                out.push(klend::withdraw(&owner, keys, collateral));
            }
            Action::Borrow { amount: units } => {
                let units = units.ok_or("a borrow names its amount")?;
                out.extend(refresh(vec![keys.address]).await?);
                out.push(klend::borrow(&owner, keys, amount(units)?));
            }
            Action::Repay { amount: units } => {
                let atoms = match units {
                    None => u64::MAX,
                    Some(units) => amount(*units)?,
                };
                out.extend(refresh(vec![keys.address]).await?);
                out.push(klend::repay(&owner, keys, atoms));
            }
            Action::Liquidate {
                obligation,
                collateral,
                repay,
                min_received,
            } => {
                let target: Pubkey = obligation
                    .parse()
                    .map_err(|_| format!("{obligation} is not an address"))?;
                let withdraw = books
                    .get(collateral)
                    .ok_or_else(|| format!("{collateral} isn't a reserve of this session"))?;
                if withdraw.keys.market != market {
                    return Err(format!("{collateral} is in another lending market"));
                }
                let theirs = chain
                    .get_account(&target)
                    .await
                    .map_err(|e| e.to_string())?
                    .filter(|a| a.owner == klend::PROGRAM_ID)
                    .and_then(|a| Obligation::decode(&a.data))
                    .filter(|o| o.lending_market == market)
                    .ok_or_else(|| {
                        format!("{target} is not an obligation in this lending market")
                    })?;
                let min = match min_received {
                    Some(units) => self.atoms(*units, withdraw.decimals).unwrap_or(0),
                    None => 0,
                };
                out.extend(klend::liquidator_accounts(&owner, keys, &withdraw.keys));
                out.extend(
                    refreshed(
                        chain,
                        books,
                        &market,
                        &target,
                        Some(&theirs),
                        &[keys.address, withdraw.keys.address],
                    )
                    .await
                    .map_err(|e| e.to_string())?,
                );
                out.push(klend::liquidate(
                    &owner,
                    &target,
                    keys,
                    &withdraw.keys,
                    amount(*repay)?,
                    min,
                ));
            }
            _ => return Err("not a lending action".into()),
        }
        Ok(out)
    }

    async fn reserve(&self) -> std::result::Result<Reserve, String> {
        self.chain
            .get_account(&self.book.keys.address)
            .await
            .map_err(|e| e.to_string())?
            .and_then(|a| Reserve::decode(&a.data))
            .ok_or_else(|| "the reserve is gone".into())
    }

    async fn wallet_atoms(&self, owner: &Pubkey) -> std::result::Result<u64, String> {
        let ata = mtx_kit::token::associated_token_address(
            owner,
            &self.book.keys.mint,
            &self.book.keys.token_program,
        );
        Ok(self
            .chain
            .get_account(&ata)
            .await
            .map_err(|e| e.to_string())?
            .and_then(|a| mtx_kit::token::TokenAccountInfo::decode(&a.owner, &a.data).ok())
            .map_or(0, |t| t.amount))
    }

    /// What became of a send the world will land in its next block.
    async fn outcome(&self, signature: mtx_kit::Signature) -> SendState {
        match self.chain.get_transaction(&signature).await {
            Ok(Some(summary)) => match summary.error {
                None => SendState::Landed(signature.to_string()),
                Some(error) => SendState::Failed {
                    reason: error.to_string(),
                    signature: Some(signature.to_string()),
                },
            },
            Ok(None) | Err(_) => SendState::Delayed,
        }
    }
}

#[async_trait]
impl Seat for LendingSeat {
    fn market(&self) -> &str {
        &self.market
    }

    fn executes(&self, action: &Action) -> bool {
        matches!(
            action,
            Action::Deposit { .. }
                | Action::Withdraw { .. }
                | Action::Borrow { .. }
                | Action::Repay { .. }
                | Action::Liquidate { .. }
        )
    }

    async fn send(&self, planned: &Planned) -> Sent {
        if planned.reduce_only {
            return Sent {
                state: SendState::NotSent("reduce-only doesn't apply to lending".into()),
                sent: false,
            };
        }
        let instructions = match self.instructions(&planned.action).await {
            Ok(instructions) => instructions,
            Err(reason) => {
                return Sent {
                    state: SendState::NotSent(reason),
                    sent: false,
                };
            }
        };
        let label = format!("{} {:?}", self.market, planned.action);
        match send(
            self.chain.as_ref(),
            &self.books,
            &self.book.keys.market,
            &self.signer,
            &label,
            instructions,
        )
        .await
        {
            Ok(signature) => Sent {
                state: self.outcome(signature).await,
                sent: true,
            },
            Err(reason) => Sent {
                state: SendState::failed(reason),
                sent: false,
            },
        }
    }

    async fn position(&self) -> SeatResult<f64> {
        let reading = read(
            self.chain.as_ref(),
            &self.books,
            &self.signer.pubkey(),
            &self.book.keys.market,
            &self.signer.pubkey(),
        )
        .await
        .map_err(|e| e.to_string())?;
        let (supplied, borrowed) = reading.balance(&self.book);
        Ok(supplied - borrowed)
    }
}
