//! The grader's takers: the taker orders a scenario lists (a market-making
//! session's flow, see [`crate::making`], an off-hours session's, a tape),
//! on any market.
//!
//! Before the cheatcodes lock, the session funds a pool of taker accounts,
//! one seat each (on a perp, a Phoenix account with collateral; on a spot
//! book, a wallet). While the bot runs, at the start of each step they send
//! the orders due by the step before as ordinary market orders, through
//! the grader's own RPC: the same transaction any trader sends, filled
//! against whatever quotes are best. They go one at a time, in the
//! scenario's order, between the bot's turns, so the same session sends the
//! same flow into the same book every time.

use async_trait::async_trait;
use mtx_kit::Side;
use serde::Serialize;
use tracing::warn;

use super::{Actor, ActorsReport, Moment};
use crate::{
    making::{TakerOrder, TakerSide},
    venue::Seat,
};

/// What the grader's takers did in a session.
#[derive(Debug, Clone, Default, Serialize, PartialEq)]
pub struct FlowReport {
    /// Orders the scenario lists, and how many of them were sent and failed.
    pub orders: usize,
    pub sent: usize,
    pub failed: usize,
    /// Taker volume sent, in base units.
    pub volume: f64,
    /// Ticks the makers were pushed out by, to the scenario's half-spread.
    pub makers_widened_ticks: u64,
}

/// One taker order as the grader sent it.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct TakerRecord {
    /// Slots into the session it was sent at, and the slot the scenario
    /// lists it for.
    pub at: u64,
    pub slot: u64,
    pub side: TakerSide,
    pub size: f64,
    /// Its worst price, in USD.
    pub limit_usd: f64,
    pub account: u8,
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// What the takers' limits are reached from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pricing {
    /// The scenario's level: a flow trading around the market's value.
    Level,
    /// The book's level, where its premium puts it: a tape.
    Book,
}

/// The takers through a session: the orders still to send, and what they
/// sent.
pub struct Takers {
    /// One seat per taker account, by account number.
    seats: Vec<Box<dyn Seat>>,
    orders: Vec<TakerOrder>,
    pricing: Pricing,
    next: usize,
    report: FlowReport,
    sent: Vec<TakerRecord>,
}

impl Takers {
    /// Takers sending `orders` from `seats` (order `account` n from seat
    /// n), priced off `pricing`. `widened_ticks` is how far setup pushed
    /// the venue's makers out, for the report.
    pub fn new(
        seats: Vec<Box<dyn Seat>>,
        orders: Vec<TakerOrder>,
        pricing: Pricing,
        widened_ticks: u64,
    ) -> Self {
        Self {
            seats,
            report: FlowReport {
                orders: orders.len(),
                makers_widened_ticks: widened_ticks,
                ..FlowReport::default()
            },
            orders,
            pricing,
            next: 0,
            sent: Vec::new(),
        }
    }

    /// Send, in order, every order due by `at` slots into the session,
    /// priced off `level` (USD).
    async fn send(&mut self, level: f64, at: u64) {
        while let Some(order) = self.orders.get(self.next).filter(|o| o.slot <= at) {
            let order = *order;
            self.next += 1;
            // The order's own limit: it fills only against quotes at least
            // this close to the level, or not at all.
            let reach = order.limit_bps / 10_000.0;
            let (side, limit) = match order.side {
                TakerSide::Buy => (Side::Bid, level * (1.0 + reach)),
                TakerSide::Sell => (Side::Ask, level * (1.0 - reach)),
            };
            self.report.sent += 1;
            self.report.volume += order.size;
            let limit_usd = (limit * 100.0).round() / 100.0;
            let sent = match self.seats.get(order.account as usize) {
                Some(seat) => seat.take(side, order.size, limit_usd).await,
                None => Err(format!("no seat for taker account {}", order.account)),
            };
            let error = match sent {
                Ok(()) => None,
                Err(error) => {
                    self.report.failed += 1;
                    warn!("a taker order failed: {error}");
                    Some(error)
                }
            };
            self.sent.push(TakerRecord {
                at,
                slot: order.slot,
                side: order.side,
                size: order.size,
                limit_usd,
                account: order.account,
                ok: error.is_none(),
                error,
            });
        }
    }

