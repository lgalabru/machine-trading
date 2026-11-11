//! The run loop: write a [`Strategy`], and [`run`] does the rest.
//!
//! A bot is the same plumbing every time: answer the grader's hello, read
//! the session, connect to the venue, and then, every step, read the market,
//! decide, send orders, and hand the turn back, all without breaking the
//! mandate. [`run`] does all of it, and calls the strategy with what it needs:
//!
//! ```no_run
//! use mtx_kit::prelude::*;
//!
//! #[derive(Default)]
//! struct FadeTheDip {
//!     peak: f64,
//! }
//!
//! impl Strategy for FadeTheDip {
//!     async fn step(&mut self, market: &Market, orders: &mut Orders<'_>) -> BotResult {
//!         self.peak = self.peak.max(market.mark);
//!         if market.mark < self.peak * 0.97 {
//!             orders.target_position(market.cap);
//!         } else if market.mark >= self.peak {
//!             orders.target_position(0.0);
//!         }
//!         Ok(())
//!     }
//! }
//!
//! fn main() -> BotResult {
//!     mtx_kit::run(FadeTheDip::default())
//! }
//! ```
//!
//! - [`Strategy::step`] gets a [`Market`]: a plain value with the step, the
//!   prices, the position and the mandate's room, read for it before the
//!   call. It fills in [`Orders`], a sheet of intentions such as "be at this
//!   position" or "quote these prices", which the run loop turns into
//!   transactions after the call, inside the mandate's position cap.
//! - Because a market is a plain value and a sheet records what it's asked,
//!   a strategy can be tested without a chain: build markets, call `step`,
//!   and read [`Orders::intents`] or [`plan`] them.
//! - For anything the sheet doesn't say, [`Orders::trader`] is the venue's
//!   [`Trader`], and everything under it.

use std::{
    error::Error as StdError,
    time::{Duration, Instant},
};

use tracing::{info, warn};

use crate::{
    BotEnv, Mandate, Position, Side, Trader, Turns,
    turns::{BookLevels, Hello, Trade},
};

/// What a strategy's hooks and [`run`] return: any error, so a strategy can
/// use `?` on whatever it calls.
pub type BotResult<T = ()> = std::result::Result<T, Box<dyn StdError + Send + Sync>>;

/// What all of a session's turns may take together, per step (see the bot
/// contract): the run loop reports what's left as [`Market::budget_left`].
const TURN_BUDGET_PER_STEP: Duration = Duration::from_millis(500);

/// A trading strategy. Implement [`Strategy::step`]; the other hooks are
/// optional.
#[allow(async_fn_in_trait)]
pub trait Strategy {
    /// Once, before the first step: the session's clock, mandate and venue.
    /// Read what the strategy needs once here, such as the book's shape.
    async fn start(&mut self, session: &Session) -> BotResult {
        let _ = session;
        Ok(())
    }

    /// Every step: decide, from the market as it is now, and fill in the
    /// order sheet. The run loop sends it when this returns, then hands the
    /// turn back. An error skips this step's orders, and the session goes on.
    async fn step(&mut self, market: &Market, orders: &mut Orders<'_>) -> BotResult;

    /// After the last step, when the grader says the session is over. The
    /// account is valued at the last step: there is nothing left to send.
    fn end(&mut self, last: Option<&Market>) {
        let _ = last;
    }
}

/// The session, as the strategy's [`Strategy::start`] sees it.
pub struct Session {
    pub env: BotEnv,
    pub mandate: Mandate,
    /// The session's clock: its steps, slots and per-turn limit.
    pub hello: Hello,
    /// The venue, for reads the run loop doesn't make.
    pub trader: Trader,
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
    /// The position, in base units (positive is long), and its entry.
    pub position: Position,
    /// The mandate's position cap and drawdown limit.
    pub cap: f64,
    pub max_drawdown: f64,
    /// The bids and asks the run loop has resting, in base units: every one
    /// it placed since its last cancel, filled or not. An upper bound, so
    /// that [`plan`] can keep the position within the cap even if they all
    /// fill: a fill already in `position` is counted again until the next
    /// cancel, which only makes the sheet cut sooner. Orders sent through
    /// [`Orders::trader`] aren't counted.
    pub resting_bids: f64,
    pub resting_asks: f64,
    /// The account's PnL since the first step, marked to market: the
    /// position held times each step's move in the mark. An estimate: it
    /// leaves out fees, funding and slippage, which the grader counts.
    pub pnl: f64,
    /// The highest `pnl` so far, and how far `pnl` is below it.
    pub peak_pnl: f64,
    pub drawdown: f64,
    /// What's left of the session's turn budget (500 ms a step, summed).
    pub budget_left: Duration,
    /// The step's turn data, when the scenario reports it: every trade since
    /// the previous step on every venue, the top of each book, and the
    /// world's clock in Unix seconds.
    pub tape: Vec<Trade>,
    pub depth: Vec<BookLevels>,
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

/// The step's order sheet: what the strategy wants, which the run loop sends
/// after [`Strategy::step`] returns.
pub struct Orders<'a> {
    intents: Vec<Intent>,
    trader: Option<&'a Trader>,
}

impl<'a> Orders<'a> {
    /// A sheet that only records, for tests: [`Orders::trader`] is `None`.
    pub fn recording() -> Self {
        Orders {
            intents: Vec::new(),
            trader: None,
        }
    }

