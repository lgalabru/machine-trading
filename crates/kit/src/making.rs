//! Building blocks for a market maker, pure and composable: where to quote
//! off the book ([`touch_targets`]), how one-sided the takers have been
//! ([`taker_flow`]), and when to stop quoting the side they hit
//! ([`Guard`]). A strategy composes them in its `step`; each is a function
//! of what it is given, so each is tested on its own.
//!
//! - **Quote off the book, not the oracle.** On Phoenix the book leads and
//!   the oracle follows by seconds; a maker quoting around the oracle
//!   crosses the book and takes. [`touch_targets`] joins the touch, steps a
//!   tick inside only where the spread has room, skews against inventory,
//!   steps back from a moving book and a far oracle, and never crosses.
//! - **Watch the takers.** A maker is picked off by takers who know the
//!   next tick: their orders come one-sided just before it moves.
//!   [`taker_flow`] is the share of the recent taker volume that bought.
//! - **Watch the exchanges.** SOL's price is made off-chain and Phoenix
//!   follows about half a second later: a maker's fills right after the
//!   exchange moved against them are the worst. [`Lead`] tracks a reference
//!   venue's mid (an exchange, as the live runtime shows it in
//!   `market.venues`) and [`lead_call`] says which side its move runs into.
//! - **Keep transactions few.** [`Momentum`] tracks the mid's recent move,
//!   [`Requote`] says when quotes are worth replacing, and [`place`] sends
//!   them post-only, sized against the cap.
//! - **Stop quoting the side being hit.** When that share passes a
//!   threshold, [`Guard`] says which side to pull, and holds it a while:
//!   takers buying hard lift the asks before a rally, so the asks go.

use std::collections::VecDeque;

use mtx_core::Side;
/// The tape's types: a trade, its taker's side, and the strategy's part
/// in it, as `market.tape` holds them.
pub use mtx_core::{
    Side as TapeSide,
    turns::{Role, Trade},
};

use crate::Orders;

/// How [`touch_targets`] quotes. The defaults are the book maker's, fitted
/// on mainnet's SOL perp (`bots/book-maker`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TouchConfig {
    /// How far the inventory skew steps the adding side back at the cap,
    /// in ticks: a tick per fifth of the cap.
    pub skew_ticks_at_cap: f64,
    /// Past this share of the cap, the side that adds isn't quoted.
    pub inventory_limit: f64,
    /// The mid's move past which the side it runs into steps back, and by
    /// how many ticks: 3 bps is two ticks on SOL at $66.
    pub momentum_bps: f64,
    pub momentum_ticks: i64,
    /// The oracle gap past which both sides step back, and by how much:
    /// the live p95 is 11 bps.
    pub basis_wide_bps: f64,
    pub basis_wide_ticks: i64,
}

impl Default for TouchConfig {
    fn default() -> Self {
        Self {
            skew_ticks_at_cap: 5.0,
            inventory_limit: 0.75,
            momentum_bps: 3.0,
            momentum_ticks: 2,
            basis_wide_bps: 12.0,
            basis_wide_ticks: 1,
        }
    }
}

/// The book, the oracle and the position at a step.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct View {
    pub best_bid: f64,
    pub best_ask: f64,
    pub oracle: f64,
    /// Position over the cap, in [-1, 1].
    pub inventory: f64,
    /// The mid's move over the strategy's momentum window, in bps.
    pub momentum_bps: f64,
}

/// Where to quote, in ticks: `None` for a side not quoted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Targets {
    pub bid: Option<i64>,
    pub ask: Option<i64>,
}

impl Targets {
    /// These targets without the side `pull` names.
    pub fn without(self, pull: Pull) -> Self {
        match pull {
            Pull::Neither => self,
            Pull::Bids => Self { bid: None, ..self },
            Pull::Asks => Self { ask: None, ..self },
        }
    }
}

