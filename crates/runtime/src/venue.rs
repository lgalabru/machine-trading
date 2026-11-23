//! Venues and their markets, behind one interface.
//!
//! A [`Venue`] is a protocol, as `mtx_core`'s [`VenueId`] names one
//! (`phoenix`, `openbook`, `kamino`): it offers instruments, each named by
//! the mandate's reference `<venue>:<kind>:<symbol>` ([`InstrumentRef`]).
//! Phoenix offers perpetuals and spot books; OpenBook, spot books. A
//! [`Market`] is one of those instruments as a session trades it: its top,
//! its depth, the matches it recorded, and what an account holds there, as
//! a core [`Holding`]. A [`Seat`] is one account trading a market, signed by
//! its host: a strategy component's, or, in a grade, one of the grader's
//! actors'.
//!
//! What a market does follows its instrument's kind, as
//! `specs/instruments/` defines each kind, not its venue: a perp margins, so
//! the session meters its account and [`Market::holdings`] is its signed
//! position; a spot market holds the assets themselves, wherever they sit.
//! Kinds and `mtx:strategy@0.2.0`'s market kinds ([`MarketKind`]) map as:
//!
//! | WIT `market-kind` | Instrument kind (`specs/instruments/`) | Holding |
//! |---|---|---|
//! | `perp` | `perp` | [`Holding::Perp`] |
//! | `spot` | `spot` | [`Holding::Spot`] |
//! | `lending` | `money-market` | [`Holding::MoneyMarket`] |
//! | `amm` | `amm` (draft profile) | [`Holding::Spot`], both assets |
//! | `stake-pool` | `stake-pool` (draft profile) | none yet |
//!
//! A market answers what it can and leaves the rest at the defaults. Its
//! seat says which of the sheet's actions it executes ([`Seat::executes`]);
//! a line it doesn't comes back [`SendState::NotSent`](mtx_kit::SendState).
//! A new venue (Meteora, Raydium's constant-product pools) is one more
//! [`Venue`] offering its markets: a pool's top is the price of a small
//! trade each way, its fills come from its vaults, and its seat executes
//! swaps, liquidity or deposits.
//!
//! A seat that can say what it would send without sending it
//! ([`Seat::instructions`], a [`Built`]) can have its line joined with
//! others into one transaction: an atomic route
//! (`docs/rfd/0005-atomic-arbitrage-tasks.md`, "Atomic groups in the
//! sheet"). Its own [`Seat::send`] sends the same [`Built`] alone, through
//! [`send_built`], so a line has one path to the chain either way.
//!
//! - [`phoenix`]: Phoenix, its Eternal perpetuals ([`perps`]) and its v1
//!   spot books ([`books`]).
//! - [`openbook`]: OpenBook v2 and its spot books ([`books`]).
//! - [`kamino`]: Kamino Lending and its reserves, money markets.
//! - [`amm`]: Orca Whirlpool and Raydium CLMM pools.

pub mod amm;
pub mod books;
pub mod kamino;
pub mod openbook;
pub mod perps;
pub mod phoenix;

use std::collections::BTreeMap;

use async_trait::async_trait;
use mtx_core::{
    Decimal, Holding, MeterError, VenueId,
    mandate::{InstrumentRef, Kind},
    turns::BookLevels,
};
pub use mtx_kit::MarketKind;
use mtx_kit::{
    Action, Planned, SendState, Sent, Side, Signature, VenueInfo,
    chain::{AddressLookupTableAccount, ChainClient, InstructionGroup, TransactionPlan},
    protocols::{USDC_MINT, pyth, spot::MarketFill},
};
use solana_keypair::Keypair;
use solana_pubkey::Pubkey;
use solana_signer::Signer;
use solana_transaction::{Instruction, versioned::VersionedTransaction};

use crate::{Error, Result};

/// What a seat's own orders come to: the venue's error, as text.
pub type SeatResult<T = ()> = std::result::Result<T, String>;

/// A protocol, and the instruments it offers.
#[async_trait]
pub trait Venue: Send + Sync {
    fn id(&self) -> &VenueId;

    /// Every instrument it offers that a session can trade, by the
    /// mandate's reference.
    fn instruments(&self) -> Vec<InstrumentRef>;

    /// The market of one of its instruments.
    async fn market(&self, instrument: &InstrumentRef) -> Result<Box<dyn Market>>;
}