    fn sending(trader: &'a Trader) -> Self {
        Orders {
            intents: Vec::new(),
            trader: Some(trader),
        }
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

    /// The venue, for what the sheet doesn't say: whatever it sends goes out
    /// at once, before the sheet. `None` on a [`Orders::recording`] sheet.
    pub fn trader(&self) -> Option<&'a Trader> {
        self.trader
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

/// Run a strategy for one session: the whole bot. Call it from `main`.
///
/// It answers the grader's hello, reads the session, connects to the venue,
/// and calls [`Strategy::start`]. Then, every step, it reads the market,
/// calls [`Strategy::step`], sends the sheet, and hands the turn back, until
/// the grader ends the session. Logs go to standard error, at the level
/// `RUST_LOG` names.
pub fn run<S: Strategy>(strategy: S) -> BotResult {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .try_init();
    // Multi-threaded: the blocking RPC client underneath the trader only
    // runs on that kind of runtime.
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?
        .block_on(session(strategy))
}

async fn session<S: Strategy>(mut strategy: S) -> BotResult {
    // First: the world waits for the bot's answer to the hello.
    let mut turns = Turns::connect()?;
    let hello = turns.hello().clone();
    let env = BotEnv::from_env()?;
    let mandate = env.mandate()?;
    let trader = Trader::connect(&env)?;
    let session = Session {
        env,
        mandate,
        hello,
        trader,
    };
    strategy.start(&session).await?;
    info!(authority = %session.trader.authority(), end_slot = session.hello.end_slot, "started");

    let cap = session.env.max_position;
    let max_drawdown = session.env.max_drawdown_usd;
    let budget = TURN_BUDGET_PER_STEP * session.hello.steps as u32;
    let mut used = Duration::ZERO;
    let (mut pnl, mut peak_pnl, mut last_mark): (f64, f64, Option<f64>) = (0.0, 0.0, None);
    let mut last_market: Option<Market> = None;
    // What the loop has resting: see [`Market::resting_bids`].
    let (mut resting_bids, mut resting_asks) = (0.0, 0.0);
    while let Some(step) = turns.next()? {
        let started = Instant::now();
        let (mark, oracle, position) = match (
            session.trader.mark_usd().await,
            session.trader.oracle_usd().await,
            session.trader.position().await,
        ) {
            (Ok(mark), Ok(oracle), Ok(position)) => (mark, oracle, position),
            (mark, oracle, position) => {
                warn!(step = step.k, mark = ?mark.err(), oracle = ?oracle.err(), position = ?position.err(), "couldn't read the market; skipping the step");
                used += started.elapsed();
                continue;
            }
        };
        if let Some(last) = last_mark {
            pnl += position.size * (mark - last);
        }
        last_mark = Some(mark);
        peak_pnl = peak_pnl.max(pnl);
        let market = Market {
            k: step.k,
            slot: step.slot,
            last: step.last,
            steps_left: session.hello.steps.saturating_sub(step.k + 1),
            slots_left: session.hello.end_slot.saturating_sub(step.slot),
            mark,
            oracle,
            position,
            cap,
            max_drawdown,
            resting_bids,
            resting_asks,
            pnl,
            peak_pnl,
            drawdown: peak_pnl - pnl,
            budget_left: budget.saturating_sub(used),
            tape: step.tape,
            depth: step.depth,
            unix_timestamp: step.unix_timestamp,
        };
        let mut orders = Orders::sending(&session.trader);
        match strategy.step(&market, &mut orders).await {
            Ok(()) => {
                let (actions, notes) = plan(orders.intents(), &market);
                for note in notes {
                    warn!(step = step.k, "{note}");
                }
                send(&session.trader, &actions).await;
                // A resting order that failed to send counts anyway: the
                // ledger only ever over-counts.
                (resting_bids, resting_asks) = resting_after(resting_bids, resting_asks, &actions);
            }
            Err(error) => warn!(step = step.k, %error, "the strategy failed; no orders this step"),
        }
        used += started.elapsed();
        last_market = Some(market);
    }
    strategy.end(last_market.as_ref());
    Ok(())
}

/// Call a strategy's [`Strategy::step`] on a market you built, without a
/// chain, and return what it put on the sheet: for tests. The sheet's
/// [`Orders::trader`] is `None`.
///
/// ```
/// use mtx_kit::prelude::*;
///
/// struct AlwaysLong;
/// impl Strategy for AlwaysLong {
///     async fn step(&mut self, market: &Market, orders: &mut Orders<'_>) -> BotResult {
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
    tokio::runtime::Builder::new_current_thread()
        .build()?
        .block_on(async {
            let mut orders = Orders::recording();
            strategy.step(market, &mut orders).await?;
            Ok(orders.intents)
        })
}

/// Send planned orders, in order. A failed order is logged, and the rest of
/// the step's orders still go: each is its own transaction.
async fn send(trader: &Trader, actions: &[Action]) {
    for action in actions {
        let result = match *action {
            Action::Market { side, size, limit } => {
                trader.place_market(side, size, limit).await.map(|_| ())
            }
            Action::Limit { side, price, size } => {
                trader.place_limit(side, price, size).await.map(|_| ())
            }
            Action::CancelAll => trader.cancel_all().map(|_| ()),
        };
        if let Err(error) = result {
            warn!(?action, %error, "order failed");
        }
    }
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
    fn a_recording_sheet_keeps_what_it_was_asked() {
        let mut orders = Orders::recording();
        orders.cancel_all().buy(3.0, 101.0);
        assert_eq!(orders.intents().len(), 2);
        assert!(orders.trader().is_none());
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
