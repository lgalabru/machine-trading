//! A counterparty that trades ahead of the bot.
//!
//! It watches the bot's fills on its `side` and keeps their average per
//! step over the last `window` steps. At the start of each step, before the
//! bot's next turn, it takes `share` of that average on the same side, on
//! the venues the bot last traded on, in the same proportions: a steady
//! seller finds the bids already hit. Once the bot has traded nothing on
//! that side for `stand_down` steps, it stops until the bot trades again.
//! It takes at no worse than `reach_bps` past the makers' quotes.

use std::{collections::VecDeque, sync::Arc};

use futures::future::BoxFuture;
use mtx_core::Side;
use serde::{Deserialize, Serialize};
use tracing::warn;

use super::{Actions, Actor, ActorsReport, Moment, StepFlow, seats::Seat};

/// The kinds of counterparty.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CounterpartyKind {
    /// Trades the bot's way before its next turn.
    Ahead,
}

fn default_window() -> u64 {
    5
}

fn default_reach_bps() -> f64 {
    50.0
}

/// A counterparty's parameters; see the module docs.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Counterparty {
    pub kind: CounterpartyKind,
    /// The side it trades, and of the bot's fills it follows.
    pub side: Side,
    /// The share of the bot's average fills it trades each step.
    pub share: f64,
    /// Steps the average runs over.
    #[serde(default = "default_window")]
    pub window: u64,
    /// Steps without a fill of the bot's on its side before it stops.
    pub stand_down: u64,
    /// How far past the makers' nearest quote it takes, in basis points.
    #[serde(default = "default_reach_bps")]
    pub reach_bps: f64,
}

impl Counterparty {
    pub fn validate(&self) -> std::result::Result<(), String> {
        if !(self.share.is_finite() && self.share > 0.0) {
            return Err("share must be above 0".into());
        }
        if self.window == 0 || self.stand_down == 0 {
            return Err("window and stand_down are at least one step".into());
        }
        if !(self.reach_bps.is_finite() && self.reach_bps >= 0.0) {
            return Err("reach_bps must be a number of at least 0".into());
        }
        Ok(())
    }
}

/// What it has seen of the bot.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Watch {
    /// The bot's fills on the side, per step, newest last.
    fills: VecDeque<f64>,
    /// Steps since the bot last traded on the side.
    quiet: u64,
    /// Where the bot last traded on the side, and how much on each.
    venues: Vec<(String, f64)>,
}

impl Watch {
    /// After a step: the bot's fills on the side, by venue.
    pub fn observe(&mut self, counterparty: &Counterparty, by_venue: Vec<(String, f64)>) {
        let traded: f64 = by_venue.iter().map(|(_, b)| b).sum();
        self.fills.push_back(traded);
        while self.fills.len() as u64 > counterparty.window {
            self.fills.pop_front();
        }
        if traded > 0.0 {
            self.quiet = 0;
            self.venues = by_venue.into_iter().filter(|(_, b)| *b > 0.0).collect();
        } else {
            self.quiet += 1;
        }
    }

    /// What it trades at the start of the next step, by venue; nothing once
    /// it has stood down or before the bot has traded.
    pub fn next(&self, counterparty: &Counterparty) -> Vec<(String, f64)> {
        if self.venues.is_empty() || self.quiet >= counterparty.stand_down {
            return Vec::new();
        }
        let average = self.fills.iter().sum::<f64>() / self.fills.len().max(1) as f64;
        let total: f64 = self.venues.iter().map(|(_, b)| b).sum();
        self.venues
            .iter()
            .map(|(venue, b)| (venue.clone(), counterparty.share * average * b / total))
            .collect()
    }
}

/// One of its takes.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AheadTrade {
    pub at: u64,
    pub venue: String,
    /// What it sent, in base units.
    pub base: f64,
}

/// What one counterparty did in a session.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CounterpartyReport {
    pub kind: CounterpartyKind,
    pub side: Side,
    pub trades: Vec<AheadTrade>,
    /// What it bought and sold, in base units, from the venues' fills.
    pub bought: f64,
    pub sold: f64,
    pub actions: Actions,
}

/// A counterparty at run time: a seat on each venue.
pub struct Ahead {
    counterparty: Counterparty,
    seats: Vec<(usize, Arc<Seat>)>,
    /// The makers' half-spread, as a fraction of the reference.
    gap: f64,
    watch: Watch,
    report: CounterpartyReport,
}

