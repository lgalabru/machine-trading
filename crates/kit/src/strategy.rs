//! A trading strategy, and what it decides with: write a [`Strategy`], and
//! the same code runs as a component the grader calls
//! (`docs/rfd/0002-wasm-strategies.md`; feature `component`, built for
//! `wasm32-wasip2`) and as a process ([`run`](crate::run), feature
//! `chain`), for practice and tests.
//!
//! ```
//! use mtx_kit::prelude::*;
//!
//! #[derive(Default)]
//! struct FadeTheDip {
//!     peak: f64,
//! }
//!
//! impl Strategy for FadeTheDip {
//!     fn step(&mut self, market: &Market, orders: &mut Orders) -> BotResult {
//!         self.peak = self.peak.max(market.mark);
//!         if market.mark < self.peak * 0.97 {
//!             orders.target_position(market.cap);
//!         } else if market.mark >= self.peak {
//!             orders.target_position(0.0);
//!         }
//!         Ok(())
//!     }
//! }
//! ```
//!
//! - [`Strategy::step`] gets a [`Market`]: a plain value with the step, the
//!   prices, the position and the mandate's room, read for it before the
//!   call. It fills in [`Orders`], a sheet of intentions such as "be at this
//!   position" or "quote these prices", which are turned into transactions
//!   after the call, inside the mandate's position cap ([`plan`]).
//! - The strategy is synchronous. For what the market doesn't carry, [`rpc`]
//!   reads the chain as it is at this step: the world waits for the call.
//! - Because a market is a plain value and a sheet records what it's asked,
//!   a strategy can be tested without a chain: build markets and call
//!   [`step_offline`].

use std::error::Error as StdError;

use crate::Mandate;

/// What a strategy's hooks return: any error, so a strategy can use `?` on
/// whatever it calls.
pub type BotResult<T = ()> = std::result::Result<T, Box<dyn StdError + Send + Sync>>;

/// A trading strategy. Implement [`Strategy::step`]; the other hooks are
/// optional. Synchronous: the world is paused while a hook runs.
pub trait Strategy {
    /// Once, before the first step: the session's clock, mandate and market.
    fn start(&mut self, session: &Session) -> BotResult {
        let _ = session;
        Ok(())
    }

    /// Every step: decide, from the market as it is now, and fill in the
    /// order sheet. It is sent when this returns. An error skips this step's
    /// orders, and the session goes on.
    fn step(&mut self, market: &Market, orders: &mut Orders) -> BotResult;

    /// After the last step, when the session is over. The account is valued
    /// at the last step: there is nothing left to send.
    fn end(&mut self, last: Option<&Market>) {
        let _ = last;
    }
}

/// The session, as [`Strategy::start`] sees it.
#[derive(Debug, Clone, PartialEq)]
pub struct Session {
    /// Steps in the session, and slots per step.
    pub steps: u64,
    pub slots_per_step: u64,
    /// The slots the session starts and ends at.
    pub start_slot: u64,
    pub end_slot: u64,
    /// The market traded, as the venue names it (`SPCX`).
    pub symbol: String,
    /// The mark and the oracle when the session starts.
    pub start_mark: f64,
    pub start_oracle: f64,
    /// The mandate's position cap, in base units, and its drawdown limit.
    pub cap: f64,
    pub max_drawdown: f64,
    /// The mandate, as the grader audits it.
    pub mandate: Mandate,
    /// The venue's metadata, as it publishes it: for Phoenix, its
    /// `/exchange` response, the file `MTX_EXCHANGE` names (read it with
    /// `mtx_kit::venue::PhoenixVenue`, feature `venue`).
    pub venue: String,
    /// The bot's authority: the key its orders are signed with, and its
    /// trader account's owner, base58.
    pub authority: String,
    /// The task's variables for the session, as a process bot gets them in
    /// its environment: one of [`SESSION_VARS`]. Read them with
    /// [`Session::var`].
    pub vars: Vec<(String, String)>,
    /// A seed, for a strategy that wants randomness: the same in every run
    /// of the session, as a component or as a process.
    pub seed: u64,
    /// The markets of the mandate's universe, the session's own first: a
    /// line of the sheet that names no venue is for it.
    pub venues: Vec<VenueInfo>,
    /// What the session's steps report beyond prices.
    pub reports: Reports,
    /// Whether the session applies the sheet's [`TxOptions`]. None does yet:
    /// they are accepted and counted, and every order is sent as it would be
    /// without them.
    pub applies_tx_options: bool,
}

/// What a session's steps report beyond prices: the venues' tapes and
/// depth, the world's clock, and the fee market. Where the tape is reported,
/// an empty one is a step without trades.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Reports {
    pub tape: bool,
    pub depth: bool,
    pub timestamps: bool,
    pub fees: bool,
}

impl From<Option<mtx_core::turns::Reports>> for Reports {
    fn from(reports: Option<mtx_core::turns::Reports>) -> Self {
        let reports = reports.unwrap_or_default();
        Reports {
            tape: reports.tape,
            depth: reports.depth,
            timestamps: reports.timestamps,
            fees: false,
        }
    }
}

/// What kind of market a venue is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MarketKind {
    /// A perpetual future on an order book (Phoenix Eternal).
    Perp,
    /// A spot order book (Phoenix v1, OpenBook v2).
    Spot,
    /// A pool: constant product, concentrated liquidity or bins.
    Amm,
    /// A lending reserve.
    Lending,
    /// A stake pool, and its liquid staking token.
    StakePool,
}

impl From<mtx_core::mandate::Kind> for MarketKind {
    fn from(kind: mtx_core::mandate::Kind) -> Self {
        match kind {
            mtx_core::mandate::Kind::Perp => MarketKind::Perp,
            mtx_core::mandate::Kind::Spot => MarketKind::Spot,
            mtx_core::mandate::Kind::Amm => MarketKind::Amm,
        }
    }
}

/// A market the session knows.
#[derive(Debug, Clone, PartialEq)]
pub struct VenueInfo {
    /// The market's id, unique in the session: `<venue>:<kind>:<symbol>`,
    /// as the mandate's universe writes it (`phoenix:perp:SPCX`), and
    /// `@<address>` after it where two of the session's markets would
    /// otherwise share it (two pools of one pair).
    pub id: String,
    pub kind: MarketKind,
    /// The market's account, base58: the perp's or the book's market, the
    /// pool, the lending reserve, the stake pool.
    pub address: Option<String>,
    /// Whether the session can send orders there. A line for a venue it
    /// can't comes back as [`SendState::NotSent`].
    pub tradable: bool,
}

impl VenueInfo {
    /// The venue's name: `phoenix` in `phoenix:perp:SPCX`.
    pub fn venue(&self) -> &str {
        self.id.split(':').next().unwrap_or_default()
    }

    /// A perp session's venues: its own market, `symbol`'s perp in the
    /// mandate's universe (`phoenix:perp:<symbol>` if it names none), at
    /// `address`, which is the one it trades; and the universe's others.
    pub fn of_perp_session(
        mandate: &Mandate,
        symbol: &str,
        address: Option<String>,
    ) -> Vec<VenueInfo> {
        use mtx_core::mandate::{InstrumentRef, Kind};
        let own = mandate
            .universe
            .iter()
            .map(|allowance| &allowance.instrument)
            .find(|i| i.kind == Kind::Perp && i.symbol == symbol)
            .cloned()
            .unwrap_or_else(|| InstrumentRef {
                venue: "phoenix".into(),
                kind: Kind::Perp,
                symbol: symbol.into(),
            });
        let mut venues = Self::of_mandate(mandate, &own, |instrument| *instrument == own);
        venues[0].address = address;
        venues
    }

    /// The mandate's universe, once each, with `first` (the session's own
    /// market) first, and `tradable` saying which the session can trade.
    pub fn of_mandate(
        mandate: &Mandate,
        first: &mtx_core::mandate::InstrumentRef,
        tradable: impl Fn(&mtx_core::mandate::InstrumentRef) -> bool,
    ) -> Vec<VenueInfo> {
        let mut instruments = vec![first];
        for allowance in &mandate.universe {
            if !instruments.contains(&&allowance.instrument) {
                instruments.push(&allowance.instrument);
            }
        }
        instruments
            .into_iter()
            .map(|instrument| VenueInfo {
                id: instrument.to_string(),
                kind: instrument.kind.into(),
                address: None,
                tradable: tradable(instrument),
            })
            .collect()
    }
}

/// The market account of `symbol` in Phoenix's `/exchange` metadata
/// ([`Session::venue`]), if it names one.
pub fn phoenix_market_address(exchange: &str, symbol: &str) -> Option<String> {
    let exchange: serde_json::Value = serde_json::from_str(exchange).ok()?;
    exchange["markets"]
        .as_array()?
        .iter()
        .find(|market| market["symbol"] == symbol)?["marketPubkey"]
        .as_str()
        .map(str::to_string)
}

/// The variables a task may set for a session, beyond the bot's contract:
/// the slot an off-hours market reopens at; the session's regime and the
/// rung a reference plays, which only the benchmark's ladder hands a
/// process bot; and, for a component in a session that enables raw sends,
/// the programs they may run, base58, comma-separated.
pub const SESSION_VARS: &[&str] = &[
    "MTX_REOPEN_SLOT",
    "MTX_REGIME",
    "MTX_RUNG",
    "MTX_RAW_PROGRAMS",
];

impl Session {
    /// A task variable for the session ([`SESSION_VARS`]), if it set one.
    pub fn var(&self, name: &str) -> Option<&str> {
        self.vars
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    }
}

/// An order's side.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Side {
    /// Buy.
    Bid,
    /// Sell.
    Ask,
}

impl Side {
    /// The other side.
    pub fn opposite(self) -> Side {
        match self {
            Side::Bid => Side::Ask,
            Side::Ask => Side::Bid,
        }
    }
}

/// A position in base units (positive is long) and its average entry in USD.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Position {
    pub size: f64,
    pub entry_usd: f64,
}

/// The RPC methods a strategy may call through [`rpc`]: reads of the chain
/// as it is. Nothing that writes, and nothing that depends on wall time or
/// the node's own state. The grader answers the same list.
pub const RPC_METHODS: &[&str] = &[
    "getAccountInfo",
    "getBalance",
    "getBlock",
    "getBlockHeight",
    "getMultipleAccounts",
    "getProgramAccounts",
    "getSignaturesForAddress",
    "getSlot",
    "getTokenAccountBalance",
    "getTokenAccountsByOwner",
    "getTransaction",
    "simulateTransaction",
];