/// The quotes for a view, in ticks: at the touch, a tick inside where the
/// spread has room, stepped back for inventory, momentum and a wide oracle
/// gap, and never crossing the book or each other.
pub fn touch_targets(view: &View, tick: f64, config: &TouchConfig) -> Targets {
    assert!(tick > 0.0, "a tick has a size");
    assert!(view.best_ask > view.best_bid, "a book that isn't crossed");
    let best_bid = (view.best_bid / tick).round() as i64;
    let best_ask = (view.best_ask / tick).round() as i64;
    // A tick inside each side only if the two can't meet.
    let room = best_ask - best_bid >= 3;
    let mut bid = best_bid + i64::from(room);
    let mut ask = best_ask - i64::from(room);

    let skew = (view.inventory.abs() * config.skew_ticks_at_cap).round() as i64;
    if view.inventory > 0.0 {
        bid -= skew;
    } else {
        ask += skew;
    }
    if view.momentum_bps > config.momentum_bps {
        ask += config.momentum_ticks;
    } else if view.momentum_bps < -config.momentum_bps {
        bid -= config.momentum_ticks;
    }
    if view.oracle > 0.0 {
        let mid = (best_bid + best_ask) as f64 * tick / 2.0;
        let basis_bps = (mid / view.oracle - 1.0) * 10_000.0;
        if basis_bps.abs() > config.basis_wide_bps {
            bid -= config.basis_wide_ticks;
            ask += config.basis_wide_ticks;
        }
    }
    // Post-only refuses a crossing quote anyway; keep it off the book's far
    // side so it isn't refused.
    bid = bid.min(best_ask - 1);
    ask = ask.max(best_bid + 1);
    let targets = Targets {
        bid: (view.inventory < config.inventory_limit).then_some(bid),
        ask: (view.inventory > -config.inventory_limit).then_some(ask),
    };
    if let (Some(bid), Some(ask)) = (targets.bid, targets.ask) {
        assert!(bid < ask, "the quotes don't cross: {bid} {ask}");
    }
    targets
}

/// The takers' recent flow: the share of their volume that bought, from
/// -1 (all sold) to +1 (all bought), and how much volume that is.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Flow {
    pub share: f64,
    pub volume: f64,
}

/// The takers' flow in the `window_slots` up to and including `slot`,
/// from `tape`, leaving out the strategy's own trades. `None` if nothing
/// else traded then.
pub fn taker_flow(tape: &[Trade], slot: u64, window_slots: u64) -> Option<Flow> {
    let from = slot.saturating_sub(window_slots.saturating_sub(1));
    let (mut signed, mut volume) = (0.0, 0.0);
    for trade in tape {
        let theirs = trade.own.is_none() || trade.own == Some(Role::Maker);
        if !(theirs && (from..=slot).contains(&trade.slot) && trade.base.is_finite()) {
            continue;
        }
        let size = trade.base.abs();
        volume += size;
        signed += match trade.side {
            Side::Buy => size,
            Side::Sell => -size,
        };
    }
    (volume > 0.0).then(|| Flow {
        share: (signed / volume).clamp(-1.0, 1.0),
        volume,
    })
}

/// Which side to stop quoting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pull {
    Neither,
    /// Takers are selling: the bids are hit before the price falls.
    Bids,
    /// Takers are buying: the asks are lifted before the price rises.
    Asks,
}

/// When [`Guard`] pulls a side.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GuardConfig {
    /// The flow's share past which it pulls: 0.6 is four fifths of the
    /// volume one way.
    pub threshold: f64,
    /// Less volume than this, in base units, says nothing.
    pub volume_min: f64,
    /// How long a pull holds after the flow last passed the threshold.
    pub hold_slots: u64,
}

/// Pulls the side one-sided takers hit, and holds it for
/// [`GuardConfig::hold_slots`] after the flow last called for it. A flow
/// the other way past the threshold turns the pull at once.
#[derive(Debug, Clone, PartialEq)]
pub struct Guard {
    config: GuardConfig,
    pulled: Option<(Pull, u64)>,
}

impl Guard {
    pub fn new(config: GuardConfig) -> Self {
        assert!(
            (0.0..=1.0).contains(&config.threshold),
            "a threshold is a share: {}",
            config.threshold
        );
        assert!(config.volume_min >= 0.0, "a volume is not negative");
        Self {
            config,
            pulled: None,
        }
    }

