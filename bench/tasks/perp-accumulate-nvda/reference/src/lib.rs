//! Reference: work the client's NVDA buy order passively, read the taker
//! flow through the grader's maker, buy ahead of informed buying, stand
//! aside ahead of informed selling, and leave to the broker what costs more
//! to buy than its haircut.
//!
//! The order comes in the session's vars (`MTX_ORDER`'s quantity, the
//! broker's `MTX_BROKER_HAIRCUT_BPS`) and the mandate (the cap is
//! `max_position`, the price cap `max_buy_price`).
//!
//! - **Waits by default.** Until a move can no longer start (75% of the
//!   session) it buys nothing: what it buys early costs it in a falling
//!   session and saves little in a calm one. Then it finishes: a bid at the
//!   front of the book, never over the broker's price less the fee and
//!   3 bp, and takes of the asks under that price at an even pace; in the
//!   last 120 slots, whatever still beats the broker.
//! - **The flow.** The grader's maker leans 1 bp a unit against what it
//!   holds, so its ask, less its first half-spread, is its short in units:
//!   how much takers bought from it. Each step's change, less the desk's
//!   own buys and the sells its bid caught, is the step's net taker flow.
//!   An average over about 30 slots, scored against the flow's own noise,
//!   is the burst signal; one over about 120 slots is the lean.
//! - **A burst** that holds for a few slots puts it on watch. A buy burst
//!   lifts its floor to half the order and lets it pay 10 bp over the
//!   haircut; a sell burst stops it buying until 85% of the session. Once the
//!   oracle has moved 15 bp the burst's way over 200 slots with the lean
//!   still that way, the move is confirmed: it buys out (up), paying up to
//!   30 bp over the haircut, or keeps waiting (down).
//!   A move that gets there with the lean behind it and no burst seen puts
//!   it on watch all the same.
//! - **Resilience.** It never sweeps: one or two units a step, and only
//!   while the ask is near the maker's quote. When it lifts the maker, the
//!   ask jumps to the makers' backstop, the price check fails, and it waits
//!   for the maker to come back.
//!
//! It doesn't parse the tape, doesn't tell a fake-out from a real burst
//! except by the lean and the price that follow, and sizes nothing to the
//! move it expects.
//!
//! It builds two ways: a strategy component (`bot.wasm`, the task's oracle
//! solution), and a process (`src/main.rs`), the anchor hidden sets are
//! scored against.

use mtx_kit::{log, prelude::*};

/// The flow signal: its short and long half-lives in slots, the burst line
/// in standard deviations and how long it must hold, and the lean that
/// keeps a watch confirmed.
const SHORT_SLOTS: f64 = 30.0;
const LONG_SLOTS: f64 = 120.0;
const BURST_Z: f64 = 2.5;
const PERSIST_SLOTS: u64 = 8;
const LEAN_Z: f64 = 0.5;
/// A watch with no lean after this many slots ends.
const WATCH_SLOTS: u64 = 300;
/// The price move that confirms a burst, in basis points over 200 slots.
const CONFIRM_BPS: f64 = 15.0;
const CONFIRM_SLOTS: u64 = 200;
/// What it pays over the haircut ahead of a rise: on watch, confirmed.
const WATCH_EDGE_BPS: f64 = 10.0;
const CONFIRMED_EDGE_BPS: f64 = 30.0;
/// The taker fee, in basis points.
const TAKER_FEE_BPS: f64 = 3.5;
/// Its largest buy a step, on watch and confirmed.
const SLICE: f64 = 1.0;
const FAST_SLICE: f64 = 2.0;
/// After a sell burst, the share of the session from which it finishes.
const LATE: f64 = 0.85;
/// The units a basis point of the maker's lean stands for.
const UNITS_PER_BP: f64 = 1.0;