/// The router's read-only methods, in a session that has one: Jupiter's
/// v1 quote and swap instructions, each taking the same JSON object as its
/// first parameter (`specs/venues/router.md`). A session without a router
/// refuses them.
pub const ROUTER_METHODS: &[&str] = &["mtx_routerQuote", "mtx_routerSwapInstructions"];

/// A read-only JSON-RPC call against the session's chain: `params` and the
/// result are JSON text, and `method` one of [`RPC_METHODS`] (or
/// [`ROUTER_METHODS`]). Synchronous: it returns before the world moves. In a
/// component the grader answers it; in a process, the session's endpoint
/// does.
pub fn rpc(method: &str, params: &str) -> Result<String, String> {
    if !RPC_METHODS.contains(&method) && !ROUTER_METHODS.contains(&method) {
        return Err(format!("{method} is not a read a strategy may make"));
    }
    rpc_backend(method, params)
}

/// A line in the strategy's log, which the grader keeps beside the session:
/// through the grader in a component, on standard error in a process.
pub fn log(line: &str) {
    #[cfg(all(feature = "component", target_arch = "wasm32"))]
    crate::component::bindings::log(line);
    #[cfg(not(all(feature = "component", target_arch = "wasm32")))]
    eprintln!("{line}");
}

/// The data of the account at `address` (base58), read through [`rpc`]:
/// `None` if there is no such account.
pub fn rpc_account(address: &str) -> Result<Option<Vec<u8>>, String> {
    let answer = rpc(
        "getAccountInfo",
        &serde_json::json!([address, { "encoding": "base64" }]).to_string(),
    )?;
    let answer: serde_json::Value = serde_json::from_str(&answer).map_err(|e| e.to_string())?;
    account_data(&answer["value"])
}

/// [`rpc_account`] for several accounts in one call, in order.
pub fn rpc_accounts(addresses: &[&str]) -> Result<Vec<Option<Vec<u8>>>, String> {
    let answer = rpc(
        "getMultipleAccounts",
        &serde_json::json!([addresses, { "encoding": "base64" }]).to_string(),
    )?;
    let answer: serde_json::Value = serde_json::from_str(&answer).map_err(|e| e.to_string())?;
    answer["value"]
        .as_array()
        .ok_or("getMultipleAccounts answered no list")?
        .iter()
        .map(account_data)
        .collect()
}

/// An account's data from an RPC answer's `value`, base64.
fn account_data(value: &serde_json::Value) -> Result<Option<Vec<u8>>, String> {
    if value.is_null() {
        return Ok(None);
    }
    let encoded = value["data"][0]
        .as_str()
        .ok_or("an account without base64 data")?;
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .map(Some)
        .map_err(|e| format!("account data: {e}"))
}

#[cfg(all(feature = "component", target_arch = "wasm32"))]
fn rpc_backend(method: &str, params: &str) -> Result<String, String> {
    crate::component::bindings::rpc(method, params)
}

#[cfg(all(
    feature = "chain",
    not(all(feature = "component", target_arch = "wasm32"))
))]
fn rpc_backend(method: &str, params: &str) -> Result<String, String> {
    crate::run::process_rpc(method, params)
}

#[cfg(not(any(feature = "chain", all(feature = "component", target_arch = "wasm32"))))]
fn rpc_backend(method: &str, _params: &str) -> Result<String, String> {
    Err(format!("{method}: no chain to read here"))
}

/// The market at one step, read for the strategy before [`Strategy::step`].
///
/// A plain value: tests build their own, with the fields they care about and
/// `..Market::default()`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Market {
    /// The step, the slot the chain is at, and whether it's the last step.
    pub k: u64,
    pub slot: u64,
    pub last: bool,
    /// Steps and slots until the session ends.
    pub steps_left: u64,
    pub slots_left: u64,
    /// The venue's mark price, which values the account, and its oracle.
    pub mark: f64,
    pub oracle: f64,
    /// The venue's index price, and the book's best bid and ask, if any: the
    /// same read as the mark.
    pub index: f64,
    pub best_bid: Option<f64>,
    pub best_ask: Option<f64>,
    /// The position, in base units (positive is long), and its entry.
    pub position: Position,
    /// The mandate's position cap and drawdown limit.
    pub cap: f64,
    pub max_drawdown: f64,
    /// The bids and asks the run loop has resting, in base units: every one
    /// it placed since its last cancel, filled or not. An upper bound, so
    /// that [`plan`] can keep the position within the cap even if they all
    /// fill: a fill already in `position` is counted again until the next
    /// cancel, which only makes the sheet cut sooner.
    pub resting_bids: f64,
    pub resting_asks: f64,
    /// The account's PnL since the first step, marked to market: the
    /// position held times each step's move in the mark. An estimate: it
    /// leaves out fees, funding and slippage, which the grader counts.
    pub pnl: f64,
    /// The highest `pnl` so far, and how far `pnl` is below it.
    pub peak_pnl: f64,
    pub drawdown: f64,
    /// The step's turn data, when the scenario reports it (spot sessions):
    /// every trade since the previous step on every venue, the top of each
    /// book, and the world's clock in Unix seconds. [`Market::venues`] has
    /// the same trades and books, by venue.
    pub tape: Vec<mtx_core::turns::Trade>,
    pub depth: Vec<mtx_core::turns::BookLevels>,
    pub unix_timestamp: Option<i64>,
    /// Each of the session's venues ([`Session::venues`], in its order): its
    /// book, its tape, and the position held there.
    pub venues: Vec<VenueView>,
    /// What became of every order the previous step's sheet planned.
    pub sends: Vec<SendStatus>,
    /// The strategy's own trades since the previous step, from the venues'
    /// tapes: none where the scenario reports no tape.
    pub fills: Vec<OwnFill>,
    /// Requests for quotes open at this step.
    pub rfqs: Vec<Rfq>,
    /// What the planner cut or dropped from the previous step's sheet, and
    /// why ([`plan_sheet`]'s notes).
    pub notes: Vec<String>,
    /// The fee market, where the session reports it.
    pub fees: Option<FeeView>,
}

/// A venue at one step.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct VenueView {
    /// As [`VenueInfo::id`] names it.
    pub id: String,
    /// The book's best bid and ask, or a pool's prices for a small trade
    /// each way.
    pub best_bid: Option<f64>,
    pub best_ask: Option<f64>,
    /// The top levels of the book, best first, where the session reports
    /// depth.
    pub depth: Option<Book>,
    /// Every trade there since the previous step, oldest first, where the
    /// session reports a tape: `Some` and empty is a step without trades.
    pub tape: Option<Vec<mtx_core::turns::Trade>>,
    /// The strategy's position there, if the session reads it.
    pub position: Option<Position>,
    /// Its orders resting there, as far as the session reads them.
    pub orders: Vec<OpenOrder>,
    /// Its liquidity positions there (a pool).
    pub liquidity: Vec<LiquidityPosition>,
    /// Its supply and debt there (a lending reserve).
    pub lending: Option<LendingPosition>,
    /// The venue's own numbers, by name: a pool's price and liquidity, a
    /// reserve's utilization and rates, a stake pool's exchange rate.
    pub stats: Vec<(String, f64)>,
}

/// One of the strategy's orders resting on a book.
#[derive(Debug, Clone, PartialEq)]
pub struct OpenOrder {
    pub client_id: Option<u64>,
    pub side: Side,
    pub price: f64,
    /// Base units left.
    pub size: f64,
}

/// A price range, in quote units per base unit.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PriceRange {
    pub lower: f64,
    pub upper: f64,
}

/// One of the strategy's liquidity positions in a pool.
#[derive(Debug, Clone, PartialEq)]
pub struct LiquidityPosition {
    /// What [`Intent::RemoveLiquidity`] names it by.
    pub id: String,
    /// Where it provides liquidity; `None` for the whole curve.
    pub range: Option<PriceRange>,
    /// What it holds now, and its uncollected fees.
    pub base: f64,
    pub quote: f64,
    pub fees_base: f64,
    pub fees_quote: f64,
}

/// The strategy's account at a lending reserve, in the reserve's asset.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LendingPosition {
    pub supplied: f64,
    pub borrowed: f64,
    /// The obligation's liquidation threshold over its debt: liquidatable
    /// below 1. `None` without debt.
    pub health: Option<f64>,
}

/// The fee market, where the session models one.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct FeeView {
    /// Recent priority fees, micro-lamports per compute unit, as
    /// `(percentile, price)`.
    pub cu_prices: Vec<(u8, u64)>,
    /// The tip floor for a bundle, in lamports.
    pub tip_floor: Option<u64>,
    /// The next slots' leaders, in order.
    pub leaders: Vec<String>,
}

/// The session's venues at one step, and the strategy's fills, from what
/// the run loop or the grader read: the top of the session's own market
/// and the position there, and the step's turn data, which other venues'
/// views come from.
///
/// A tape entry or a book belongs to the venue its `market` names; one
/// that names none, to the only venue of its venue's name, and to none if
/// two share it. A venue's tape is `Some` (maybe empty) where the session
/// reports a tape; its depth, where it reports depth and has levels there.
#[doc(hidden)]
pub fn venue_views(
    venues: &[VenueInfo],
    reports: Reports,
    best_bid: Option<f64>,
    best_ask: Option<f64>,
    position: Position,
    tape: &[mtx_core::turns::Trade],
    depth: &[mtx_core::turns::BookLevels],
) -> (Vec<VenueView>, Vec<OwnFill>) {
    let belongs = |venue: &VenueInfo, market: &Option<String>, name: &str| match market {
        Some(market) => *market == venue.id,
        None => name == venue.venue() && venues.iter().filter(|v| v.venue() == name).count() == 1,
    };
    let mut fills = Vec::new();
    let views = venues
        .iter()
        .enumerate()
        .map(|(i, venue)| {
            let levels = depth
                .iter()
                .find(|d| belongs(venue, &d.market, &d.venue))
                .filter(|_| reports.depth);
            let trades: Option<Vec<_>> = reports.tape.then(|| {
                tape.iter()
                    .filter(|t| belongs(venue, &t.market, &t.venue))
                    .cloned()
                    .collect()
            });
            fills.extend(
                trades
                    .iter()
                    .flatten()
                    .filter_map(|t| OwnFill::of_trade(&venue.id, t)),
            );
            let own = i == 0;
            VenueView {
                id: venue.id.clone(),
                best_bid: if own {
                    best_bid
                } else {
                    levels.and_then(|l| l.bids.first()).map(|l| l.price)
                },
                best_ask: if own {
                    best_ask
                } else {
                    levels.and_then(|l| l.asks.first()).map(|l| l.price)
                },
                depth: levels.map(|l| Book {
                    bids: l.bids.clone(),
                    asks: l.asks.clone(),
                }),
                tape: trades,
                position: own.then_some(position),
                ..VenueView::default()
            }
        })
        .collect();
    (views, fills)
}

