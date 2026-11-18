//! Reference: work the session's order (build a TSLA long by the end, or
//! unwind one) and time it by learning which rival maker's lean leads the
//! price.
//!
//! - **The order** comes in the session's vars: `MTX_ORDER`'s side and
//!   quantity, and the broker's haircut `MTX_BROKER_HAIRCUT_BPS`. Whatever is
//!   left at the end, the broker does at the closing index worse by the
//!   haircut.
//! - **The makers.** Every step it reads Phoenix's TSLA orderbook account
//!   and groups the resting orders by their trader: a trader with a bid and
//!   an ask resting is a maker. Its skew is how far its mid sat from the
//!   index when it placed the quote, over its half-width: `+1` leans fully
//!   up, `-1` fully down. A quote the index has moved away from since is
//!   stale, not leaning, and a maker's lean is the mean over its last two
//!   quotes, so one quote's noise is not a lean.
//! - **Marking out.** For each maker it keeps the PnL of following its
//!   lean: a unit position the way it leans while it leans, marked at the
//!   index. A maker that leans ahead of moves earns it; one that leans on
//!   nothing earns about nothing.
//! - **Committing.** Once one maker's following PnL leads every other's by
//!   `LEAD_BPS` and is positive, it reads that maker; until then it reads
//!   no one.
//! - **Pacing.** The read sets only when and how hard it trades, never the
//!   side or the size. A full lean the order's way (up ahead of a buy) means
//!   the price is about to run from it: it crosses for the rest of the order
//!   now. A lean against means the price is about to come to it: it waits.
//!   Either holds for about a horizon after the lean fades. Otherwise it
//!   holds off, and late in the session finishes what is left while the
//!   book beats the broker.
//!
//! It builds two ways: a strategy component (`bot.wasm`, the task's oracle
//! solution), and a process (`src/main.rs`).

use std::collections::{BTreeMap, VecDeque};

use mtx_kit::{log, prelude::*, rpc_account, venue::PhoenixVenue};

/// The taker's limit past the index, in basis points, and its fee.
pub const TAKE_DEPTH_BPS: f64 = 15.0;
pub const TAKER_FEE_BPS: f64 = 3.5;
/// The most it takes a step, in TSLA.
pub const SLICE: f64 = 1.0;
/// Steps a lean must be gone before its pace lapses: about a horizon, so it
/// holds through the move the lean was ahead of.
pub const EXIT_STEPS: u32 = 40;
/// The lead, in basis points of a unit, a maker's following PnL must hold
/// over every other's before the reference reads it.
pub const LEAD_BPS: f64 = 30.0;
/// From this share of the session on, it finishes the order while the book
/// beats the broker.
pub const LATE: f64 = 0.9;

/// One resting order on Phoenix's book.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rest {
    pub bid: bool,
    pub ticks: u64,
    /// The book's number for its trader.
    pub trader: u32,
    pub seq: u64,
    /// What is left of it, in TSLA, and the slot it was placed in.
    pub size: f64,
    pub slot: u64,
}

/// Reads the market's resting orders.
#[derive(Debug, Clone, Default)]
pub struct BookReader {
    pub orderbook: String,
    pub tick_usd: f64,
    pub lot: f64,
    words: Vec<u64>,
}

impl BookReader {
    pub fn new(session: &Session) -> BotResult<Self> {
        let venue = PhoenixVenue::from_json(&session.venue)?;
        let market = venue.market(&session.symbol)?;
        Ok(Self {
            orderbook: market.market_pubkey.clone(),
            tick_usd: venue.ticks_to_usd(market, 1),
            lot: venue.lots_to_base_units(market, 1),
            words: Vec::new(),
        })
    }

    pub fn price(&self, ticks: u64) -> f64 {
        ticks as f64 * self.tick_usd
    }

