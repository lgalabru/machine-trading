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
    /// of the session.
    pub seed: u64,
    /// The markets of the mandate's universe, the session's own first: a
    /// line of the sheet that names no venue is for it.
    pub venues: Vec<VenueInfo>,
    /// Whether the session applies the sheet's [`TxOptions`]. None does yet:
    /// they are accepted and counted, and every order is sent as it would be
    /// without them.
    pub applies_tx_options: bool,
}

/// A market the session knows.
#[derive(Debug, Clone, PartialEq)]
pub struct VenueInfo {
    /// The market as the mandate's universe writes it,
    /// `<venue>:<kind>:<symbol>`: `phoenix:perp:SPCX`.
    pub id: String,
    pub kind: mtx_core::mandate::Kind,
    /// Whether the session can send orders there. A line for a venue it
    /// can't comes back as [`SendState::NotSent`].
    pub tradable: bool,
}

impl VenueInfo {
    /// A perp session's venues: its own market, `symbol`'s perp in the
    /// mandate's universe (`phoenix:perp:<symbol>` if it names none), which
    /// is the one it trades, and the universe's others.
    pub fn of_perp_session(mandate: &Mandate, symbol: &str) -> Vec<VenueInfo> {
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
        Self::of_mandate(mandate, &own, |instrument| *instrument == own)
    }

    /// The mandate's universe, with `first` (the session's own market)
    /// first, and `tradable` saying which the session can trade.
    pub fn of_mandate(
        mandate: &Mandate,
        first: &mtx_core::mandate::InstrumentRef,
        tradable: impl Fn(&mtx_core::mandate::InstrumentRef) -> bool,
    ) -> Vec<VenueInfo> {
        let mut instruments: Vec<_> = mandate
            .universe
            .iter()
            .map(|allowance| &allowance.instrument)
            .collect();
        instruments.sort_by_key(|instrument| *instrument != first);
        if instruments.first() != Some(&first) {
            instruments.insert(0, first);
        }
        instruments
            .into_iter()
            .map(|instrument| VenueInfo {
                id: instrument.to_string(),
                kind: instrument.kind,
                tradable: tradable(instrument),
            })
            .collect()
    }
}

/// The variables a task may set for a session, beyond the bot's contract:
/// the slot an off-hours market reopens at; and the session's regime and
/// the rung a reference plays, which only the benchmark's ladder hands a
/// process bot.
pub const SESSION_VARS: &[&str] = &["MTX_REOPEN_SLOT", "MTX_REGIME", "MTX_RUNG"];

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

/// A read-only JSON-RPC call against the session's chain: `params` and the
/// result are JSON text, and `method` one of [`RPC_METHODS`]. Synchronous:
/// it returns before the world moves. In a component the grader answers it;
/// in a process, the session's endpoint does.
pub fn rpc(method: &str, params: &str) -> Result<String, String> {
    if !RPC_METHODS.contains(&method) {
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
}

/// A venue at one step.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct VenueView {
    /// As [`VenueInfo::id`] names it.
    pub id: String,
    /// The book's best bid and ask, if any.
    pub best_bid: Option<f64>,
    pub best_ask: Option<f64>,
    /// The top levels of the book, best first, when the scenario reports
    /// depth.
    pub depth: Option<Book>,
    /// Every trade there since the previous step, oldest first, when the
    /// scenario reports a tape: `Some` and empty is a step without trades.
    pub tape: Option<Vec<mtx_core::turns::Trade>>,
    /// The strategy's position there, if the session reads it.
    pub position: Option<Position>,
}