/// The top of a book, best first.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Book {
    pub bids: Vec<mtx_core::turns::Level>,
    pub asks: Vec<mtx_core::turns::Level>,
}

/// Which side of a trade the strategy was on.
pub use mtx_core::turns::Role;

/// One of the strategy's own fills.
#[derive(Debug, Clone, PartialEq)]
pub struct OwnFill {
    pub venue: String,
    pub slot: u64,
    /// The strategy's side: [`Side::Bid`] bought.
    pub side: Side,
    pub price: f64,
    pub size: f64,
    pub role: Role,
}

impl OwnFill {
    /// The strategy's fill in a trade of the tape, if it was in it.
    pub fn of_trade(venue: &str, trade: &mtx_core::turns::Trade) -> Option<OwnFill> {
        let role = trade.own?;
        let taker = match trade.side {
            mtx_core::Side::Buy => Side::Bid,
            mtx_core::Side::Sell => Side::Ask,
        };
        Some(OwnFill {
            venue: venue.to_string(),
            slot: trade.slot,
            side: match role {
                Role::Taker => taker,
                Role::Maker => taker.opposite(),
            },
            price: trade.price,
            size: trade.base,
            role,
        })
    }
}

/// A request for a quote.
#[derive(Debug, Clone, PartialEq)]
pub struct Rfq {
    /// The request's id, which a response names ([`Orders::respond`]).
    pub id: String,
    pub venue: String,
    /// The requester's side: [`Side::Bid`] wants to buy, so a quote
    /// answering it sells. `None` asks for both sides.
    pub side: Option<Side>,
    /// Base units wanted.
    pub size: f64,
    /// The last slot a response is taken at.
    pub deadline_slot: u64,
}

/// Where an order came from: a line of the sheet, or a raw transaction
/// (reserved for the grader-signed `send`), numbered from 0 in its step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SendSource {
    Line(u32),
    Raw(u32),
}

/// An order the planner made of a line of the sheet, and what became of it.
#[derive(Debug, Clone, PartialEq)]
pub struct SendStatus {
    /// The step that planned it, and where it came from.
    pub step: u64,
    pub source: SendSource,
    /// The venue it went to, as [`VenueInfo::id`] names it; empty for a raw
    /// transaction.
    pub venue: String,
    pub client_id: Option<u64>,
    /// The order as planned; `None` for a raw transaction.
    pub order: Option<Action>,
    /// The order's expiry, as its line set it.
    pub expires_slot: Option<u64>,
    pub state: SendState,
    /// The slot it landed in, its fee in lamports, and its tip, where the
    /// session knows them.
    pub slot: Option<u64>,
    pub fee: Option<u64>,
    pub tip: Option<u64>,
}

/// What became of an order.
#[derive(Debug, Clone, PartialEq)]
pub enum SendState {
    /// It executed: its signature.
    Landed(String),
    /// It was sent, or checked, and failed: why, and, if it ran on chain,
    /// its signature.
    Failed {
        reason: String,
        signature: Option<String>,
    },
    /// It wasn't sent: why.
    NotSent(String),
    /// It is held, and may land in a later step.
    Delayed,
    /// Its blockhash expired before it landed.
    Expired,
}

impl SendState {
    /// Whether it executed.
    pub fn landed(&self) -> bool {
        matches!(self, SendState::Landed(_))
    }

    /// Why it failed or wasn't sent, if it did either.
    pub fn reason(&self) -> Option<&str> {
        match self {
            SendState::Failed { reason, .. } | SendState::NotSent(reason) => Some(reason),
            SendState::Landed(_) => None,
            SendState::Delayed => Some("delayed"),
            SendState::Expired => Some("expired"),
        }
    }

    /// A failure that didn't reach the chain.
    pub fn failed(reason: impl Into<String>) -> Self {
        SendState::Failed {
            reason: reason.into(),
            signature: None,
        }
    }
}

impl Market {
    /// How much more the bot may buy before its position could reach the
    /// cap: counting its resting bids as filled.
    pub fn room_to_buy(&self) -> f64 {
        self.room().0
    }

    /// How much more it may sell before its position could reach minus the
    /// cap: counting its resting asks as filled.
    pub fn room_to_sell(&self) -> f64 {
        self.room().1
    }

    /// `(room to buy, room to sell)`, by the mandate's own rule
    /// ([`mtx_core::mandate::position_room`]).
    fn room(&self) -> (f64, f64) {
        mtx_core::mandate::position_room(
            self.cap,
            self.position.size,
            self.resting_bids,
            self.resting_asks,
        )
    }

    /// How much further the account may draw down before the mandate's limit,
    /// on [`Market::drawdown`]'s estimate.
    pub fn drawdown_room(&self) -> f64 {
        (self.max_drawdown - self.drawdown).max(0.0)
    }
}

impl Default for Position {
    fn default() -> Self {
        Position {
            size: 0.0,
            entry_usd: 0.0,
        }
    }
}

/// How [`Orders::target_position`] trades toward its target.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Execution {
    /// The most to trade in one step; the rest waits for the next steps.
    pub max_per_step: f64,
    /// The size of each market order: one that crosses much of the book can
    /// exceed a transaction's compute budget.
    pub slice: f64,
    /// How far from the mark each market order may fill, in basis points.
    pub slippage_bps: f64,
    /// Differences smaller than this are left alone.
    pub min_trade: f64,
}

impl Default for Execution {
    fn default() -> Self {
        Execution {
            max_per_step: f64::INFINITY,
            slice: 10.0,
            slippage_bps: 200.0,
            min_trade: 1.0,
        }
    }
}

/// One thing the strategy asked for, as [`Orders`] records it.
#[derive(Debug, Clone, PartialEq)]
pub enum Intent {
    /// Trade toward this position, with market orders.
    Target {
        position: f64,
        execution: Execution,
    },
    /// Cancel every resting order, then rest a bid and an ask of `size`.
    Quote {
        bid: f64,
        ask: f64,
        size: f64,
    },
    /// A market order of `size`, filling at no worse than `limit`.
    Market {
        side: Side,
        size: f64,
        limit: f64,
    },
    /// A resting order of `size` at `price`.
    Limit {
        side: Side,
        price: f64,
        size: f64,
    },
    /// Cancel every resting order.
    CancelAll,
    /// Cancel the resting order the strategy gave this client id.
    Cancel {
        client_id: u64,
    },
    /// Answer the request for a quote `id` ([`Rfq`]): a price for each side
    /// it answers, for `size` or the whole request.
    Respond {
        id: String,
        bid: Option<f64>,
        ask: Option<f64>,
        size: Option<f64>,
    },
    /// Swap against a pool: buy ([`Side::Bid`]) or sell the base asset,
    /// `amount` of it or of the quote asset, at no worse than `limit`.
    Swap {
        side: Side,
        amount: f64,
        unit: AmountUnit,
        limit: f64,
    },
    /// Provide up to `base` and `quote` to a pool, over `range` or the whole
    /// curve: to the liquidity position `position`, or a new one; spread as
    /// `shape` in a pool of bins.
    AddLiquidity {
        base: f64,
        quote: f64,
        range: Option<PriceRange>,
        position: Option<String>,
        shape: Option<LiquidityShape>,
    },
    /// Take `share` (in [0, 1]) of a liquidity position out and collect its
    /// fees ([`LiquidityPosition::id`]; `None` for the only one): a share of
    /// 0 collects the fees only. At least `min_base` and `min_quote`, if
    /// given, fees included.
    RemoveLiquidity {
        position: Option<String>,
        share: f64,
        min_base: Option<f64>,
        min_quote: Option<f64>,
    },
    /// Supply to a lending reserve, or stake into a stake pool.
    Deposit {
        amount: Option<f64>,
    },
    /// Take supply out, or unstake: `None` is all of it.
    Withdraw {
        amount: Option<f64>,
    },
    Borrow {
        amount: Option<f64>,
    },
    /// Repay debt: `None` is all of it.
    Repay {
        amount: Option<f64>,
    },
    /// Repay `repay` of `obligation`'s debt at this reserve, and take its
    /// collateral at the reserve `collateral` (a venue id): at least
    /// `min_received` of it, if given.
    Liquidate {
        obligation: String,
        collateral: String,
        repay: f64,
        min_received: Option<f64>,
    },
}

/// How liquidity spreads over a range of bins (Meteora DLMM's strategies).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LiquidityShape {
    /// Evenly.
    Spot,
    /// Peaked at the price.
    Curve,
    /// Heavier away from the price.
    BidAsk,
}

/// What a swap's amount counts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AmountUnit {
    Base,
    Quote,
}

/// How a limit order, or a quote, lives.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum TimeInForce {
    /// Rests until it fills or is cancelled, filling first against anything
    /// it crosses.
    #[default]
    Gtc,
    /// Rests without taking: one that would cross the book isn't placed,
    /// and comes back [`SendState::Failed`].
    PostOnly,
    /// Fills what it can now, at no worse than its price, and rests nothing.
    Ioc,
}

/// Where a transaction is sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Route {
    /// The RPC node, as today.
    Rpc,
    /// A staked connection.
    Staked,
    /// A block engine, as a bundle: never outside one if `bundle_only`, and
    /// not in front of the strategy's other transactions if `dont_front`.
    Jito { bundle_only: bool, dont_front: bool },
}

/// How a line's transactions are sent: a priority fee, a tip, a bundle, a
/// route, retries. Every sheet may carry them; a session applies them only
/// if it says so ([`Session::applies_tx_options`]), and none does yet.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TxOptions {
    /// Compute units the transaction asks for; `None` is the simulation's
    /// units plus a margin.
    pub cu_limit: Option<u32>,
    /// The priority fee: micro-lamports per compute unit.
    pub cu_price: Option<u64>,
    /// A tip to the block builder, in lamports.
    pub tip: Option<u64>,
    /// Lines with the same bundle id are sent as one bundle.
    pub bundle: Option<u32>,
    pub route: Option<Route>,
    /// Send the same transaction again each slot until this one, if it
    /// hasn't landed; once its blockhash expires, sign it again with a new
    /// one, after checking it didn't land.
    pub retry_until_slot: Option<u64>,
}

