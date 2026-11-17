//! Reference: sell the inherited QQQ long by the close, and time the sale
//! by what the large orders on the book turn out to be.
//!
//! The order comes in the session's vars (`MTX_ORDER`'s quantity, the
//! broker's `MTX_BROKER_HAIRCUT_BPS`).
//!
//! - **The walls.** Every step it reads Phoenix's QQQ orderbook account and
//!   keeps the large resting orders near the touch (walls) by side and
//!   price: one owner walls each side.
//! - **Pulled or reloaded.** A wall that leaves the book unfilled while the
//!   touch is within a few ticks of it was pulled on approach: its owner is
//!   a spoofer. A wall that trades, or comes back at the same price after
//!   it went, reloaded: its owner is an iceberg. A wall gone with the touch
//!   far off says nothing: walls also expire and go up again. One of each
//!   walls a session, one a side, so knowing one names the other.
//! - **Acting on a known wall.** A spoofer's bid or an iceberg's ask says
//!   the price is about to fall: it sells the rest at once. A spoofer's ask
//!   or an iceberg's bid says it is about to rise: it holds.
//! - **Otherwise** it holds, and sells in the last part of the session
//!   while the bid, less the taker fee, beats the broker's price.
//!
//! It builds two ways: a strategy component (`bot.wasm`, the task's oracle
//! solution), and a process (`src/main.rs`), the anchor hidden sets are
//! scored against.

use std::collections::BTreeMap;

use mtx_kit::{log, prelude::*, rpc_account, venue::PhoenixVenue};

/// The taker fee, in basis points.
pub const TAKER_FEE_BPS: f64 = 3.5;
/// An order this large, in QQQ, and this near the touch, in ticks, is a
/// wall: walls and slices are drawn around 1 to 4 QQQ, the refill's orders
/// stay under 0.2, and the book also holds old orders far from the price.
pub const WALL_MIN: f64 = 0.25;
pub const WALL_TICKS: f64 = 150.0;
/// A wall that leaves with the touch this many ticks from it or nearer
/// was pulled on approach.
pub const NEAR_TICKS: u64 = 10;
/// The most it sells a step, and how far under the bid it will go.
pub const SLICE: f64 = 4.0;
pub const SELL_DEPTH_BPS: f64 = 8.0;
/// From here on, in share of the session, it sells what is left.
pub const LATE: f64 = 0.9;

/// The order, from the session.
#[derive(Debug, Clone, Copy, Default)]
pub struct Order {
    pub quantity: f64,
    pub haircut_bps: f64,
    pub start_slot: u64,
    pub end_slot: u64,
}

impl Order {
    pub fn from_session(session: &Session) -> BotResult<Self> {
        let quantity = session
            .var("MTX_ORDER")
            .and_then(|order| number_after(order, "\"quantity\""))
            .unwrap_or(session.mandate.universe[0].max_position);
        let haircut_bps = session
            .var("MTX_BROKER_HAIRCUT_BPS")
            .and_then(|h| h.trim().parse().ok())
            .ok_or("no MTX_BROKER_HAIRCUT_BPS")?;
        Ok(Self {
            quantity,
            haircut_bps,
            start_slot: session.start_slot,
            end_slot: session.end_slot,
        })
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

/// One resting order on Phoenix's book.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rest {
    pub bid: bool,
    pub ticks: u64,
    /// The book's slot for its trader: reused, so not an owner.
    pub trader: u32,
    pub seq: u64,
    /// What is left of it and what it was placed at, in QQQ.
    pub size: f64,
    pub initial: f64,
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
                    initial: order.initial_trade_size().as_inner() as f64 * lot,
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

/// The walls in a book: large orders near the touch (`bid` and `ask`, USD).
pub fn walls_in(book: &[Rest], tick_usd: f64, bid: Option<f64>, ask: Option<f64>) -> Vec<Rest> {
    book.iter()
        .copied()
        .filter(|r| {
            let p = r.ticks as f64 * tick_usd;
            let away = if r.bid {
                bid.map(|b| (b - p) / tick_usd)
            } else {
                ask.map(|a| (p - a) / tick_usd)
            };
            r.initial >= WALL_MIN && away.is_some_and(|a| a <= WALL_TICKS)
        })
        .collect()
}

/// What a wall's owner is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Spoofer,
    Iceberg,
}

/// What a wall says the price is about to do: `-1` fall, `1` rise.
pub fn sign_of(kind: Kind, bid: bool) -> f64 {
    match (kind, bid) {
        (Kind::Spoofer, true) | (Kind::Iceberg, false) => -1.0,
        (Kind::Spoofer, false) | (Kind::Iceberg, true) => 1.0,
    }
}

/// The walls on the book, step by step, and what their owners did.
///
/// The book's trader numbers are slots Phoenix reuses, not owners, so a
/// wall is followed by its side and price. One owner walls each side in a
/// session: the spoofer the side the jump goes against, the iceberg the
/// other.
#[derive(Debug, Clone, Default)]
pub struct Walls {
    /// Last step's walls, by sequence number.
    last: BTreeMap<u64, Rest>,
    /// What the bids' owner and the asks' owner showed themselves to be.
    pub bids: Option<Kind>,
    pub asks: Option<Kind>,
    /// The walls standing now.
    pub now: Vec<Rest>,
}

impl Walls {
    /// Take in a step's book; `bid` and `ask` are the touch in USD.
    pub fn update(
        &mut self,
        book: &[Rest],
        tick_usd: f64,
        bid: Option<f64>,
        ask: Option<f64>,
        slot: u64,
    ) {
        let now = walls_in(book, tick_usd, bid, ask);
        for (seq, before) in &self.last {
            if self.kind(before.bid).is_some() {
                continue;
            }
            let still = now.iter().find(|r| r.seq == *seq);
            let seen = match still {
                Some(r) if r.size + 1e-9 < before.size => Some(Kind::Iceberg),
                Some(_) => None,
                None => {
                    let back = now
                        .iter()
                        .any(|r| r.bid == before.bid && r.ticks == before.ticks && r.seq != *seq);
                    let touch = if before.bid { bid } else { ask };
                    let gap = touch.map(|t| {
                        let p = before.ticks as f64 * tick_usd;
                        if before.bid {
                            (t - p) / tick_usd
                        } else {
                            (p - t) / tick_usd
                        }
                    });
                    if back {
                        Some(Kind::Iceberg)
                    } else if gap.is_some_and(|g| g <= NEAR_TICKS as f64 + 0.5) {
                        Some(Kind::Spoofer)
                    } else {
                        None
                    }
                }
            };
            if let Some(kind) = seen {
                let side = if before.bid { "bids" } else { "asks" };
                log(&format!("slot {slot}: the {side}' owner is a {kind:?}"));
                // One of each: knowing one names the other.
                let other = match kind {
                    Kind::Spoofer => Kind::Iceberg,
                    Kind::Iceberg => Kind::Spoofer,
                };
                if before.bid {
                    (self.bids, self.asks) = (Some(kind), Some(other));
                } else {
                    (self.asks, self.bids) = (Some(kind), Some(other));
                }
            }
        }
        self.last = now.iter().map(|r| (r.seq, *r)).collect();
        self.now = now;
    }

