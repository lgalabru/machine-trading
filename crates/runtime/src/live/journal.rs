//! What a hosted strategy saw, decided and sent at each step, kept to go
//! back to: `logs/<strategy>.steps.jsonl`, one line per step. Replayed
//! through a changed strategy, it shows what the change would have done
//! on the same market.
//!
//! A shadow strategy's orders never reach the book, so they never fill, and
//! its PnL says nothing about its prices. For it, [`PaperBook`] keeps the
//! orders it would have resting and fills them against the market that
//! follows, then marks each fill to the mid 1, 10 and 60 seconds later:
//! the markout, the number a market maker tunes on. Fills and markouts go
//! to `logs/<strategy>.fills.jsonl` and to metrics.
//!
//! Fills come from the market's real trades ([`super::tape`]) and from the
//! book moving through a quote, under two models that bracket what the
//! book would have done, each with its own paper book:
//!
//! - **through**: a real trade at a worse price than the quote (a taker's
//!   sale below a paper bid) fills it, since price priority puts the quote
//!   first; so does the book's far side reaching it. A lower bound.
//! - **touch**: trades at the quote's own price fill it too, as if it were
//!   first in the queue there. An upper bound: where the queue really
//!   starts is unknown.
//!
//! Without the tape (a transaction notification the host doesn't send),
//! only the book's moves fill, and both models agree.

use std::{
    collections::VecDeque,
    io::Write,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{SystemTime, UNIX_EPOCH},
};

use mtx_kit::{Action, SendState, Side, protocols::phoenix::eternal::PhoenixMarket};
use serde_json::{Value, json};

use super::{
    feed::Watch,
    tape::{self},
};
use crate::desk::{StepObserver, StepTrace};

/// A journal past this is moved aside to `<name>.1` and started again: at
/// about 1.5 KB a step and 2.5 steps a second, a day of steps, and one
/// day kept before it.
const JOURNAL_BYTES_MAX: u64 = 512 * 1024 * 1024;

/// The markout horizons, in slots: about 1, 10 and 60 seconds at 400 ms a
/// slot.
const MARKOUT_SLOTS: [u64; 3] = [3, 25, 150];

/// Orders resting in the paper book at most: a quoting strategy has two;
/// past this the oldest go, as if cancelled.
const PAPER_RESTING_MAX: usize = 64;

/// Sizes under this are nothing left: base units, far under a lot.
const SIZE_EPSILON: f64 = 1e-12;

/// Prices this close, relative, are the same tick: a tick is 1e-4 of the
/// price or more.
const PRICE_EPSILON: f64 = 1e-7;

/// Fills waiting on their markouts at most: at 60 s for the longest
/// horizon, 1024 is more than a fill every 60 ms.
const PENDING_MARKOUTS_MAX: usize = 1024;

/// A hosted strategy's journals: handed to its session as the observer of
/// each step.
pub struct Journal {
    strategy: String,
    events: super::events::Events,
    steps: Mutex<Appender>,
    paper: Option<Mutex<Paper>>,
}

struct Paper {
    books: [PaperBook; 2],
    fills: Appender,
    /// The tape, its reader's cursor, and the market its fills are read
    /// for.
    tape: Option<TapeReader>,
}

/// Where a journal reads its market's trades.
pub struct TapeReader {
    pub watch: Arc<Watch>,
    pub market: PhoenixMarket,
    cursor: Option<u64>,
}

impl TapeReader {
    pub fn new(watch: Arc<Watch>, market: PhoenixMarket) -> Self {
        Self {
            watch,
            market,
            cursor: None,
        }
    }

    /// The market's trades since the last read, in order. `reader` names
    /// who reads, for the logs.
    pub fn trades(&mut self, reader: &str) -> Vec<Trade> {
        let strategy = reader;
        let (read, cursor) = self.watch.tape.since(self.cursor);
        self.cursor = Some(cursor);
        if read.missed > 0 {
            tracing::warn!(
                strategy,
                missed = read.missed,
                counter.mtx_tape_missed = read.missed,
                "the tape ran ahead of the paper book"
            );
        }
        let (fills, undecoded) = tape::fills(&read.transactions, &self.market);
        if undecoded > 0 {
            tracing::warn!(
                strategy,
                undecoded,
                counter.mtx_tape_undecoded = undecoded,
                "tape transactions that didn't decode"
            );
        }
        fills
            .iter()
            .filter_map(|fill| {
                let price: f64 = fill.price.to_string().parse().ok()?;
                let size: f64 = fill.quantity.to_string().parse().ok()?;
                let taker = match fill.taker_side {
                    mtx_core::Side::Buy => Side::Bid,
                    mtx_core::Side::Sell => Side::Ask,
                };
                (price > 0.0 && size > 0.0).then_some(Trade {
                    slot: fill.slot,
                    taker,
                    price,
                    size,
                })
            })
            .collect()
    }
}

