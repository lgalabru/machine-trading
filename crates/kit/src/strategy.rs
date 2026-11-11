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
}

/// The variables a task may set for a session, beyond the bot's contract:
/// the slot an off-hours market reopens at, and the session's regime, which
/// only the benchmark's ladder hands a bot.
pub const SESSION_VARS: &[&str] = &["MTX_REOPEN_SLOT", "MTX_REGIME"];

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
    /// book, and the world's clock in Unix seconds. A component gets none
    /// yet: no task that runs one reports it.
    pub tape: Vec<mtx_core::turns::Trade>,
    pub depth: Vec<mtx_core::turns::BookLevels>,
    pub unix_timestamp: Option<i64>,
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
}

/// The step's order sheet: what the strategy wants, which the grader (or
/// the process run loop) cuts to the mandate's cap and sends after
/// [`Strategy::step`] returns.
#[derive(Debug, Clone, Default)]
pub struct Orders {
    intents: Vec<Intent>,
}

impl Orders {
    /// An empty sheet.
    pub fn new() -> Self {
        Self::default()
    }

    /// What the sheet holds, taken out of it.
    pub fn into_intents(self) -> Vec<Intent> {
        self.intents
    }

    /// Be at `position` (base units, positive long), trading toward it with
    /// market orders, as [`Execution::default`] says. The run loop keeps the
    /// target within the mandate's cap.
    pub fn target_position(&mut self, position: f64) -> &mut Self {
        self.target_position_with(position, Execution::default())
    }

    /// [`Orders::target_position`], with its own pacing.
    pub fn target_position_with(&mut self, position: f64, execution: Execution) -> &mut Self {
        self.intents.push(Intent::Target {
            position,
            execution,
        });
        self
    }

    /// Replace the resting quotes with a bid and an ask of `size` each, each
    /// cut to what can fill without taking the position past the cap.
    pub fn quote(&mut self, bid: f64, ask: f64, size: f64) -> &mut Self {
        self.intents.push(Intent::Quote { bid, ask, size });
        self
    }

    /// Buy `size` now, at no more than `limit`.
    pub fn buy(&mut self, size: f64, limit: f64) -> &mut Self {
        self.intents.push(Intent::Market {
            side: Side::Bid,
            size,
            limit,
        });
        self
    }

    /// Sell `size` now, at no less than `limit`.
    pub fn sell(&mut self, size: f64, limit: f64) -> &mut Self {
        self.intents.push(Intent::Market {
            side: Side::Ask,
            size,
            limit,
        });
        self
    }

    /// Rest a bid of `size` at `price`.
    pub fn bid(&mut self, price: f64, size: f64) -> &mut Self {
        self.intents.push(Intent::Limit {
            side: Side::Bid,
            price,
            size,
        });
        self
    }

    /// Rest an ask of `size` at `price`.
    pub fn ask(&mut self, price: f64, size: f64) -> &mut Self {
        self.intents.push(Intent::Limit {
            side: Side::Ask,
            price,
            size,
        });
        self
    }

    /// Cancel every resting order.
    pub fn cancel_all(&mut self) -> &mut Self {
        self.intents.push(Intent::CancelAll);
        self
    }

    /// What the sheet holds so far.
    pub fn intents(&self) -> &[Intent] {
        &self.intents
    }
}

/// A concrete order, as [`plan`] turns intentions into them.
#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    Market { side: Side, size: f64, limit: f64 },
    Limit { side: Side, price: f64, size: f64 },
    CancelAll,
}

/// Turn a sheet's intentions into orders, in order, for a market. Every order
/// is cut so that the position stays within the cap even if every order
/// resting fills: the market's [`Market::resting_bids`] and
/// [`Market::resting_asks`], and every resting order the sheet adds.
/// Market orders and targets count as filled. A cancel, or a quote, which
/// cancels first, clears what was resting. Returns the orders, and a note for
/// each one it cut or dropped.
pub fn plan(intents: &[Intent], market: &Market) -> (Vec<Action>, Vec<String>) {
    let cap = market.cap;
    let mut position = market.position.size;
    let (mut bids, mut asks) = (market.resting_bids, market.resting_asks);
    let mut actions = Vec::new();
    let mut notes = Vec::new();
    let room = |position: f64, bids: f64, asks: f64, side: Side| {
        let (buy, sell) = mtx_core::mandate::position_room(cap, position, bids, asks);
        match side {
            Side::Bid => buy,
            Side::Ask => sell,
        }
    };
    for intent in intents {
        match *intent {
            Intent::Target {
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
                let mut missing =
                    (wanted - position).clamp(-execution.max_per_step, execution.max_per_step);
                // Resting orders could fill too: trade only what keeps the
                // position within the cap if they do.
                let side = if missing > 0.0 { Side::Bid } else { Side::Ask };
                let room = room(position, bids, asks, side);
                if missing.abs() > room {
                    notes.push(format!(
                        "target {target} would leave the position past the cap if the resting orders filled: trading {room} of {}",
                        missing.abs()
                    ));
                    missing = missing.signum() * room;
                }
                while missing.abs() >= execution.min_trade {
                    let size = (missing.abs() / execution.min_trade).floor() * execution.min_trade;
                    let size = size.min(execution.slice);
                    let side = if missing > 0.0 { Side::Bid } else { Side::Ask };
                    let limit = match side {
                        Side::Bid => market.mark * (1.0 + execution.slippage_bps / 10_000.0),
                        Side::Ask => market.mark * (1.0 - execution.slippage_bps / 10_000.0),
                    };
                    actions.push(Action::Market {
                        side,
                        size,
                        limit: round_cents(limit),
                    });
                    position += signed(side, size);
                    missing -= signed(side, size);
                }
            }
            Intent::Market { side, size, limit } => {
                let size = cut(
                    size,
                    room(position, bids, asks, side),
                    "market order",
                    &mut notes,
                );
                if size > 0.0 {
                    actions.push(Action::Market { side, size, limit });
                    position += signed(side, size);
                }
            }
            Intent::Limit { side, price, size } => {
                let size = cut(
                    size,
                    room(position, bids, asks, side),
                    "resting order",
                    &mut notes,
                );
                if size > 0.0 {
                    actions.push(Action::Limit { side, price, size });
                    match side {
                        Side::Bid => bids += size,
                        Side::Ask => asks += size,
                    }
                }
            }
            Intent::Quote { bid, ask, size } => {
                actions.push(Action::CancelAll);
                (bids, asks) = (0.0, 0.0);
                let bid_size = cut(
                    size,
                    room(position, bids, asks, Side::Bid),
                    "bid",
                    &mut notes,
                );
                let ask_size = cut(
                    size,
                    room(position, bids, asks, Side::Ask),
                    "ask",
                    &mut notes,
                );
                if bid_size > 0.0 {
                    actions.push(Action::Limit {
                        side: Side::Bid,
                        price: bid,
                        size: bid_size,
                    });
                    bids += bid_size;
                }
                if ask_size > 0.0 {
                    actions.push(Action::Limit {
                        side: Side::Ask,
                        price: ask,
                        size: ask_size,
                    });
                    asks += ask_size;
                }
            }
            Intent::CancelAll => {
                actions.push(Action::CancelAll);
                (bids, asks) = (0.0, 0.0);
            }
        }
    }
    (actions, notes)
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
            } => (bids + size, asks),
            Action::Limit {
                side: Side::Ask,
                size,
                ..
            } => (bids, asks + size),
            Action::Market { .. } => (bids, asks),
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
}