    /// Every resting order, or none when the account can't be read.
    pub fn read(&mut self) -> Vec<Rest> {
        use mtx_kit::venue::phoenix_rise::accounts::orderbook::Orderbook;
        let Ok(Some(data)) = rpc_account(&self.orderbook) else {
            return Vec::new();
        };
        // The account is read in place, and wants its fields aligned.
        self.words.clear();
        self.words.resize(data.len().div_ceil(8), 0);
        let bytes: &mut [u8] = bytes_of_words(&mut self.words);
        bytes[..data.len()].copy_from_slice(&data);
        let bytes = &bytes[..data.len()];
        let Ok(book) = Orderbook::try_from_account_bytes(bytes) else {
            return Vec::new();
        };
        let lot = self.lot;
        let mut out = Vec::new();
        for (bid, side) in [(true, book.bid_orders()), (false, book.ask_orders())] {
            for entry in side {
                let order = entry.order();
                out.push(Rest {
                    bid,
                    ticks: entry.price_in_ticks().as_inner(),
                    trader: order.trader_position_id().trader_id().to_u32_or(0),
                    seq: entry.order_sequence_number(),
                    size: entry.num_base_lots_remaining().as_inner() as f64 * lot,
                    slot: order.initial_slot(),
                });
            }
        }
        out
    }
}

fn bytes_of_words(words: &mut [u64]) -> &mut [u8] {
    let len = std::mem::size_of_val(words);
    // SAFETY: a u64 slice is a valid, initialised byte slice of 8x its length.
    unsafe { std::slice::from_raw_parts_mut(words.as_mut_ptr().cast::<u8>(), len) }
}

/// A maker's quote: its best bid and ask, in USD, and the slot it placed
/// the newer of them in.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Quote {
    pub bid: f64,
    pub ask: f64,
    pub slot: u64,
}

impl Quote {
    /// How far its mid sits from `index`, over its half-width.
    pub fn skew(&self, index: f64) -> f64 {
        let half = (self.ask - self.bid) / 2.0;
        if half <= 0.0 {
            return 0.0;
        }
        (((self.ask + self.bid) / 2.0 - index) / half).clamp(-1.5, 1.5)
    }
}

/// The makers on a book: each trader with both a bid and an ask resting,
/// by its number.
pub fn makers(book: &[Rest], tick_usd: f64) -> BTreeMap<u32, Quote> {
    type Side = Option<(u64, u64)>;
    let mut sides: BTreeMap<u32, (Side, Side)> = BTreeMap::new();
    for r in book.iter().filter(|r| r.size > 0.0) {
        let entry = sides.entry(r.trader).or_default();
        if r.bid {
            if entry.0.is_none_or(|(b, _)| r.ticks > b) {
                entry.0 = Some((r.ticks, r.slot));
            }
        } else if entry.1.is_none_or(|(a, _)| r.ticks < a) {
            entry.1 = Some((r.ticks, r.slot));
        }
    }
    sides
        .into_iter()
        .filter_map(|(trader, (bid, ask))| {
            let ((bid, bid_slot), (ask, ask_slot)) = (bid?, ask?);
            Some((
                trader,
                Quote {
                    bid: bid as f64 * tick_usd,
                    ask: ask as f64 * tick_usd,
                    slot: bid_slot.max(ask_slot),
                },
            ))
        })
        .collect()
}

/// The price the makers quote around: Phoenix's index, else the oracle.
pub fn index_of(market: &Market) -> f64 {
    if market.index > 0.0 {
        market.index
    } else {
        market.oracle
    }
}

/// The index at each recent step, to read a quote against the index it was
/// placed at.
#[derive(Debug, Clone, Default)]
pub struct IndexHistory(VecDeque<(u64, f64)>);

impl IndexHistory {
    pub fn push(&mut self, slot: u64, index: f64) {
        self.0.push_back((slot, index));
        while self.0.len() > 400 {
            self.0.pop_front();
        }
    }

    /// The index at the last step at or before `slot`.
    pub fn at(&self, slot: u64) -> f64 {
        self.0
            .iter()
            .rev()
            .find(|(s, _)| *s <= slot)
            .or(self.0.front())
            .map_or(0.0, |(_, i)| *i)
    }

    /// A quote's skew against the index it was placed at.
    pub fn skew(&self, quote: &Quote) -> f64 {
        quote.skew(self.at(quote.slot))
    }
}

/// Each maker's recent quotes, read against the index each was placed at:
/// a maker's lean is the mean skew of its last `QUOTES_READ` quotes, so one
/// quote's noise is no lean. A maker a fill has left one-sided keeps the
/// lean of its last full quote.
#[derive(Debug, Clone, Default)]
pub struct Leans {
    quotes: BTreeMap<u32, VecDeque<(u64, f64)>>,
}

/// The quotes a maker's lean is read over.
pub const QUOTES_READ: usize = 2;