/// The venue `name` names, for spot sessions: its books read through
/// `rpc_url`.
pub fn spot_venue(name: &str, rpc_url: &str) -> Result<Box<dyn Venue>> {
    match name {
        "phoenix" => Ok(Box::new(phoenix::Phoenix::new(rpc_url))),
        "openbook" => Ok(Box::new(openbook::OpenBook::new(rpc_url))),
        other => Err(Error::Transaction {
            label: "a spot venue".into(),
            reason: format!("{other} is not a spot venue: {:?}", books::VENUES),
        }),
    }
}

/// An instrument a venue doesn't offer.
fn not_offered(venue: &VenueId, instrument: &InstrumentRef) -> Error {
    Error::UnknownMarket(format!("{venue} offers no {instrument}"))
}

/// One instrument of a venue's, as a session trades it.
#[async_trait]
pub trait Market: Send + Sync {
    /// The instrument, as the mandate's universe names it.
    fn instrument(&self) -> &InstrumentRef;

    /// The market's account: the perp's or the book's market, the pool.
    fn address(&self) -> Pubkey;

    /// The instrument's kind: what holding it means.
    fn kind(&self) -> Kind {
        self.instrument().kind
    }

    /// The market as a strategy's session lists it.
    fn info(&self, tradable: bool) -> VenueInfo {
        VenueInfo {
            id: self.instrument().to_string(),
            kind: self.kind().into(),
            address: Some(self.address().to_string()),
            tradable,
        }
    }

    /// The best bid and ask, in quote per base unit; a pool's prices for a
    /// small trade each way.
    async fn top(&self) -> Result<(Option<f64>, Option<f64>)>;

    /// The top `levels` price levels per side at `slot`, where the market
    /// has a book.
    async fn depth(
        &self,
        levels: usize,
        slot: u64,
        unix_timestamp: i64,
    ) -> Result<Option<BookLevels>> {
        let _ = (levels, slot, unix_timestamp);
        Ok(None)
    }

    /// What `owner` holds in this market, as its instrument's profile
    /// counts it: the market's own holding first (a perp's position, a spot
    /// market's base asset), then any other asset the account's value
    /// counts there (a spot market's quote, also a [`Holding::Spot`]).
    /// [`value_usd`] values them.
    async fn holdings(&self, owner: &Pubkey) -> Result<Vec<Holding>>;

    /// Every match the market recorded in slots after `after_slot`, each
    /// with the ids `owner` trades under there (its wallet, and an account
    /// of the venue's of its): a fill naming one of them is its own.
    async fn fills(
        &self,
        after_slot: u64,
        owner: &Pubkey,
    ) -> Result<Vec<(MarketFill, Vec<Pubkey>)>> {
        let _ = (after_slot, owner);
        Ok(Vec::new())
    }

    /// What `holdings`, this market's, are worth in USD, where the market
    /// values its own (a money market's supply and debt, a pool position);
    /// `None` leaves them to the reference prices ([`value_usd`]).
    async fn value_usd(
        &self,
        chain: &dyn ChainClient,
        holdings: &[Holding],
    ) -> Result<Option<Decimal>> {
        let _ = (chain, holdings);
        Ok(None)
    }

    /// An account on this market, signed with `signer`, trading through
    /// `rpc_url`. It must already hold what it trades (setup funds it).
    async fn seat(&self, signer: Keypair, rpc_url: &str) -> Result<Box<dyn Seat>>;

    /// An account on this market, signed with `signer`, reading and sending
    /// through the chain `on` injects: a transaction layer's pool, say. A
    /// market whose seats can't take one refuses.
    async fn seat_on(&self, signer: Keypair, on: SeatChain) -> Result<Box<dyn Seat>> {
        let _ = (signer, on);
        Err(Error::Transaction {
            label: self.instrument().to_string(),
            reason: "this market's seats send through their own client only".into(),
        })
    }
}

/// The chain a seat reads and sends through, when its host injects one
/// ([`Market::seat_on`]).
#[derive(Clone)]
pub struct SeatChain {
    pub chain: std::sync::Arc<dyn ChainClient>,
    /// The chain pools sends rather than landing them: a send reports
    /// [`SendState::Landed`] with its signature at once, which the desk
    /// then follows to its fate, as a perp seat's under the transaction
    /// layer does.
    pub pooled: bool,
    /// Lookup tables every transaction the seat builds may use, besides its
    /// venue's own: a session's table of the accounts its routes name.
    pub lookup_tables: Vec<AddressLookupTableAccount>,
}

/// What a seat answers for a book's method on a market with no book.
pub const NOT_A_BOOK: &str = "unsupported: not an order book";

/// What a seat that sends whole transactions only answers
/// [`Seat::instructions`].
pub const NOT_BUILT: &str = "unsupported: this seat sends whole transactions only";