/// The order and its limits, from the session.
#[derive(Debug, Clone, Copy, Default)]
pub struct Order {
    pub quantity: f64,
    pub haircut_bps: f64,
    pub max_buy_price: f64,
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
        let max_buy_price = session.mandate.universe[0]
            .max_buy_price
            .unwrap_or(f64::INFINITY);
        Ok(Self {
            quantity,
            haircut_bps,
            max_buy_price,
            start_slot: session.start_slot,
            end_slot: session.end_slot,
        })
    }

    /// How far into the session `slot` is, from 0 to 1.
    pub fn progress(&self, slot: u64) -> f64 {
        let span = self.end_slot.saturating_sub(self.start_slot).max(1) as f64;
        (slot.saturating_sub(self.start_slot) as f64 / span).min(1.0)
    }

    /// The session's steps, of two slots each.
    pub fn span_steps(&self) -> f64 {
        (self.end_slot.saturating_sub(self.start_slot) as f64 / 2.0).max(1.0)
    }

    /// The highest price worth paying now: the broker's price less the
    /// fee, plus `edge_bps`, and never over the cap.
    pub fn worth_usd(&self, oracle: f64, edge_bps: f64) -> f64 {
        let bps = self.haircut_bps - TAKER_FEE_BPS + edge_bps;
        round_down_cents((oracle * (1.0 + bps / 10_000.0)).min(self.max_buy_price))
    }
}

/// The net taker flow, read through the grader's maker's lean.
#[derive(Debug, Clone, Default)]
pub struct Flow {
    /// The maker's half-spread at the first look, in basis points.
    pub base_bps: Option<f64>,
    last_lean: Option<f64>,
    last_slot: u64,
    short: f64,
    long: f64,
    /// Running mean square of a slot's flow, for the z-scores.
    noise: f64,
    seen: u64,
}

impl Flow {
    /// Take in a step: the ask over the oracle in basis points, the slots
    /// since the last step, and the desk's own buys and the sells its bid
    /// caught since then, in base units.
    pub fn update(&mut self, slot: u64, ask_bps: Option<f64>, bought: f64, caught: f64) {
        let Some(ask_bps) = ask_bps else { return };
        let base = *self.base_bps.get_or_insert(ask_bps);
        let lean = (ask_bps - base) * UNITS_PER_BP;
        let gap = slot.saturating_sub(self.last_slot).max(1) as f64;
        if let Some(last) = self.last_lean {
            let flow = (lean - last) - bought - caught;
            // A lean that jumps by the backstop is a sweep, not flow.
            let flow = flow.clamp(-4.0, 4.0);
            let decay = |half: f64| 0.5f64.powf(gap / half);
            self.short = self.short * decay(SHORT_SLOTS) + flow;
            self.long = self.long * decay(LONG_SLOTS) + flow;
            self.seen += 1;
            let w = 1.0 / (self.seen as f64).min(200.0);
            self.noise += w * (flow * flow / gap - self.noise);
        }
        self.last_lean = Some(lean);
        self.last_slot = slot;
    }

    /// The short and long averages in standard deviations of their noise.
    pub fn z(&self) -> (f64, f64) {
        if self.seen < 20 {
            return (0.0, 0.0);
        }
        let sd = |half: f64| {
            // An average over slots each with this variance, weighted 2^(-age/half).
            let r = 0.5f64.powf(2.0 / half);
            (self.noise.max(1e-6) * 2.0 / (1.0 - r * r)).sqrt()
        };
        (self.short / sd(SHORT_SLOTS), self.long / sd(LONG_SLOTS))
    }
}

/// The strategy.
#[derive(Default)]
pub struct Accumulator {
    order: Order,
    flow: Flow,
    /// Position at the last step, and what it bought with market orders
    /// then.
    held: f64,
    sent: f64,
    /// +1 on watch for a rise, -1 for a fall, from a slot.
    watch: i8,
    watch_from: u64,
    over: (u64, u64),
    oracles: std::collections::VecDeque<(u64, f64)>,
}

impl Strategy for Accumulator {
    fn start(&mut self, session: &Session) -> BotResult {
        self.order = Order::from_session(session)?;
        log(&format!(
            "buy {} by slot {}, at most {:.2}; the broker charges {} bp",
            self.order.quantity,
            self.order.end_slot,
            self.order.max_buy_price,
            self.order.haircut_bps
        ));
        Ok(())
    }

