//! The grader's own maker in a market-making session (see
//! [`crate::making`]).
//!
//! The snapshot's makers are splines the grader can only reshape before the
//! cheatcodes lock, so on their own they stand still: a bot one tick inside
//! them collects the spread on every order, unopposed. This maker is the
//! opposition. Funded at setup like the takers, it trades from its own seat
//! through the grader's own RPC with ordinary orders, between the bot's
//! turns, on whatever market its seat is on.
//!
//! Every `react_slots` it reads the best bid and offer. A side where it
//! isn't best means someone quotes inside it: it steps one tick inside them,
//! never closer to the level than its floor. A side where it is best, it
//! eases one tick back out toward the competitor's half-spread, the way a
//! maker probes for a wider price once nobody is undercutting. And it skews
//! both quotes against its inventory, so it doesn't warehouse the flow.
//!
//! A scenario can opt into the reactive competitor
//! ([`Competition::reactive`]), which differs in four ways. It keeps its
//! prices in whole ticks, so a step inside a rival is exactly the rival's
//! price plus its step and never one tick off by a float's rounding. It
//! reacts only to rivals resting at least `q_react` (smaller quotes are odd
//! lots and set no touch). Its look delay and its step are drawn from the
//! session's seeded randomness, so a run replays but a bot can't time its
//! quotes to a fixed cadence. And after one-sided fills it quotes the hit
//! side at its full half-spread for a while, as a maker does against toxic
//! flow. It pulls its own orders before it reads the book at each look, so
//! the touch it reads is the rivals' alone and it never ties one.

use std::collections::VecDeque;

use async_trait::async_trait;
use mtx_kit::Side;
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use super::{Actor, ActorsReport, Moment};
use crate::{
    making::{Competition, Making, Reactive},
    scenario::SplitMix64,
    venue::Seat,
};

/// How far its quotes lean against what it holds, in basis points per base
/// unit, and the inventory at which it stops quoting the side that adds.
const SKEW_BPS_PER_UNIT: f64 = 1.0;
const MAX_INVENTORY: f64 = 25.0;
/// Slack, in ticks, when a price in ticks is rounded to the grid, so a price
/// that is on a tick in decimal isn't pushed off it by a float's error.
const TICK_EPS: f64 = 1e-9;

/// What the grader's maker did in a session.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct CompeteReport {
    /// Times it requoted, and how many of those it stepped inside a quote
    /// better than its own.
    pub requotes: usize,
    pub undercuts: usize,
    /// Its tightest half-spread, in basis points.
    pub tightest_bps: f64,
    /// Its position at the end, in base units.
    pub end_position: f64,
    /// Times the reactive competitor read one-sided fills as toxic flow and
    /// widened the side.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub widenings: usize,
}

fn is_zero(n: &usize) -> bool {
    *n == 0
}

/// The grader's maker through a session.
pub struct Competitor {
    seat: Box<dyn Seat>,
    making: Making,
    report: CompeteReport,
    /// Its half-spread on each side, in basis points: where it quotes before
    /// skew.
    bid_bps: f64,
    ask_bps: f64,
    /// Its resting prices, to tell its own quotes from a better one.
    quoted: Option<(f64, f64)>,
    /// The next slot into the session at which it looks at the book.
    next_look: u64,
    /// The reactive competitor's state, where the scenario opts in.
    reactive: Option<ReactiveState>,
}

/// What the reactive competitor carries between looks.
struct ReactiveState {
    rules: Reactive,
    rng: SplitMix64,
    /// Its half-spread on each side, in whole ticks.
    bid_off: i64,
    ask_off: i64,
    /// Whether it has orders resting.
    resting: bool,
    /// Its position at the last step, and its recent changes by slot.
    position: f64,
    moves: VecDeque<(u64, f64)>,
    /// Until which slot each side is quoted at its full half-spread.
    wide_bid_until: u64,
    wide_ask_until: u64,
}