    fn price(&self, moment: &Moment) -> f64 {
        match self.pricing {
            Pricing::Level => moment.level_usd,
            Pricing::Book => moment.book_usd,
        }
    }
}

#[async_trait]
impl Actor for Takers {
    async fn before_blocks(&mut self, last: &Moment) {
        self.send(self.price(last), last.at).await;
    }

    async fn end(&mut self, last: &Moment) {
        self.send(self.price(last), last.at).await;
    }

    async fn report(&mut self, report: &mut ActorsReport) {
        report.flow = Some(self.report.clone());
        report.taker_orders = std::mem::take(&mut self.sent);
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::venue::SeatResult;

    /// A seat that notes its takes and fails the ones over `fail_over`.
    pub(crate) struct NotingSeat {
        pub market: String,
        pub fail_over: f64,
        pub log: Arc<Mutex<Vec<String>>>,
    }

    #[async_trait]
    impl Seat for NotingSeat {
        fn market(&self) -> &str {
            &self.market
        }
        fn executes(&self, _: &mtx_kit::Action) -> bool {
            true
        }
        async fn send(&self, _: &mtx_kit::Planned) -> mtx_kit::Sent {
            unimplemented!()
        }
        async fn take(&self, side: Side, size: f64, limit: f64) -> SeatResult {
            self.log
                .lock()
                .unwrap()
                .push(format!("{} {side:?} {size} at {limit}", self.market));
            if size > self.fail_over {
                return Err("too big".into());
            }
            Ok(())
        }
        async fn rest(&self, side: Side, price: f64, size: f64) -> SeatResult {
            self.log
                .lock()
                .unwrap()
                .push(format!("{} rest {side:?} {size} at {price}", self.market));
            Ok(())
        }
        async fn cancel_all(&self) -> SeatResult {
            self.log
                .lock()
                .unwrap()
                .push(format!("{} cancel", self.market));
            Ok(())
        }
        async fn top(&self) -> SeatResult<(Option<f64>, Option<f64>)> {
            Ok((None, None))
        }
        async fn position(&self) -> SeatResult<f64> {
            Ok(0.0)
        }
        fn tick(&self) -> f64 {
            0.01
        }
    }

    fn order(slot: u64, side: TakerSide, size: f64, account: u8) -> TakerOrder {
        TakerOrder {
            slot,
            side,
            size,
            limit_bps: 100.0,
            account,
        }
    }

    #[tokio::test]
    async fn orders_go_out_when_due_from_their_accounts_seats() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let seat = |market: &str| {
            Box::new(NotingSeat {
                market: market.into(),
                fail_over: 3.0,
                log: log.clone(),
            }) as Box<dyn Seat>
        };
        let mut takers = Takers::new(
            vec![seat("a"), seat("b")],
            vec![
                order(1, TakerSide::Buy, 1.0, 1),
                order(2, TakerSide::Sell, 4.0, 0),
                order(5, TakerSide::Sell, 2.0, 7),
            ],
            Pricing::Book,
            3,
        );
        let at = |at, book_usd| Moment {
            at,
            level_usd: 1.0,
            book_usd,
        };
        takers.before_blocks(&at(0, 100.0)).await;
        assert!(log.lock().unwrap().is_empty());
        takers.before_blocks(&at(2, 100.0)).await;
        takers.end(&at(6, 50.0)).await;
        assert_eq!(
            *log.lock().unwrap(),
            ["b Bid 1 at 101", "a Ask 4 at 99"],
            "each from its account's seat, at its limit off the book"
        );
        let mut report = ActorsReport::default();
        takers.report(&mut report).await;
        let flow = report.flow.unwrap();
        assert_eq!((flow.orders, flow.sent, flow.failed), (3, 3, 2));
        assert_eq!(flow.makers_widened_ticks, 3);
        let errors: Vec<_> = report
            .taker_orders
            .iter()
            .map(|o| o.error.clone())
            .collect();
        assert_eq!(
            errors,
            [
                None,
                Some("too big".to_string()),
                Some("no seat for taker account 7".to_string())
            ]
        );
    }
}