/// The session's venues at one step, and the strategy's fills, from what
/// the run loop or the grader read: the top of the session's own market
/// and the position there, and the step's turn data, which other venues'
/// views come from. A venue's tape is its trades on the step's tape, if the
/// step carries one; its depth, the step's levels for it.
#[doc(hidden)]
pub fn venue_views(
    venues: &[VenueInfo],
    best_bid: Option<f64>,
    best_ask: Option<f64>,
    position: Position,
    tape: &[mtx_core::turns::Trade],
    depth: &[mtx_core::turns::BookLevels],
) -> (Vec<VenueView>, Vec<OwnFill>) {
    let mut fills = Vec::new();
    let views = venues
        .iter()
        .enumerate()
        .map(|(i, venue)| {
            let name = venue.id.split(':').next().unwrap_or_default();
            let levels = depth.iter().find(|d| d.venue == name);
            let trades: Vec<_> = tape.iter().filter(|t| t.venue == name).cloned().collect();
            fills.extend(
                trades
                    .iter()
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
                tape: (!tape.is_empty()).then_some(trades),
                position: own.then_some(position),
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
    /// answering it sells.
    pub side: Side,
    /// Base units wanted.
    pub size: f64,
    /// The last slot a response is taken at.
    pub deadline_slot: u64,
}

/// An order the planner made of a line of the sheet, and what became of it.
#[derive(Debug, Clone, PartialEq)]
pub struct SendStatus {
    /// The step whose sheet planned it, and its line on the sheet, from 0.
    pub step: u64,
    pub line: u32,
    /// The venue it went to, as [`VenueInfo::id`] names it.
    pub venue: String,
    pub order: Action,
    pub state: SendState,
}

/// What became of an order.
#[derive(Debug, Clone, PartialEq)]
pub enum SendState {
    /// It executed: its signature.
    Landed(String),
    /// It was sent and failed: why.
    Failed(String),
    /// It wasn't sent: why.
    NotSent(String),
    /// It is held, and may land in a later step.
    Delayed,
    /// Its blockhash expired before it landed.
    Expired,
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
    Target { position: f64, execution: Execution },
    /// Cancel every resting order, then rest a bid and an ask of `size`.
    Quote { bid: f64, ask: f64, size: f64 },
    /// A market order of `size`, filling at no worse than `limit`.
    Market { side: Side, size: f64, limit: f64 },
    /// A resting order of `size` at `price`.
    Limit { side: Side, price: f64, size: f64 },
    /// Cancel every resting order.
    CancelAll,
    /// Answer the request for a quote `id` ([`Rfq`]): `price` for `size`, or
    /// for the whole request.
    Respond {
        id: String,
        price: f64,
        size: Option<f64>,
    },
}

/// How a limit order, or a quote, lives.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum TimeInForce {
    /// Rests until it fills or is cancelled, filling first against anything
    /// it crosses.
    #[default]
    Gtc,
    /// Rests without taking: on Phoenix, one that would cross the book
    /// isn't placed.
    PostOnly,
    /// Fills what it can now, at no worse than its price, and rests nothing.
    Ioc,
}

/// How a line's transactions are sent: a priority fee, a tip, a bundle.
/// Every sheet may carry them; a session applies them only if it says so
/// ([`Session::applies_tx_options`]), and none does yet.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TxOptions {
    /// Compute units the transaction asks for.
    pub cu_limit: Option<u32>,
    /// The priority fee: micro-lamports per compute unit.
    pub cu_price: Option<u64>,
    /// A tip to the block builder, in lamports.
    pub tip: Option<u64>,
    /// Lines with the same bundle id are sent as one bundle.
    pub bundle: Option<u32>,
}

/// What a line of the sheet says beyond its intent. The default is the
/// session's own market, orders that rest until cancelled, and no other
/// limit than the cap.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct LineOptions {
    /// One of [`Session::venues`]' ids; `None` is the first, the session's
    /// own market.
    pub venue: Option<String>,
    /// For a limit order or a quote.
    pub time_in_force: TimeInForce,
    /// Orders the line plans may only shrink the position, never grow or
    /// flip it.
    pub reduce_only: bool,
    /// For a target: the most of the venue's traded volume since the
    /// previous step to trade this step, a share in (0, 1].
    pub participation: Option<f64>,
    pub tx: Option<TxOptions>,
}

/// The options of a line that says nothing more.
const DEFAULT_OPTIONS: LineOptions = LineOptions {
    venue: None,
    time_in_force: TimeInForce::Gtc,
    reduce_only: false,
    participation: None,
    tx: None,
};

/// The step's order sheet: what the strategy wants, which the grader (or
/// the process run loop) cuts to the mandate's cap and sends after
/// [`Strategy::step`] returns.
///
/// Each call adds a line; [`Orders::at`], [`Orders::post_only`],
/// [`Orders::ioc`], [`Orders::reduce_only`], [`Orders::participation`] and
/// [`Orders::tx`] change the line added last:
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

    /// Answer the request for a quote `id` with `price`, for all of it.
    pub fn respond(&mut self, id: &str, price: f64) -> &mut Self {
        self.add(Intent::Respond {
            id: id.to_string(),
            price,
            size: None,
        })
    }

    /// The last line is for `venue`, one of [`Session::venues`]' ids.
    pub fn at(&mut self, venue: &str) -> &mut Self {
        self.last(|o| o.venue = Some(venue.to_string()))
    }

    /// The last line's resting orders take nothing: on Phoenix, one that
    /// would cross the book isn't placed.
    pub fn post_only(&mut self) -> &mut Self {
        self.last(|o| o.time_in_force = TimeInForce::PostOnly)
    }

    /// The last line, a limit order, fills what it can now and rests
    /// nothing.
    pub fn ioc(&mut self) -> &mut Self {
        self.last(|o| o.time_in_force = TimeInForce::Ioc)
    }

    /// The last line's orders may only shrink the position.
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

/// A concrete order, as [`plan`] turns intentions into them.
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
    /// A quote answering a request.
    Respond {
        id: String,
        price: f64,
        size: Option<f64>,
    },
}