impl Competitor {
    /// The maker quoting from `seat` as `making` says, before its first
    /// quote.
    pub fn new(seat: Box<dyn Seat>, making: &Making, seed: [u8; 32]) -> Self {
        let reactive = making.competition.reactive.map(|rules| ReactiveState {
            rules,
            rng: SplitMix64(
                seed.chunks_exact(8)
                    .map(|c| u64::from_le_bytes(c.try_into().expect("8 bytes")))
                    .fold(0, |acc, word| acc.rotate_left(17) ^ word),
            ),
            bid_off: i64::MAX,
            ask_off: i64::MAX,
            resting: false,
            position: 0.0,
            moves: VecDeque::new(),
            wide_bid_until: 0,
            wide_ask_until: 0,
        });
        Self {
            reactive,
            seat,
            report: CompeteReport {
                tightest_bps: making.competitor_bps,
                ..CompeteReport::default()
            },
            bid_bps: making.competitor_bps,
            ask_bps: making.competitor_bps,
            quoted: None,
            next_look: 0,
            making: making.clone(),
        }
    }

    /// Look at the book and requote, if it's due `now`.
    async fn quote(&mut self, now: &Moment) {
        if self.reactive.is_some() {
            return self.quote_reactive(now).await;
        }
        let making = &self.making;
        let c: Competition = making.competition;
        if now.at < self.next_look {
            return;
        }
        self.next_look = now.at + c.react_slots;
        let level = now.level_usd;
        let tick = self.seat.tick();
        let tick_bps = tick / level * 10_000.0;

        if let (Some((bid, ask)), Ok((best_bid, best_ask))) = (self.quoted, self.seat.top().await) {
            let best_bid = best_bid.unwrap_or(0.0);
            let best_ask = best_ask.unwrap_or(0.0);
            // The splines' level: quotes there are the background, not a
            // rival. Only a quote inside both it and its own counts.
            let wide = making.competitor_bps / 10_000.0;
            let (spline_bid, spline_ask) = (level * (1.0 - wide), level * (1.0 + wide));
            // Someone bids above it: step inside them, down to the floor.
            if best_bid > bid.max(spline_bid) + tick / 2.0 {
                let theirs = (level - best_bid) / level * 10_000.0;
                self.bid_bps = (theirs - tick_bps).max(c.floor_bps);
                self.report.undercuts += 1;
            } else {
                self.bid_bps = (self.bid_bps + tick_bps).min(making.competitor_bps);
            }
            if best_ask > 0.0 && best_ask < ask.min(spline_ask) - tick / 2.0 {
                let theirs = (best_ask - level) / level * 10_000.0;
                self.ask_bps = (theirs - tick_bps).max(c.floor_bps);
                self.report.undercuts += 1;
            } else {
                self.ask_bps = (self.ask_bps + tick_bps).min(making.competitor_bps);
            }
        }
        self.report.tightest_bps = self.report.tightest_bps.min(self.bid_bps).min(self.ask_bps);

        let held = match self.seat.position().await {
            Ok(position) => position,
            Err(error) => {
                warn!("competitor reading its position: {error}");
                return;
            }
        };
        let skew = SKEW_BPS_PER_UNIT * held;
        let bid = round_down(level * (1.0 - (self.bid_bps + skew) / 10_000.0), tick);
        let ask = round_up(level * (1.0 + (self.ask_bps - skew) / 10_000.0), tick);
        if self.quoted == Some((bid, ask)) {
            return;
        }
        if let Err(error) = self.seat.cancel_all().await {
            warn!("competitor cancelling: {error}");
            return;
        }
        if held < MAX_INVENTORY
            && let Err(error) = self.seat.rest(Side::Bid, bid, c.size).await
        {
            warn!("competitor bidding: {error}");
        }
        if held > -MAX_INVENTORY
            && let Err(error) = self.seat.rest(Side::Ask, ask, c.size).await
        {
            warn!("competitor offering: {error}");
        }
        self.quoted = Some((bid, ask));
        self.report.requotes += 1;
    }