    fn step(&mut self, market: &Market, orders: &mut Orders) -> BotResult {
        let slot = market.slot;
        let held = market.position.size;
        let oracle = market.oracle;
        // What this step's fills were: the market orders it sent, then the
        // sells its bid caught.
        let delta = held - self.held;
        let bought = self.sent.min(delta.max(0.0));
        let caught = (delta - bought).max(0.0);
        self.held = held;
        self.sent = 0.0;
        let ask_bps = market.best_ask.map(|ask| (ask / oracle - 1.0) * 10_000.0);
        self.flow.update(slot, ask_bps, bought, caught);
        let (z, lean) = self.flow.z();

        // A burst that holds puts it on watch; the lean keeps it there.
        self.over.0 = if z > BURST_Z { self.over.0 + 1 } else { 0 };
        self.over.1 = if z < -BURST_Z { self.over.1 + 1 } else { 0 };
        let steps_to_persist = PERSIST_SLOTS.div_ceil(2);
        if self.over.0 >= steps_to_persist {
            (self.watch, self.watch_from) = (1, slot);
        } else if self.over.1 >= steps_to_persist {
            (self.watch, self.watch_from) = (-1, slot);
        } else if self.watch != 0 && slot > self.watch_from + WATCH_SLOTS && lean.abs() < 1.0 {
            self.watch = 0;
        }
        self.oracles.push_back((slot, oracle));
        while self
            .oracles
            .front()
            .is_some_and(|(s, _)| s + CONFIRM_SLOTS < slot)
        {
            self.oracles.pop_front();
        }
        let moved_bps = self
            .oracles
            .front()
            .map_or(0.0, |(_, then)| (oracle / then - 1.0) * 10_000.0);
        // A move under way with the flow leaning its way confirms itself,
        // burst or none.
        if self.watch == 0 && moved_bps.abs() > CONFIRM_BPS && moved_bps * lean > 0.0 {
            (self.watch, self.watch_from) = (moved_bps.signum() as i8, slot);
        }
        let w = f64::from(self.watch);
        let confirmed = self.watch != 0 && w * moved_bps > CONFIRM_BPS && w * lean > LEAN_Z;

        let q = self.order.quantity;
        let left = (q - held).max(0.0);
        orders.cancel_all();
        if left < 1e-3 {
            return Ok(());
        }
        match self.watch {
            // Ahead of a rise: half the order on watch, all of it confirmed.
            1 => {
                let (floor, edge, slice) = if confirmed {
                    (q, CONFIRMED_EDGE_BPS, FAST_SLICE)
                } else {
                    (0.5 * q, WATCH_EDGE_BPS, SLICE)
                };
                let limit = self.order.worth_usd(oracle, edge);
                let behind = (floor - held).max(0.0);
                if behind > 1e-3 && market.best_ask.is_some_and(|ask| ask <= limit) {
                    let size = round_lots(behind.min(slice).min(left));
                    if size > 0.0 {
                        orders.buy(size, limit);
                        self.sent = size;
                    }
                }
            }
            // Ahead of a fall: nothing until it is late.
            -1 => self.sent = finish(&self.order, market, orders, LATE),
            // No move: nothing until no move can start, then the late finish.
            _ => self.sent = finish(&self.order, market, orders, FINISH_FROM),
        }
        Ok(())
    }
}

/// From here on, in share of the session, a bot with no move to act on
/// works the order: no informed move starts after 70% of the session.
pub const FINISH_FROM: f64 = 0.75;
/// What a late take keeps under the broker's price, in basis points.
pub const FINISH_MARGIN_BPS: f64 = 3.0;
/// In its last slots it takes whatever beats the broker.
pub const FINAL_SLOTS: u64 = 120;
/// Its late bid's largest size, in units.
pub const FINISH_BID: f64 = 2.0;

/// The late finish, from `from` (a share of the session) to the close: a
/// bid at the front of the book, never over the broker's price less the
/// fee and a margin, and takes of the asks under that price at an even pace
/// to 97% of the session; in the last [`FINAL_SLOTS`] slots, whatever is
/// left that beats the broker. Before `from` it does nothing. Returns what
/// it sent as market orders.
pub fn finish(order: &Order, market: &Market, orders: &mut Orders, from: f64) -> f64 {
    let held = market.position.size;
    let left = round_lots((order.quantity - held).max(0.0));
    let t = order.progress(market.slot);
    if left <= 0.0 || t < from {
        return 0.0;
    }
    let limit = order.worth_usd(market.oracle, -FINISH_MARGIN_BPS);
    let size = if market.slots_left <= FINAL_SLOTS {
        left
    } else {
        let steps = (order.span_steps() * (0.97 - t)).max(1.0);
        round_lots((left / steps).max(0.001)).min(left)
    };
    let mut sent = 0.0;
    if market.best_ask.is_some_and(|ask| ask <= limit) {
        orders.buy(size, limit);
        sent = size;
    }
    let rest = round_lots(FINISH_BID.min(left - sent));
    if rest > 0.0 {
        let front = market.best_bid.map_or(limit, |bid| bid + 0.01);
        let price = round_down_cents(front.min(limit));
        if price > 0.0 && market.best_ask.is_none_or(|ask| price < ask) {
            orders.bid(price, rest).post_only();
        }
    }
    sent
}

