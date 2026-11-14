//! Actors: the participants the grader runs in a session besides the bot,
//! each trading from accounts of its own through a [`Seat`](crate::venue::Seat)
//! on any [`Market`](crate::venue::Market) of any venue.
//!
//! A session's actors are its [`Cast`]. The session loop (see
//! [`crate::session`]) has them act at fixed points of every step, so the
//! same session plays the same way every time:
//!
//! 1. **Before the blocks** ([`Actor::before_blocks`]), at the start of a
//!    step, against what the bot left after its last turn. The takers trade
//!    here: the orders due by the previous step, sent before any maker can
//!    requote after the move, would otherwise pick off every quote.
//! 2. **After the blocks** ([`Actor::after_blocks`]), once the step's blocks
//!    have landed and before the bot's turn. The grader's own maker looks at
//!    the book and requotes here.
//! 3. **At the end** ([`Actor::end`]), after the bot's last turn.
//!
//! Each acts at a [`Moment`]: the slot, and the scenario's prices there.
//! An actor that reacts to what traded also gets each step's [`StepFlow`]
//! ([`Actor::flow`]) just before it acts after the blocks, and one that acts
//! on the bot's orders as they arrive hands the bot's guard a
//! [`SendHook`] ([`Actor::send_hook`]).
//!
//! Two actors come with the engine: [`takers`], the taker orders a scenario
//! lists, and [`competitor`], a maker that competes with the bot's quotes.
//! Both work on any market whose seats take and rest orders. A scenario's
//! [`library`] adds more: noise and informed traders, spoofers, stop
//! hunters, front-runners, an arbitrageur, RFQ clients and a competing
//! liquidator, each trading from wallets of its own.

pub mod competitor;
pub mod flow;
pub mod library;
pub mod takers;

use std::sync::Arc;

use async_trait::async_trait;
pub use competitor::{CompeteReport, Competitor};
pub use flow::{FlowWatch, SendReader, SentOrder, StepFlow, VenueFlow};
pub use library::{Library, LibraryReport};
pub use takers::{FlowReport, Pricing, TakerRecord, Takers};

use crate::guard::SendHook;

/// Where a step stands for the actors.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Moment {
    /// Slots into the session.
    pub at: u64,
    /// The scenario's level there, in USD: what the market trades around.
    pub level_usd: f64,
    /// Where the scenario puts the book's makers there, in USD: the level,
    /// moved by any premium the book trades at.
    pub book_usd: f64,
    /// The scenario's fair value there, in USD, when it reports one (see
    /// [`Scenario::fair_path`](crate::scenario::Scenario::fair_path)).
    pub fair_usd: Option<f64>,
}

/// A participant the grader runs.
#[async_trait]
pub trait Actor: Send + Sync {
    /// The start of a step, before its blocks: `last` is the moment the
    /// previous step ended at.
    async fn before_blocks(&mut self, last: &Moment) {
        let _ = last;
    }

    /// After the step's blocks, before the bot's turn.
    async fn after_blocks(&mut self, now: &Moment) {
        let _ = now;
    }

    /// After the bot's last turn: `last` is the moment the last step ended
    /// at.
    async fn end(&mut self, last: &Moment) {
        let _ = last;
    }

    /// Whether it reads each step's flow: the session then works it out
    /// for it ([`Cast::watch`]).
    fn wants_flow(&self) -> bool {
        false
    }

    /// What traded in the step, just before [`Actor::after_blocks`].
    async fn flow(&mut self, flow: &StepFlow) {
        let _ = flow;
    }

    /// What it runs around each of the bot's transactions, if anything.
    fn send_hook(&self) -> Option<Arc<dyn SendHook>> {
        None
    }

    /// What it did, into the session's report.
    async fn report(&mut self, report: &mut ActorsReport);
}

/// What a session's actors did.
#[derive(Debug, Clone, Default)]
pub struct ActorsReport {
    /// The takers', and each order they sent.
    pub flow: Option<FlowReport>,
    pub taker_orders: Vec<TakerRecord>,
    /// The grader's own maker's.
    pub competitor: Option<CompeteReport>,
    /// The scenario's library actors'.
    pub library: Option<LibraryReport>,
}

