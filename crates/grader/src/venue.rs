//! Venues: the markets a session trades, behind one interface.
//!
//! A [`Venue`] is one market of the session's: a Phoenix perpetual, a spot
//! book, and later a pool, a lending reserve or a stake pool, the market
//! kinds of `mtx:strategy@0.2.0` ([`MarketKind`]). The session loop
//! ([`crate::session`]) reads every venue the same way: its top, its depth,
//! the matches it recorded, and what an account holds there. It trades
//! through a [`Seat`]: one account on one venue, signed by the grader, for
//! the bot's component (see [`crate::wasm`]) or for an actor (see
//! [`crate::actors`]).
//!
//! A venue answers what it can and leaves the rest at the defaults: a perp
//! margins rather than holds tokens, so it has no [`Venue::balances`]; a
//! pool has no depth to list. Its seat says which of the sheet's actions it
//! executes ([`Seat::executes`]); a line it doesn't comes back
//! [`SendState::NotSent`](mtx_kit::SendState). A new venue (a Raydium or
//! Meteora pool, a Kamino reserve) is one more implementation: its top is
//! the price of a small trade each way, its fills come from its vaults, and
//! its seat executes swaps, liquidity or deposits.
//!
//! - [`phoenix_perp`]: Phoenix Eternal's perpetuals.
//! - [`books`]: Phoenix v1 and OpenBook v2 spot books.

pub mod books;
pub mod phoenix_perp;

use async_trait::async_trait;
use mtx_core::{mandate::InstrumentRef, turns::BookLevels};
pub use mtx_kit::MarketKind;
use mtx_kit::{
    Action, Planned, Sent, Side, VenueInfo, core::SpotBalances, protocols::spot::MarketFill,
};
use solana_keypair::Keypair;
use solana_pubkey::Pubkey;

use crate::Result;

/// What a seat's own orders come to: the venue's error, as text.
pub type SeatResult<T = ()> = std::result::Result<T, String>;

/// One market a session trades.
#[async_trait]
pub trait Venue: Send + Sync {
    /// The market, as the mandate's universe names it.
    fn instrument(&self) -> &InstrumentRef;

    fn kind(&self) -> MarketKind;

    /// The market's account: the perp's or the book's market, the pool.
    fn address(&self) -> Pubkey;

    /// The market as a strategy's session lists it.
    fn info(&self, tradable: bool) -> VenueInfo {
        VenueInfo {
            id: self.instrument().to_string(),
            kind: self.kind(),
            address: Some(self.address().to_string()),
            tradable,
        }
    }

    /// The best bid and ask, in quote per base unit; a pool's prices for a
    /// small trade each way.
    async fn top(&self) -> Result<(Option<f64>, Option<f64>)>;

    /// The top `levels` price levels per side at `slot`, where the venue
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

    /// What `owner` holds here, its wallet included, where the venue holds
    /// tokens for it; `None` where it margins instead (a perp, whose
    /// account the session meters).
    async fn balances(&self, owner: &Pubkey) -> Result<Option<SpotBalances>> {
        let _ = owner;
        Ok(None)
    }

    /// Every match the venue recorded in slots after `after_slot`, each
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

    /// An account on this venue, signed with `signer`, trading through
    /// `rpc_url`. It must already hold what it trades (setup funds it).
    async fn seat(&self, signer: Keypair, rpc_url: &str) -> Result<Box<dyn Seat>>;
}

/// One account trading on one venue.
#[async_trait]
pub trait Seat: Send + Sync {
    /// The venue's market, as [`Venue::info`] names it.
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
}