/// Program log lines a failed simulation's reason keeps, from its end:
/// enough for the failing program's own message and the frames above it.
const FAILURE_LOG_LINES_MAX: usize = 14;

/// One line of a sheet as instructions, not yet a transaction: what
/// [`Seat::instructions`] returns. An atomic group joins several, in sheet
/// order, into one transaction, and adds their compute and lookup tables
/// up; a seat's own send sends one alone ([`send_built`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Built {
    /// The line's instructions, in order, without a compute budget: setup
    /// (token accounts, refreshes) first, then the action.
    pub instructions: Vec<Instruction>,
    /// Lookup tables the instructions' accounts may come from.
    pub lookup_tables: Vec<AddressLookupTableAccount>,
    /// Compute units the instructions may use, at most.
    pub compute_unit_limit: u32,
    /// What the line is, for errors and logs.
    pub label: String,
}

impl Built {
    /// The line as one transaction: a version 0 transaction if it has
    /// lookup tables, else legacy.
    pub fn plan(&self) -> TransactionPlan {
        TransactionPlan::from_group(
            &InstructionGroup::new(&self.label, self.instructions.clone()),
            self.compute_unit_limit,
        )
        .with_lookup_tables(&self.lookup_tables)
    }
}

/// A flash loan for an atomic group, as a lending seat builds it
/// ([`Seat::flash`]): what goes before the group's lines, the borrow last,
/// and the repay that goes after them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Flash {
    /// The setup the borrow needs (the account it lends into), then the
    /// borrow; its lookup tables and compute.
    pub borrow: Built,
    /// The repay, which names the borrow by its index in the transaction.
    pub repay: Vec<Instruction>,
}

/// Sign and send `built` alone as `signer`: simulated first, so a refusal
/// comes back with the program's reason and never reaches the chain.
pub async fn send_built(
    chain: &dyn ChainClient,
    signer: &Keypair,
    built: &Built,
) -> SeatResult<Signature> {
    send_plan(chain, signer, &built.plan()).await
}

/// Sign and send the transaction `plan` as `signer`, simulated first: a
/// line alone ([`send_built`]), or an atomic group's lines joined.
pub async fn send_plan(
    chain: &dyn ChainClient,
    signer: &Keypair,
    plan: &TransactionPlan,
) -> SeatResult<Signature> {
    assert!(
        !plan.instructions.is_empty(),
        "{}: a transaction with nothing to send",
        plan.label()
    );
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
    assert_eq!(tx.signatures.len(), 1, "{}: one signer", plan.label());
    chain.send(&tx).await.map_err(|e| e.to_string())
}

/// Sign and send `plan` as `signer` ([`send_plan`]), and say what became
/// of it: a seat's send, alone or a group's.
pub async fn sent_plan(chain: &dyn ChainClient, signer: &Keypair, plan: &TransactionPlan) -> Sent {
    match send_plan(chain, signer, plan).await {
        Ok(signature) => Sent {
            state: landed(chain, signature).await,
            sent: true,
        },
        Err(reason) => Sent {
            state: SendState::failed(reason),
            sent: false,
        },
    }
}

/// Sign and send `plan` as `signer` ([`send_plan`]) into a pool that lands
/// it later: its signature as [`SendState::Landed`], which the desk follows
/// to its fate, or why it wasn't sent.
pub async fn pooled_plan(
    chain: &dyn ChainClient,
    signer: &Keypair,
    plan: &TransactionPlan,
) -> Sent {
    match send_plan(chain, signer, plan).await {
        Ok(signature) => Sent {
            state: SendState::Landed(signature.to_string()),
            sent: true,
        },
        Err(reason) => Sent {
            state: SendState::failed(reason),
            sent: false,
        },
    }
}

/// [`sent_plan`], or [`pooled_plan`] where `pooled`: a seat's one path to
/// its chain.
pub async fn sent_on(
    chain: &dyn ChainClient,
    signer: &Keypair,
    plan: &TransactionPlan,
    pooled: bool,
) -> Sent {
    if pooled {
        pooled_plan(chain, signer, plan).await
    } else {
        sent_plan(chain, signer, plan).await
    }
}

/// What became of a send the world will land in its next block: landed or
/// failed if the chain already has it, else delayed.
pub async fn landed(chain: &dyn ChainClient, signature: Signature) -> SendState {
    match chain.get_transaction(&signature).await {
        Ok(Some(summary)) => match summary.error {
            None => SendState::Landed(signature.to_string()),
            Some(error) => SendState::Failed {
                reason: error.to_string(),
                signature: Some(signature.to_string()),
            },
        },
        // Not yet in a block, or not readable yet: the step after says.
        Ok(None) | Err(_) => SendState::Delayed,
    }
}