/// What a line of the sheet says beyond its intent. The default is the
/// session's own market, orders that rest until cancelled, and no other
/// limit than the cap.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct LineOptions {
    /// One of [`Session::venues`]' ids; `None` is the first, the session's
    /// own market.
    pub venue: Option<String>,
    /// The strategy's own id for the line's orders: echoed in their status,
    /// and what [`Intent::Cancel`] names.
    pub client_id: Option<u64>,
    /// For a limit order or a quote.
    pub time_in_force: TimeInForce,
    /// Orders the line plans may only shrink the position, never grow or
    /// flip it, even if the sheet's earlier orders don't fill.
    pub reduce_only: bool,
    /// For a target: the most of the venue's traded volume since the
    /// previous step to trade this step, a share in (0, 1].
    pub participation: Option<f64>,
    /// For a limit order or a quote, the last slot it rests in; for a
    /// response to a request for a quote, the last slot it can be taken at.
    pub expires_slot: Option<u64>,
    pub tx: Option<TxOptions>,
}

/// The most market orders one target sends in a step: a target sliced
/// finer trades the rest at the next step.
pub const MAX_TARGET_ORDERS: usize = 64;

/// The options of a line that says nothing more.
const DEFAULT_OPTIONS: LineOptions = LineOptions {
    venue: None,
    client_id: None,
    time_in_force: TimeInForce::Gtc,
    reduce_only: false,
    participation: None,
    expires_slot: None,
    tx: None,
};

/// The step's order sheet: what the strategy wants, which the grader (or
/// the process run loop) cuts to the mandate's cap and sends after
/// [`Strategy::step`] returns.
///
/// Each call adds a line; [`Orders::at`], [`Orders::client_id`],
/// [`Orders::post_only`], [`Orders::ioc`], [`Orders::reduce_only`],
/// [`Orders::participation`] and [`Orders::tx`] change the line added last:
/// `orders.bid(99.0, 5.0).post_only()`.
#[derive(Debug, Clone, Default)]
pub struct Orders {
    intents: Vec<Intent>,
    options: Vec<LineOptions>,
}

impl Orders {
    /// An empty sheet.
    pub fn new() -> Self {
        Self::default()
    }

    /// What the sheet holds, taken out of it, without the lines' options.
    pub fn into_intents(self) -> Vec<Intent> {
        self.intents
    }

    /// Every line, with its options, taken out of the sheet.
    pub fn into_lines(self) -> Vec<(Intent, LineOptions)> {
        self.intents.into_iter().zip(self.options).collect()
    }

    /// Every line, with its options.
    pub fn lines(&self) -> impl Iterator<Item = (&Intent, &LineOptions)> {
        self.intents.iter().zip(&self.options)
    }

    /// Add a line.
    pub fn push(&mut self, intent: Intent, options: LineOptions) -> &mut Self {
        self.intents.push(intent);
        self.options.push(options);
        self
    }

    fn add(&mut self, intent: Intent) -> &mut Self {
        self.push(intent, LineOptions::default())
    }

    /// The line added last, changed by `change`. On an empty sheet, nothing.
    fn last(&mut self, change: impl FnOnce(&mut LineOptions)) -> &mut Self {
        if let Some(options) = self.options.last_mut() {
            change(options);
        }
        self
    }

    /// Be at `position` (base units, positive long), trading toward it with
    /// market orders, as [`Execution::default`] says. The run loop keeps the
    /// target within the mandate's cap.
    pub fn target_position(&mut self, position: f64) -> &mut Self {
        self.target_position_with(position, Execution::default())
    }

    /// [`Orders::target_position`], with its own pacing.
    pub fn target_position_with(&mut self, position: f64, execution: Execution) -> &mut Self {
        self.add(Intent::Target {
            position,
            execution,
        })
    }

    /// Replace the resting quotes with a bid and an ask of `size` each, each
    /// cut to what can fill without taking the position past the cap.
    pub fn quote(&mut self, bid: f64, ask: f64, size: f64) -> &mut Self {
        self.add(Intent::Quote { bid, ask, size })
    }

    /// Buy `size` now, at no more than `limit`.
    pub fn buy(&mut self, size: f64, limit: f64) -> &mut Self {
        self.add(Intent::Market {
            side: Side::Bid,
            size,
            limit,
        })
    }

    /// Sell `size` now, at no less than `limit`.
    pub fn sell(&mut self, size: f64, limit: f64) -> &mut Self {
        self.add(Intent::Market {
            side: Side::Ask,
            size,
            limit,
        })
    }

    /// Rest a bid of `size` at `price`.
    pub fn bid(&mut self, price: f64, size: f64) -> &mut Self {
        self.add(Intent::Limit {
            side: Side::Bid,
            price,
            size,
        })
    }

    /// Rest an ask of `size` at `price`.
    pub fn ask(&mut self, price: f64, size: f64) -> &mut Self {
        self.add(Intent::Limit {
            side: Side::Ask,
            price,
            size,
        })
    }

    /// Cancel every resting order.
    pub fn cancel_all(&mut self) -> &mut Self {
        self.add(Intent::CancelAll)
    }

    /// Cancel the resting order given `client_id` ([`Orders::client_id`]).
    pub fn cancel(&mut self, client_id: u64) -> &mut Self {
        self.add(Intent::Cancel { client_id })
    }

    /// Answer the request for a quote `id`: a bid, an ask, or both, for the
    /// whole request.
    pub fn respond(&mut self, id: &str, bid: Option<f64>, ask: Option<f64>) -> &mut Self {
        self.add(Intent::Respond {
            id: id.to_string(),
            bid,
            ask,
            size: None,
        })
    }

    /// Swap `amount` (of the base asset, or of the quote asset) against a
    /// pool, at no worse than `limit`.
    pub fn swap(&mut self, side: Side, amount: f64, unit: AmountUnit, limit: f64) -> &mut Self {
        self.add(Intent::Swap {
            side,
            amount,
            unit,
            limit,
        })
    }

    /// Provide up to `base` and `quote` to a pool, over `range` or the
    /// whole curve.
    pub fn add_liquidity(&mut self, base: f64, quote: f64, range: Option<PriceRange>) -> &mut Self {
        self.add(Intent::AddLiquidity {
            base,
            quote,
            range,
            position: None,
            shape: None,
        })
    }

    /// Take `share` of a liquidity position out and collect its fees: 0
    /// collects the fees only.
    pub fn remove_liquidity(&mut self, position: Option<&str>, share: f64) -> &mut Self {
        self.add(Intent::RemoveLiquidity {
            position: position.map(str::to_string),
            share,
            min_base: None,
            min_quote: None,
        })
    }

    /// Supply `amount` to a lending reserve, or stake it into a stake pool.
    pub fn deposit(&mut self, amount: f64) -> &mut Self {
        self.add(Intent::Deposit {
            amount: Some(amount),
        })
    }

    /// Take supply out, or unstake: `None` is all of it.
    pub fn withdraw(&mut self, amount: Option<f64>) -> &mut Self {
        self.add(Intent::Withdraw { amount })
    }

    pub fn borrow(&mut self, amount: f64) -> &mut Self {
        self.add(Intent::Borrow {
            amount: Some(amount),
        })
    }

    /// Repay debt: `None` is all of it.
    pub fn repay(&mut self, amount: Option<f64>) -> &mut Self {
        self.add(Intent::Repay { amount })
    }

    /// Repay `repay` of `obligation`'s debt and take its collateral at the
    /// reserve `collateral`.
    pub fn liquidate(&mut self, obligation: &str, collateral: &str, repay: f64) -> &mut Self {
        self.add(Intent::Liquidate {
            obligation: obligation.to_string(),
            collateral: collateral.to_string(),
            repay,
            min_received: None,
        })
    }

    /// The last line is for `venue`, one of [`Session::venues`]' ids.
    pub fn at(&mut self, venue: &str) -> &mut Self {
        self.last(|o| o.venue = Some(venue.to_string()))
    }

    /// The last line's orders carry `id`: in their status, and for
    /// [`Orders::cancel`].
    pub fn client_id(&mut self, id: u64) -> &mut Self {
        self.last(|o| o.client_id = Some(id))
    }

    /// The last line's resting orders take nothing: one that would cross
    /// the book isn't placed.
    pub fn post_only(&mut self) -> &mut Self {
        self.last(|o| o.time_in_force = TimeInForce::PostOnly)
    }

    /// The last line, a limit order, fills what it can now and rests
    /// nothing.
    pub fn ioc(&mut self) -> &mut Self {
        self.last(|o| o.time_in_force = TimeInForce::Ioc)
    }

    /// The last line, a limit order, a quote or a response to a request for
    /// a quote, lasts until `slot`.
    pub fn expires(&mut self, slot: u64) -> &mut Self {
        self.last(|o| o.expires_slot = Some(slot))
    }

    /// The last line's orders may only shrink the position: they are cut so
    /// on the sheet, and carry the venue's reduce-only flag too.
    pub fn reduce_only(&mut self) -> &mut Self {
        self.last(|o| o.reduce_only = true)
    }

    /// The last line, a target, trades at most `share` of the venue's
    /// volume since the previous step, each step.
    pub fn participation(&mut self, share: f64) -> &mut Self {
        self.last(|o| o.participation = Some(share))
    }

    /// The last line's transactions are sent with `options`, where the
    /// session applies them.
    pub fn tx(&mut self, options: TxOptions) -> &mut Self {
        self.last(|o| o.tx = Some(options))
    }

    /// What the sheet holds so far, without the lines' options.
    pub fn intents(&self) -> &[Intent] {
        &self.intents
    }
}

/// A concrete order, as [`plan`] turns intentions into them, or an action
/// that isn't an order, as the sheet gave it.
#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    Market {
        side: Side,
        size: f64,
        limit: f64,
    },
    Limit {
        side: Side,
        price: f64,
        size: f64,
    },
    /// A resting order that takes nothing: not placed if it would cross.
    PostOnly {
        side: Side,
        price: f64,
        size: f64,
    },
    CancelAll,
    Cancel {
        client_id: u64,
    },
    /// A quote answering a request.
    Respond {
        id: String,
        bid: Option<f64>,
        ask: Option<f64>,
        size: Option<f64>,
    },
    Swap {
        side: Side,
        amount: f64,
        unit: AmountUnit,
        limit: f64,
    },
    AddLiquidity {
        base: f64,
        quote: f64,
        range: Option<PriceRange>,
        position: Option<String>,
        shape: Option<LiquidityShape>,
    },
    RemoveLiquidity {
        position: Option<String>,
        share: f64,
        min_base: Option<f64>,
        min_quote: Option<f64>,
    },
    Deposit {
        amount: Option<f64>,
    },
    Withdraw {
        amount: Option<f64>,
    },
    Borrow {
        amount: Option<f64>,
    },
    Repay {
        amount: Option<f64>,
    },
    Liquidate {
        obligation: String,
        collateral: String,
        repay: f64,
        min_received: Option<f64>,
    },
}