impl Journal {
    /// `strategy`'s journals in `logs`; a shadow strategy's paper fills
    /// with them.
    /// `tape`, where the market's trades are read, fills the paper books
    /// at the touch; without it, only the book's moves do.
    pub fn open(
        logs: &Path,
        strategy: &str,
        shadow: bool,
        tape: Option<TapeReader>,
        events: super::events::Events,
    ) -> Result<Self, String> {
        let steps = Appender::open(logs.join(format!("{strategy}.steps.jsonl")))?;
        let paper = match shadow {
            true => Some(Mutex::new(Paper {
                books: [
                    PaperBook::new(FillModel::Through),
                    PaperBook::new(FillModel::Touch),
                ],
                fills: Appender::open(logs.join(format!("{strategy}.fills.jsonl")))?,
                tape,
            })),
            false => None,
        };
        Ok(Self {
            strategy: strategy.to_string(),
            events,
            steps: Mutex::new(steps),
            paper,
        })
    }

    fn paper_step(&self, trace: &StepTrace, unix_ms: u64) {
        let Some(paper) = &self.paper else {
            return;
        };
        let Ok(mut paper) = paper.lock() else {
            tracing::error!(strategy = %self.strategy, "the paper book was poisoned");
            return;
        };
        let top = Top::of(trace);
        let trades = match paper.tape.as_mut() {
            Some(tape) => tape.trades(&self.strategy),
            None => Vec::new(),
        };
        let Paper { books, fills, .. } = &mut *paper;
        for book in books.iter_mut() {
            let events = book.step(trace.market.slot, unix_ms, top, &trades, &trace.sent);
            for event in &events {
                self.emit(event, book);
                fills.write(&event.to_json(&self.strategy, book.model));
            }
            if let Some(mid) = top.mid() {
                tracing::info!(
                    gauge.mtx_paper_position = book.position,
                    gauge.mtx_paper_pnl_usd = book.pnl(mid),
                    gauge.mtx_paper_at_touch = u64::from(book.at_touch(top)),
                    strategy = %self.strategy,
                    model = book.model.name(),
                    "the paper book",
                );
            }
        }
        if !trades.is_empty() {
            tracing::info!(counter.mtx_tape_trades = trades.len() as u64, strategy = %self.strategy, "the market's trades");
        }
    }

    fn emit(&self, event: &PaperEvent, book: &PaperBook) {
        match event {
            PaperEvent::Fill(fill) => tracing::info!(
                counter.mtx_paper_fills = 1,
                histogram.mtx_paper_fill_edge_bps = fill.edge_bps,
                strategy = %self.strategy,
                side = side_name(fill.side),
                liquidity = fill.liquidity.name(),
                model = book.model.name(),
                price = fill.price,
                size = fill.size,
                position = book.position,
                "a paper fill",
            ),
            PaperEvent::Markout {
                fill,
                horizon_slots,
                bps,
            } => tracing::info!(
                histogram.mtx_paper_markout_bps = *bps,
                strategy = %self.strategy,
                horizon = horizon_name(*horizon_slots),
                side = side_name(fill.side),
                liquidity = fill.liquidity.name(),
                model = book.model.name(),
                "a paper fill's markout",
            ),
        }
        let model = match book.model {
            FillModel::Through => mtx_core::events::PaperModel::Through,
            FillModel::Touch => mtx_core::events::PaperModel::Touch,
        };
        let side = |side| match side {
            Side::Bid => mtx_core::events::Side::Buy,
            Side::Ask => mtx_core::events::Side::Sell,
        };
        self.events.emit(match event {
            PaperEvent::Fill(fill) => mtx_core::events::Event::PaperFill {
                strategy: self.strategy.clone(),
                model,
                slot: fill.slot,
                side: side(fill.side),
                price: fill.price,
                size: fill.size,
                liquidity: match fill.liquidity {
                    Liquidity::Maker => mtx_core::events::Liquidity::Maker,
                    Liquidity::Taker => mtx_core::events::Liquidity::Taker,
                },
                edge_bps: fill.edge_bps,
                position: book.position,
            },
            PaperEvent::Markout {
                fill,
                horizon_slots,
                bps,
            } => mtx_core::events::Event::Markout {
                strategy: self.strategy.clone(),
                model,
                fill_slot: fill.slot,
                horizon_slots: *horizon_slots,
                bps: *bps,
            },
        });
    }
}

