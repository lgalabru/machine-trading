//! A maker that quotes off the book, not the oracle.
//!
//! On mainnet the Phoenix book leads and the oracle follows: the gap
//! between the book's mid and the oracle predicts the oracle's next move
//! (correlation 0.74 four seconds ahead) and says nothing of the mid's, and
//! the spread is one tick nearly always. A maker quoting around the oracle
//! quotes around a price a few seconds old, 5 bps from the book on average,
//! and its "one tick inside" crosses the book: over half the oracle maker's
//! paper fills took liquidity, and its markouts were -2 to -3 bps.
//!
//! So this maker:
//!
//! - **anchors on the book**: it joins the best bid and ask, and steps one
//!   tick inside only where the spread leaves room without crossing;
//! - **never takes**: every quote is post-only, so a quote the book moved
//!   through is refused rather than filled as a taker;
//! - **skews against inventory in ticks**: holding long, its bid steps back
//!   and its ask stays at the touch, so fills shed the position;
//! - **steps back from a moving book**: when the mid ran more than
//!   [`MOMENTUM_BPS`] over the last few steps, the side the move runs into
//!   (the ask in a rally) steps back, where a stale quote is picked off;
//! - **steps back when the oracle is far**: a gap past [`BASIS_WIDE_BPS`]
//!   is a fast market the book is still finding; both sides step back.
//!
//! It requotes only when a target price changes, or every
//! [`REQUOTE_SLOTS`], to keep its transactions few.

use std::collections::VecDeque;

use mtx_kit::{log, prelude::*, venue::PhoenixVenue};

/// The quote's size, as a share of the position cap: two fills the same
/// way reach it.
const QUOTE_SHARE_OF_CAP: f64 = 0.5;

/// How far the inventory skew steps the adding side back, in ticks, at
/// the cap: a tick per fifth of the cap.
const SKEW_TICKS_AT_CAP: f64 = 5.0;

/// Past this share of the cap it stops quoting the side that adds.
const INVENTORY_LIMIT: f64 = 0.75;

/// The momentum window, in steps (2 slots each: about 2.4 s), and the move
/// in it past which the side it runs into steps back. 3 bps is two ticks
/// on SOL at $66: the book walked through the touch twice.
const MOMENTUM_STEPS: usize = 3;
const MOMENTUM_BPS: f64 = 3.0;
const MOMENTUM_TICKS: i64 = 2;

/// The oracle gap past which both sides step back: the live p95 is 11 bps,
/// so this is a market moving faster than the oracle keeps up with.
const BASIS_WIDE_BPS: f64 = 12.0;
const BASIS_WIDE_TICKS: i64 = 1;

/// Requote at least this often even if nothing moved, so a quote refused
/// as crossing comes back.
const REQUOTE_SLOTS: u64 = 20;

/// A quote under this size isn't worth a transaction: Phoenix's SOL base
/// lot is 0.001.
const SIZE_MIN: f64 = 0.001;

/// The strategy.
#[derive(Default)]
pub struct BookMaker {
    cap: f64,
    tick: f64,
    mids: VecDeque<f64>,
    quoted: Option<Quoted>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct Quoted {
    slot: u64,
    bid: Option<i64>,
    ask: Option<i64>,
}

/// The book, the oracle and the position at a step, in the units the
/// quoting works in.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct View {
    pub best_bid: f64,
    pub best_ask: f64,
    pub oracle: f64,
    /// Position over the cap, in [-1, 1].
    pub inventory: f64,
    /// The mid's move over the momentum window, in bps.
    pub momentum_bps: f64,
}

/// Where to quote, in ticks: `None` for a side not quoted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Targets {
    pub bid: Option<i64>,
    pub ask: Option<i64>,
}