/// An order [`plan_sheet`] made of a line of the sheet.
#[derive(Debug, Clone, PartialEq)]
pub struct Planned {
    /// The line it came from, from 0.
    pub line: usize,
    /// The line's venue: `None` is the session's own market.
    pub venue: Option<String>,
    pub action: Action,
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
    let (planned, notes) = plan_lines(intents.iter().map(|i| (i, &DEFAULT_OPTIONS)), market);
    (planned.into_iter().map(|p| p.action).collect(), notes)
}

/// [`plan`], for a sheet's lines and their options. Each venue keeps its own
/// count of position and resting orders: the session's own market starts
/// from the market's, another from its [`VenueView`]'s position, with
/// nothing resting. The cap is the mandate's on every venue.
///
/// - A reduce-only line is cut, as the cap cuts, so that no fill can take
///   the position past zero.
/// - An immediate-or-cancel limit order is a market order bounded by its
///   price; a post-only one rests as [`Action::PostOnly`]. A quote may be
///   post-only.
/// - A target's participation cuts each step's trading to its share of the
///   volume on the venue's tape since the previous step. A venue without a
///   tape has no volume to take a share of: the target doesn't trade, and a
///   note says so.
/// - A target on another venue trades at no worse than its book's mid,
///   moved by the slippage.
pub fn plan_sheet(orders: &Orders, market: &Market) -> (Vec<Planned>, Vec<String>) {
    plan_lines(orders.lines(), market)
}

/// One venue's count, as the planner goes down the sheet.
struct Ledger {
    venue: Option<String>,
    position: f64,
    bids: f64,
    asks: f64,
}