    /// The side to pull at `slot`, given the flow then.
    pub fn update(&mut self, slot: u64, flow: Option<Flow>) -> Pull {
        let called = flow
            .filter(|f| f.volume >= self.config.volume_min)
            .and_then(|f| match f.share {
                s if s >= self.config.threshold => Some(Pull::Asks),
                s if s <= -self.config.threshold => Some(Pull::Bids),
                _ => None,
            });
        self.hold(slot, called)
    }

    /// The side to pull at `slot`, given what some signal called for then
    /// (`None`: nothing new), held [`GuardConfig::hold_slots`] after the
    /// last call. A call the other way turns the pull at once.
    pub fn hold(&mut self, slot: u64, called: Option<Pull>) -> Pull {
        if let Some(pull) = called.filter(|p| *p != Pull::Neither) {
            self.pulled = Some((pull, slot.saturating_add(self.config.hold_slots)));
        }
        match self.pulled {
            Some((pull, until)) if slot < until => pull,
            _ => {
                self.pulled = None;
                Pull::Neither
            }
        }
    }
}

/// A reference venue's mid (an exchange's book, as `market.venues` shows
/// it), and its move over the last `window_slots` slots.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Lead {
    window_slots: u64,
    mids: VecDeque<(u64, f64)>,
}

impl Lead {
    pub fn new(window_slots: u64) -> Self {
        assert!(window_slots > 0, "a window of at least a slot");
        Self {
            window_slots,
            mids: VecDeque::new(),
        }
    }

    /// Take the reference's mid at `slot`; its move since the newest mid
    /// at least `window_slots` before, in bps. `None` until the history
    /// covers the window, and for a slot without a reference (the stream
    /// down or stale), which also forgets what came before: a gap isn't a
    /// move.
    pub fn push(&mut self, slot: u64, mid: Option<f64>) -> Option<f64> {
        let Some(mid) = mid.filter(|m| m.is_finite() && *m > 0.0) else {
            self.mids.clear();
            return None;
        };
        if self.mids.back().is_some_and(|(s, _)| *s >= slot) {
            self.mids.pop_back();
        }
        self.mids.push_back((slot, mid));
        // The window's start; none yet this early in the chain's slots.
        let from = slot.checked_sub(self.window_slots)?;
        // Keep the newest mid at or before the window's start, and after.
        while self.mids.len() > 1 && self.mids[1].0 <= from {
            self.mids.pop_front();
        }
        let (first_slot, first) = *self.mids.front()?;
        (first_slot <= from).then(|| (mid / first - 1.0) * 10_000.0)
    }
}

/// The side a reference's move runs into: a rise past `threshold_bps`
/// lifts the asks before the book follows, a fall hits the bids.
pub fn lead_call(move_bps: Option<f64>, threshold_bps: f64) -> Option<Pull> {
    assert!(threshold_bps > 0.0, "a threshold is a move");
    match move_bps? {
        m if m >= threshold_bps => Some(Pull::Asks),
        m if m <= -threshold_bps => Some(Pull::Bids),
        _ => None,
    }
}

/// The mid of the venue named `id` in `venues`, if it has a book.
pub fn venue_mid(venues: &[crate::VenueView], id: &str) -> Option<f64> {
    let venue = venues.iter().find(|v| v.id == id)?;
    let (bid, ask) = (venue.best_bid?, venue.best_ask?);
    (bid > 0.0 && ask >= bid).then_some((bid + ask) / 2.0)
}

/// The mid's move over the last few steps, in bps.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Momentum {
    steps: usize,
    mids: VecDeque<f64>,
}

impl Momentum {
    /// The move over `steps` steps.
    pub fn new(steps: usize) -> Self {
        assert!(steps > 0, "a window of at least a step");
        Self {
            steps,
            mids: VecDeque::with_capacity(steps + 1),
        }
    }

    /// Take this step's mid; the move from `steps` steps ago, in bps.
    pub fn push(&mut self, mid: f64) -> f64 {
        assert!(mid > 0.0 && mid.is_finite(), "a mid is a price: {mid}");
        self.mids.push_back(mid);
        while self.mids.len() > self.steps + 1 {
            self.mids.pop_front();
        }
        debug_assert!(self.mids.len() <= self.steps + 1);
        match self.mids.front() {
            Some(first) => (mid / first - 1.0) * 10_000.0,
            None => 0.0,
        }
    }
}