/// The last lines of a failed simulation's logs, joined.
fn tail(logs: &[String]) -> String {
    logs[logs.len().saturating_sub(FAILURE_LOG_LINES_MAX)..].join(" | ")
}

/// One account trading on one market.
#[async_trait]
pub trait Seat: Send + Sync {
    /// The market, as [`Market::info`] names it.
    fn market(&self) -> &str;

    /// Whether [`Seat::send`] executes `action`.
    fn executes(&self, action: &Action) -> bool;

    /// Send one order a sheet planned for this market, and say what became
    /// of it: the bot's, for a component. Its line's options go on it where
    /// the venue has them.
    async fn send(&self, planned: &Planned) -> Sent;

    /// The instructions [`Seat::send`] would send for `planned`, unsent, so
    /// a host can join several lines into one transaction; `Err` says why
    /// the line can't be built. A seat that sends whole transactions only
    /// answers [`NOT_BUILT`].
    async fn instructions(&self, planned: &Planned) -> SeatResult<Built> {
        let _ = planned;
        Err(NOT_BUILT.into())
    }

    /// The flash loan `planned` (an [`Action::Flash`]) asks for, for an
    /// atomic group whose transaction will hold [`Flash::borrow`]'s first
    /// instruction at index `at`, so the repay can name the borrow. A seat
    /// that lends nothing answers [`NOT_BUILT`].
    async fn flash(&self, planned: &Planned, at: u8) -> SeatResult<Flash> {
        let _ = (planned, at);
        Err(NOT_BUILT.into())
    }

    /// Sign and send `plan`, an atomic group's lines joined into one
    /// transaction, through the seat's chain with its key, and say what
    /// became of it. A seat that builds lines ([`Seat::instructions`])
    /// sends them; every seat of a component's session signs with the same
    /// key, so any of the group's seats can.
    async fn send_plan(&self, plan: &TransactionPlan) -> Sent {
        let _ = plan;
        Sent {
            state: SendState::NotSent(NOT_BUILT.into()),
            sent: false,
        }
    }

    /// Take up to `size` base units on `side`, at no worse than `limit`.
    async fn take(&self, side: Side, size: f64, limit: f64) -> SeatResult {
        let _ = (side, size, limit);
        Err(NOT_A_BOOK.into())
    }

    /// Rest `size` base units on `side` at `price`.
    async fn rest(&self, side: Side, price: f64, size: f64) -> SeatResult {
        let _ = (side, price, size);
        Err(NOT_A_BOOK.into())
    }

    /// Cancel every order the seat has resting.
    async fn cancel_all(&self) -> SeatResult {
        Err(NOT_A_BOOK.into())
    }

    /// The best bid and ask, as the seat reads them.
    async fn top(&self) -> SeatResult<(Option<f64>, Option<f64>)> {
        Err(NOT_A_BOOK.into())
    }

    /// The best bid and ask among resting orders of at least `min_size` base
    /// units at their price: a quote smaller than that is an odd lot and
    /// sets no touch. A seat that can't read sizes says what [`Seat::top`]
    /// does.
    async fn top_sized(&self, min_size: f64) -> SeatResult<(Option<f64>, Option<f64>)> {
        let _ = min_size;
        self.top().await
    }

    /// The account's position here, in base units: a perp's, the base asset
    /// a spot account holds, or a money market's supply less its debt.
    async fn position(&self) -> SeatResult<f64>;

    /// The market's tick, in quote units; 0 without a book.
    fn tick(&self) -> f64 {
        0.0
    }

    /// The market's lot, in base units: the smallest size an order can
    /// have; 0 where the seat has none.
    fn lot(&self) -> f64 {
        0.0
    }

    /// The price the market itself refers to, where it has one: a perp's
    /// index.
    async fn index(&self) -> SeatResult<Option<f64>> {
        Ok(None)
    }

    /// The signature of the last transaction it landed, where it keeps it.
    fn last_signature(&self) -> Option<String> {
        None
    }
}

/// An asset's reference price in USD, as the spot profile values it: USDC
/// at 1, any other mint at its Pyth feed.
pub async fn price_usd(chain: &dyn ChainClient, mint: &Pubkey) -> Result<Decimal> {
    if *mint == USDC_MINT {
        return Ok(Decimal::ONE);
    }
    let feed = pyth::feed_for_mint(mint)
        .ok_or_else(|| MeterError::Unsupported(format!("no reference price for {mint}")))?;
    pyth::read_price(chain, &feed)
        .await
        .map_err(|e| Error::Transaction {
            label: format!("the price of {mint}"),
            reason: e.to_string(),
        })
}

