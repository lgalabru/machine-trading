//! A spot scenario's tape: the taker orders it lists, sent at the start of
//! each step from seats of their own, as anyone trading the market would.
//!
//! Each order goes out once its slot is due, from the seat its `account`
//! names on its `venue` (the session's first without one), at no worse than
//! `limit_bps` through the session's reference. A perp scenario's tape is
//! the takers' (see [`crate::flow`]).

use std::sync::Arc;

use futures::future::BoxFuture;
use mtx_core::Side;
use serde::Serialize;
use tracing::warn;

use super::{Actor, ActorsReport, Moment, StepFlow, seats::Seat};
use crate::making::{TakerOrder, TakerSide};

/// What the tape did in a session.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct TapeReport {
    /// Orders the scenario lists, and how many were sent and failed.
    pub orders: usize,
    pub sent: usize,
    pub failed: usize,
    /// Base units sent.
    pub volume: f64,
}

/// The tape at run time.
pub struct Tape {
    orders: Vec<TakerOrder>,
    first_venue: String,
    next: usize,
    seats: Vec<((u8, String), Arc<Seat>)>,
    report: TapeReport,
}

impl Tape {
    fn venue(order: &TakerOrder, venues: &[String]) -> String {
        order.venue.map_or_else(
            || venues.first().cloned().unwrap_or_default(),
            |v| v.name().to_string(),
        )
    }

    /// The seats `orders` send from, each (account, venue) once, in the
    /// order of the accounts.
    pub fn seats_for(orders: &[TakerOrder], venues: &[String]) -> Vec<(u8, String)> {
        let mut seats: Vec<(u8, String)> = orders
            .iter()
            .map(|o| (o.account, Self::venue(o, venues)))
            .collect();
        seats.sort_by_key(|(account, venue)| (*account, venues.iter().position(|v| v == venue)));
        seats.dedup();
        seats
    }

    pub fn new(
        orders: Vec<TakerOrder>,
        venues: &[String],
        seats: Vec<((u8, String), Arc<Seat>)>,
    ) -> Self {
        Self {
            report: TapeReport {
                orders: orders.len(),
                ..TapeReport::default()
            },
            orders,
            first_venue: venues.first().cloned().unwrap_or_default(),
            next: 0,
            seats,
        }
    }
}

impl Actor for Tape {
    fn before_flow<'a>(&'a mut self, now: &'a Moment) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            while let Some(order) = self.orders.get(self.next).filter(|o| o.slot <= now.at) {
                self.next += 1;
                let venue = order
                    .venue
                    .map_or(self.first_venue.clone(), |v| v.name().to_string());
                let reach = order.limit_bps / 1e4;
                let (side, limit) = match order.side {
                    TakerSide::Buy => (Side::Buy, now.reference_usd * (1.0 + reach)),
                    TakerSide::Sell => (Side::Sell, now.reference_usd * (1.0 - reach)),
                };
                let Some((_, seat)) = self
                    .seats
                    .iter()
                    .find(|((a, v), _)| *a == order.account && *v == venue)
                else {
                    continue;
                };
                self.report.sent += 1;
                self.report.volume += order.size;
                if let Err(error) = seat.take(side, order.size, limit).await {
                    warn!(venue, %error, "a tape order failed");
                    self.report.failed += 1;
                }
            }
        })
    }

    fn after_blocks<'a>(&'a mut self, _now: &'a Moment, _flow: &'a StepFlow) -> BoxFuture<'a, ()> {
        Box::pin(async {})
    }

    fn report<'a>(&'a self, report: &'a mut ActorsReport) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            report.actions.took += self.report.sent;
            report.actions.failed += self.report.failed;
            report.tape = Some(self.report.clone());
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::making::TakerVenue;

    #[test]
    fn each_account_gets_a_seat_on_each_venue_it_trades() {
        let order = |account, venue| TakerOrder {
            slot: 0,
            side: TakerSide::Buy,
            size: 1.0,
            limit_bps: 50.0,
            account,
            venue,
        };
        let venues = ["phoenix".to_string(), "openbook".to_string()];
        let orders = [
            order(2, None),
            order(0, Some(TakerVenue::Openbook)),
            order(2, Some(TakerVenue::Phoenix)),
            order(0, None),
        ];
        assert_eq!(
            Tape::seats_for(&orders, &venues),
            [
                (0, "phoenix".to_string()),
                (0, "openbook".to_string()),
                (2, "phoenix".to_string())
            ]
        );
    }
}