impl Leans {
    /// Take in a step's makers.
    pub fn update(&mut self, makers: &BTreeMap<u32, Quote>, history: &IndexHistory) {
        for (trader, quote) in makers {
            let seen = self.quotes.entry(*trader).or_default();
            if seen.back().is_none_or(|(slot, _)| *slot != quote.slot) {
                seen.push_back((quote.slot, history.skew(quote)));
                while seen.len() > QUOTES_READ {
                    seen.pop_front();
                }
            }
        }
    }

    /// A maker's lean: its last quotes' mean skew.
    pub fn of(&self, trader: u32) -> f64 {
        self.quotes.get(&trader).map_or(0.0, |q| {
            q.iter().map(|(_, s)| s).sum::<f64>() / q.len().max(1) as f64
        })
    }

    /// Every maker's lean, by trader number.
    pub fn all(&self) -> Vec<(u32, f64)> {
        self.quotes.keys().map(|t| (*t, self.of(*t))).collect()
    }
}

/// `+1`, `-1` or 0, as `x` is positive, negative or zero.
pub fn sign(x: f64) -> f64 {
    if x > 0.0 {
        1.0
    } else if x < 0.0 {
        -1.0
    } else {
        0.0
    }
}

/// The session's order.
#[derive(Debug, Clone, Copy, Default)]
pub struct Order {
    /// Buying (building a long from flat) or selling (an inherited long).
    pub buy: bool,
    pub quantity: f64,
    pub haircut_bps: f64,
    pub start_slot: u64,
    pub end_slot: u64,
}

impl Order {
    pub fn from_session(session: &Session) -> BotResult<Self> {
        let order = session.var("MTX_ORDER").ok_or("no MTX_ORDER")?;
        let side = order
            .split("\"side\"")
            .nth(1)
            .ok_or("MTX_ORDER has no side")?;
        let buy = side.trim_start_matches([':', ' ', '"']).starts_with("buy");
        let quantity = number_after(order, "\"quantity\"").ok_or("MTX_ORDER has no quantity")?;
        let haircut_bps = session
            .var("MTX_BROKER_HAIRCUT_BPS")
            .and_then(|h| h.trim().parse().ok())
            .ok_or("no MTX_BROKER_HAIRCUT_BPS")?;
        Ok(Self {
            buy,
            quantity,
            haircut_bps,
            start_slot: session.start_slot,
            end_slot: session.end_slot,
        })
    }

    /// `+1` for a buy, `-1` for a sell.
    pub fn way(&self) -> f64 {
        if self.buy { 1.0 } else { -1.0 }
    }

    /// What is left of the order at `position`.
    pub fn left(&self, position: f64) -> f64 {
        let left = if self.buy {
            self.quantity - position
        } else {
            position
        };
        round_lots(left.max(0.0))
    }

    /// How far into the session `slot` is, from 0 to 1.
    pub fn progress(&self, slot: u64) -> f64 {
        let span = self.end_slot.saturating_sub(self.start_slot).max(1) as f64;
        (slot.saturating_sub(self.start_slot) as f64 / span).min(1.0)
    }
}

/// The number after `key` in a JSON text: enough for the session's vars.
pub fn number_after(text: &str, key: &str) -> Option<f64> {
    let rest = &text[text.find(key)? + key.len()..];
    let rest = rest.trim_start_matches([':', ' ', '"']);
    let end = rest
        .find(|c: char| !(c.is_ascii_digit() || c == '.' || c == '-' || c == 'e'))
        .unwrap_or(rest.len());
    rest[..end].parse().ok()
}

/// Crosses for up to `size` of the order, at most `SLICE` a step.
pub fn cross(market: &Market, orders: &mut Orders, order: &Order, size: f64) {
    let index = index_of(market);
    let size = round_lots(size.min(SLICE));
    if size < 0.001 || index <= 0.0 {
        return;
    }
    if order.buy {
        orders.buy(
            size,
            round_up_cents(index * (1.0 + TAKE_DEPTH_BPS / 10_000.0)),
        );
    } else {
        orders.sell(
            size,
            round_down_cents(index * (1.0 - TAKE_DEPTH_BPS / 10_000.0)),
        );
    }
}