/// The number after `key` in a flat JSON object.
pub fn number_after(json: &str, key: &str) -> Option<f64> {
    let at = json.find(key)? + key.len();
    let rest = json[at..].trim_start().strip_prefix(':')?.trim_start();
    let end = rest
        .find(|c: char| !(c.is_ascii_digit() || matches!(c, '.' | '-' | '+' | 'e' | 'E')))
        .unwrap_or(rest.len());
    rest[..end].parse().ok()
}

/// Down to Phoenix NVDA's tick, a cent.
pub fn round_down_cents(usd: f64) -> f64 {
    (usd * 100.0 + 1e-9).floor() / 100.0
}

/// To NVDA's base lot, a thousandth.
pub fn round_lots(units: f64) -> f64 {
    (units * 1000.0).floor() / 1000.0
}

#[cfg(target_arch = "wasm32")]
mtx_kit::export!(Accumulator);

#[cfg(test)]
mod tests {
    use super::*;

    /// 30 NVDA over slots 0 to 2000, a 20 bp broker, a $170 cap.
    fn order() -> Order {
        Order {
            quantity: 30.0,
            haircut_bps: 20.0,
            max_buy_price: 170.0,
            start_slot: 0,
            end_slot: 2000,
        }
    }

    fn market(slot: u64, held: f64, bid: f64, ask: f64) -> Market {
        Market {
            slot,
            slots_left: 2000 - slot,
            oracle: 160.0,
            best_bid: Some(bid),
            best_ask: Some(ask),
            position: Position {
                size: held,
                entry_usd: 160.0,
            },
            cap: 30.0,
            ..Market::default()
        }
    }

    fn intents(market: &Market, from: f64) -> Vec<Intent> {
        let mut orders = Orders::new();
        finish(&order(), market, &mut orders, from);
        orders.into_intents()
    }

    #[test]
    fn finish_does_nothing_before_its_start() {
        assert!(intents(&market(1000, 0.0, 159.9, 160.1), FINISH_FROM).is_empty());
    }

    #[test]
    fn finish_takes_under_the_broker_and_bids_a_unit_at_the_front() {
        // The broker's price less the fee and 3 bp: 160 x (1 + 13.5 bp) = 160.21.
        let got = intents(&market(1600, 10.0, 159.9, 160.1), FINISH_FROM);
        let limit = order().worth_usd(160.0, -FINISH_MARGIN_BPS);
        assert!((limit - 160.21).abs() < 1e-9);
        let Intent::Market { size, limit: l, .. } = got[0] else {
            panic!("{got:?}")
        };
        assert!(size > 0.0 && size < 20.0 && (l - limit).abs() < 1e-9);
        let Intent::Limit { price, size, .. } = got[1] else {
            panic!("{got:?}")
        };
        assert!((price - 159.91).abs() < 1e-9 && size == FINISH_BID);
    }

    #[test]
    fn finish_leaves_dear_asks_and_never_bids_over_the_broker() {
        let got = intents(&market(1600, 10.0, 160.3, 160.4), FINISH_FROM);
        assert_eq!(got.len(), 1, "{got:?}");
        let Intent::Limit { price, .. } = got[0] else {
            panic!("{got:?}")
        };
        assert!(price <= order().worth_usd(160.0, -FINISH_MARGIN_BPS));
    }

    #[test]
    fn finish_takes_the_rest_at_the_close() {
        let got = intents(&market(1950, 12.0, 159.9, 160.1), FINISH_FROM);
        let Intent::Market { size, .. } = got[0] else {
            panic!("{got:?}")
        };
        assert!((size - 18.0).abs() < 1e-9);
    }

    #[test]
    fn the_reference_buys_nothing_early_without_a_move() {
        let mut bot = Accumulator {
            order: order(),
            ..Accumulator::default()
        };
        for slot in (0..1400).step_by(2) {
            let got = step_offline(&mut bot, &market(slot, 0.0, 159.94, 160.06)).unwrap();
            assert!(
                got.iter().all(|i| matches!(i, Intent::CancelAll)),
                "slot {slot}: {got:?}"
            );
        }
    }
}