impl Ahead {
    pub fn new(
        counterparty: Counterparty,
        seats: Vec<(usize, Arc<Seat>)>,
        makers_half_spread_bps: f64,
    ) -> Self {
        Self {
            report: CounterpartyReport {
                kind: counterparty.kind,
                side: counterparty.side,
                trades: Vec::new(),
                bought: 0.0,
                sold: 0.0,
                actions: Actions::default(),
            },
            gap: makers_half_spread_bps / 1e4,
            counterparty,
            seats,
            watch: Watch::default(),
        }
    }
}

impl Actor for Ahead {
    fn before_flow<'a>(&'a mut self, now: &'a Moment) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            let side = self.counterparty.side;
            let sign = match side {
                Side::Buy => 1.0,
                Side::Sell => -1.0,
            };
            let reach = self.gap + self.counterparty.reach_bps / 1e4;
            let limit = now.reference_usd * (1.0 + sign * reach);
            for (venue, base) in self.watch.next(&self.counterparty) {
                let Some((_, seat)) = self.seats.iter().find(|(_, s)| s.venue == venue) else {
                    continue;
                };
                self.report.actions.took += 1;
                self.report.trades.push(AheadTrade {
                    at: now.at,
                    venue,
                    base,
                });
                if let Err(error) = seat.take(side, base, limit).await {
                    warn!(venue = seat.venue, %error, "a counterparty's take failed");
                    self.report.actions.failed += 1;
                }
            }
        })
    }

    fn after_blocks<'a>(&'a mut self, _now: &'a Moment, flow: &'a StepFlow) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            let side = self.counterparty.side;
            let by_venue = self
                .seats
                .iter()
                .map(|(_, s)| (s.venue.clone(), flow.bot_on(&s.venue, side)))
                .collect();
            self.watch.observe(&self.counterparty, by_venue);
            for (n, _) in &self.seats {
                if let Some(f) = flow.seats.get(*n) {
                    self.report.bought += f.bought;
                    self.report.sold += f.sold;
                }
            }
        })
    }

    fn report<'a>(&'a self, report: &'a mut ActorsReport) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            let a = self.report.actions;
            report.actions.took += a.took;
            report.actions.failed += a.failed;
            report.counterparties.push(self.report.clone());
        })
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn counterparty() -> Counterparty {
        Counterparty {
            kind: CounterpartyKind::Ahead,
            side: Side::Sell,
            share: 0.5,
            window: 3,
            stand_down: 2,
            reach_bps: 50.0,
        }
    }

    fn on(phoenix: f64, openbook: f64) -> Vec<(String, f64)> {
        vec![("phoenix".into(), phoenix), ("openbook".into(), openbook)]
    }

    #[test]
    fn it_trades_a_share_of_the_bots_average_where_the_bot_traded() {
        let c = counterparty();
        let mut watch = Watch::default();
        assert!(watch.next(&c).is_empty(), "nothing before the bot trades");
        watch.observe(&c, on(3.0, 0.0));
        assert_eq!(watch.next(&c), [("phoenix".to_string(), 1.5)]);
        watch.observe(&c, on(1.0, 2.0));
        // Average 3 a step; split as the bot's last step was.
        assert_eq!(
            watch.next(&c),
            [("phoenix".to_string(), 0.5), ("openbook".to_string(), 1.0)]
        );
        watch.observe(&c, on(6.0, 0.0));
        watch.observe(&c, on(3.0, 0.0));
        // The window keeps the last three steps: 3 + 6 + 3.
        assert_eq!(watch.next(&c), [("phoenix".to_string(), 2.0)]);
    }

    #[test]
    fn it_stands_down_when_the_bot_pauses() {
        let c = counterparty();
        let mut watch = Watch::default();
        watch.observe(&c, on(3.0, 0.0));
        watch.observe(&c, on(0.0, 0.0));
        assert_eq!(watch.next(&c), [("phoenix".to_string(), 0.75)]);
        watch.observe(&c, on(0.0, 0.0));
        assert!(watch.next(&c).is_empty());
        watch.observe(&c, on(0.0, 3.0));
        assert_eq!(watch.next(&c), [("openbook".to_string(), 0.5)]);
    }

    #[test]
    fn the_parameters_are_checked() {
        assert_eq!(counterparty().validate(), Ok(()));
        let mut bad = counterparty();
        bad.stand_down = 0;
        assert!(bad.validate().is_err());
        let parsed: Counterparty =
            serde_json::from_str(r#"{"kind":"ahead","side":"buy","share":1,"stand_down":3}"#)
                .unwrap();
        assert_eq!(parsed.window, 5);
        assert_eq!(parsed.side, Side::Buy);
    }
}