impl StepObserver for Journal {
    fn started(&self, _session: &mtx_kit::Session, printed: &[String], failed: Option<&str>) {
        let line = json!({ "started": true, "printed": printed, "failed": failed });
        match self.steps.lock() {
            Ok(mut steps) => steps.write(&line),
            Err(_) => tracing::error!(strategy = %self.strategy, "the step journal was poisoned"),
        }
    }

    fn step(&self, trace: &StepTrace) {
        let unix_ms = now_ms();
        match self.steps.lock() {
            Ok(mut steps) => steps.write(&step_line(trace, unix_ms)),
            Err(_) => tracing::error!(strategy = %self.strategy, "the step journal was poisoned"),
        }
        let quotes: Vec<String> = trace.sent.iter().map(|(a, _)| action_brief(a)).collect();
        tracing::info!(
            strategy = %self.strategy,
            slot = trace.market.slot,
            mark = trace.market.mark,
            oracle = trace.market.oracle,
            best_bid = trace.market.best_bid,
            best_ask = trace.market.best_ask,
            position = trace.market.position.size,
            orders = quotes.join(" "),
            printed = trace.printed.join(" | "),
            failed = trace.failed.as_deref(),
            "a strategy decided",
        );
        self.paper_step(trace, unix_ms);
    }
}

/// One step's line: the market the strategy saw, what it printed and asked
/// for, what the planner cut, and what became of each order.
fn step_line(trace: &StepTrace, unix_ms: u64) -> Value {
    let market = &trace.market;
    json!({
        "k": market.k,
        "slot": market.slot,
        "unix_ms": unix_ms,
        "market": {
            "mark": market.mark,
            "oracle": market.oracle,
            "index": market.index,
            "best_bid": market.best_bid,
            "best_ask": market.best_ask,
            "position": market.position.size,
            "entry": market.position.entry_usd,
            "resting_bids": market.resting_bids,
            "resting_asks": market.resting_asks,
            "pnl": market.pnl,
            "drawdown": market.drawdown,
            "cap": market.cap,
        },
        "printed": trace.printed,
        "failed": trace.failed,
        "intents": trace.intents.iter().map(|i| format!("{i:?}")).collect::<Vec<_>>(),
        "notes": trace.notes,
        "orders": trace.sent.iter().map(|(action, state)| json!({
            "action": action_json(action),
            "state": state_json(state),
        })).collect::<Vec<_>>(),
        "fuel": trace.fuel,
        "rpc_calls": trace.rpc_calls,
    })
}

fn action_json(action: &Action) -> Value {
    match action {
        Action::Market { side, size, limit } => {
            json!({ "kind": "market", "side": side, "size": size, "limit": limit })
        }
        Action::Limit { side, price, size } => {
            json!({ "kind": "limit", "side": side, "price": price, "size": size })
        }
        Action::PostOnly { side, price, size } => {
            json!({ "kind": "post_only", "side": side, "price": price, "size": size })
        }
        Action::CancelAll => json!({ "kind": "cancel_all" }),
        Action::Cancel { client_id } => json!({ "kind": "cancel", "client_id": client_id }),
        other => json!({ "kind": "other", "debug": format!("{other:?}") }),
    }
}

fn state_json(state: &SendState) -> Value {
    match state {
        SendState::Landed(signature) => json!({ "landed": signature }),
        SendState::Failed { reason, signature } => {
            json!({ "failed": reason, "signature": signature })
        }
        SendState::NotSent(reason) => json!({ "not_sent": reason }),
        SendState::Delayed => json!("delayed"),
        SendState::Expired => json!("expired"),
    }
}

/// An order in a few characters, for a log line: `bid 0.1@142.31`.
fn action_brief(action: &Action) -> String {
    match action {
        Action::Market { side, size, .. } => format!("{}-market {size}", side_name(*side)),
        Action::Limit { side, price, size } | Action::PostOnly { side, price, size } => {
            format!("{} {size}@{price}", side_name(*side))
        }
        Action::CancelAll => "cancel-all".into(),
        Action::Cancel { client_id } => format!("cancel {client_id}"),
        _ => "other".into(),
    }
}

fn side_name(side: Side) -> &'static str {
    match side {
        Side::Bid => "bid",
        Side::Ask => "ask",
    }
}

fn horizon_name(slots: u64) -> &'static str {
    match slots {
        3 => "1s",
        25 => "10s",
        150 => "60s",
        _ => unreachable!("a horizon not in MARKOUT_SLOTS: {slots}"),
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

/// The book's best bid and ask at a step.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Top {
    pub bid: Option<f64>,
    pub ask: Option<f64>,
}