/// When quotes are worth replacing: a price or a size changed (a fill
/// changes what a side may quote), or they are older than `every_slots` (a
/// quote the book refused as crossing comes back).
#[derive(Debug, Clone, PartialEq)]
pub struct Requote {
    every_slots: u64,
    last: Option<(u64, Quotes)>,
}

impl Requote {
    pub fn new(every_slots: u64) -> Self {
        Self {
            every_slots,
            last: None,
        }
    }

    /// Whether to replace what rests at `slot` with `quotes`; if so, they
    /// are taken as placed.
    pub fn due(&mut self, slot: u64, quotes: Quotes) -> bool {
        let due = self.last.is_none_or(|(at, placed)| {
            placed != quotes || slot >= at.saturating_add(self.every_slots)
        });
        if due {
            self.last = Some((slot, quotes));
        }
        due
    }
}

/// How [`quotes`] sizes them.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Sizing {
    /// Each quote's size, base units.
    pub size: f64,
    /// The position cap, base units.
    pub cap: f64,
    /// A quote under this isn't sent: a venue's base lot.
    pub size_min: f64,
}

/// What to rest: each side's price in ticks and size, `None` for a side
/// not quoted.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Quotes {
    pub bid: Option<(i64, f64)>,
    pub ask: Option<(i64, f64)>,
}

/// `targets` sized against the cap. A strategy sees its position a step
/// late (the fills since its last step are applied after it decides), so
/// the quote resting on a side may have filled unseen: each side's size
/// leaves room for that one, and no unseen fill and new fill together can
/// take the position past the cap.
pub fn quotes(targets: Targets, held: f64, sizing: Sizing) -> Quotes {
    assert!(sizing.cap > 0.0 && sizing.size >= 0.0, "a cap and a size");
    let in_flight = sizing.size;
    let room_to_buy = (sizing.cap - held - in_flight).max(0.0);
    let room_to_sell = (sizing.cap + held - in_flight).max(0.0);
    let side = |price: Option<i64>, room: f64| {
        let size = sizing.size.min(room);
        price.filter(|_| size >= sizing.size_min).map(|p| (p, size))
    };
    let quotes = Quotes {
        bid: side(targets.bid, room_to_buy),
        ask: side(targets.ask, room_to_sell),
    };
    debug_assert!(
        quotes
            .bid
            .is_none_or(|(_, s)| held + s + in_flight <= sizing.cap + 1e-9)
    );
    debug_assert!(
        quotes
            .ask
            .is_none_or(|(_, s)| held - s - in_flight >= -sizing.cap - 1e-9)
    );
    quotes
}