/// Late in the session, finishes what is left while the touch, after the
/// fee, beats the broker's price at the index now.
pub fn finish_late(market: &Market, orders: &mut Orders, order: &Order) {
    let left = order.left(market.position.size);
    if left < 0.001 || order.progress(market.slot) < LATE {
        return;
    }
    let index = index_of(market);
    let fee = TAKER_FEE_BPS / 10_000.0;
    let haircut = order.haircut_bps / 10_000.0;
    let beats = if order.buy {
        market
            .best_ask
            .is_some_and(|a| a * (1.0 + fee) < index * (1.0 + haircut))
    } else {
        market
            .best_bid
            .is_some_and(|b| b * (1.0 - fee) > index * (1.0 - haircut))
    };
    if beats {
        cross(market, orders, order, left);
    }
}

/// How a read paces the order: the lean, as a share of a full one, at
/// which a lean the order's way sends it in now, and at which a lean
/// against holds it back.
#[derive(Debug, Clone, Copy)]
pub struct Pace {
    pub now: f64,
    pub wait: f64,
}

impl Default for Pace {
    fn default() -> Self {
        Self {
            now: 0.6,
            wait: 0.35,
        }
    }
}

/// What a read has the order doing.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Mode {
    /// No read: hold off, and finish late while the book beats the broker.
    #[default]
    Idle,
    /// The price is about to run from the order: cross for the rest.
    Now,
    /// The price is about to come to the order: wait.
    Wait,
}

/// Pacing an order on a signal (`+1` a full lean up, `-1` down, 0 none).
#[derive(Debug, Clone, Default)]
pub struct Pacer {
    pub pace: Pace,
    pub mode: Mode,
    quiet: u32,
}

impl Pacer {
    pub fn new(pace: Pace) -> Self {
        Self {
            pace,
            ..Self::default()
        }
    }

    /// One step of the order on `signal`.
    pub fn step(&mut self, market: &Market, orders: &mut Orders, order: &Order, signal: f64) {
        orders.cancel_all();
        let aligned = signal * order.way();
        if aligned >= self.pace.now {
            (self.mode, self.quiet) = (Mode::Now, 0);
        } else if aligned <= -self.pace.wait {
            (self.mode, self.quiet) = (Mode::Wait, 0);
        } else if self.mode != Mode::Idle {
            self.quiet += 1;
            if self.quiet >= EXIT_STEPS {
                self.mode = Mode::Idle;
            }
        }
        match self.mode {
            Mode::Now => cross(market, orders, order, order.left(market.position.size)),
            Mode::Wait => {}
            Mode::Idle => finish_late(market, orders, order),
        }
    }
}

pub fn round_lots(size: f64) -> f64 {
    (size * 1000.0 + 1e-9).floor() / 1000.0
}

pub fn round_down_cents(price: f64) -> f64 {
    (price * 100.0 + 1e-9).floor() / 100.0
}

pub fn round_up_cents(price: f64) -> f64 {
    (price * 100.0 - 1e-9).ceil() / 100.0
}

/// Each maker's PnL from following its lean, in basis points of a unit.
#[derive(Debug, Clone, Default)]
pub struct Markout {
    /// Each maker's lean last step, and what following it has made.
    leans: BTreeMap<u32, f64>,
    pub pnl: BTreeMap<u32, f64>,
    last_index: f64,
}

impl Markout {
    /// Take in a step: the makers' leans and the index.
    pub fn update(&mut self, leans: &Leans, index: f64, lean: f64) {
        if self.last_index > 0.0 && index > 0.0 {
            let moved = (index / self.last_index - 1.0) * 10_000.0;
            for (trader, way) in &self.leans {
                *self.pnl.entry(*trader).or_default() += way * moved;
            }
        }
        self.leans = leans
            .all()
            .into_iter()
            .map(|(trader, s)| (trader, if s.abs() >= lean { sign(s) } else { 0.0 }))
            .collect();
        self.last_index = index;
    }

    /// The maker whose following PnL leads every other's by `lead` and is
    /// positive.
    pub fn leader(&self, lead: f64) -> Option<u32> {
        let mut ranked: Vec<(u32, f64)> = self.pnl.iter().map(|(t, p)| (*t, *p)).collect();
        ranked.sort_by(|a, b| b.1.total_cmp(&a.1));
        let (first, best) = *ranked.first()?;
        let second = ranked.get(1).map_or(0.0, |r| r.1).max(0.0);
        (best > 0.0 && best - second >= lead).then_some(first)
    }
}

/// The strategy.
#[derive(Default)]
pub struct Reference {
    order: Order,
    book: BookReader,
    history: IndexHistory,
    leans: Leans,
    markout: Markout,
    pacer: Pacer,
    chosen: Option<u32>,
}

