//! Venues and their markets, behind one interface.
//!
//! A [`Venue`] is a protocol, as `mtx_core`'s [`VenueId`] names one
//! (`phoenix`, `openbook`, `kamino`): it offers instruments, each named by
//! the mandate's reference `<venue>:<kind>:<symbol>` ([`InstrumentRef`]).
//! Phoenix offers perpetuals and spot books; OpenBook, spot books. A
//! [`Market`] is one of those instruments as a session trades it: its top,
//! its depth, the matches it recorded, and what an account holds there, as
//! a core [`Holding`]. A [`Seat`] is one account trading a market, signed by
//! the grader: the bot's component's (see [`crate::wasm`]) or an actor's
//! (see [`crate::actors`]).
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
//! | `amm` | `amm` (draft profile) | none yet |
//! | `stake-pool` | `stake-pool` (draft profile) | none yet |
//!
//! A market answers what it can and leaves the rest at the defaults. Its
//! seat says which of the sheet's actions it executes ([`Seat::executes`]);
//! a line it doesn't comes back [`SendState::NotSent`](mtx_kit::SendState).
//! A new venue (Raydium, Meteora, Kamino) is one more [`Venue`] offering its
//! markets: a pool's top is the price of a small trade each way, its fills
//! come from its vaults, and its seat executes swaps, liquidity or
//! deposits.
//!
//! - [`phoenix`]: Phoenix, its Eternal perpetuals ([`perps`]) and its v1
//!   spot books ([`books`]).
//! - [`openbook`]: OpenBook v2 and its spot books ([`books`]).

pub mod books;
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
    Action, Planned, Sent, Side, VenueInfo,
    chain::ChainClient,
    protocols::{USDC_MINT, pyth, spot::MarketFill},
};
use solana_keypair::Keypair;
use solana_pubkey::Pubkey;

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
            reason: format!("{other} is not a spot venue: {:?}", crate::spot::VENUES),
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

    /// An account on this market, signed with `signer`, trading through
    /// `rpc_url`. It must already hold what it trades (setup funds it).
    async fn seat(&self, signer: Keypair, rpc_url: &str) -> Result<Box<dyn Seat>>;
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

    /// Take up to `size` base units on `side`, at no worse than `limit`.
    async fn take(&self, side: Side, size: f64, limit: f64) -> SeatResult;

    /// Rest `size` base units on `side` at `price`.
    async fn rest(&self, side: Side, price: f64, size: f64) -> SeatResult;

    /// Cancel every order the seat has resting.
    async fn cancel_all(&self) -> SeatResult;

    /// The best bid and ask, as the seat reads them.
    async fn top(&self) -> SeatResult<(Option<f64>, Option<f64>)>;

    /// The account's position here, in base units: a perp's, or the base
    /// asset a spot account holds.
    async fn position(&self) -> SeatResult<f64>;

    /// The market's tick, in quote units.
    fn tick(&self) -> f64;

    /// The price the market itself refers to, where it has one: a perp's
    /// index.
    async fn index(&self) -> SeatResult<Option<f64>> {
        Ok(None)
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
