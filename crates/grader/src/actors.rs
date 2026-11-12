//! Actors: the participants the grader runs in a session besides the bot,
//! each trading from accounts of its own through a [`Seat`](crate::venue::Seat)
//! on any [`Venue`](crate::venue::Venue).
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
//! Two actors come with the engine: [`takers`], the taker orders a scenario
//! lists, and [`competitor`], a maker that competes with the bot's quotes.
//! Both work on any venue whose seats take and rest orders.

pub mod competitor;
pub mod takers;

use async_trait::async_trait;
pub use competitor::{CompeteReport, Competitor};
pub use takers::{FlowReport, Pricing, TakerRecord, Takers};

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
}

/// A session's actors, in the order they act at each point.
#[derive(Default)]
pub struct Cast {
    actors: Vec<Box<dyn Actor>>,
    /// The moment the last step ended at, once there is one.
    last: Option<Moment>,
}

impl Cast {
    /// Add an actor: it acts after those added before it.
    pub fn add(&mut self, actor: Box<dyn Actor>) {
        self.actors.push(actor);
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