/// Every actor's [`SendHook`], run in the cast's order.
pub struct Hooks(pub Vec<Arc<dyn SendHook>>);

impl SendHook for Hooks {
    fn before<'a>(&'a self, sends: &'a [serde_json::Value]) -> futures::future::BoxFuture<'a, ()> {
        Box::pin(async move {
            for hook in &self.0 {
                hook.before(sends).await;
            }
        })
    }

    fn after(&self) -> futures::future::BoxFuture<'_, ()> {
        Box::pin(async move {
            for hook in &self.0 {
                hook.after().await;
            }
        })
    }
}

/// A session's actors, in the order they act at each point.
#[derive(Default)]
pub struct Cast {
    actors: Vec<Box<dyn Actor>>,
    /// The moment the last step ended at, once there is one.
    last: Option<Moment>,
    /// Where each step's flow is read, for the actors that want it.
    watch: Option<FlowWatch>,
}

impl Cast {
    /// Add an actor: it acts after those added before it.
    pub fn add(&mut self, actor: Box<dyn Actor>) {
        self.actors.push(actor);
    }

    /// Read each step's flow from `watch`, for the actors that want it.
    pub fn watch(&mut self, watch: FlowWatch) {
        self.watch = Some(watch);
    }

    /// The hooks the actors run around each of the bot's transactions.
    pub fn send_hooks(&self) -> Vec<Arc<dyn SendHook>> {
        self.actors.iter().filter_map(|a| a.send_hook()).collect()
    }

    /// The start of a step: nothing before the first.
    pub async fn before_blocks(&mut self) {
        let Some(last) = self.last else {
            return;
        };
        for actor in &mut self.actors {
            actor.before_blocks(&last).await;
        }
    }

    /// After the step's blocks, at `now`.
    pub async fn after_blocks(&mut self, now: Moment) {
        self.last = Some(now);
        if let Some(watch) = self.watch.as_mut()
            && self.actors.iter().any(|a| a.wants_flow())
        {
            let flow = watch.step().await;
            for actor in self.actors.iter_mut().filter(|a| a.wants_flow()) {
                actor.flow(&flow).await;
            }
        }
        for actor in &mut self.actors {
            actor.after_blocks(&now).await;
        }
    }

    /// After the bot's last turn.
    pub async fn end(&mut self) {
        let Some(last) = self.last else {
            return;
        };
        for actor in &mut self.actors {
            actor.end(&last).await;
        }
    }

    pub async fn report(mut self) -> ActorsReport {
        let mut report = ActorsReport::default();
        for actor in &mut self.actors {
            actor.report(&mut report).await;
        }
        report
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;

    /// Notes when it acts, under its name.
    struct Noting(&'static str, Arc<Mutex<Vec<String>>>);

    #[async_trait]
    impl Actor for Noting {
        async fn before_blocks(&mut self, last: &Moment) {
            self.1
                .lock()
                .unwrap()
                .push(format!("{} before {}", self.0, last.at));
        }
        async fn after_blocks(&mut self, now: &Moment) {
            self.1
                .lock()
                .unwrap()
                .push(format!("{} after {}", self.0, now.at));
        }
        async fn end(&mut self, last: &Moment) {
            self.1
                .lock()
                .unwrap()
                .push(format!("{} end {}", self.0, last.at));
        }
        async fn report(&mut self, _: &mut ActorsReport) {}
    }

    #[tokio::test]
    async fn actors_act_in_order_at_each_point_of_a_step() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let mut cast = Cast::default();
        cast.add(Box::new(Noting("takers", log.clone())));
        cast.add(Box::new(Noting("maker", log.clone())));
        let at = |at| Moment {
            at,
            level_usd: 100.0,
            book_usd: 100.0,
            fair_usd: None,
        };
        // Nothing happened before the first step.
        cast.before_blocks().await;
        cast.after_blocks(at(2)).await;
        cast.before_blocks().await;
        cast.after_blocks(at(4)).await;
        cast.end().await;
        assert_eq!(
            *log.lock().unwrap(),
            [
                "takers after 2",
                "maker after 2",
                "takers before 2",
                "maker before 2",
                "takers after 4",
                "maker after 4",
                "takers end 4",
                "maker end 4",
            ]
        );
    }
}