/// Cancel what rests and rest `quotes`, post-only.
pub fn place(orders: &mut Orders, quotes: Quotes, tick: f64) {
    assert!(tick > 0.0, "a tick has a size");
    orders.cancel_all();
    if let Some((bid, size)) = quotes.bid {
        orders.bid(bid as f64 * tick, size).post_only();
    }
    if let Some((ask, size)) = quotes.ask {
        orders.ask(ask as f64 * tick, size).post_only();
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    const TICK: f64 = 0.01;

    fn trade(slot: u64, side: Side, base: f64) -> Trade {
        Trade {
            venue: "phoenix".into(),
            market: Some("phoenix:perp:SOL".into()),
            side,
            price: 70.8,
            base,
            slot,
            taker: None,
            maker: None,
            own: None,
        }
    }

    fn views() -> impl Strategy<Value = View> {
        (
            6000i64..7000,
            1i64..8,
            -1.0f64..=1.0,
            -20.0f64..20.0,
            -0.003f64..0.003,
        )
            .prop_map(|(bid, width, inventory, momentum_bps, gap)| {
                let (best_bid, best_ask) = (bid as f64 * TICK, (bid + width) as f64 * TICK);
                View {
                    best_bid,
                    best_ask,
                    oracle: (best_bid + best_ask) / 2.0 * (1.0 + gap),
                    inventory,
                    momentum_bps,
                }
            })
    }

    fn tapes() -> impl Strategy<Value = Vec<Trade>> {
        prop::collection::vec(
            (0u64..100, any::<bool>(), 0.001f64..50.0).prop_map(|(slot, buy, base)| {
                trade(slot, if buy { Side::Buy } else { Side::Sell }, base)
            }),
            0..60,
        )
    }

    proptest! {
        /// Whatever the view, the quotes stay off the book's far side and
        /// apart, and the side that adds goes past the inventory limit.
        #[test]
        fn touch_never_crosses(view in views()) {
            let config = TouchConfig::default();
            let t = touch_targets(&view, TICK, &config);
            let best_bid = (view.best_bid / TICK).round() as i64;
            let best_ask = (view.best_ask / TICK).round() as i64;
            prop_assert!(t.bid.is_none_or(|b| b < best_ask));
            prop_assert!(t.ask.is_none_or(|a| a > best_bid));
            if let (Some(b), Some(a)) = (t.bid, t.ask) { prop_assert!(b < a); }
            prop_assert_eq!(t.bid.is_none(), view.inventory >= config.inventory_limit);
            prop_assert_eq!(t.ask.is_none(), view.inventory <= -config.inventory_limit);
        }

        /// A bid never improves on the touch by more than a tick, nor an
        /// ask: a maker joins or betters the book, it doesn't jump ahead.
        #[test]
        fn touch_stays_near_the_book(view in views()) {
            let t = touch_targets(&view, TICK, &TouchConfig::default());
            prop_assert!(t.bid.is_none_or(|b| b <= (view.best_bid / TICK).round() as i64 + 1));
            prop_assert!(t.ask.is_none_or(|a| a >= (view.best_ask / TICK).round() as i64 - 1));
        }

        /// Pulling only ever removes a side.
        #[test]
        fn pulling_removes_and_never_adds(view in views()) {
            let t = touch_targets(&view, TICK, &TouchConfig::default());
            for pull in [Pull::Neither, Pull::Bids, Pull::Asks] {
                let p = t.without(pull);
                prop_assert!(p.bid.is_none() || p.bid == t.bid);
                prop_assert!(p.ask.is_none() || p.ask == t.ask);
            }
            prop_assert_eq!(t.without(Pull::Neither), t);
        }

        /// The flow is a share, its volume the window's, and mirroring the
        /// takers' sides mirrors it.
        #[test]
        fn flow_is_a_share_that_mirrors(tape in tapes(), slot in 0u64..110, window in 1u64..30) {
            let in_window: Vec<&Trade> = tape.iter().filter(|t| t.slot + window > slot && t.slot <= slot).collect();
            let flow = taker_flow(&tape, slot, window);
            prop_assert_eq!(flow.is_some(), !in_window.is_empty());
            let mirrored: Vec<Trade> = tape.iter().map(|t| Trade {
                side: if t.side == Side::Buy { Side::Sell } else { Side::Buy }, ..t.clone()
            }).collect();
            if let (Some(f), Some(m)) = (flow, taker_flow(&mirrored, slot, window)) {
                prop_assert!((-1.0..=1.0).contains(&f.share));
                prop_assert!((f.share + m.share).abs() < 1e-9);
                let volume: f64 = in_window.iter().map(|t| t.base).sum();
                prop_assert!((f.volume - volume).abs() < 1e-6);
            }
        }

        /// A guard pulls only past its threshold and enough volume, holds
        /// a pull exactly its hold, and quiet flow releases it after.
        #[test]
        fn the_guard_pulls_past_the_threshold_and_holds(
            share in -1.0f64..=1.0, volume in 0.0f64..10.0, slot in 0u64..1000, hold in 1u64..20,
        ) {
            let config = GuardConfig { threshold: 0.6, volume_min: 1.0, hold_slots: hold };
            let mut guard = Guard::new(config);
            let pull = guard.update(slot, Some(Flow { share, volume }));
            let expected = match (volume >= 1.0, share) {
                (true, s) if s >= 0.6 => Pull::Asks,
                (true, s) if s <= -0.6 => Pull::Bids,
                _ => Pull::Neither,
            };
            prop_assert_eq!(pull, expected);
            for later in 1..hold {
                prop_assert_eq!(guard.update(slot + later, None), expected);
            }
            prop_assert_eq!(guard.update(slot + hold, None), Pull::Neither);
        }
    }

    proptest! {
        /// The momentum is the move from the window's first mid: zero for a
        /// flat market, and it forgets what left the window.
        #[test]
        fn momentum_is_the_move_over_its_window(mids in prop::collection::vec(1.0f64..200.0, 1..40), steps in 1usize..8) {
            let mut momentum = Momentum::new(steps);
            let mut last = 0.0;
            for &mid in &mids { last = momentum.push(mid); }
            let first = mids[mids.len().saturating_sub(steps + 1)];
            let expected = (mids[mids.len() - 1] / first - 1.0) * 10_000.0;
            prop_assert!((last - expected).abs() < 1e-6);
            let mut flat = Momentum::new(steps);
            for _ in 0..10 { prop_assert_eq!(flat.push(66.5), 0.0); }
        }

        /// A requote is due for new prices or sizes, or once the quotes are
        /// old, and not otherwise.
        #[test]
        fn requotes_come_when_quotes_change_or_age(every in 1u64..50, gap in 0u64..100, bid in 0i64..10, moved in any::<bool>(), resized in any::<bool>()) {
            let mut requote = Requote::new(every);
            let first = Quotes { bid: Some((bid, 1.0)), ask: Some((bid + 2, 1.0)) };
            prop_assert!(requote.due(100, first));
            let mut next = first;
            if moved { next.bid = Some((bid - 1, 1.0)); }
            if resized { next.ask = Some((bid + 2, 0.5)); }
            prop_assert_eq!(requote.due(100 + gap, next), moved || resized || gap >= every);
        }

        /// Seen a step late, with every resting quote filling at random
        /// between steps, the position never passes the cap.
        #[test]
        fn a_position_seen_late_never_passes_the_cap(fills in prop::collection::vec((any::<bool>(), any::<bool>()), 1..200), size_share in 0.05f64..1.0) {
            let sizing = Sizing { size: size_share, cap: 1.0, size_min: 0.001 };
            let (mut held, mut seen) = (0.0f64, 0.0f64);
            let mut resting = Quotes { bid: None, ask: None };
            for (bid_fills, ask_fills) in fills {
                // The step decides on what it saw; then the trades since fill
                // what rested, and only after that does it see them.
                let placed = quotes(Targets { bid: Some(6654), ask: Some(6655) }, seen, sizing);
                let before = resting;
                resting = placed;
                if bid_fills { if let Some((_, s)) = before.bid { held += s; } }
                if ask_fills { if let Some((_, s)) = before.ask { held -= s; } }
                prop_assert!(held.abs() <= 1.0 + 1e-9, "held {held}");
                seen = held;
            }
        }
    }

    /// Found by the property test: a position already past the cap (a
    /// forced fill) is only quoted back, never further.
    #[test]
    fn past_the_cap_only_the_way_back_is_quoted() {
        let sizing = Sizing {
            size: 0.5,
            cap: 1.0,
            size_min: 0.001,
        };
        let both = Targets {
            bid: Some(6654),
            ask: Some(6655),
        };
        let long = quotes(both, 1.2, sizing);
        assert_eq!((long.bid, long.ask), (None, Some((6655, 0.5))));
        let short = quotes(both, -1.2, sizing);
        assert_eq!((short.bid, short.ask), (Some((6654, 0.5)), None));
        let mut orders = Orders::default();
        place(&mut orders, long, TICK);
        assert_eq!(orders.intents().len(), 2, "a cancel and the ask");
    }

    proptest! {
        /// The lead is the move since the newest mid at least the window
        /// back, it waits for the window, and a gap forgets the history.
        #[test]
        fn the_lead_is_the_move_over_its_window(mids in prop::collection::vec(60.0f64..80.0, 1..30), window in 1u64..8) {
            let mut lead = Lead::new(window);
            let mut last = None;
            for (i, &m) in mids.iter().enumerate() {
                last = lead.push(i as u64, Some(m));
            }
            let n = mids.len() as u64;
            match last {
                Some(moved) => {
                    prop_assert!(n > window);
                    let first = mids[(n - 1 - window) as usize];
                    prop_assert!((moved - (mids[mids.len() - 1] / first - 1.0) * 10_000.0).abs() < 1e-9);
                }
                None => prop_assert!(n <= window),
            }
            prop_assert_eq!(lead.push(n, None), None);
            prop_assert_eq!(lead.push(n + 1, Some(70.0)), None);
        }

        /// A call is symmetric, and nothing under the threshold calls.
        #[test]
        fn lead_calls_are_symmetric(m in -10.0f64..10.0, threshold in 0.1f64..5.0) {
            let up = lead_call(Some(m), threshold);
            let down = lead_call(Some(-m), threshold);
            match up {
                Some(Pull::Asks) => prop_assert_eq!(down, Some(Pull::Bids)),
                Some(Pull::Bids) => prop_assert_eq!(down, Some(Pull::Asks)),
                _ => { prop_assert_eq!(up, None); prop_assert_eq!(down, None); prop_assert!(m.abs() < threshold); }
            }
            prop_assert_eq!(lead_call(None, threshold), None);
        }
    }

    #[test]
    fn a_venue_mid_is_found_by_name() {
        let venues = vec![crate::VenueView {
            id: "coinbase:spot:SOL-USD".into(),
            best_bid: Some(69.04),
            best_ask: Some(69.06),
            ..Default::default()
        }];
        assert!((venue_mid(&venues, "coinbase:spot:SOL-USD").unwrap() - 69.05).abs() < 1e-9);
        assert_eq!(venue_mid(&venues, "binance:spot:SOLUSDT"), None);
    }

    #[test]
    fn own_taker_trades_are_left_out() {
        let mut mine = trade(10, Side::Buy, 5.0);
        mine.own = Some(Role::Taker);
        let tape = vec![mine, trade(10, Side::Sell, 1.0)];
        let flow = taker_flow(&tape, 10, 4).unwrap();
        assert_eq!(flow.share, -1.0);
        assert_eq!(flow.volume, 1.0);
        // A trade where the strategy made is still the taker's flow.
        let mut made = trade(10, Side::Buy, 2.0);
        made.own = Some(Role::Maker);
        assert_eq!(taker_flow(&[made], 10, 4).unwrap().share, 1.0);
    }

    #[test]
    fn a_turn_in_the_flow_turns_the_pull_at_once() {
        let mut guard = Guard::new(GuardConfig {
            threshold: 0.6,
            volume_min: 0.0,
            hold_slots: 10,
        });
        let buying = Some(Flow {
            share: 0.9,
            volume: 3.0,
        });
        let selling = Some(Flow {
            share: -0.9,
            volume: 3.0,
        });
        assert_eq!(guard.update(1, buying), Pull::Asks);
        assert_eq!(guard.update(2, selling), Pull::Bids);
        assert_eq!(guard.update(11, None), Pull::Bids);
        assert_eq!(guard.update(12, None), Pull::Neither);
    }

    /// The book maker's own cases, now here.
    #[test]
    fn the_book_makers_cases_hold() {
        let config = TouchConfig::default();
        let view = |bid: f64, ask: f64| View {
            best_bid: bid,
            best_ask: ask,
            oracle: (bid + ask) / 2.0,
            inventory: 0.0,
            momentum_bps: 0.0,
        };
        let t = |v: View| touch_targets(&v, TICK, &config);
        assert_eq!(
            t(view(66.54, 66.55)),
            Targets {
                bid: Some(6654),
                ask: Some(6655)
            }
        );
        assert_eq!(
            t(view(66.50, 66.55)),
            Targets {
                bid: Some(6651),
                ask: Some(6654)
            }
        );
        assert_eq!(
            t(view(66.50, 66.52)),
            Targets {
                bid: Some(6650),
                ask: Some(6652)
            }
        );
        let far = View {
            oracle: 66.445,
            ..view(66.54, 66.55)
        };
        assert_eq!(
            t(far),
            Targets {
                bid: Some(6653),
                ask: Some(6656)
            }
        );
        let long = View {
            inventory: 0.4,
            ..view(66.54, 66.55)
        };
        assert_eq!(
            t(long),
            Targets {
                bid: Some(6652),
                ask: Some(6655)
            }
        );
        let rally = View {
            momentum_bps: 4.0,
            ..view(66.54, 66.55)
        };
        assert_eq!(
            t(rally),
            Targets {
                bid: Some(6654),
                ask: Some(6657)
            }
        );
    }
}