    /// What a side's owner is, once known.
    pub fn kind(&self, bid: bool) -> Option<Kind> {
        if bid { self.bids } else { self.asks }
    }

    /// What the standing walls say: `-1` a fall, `1` a rise, `0` nothing
    /// known.
    pub fn signal(&self) -> f64 {
        self.now
            .iter()
            .find_map(|r| self.kind(r.bid).map(|k| sign_of(k, r.bid)))
            .unwrap_or(0.0)
    }
}

/// Sells: `size` now, crossing at most `SELL_DEPTH_BPS` under the bid.
pub fn sell_now(market: &Market, orders: &mut Orders, size: f64) {
    let Some(bid) = market.best_bid else {
        return;
    };
    let size = round_lots(size.min(SLICE).min(market.position.size.max(0.0)));
    if size > 0.0 {
        orders.sell(
            size,
            round_down_cents(bid * (1.0 - SELL_DEPTH_BPS / 10_000.0)),
        );
    }
}

/// Late in the session, sells what is left while the bid, less the fee,
/// beats the broker's price.
pub fn sell_late(market: &Market, orders: &mut Orders, order: &Order) {
    let held = market.position.size;
    if held <= 1e-3 || order.progress(market.slot) < LATE {
        return;
    }
    let broker = market.oracle * (1.0 - order.haircut_bps / 10_000.0);
    if market
        .best_bid
        .is_some_and(|b| b * (1.0 - TAKER_FEE_BPS / 10_000.0) > broker)
    {
        sell_now(market, orders, held);
    }
}

pub fn round_lots(size: f64) -> f64 {
    (size * 1000.0).floor() / 1000.0
}

pub fn round_down_cents(price: f64) -> f64 {
    (price * 100.0).floor() / 100.0
}

/// The strategy.
#[derive(Default)]
pub struct Reference {
    order: Order,
    book: BookReader,
    walls: Walls,
    selling: bool,
}

impl Strategy for Reference {
    fn start(&mut self, session: &Session) -> BotResult {
        self.order = Order::from_session(session)?;
        self.book = BookReader::new(session)?;
        Ok(())
    }

    fn step(&mut self, market: &Market, orders: &mut Orders) -> BotResult {
        orders.cancel_all();
        let book = self.book.read();
        self.walls.update(
            &book,
            self.book.tick_usd,
            market.best_bid,
            market.best_ask,
            market.slot,
        );
        if !self.selling && self.walls.signal() < 0.0 {
            log(&format!("slot {}: a fall is coming; selling", market.slot));
            self.selling = true;
        }
        if self.selling {
            sell_now(market, orders, market.position.size);
        } else {
            sell_late(market, orders, &self.order);
        }
        Ok(())
    }
}

#[cfg(all(target_arch = "wasm32", not(feature = "embedded")))]
mtx_kit::export!(Reference);
