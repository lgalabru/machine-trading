//! Reference: trade the TSLA perp in the opening half hour by learning which
//! rival maker's lean leads the price, and following it.
//!
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
//!   `LEAD_BPS` and is positive, it follows that maker; until then it does
//!   nothing.
//! - **Following.** On a full lean it takes the cap the maker's way, and on
//!   a partial one it rests the favoured side only; once the lean has gone
//!   for a few steps it closes. It never turns a position over in one go.
//!
//! It builds two ways: a strategy component (`bot.wasm`, the task's oracle
//! solution), and a process (`src/main.rs`).

use std::collections::{BTreeMap, VecDeque};

use mtx_kit::{log, prelude::*, rpc_account, venue::PhoenixVenue};

/// The taker's limit past the index, in basis points.
pub const TAKE_DEPTH_BPS: f64 = 15.0;
/// The most it takes a step, in TSLA.
pub const SLICE: f64 = 1.0;
/// Its resting order on the favoured side: size, in TSLA, and how far
/// under (over) the index, in basis points.
pub const LEAN_SIZE: f64 = 0.25;
pub const LEAN_IN_BPS: f64 = 1.0;
/// Steps a lean must be gone before it closes: about a horizon, so it
/// holds through the move the lean was ahead of.
pub const EXIT_STEPS: u32 = 40;
/// The lead, in basis points of a unit, a maker's following PnL must hold
/// over every other's before the reference follows it.
pub const LEAD_BPS: f64 = 30.0;

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

/// How a signal is followed: the skew at which it takes the cap, and the
/// skew at which it rests the favoured side.
#[derive(Debug, Clone, Copy)]
pub struct Follow {
    pub take: f64,
    pub lean: f64,
}

impl Default for Follow {
    fn default() -> Self {
        Self {
            take: 0.6,
            lean: 0.35,
        }
    }
}

/// Following a signal: takes the cap its way on a full lean, rests the
/// favoured side on a partial one, and closes once it has gone.
#[derive(Debug, Clone, Default)]
pub struct Follower {
    pub follow: Follow,
    quiet: u32,
}

impl Follower {
    pub fn new(follow: Follow) -> Self {
        Self { follow, quiet: 0 }
    }

    /// One step on `signal` (`+1` a full lean up, `-1` down, 0 none).
    pub fn step(&mut self, market: &Market, orders: &mut Orders, signal: f64) {
        orders.cancel_all();
        let held = market.position.size;
        let cap = market.cap;
        let index = index_of(market);
        let way = sign(signal);
        let strength = signal.abs();
        let mut target = held;
        if strength >= self.follow.take {
            self.quiet = 0;
            // Never turned over in one go: close first.
            target = if held * way < -1e-9 { 0.0 } else { way * cap };
        } else if strength >= self.follow.lean && held * way >= -1e-9 {
            self.quiet = 0;
        } else {
            self.quiet += 1;
            let against = held * way < -1e-9 && strength >= self.follow.lean;
            if self.quiet >= EXIT_STEPS || against {
                target = 0.0;
            }
        }
        let gap = target - held;
        let size = round_lots(gap.abs().min(SLICE));
        if size >= 0.001 && index > 0.0 {
            if gap > 0.0 {
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
            return;
        }
        // A partial lean: rest the favoured side only, inside the cap.
        if strength >= self.follow.lean && index > 0.0 {
            let room = round_lots((cap - held * way).clamp(0.0, LEAN_SIZE));
            if room >= 0.001 {
                if way > 0.0 {
                    orders.bid(
                        round_down_cents(index * (1.0 - LEAN_IN_BPS / 10_000.0)),
                        room,
                    );
                } else {
                    orders.ask(round_up_cents(index * (1.0 + LEAN_IN_BPS / 10_000.0)), room);
                }
            }
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
    book: BookReader,
    history: IndexHistory,
    leans: Leans,
    markout: Markout,
    follower: Follower,
    chosen: Option<u32>,
}

impl Strategy for Reference {
    fn start(&mut self, session: &Session) -> BotResult {
        self.book = BookReader::new(session)?;
        self.follower = Follower::new(Follow::default());
        Ok(())
    }

    fn step(&mut self, market: &Market, orders: &mut Orders) -> BotResult {
        let index = index_of(market);
        self.history.push(market.slot, index);
        let book = self.book.read();
        let makers = makers(&book, self.book.tick_usd);
        self.leans.update(&makers, &self.history);
        self.markout
            .update(&self.leans, index, self.follower.follow.lean);
        if let Some(leader) = self.markout.leader(LEAD_BPS)
            && self.chosen != Some(leader)
        {
            log(&format!(
                "slot {}: following maker {leader} ({:.0} bp)",
                market.slot, self.markout.pnl[&leader]
            ));
            self.chosen = Some(leader);
        }
        let signal = self.chosen.map_or(0.0, |m| self.leans.of(m));
        self.follower.step(market, orders, signal);
        Ok(())
    }
}

#[cfg(all(target_arch = "wasm32", feature = "export"))]
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