    /// The reactive competitor's step: note its fills, and look at the book
    /// and requote if it's due or just read toxic flow.
    async fn quote_reactive(&mut self, now: &Moment) {
        let c: Competition = self.making.competition;
        let held = match self.seat.position().await {
            Ok(position) => position,
            Err(error) => {
                warn!("competitor reading its position: {error}");
                return;
            }
        };
        let state = self.reactive.as_mut().expect("reactive");
        let rules = state.rules;
        let moved = held - state.position;
        state.position = held;
        if moved != 0.0 {
            state.moves.push_back((now.at, moved));
        }
        while state
            .moves
            .front()
            .is_some_and(|(at, _)| at + rules.toxic_window <= now.at)
        {
            state.moves.pop_front();
        }
        // Net buying is its bid being hit.
        let net: f64 = state.moves.iter().map(|(_, m)| m).sum();
        let threshold = rules.toxic_size_mult * c.size;
        let mut toxic = false;
        for (hit, until) in [
            (net >= threshold, &mut state.wide_bid_until),
            (-net >= threshold, &mut state.wide_ask_until),
        ] {
            if hit && now.at >= *until {
                *until = now.at + draw_u64(&mut state.rng, rules.widen_slots);
                toxic = true;
            }
        }
        if toxic {
            self.report.widenings += 1;
            self.next_look = now.at;
        }
        if now.at < self.next_look {
            return;
        }
        let state = self.reactive.as_mut().expect("reactive");
        let delay = (c.react_slots as f64
            * (1.0 + rules.jitter * (2.0 * state.rng.next_f64() - 1.0)))
        .round()
        .max(1.0) as u64;
        self.next_look = now.at + delay;

        let tick = self.seat.tick();
        let level_t = now.level_usd / tick;
        let wide_t = level_t * self.making.competitor_bps / 10_000.0;
        let wide_off = ceil_ticks(wide_t).max(1);
        let floor_off = ceil_ticks(level_t * c.floor_bps / 10_000.0)
            .max(1)
            .min(wide_off);
        let (below, above) = (floor_ticks(level_t), ceil_ticks(level_t));

        // Its own orders come off the book first, so what the book then
        // shows is the rivals' alone: it can step inside them exactly, and
        // never mistake its own price for theirs.
        if self.reactive.as_ref().is_some_and(|s| s.resting) {
            if let Err(error) = self.seat.cancel_all().await {
                warn!("competitor cancelling: {error}");
                return;
            }
            self.reactive.as_mut().expect("reactive").resting = false;
        }
        let book = self.seat.top_sized(rules.q_react).await;
        let state = self.reactive.as_mut().expect("reactive");
        let widened = (now.at < state.wide_bid_until, now.at < state.wide_ask_until);
        let (best_bid, best_ask) = match book {
            Ok(top) => top,
            Err(error) => {
                warn!("competitor reading the book: {error}");
                (None, None)
            }
        };
        // The splines' level: quotes there are the background, not a rival.
        let spline_bid = floor_ticks(level_t - wide_t);
        let spline_ask = ceil_ticks(level_t + wide_t);
        let mut undercuts = 0;
        let step = draw_u32(&mut state.rng, rules.step_ticks) as i64;
        state.bid_off = match best_bid.map(|p| to_ticks(p, tick)) {
            _ if widened.0 => wide_off,
            Some(theirs) if theirs > spline_bid => {
                undercuts += 1;
                (below - (theirs + step)).max(floor_off)
            }
            // Nobody inside it: ease a tick back out toward its width.
            _ => state.bid_off.saturating_add(1).min(wide_off),
        };
        let step = draw_u32(&mut state.rng, rules.step_ticks) as i64;
        state.ask_off = match best_ask.map(|p| to_ticks(p, tick)) {
            _ if widened.1 => wide_off,
            Some(theirs) if theirs < spline_ask => {
                undercuts += 1;
                ((theirs - step) - above).max(floor_off)
            }
            _ => state.ask_off.saturating_add(1).min(wide_off),
        };
        self.report.undercuts += undercuts;
        self.requote(level_t, tick, held).await;
    }

