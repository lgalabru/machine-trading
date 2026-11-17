//! Reference: sell the inherited SPY long by the close without showing a
//! front-runner the selling it hunts.
//!
//! The order comes in the session's vars (`MTX_ORDER`'s quantity, the
//! broker's `MTX_BROKER_HAIRCUT_BPS`). Near the touch the book is one maker's
//! small quote; behind it the makers stand well out, dearer than the broker.
//!
//! - **Selling.** Each step it sells what its pace has earned, in small
//!   slices at the maker's bid, and only while the bid, less the taker fee,
//!   beats the broker's price.
//! - **Finding the predator.** A bid that is gone at its turn, just after it
//!   sold, means someone sold into it ahead of it: twice in a few steps, and
//!   it has been found.
//! - **Pacing.** It starts slow and speeds up by half every so often while
//!   nobody trades ahead of it. Once found, it stops until the bid stays,
//!   and goes on at half the pace that was found, never again above it.
//!
//! The same machinery runs the ladder's rungs ([`Mode`]): the informed bot,
//! told the predator's window and threshold, sells as fast as stays under
//! them; the rungs that read nothing sell on a fixed schedule.
//!
//! It builds two ways: a strategy component (`bot.wasm`, the task's oracle
//! solution), and a process (`src/main.rs`), the anchor hidden sets are
//! scored against.

use std::collections::VecDeque;

use mtx_kit::prelude::*;

/// The taker fee, in basis points.
pub const TAKER_FEE_BPS: f64 = 3.5;
/// A bid within this of the index is the maker's: one further out is the
/// backstop's.
pub const GOOD_BPS: f64 = 20.0;
/// The most it sells a step.
pub const SLICE: f64 = 0.08;
/// The reference's first pace, in SPY a step; how often it speeds up, in
/// steps, and by how much.
pub const START_PACE: f64 = 0.0012;
pub const GROW_STEPS: u64 = 25;
pub const GROW: f64 = 1.5;
/// Gone bids that find the predator, within this many steps of a sale.
pub const FOUND_AFTER: u32 = 2;
pub const SOLD_WITHIN: u64 = 4;
/// Turns the bid must stay before it sells again once found.
pub const CLEAR_STEPS: u32 = 3;
/// The tiny rung's slice, in SPY a step.
pub const TINY: f64 = 0.003;
/// The informed bot keeps its window's selling under this share of the
/// threshold.
pub const MARGIN: f64 = 0.9;

/// Which rung the machinery runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Reference,
    /// Told the predator's window and threshold (`MTX_REGIME`).
    Informed,
    /// Sells the whole order at once.
    Dump,
    /// Sells evenly by the close, crossing whatever the bid.
    Twap,
    /// The same pace in random slices with random pauses.
    RandomTwap,
    /// Half the even pace, only at a good bid.
    Slow,
    /// A fixed tiny slice a step, only at a good bid.
    Tiny,
    /// The even pace, only at a good bid, catching up after.
    Patient,
    /// Rests asks inside the maker and lets buyers come.
    Passive,
}

impl Mode {
    pub fn parse(name: &str) -> Option<Self> {
        Some(match name {
            "reference" => Self::Reference,
            "informed" => Self::Informed,
            "dump" => Self::Dump,
            "twap" => Self::Twap,
            "random-twap" => Self::RandomTwap,
            "slow" => Self::Slow,
            "tiny" => Self::Tiny,
            "patient" => Self::Patient,
            "passive" => Self::Passive,
            _ => return None,
        })
    }
}

/// The order, from the session.
#[derive(Debug, Clone, Copy, Default)]
pub struct Order {
    pub quantity: f64,
    pub haircut_bps: f64,
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
        })
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

pub fn round_lots(size: f64) -> f64 {
    (size * 1000.0).floor() / 1000.0
}

fn round_down_cents(price: f64) -> f64 {
    (price * 100.0).floor() / 100.0
}

