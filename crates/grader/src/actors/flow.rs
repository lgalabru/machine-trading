//! What traded in a step, for the actors that react to it.
//!
//! A step's flow is read the way any trader on the chain can read it: from
//! the bot's position on each market it trades, before and after the step.
//! The session reads it once per step, after the blocks and before the
//! actors act, only when some actor wants it ([`super::Actor::wants_flow`]).
//!
//! An actor that acts on the bot's orders as they arrive reads them from
//! the transactions themselves, as a searcher reading the mempool would: a
//! venue's [`SendReader`] says which orders a send carries on its market.

use std::sync::Arc;

use futures::future::BoxFuture;
use mtx_kit::Side;
use tracing::warn;

use crate::venue::Seat;

/// What someone bought and sold on one market in a step, in base units.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct VenueFlow {
    pub market: String,
    pub bought: f64,
    pub sold: f64,
}

impl VenueFlow {
    /// Bought less sold.
    pub fn net(&self) -> f64 {
        self.bought - self.sold
    }
}

/// What traded in a step: the bot's trading, on each market it trades.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct StepFlow {
    pub bot: Vec<VenueFlow>,
}

impl StepFlow {
    /// The bot's trading on `side`, over every market.
    pub fn bot_traded(&self, side: Side) -> f64 {
        self.bot
            .iter()
            .map(|f| match side {
                Side::Bid => f.bought,
                Side::Ask => f.sold,
            })
            .sum()
    }

    /// The bot's net buying, over every market.
    pub fn bot_net(&self) -> f64 {
        self.bot.iter().map(VenueFlow::net).sum()
    }
}

/// An order on the session's market that one of the bot's transactions
/// carries, as anyone who reads the transaction before it lands sees it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SentOrder {
    pub side: Side,
    /// In base units.
    pub size: f64,
    /// Its limit, where it has one.
    pub price: Option<f64>,
    /// It takes at once or not at all (immediate-or-cancel): a trade, not
    /// a quote.
    pub immediate: bool,
}

/// Reads a `sendTransaction` call of the bot's: the orders it carries on
/// the session's market, or none if the chain wouldn't run it.
pub type SendReader =
    Arc<dyn Fn(serde_json::Value) -> BoxFuture<'static, Vec<SentOrder>> + Send + Sync>;

/// Where a session reads each step's flow: a seat on each of the bot's
/// markets, reading the bot's own account (its position), never trading.
pub struct FlowWatch {
    seats: Vec<Box<dyn Seat>>,
    /// Each seat's position at the last read.
    positions: Vec<f64>,
}

impl FlowWatch {
    /// Watch the bot through `seats`, one per market, which read its
    /// account. Positions count from zero: the bot starts flat.
    pub fn new(seats: Vec<Box<dyn Seat>>) -> Self {
        Self {
            positions: vec![0.0; seats.len()],
            seats,
        }
    }

    /// Count from the positions the bot starts with, one per seat, so the
    /// start's setup fill isn't read as its trading.
    pub fn starting_at(mut self, positions: &[f64]) -> Self {
        for (before, start) in self.positions.iter_mut().zip(positions) {
            *before = *start;
        }
        self
    }

    /// The flow since the last read. A market whose position can't be read
    /// counts as unmoved, and is read again next step.
    pub async fn step(&mut self) -> StepFlow {
        let mut flow = StepFlow::default();
        for (seat, before) in self.seats.iter().zip(self.positions.iter_mut()) {
            let now = match seat.position().await {
                Ok(now) => now,
                Err(error) => {
                    warn!(market = seat.market(), %error, "reading the bot's position for the actors");
                    *before
                }
            };
            let moved = now - *before;
            *before = now;
            flow.bot.push(VenueFlow {
                market: seat.market().to_string(),
                bought: moved.max(0.0),
                sold: (-moved).max(0.0),
            });
        }
        flow
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::venue::SeatResult;

    struct Held(f64);

    #[async_trait::async_trait]
    impl Seat for Held {
        fn market(&self) -> &str {
            "m"
        }
        fn executes(&self, _: &mtx_kit::Action) -> bool {
            true
        }
        async fn send(&self, _: &mtx_kit::Planned) -> mtx_kit::Sent {
            unimplemented!()
        }
        async fn take(&self, _: Side, _: f64, _: f64) -> SeatResult {
            Ok(())
        }
        async fn rest(&self, _: Side, _: f64, _: f64) -> SeatResult {
            Ok(())
        }
        async fn cancel_all(&self) -> SeatResult {
            Ok(())
        }
        async fn top(&self) -> SeatResult<(Option<f64>, Option<f64>)> {
            Ok((None, None))
        }
        async fn position(&self) -> SeatResult<f64> {
            Ok(self.0)
        }
        fn tick(&self) -> f64 {
            0.01
        }
        fn lot(&self) -> f64 {
            0.0
        }
    }

    /// A bot that starts long and doesn't trade has bought nothing: the
    /// watch counts from where the bot starts, not from flat.
    #[tokio::test]
    async fn a_bot_that_starts_long_and_holds_has_not_bought() {
        let mut watch = FlowWatch::new(vec![Box::new(Held(7.0))]).starting_at(&[7.0]);
        assert_eq!(watch.step().await.bot_net(), 0.0);
        let mut flat = FlowWatch::new(vec![Box::new(Held(7.0))]);
        assert_eq!(flat.step().await.bot_net(), 7.0);
    }

    #[test]
    fn the_flow_sums_by_side() {
        let flow = StepFlow {
            bot: vec![
                VenueFlow {
                    market: "a".into(),
                    bought: 1.0,
                    sold: 0.0,
                },
                VenueFlow {
                    market: "b".into(),
                    bought: 0.0,
                    sold: 3.0,
                },
            ],
        };
        assert_eq!(flow.bot_traded(Side::Bid), 1.0);
        assert_eq!(flow.bot_traded(Side::Ask), 3.0);
        assert_eq!(flow.bot_net(), -2.0);
    }
}