    /// Send the reactive competitor's quotes, from its half-spreads in ticks.
    async fn requote(&mut self, level_t: f64, tick: f64, held: f64) {
        let c: Competition = self.making.competition;
        let state = self.reactive.as_mut().expect("reactive");
        let skew = (SKEW_BPS_PER_UNIT * held * level_t / 10_000.0).round() as i64;
        let bid = floor_ticks(level_t) - state.bid_off - skew;
        let ask = ceil_ticks(level_t) + state.ask_off - skew;
        let half = |off: i64| off as f64 / level_t * 10_000.0;
        self.report.tightest_bps = self
            .report
            .tightest_bps
            .min(half(state.bid_off))
            .min(half(state.ask_off));
        if held < MAX_INVENTORY
            && let Err(error) = self.seat.rest(Side::Bid, from_ticks(bid, tick), c.size).await
        {
            warn!("competitor bidding: {error}");
        }
        if held > -MAX_INVENTORY
            && let Err(error) = self.seat.rest(Side::Ask, from_ticks(ask, tick), c.size).await
        {
            warn!("competitor offering: {error}");
        }
        state.resting = true;
        self.report.requotes += 1;
    }
}

#[async_trait]
impl Actor for Competitor {
    async fn after_blocks(&mut self, now: &Moment) {
        self.quote(now).await;
    }

    async fn report(&mut self, report: &mut ActorsReport) {
        self.report.end_position = self.seat.position().await.unwrap_or(0.0);
        info!(report = ?self.report, "competitor done");
        report.competitor = Some(self.report.clone());
    }
}

/// The tick a price on the grid is at.
fn to_ticks(usd: f64, tick: f64) -> i64 {
    (usd / tick).round() as i64
}

/// The price of a tick, as the decimal it is: a product like `3 * 0.1`
/// would be a hair under it, and an order's price rounds down.
fn from_ticks(ticks: i64, tick: f64) -> f64 {
    format!("{:.9}", ticks as f64 * tick)
        .parse()
        .unwrap_or(f64::NAN)
}

/// The whole tick at or below `ticks`, and the one at or above it, allowing
/// for float error in the figure.
fn floor_ticks(ticks: f64) -> i64 {
    (ticks + TICK_EPS).floor() as i64
}

fn ceil_ticks(ticks: f64) -> i64 {
    (ticks - TICK_EPS).ceil() as i64
}

fn draw_u64(rng: &mut SplitMix64, (lo, hi): (u64, u64)) -> u64 {
    rng.range_u64((lo, hi))
}

fn draw_u32(rng: &mut SplitMix64, (lo, hi): (u32, u32)) -> u32 {
    rng.range_u64((u64::from(lo), u64::from(hi))) as u32
}

fn round_down(usd: f64, tick: f64) -> f64 {
    (usd / tick).floor() * tick
}