fn plan_lines<'a>(
    lines: impl IntoIterator<Item = (&'a Intent, &'a LineOptions)>,
    market: &Market,
) -> (Vec<Planned>, Vec<String>) {
    let cap = market.cap;
    let own = market.venues.first().map(|v| v.id.as_str());
    let mut ledgers = vec![Ledger {
        venue: None,
        position: market.position.size,
        bids: market.resting_bids,
        asks: market.resting_asks,
    }];
    let mut planned = Vec::new();
    let mut notes = Vec::new();
    let room = |position: f64, bids: f64, asks: f64, side: Side| {
        let (buy, sell) = mtx_core::mandate::position_room(cap, position, bids, asks);
        match side {
            Side::Bid => buy,
            Side::Ask => sell,
        }
    };
    // What a reduce-only order may trade: back to zero, counting what rests
    // on its side as filled.
    let reduce_room = |position: f64, bids: f64, asks: f64, side: Side| match side {
        Side::Bid => (-(position + bids)).max(0.0),
        Side::Ask => (position - asks).max(0.0),
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
                ledgers.push(Ledger {
                    venue: venue.clone(),
                    position: view.and_then(|v| v.position).map_or(0.0, |p| p.size),
                    bids: 0.0,
                    asks: 0.0,
                });
                ledgers.len() - 1
            }
        };
        let Ledger {
            position,
            bids,
            asks,
            ..
        } = &mut ledgers[index];
        let mut push = |action: Action| {
            planned.push(Planned {
                line,
                venue: venue.clone(),
                action,
                tx: options.tx,
            })
        };
        let reduce = |size: f64, room: f64, what: &str, notes: &mut Vec<String>| {
            if options.reduce_only && size > room {
                notes.push(format!(
                    "reduce-only {what} of {size} cut to {room}, so as not to grow the position"
                ));
                room
            } else {
                size
            }
        };
        match intent {
            &Intent::Target {
                position: target,
                execution,
            } => {
                let wanted = target.clamp(-cap, cap);
                if wanted != target {
                    notes.push(format!(
                        "target {target} is past the cap {cap}: aiming at {wanted}"
                    ));
                }
                if !(execution.slice > 0.0 && execution.min_trade > 0.0) {
                    notes.push("a target needs a positive slice and min_trade: not trading".into());
                    continue;
                }
                let mark = match &venue {
                    None => market.mark,
                    Some(id) => match view.and_then(|v| v.best_bid.zip(v.best_ask)) {
                        Some((bid, ask)) => (bid + ask) / 2.0,
                        None => {
                            notes.push(format!(
                                "target on {id}: no bid and ask to price it at: not trading"
                            ));
                            continue;
                        }
                    },
                };
                let mut missing =
                    (wanted - *position).clamp(-execution.max_per_step, execution.max_per_step);
                if let Some(share) = options.participation {
                    if !(share > 0.0 && share <= 1.0) {
                        notes.push(format!(
                            "participation {share} must be in (0, 1]: not trading"
                        ));
                        continue;
                    }
                    // No tape is no volume seen: nothing to take a share of.
                    let volume = view
                        .and_then(|v| v.tape.as_ref())
                        .map(|tape| tape.iter().map(|t| t.base).sum::<f64>());
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
                let room = room(*position, *bids, *asks, side);
                if missing.abs() > room {
                    notes.push(format!(
                        "target {target} would leave the position past the cap if the resting orders filled: trading {room} of {}",
                        missing.abs()
                    ));
                    missing = missing.signum() * room;
                }
                missing = missing.signum()
                    * reduce(
                        missing.abs(),
                        reduce_room(*position, *bids, *asks, side),
                        "target",
                        &mut notes,
                    );
                while missing.abs() >= execution.min_trade {
                    let size = (missing.abs() / execution.min_trade).floor() * execution.min_trade;
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
                    *position += signed(side, size);
                    missing -= signed(side, size);
                }
            }
            &Intent::Market { side, size, limit } => {
                let size = cut(
                    size,
                    room(*position, *bids, *asks, side),
                    "market order",
                    &mut notes,
                );
                let size = reduce(
                    size,
                    reduce_room(*position, *bids, *asks, side),
                    "market order",
                    &mut notes,
                );
                if size > 0.0 {
                    push(Action::Market { side, size, limit });
                    *position += signed(side, size);
                }
            }
            &Intent::Limit { side, price, size } => {
                let size = cut(
                    size,
                    room(*position, *bids, *asks, side),
                    "resting order",
                    &mut notes,
                );
                let size = reduce(
                    size,
                    reduce_room(*position, *bids, *asks, side),
                    "resting order",
                    &mut notes,
                );
                if size > 0.0 {
                    match options.time_in_force {
                        TimeInForce::Ioc => {
                            push(Action::Market {
                                side,
                                size,
                                limit: price,
                            });
                            *position += signed(side, size);
                        }
                        tif => {
                            push(if tif == TimeInForce::PostOnly {
                                Action::PostOnly { side, price, size }
                            } else {
                                Action::Limit { side, price, size }
                            });
                            match side {
                                Side::Bid => *bids += size,
                                Side::Ask => *asks += size,
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
                (*bids, *asks) = (0.0, 0.0);
                let bid_size = cut(
                    size,
                    room(*position, *bids, *asks, Side::Bid),
                    "bid",
                    &mut notes,
                );
                let ask_size = cut(
                    size,
                    room(*position, *bids, *asks, Side::Ask),
                    "ask",
                    &mut notes,
                );
                let bid_size = reduce(
                    bid_size,
                    reduce_room(*position, *bids, *asks, Side::Bid),
                    "bid",
                    &mut notes,
                );
                let ask_size = reduce(
                    ask_size,
                    reduce_room(*position, *bids, *asks, Side::Ask),
                    "ask",
                    &mut notes,
                );
                if bid_size > 0.0 {
                    push(rest(Side::Bid, bid, bid_size));
                    *bids += bid_size;
                }
                if ask_size > 0.0 {
                    push(rest(Side::Ask, ask, ask_size));
                    *asks += ask_size;
                }
            }
            Intent::CancelAll => {
                push(Action::CancelAll);
                (*bids, *asks) = (0.0, 0.0);
            }
            Intent::Respond { id, price, size } => push(Action::Respond {
                id: id.clone(),
                price: *price,
                size: *size,
            }),
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
            Action::Market { .. } | Action::Respond { .. } => (bids, asks),
        })
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
        }
    }

    #[test]
    fn participation_takes_a_share_of_the_tape() {
        let venues =
            VenueInfo::of_perp_session(&Mandate::perp("phoenix", "SPCX", 70.0, 1_000.0), "SPCX");
        let mut orders = Orders::new();
        orders.target_position(70.0).participation(0.25);
        let traded = |tape: &[mtx_core::turns::Trade]| {
            let (views, _) = venue_views(&venues, None, None, Position::default(), tape, &[]);
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
        let (size, notes) = traded(&[trade(30.0), trade(10.0)]);
        assert_eq!(size, 10.0);
        assert!(notes[0].contains("participation 0.25"), "{notes:?}");
        // No tape, no volume: no trading.
        let (size, notes) = traded(&[]);
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
        let venues = VenueInfo::of_perp_session(&mandate, "SPCX");
        assert_eq!(
            venues.iter().map(|v| v.id.as_str()).collect::<Vec<_>>(),
            ["phoenix:perp:SPCX", "openbook:spot:SOL-USDC"]
        );
        assert!(venues[0].tradable && !venues[1].tradable);
        let (views, _) = venue_views(&venues, None, None, Position::default(), &[], &[]);
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
        let venues =
            VenueInfo::of_perp_session(&Mandate::perp("phoenix", "SPCX", 70.0, 1_000.0), "SPCX");
        let mut taken = trade(2.0);
        taken.own = Some(Role::Taker);
        let mut made = trade(3.0);
        made.own = Some(Role::Maker);
        let (views, fills) = venue_views(
            &venues,
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