impl Strategy for Reference {
    fn start(&mut self, session: &Session) -> BotResult {
        self.order = Order::from_session(session)?;
        self.book = BookReader::new(session)?;
        self.pacer = Pacer::new(Pace::default());
        Ok(())
    }

    fn step(&mut self, market: &Market, orders: &mut Orders) -> BotResult {
        let index = index_of(market);
        self.history.push(market.slot, index);
        let book = self.book.read();
        let makers = makers(&book, self.book.tick_usd);
        self.leans.update(&makers, &self.history);
        self.markout
            .update(&self.leans, index, self.pacer.pace.wait);
        if let Some(leader) = self.markout.leader(LEAD_BPS)
            && self.chosen != Some(leader)
        {
            log(&format!(
                "slot {}: reading maker {leader} ({:.0} bp)",
                market.slot, self.markout.pnl[&leader]
            ));
            self.chosen = Some(leader);
        }
        let signal = self.chosen.map_or(0.0, |m| self.leans.of(m));
        self.pacer.step(market, orders, &self.order, signal);
        Ok(())
    }
}

#[cfg(all(target_arch = "wasm32", not(feature = "embedded")))]
mtx_kit::export!(Reference);

#[cfg(test)]
mod tests {
    use super::*;

    fn rest(bid: bool, ticks: u64, trader: u32) -> Rest {
        Rest {
            bid,
            ticks,
            trader,
            seq: 0,
            size: 0.04,
            slot: 0,
        }
    }

    #[test]
    fn a_maker_quotes_both_sides_and_its_skew_is_its_lean() {
        let book = [
            rest(true, 39_990, 1),
            rest(false, 40_030, 1),
            rest(true, 40_000, 2),
            rest(true, 39_970, 3),
            rest(false, 40_010, 3),
        ];
        let makers = makers(&book, 0.01);
        assert_eq!(makers.len(), 2, "trader 2 rests one side only");
        assert!((makers[&1].skew(400.0) - 0.5).abs() < 1e-9);
        assert!((makers[&3].skew(400.0) + 0.5).abs() < 1e-9);
        // Read against the index it was placed at, not today's.
        let mut history = IndexHistory::default();
        history.push(0, 400.0);
        history.push(10, 401.0);
        assert!((history.skew(&makers[&1]) - 0.5).abs() < 1e-9);
    }

    #[test]
    fn a_lean_the_orders_way_crosses_now_and_one_against_waits() {
        let order = Order {
            buy: true,
            quantity: 6.0,
            haircut_bps: 5.0,
            start_slot: 0,
            end_slot: 4_500,
        };
        assert_eq!(order.left(2.5), 3.5);
        assert_eq!(order.left(7.0), 0.0);
        let sell = Order { buy: false, ..order };
        assert_eq!((sell.left(6.0), sell.way()), (6.0, -1.0));
        let market = Market {
            slot: 100,
            index: 400.0,
            oracle: 400.0,
            best_bid: Some(399.9),
            best_ask: Some(400.1),
            ..Market::default()
        };
        let mut pacer = Pacer::new(Pace::default());
        let mut orders = Orders::new();
        pacer.step(&market, &mut orders, &order, 0.8);
        assert_eq!(pacer.mode, Mode::Now);
        let mut orders = Orders::new();
        pacer.step(&market, &mut orders, &order, -0.5);
        assert_eq!(pacer.mode, Mode::Wait);
        // Early in the session, with no read, it holds off.
        let mut idle = Pacer::new(Pace::default());
        let mut orders = Orders::new();
        idle.step(&market, &mut orders, &order, 0.0);
        assert_eq!(idle.mode, Mode::Idle);
        assert_eq!(orders.lines().count(), 1, "only the cancel");
    }

    #[test]
    fn the_order_is_read_from_the_session_vars() {
        assert_eq!(
            number_after(r#"{"side": "buy", "quantity": 6}"#, "\"quantity\""),
            Some(6.0)
        );
    }

    #[test]
    fn the_leader_must_lead_every_other_maker() {
        let mut m = Markout {
            pnl: BTreeMap::from([(1, 60.0), (2, 10.0), (3, -30.0)]),
            ..Markout::default()
        };
        assert_eq!(m.leader(40.0), Some(1));
        m.pnl.insert(2, 30.0);
        assert_eq!(m.leader(40.0), None);
    }
}