fn round_up(usd: f64, tick: f64) -> f64 {
    (usd / tick).ceil() * tick
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::{
        actors::takers::tests::NotingSeat,
        making::{Competition, Making},
    };

    #[tokio::test]
    async fn it_quotes_around_the_level_when_due() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let making = Making {
            competitor_bps: 20.0,
            competition: Competition {
                floor_bps: 5.0,
                react_slots: 4,
                size: 2.0,
                reactive: None,
            },
            takers: Vec::new(),
        };
        let seat = Box::new(NotingSeat {
            market: "x".into(),
            fail_over: f64::MAX,
            log: log.clone(),
        });
        let mut maker = Competitor::new(seat, &making, [0; 32]);
        let at = |at, level_usd| Moment {
            at,
            level_usd,
            book_usd: level_usd,
        };
        maker.after_blocks(&at(2, 100.0)).await;
        // Not due again until 4 slots on.
        maker.after_blocks(&at(4, 101.0)).await;
        maker.after_blocks(&at(6, 100.0)).await;
        let log = log.lock().unwrap().clone();
        assert_eq!(log.len(), 3, "the same quote isn't sent again: {log:?}");
        assert_eq!(log[0], "x cancel");
        let price = |line: &str| line.rsplit(' ').next().unwrap().parse::<f64>().unwrap();
        assert!(log[1].starts_with("x rest Bid 2 at") && (price(&log[1]) - 99.8).abs() < 0.011);
        assert!(log[2].starts_with("x rest Ask 2 at") && (price(&log[2]) - 100.2).abs() < 0.011);
        let mut report = ActorsReport::default();
        maker.report(&mut report).await;
        assert_eq!(report.competitor.unwrap().requotes, 1);
    }

    /// A book the competitor and a test rival share, in whole ticks: orders
    /// are `(ticks, size)`, and the seat's own rests go in beside the
    /// rival's.
    #[derive(Default)]
    struct Book {
        rival_bids: Vec<(i64, f64)>,
        rival_asks: Vec<(i64, f64)>,
        own: Vec<(Side, i64, f64)>,
        position: f64,
    }

    struct BookSeat {
        tick: f64,
        book: Arc<Mutex<Book>>,
    }

    impl BookSeat {
        fn levels(&self, side: Side, min: f64) -> Option<f64> {
            let book = self.book.lock().unwrap();
            let rival = if side == Side::Bid {
                &book.rival_bids
            } else {
                &book.rival_asks
            };
            let mut sized: std::collections::BTreeMap<i64, f64> = Default::default();
            for (ticks, size) in rival {
                *sized.entry(*ticks).or_default() += size;
            }
            for (s, ticks, size) in &book.own {
                if *s == side {
                    *sized.entry(*ticks).or_default() += size;
                }
            }
            let sized = sized.into_iter().filter(|(_, size)| *size >= min);
            let best = if side == Side::Bid {
                sized.map(|(t, _)| t).max()
            } else {
                sized.map(|(t, _)| t).min()
            };
            best.map(|t| t as f64 * self.tick)
        }
    }

    #[async_trait]
    impl Seat for BookSeat {
        fn market(&self) -> &str {
            "book"
        }
        fn executes(&self, _: &mtx_kit::Action) -> bool {
            true
        }
        async fn send(&self, _: &mtx_kit::Planned) -> mtx_kit::Sent {
            unimplemented!()
        }
        async fn take(&self, _: Side, _: f64, _: f64) -> crate::venue::SeatResult {
            Ok(())
        }
        async fn rest(&self, side: Side, price: f64, size: f64) -> crate::venue::SeatResult {
            let ticks = (price / self.tick).round() as i64;
            assert!(
                (price / self.tick - ticks as f64).abs() < 1e-6,
                "{price} is off the {} grid",
                self.tick
            );
            self.book.lock().unwrap().own.push((side, ticks, size));
            Ok(())
        }
        async fn cancel_all(&self) -> crate::venue::SeatResult {
            self.book.lock().unwrap().own.clear();
            Ok(())
        }
        async fn top(&self) -> crate::venue::SeatResult<(Option<f64>, Option<f64>)> {
            self.top_sized(0.0).await
        }
        async fn top_sized(
            &self,
            min_size: f64,
        ) -> crate::venue::SeatResult<(Option<f64>, Option<f64>)> {
            Ok((
                self.levels(Side::Bid, min_size),
                self.levels(Side::Ask, min_size),
            ))
        }
        async fn position(&self) -> crate::venue::SeatResult<f64> {
            Ok(self.book.lock().unwrap().position)
        }
        fn tick(&self) -> f64 {
            self.tick
        }
    }

    struct Rig {
        maker: Competitor,
        book: Arc<Mutex<Book>>,
    }

    fn making(reactive: Option<Reactive>) -> Making {
        Making {
            competitor_bps: 15.0,
            competition: Competition {
                floor_bps: 4.0,
                react_slots: 4,
                size: 2.0,
                reactive,
            },
            takers: Vec::new(),
        }
    }

    fn rig(tick: f64, reactive: Reactive, seed: u8) -> Rig {
        let book = Arc::new(Mutex::new(Book::default()));
        let seat = Box::new(BookSeat {
            tick,
            book: book.clone(),
        });
        Rig {
            maker: Competitor::new(seat, &making(Some(reactive)), [seed; 32]),
            book,
        }
    }

    impl Rig {
        async fn at(&mut self, at: u64, level_usd: f64) {
            self.maker
                .after_blocks(&Moment {
                    at,
                    level_usd,
                    book_usd: level_usd,
                })
                .await;
        }

        /// Its resting bid and ask, in ticks.
        fn quotes(&self) -> (i64, i64) {
            let book = self.book.lock().unwrap();
            let own = |side| {
                book.own
                    .iter()
                    .find(|(s, ..)| *s == side)
                    .map(|(_, t, _)| *t)
                    .expect("it quotes")
            };
            (own(Side::Bid), own(Side::Ask))
        }
    }

    fn fixed_steps(ticks: u32) -> Reactive {
        Reactive {
            jitter: 0.0,
            step_ticks: (ticks, ticks),
            ..Reactive::default()
        }
    }

    /// The five equity perps' tick (USD per base unit, from the snapshot's
    /// exchange metadata: 10 quote lots per base lot at 6 and 3 decimals)
    /// and a price range each trades in, with two finer ticks as well.
    const MARKETS: [(&str, f64, f64, f64); 7] = [
        ("SPCX", 0.01, 20.0, 400.0),
        ("NVDA", 0.01, 80.0, 260.0),
        ("TSLA", 0.01, 150.0, 650.0),
        ("SPY", 0.01, 400.0, 800.0),
        ("QQQ", 0.01, 350.0, 750.0),
        ("fine", 0.001, 1.0, 90.0),
        ("coarse", 0.05, 100.0, 900.0),
    ];

    #[test]
    fn whole_tick_rounding_never_drifts_on_any_markets_tick() {
        for (symbol, tick, lo, hi) in MARKETS {
            let (mut legacy_drift, mut drift, mut n) = (0, 0, 0);
            // Every tick-aligned price in the range, as the float the book
            // reports it: the level a quote's width is measured from.
            let first = (lo / tick).round() as i64;
            let last = (hi / tick).round() as i64;
            for k in (first..=last).step_by(7) {
                let price = k as f64 * tick;
                n += 1;
                // The exact tick a price on the grid is on, up and down.
                if ceil_ticks(price / tick) != k || floor_ticks(price / tick) != k {
                    drift += 1;
                }
                if (price / tick).ceil() as i64 != k {
                    legacy_drift += 1;
                }
                assert_eq!(to_ticks(from_ticks(k, tick), tick), k, "{symbol} {k}");
                let sent = from_ticks(k, tick).to_string();
                assert!(
                    sent.split('.').nth(1).unwrap_or("").len() <= 9,
                    "{symbol}: {sent} carries float noise"
                );
            }
            assert_eq!(drift, 0, "{symbol}: {drift} of {n} prices drift");
            eprintln!("{symbol} tick {tick}: {n} prices, whole-tick drift {drift}, float drift {legacy_drift}");
        }
    }

    #[tokio::test]
    async fn its_quotes_never_drift_a_tick_across_levels() {
        for (symbol, tick, lo, hi) in MARKETS {
            let mut maker = rig(tick, fixed_steps(1), 1);
            let mut at = 0;
            let steps = 400;
            for i in 0..steps {
                // Levels that are not on the grid, and some that are.
                let level = lo + (hi - lo) * f64::from(i) / f64::from(steps);
                let level = if i % 3 == 0 {
                    (level / tick).round() * tick
                } else {
                    level
                };
                at += 16;
                maker.at(at, level).await;
                let (bid, ask) = maker.quotes();
                let level_t = level / tick;
                let width = (level_t * 15.0 / 10_000.0 - TICK_EPS).ceil() as i64;
                // The first look quotes the full width from the level's
                // whole ticks; later looks keep to that or step in.
                assert!(
                    bid <= (level_t + TICK_EPS).floor() as i64 - 1
                        && ask >= (level_t - TICK_EPS).ceil() as i64 + 1,
                    "{symbol}: {bid}/{ask} around {level_t}"
                );
                assert!(
                    (level_t - bid as f64) <= width as f64 + 1.0 + 1e-6
                        && (ask as f64 - level_t) <= width as f64 + 1.0 + 1e-6,
                    "{symbol}: {bid}/{ask} too wide around {level_t}, width {width}"
                );
            }
        }
    }

    #[tokio::test]
    async fn it_steps_inside_a_rival_on_both_sides_and_never_ties() {
        for ticks in [1u32, 2] {
            for (symbol, tick, _, hi) in MARKETS {
                let mut rig = rig(tick, fixed_steps(ticks), 3);
                let level = (hi * 0.9 / tick).round() * tick;
                let level_t = (level / tick).round() as i64;
                rig.at(4, level).await;
                // A rival 9 bp out on both sides, resting well over a lot.
                let off = ((level / tick) * 9.0 / 10_000.0).round() as i64;
                {
                    let mut book = rig.book.lock().unwrap();
                    book.rival_bids.push((level_t - off, 3.0));
                    book.rival_asks.push((level_t + off, 3.0));
                }
                rig.at(8, level).await;
                let (bid, ask) = rig.quotes();
                assert_eq!(bid, level_t - off + i64::from(ticks), "{symbol} bid");
                assert_eq!(ask, level_t + off - i64::from(ticks), "{symbol} ask");
            }
        }
    }

    #[tokio::test]
    async fn it_holds_at_its_floor_when_a_rival_is_inside_it() {
        let mut rig = rig(0.01, fixed_steps(1), 5);
        let level = 400.0;
        let level_t = 40_000;
        rig.at(4, level).await;
        // 2 bp out is 8 ticks; its floor of 4 bp is 16 ticks.
        {
            let mut book = rig.book.lock().unwrap();
            book.rival_bids.push((level_t - 8, 5.0));
            book.rival_asks.push((level_t + 8, 5.0));
        }
        for at in (8..80).step_by(4) {
            rig.at(at, level).await;
            let (bid, ask) = rig.quotes();
            assert_eq!((bid, ask), (level_t - 16, level_t + 16), "at {at}");
        }
    }

    #[tokio::test]
    async fn it_ignores_quotes_under_the_reaction_size() {
        let mut rig = rig(0.01, fixed_steps(1), 7);
        let level = 400.0;
        let level_t = 40_000;
        rig.at(4, level).await;
        let before = rig.quotes();
        {
            let mut book = rig.book.lock().unwrap();
            // Odd lots, 5 bp out: two at one price still sum under the
            // 1-unit reaction size (0.4 + 0.4), and 0.99 is under it alone.
            book.rival_bids.push((level_t - 20, 0.4));
            book.rival_bids.push((level_t - 20, 0.4));
            book.rival_asks.push((level_t + 20, 0.99));
        }
        for at in (8..40).step_by(4) {
            rig.at(at, level).await;
        }
        let mut report = ActorsReport::default();
        rig.maker.report(&mut report).await;
        assert_eq!(report.competitor.unwrap().undercuts, 0);
        let (bid, ask) = rig.quotes();
        assert!(bid <= before.0 && ask >= before.1, "it never moved in");
        // The same quotes at the reaction size are a rival.
        rig.book.lock().unwrap().rival_asks.push((level_t + 20, 0.01));
        rig.at(44, level).await;
        assert_eq!(rig.quotes().1, level_t + 19);
    }

    #[tokio::test]
    async fn jitter_replays_from_the_seed_and_stays_in_range() {
        let run = |seed: u8| async move {
            let reactive = Reactive::default();
            let mut rig = rig(0.01, reactive, seed);
            let level = 400.0;
            let level_t = 40_000;
            let mut trail = Vec::new();
            for at in 0..400u64 {
                // A rival that moves each time it's undercut keeps it
                // stepping, so the draws show in its prices.
                {
                    let mut book = rig.book.lock().unwrap();
                    let wobble = ((at / 4) % 5) as i64;
                    book.rival_bids = vec![(level_t - 60 + wobble, 3.0)];
                    book.rival_asks = vec![(level_t + 60 - wobble, 3.0)];
                }
                rig.at(at, level).await;
                trail.push(rig.book.lock().unwrap().own.clone());
            }
            let mut report = ActorsReport::default();
            rig.maker.report(&mut report).await;
            (trail, report.competitor.unwrap())
        };
        let (a, report_a) = run(9).await;
        let (b, report_b) = run(9).await;
        let (c, report_c) = run(10).await;
        assert_eq!(a, b, "the same seed replays the same");
        assert_eq!(report_a, report_b);
        assert!(a != c || report_a.requotes != report_c.requotes, "another seed differs");
        // Looks fall 2..=6 slots apart (4 ± 50%), and use the range.
        let mut gaps = std::collections::BTreeSet::new();
        let mut last = None;
        let mut previous: Vec<(Side, i64, f64)> = Vec::new();
        for (at, own) in a.iter().enumerate() {
            if *own != previous {
                if let Some(prev) = last {
                    gaps.insert(at - prev);
                }
                last = Some(at);
                previous = own.clone();
            }
        }
        assert!(gaps.len() > 1, "looks aren't on a fixed cadence: {gaps:?}");
    }

    #[test]
    fn a_look_delay_draws_within_the_jitter() {
        let mut rng = SplitMix64(42);
        let mut seen = std::collections::BTreeSet::new();
        for _ in 0..500 {
            let delay = (4.0 * (1.0 + 0.5 * (2.0 * rng.next_f64() - 1.0))).round().max(1.0) as u64;
            assert!((2..=6).contains(&delay));
            seen.insert(delay);
        }
        assert_eq!(seen.len(), 5, "{seen:?}");
    }

    #[tokio::test]
    async fn one_sided_fills_widen_the_hit_side_then_ease_back() {
        let reactive = Reactive {
            widen_slots: (20, 20),
            ..fixed_steps(1)
        };
        let mut rig = rig(0.01, reactive, 11);
        let level = 400.0;
        let level_t = 40_000;
        let wide = 60; // 15 bp of 40,000 ticks
        rig.at(0, level).await;
        // Its first quote is the full width; nobody rivals it, so it eases
        // nowhere and stays there. Bring a rival in so it steps tight first.
        {
            let mut book = rig.book.lock().unwrap();
            book.rival_bids.push((level_t - 30, 3.0));
            book.rival_asks.push((level_t + 30, 3.0));
        }
        for at in (4..24).step_by(4) {
            rig.at(at, level).await;
        }
        assert_eq!(rig.quotes(), (level_t - 29, level_t + 29), "tight");
        // Its bid is hit for 5 units (2.5 times its quote size) in a step.
        rig.book.lock().unwrap().position = 5.0;
        rig.at(25, level).await;
        let (bid, ask) = rig.quotes();
        let skew = (5.0_f64 * level_t as f64 / 10_000.0).round() as i64;
        assert_eq!(bid, level_t - wide - skew, "the hit side is at its full width");
        assert!(ask < level_t + wide - skew, "the other side stays tight: {ask}");
        let mut report = ActorsReport::default();
        rig.maker.report(&mut report).await;
        assert_eq!(report.competitor.unwrap().widenings, 1);
        // It stays wide for the drawn 20 slots, then steps back inside.
        rig.at(40, level).await;
        assert_eq!(rig.quotes().0, level_t - wide - skew);
        rig.book.lock().unwrap().position = 0.0;
        for at in 45..80 {
            rig.at(at, level).await;
        }
        assert!(
            rig.quotes().0 > level_t - 40,
            "back inside the rival's width: {:?}",
            rig.quotes()
        );
    }

    #[tokio::test]
    async fn fills_on_both_sides_are_not_toxic() {
        let mut rig = rig(0.01, fixed_steps(1), 13);
        rig.at(0, 400.0).await;
        for (at, position) in [(1, 3.0), (2, 0.0), (3, 3.0), (4, 0.0)] {
            rig.book.lock().unwrap().position = position;
            rig.at(at, 400.0).await;
        }
        let mut report = ActorsReport::default();
        rig.maker.report(&mut report).await;
        assert_eq!(report.competitor.unwrap().widenings, 0);
    }

    #[test]
    fn the_default_competition_is_the_old_one_on_the_wire() {
        let old = r#"{"floor_bps":3.97,"react_slots":2,"size":4.98}"#;
        let parsed: Competition = serde_json::from_str(old).unwrap();
        assert_eq!(parsed.reactive, None);
        assert_eq!(serde_json::to_string(&parsed).unwrap(), old);
        let on: Competition = serde_json::from_str(
            r#"{"floor_bps":3.97,"react_slots":2,"size":4.98,"reactive":{}}"#,
        )
        .unwrap();
        assert_eq!(on.reactive, Some(Reactive::default()));
        assert!(
            serde_json::from_str::<Competition>(
                r#"{"floor_bps":3.97,"react_slots":2,"size":4.98,"reactive":{"nope":1}}"#
            )
            .is_err()
        );
    }
}