impl Top {
    fn of(trace: &StepTrace) -> Self {
        Self {
            bid: trace.market.best_bid,
            ask: trace.market.best_ask,
        }
    }

    pub fn mid(self) -> Option<f64> {
        match (self.bid, self.ask) {
            (Some(bid), Some(ask)) if bid > 0.0 && ask >= bid => Some((bid + ask) / 2.0),
            _ => None,
        }
    }
}

/// How a paper book fills against the market's trades (see the module).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FillModel {
    #[default]
    Through,
    Touch,
}

impl FillModel {
    pub fn name(self) -> &'static str {
        match self {
            FillModel::Through => "through",
            FillModel::Touch => "touch",
        }
    }
}

/// A real trade on the market: the taker's side (a taker's sale,
/// [`Side::Ask`], hits bids), its price and size.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Trade {
    pub slot: u64,
    pub taker: Side,
    pub price: f64,
    pub size: f64,
}

/// Whether a fill made or took liquidity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Liquidity {
    Maker,
    Taker,
}

impl Liquidity {
    fn name(self) -> &'static str {
        match self {
            Liquidity::Maker => "maker",
            Liquidity::Taker => "taker",
        }
    }
}

/// A paper fill.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Fill {
    pub slot: u64,
    pub unix_ms: u64,
    pub side: Side,
    pub price: f64,
    pub size: f64,
    pub liquidity: Liquidity,
    /// The mid when it filled.
    pub mid: f64,
    /// How far the fill was on the good side of that mid, in bps: the
    /// spread it captured, before the market moves.
    pub edge_bps: f64,
    /// How long the order rested, in slots (0 for a taker).
    pub rested_slots: u64,
}

/// What the paper book reports at a step.
#[derive(Debug, Clone, PartialEq)]
pub enum PaperEvent {
    Fill(Fill),
    /// `fill` marked to the mid `horizon_slots` later: positive is good.
    Markout {
        fill: Fill,
        horizon_slots: u64,
        bps: f64,
    },
}