fn round_up_cents(price: f64) -> f64 {
    (price * 100.0).ceil() / 100.0
}

/// The strategy, in one of its modes.
pub struct Unwind {
    mode: Mode,
    order: Order,
    /// What each of its last steps sold, as its position shows it.
    sold: VecDeque<f64>,
    last_position: Option<f64>,
    /// Earned and not yet sold, in SPY.
    budget: f64,
    pace: f64,
    /// The reference's ceiling once it has been found, and its watch.
    ceiling: Option<f64>,
    gone: u32,
    clear: u32,
    waiting: bool,
    since_grow: u64,
    last_sale: Option<u64>,
    /// The informed bot's window and threshold, in steps and SPY.
    window: usize,
    threshold: f64,
    rng: u64,
}

impl Unwind {
    pub fn new(mode: Mode) -> Self {
        Self {
            mode,
            order: Order::default(),
            sold: VecDeque::new(),
            last_position: None,
            budget: 0.0,
            pace: START_PACE,
            ceiling: None,
            gone: 0,
            clear: 0,
            waiting: false,
            since_grow: 0,
            last_sale: None,
            window: 1,
            threshold: f64::INFINITY,
            rng: 0x9e37_79b9_7f4a_7c15,
        }
    }

    fn uniform(&mut self) -> f64 {
        self.rng ^= self.rng << 13;
        self.rng ^= self.rng >> 7;
        self.rng ^= self.rng << 17;
        (self.rng >> 11) as f64 / (1u64 << 53) as f64
    }

    /// The bid, if it is the maker's and beats the broker after the fee.
    fn good_bid(&self, market: &Market) -> Option<f64> {
        let bid = market.best_bid?;
        let near = bid >= market.index * (1.0 - GOOD_BPS / 10_000.0);
        let broker = market.index * (1.0 - self.order.haircut_bps / 10_000.0);
        (near && bid * (1.0 - TAKER_FEE_BPS / 10_000.0) > broker).then_some(bid)
    }

    fn sell_at(&mut self, market: &Market, orders: &mut Orders, size: f64, bid: f64) -> f64 {
        let size = round_lots(size.min(market.position.size.max(0.0)));
        if size < 0.001 {
            return 0.0;
        }
        orders.sell(size, round_down_cents(bid));
        self.last_sale = Some(market.k);
        size
    }

    /// Sells `size` crossing whatever stands: the blind rungs' way.
    fn cross(&mut self, market: &Market, orders: &mut Orders, size: f64) {
        let Some(bid) = market.best_bid else {
            return;
        };
        let floor = bid.min(market.index) * (1.0 - 0.006);
        let size = round_lots(size.min(market.position.size.max(0.0)));
        if size >= 0.001 {
            orders.sell(size, round_down_cents(floor));
        }
    }

    fn even_pace(&self, market: &Market) -> f64 {
        market.position.size.max(0.0) / (market.steps_left.max(1) as f64)
    }

    fn reference(&mut self, market: &Market, orders: &mut Orders) {
        let near = market
            .best_bid
            .is_some_and(|b| b >= market.index * (1.0 - GOOD_BPS / 10_000.0));
        let after_sale = self
            .last_sale
            .is_some_and(|k| market.k <= k + SOLD_WITHIN);
        if !near && after_sale {
            self.gone += 1;
            if self.gone >= FOUND_AFTER && !self.waiting {
                let found = self.pace;
                let ceiling = self.ceiling.map_or(found, |c: f64| c.min(found));
                self.ceiling = Some(ceiling);
                self.pace = 0.5 * ceiling;
                self.budget = 0.0;
                self.waiting = true;
                self.clear = 0;
            }
        }
        if self.waiting {
            if near {
                self.clear += 1;
            } else {
                self.clear = 0;
            }
            if self.clear >= CLEAR_STEPS {
                self.waiting = false;
                self.gone = 0;
                self.since_grow = 0;
            }
            return;
        }
        self.since_grow += 1;
        if self.since_grow >= GROW_STEPS {
            self.since_grow = 0;
            self.gone = 0;
            let grown = self.pace * GROW;
            self.pace = match self.ceiling {
                Some(c) => grown.min(0.7 * c),
                None => grown,
            };
        }
        self.budget += self.pace;
        if let Some(bid) = self.good_bid(market) {
            let sold = self.sell_at(market, orders, self.budget.min(SLICE), bid);
            self.budget -= sold;
        }
    }