impl Strategy for BookMaker {
    fn start(&mut self, session: &Session) -> BotResult {
        self.cap = session.mandate.universe[0].max_position;
        let venue = PhoenixVenue::from_json(&session.venue)?;
        let market = venue.market(&session.symbol)?;
        self.tick = venue.ticks_to_usd(market, 1);
        if !(self.tick > 0.0 && self.cap > 0.0) {
            return Err(format!("a tick of {} and a cap of {}", self.tick, self.cap).into());
        }
        log(&format!(
            "quoting off the book: tick {}, cap {}, quote size {}",
            self.tick,
            self.cap,
            self.cap * QUOTE_SHARE_OF_CAP
        ));
        Ok(())
    }

    fn step(&mut self, market: &Market, orders: &mut Orders) -> BotResult {
        let (Some(best_bid), Some(best_ask)) = (market.best_bid, market.best_ask) else {
            return Ok(());
        };
        if !(best_bid > 0.0 && best_ask > best_bid) {
            return Ok(());
        }
        let mid = (best_bid + best_ask) / 2.0;
        self.mids.push_back(mid);
        while self.mids.len() > MOMENTUM_STEPS + 1 {
            self.mids.pop_front();
        }
        let momentum_bps = match self.mids.front() {
            Some(first) => (mid / first - 1.0) * 10_000.0,
            None => 0.0,
        };
        let held = market.position.size;
        let view = View {
            best_bid,
            best_ask,
            oracle: market.oracle,
            inventory: (held / self.cap).clamp(-1.0, 1.0),
            momentum_bps,
        };
        let targets = targets(&view, self.tick);
        let stale = self
            .quoted
            .is_none_or(|q| market.slot >= q.slot + REQUOTE_SLOTS);
        let moved = self
            .quoted
            .is_none_or(|q| q.bid != targets.bid || q.ask != targets.ask);
        if !(moved || stale) {
            return Ok(());
        }
        orders.cancel_all();
        let size = self.cap * QUOTE_SHARE_OF_CAP;
        if let Some(bid) = targets.bid {
            let size = size.min(self.cap - held);
            if size >= SIZE_MIN {
                orders.bid(bid as f64 * self.tick, size).post_only();
            }
        }
        if let Some(ask) = targets.ask {
            let size = size.min(self.cap + held);
            if size >= SIZE_MIN {
                orders.ask(ask as f64 * self.tick, size).post_only();
            }
        }
        self.quoted = Some(Quoted {
            slot: market.slot,
            bid: targets.bid,
            ask: targets.ask,
        });
        Ok(())
    }
}

/// The quotes for a view, in ticks: at the touch, a tick inside where the
/// spread has room, stepped back for inventory, momentum and a wide oracle
/// gap, and never crossing.
pub fn targets(view: &View, tick: f64) -> Targets {
    assert!(tick > 0.0, "a tick has a size");
    assert!(view.best_ask > view.best_bid, "a book that isn't crossed");
    let best_bid = (view.best_bid / tick).round() as i64;
    let best_ask = (view.best_ask / tick).round() as i64;
    // A tick inside each side only if the two can't meet.
    let room = best_ask - best_bid >= 3;
    let mut bid = best_bid + i64::from(room);
    let mut ask = best_ask - i64::from(room);

    let skew = (view.inventory.abs() * SKEW_TICKS_AT_CAP).round() as i64;
    if view.inventory > 0.0 {
        bid -= skew;
    } else {
        ask += skew;
    }
    if view.momentum_bps > MOMENTUM_BPS {
        ask += MOMENTUM_TICKS;
    } else if view.momentum_bps < -MOMENTUM_BPS {
        bid -= MOMENTUM_TICKS;
    }
    let basis_bps = ((best_bid + best_ask) as f64 * tick / 2.0 / view.oracle - 1.0) * 10_000.0;
    if view.oracle > 0.0 && basis_bps.abs() > BASIS_WIDE_BPS {
        bid -= BASIS_WIDE_TICKS;
        ask += BASIS_WIDE_TICKS;
    }
    // Post-only refuses a crossing quote anyway; keep it off the book's far
    // side so it isn't refused.
    bid = bid.min(best_ask - 1);
    ask = ask.max(best_bid + 1);
    let targets = Targets {
        bid: (view.inventory < INVENTORY_LIMIT).then_some(bid),
        ask: (view.inventory > -INVENTORY_LIMIT).then_some(ask),
    };
    if let (Some(bid), Some(ask)) = (targets.bid, targets.ask) {
        assert!(bid < ask, "the quotes don't cross: {bid} {ask}");
    }
    targets
}