/// An order [`plan_sheet`] made of a line of the sheet.
#[derive(Debug, Clone, PartialEq)]
pub struct Planned {
    /// The line it came from, from 0.
    pub line: usize,
    /// The line's venue: `None` is the session's own market.
    pub venue: Option<String>,
    pub client_id: Option<u64>,
    pub action: Action,
    /// The line's reduce-only flag, which the venue enforces too.
    pub reduce_only: bool,
    /// The line's expiry: the last slot a resting order rests in, or a
    /// response can be taken at.
    pub expires_slot: Option<u64>,
    pub tx: Option<TxOptions>,
}

/// Turn a sheet's intentions into orders, in order, for a market. Every order
/// is cut so that the position stays within the cap even if every order
/// resting fills: the market's [`Market::resting_bids`] and
/// [`Market::resting_asks`], and every resting order the sheet adds.
/// Market orders and targets count as filled. A cancel, or a quote, which
/// cancels first, clears what was resting. Returns the orders, and a note for
/// each one it cut or dropped.
///
/// The intents are lines with no options: [`plan_sheet`] plans a whole sheet.
pub fn plan(intents: &[Intent], market: &Market) -> (Vec<Action>, Vec<String>) {
    let (planned, notes) = plan_lines(
        intents.iter().map(|i| (i, &DEFAULT_OPTIONS)),
        market,
        &|_, _| true,
    );
    (planned.into_iter().map(|p| p.action).collect(), notes)
}

/// [`plan`], for a sheet's lines and their options. Each venue keeps its own
/// count of position and resting orders: the session's own market starts
/// from the market's, another from its [`VenueView`]'s position, with
/// nothing resting. The cap is the mandate's on every venue.
///
/// - A reduce-only line is cut so that no fill can take the position past
///   zero, whatever else fills: the sheet's earlier orders that shrink the
///   position, and the orders resting on its side, count as filled; earlier
///   orders that grow it count as not filled.
/// - An immediate-or-cancel limit order is a market order bounded by its
///   price; a post-only one rests as [`Action::PostOnly`]. A quote may be
///   post-only.
/// - A swap is cut as a market order; one counted in the quote asset is
///   counted in base units at its limit price.
/// - A target's participation cuts each step's trading to its share of the
///   volume on the venue's tape since the previous step. A venue without a
///   tape has no volume to take a share of: the target doesn't trade, and a
///   note says so.
/// - A target on another venue trades at no worse than its book's mid,
///   moved by the slippage.
/// - Cancels by client id, responses to requests for quotes, liquidity and
///   lending actions go through as they are.
/// - A line that asks for what can't be done (a target with no positive
///   pacing, an amount or a price that isn't a number) is dropped, with a
///   note.
pub fn plan_sheet(orders: &Orders, market: &Market) -> (Vec<Planned>, Vec<String>) {
    plan_lines(orders.lines(), market, &|_, _| true)
}

/// [`plan_sheet`], for a session that executes only some of what a sheet
/// can ask: `executes(venue, action)` says which. A line with an order the
/// session won't execute is still planned, so its status can say so, but
/// counts for nothing: the venue's position and resting orders are as if
/// it weren't on the sheet.
pub fn plan_sheet_with(
    orders: &Orders,
    market: &Market,
    executes: impl Fn(Option<&str>, &Action) -> bool,
) -> (Vec<Planned>, Vec<String>) {
    plan_lines(orders.lines(), market, &executes)
}

/// One venue's count, as the planner goes down the sheet.
#[derive(Clone)]
struct Ledger {
    venue: Option<String>,
    /// The position if every order so far fills, and what rests each way.
    position: f64,
    bids: f64,
    asks: f64,
    /// The position before the sheet, and what the sheet's taking orders
    /// bought and sold.
    start: f64,
    bought: f64,
    sold: f64,
}

impl Ledger {
    fn new(venue: Option<String>, position: f64, bids: f64, asks: f64) -> Self {
        Ledger {
            venue,
            position,
            bids,
            asks,
            start: position,
            bought: 0.0,
            sold: 0.0,
        }
    }

    /// What a reduce-only order may trade on `side`: back to zero from the
    /// position nearest it, where the sheet's sells (for a sell) or buys
    /// (for a buy) all fill, with what rests on that side, and nothing that
    /// grows the position does.
    fn reduce_room(&self, side: Side) -> f64 {
        match side {
            Side::Bid => (-(self.start + self.bought + self.bids)).max(0.0),
            Side::Ask => (self.start - self.sold - self.asks).max(0.0),
        }
    }

    /// An order that takes, counted as filled.
    fn took(&mut self, side: Side, size: f64) {
        self.position += signed(side, size);
        match side {
            Side::Bid => self.bought += size,
            Side::Ask => self.sold += size,
        }
    }
}

fn plan_lines<'a>(
    lines: impl IntoIterator<Item = (&'a Intent, &'a LineOptions)>,
    market: &Market,
    executes: &dyn Fn(Option<&str>, &Action) -> bool,
) -> (Vec<Planned>, Vec<String>) {
    // A cap that isn't a number allows nothing.
    let cap = if market.cap >= 0.0 { market.cap } else { 0.0 };
    let own = market.venues.first().map(|v| v.id.as_str());
    let mut ledgers = vec![Ledger::new(
        None,
        market.position.size,
        market.resting_bids,
        market.resting_asks,
    )];
    let mut planned = Vec::new();
    let mut notes = Vec::new();
    let room = |l: &Ledger, side: Side| {
        let (buy, sell) = mtx_core::mandate::position_room(cap, l.position, l.bids, l.asks);
        match side {
            Side::Bid => buy,
            Side::Ask => sell,
        }
    };
    for (line, (intent, options)) in lines.into_iter().enumerate() {
        let venue = options
            .venue
            .clone()
            .filter(|venue| Some(venue.as_str()) != own);
        let view = match &venue {
            None => market.venues.first(),
            Some(id) => market.venues.iter().find(|v| &v.id == id),
        };
        let index = match ledgers.iter().position(|l| l.venue == venue) {
            Some(index) => index,
            None => {
                let position = view.and_then(|v| v.position).map_or(0.0, |p| p.size);
                ledgers.push(Ledger::new(venue.clone(), position, 0.0, 0.0));
                ledgers.len() - 1
            }
        };
        let before = ledgers[index].clone();
        let first = planned.len();
        'line: {
            let l = &mut ledgers[index];
            let mut push = |action: Action| {
                planned.push(Planned {
                    line,
                    venue: venue.clone(),
                    client_id: options.client_id,
                    action,
                    reduce_only: options.reduce_only,
                    expires_slot: options.expires_slot,
                    tx: options.tx,
                })
            };
            let reduce =
                |size: f64, l: &Ledger, side: Side, what: &str, notes: &mut Vec<String>| {
                    let room = l.reduce_room(side);
                    if options.reduce_only && size > room {
                        notes.push(format!(
                    "reduce-only {what} of {size} cut to {room}, so as not to grow the position"
                ));
                        room
                    } else {
                        size
                    }
                };
            if let Some(what) = not_a_number(intent) {
                notes.push(format!(
                    "{what} with an amount or a price that isn't a number: not sent"
                ));
                break 'line;
            }
            match intent {
                &Intent::Target {
                    position: target,
                    execution,
                } => {
                    if target.is_nan() {
                        notes.push("a target needs a position: not trading".into());
                        break 'line;
                    }
                    let wanted = target.clamp(-cap, cap);
                    if wanted != target {
                        notes.push(format!(
                            "target {target} is past the cap {cap}: aiming at {wanted}"
                        ));
                    }
                    if !(execution.slice > 0.0 && execution.min_trade > 0.0) {
                        notes.push(
                            "a target needs a positive slice and min_trade: not trading".into(),
                        );
                        break 'line;
                    }
                    if execution.max_per_step.is_nan() || execution.max_per_step <= 0.0 {
                        notes.push("a target needs a positive max_per_step: not trading".into());
                        break 'line;
                    }
                    let mark = match &venue {
                        None => market.mark,
                        Some(id) => match view.and_then(|v| v.best_bid.zip(v.best_ask)) {
                            Some((bid, ask)) => (bid + ask) / 2.0,
                            None => {
                                notes.push(format!(
                                    "target on {id}: no bid and ask to price it at: not trading"
                                ));
                                break 'line;
                            }
                        },
                    };
                    let mut missing = (wanted - l.position)
                        .clamp(-execution.max_per_step, execution.max_per_step);
                    if let Some(share) = options.participation {
                        if !(share > 0.0 && share <= 1.0) {
                            notes.push(format!(
                                "participation {share} must be in (0, 1]: not trading"
                            ));
                            break 'line;
                        }
                        // No tape is no volume seen: nothing to take a share of.
                        let volume = view
                            .and_then(|v| v.tape.as_ref())
                            .map(|tape| tape.iter().fold(0.0, |volume, t| volume + t.base));
                        if volume.is_none() {
                            notes.push(
                            "participation needs the venue's tape, which this session doesn't report: not trading"
                                .into(),
                        );
                        }
                        let most = share * volume.unwrap_or(0.0);
                        if missing.abs() > most {
                            if volume.is_some() {
                                notes.push(format!(
                                "participation {share} of the step's volume: trading {most} of {}",
                                missing.abs()
                            ));
                            }
                            missing = missing.signum() * most;
                        }
                    }
                    // Resting orders could fill too: trade only what keeps the
                    // position within the cap if they do.
                    let side = if missing > 0.0 { Side::Bid } else { Side::Ask };
                    let room = room(l, side);
                    if missing.abs() > room {
                        notes.push(format!(
                        "target {target} would leave the position past the cap if the resting orders filled: trading {room} of {}",
                        missing.abs()
                    ));
                        missing = missing.signum() * room;
                    }
                    missing =
                        missing.signum() * reduce(missing.abs(), l, side, "target", &mut notes);
                    let mut orders = 0;
                    while missing.abs() >= execution.min_trade {
                        if orders == MAX_TARGET_ORDERS {
                            notes.push(format!(
                                "target {target}: at most {MAX_TARGET_ORDERS} orders a step; {} waits",
                                missing.abs()
                            ));
                            break;
                        }
                        orders += 1;
                        let size =
                            (missing.abs() / execution.min_trade).floor() * execution.min_trade;
                        let size = size.min(execution.slice);
                        let side = if missing > 0.0 { Side::Bid } else { Side::Ask };
                        let limit = match side {
                            Side::Bid => mark * (1.0 + execution.slippage_bps / 10_000.0),
                            Side::Ask => mark * (1.0 - execution.slippage_bps / 10_000.0),
                        };
                        push(Action::Market {
                            side,
                            size,
                            limit: round_cents(limit),
                        });
                        l.took(side, size);
                        missing -= signed(side, size);
                    }
                }
                &Intent::Market { side, size, limit } => {
                    let size = cut(size, room(l, side), "market order", &mut notes);
                    let size = reduce(size, l, side, "market order", &mut notes);
                    if size > 0.0 {
                        push(Action::Market { side, size, limit });
                        l.took(side, size);
                    }
                }
                &Intent::Swap {
                    side,
                    amount,
                    unit,
                    limit,
                } => {
                    // In base units, at the limit price for a quote amount.
                    let base = match unit {
                        AmountUnit::Base => amount,
                        AmountUnit::Quote if limit > 0.0 => amount / limit,
                        AmountUnit::Quote => {
                            notes.push(
                                "a swap in the quote asset needs a positive limit: not trading"
                                    .into(),
                            );
                            break 'line;
                        }
                    };
                    let size = cut(base, room(l, side), "swap", &mut notes);
                    let size = reduce(size, l, side, "swap", &mut notes);
                    if size > 0.0 {
                        push(Action::Swap {
                            side,
                            amount: if size < base {
                                amount * size / base
                            } else {
                                amount
                            },
                            unit,
                            limit,
                        });
                        l.took(side, size);
                    }
                }
                &Intent::Limit { side, price, size } => {
                    let size = cut(size, room(l, side), "resting order", &mut notes);
                    let size = reduce(size, l, side, "resting order", &mut notes);
                    if size > 0.0 {
                        match options.time_in_force {
                            TimeInForce::Ioc => {
                                push(Action::Market {
                                    side,
                                    size,
                                    limit: price,
                                });
                                l.took(side, size);
                            }
                            tif => {
                                push(if tif == TimeInForce::PostOnly {
                                    Action::PostOnly { side, price, size }
                                } else {
                                    Action::Limit { side, price, size }
                                });
                                match side {
                                    Side::Bid => l.bids += size,
                                    Side::Ask => l.asks += size,
                                }
                            }
                        }
                    }
                }
                &Intent::Quote { bid, ask, size } => {
                    if options.time_in_force == TimeInForce::Ioc {
                        notes.push("a quote rests: immediate-or-cancel doesn't apply".into());
                    }
                    let post_only = options.time_in_force == TimeInForce::PostOnly;
                    let rest = |side, price, size| {
                        if post_only {
                            Action::PostOnly { side, price, size }
                        } else {
                            Action::Limit { side, price, size }
                        }
                    };
                    push(Action::CancelAll);
                    (l.bids, l.asks) = (0.0, 0.0);
                    let bid_size = cut(size, room(l, Side::Bid), "bid", &mut notes);
                    let ask_size = cut(size, room(l, Side::Ask), "ask", &mut notes);
                    let bid_size = reduce(bid_size, l, Side::Bid, "bid", &mut notes);
                    let ask_size = reduce(ask_size, l, Side::Ask, "ask", &mut notes);
                    if bid_size > 0.0 {
                        push(rest(Side::Bid, bid, bid_size));
                        l.bids += bid_size;
                    }
                    if ask_size > 0.0 {
                        push(rest(Side::Ask, ask, ask_size));
                        l.asks += ask_size;
                    }
                }
                Intent::CancelAll => {
                    push(Action::CancelAll);
                    (l.bids, l.asks) = (0.0, 0.0);
                }
                &Intent::Cancel { client_id } => push(Action::Cancel { client_id }),
                Intent::Respond { id, bid, ask, size } => push(Action::Respond {
                    id: id.clone(),
                    bid: *bid,
                    ask: *ask,
                    size: *size,
                }),
                Intent::AddLiquidity {
                    base,
                    quote,
                    range,
                    position,
                    shape,
                } => push(Action::AddLiquidity {
                    base: *base,
                    quote: *quote,
                    range: *range,
                    position: position.clone(),
                    shape: *shape,
                }),
                Intent::RemoveLiquidity {
                    position,
                    share,
                    min_base,
                    min_quote,
                } => push(Action::RemoveLiquidity {
                    position: position.clone(),
                    share: *share,
                    min_base: *min_base,
                    min_quote: *min_quote,
                }),
                &Intent::Deposit { amount } => push(Action::Deposit { amount }),
                &Intent::Withdraw { amount } => push(Action::Withdraw { amount }),
                &Intent::Borrow { amount } => push(Action::Borrow { amount }),
                &Intent::Repay { amount } => push(Action::Repay { amount }),
                Intent::Liquidate {
                    obligation,
                    collateral,
                    repay,
                    min_received,
                } => push(Action::Liquidate {
                    obligation: obligation.clone(),
                    collateral: collateral.clone(),
                    repay: *repay,
                    min_received: *min_received,
                }),
            }
        }
        // A line the session won't execute counts for nothing.
        if planned[first..]
            .iter()
            .any(|p| !executes(p.venue.as_deref(), &p.action))
        {
            ledgers[index] = before;
        }
    }
    (planned, notes)
}