    fn informed(&mut self, market: &Market, orders: &mut Orders) {
        let recent: f64 = self
            .sold
            .iter()
            .rev()
            .take(self.window.saturating_sub(1))
            .sum();
        let room = MARGIN * self.threshold - recent;
        if room <= 0.0 {
            return;
        }
        if let Some(bid) = self.good_bid(market) {
            self.sell_at(market, orders, room.min(SLICE), bid);
        }
    }

    fn read_regime(&mut self, session: &Session) -> BotResult {
        let regime = session
            .var("MTX_REGIME")
            .ok_or("no MTX_REGIME: the informed bot is told the session's latent state")?;
        self.window = number_after(regime, "\"window_steps\"").ok_or("no window_steps")? as usize;
        self.threshold = number_after(regime, "\"threshold\"").ok_or("no threshold")?;
        Ok(())
    }
}

impl Default for Unwind {
    fn default() -> Self {
        Self::new(Mode::Reference)
    }
}

impl Strategy for Unwind {
    fn start(&mut self, session: &Session) -> BotResult {
        self.order = Order::from_session(session)?;
        if self.mode == Mode::Informed {
            self.read_regime(session)?;
        }
        Ok(())
    }

    fn step(&mut self, market: &Market, orders: &mut Orders) -> BotResult {
        orders.cancel_all();
        let held = market.position.size.max(0.0);
        if let Some(before) = self.last_position {
            self.sold.push_back((before - held).max(0.0));
            if self.sold.len() > 200 {
                self.sold.pop_front();
            }
        }
        self.last_position = Some(held);
        if held < 0.001 {
            return Ok(());
        }
        match self.mode {
            Mode::Reference => self.reference(market, orders),
            Mode::Informed => self.informed(market, orders),
            Mode::Dump => self.cross(market, orders, held),
            Mode::Twap => {
                self.budget += self.even_pace(market);
                let size = round_lots(self.budget);
                self.cross(market, orders, size);
                self.budget -= size;
            }
            Mode::RandomTwap => {
                let pace = self.even_pace(market);
                self.budget += pace;
                if self.uniform() < 0.35 {
                    return Ok(());
                }
                let size = round_lots(self.budget * (0.5 + self.uniform()));
                self.cross(market, orders, size);
                self.budget = (self.budget - size).max(-pace * 4.0);
            }
            Mode::Slow | Mode::Tiny => {
                self.budget += if self.mode == Mode::Slow {
                    0.5 * self.order.quantity / (market.steps_left + market.k + 1) as f64
                } else {
                    TINY
                };
                if let Some(bid) = self.good_bid(market) {
                    let sold = self.sell_at(market, orders, self.budget.min(SLICE), bid);
                    self.budget -= sold;
                }
            }
            Mode::Patient => {
                self.budget = self.budget.max(0.0) + self.even_pace(market);
                if let Some(bid) = self.good_bid(market) {
                    let sold = self.sell_at(market, orders, self.budget.min(SLICE), bid);
                    self.budget -= sold;
                }
            }
            Mode::Passive => {
                if let (Some(bid), Some(ask)) = (market.best_bid, market.best_ask) {
                    let price = round_up_cents((ask - 0.01).max(bid + 0.01));
                    orders.ask(price, round_lots(held.min(0.5))).post_only();
                }
            }
        }
        Ok(())
    }
}

#[cfg(target_arch = "wasm32")]
mtx_kit::export!(Unwind);