/// What each market's holdings are worth in USD: the market's own value
/// where it has one ([`Market::value_usd`]), the reference prices
/// ([`value_usd`]) for the rest.
pub async fn value_of_markets(
    chain: &dyn ChainClient,
    held: &[(&dyn Market, Vec<Holding>)],
) -> Result<Decimal> {
    let mut value = Decimal::ZERO;
    let mut rest = Vec::new();
    for (market, holdings) in held {
        match market.value_usd(chain, holdings).await? {
            Some(own) => value += own,
            None => rest.extend(holdings.iter().cloned()),
        }
    }
    Ok(value + value_usd(chain, &rest).await?)
}

/// What `holdings`, read across a session's markets, are worth in USD at
/// the reference prices ([`price_usd`]). A spot asset counts its wallet
/// once, however many markets report it, and what it has on each venue.
/// Holdings that need their own valuation (a perp's, which the session
/// meters; a money market's) are refused.
pub async fn value_usd(chain: &dyn ChainClient, holdings: &[Holding]) -> Result<Decimal> {
    let mut held: BTreeMap<String, (Decimal, Decimal)> = BTreeMap::new();
    for holding in holdings {
        let Holding::Spot(asset) = holding else {
            return Err(MeterError::Unsupported(format!(
                "valuing {holding:?} in a wallet's account"
            ))
            .into());
        };
        let (wallet, on_venue) = held.entry(asset.base.id.0.clone()).or_default();
        *wallet = asset.wallet;
        *on_venue += asset.free + asset.locked;
    }
    let mut value = Decimal::ZERO;
    for (id, (wallet, on_venue)) in held {
        let mint: Pubkey = id.parse().map_err(|_| {
            MeterError::Unsupported(format!(
                "asset {id} is not a mint, so it has no reference price"
            ))
        })?;
        value += (wallet + on_venue) * price_usd(chain, &mint).await?;
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use mtx_core::{Asset, Holdings, Position};
    use mtx_kit::chain::RpcChainClient;

    use super::*;

    fn spot(symbol: &str) -> InstrumentRef {
        InstrumentRef {
            venue: "phoenix".into(),
            kind: Kind::Spot,
            symbol: symbol.into(),
        }
    }

    #[tokio::test]
    async fn a_venue_offers_instruments_by_kind() {
        let phoenix = spot_venue("phoenix", "http://127.0.0.1:1").unwrap();
        assert_eq!(phoenix.id(), &VenueId::new("phoenix"));
        assert!(phoenix.instruments().contains(&spot("SOL-USDC")));
        // Without the snapshot's exchange, no perps.
        assert!(phoenix.instruments().iter().all(|i| i.kind == Kind::Spot));
        let perp = InstrumentRef {
            kind: Kind::Perp,
            ..spot("SPCX")
        };
        assert!(phoenix.market(&perp).await.is_err());
        let openbook = spot_venue("openbook", "http://127.0.0.1:1").unwrap();
        assert!(
            openbook
                .instruments()
                .iter()
                .all(|i| i.venue == "openbook" && i.kind == Kind::Spot)
        );
        assert!(
            openbook.market(&spot("SOL-USDC")).await.is_err(),
            "Phoenix's"
        );
        assert!(spot_venue("raydium", "http://127.0.0.1:1").is_err());
    }

    #[tokio::test]
    async fn a_wallet_counts_each_asset_once_and_values_it() {
        let usdc = Asset::new(USDC_MINT.to_string(), "USDC", 6).unwrap();
        let held = |wallet, free, locked| {
            Holding::Spot(Holdings {
                wallet,
                free,
                locked,
                ..Holdings::empty(usdc.clone())
            })
        };
        // No read for USDC: nothing answers at this address.
        let chain = RpcChainClient::new("http://127.0.0.1:1");
        let value = value_usd(
            &chain,
            &[
                held(Decimal::from(100), Decimal::from(5), Decimal::from(1)),
                held(Decimal::from(100), Decimal::from(2), Decimal::from(0)),
            ],
        )
        .await
        .unwrap();
        assert_eq!(
            value,
            Decimal::from(108),
            "the wallet once, each venue's own"
        );
        let perp = Holding::Perp(Position::flat(
            mtx_core::InstrumentId::new("phoenix:perp:SPCX"),
            VenueId::new("phoenix"),
        ));
        assert!(value_usd(&chain, &[perp]).await.is_err());
    }
}