/// What is resting after `actions` are sent on top of `bids` and `asks`, in
/// base units: a cancel clears it, a resting order adds to its side.
pub fn resting_after(bids: f64, asks: f64, actions: &[Action]) -> (f64, f64) {
    actions
        .iter()
        .fold((bids, asks), |(bids, asks), action| match *action {
            Action::CancelAll => (0.0, 0.0),
            Action::Limit {
                side: Side::Bid,
                size,
                ..
            }
            | Action::PostOnly {
                side: Side::Bid,
                size,
                ..
            } => (bids + size, asks),
            Action::Limit {
                side: Side::Ask,
                size,
                ..
            }
            | Action::PostOnly {
                side: Side::Ask,
                size,
                ..
            } => (bids, asks + size),
            _ => (bids, asks),
        })
}

/// What a line is, if one of its amounts or prices isn't a number (a
/// target's position is checked where it's planned).
fn not_a_number(intent: &Intent) -> Option<&'static str> {
    let (what, numbers): (&str, Vec<f64>) = match intent {
        Intent::Target { .. } | Intent::CancelAll | Intent::Cancel { .. } => return None,
        &Intent::Market { size, limit, .. } => ("a market order", vec![size, limit]),
        &Intent::Limit { price, size, .. } => ("a limit order", vec![price, size]),
        &Intent::Quote { bid, ask, size } => ("a quote", vec![bid, ask, size]),
        &Intent::Swap { amount, limit, .. } => ("a swap", vec![amount, limit]),
        Intent::Respond { bid, ask, size, .. } => (
            "a response",
            [*bid, *ask, *size].into_iter().flatten().collect(),
        ),
        Intent::AddLiquidity {
            base, quote, range, ..
        } => (
            "liquidity",
            [*base, *quote]
                .into_iter()
                .chain(range.iter().flat_map(|r| [r.lower, r.upper]))
                .collect(),
        ),
        Intent::RemoveLiquidity {
            share,
            min_base,
            min_quote,
            ..
        } => (
            "a liquidity removal",
            [Some(*share), *min_base, *min_quote]
                .into_iter()
                .flatten()
                .collect(),
        ),
        Intent::Deposit { amount }
        | Intent::Withdraw { amount }
        | Intent::Borrow { amount }
        | Intent::Repay { amount } => ("a lending action", amount.iter().copied().collect()),
        Intent::Liquidate {
            repay,
            min_received,
            ..
        } => (
            "a liquidation",
            [Some(*repay), *min_received]
                .into_iter()
                .flatten()
                .collect(),
        ),
    };
    numbers.iter().any(|n| n.is_nan()).then_some(what)
}

fn signed(side: Side, size: f64) -> f64 {
    match side {
        Side::Bid => size,
        Side::Ask => -size,
    }
}

fn cut(size: f64, room: f64, what: &str, notes: &mut Vec<String>) -> f64 {
    if size > room {
        notes.push(format!(
            "{what} of {size} cut to {room}, to stay within the cap"
        ));
        room
    } else {
        size
    }
}

fn round_cents(usd: f64) -> f64 {
    (usd * 100.0).round() / 100.0
}