impl PaperEvent {
    fn to_json(&self, strategy: &str, model: FillModel) -> Value {
        match self {
            PaperEvent::Fill(fill) => json!({
                "event": "fill",
                "strategy": strategy,
                "model": model.name(),
                "slot": fill.slot,
                "unix_ms": fill.unix_ms,
                "side": fill.side,
                "price": fill.price,
                "size": fill.size,
                "liquidity": fill.liquidity.name(),
                "mid": fill.mid,
                "edge_bps": fill.edge_bps,
                "rested_slots": fill.rested_slots,
            }),
            PaperEvent::Markout {
                fill,
                horizon_slots,
                bps,
            } => json!({
                "event": "markout",
                "strategy": strategy,
                "model": model.name(),
                "fill_slot": fill.slot,
                "side": fill.side,
                "price": fill.price,
                "horizon_slots": horizon_slots,
                "bps": bps,
            }),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct Resting {
    side: Side,
    price: f64,
    size: f64,
    slot: u64,
}

/// The orders a shadow strategy would have resting, filled against the
/// market as it moves.
#[derive(Debug, Default)]
pub struct PaperBook {
    pub model: FillModel,
    resting: Vec<Resting>,
    pending: VecDeque<(Fill, usize)>,
    /// The paper position, base units, and the cash its fills paid.
    pub position: f64,
    cash: f64,
}

impl PaperBook {
    pub fn new(model: FillModel) -> Self {
        Self {
            model,
            ..Self::default()
        }
    }

    /// One step: the resting orders the market's trades since the last
    /// step reached fill, then those the book moved through, the fills
    /// whose horizons came due are marked out, then the step's own orders
    /// (those that went out) rest or take.
    pub fn step(
        &mut self,
        slot: u64,
        unix_ms: u64,
        top: Top,
        trades: &[Trade],
        sent: &[(Action, SendState)],
    ) -> Vec<PaperEvent> {
        let mut events = Vec::new();
        let Some(mid) = top.mid() else {
            return events;
        };
        self.fill_trades(trades, unix_ms, mid, &mut events);
        self.fill_through(slot, unix_ms, top, mid, &mut events);
        self.mark_out(slot, mid, &mut events);
        for (action, state) in sent {
            if went_out(state) {
                self.apply(action, slot, unix_ms, top, mid, &mut events);
            }
        }
        assert!(
            self.resting.len() <= PAPER_RESTING_MAX,
            "the paper book is bounded"
        );
        events
    }

    /// The paper PnL at `mid`: cash plus the position marked there.
    pub fn pnl(&self, mid: f64) -> f64 {
        self.cash + self.position * mid
    }

    /// Whether any resting order is at or inside the book's best on its
    /// side: where a taker would meet it first.
    pub fn at_touch(&self, top: Top) -> bool {
        self.resting.iter().any(|r| match r.side {
            Side::Bid => top.bid.is_some_and(|bid| r.price >= bid),
            Side::Ask => top.ask.is_some_and(|ask| r.price <= ask),
        })
    }

    fn fill_through(
        &mut self,
        slot: u64,
        unix_ms: u64,
        top: Top,
        mid: f64,
        events: &mut Vec<PaperEvent>,
    ) {
        let mut kept = Vec::with_capacity(self.resting.len());
        for order in std::mem::take(&mut self.resting) {
            let through = match order.side {
                Side::Bid => top.ask.is_some_and(|ask| ask <= order.price),
                Side::Ask => top.bid.is_some_and(|bid| bid >= order.price),
            };
            if through {
                let fill = self.fill(
                    order.side,
                    order.price,
                    order.size,
                    Liquidity::Maker,
                    slot,
                    unix_ms,
                    mid,
                );
                events.push(PaperEvent::Fill(Fill {
                    rested_slots: slot.saturating_sub(order.slot),
                    ..fill
                }));
            } else {
                kept.push(order);
            }
        }
        self.resting = kept;
    }

    /// Each trade fills the resting orders on the side it hit that it
    /// reached, best price first, up to its size: an order placed in a
    /// slot only meets trades of later slots, since it lands after them.
    fn fill_trades(
        &mut self,
        trades: &[Trade],
        unix_ms: u64,
        mid: f64,
        events: &mut Vec<PaperEvent>,
    ) {
        for trade in trades {
            let hit = match trade.taker {
                Side::Ask => Side::Bid,
                Side::Bid => Side::Ask,
            };
            let mut left = trade.size;
            // Best price first: the highest bid, the lowest ask.
            let mut order: Vec<usize> = (0..self.resting.len()).collect();
            order.sort_by(|&a, &b| {
                let (a, b) = (self.resting[a].price, self.resting[b].price);
                match hit {
                    Side::Bid => b.total_cmp(&a),
                    Side::Ask => a.total_cmp(&b),
                }
            });
            for i in order {
                let resting = self.resting[i];
                if left <= 0.0 || resting.side != hit || trade.slot <= resting.slot {
                    continue;
                }
                if !self.reached(resting.side, resting.price, trade.price) {
                    continue;
                }
                let size = resting.size.min(left);
                left -= size;
                self.resting[i].size -= size;
                let fill = self.fill(
                    resting.side,
                    resting.price,
                    size,
                    Liquidity::Maker,
                    trade.slot,
                    unix_ms,
                    mid,
                );
                events.push(PaperEvent::Fill(Fill {
                    rested_slots: trade.slot.saturating_sub(resting.slot),
                    ..fill
                }));
            }
            self.resting.retain(|r| r.size > SIZE_EPSILON);
        }
    }

    /// Whether a trade at `traded` reaches an order on `side` at `price`:
    /// past it always, at it only at the touch.
    fn reached(&self, side: Side, price: f64, traded: f64) -> bool {
        let at = (price - traded).abs() <= price * PRICE_EPSILON;
        let past = match side {
            Side::Bid => traded < price,
            Side::Ask => traded > price,
        };
        match self.model {
            FillModel::Through => past && !at,
            FillModel::Touch => past || at,
        }
    }

    fn mark_out(&mut self, slot: u64, mid: f64, events: &mut Vec<PaperEvent>) {
        let mut waiting = VecDeque::with_capacity(self.pending.len());
        while let Some((fill, next)) = self.pending.pop_front() {
            let mut next = next;
            while next < MARKOUT_SLOTS.len() && slot >= fill.slot + MARKOUT_SLOTS[next] {
                events.push(PaperEvent::Markout {
                    fill,
                    horizon_slots: MARKOUT_SLOTS[next],
                    bps: signed_bps(fill.side, fill.price, mid),
                });
                next += 1;
            }
            if next < MARKOUT_SLOTS.len() {
                waiting.push_back((fill, next));
            }
        }
        self.pending = waiting;
    }

    fn apply(
        &mut self,
        action: &Action,
        slot: u64,
        unix_ms: u64,
        top: Top,
        mid: f64,
        events: &mut Vec<PaperEvent>,
    ) {
        match *action {
            Action::CancelAll => self.resting.clear(),
            Action::PostOnly { side, price, size } => {
                if !crosses(side, price, top) {
                    self.rest(Resting {
                        side,
                        price,
                        size,
                        slot,
                    });
                }
            }
            Action::Limit { side, price, size } => match crosses(side, price, top) {
                true => {
                    let at = touch(side, top).unwrap_or(price);
                    events.push(PaperEvent::Fill(self.fill(
                        side,
                        at,
                        size,
                        Liquidity::Taker,
                        slot,
                        unix_ms,
                        mid,
                    )));
                }
                false => self.rest(Resting {
                    side,
                    price,
                    size,
                    slot,
                }),
            },
            Action::Market { side, size, limit } => {
                if let Some(at) = touch(side, top).filter(|at| !worse_than(side, *at, limit)) {
                    events.push(PaperEvent::Fill(self.fill(
                        side,
                        at,
                        size,
                        Liquidity::Taker,
                        slot,
                        unix_ms,
                        mid,
                    )));
                }
            }
            // A cancel by client id: the paper book doesn't keep them. A
            // quoting strategy cancels all; anything else isn't an order.
            _ => {}
        }
    }

    fn rest(&mut self, order: Resting) {
        if self.resting.len() == PAPER_RESTING_MAX {
            self.resting.remove(0);
        }
        self.resting.push(order);
    }

    #[allow(clippy::too_many_arguments)]
    fn fill(
        &mut self,
        side: Side,
        price: f64,
        size: f64,
        liquidity: Liquidity,
        slot: u64,
        unix_ms: u64,
        mid: f64,
    ) -> Fill {
        assert!(
            price > 0.0 && size >= 0.0,
            "a fill has a price and a size: {price} {size}"
        );
        let signed = match side {
            Side::Bid => size,
            Side::Ask => -size,
        };
        self.position += signed;
        self.cash -= signed * price;
        let fill = Fill {
            slot,
            unix_ms,
            side,
            price,
            size,
            liquidity,
            mid,
            edge_bps: signed_bps(side, price, mid),
            rested_slots: 0,
        };
        if self.pending.len() == PENDING_MARKOUTS_MAX {
            self.pending.pop_front();
        }
        self.pending.push_back((fill, 0));
        fill
    }
}

/// Whether an order sent at a step went out, so it would be on the book:
/// a shadow chain lands what it writes down.
fn went_out(state: &SendState) -> bool {
    matches!(state, SendState::Landed(_) | SendState::Delayed)
}

fn crosses(side: Side, price: f64, top: Top) -> bool {
    match side {
        Side::Bid => top.ask.is_some_and(|ask| price >= ask),
        Side::Ask => top.bid.is_some_and(|bid| price <= bid),
    }
}

/// Where a taker on `side` fills: the other side's best.
fn touch(side: Side, top: Top) -> Option<f64> {
    match side {
        Side::Bid => top.ask,
        Side::Ask => top.bid,
    }
}

fn worse_than(side: Side, price: f64, limit: f64) -> bool {
    match side {
        Side::Bid => price > limit,
        Side::Ask => price < limit,
    }
}

/// How far `mid` moved in a fill's favour from its `price`, in bps: up for
/// a buy, down for a sale.
fn signed_bps(side: Side, price: f64, mid: f64) -> f64 {
    let moved = (mid - price) / price * 10_000.0;
    match side {
        Side::Bid => moved,
        Side::Ask => -moved,
    }
}

/// A JSON-lines file, appended to, moved aside past [`JOURNAL_BYTES_MAX`].
struct Appender {
    path: PathBuf,
    file: std::fs::File,
    bytes: u64,
}

impl Appender {
    fn open(path: PathBuf) -> Result<Self, String> {
        let file = open_append(&path)?;
        let bytes = file.metadata().map_or(0, |m| m.len());
        Ok(Self { path, file, bytes })
    }

    /// A failed write is logged, not returned: the journal mustn't stop a
    /// strategy.
    fn write(&mut self, line: &Value) {
        let mut text = line.to_string();
        text.push('\n');
        if self.bytes + text.len() as u64 > JOURNAL_BYTES_MAX
            && let Err(error) = self.rotate()
        {
            tracing::error!(path = %self.path.display(), %error, "a journal couldn't be moved aside");
        }
        match self.file.write_all(text.as_bytes()) {
            Ok(()) => self.bytes += text.len() as u64,
            Err(error) => {
                tracing::error!(path = %self.path.display(), %error, "a journal line couldn't be written");
            }
        }
    }

    fn rotate(&mut self) -> Result<(), String> {
        let aside = self.path.with_extension("jsonl.1");
        std::fs::rename(&self.path, &aside).map_err(|e| e.to_string())?;
        self.file = open_append(&self.path)?;
        self.bytes = 0;
        Ok(())
    }
}

fn open_append(path: &Path) -> Result<std::fs::File, String> {
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|e| format!("{}: {e}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    const LANDED: fn() -> SendState = || SendState::Landed("sig".into());

    fn top(bid: f64, ask: f64) -> Top {
        Top {
            bid: Some(bid),
            ask: Some(ask),
        }
    }

    fn quote(bid: f64, ask: f64) -> Vec<(Action, SendState)> {
        vec![
            (Action::CancelAll, LANDED()),
            (
                Action::PostOnly {
                    side: Side::Bid,
                    price: bid,
                    size: 1.0,
                },
                LANDED(),
            ),
            (
                Action::PostOnly {
                    side: Side::Ask,
                    price: ask,
                    size: 1.0,
                },
                LANDED(),
            ),
        ]
    }

    #[test]
    fn a_quote_fills_only_when_the_book_trades_through_it() {
        let mut book = PaperBook::default();
        assert!(
            book.step(100, 0, top(99.0, 101.0), &[], &quote(99.5, 100.5))
                .is_empty()
        );
        assert!(book.at_touch(top(99.0, 101.0)));
        // The book moves, but not through either side: nothing fills.
        assert!(book.step(101, 0, top(99.4, 100.4), &[], &[]).is_empty());
        // The best ask falls to the bid: the bid fills at its own price.
        let events = book.step(102, 0, top(98.0, 99.5), &[], &[]);
        let [PaperEvent::Fill(fill)] = events.as_slice() else {
            panic!("one fill: {events:?}");
        };
        assert_eq!(
            (fill.side, fill.price, fill.liquidity),
            (Side::Bid, 99.5, Liquidity::Maker)
        );
        assert_eq!(fill.rested_slots, 2);
        assert_eq!(book.position, 1.0);
        // It bought above the mid it filled at: the model's adverse case.
        assert!(fill.edge_bps < 0.0);
    }

    #[test]
    fn a_trade_fills_past_the_quote_always_and_at_it_only_at_the_touch() {
        let sale = |slot, price, size| Trade {
            slot,
            taker: Side::Ask,
            price,
            size,
        };
        for (model, at_quote) in [(FillModel::Through, 0.0), (FillModel::Touch, 1.0)] {
            let mut book = PaperBook::new(model);
            book.step(100, 0, top(99.0, 101.0), &[], &quote(99.5, 100.5));
            // A sale in the slot the quote went out in: it landed first.
            assert!(
                book.step(100, 0, top(99.0, 101.0), &[sale(100, 99.0, 5.0)], &[])
                    .is_empty()
            );
            // A sale at the bid's own price fills it only at the touch.
            let at = book.step(101, 0, top(99.0, 101.0), &[sale(101, 99.5, 0.4)], &[]);
            assert_eq!(book.position, 0.4 * at_quote, "{model:?}");
            assert_eq!(at.len(), at_quote as usize);
            // A sale below it fills what's left of it, up to the sale's size.
            let past = book.step(102, 0, top(99.0, 101.0), &[sale(102, 99.0, 10.0)], &[]);
            let [PaperEvent::Fill(fill)] = past.as_slice() else {
                panic!("one fill: {past:?}");
            };
            assert_eq!(
                (fill.price, fill.liquidity, fill.rested_slots),
                (99.5, Liquidity::Maker, 2)
            );
            assert!(
                (book.position - 1.0).abs() < 1e-9,
                "{model:?}: {}",
                book.position
            );
            // The ask, untouched by sales, still rests; the bid is gone.
            assert!(
                book.step(103, 0, top(99.0, 101.0), &[sale(103, 90.0, 10.0)], &[])
                    .is_empty()
            );
        }
    }

    #[test]
    fn a_trade_smaller_than_the_quote_fills_part_of_it() {
        let lift = Trade {
            slot: 101,
            taker: Side::Bid,
            price: 100.6,
            size: 0.25,
        };
        let mut book = PaperBook::new(FillModel::Through);
        book.step(100, 0, top(99.0, 101.0), &[], &quote(99.5, 100.5));
        book.step(101, 0, top(99.0, 101.0), &[lift], &[]);
        assert!((book.position + 0.25).abs() < 1e-9);
        book.step(
            102,
            0,
            top(99.0, 101.0),
            &[Trade { slot: 102, ..lift }],
            &[],
        );
        assert!((book.position + 0.5).abs() < 1e-9);
    }

    #[test]
    fn a_fill_is_marked_out_at_each_horizon_once() {
        let mut book = PaperBook::default();
        book.step(100, 0, top(99.0, 101.0), &[], &quote(99.5, 100.5));
        book.step(101, 0, top(99.0, 99.5), &[], &[]);
        let markouts = |events: Vec<PaperEvent>| {
            events
                .into_iter()
                .filter_map(|e| match e {
                    PaperEvent::Markout {
                        horizon_slots, bps, ..
                    } => Some((horizon_slots, bps)),
                    PaperEvent::Fill(_) => None,
                })
                .collect::<Vec<_>>()
        };
        assert!(markouts(book.step(103, 0, top(99.0, 101.0), &[], &[])).is_empty());
        // Mid 100.5, bought at 99.5: up about 100 bps.
        let at_1s = markouts(book.step(104, 0, top(100.0, 101.0), &[], &[]));
        assert_eq!(at_1s.len(), 1);
        assert_eq!(at_1s[0].0, 3);
        assert!((at_1s[0].1 - 100.5).abs() < 0.1);
        // A gap past both later horizons marks both, and nothing after.
        let late = markouts(book.step(300, 0, top(98.0, 99.0), &[], &[]));
        assert_eq!(late.iter().map(|m| m.0).collect::<Vec<_>>(), vec![25, 150]);
        assert!(late.iter().all(|m| m.1 < 0.0));
        assert!(markouts(book.step(400, 0, top(98.0, 99.0), &[], &[])).is_empty());
    }

    #[test]
    fn orders_that_never_went_out_and_crossing_post_onlys_rest_nowhere() {
        let mut book = PaperBook::default();
        let refused = vec![(
            Action::PostOnly {
                side: Side::Bid,
                price: 99.5,
                size: 1.0,
            },
            SendState::NotSent("over the cap".into()),
        )];
        book.step(100, 0, top(99.0, 101.0), &[], &refused);
        let crossing = vec![(
            Action::PostOnly {
                side: Side::Bid,
                price: 101.0,
                size: 1.0,
            },
            LANDED(),
        )];
        book.step(101, 0, top(99.0, 101.0), &[], &crossing);
        assert!(!book.at_touch(top(99.0, 101.0)));
        assert!(book.step(102, 0, top(10.0, 11.0), &[], &[]).is_empty());
    }

    #[test]
    fn a_crossing_limit_takes_at_the_touch_and_cancel_all_clears() {
        let mut book = PaperBook::default();
        let take = vec![(
            Action::Limit {
                side: Side::Ask,
                price: 98.0,
                size: 2.0,
            },
            LANDED(),
        )];
        let events = book.step(100, 0, top(99.0, 101.0), &[], &take);
        let [PaperEvent::Fill(fill)] = events.as_slice() else {
            panic!("one fill: {events:?}");
        };
        assert_eq!((fill.price, fill.liquidity), (99.0, Liquidity::Taker));
        assert_eq!(book.position, -2.0);
        assert!((book.pnl(100.0) - (2.0 * 99.0 - 200.0)).abs() < 1e-9);
        book.step(101, 0, top(99.0, 101.0), &[], &quote(99.5, 100.5));
        book.step(
            102,
            0,
            top(99.0, 101.0),
            &[],
            &[(Action::CancelAll, LANDED())],
        );
        assert!(!book.at_touch(top(99.0, 101.0)));
    }

    #[test]
    fn the_resting_orders_and_pending_markouts_are_bounded() {
        let mut book = PaperBook::default();
        for i in 0..(PAPER_RESTING_MAX as u64 + 10) {
            let bid = vec![(
                Action::PostOnly {
                    side: Side::Bid,
                    price: 50.0,
                    size: 1.0,
                },
                LANDED(),
            )];
            book.step(i, 0, top(99.0, 101.0), &[], &bid);
        }
        assert_eq!(book.resting.len(), PAPER_RESTING_MAX);
        for i in 0..(PENDING_MARKOUTS_MAX as u64 + 10) {
            let take = vec![(
                Action::Market {
                    side: Side::Bid,
                    size: 0.1,
                    limit: 200.0,
                },
                LANDED(),
            )];
            book.step(1000, i, top(99.0, 101.0), &[], &take);
        }
        assert_eq!(book.pending.len(), PENDING_MARKOUTS_MAX);
    }

    #[test]
    fn a_journal_writes_a_line_per_step_and_moves_aside_when_full() {
        let dir = std::env::temp_dir().join(format!("mtx-journal-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut appender = Appender::open(dir.join("s.steps.jsonl")).unwrap();
        appender.bytes = JOURNAL_BYTES_MAX - 4;
        appender.write(&json!({ "k": 1 }));
        assert!(dir.join("s.steps.jsonl.1").exists());
        let text = std::fs::read_to_string(dir.join("s.steps.jsonl")).unwrap();
        assert_eq!(text, "{\"k\":1}\n");
    }
}
