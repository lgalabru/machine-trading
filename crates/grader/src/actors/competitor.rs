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

use async_trait::async_trait;
use mtx_kit::Side;
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use super::{Actor, ActorsReport, Moment};
use crate::{
    making::{Competition, Making},
    venue::Seat,
};

/// How far its quotes lean against what it holds, in basis points per base
/// unit, and the inventory at which it stops quoting the side that adds.
const SKEW_BPS_PER_UNIT: f64 = 1.0;
const MAX_INVENTORY: f64 = 25.0;

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
}

impl Competitor {
    /// The maker quoting from `seat` as `making` says, before its first
    /// quote.
    pub fn new(seat: Box<dyn Seat>, making: &Making) -> Self {
        Self {
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
            },
            takers: Vec::new(),
        };
        let seat = Box::new(NotingSeat {
            market: "x".into(),
            fail_over: f64::MAX,
            log: log.clone(),
        });
        let mut maker = Competitor::new(seat, &making);
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
}