/// Call a strategy's [`Strategy::step`] on a market you built, without a
/// chain, and return what it put on the sheet: for tests.
///
/// ```
/// use mtx_kit::prelude::*;
///
/// struct AlwaysLong;
/// impl Strategy for AlwaysLong {
///     fn step(&mut self, market: &Market, orders: &mut Orders) -> BotResult {
///         orders.target_position(market.cap);
///         Ok(())
///     }
/// }
///
/// let market = Market { mark: 100.0, cap: 70.0, ..Market::default() };
/// let intents = mtx_kit::step_offline(&mut AlwaysLong, &market).unwrap();
/// assert!(matches!(intents[0], Intent::Target { position, .. } if position == 70.0));
/// ```
pub fn step_offline<S: Strategy>(strategy: &mut S, market: &Market) -> BotResult<Vec<Intent>> {
    let mut orders = Orders::new();
    strategy.step(market, &mut orders)?;
    Ok(orders.into_intents())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn market(position: f64) -> Market {
        Market {
            mark: 100.0,
            cap: 70.0,
            position: Position {
                size: position,
                entry_usd: 100.0,
            },
            ..Market::default()
        }
    }

    #[test]
    fn a_target_trades_in_slices_toward_it() {
        let (actions, notes) = plan(
            &[Intent::Target {
                position: 25.0,
                execution: Execution::default(),
            }],
            &market(0.0),
        );
        assert!(notes.is_empty());
        let sizes: Vec<f64> = actions
            .iter()
            .map(|a| match a {
                Action::Market {
                    side: Side::Bid,
                    size,
                    limit,
                } => {
                    assert_eq!(*limit, 102.0);
                    *size
                }
                other => panic!("{other:?}"),
            })
            .collect();
        assert_eq!(sizes, vec![10.0, 10.0, 5.0]);
    }

    #[test]
    fn a_target_past_the_cap_stops_at_it() {
        let (actions, notes) = plan(
            &[Intent::Target {
                position: -500.0,
                execution: Execution::default(),
            }],
            &market(60.0),
        );
        assert_eq!(notes.len(), 1);
        let sold: f64 = actions
            .iter()
            .map(|a| match a {
                Action::Market {
                    side: Side::Ask,
                    size,
                    ..
                } => *size,
                other => panic!("{other:?}"),
            })
            .sum();
        assert_eq!(sold, 130.0, "from +60 to -70");
    }

    #[test]
    fn a_target_sliced_too_fine_sends_at_most_its_cap_of_orders() {
        let execution = Execution {
            slice: 1e-9,
            min_trade: 1e-9,
            ..Execution::default()
        };
        let (actions, notes) = plan(
            &[Intent::Target {
                position: 70.0,
                execution,
            }],
            &market(0.0),
        );
        assert_eq!(actions.len(), MAX_TARGET_ORDERS);
        assert!(notes[0].contains("at most 64 orders a step"), "{notes:?}");
        // The default slicing is far inside it.
        let (actions, notes) = plan(
            &[Intent::Target {
                position: -70.0,
                execution: Execution::default(),
            }],
            &market(70.0),
        );
        assert_eq!(actions.len(), 14);
        assert!(notes.is_empty(), "{notes:?}");
    }

    #[test]
    fn a_target_waits_for_the_next_step_past_max_per_step() {
        let execution = Execution {
            max_per_step: 15.0,
            ..Execution::default()
        };
        let (actions, _) = plan(
            &[Intent::Target {
                position: 70.0,
                execution,
            }],
            &market(0.0),
        );
        let bought: f64 = actions
            .iter()
            .map(|a| {
                if let Action::Market { size, .. } = a {
                    *size
                } else {
                    0.0
                }
            })
            .sum();
        assert_eq!(bought, 15.0);
    }

    #[test]
    fn quotes_are_cut_so_no_fill_passes_the_cap() {
        let (actions, notes) = plan(
            &[Intent::Quote {
                bid: 99.0,
                ask: 101.0,
                size: 20.0,
            }],
            &market(60.0),
        );
        assert_eq!(actions[0], Action::CancelAll);
        assert_eq!(
            actions[1],
            Action::Limit {
                side: Side::Bid,
                price: 99.0,
                size: 10.0
            }
        );
        assert_eq!(
            actions[2],
            Action::Limit {
                side: Side::Ask,
                price: 101.0,
                size: 20.0
            }
        );
        assert_eq!(notes.len(), 1);
    }

    #[test]
    fn market_orders_count_the_ones_before_them() {
        let intents = [
            Intent::Market {
                side: Side::Bid,
                size: 50.0,
                limit: 101.0,
            },
            Intent::Market {
                side: Side::Bid,
                size: 50.0,
                limit: 101.0,
            },
        ];
        let (actions, _) = plan(&intents, &market(0.0));
        assert_eq!(
            actions[1],
            Action::Market {
                side: Side::Bid,
                size: 20.0,
                limit: 101.0
            }
        );
    }

    #[test]
    fn a_sheet_keeps_what_it_was_asked() {
        let mut orders = Orders::new();
        orders.cancel_all().buy(3.0, 101.0);
        assert_eq!(orders.intents().len(), 2);
        assert_eq!(orders.into_intents()[0], Intent::CancelAll);
    }

    #[test]
    fn rpc_refuses_what_isnt_a_read() {
        assert!(
            rpc("sendTransaction", "[]")
                .unwrap_err()
                .contains("not a read")
        );
        assert!(rpc("surfnet_setAccount", "[]").is_err());
    }

    fn limit(side: Side, size: f64) -> Intent {
        Intent::Limit {
            side,
            price: 100.0,
            size,
        }
    }

    fn resting_sizes(actions: &[Action]) -> Vec<f64> {
        actions
            .iter()
            .filter_map(|a| match a {
                Action::Limit { size, .. } => Some(*size),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn resting_orders_on_one_sheet_count_together() {
        // Two bids of 50 under a cap of 70: both filling would hold 100.
        let (actions, notes) = plan(
            &[limit(Side::Bid, 50.0), limit(Side::Bid, 50.0)],
            &market(0.0),
        );
        assert_eq!(resting_sizes(&actions), [50.0, 20.0]);
        assert_eq!(notes.len(), 1);
        // A market buy after them has no room left.
        let (actions, _) = plan(
            &[
                limit(Side::Bid, 70.0),
                Intent::Market {
                    side: Side::Bid,
                    size: 5.0,
                    limit: 101.0,
                },
            ],
            &market(0.0),
        );
        assert_eq!(actions.len(), 1);
    }

    #[test]
    fn orders_resting_from_earlier_steps_count() {
        let resting = Market {
            resting_bids: 40.0,
            resting_asks: 10.0,
            ..market(20.0)
        };
        assert_eq!(
            (resting.room_to_buy(), resting.room_to_sell()),
            (10.0, 80.0)
        );
        // A bid, a market buy and a target are each cut to the room left.
        let (actions, _) = plan(&[limit(Side::Bid, 30.0)], &resting);
        assert_eq!(resting_sizes(&actions), [10.0]);
        let (actions, _) = plan(
            &[Intent::Market {
                side: Side::Bid,
                size: 30.0,
                limit: 101.0,
            }],
            &resting,
        );
        assert_eq!(
            actions,
            [Action::Market {
                side: Side::Bid,
                size: 10.0,
                limit: 101.0
            }]
        );
        let (actions, notes) = plan(
            &[Intent::Target {
                position: 70.0,
                execution: Execution::default(),
            }],
            &resting,
        );
        let bought: f64 = actions
            .iter()
            .map(|a| match a {
                Action::Market { size, .. } => *size,
                _ => 0.0,
            })
            .sum();
        assert_eq!(bought, 10.0);
        assert!(notes[0].contains("resting orders"), "{notes:?}");
    }

    #[test]
    fn a_cancel_or_a_quote_clears_what_rests() {
        let resting = Market {
            resting_bids: 70.0,
            ..market(0.0)
        };
        assert_eq!(resting.room_to_buy(), 0.0);
        let (actions, _) = plan(&[Intent::CancelAll, limit(Side::Bid, 50.0)], &resting);
        assert_eq!(resting_sizes(&actions), [50.0]);
        let (actions, _) = plan(
            &[Intent::Quote {
                bid: 99.0,
                ask: 101.0,
                size: 30.0,
            }],
            &resting,
        );
        assert_eq!(actions[0], Action::CancelAll);
        assert_eq!(resting_sizes(&actions), [30.0, 30.0]);
    }

    #[test]
    fn the_ledger_follows_what_was_sent() {
        let quoted = [
            Action::CancelAll,
            Action::Limit {
                side: Side::Bid,
                price: 99.0,
                size: 30.0,
            },
            Action::Limit {
                side: Side::Ask,
                price: 101.0,
                size: 20.0,
            },
        ];
        assert_eq!(resting_after(5.0, 5.0, &quoted), (30.0, 20.0));
        let more = [Action::Limit {
            side: Side::Bid,
            price: 98.0,
            size: 10.0,
        }];
        assert_eq!(resting_after(30.0, 20.0, &more), (40.0, 20.0));
        let taken = [Action::Market {
            side: Side::Bid,
            size: 3.0,
            limit: 101.0,
        }];
        assert_eq!(resting_after(40.0, 20.0, &taken), (40.0, 20.0));
    }

    fn planned_actions(orders: &Orders, market: &Market) -> (Vec<Action>, Vec<String>) {
        let (planned, notes) = plan_sheet(orders, market);
        (planned.into_iter().map(|p| p.action).collect(), notes)
    }

    #[test]
    fn a_sheet_without_options_plans_as_before() {
        let mut orders = Orders::new();
        orders
            .quote(99.0, 101.0, 20.0)
            .target_position(70.0)
            .buy(3.0, 101.0);
        let (sheet, sheet_notes) = planned_actions(&orders, &market(60.0));
        let (plain, plain_notes) = plan(orders.intents(), &market(60.0));
        assert_eq!((sheet, sheet_notes), (plain, plain_notes));
    }

    #[test]
    fn post_only_rests_and_ioc_takes() {
        let mut orders = Orders::new();
        orders
            .bid(99.0, 5.0)
            .post_only()
            .quote(98.0, 102.0, 3.0)
            .post_only()
            .ask(99.5, 4.0)
            .ioc();
        let (actions, _) = planned_actions(&orders, &market(0.0));
        assert_eq!(
            actions,
            [
                Action::PostOnly {
                    side: Side::Bid,
                    price: 99.0,
                    size: 5.0
                },
                Action::CancelAll,
                Action::PostOnly {
                    side: Side::Bid,
                    price: 98.0,
                    size: 3.0
                },
                Action::PostOnly {
                    side: Side::Ask,
                    price: 102.0,
                    size: 3.0
                },
                Action::Market {
                    side: Side::Ask,
                    size: 4.0,
                    limit: 99.5
                },
            ]
        );
        // A post-only order rests; an immediate-or-cancel one doesn't.
        assert_eq!(resting_after(0.0, 0.0, &actions), (3.0, 3.0));
    }

    #[test]
    fn reduce_only_never_grows_or_flips_the_position() {
        let mut orders = Orders::new();
        orders
            .sell(50.0, 99.0)
            .reduce_only()
            .buy(5.0, 101.0)
            .reduce_only()
            .target_position(-40.0)
            .reduce_only();
        let (actions, notes) = planned_actions(&orders, &market(20.0));
        // Sells 20 to flat; then nothing is left to reduce either way.
        assert_eq!(
            actions,
            [Action::Market {
                side: Side::Ask,
                size: 20.0,
                limit: 99.0
            }]
        );
        assert_eq!(notes.len(), 3, "{notes:?}");
        // An earlier buy that may not fill doesn't count: flat, an IOC buy
        // of 2 leaves a reduce-only sell nothing to sell, and long 10, a buy
        // of 5 leaves it 10, not 15.
        let mut orders = Orders::new();
        orders.bid(102.0, 2.0).ioc().sell(5.0, 98.0).reduce_only();
        let (actions, notes) = planned_actions(&orders, &market(0.0));
        assert_eq!(actions.len(), 1, "{actions:?}");
        assert!(notes[0].contains("cut to 0"), "{notes:?}");
        let mut orders = Orders::new();
        orders.buy(5.0, 101.0).sell(20.0, 99.0).reduce_only();
        let (actions, _) = planned_actions(&orders, &market(10.0));
        assert_eq!(
            actions[1],
            Action::Market {
                side: Side::Ask,
                size: 10.0,
                limit: 99.0
            }
        );
        // Earlier sells that shrink the position count as filled: two
        // reduce-only sells of 8 on a long 10 get 8 and 2.
        let mut orders = Orders::new();
        orders
            .sell(8.0, 99.0)
            .reduce_only()
            .sell(8.0, 99.0)
            .reduce_only();
        let (actions, _) = planned_actions(&orders, &market(10.0));
        assert_eq!(
            actions[1],
            Action::Market {
                side: Side::Ask,
                size: 2.0,
                limit: 99.0
            }
        );
        // Resting asks count: with 15 resting, a reduce-only sell of the
        // long 20 gets 5.
        let resting = Market {
            resting_asks: 15.0,
            ..market(20.0)
        };
        let mut orders = Orders::new();
        orders.ask(101.0, 20.0).reduce_only();
        let (actions, _) = planned_actions(&orders, &resting);
        assert_eq!(resting_sizes(&actions), [5.0]);
    }

    fn trade(base: f64) -> mtx_core::turns::Trade {
        mtx_core::turns::Trade {
            venue: "phoenix".into(),
            side: mtx_core::Side::Buy,
            price: 100.0,
            base,
            slot: 1,
            taker: None,
            maker: None,
            own: None,
            market: None,
        }
    }

    const TAPE: Reports = Reports {
        tape: true,
        depth: true,
        timestamps: false,
        fees: false,
    };

    #[test]
    fn participation_takes_a_share_of_the_tape() {
        let venues = VenueInfo::of_perp_session(
            &Mandate::perp("phoenix", "SPCX", 70.0, 1_000.0),
            "SPCX",
            None,
        );
        let mut orders = Orders::new();
        orders.target_position(70.0).participation(0.25);
        let traded = |tape: &[mtx_core::turns::Trade], reports: Reports| {
            let (views, _) =
                venue_views(&venues, reports, None, None, Position::default(), tape, &[]);
            let market = Market {
                venues: views,
                ..market(0.0)
            };
            let (actions, notes) = planned_actions(&orders, &market);
            let size: f64 = actions
                .iter()
                .map(|a| match a {
                    Action::Market { size, .. } => *size,
                    other => panic!("{other:?}"),
                })
                .sum();
            (size, notes)
        };
        // 40 traded since the last step: a quarter of it.
        let (size, notes) = traded(&[trade(30.0), trade(10.0)], TAPE);
        assert_eq!(size, 10.0);
        assert!(notes[0].contains("participation 0.25"), "{notes:?}");
        // A step without trades: nothing to take a share of.
        let (size, notes) = traded(&[], TAPE);
        assert_eq!(size, 0.0);
        assert!(notes[0].contains("trading 0 of 70"), "{notes:?}");
        // No tape at all: no volume either, and the note says why.
        let (size, notes) = traded(&[], Reports::default());
        assert_eq!(size, 0.0);
        assert!(notes[0].contains("doesn't report"), "{notes:?}");
    }

    #[test]
    fn each_venue_keeps_its_own_count() {
        let mandate = {
            let mut mandate = Mandate::perp("phoenix", "SPCX", 70.0, 1_000.0);
            mandate
                .universe
                .extend(Mandate::spot(&["openbook"], "SOL-USDC", 70.0).universe);
            mandate
        };
        let venues = VenueInfo::of_perp_session(&mandate, "SPCX", None);
        assert_eq!(
            venues.iter().map(|v| v.id.as_str()).collect::<Vec<_>>(),
            ["phoenix:perp:SPCX", "openbook:spot:SOL-USDC"]
        );
        assert!(venues[0].tradable && !venues[1].tradable);
        let (views, _) = venue_views(
            &venues,
            Reports::default(),
            None,
            None,
            Position::default(),
            &[],
            &[],
        );
        let market = Market {
            venues: views,
            ..market(65.0)
        };
        let mut orders = Orders::new();
        orders
            .buy(10.0, 101.0)
            .buy(10.0, 151.0)
            .at("openbook:spot:SOL-USDC")
            .buy(10.0, 101.0)
            .at("phoenix:perp:SPCX")
            .tx(TxOptions {
                tip: Some(5),
                ..TxOptions::default()
            });
        let (planned, _) = plan_sheet(&orders, &market);
        let sizes: Vec<_> = planned
            .iter()
            .map(|p| (p.line, p.venue.as_deref(), p.action.clone(), p.tx))
            .collect();
        assert_eq!(
            sizes,
            [
                (
                    0,
                    None,
                    Action::Market {
                        side: Side::Bid,
                        size: 5.0,
                        limit: 101.0
                    },
                    None
                ),
                (
                    1,
                    Some("openbook:spot:SOL-USDC"),
                    Action::Market {
                        side: Side::Bid,
                        size: 10.0,
                        limit: 151.0
                    },
                    None
                ),
            ],
            "the third line, the session's own market by name, has no room left"
        );
    }

    #[test]
    fn own_trades_on_the_tape_are_fills() {
        let venues = VenueInfo::of_perp_session(
            &Mandate::perp("phoenix", "SPCX", 70.0, 1_000.0),
            "SPCX",
            None,
        );
        let mut taken = trade(2.0);
        taken.own = Some(Role::Taker);
        let mut made = trade(3.0);
        made.own = Some(Role::Maker);
        let (views, fills) = venue_views(
            &venues,
            TAPE,
            Some(99.0),
            Some(101.0),
            Position::default(),
            &[taken, made, trade(1.0)],
            &[],
        );
        assert_eq!(views[0].tape.as_ref().map(Vec::len), Some(3));
        assert_eq!(
            fills
                .iter()
                .map(|f| (f.side, f.size, f.role))
                .collect::<Vec<_>>(),
            [(Side::Bid, 2.0, Role::Taker), (Side::Ask, 3.0, Role::Maker)]
        );
    }

    #[test]
    fn turn_data_belongs_to_its_market() {
        // A perp and a spot book on one venue: entries that name their
        // market go to it; one that names none is no one's.
        let mut mandate = Mandate::perp("phoenix", "SPCX", 70.0, 1_000.0);
        mandate
            .universe
            .extend(Mandate::spot(&["phoenix"], "SOL-USDC", 70.0).universe);
        let venues = VenueInfo::of_perp_session(&mandate, "SPCX", Some("market".into()));
        assert_eq!(venues[0].address.as_deref(), Some("market"));
        let mut spot = trade(2.0);
        spot.market = Some("phoenix:spot:SOL-USDC".into());
        spot.own = Some(Role::Taker);
        let mut perp = trade(3.0);
        perp.market = Some("phoenix:perp:SPCX".into());
        let depth = [mtx_core::turns::BookLevels {
            venue: "phoenix".into(),
            market: Some("phoenix:spot:SOL-USDC".into()),
            bids: vec![mtx_core::turns::Level {
                price: 150.0,
                base: 1.0,
            }],
            asks: vec![],
        }];
        let (views, fills) = venue_views(
            &venues,
            TAPE,
            None,
            None,
            Position::default(),
            &[spot, perp, trade(1.0)],
            &depth,
        );
        let sizes = |v: &VenueView| v.tape.iter().flatten().map(|t| t.base).collect::<Vec<_>>();
        assert_eq!(sizes(&views[0]), [3.0]);
        assert_eq!(sizes(&views[1]), [2.0]);
        assert!(views[0].depth.is_none());
        assert_eq!(views[1].best_bid, Some(150.0));
        assert_eq!(fills.len(), 1);
        assert_eq!(fills[0].venue, "phoenix:spot:SOL-USDC");
    }

    #[test]
    fn a_line_the_session_wont_execute_counts_for_nothing() {
        // Flat, under a cap of 70: a swap the session can't execute must
        // not make room for a buy past the cap.
        let mut orders = Orders::new();
        orders
            .swap(Side::Ask, 70.0, AmountUnit::Base, 99.0)
            .buy(80.0, 101.0);
        let (planned, notes) = plan_sheet_with(&orders, &market(0.0), |_, action| {
            !matches!(action, Action::Swap { .. })
        });
        assert_eq!(
            planned.len(),
            2,
            "the swap is still planned, to be reported"
        );
        assert_eq!(
            planned[1].action,
            Action::Market {
                side: Side::Bid,
                size: 70.0,
                limit: 101.0
            }
        );
        assert!(notes[0].contains("cut to 70"), "{notes:?}");
        // Where the session executes it, the swap counts.
        let (planned, _) = plan_sheet(&orders, &market(0.0));
        assert_eq!(
            planned[1].action,
            Action::Market {
                side: Side::Bid,
                size: 80.0,
                limit: 101.0
            }
        );
    }

    #[test]
    fn reduce_only_and_expiry_reach_the_orders() {
        let mut orders = Orders::new();
        orders
            .ask(105.0, 10.0)
            .reduce_only()
            .expires(500)
            .bid(95.0, 1.0)
            .post_only();
        let (planned, _) = plan_sheet(&orders, &market(10.0));
        assert_eq!(
            planned
                .iter()
                .map(|p| (p.reduce_only, p.expires_slot))
                .collect::<Vec<_>>(),
            [(true, Some(500)), (false, None)]
        );
    }

    #[test]
    fn a_target_that_cant_be_paced_is_dropped_not_a_panic() {
        for (position, max_per_step) in [
            (70.0, -1.0),
            (70.0, f64::NAN),
            (70.0, 0.0),
            (f64::NAN, 10.0),
        ] {
            let mut orders = Orders::new();
            orders.target_position_with(
                position,
                Execution {
                    max_per_step,
                    ..Execution::default()
                },
            );
            let (planned, notes) = plan_sheet(&orders, &market(0.0));
            assert!(planned.is_empty(), "{position} {max_per_step}");
            assert_eq!(notes.len(), 1, "{notes:?}");
        }
        // A cap that isn't a number allows nothing, and doesn't panic.
        let mut orders = Orders::new();
        orders.target_position(10.0).buy(5.0, 101.0);
        let broken = Market {
            cap: f64::NAN,
            ..market(0.0)
        };
        let (planned, _) = plan_sheet(&orders, &broken);
        assert!(planned.is_empty());
    }

    #[test]
    fn an_amount_that_isnt_a_number_gets_a_note() {
        let mut orders = Orders::new();
        orders
            .buy(f64::NAN, 101.0)
            .sell(1.0, f64::NAN)
            .bid(f64::NAN, 1.0)
            .quote(99.0, 101.0, f64::NAN)
            .swap(Side::Bid, f64::NAN, AmountUnit::Base, 101.0)
            .respond("r", Some(f64::NAN), None)
            .remove_liquidity(None, f64::NAN)
            .deposit(f64::NAN)
            .liquidate("o", "c", f64::NAN)
            .buy(1.0, 101.0);
        let (planned, notes) = plan_sheet(&orders, &market(0.0));
        assert_eq!(planned.len(), 1, "only the last buy: {planned:?}");
        assert_eq!(notes.len(), 9, "{notes:?}");
        assert!(
            notes.iter().all(|n| n.contains("isn't a number")),
            "{notes:?}"
        );
    }

    #[test]
    fn options_change_the_last_line_only() {
        let mut orders = Orders::new();
        orders.post_only();
        assert_eq!(orders.lines().count(), 0);
        orders.bid(99.0, 1.0).bid(98.0, 1.0).post_only().at("x");
        let lines = orders.into_lines();
        assert_eq!(lines[0].1, LineOptions::default());
        assert_eq!(lines[1].1.time_in_force, TimeInForce::PostOnly);
        assert_eq!(lines[1].1.venue.as_deref(), Some("x"));
    }
}