#[cfg(target_arch = "wasm32")]
mtx_kit::export!(BookMaker);

#[cfg(test)]
mod tests {
    use super::*;

    const TICK: f64 = 0.01;

    fn view(bid: f64, ask: f64) -> View {
        View {
            best_bid: bid,
            best_ask: ask,
            oracle: (bid + ask) / 2.0,
            inventory: 0.0,
            momentum_bps: 0.0,
        }
    }

    #[test]
    fn a_one_tick_book_is_joined_not_crossed() {
        let t = targets(&view(66.54, 66.55), TICK);
        assert_eq!(
            t,
            Targets {
                bid: Some(6654),
                ask: Some(6655)
            }
        );
    }

    #[test]
    fn a_wide_book_is_bettered_by_a_tick_each_side() {
        let t = targets(&view(66.50, 66.55), TICK);
        assert_eq!(
            t,
            Targets {
                bid: Some(6651),
                ask: Some(6654)
            }
        );
        // Two ticks wide: bettering both would meet, so it joins.
        let t = targets(&view(66.50, 66.52), TICK);
        assert_eq!(
            t,
            Targets {
                bid: Some(6650),
                ask: Some(6652)
            }
        );
    }

    #[test]
    fn the_oracle_does_not_move_the_quotes_until_it_is_far() {
        // 7 bps under the book, as the live market usually is: no change.
        let near = View {
            oracle: 66.50,
            ..view(66.54, 66.55)
        };
        assert_eq!(targets(&near, TICK), targets(&view(66.54, 66.55), TICK));
        // 15 bps: both sides step back.
        let far = View {
            oracle: 66.445,
            ..view(66.54, 66.55)
        };
        assert_eq!(
            targets(&far, TICK),
            Targets {
                bid: Some(6653),
                ask: Some(6656)
            }
        );
    }

    #[test]
    fn inventory_steps_the_adding_side_back_and_stops_it_near_the_cap() {
        let long = View {
            inventory: 0.4,
            ..view(66.54, 66.55)
        };
        assert_eq!(
            targets(&long, TICK),
            Targets {
                bid: Some(6652),
                ask: Some(6655)
            }
        );
        let short = View {
            inventory: -0.4,
            ..view(66.54, 66.55)
        };
        assert_eq!(
            targets(&short, TICK),
            Targets {
                bid: Some(6654),
                ask: Some(6657)
            }
        );
        let full = View {
            inventory: 0.8,
            ..view(66.54, 66.55)
        };
        assert_eq!(targets(&full, TICK).bid, None);
        assert_eq!(targets(&full, TICK).ask, Some(6655));
    }

    #[test]
    fn a_rally_steps_the_ask_back_and_a_slide_the_bid() {
        let rally = View {
            momentum_bps: 4.0,
            ..view(66.54, 66.55)
        };
        assert_eq!(
            targets(&rally, TICK),
            Targets {
                bid: Some(6654),
                ask: Some(6657)
            }
        );
        let slide = View {
            momentum_bps: -4.0,
            ..view(66.54, 66.55)
        };
        assert_eq!(
            targets(&slide, TICK),
            Targets {
                bid: Some(6652),
                ask: Some(6655)
            }
        );
    }

    #[test]
    fn no_combination_of_steps_crosses_the_book() {
        for inventory in [-0.7, -0.3, 0.0, 0.3, 0.7] {
            for momentum_bps in [-10.0, 0.0, 10.0] {
                for oracle in [60.0, 66.545, 70.0] {
                    let v = View {
                        inventory,
                        momentum_bps,
                        oracle,
                        ..view(66.54, 66.55)
                    };
                    let t = targets(&v, TICK);
                    assert!(t.bid.is_none_or(|b| b <= 6654));
                    assert!(t.ask.is_none_or(|a| a >= 6655));
                }
            }
        }
    }
}
