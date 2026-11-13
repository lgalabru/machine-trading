//! A front-runner: it watches a large trader, the bot, through what the
//! chain shows of its trading (its position, step by step), and once the
//! bot has traded the same way for `pattern_steps` steps in a row, each
//! more than `min_mult` market trade sizes, it trades that way ahead of the
//! bot's next turn: `share` of the bot's last step. It holds for
//! `hold_steps` and trades back out.
//!
//! With `on_send`, it also acts as each of the bot's transactions arrives
//! (a [`SendHook`]): while the pattern holds, it takes the bot's way just
//! before the bot's order lands, as a searcher reading the mempool would.

use std::sync::Arc;

use async_trait::async_trait;
use futures::future::BoxFuture;
use mtx_kit::Side;
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

use super::{Scale, Wallet, finite_positive};
use crate::{
    actors::{Actor, ActorsReport, Moment, StepFlow},
    guard::SendHook,
};

fn two() -> u64 {
    2
}

fn half() -> f64 {
    0.5
}

fn fifty() -> f64 {
    50.0
}

fn three() -> u64 {
    3
}

/// A front-runner's settings.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct FrontRunner {
    /// The bot's trading a step must pass to count, in market trade sizes.
    #[serde(default = "half")]
    pub min_mult: f64,
    #[serde(default = "two")]
    pub pattern_steps: u64,
    /// Its trade, as a share of the bot's last step.
    #[serde(default = "half")]
    pub share: f64,
    #[serde(default = "three")]
    pub hold_steps: u64,
    #[serde(default = "fifty")]
    pub limit_bps: f64,
    #[serde(default)]
    pub on_send: bool,
}

impl Default for FrontRunner {
    fn default() -> Self {
        Self {
            min_mult: half(),
            pattern_steps: two(),
            share: half(),
            hold_steps: three(),
            limit_bps: fifty(),
            on_send: false,
        }
    }
}

impl FrontRunner {
    pub fn validate(&self) -> Result<(), String> {
        if self.pattern_steps == 0 {
            return Err("pattern_steps must be at least 1".into());
        }
        finite_positive("min_mult", self.min_mult)?;
        finite_positive("share", self.share)?;
        finite_positive("limit_bps", self.limit_bps)
    }
}

struct Inner {
    spec: FrontRunner,
    scale: Scale,
    wallet: Wallet,
    /// The bot's way and how many steps in a row it has traded it, and its
    /// last step's size.
    run: Option<(Side, u64, f64)>,
    /// What it holds from front-running: each trade's way, size and the
    /// step it was made at.
    held: Vec<(Side, f64, u64)>,
    steps: u64,
    /// Where the last step left the price.
    last: Option<Moment>,
}

impl Inner {
    fn limit(&self, side: Side, price: f64) -> f64 {
        let reach = self.spec.limit_bps / 10_000.0;
        match side {
            Side::Bid => price * (1.0 + reach),
            Side::Ask => price * (1.0 - reach),
        }
    }

    /// The bot's way, if its pattern holds.
    fn armed(&self) -> Option<(Side, f64)> {
        self.run
            .filter(|(_, n, _)| *n >= self.spec.pattern_steps)
            .map(|(side, _, size)| (side, size * self.spec.share))
    }

    async fn front_run(&mut self, at: u64, price: f64, what: &str) {
        let Some((side, size)) = self.armed() else {
            return;
        };
        if self
            .wallet
            .take(at, side, size, self.limit(side, price))
            .await
        {
            self.held.push((side, size, self.steps));
            self.wallet.count(what, 1);
        }
    }

    async fn unwind(&mut self, at: u64, price: f64, all: bool) {
        let due: Vec<_> = self
            .held
            .iter()
            .copied()
            .filter(|(_, _, step)| all || self.steps >= step + self.spec.hold_steps)
            .collect();
        self.held
            .retain(|(_, _, step)| !(all || self.steps >= step + self.spec.hold_steps));
        for (side, size, _) in due {
            let back = side.opposite();
            self.wallet
                .take(at, back, size, self.limit(back, price))
                .await;
        }
    }
}

pub(super) struct Runner {
    inner: Arc<Mutex<Inner>>,
}

impl Runner {
    pub fn new(spec: FrontRunner, scale: Scale, wallet: Wallet) -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inner {
                spec,
                scale,
                wallet,
                run: None,
                held: Vec::new(),
                steps: 0,
                last: None,
            })),
        }
    }
}

#[async_trait]
impl Actor for Runner {
    fn wants_flow(&self) -> bool {
        true
    }

    async fn flow(&mut self, flow: &StepFlow) {
        let mut inner = self.inner.lock().await;
        let net = flow.bot_net();
        let min = inner.spec.min_mult * inner.scale.trade_size;
        let side = if net > min {
            Some(Side::Bid)
        } else if net < -min {
            Some(Side::Ask)
        } else {
            None
        };
        inner.run = side.map(|side| match inner.run {
            Some((before, n, _)) if before == side => (side, n + 1, net.abs()),
            _ => (side, 1, net.abs()),
        });
    }

    async fn after_blocks(&mut self, now: &Moment) {
        let mut inner = self.inner.lock().await;
        inner.steps += 1;
        inner.last = Some(*now);
        inner.unwind(now.at, now.book_usd, false).await;
        inner.front_run(now.at, now.book_usd, "front_runs").await;
    }

    async fn end(&mut self, last: &Moment) {
        self.inner
            .lock()
            .await
            .unwind(last.at, last.book_usd, true)
            .await;
    }

    fn send_hook(&self) -> Option<Arc<dyn SendHook>> {
        let on_send = self
            .inner
            .try_lock()
            .map(|inner| inner.spec.on_send)
            .unwrap_or(false);
        on_send.then(|| Arc::new(Hook(self.inner.clone())) as Arc<dyn SendHook>)
    }

    async fn report(&mut self, _: &mut ActorsReport) {}
}

/// Front-runs each of the bot's sends while the pattern holds.
struct Hook(Arc<Mutex<Inner>>);

impl SendHook for Hook {
    fn before(&self) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            let mut inner = self.0.lock().await;
            if let Some(last) = inner.last {
                inner
                    .front_run(last.at, last.book_usd, "send_front_runs")
                    .await;
            }
        })
    }

    fn after(&self) -> BoxFuture<'_, ()> {
        Box::pin(async {})
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::actors::{
        VenueFlow,
        library::tests::{Book, at, scale, wallet},
    };

    fn sold(base: f64) -> StepFlow {
        StepFlow {
            bot: vec![VenueFlow {
                market: "m".into(),
                bought: 0.0,
                sold: base,
            }],
        }
    }

    #[tokio::test]
    async fn after_a_run_of_selling_it_sells_ahead_then_buys_back() {
        let (book, log) = Book::new("f", 99.0, 101.0);
        let (wallet, ledger) = wallet(book);
        let mut runner = Runner::new(
            FrontRunner {
                hold_steps: 2,
                on_send: true,
                ..FrontRunner::default()
            },
            scale(),
            wallet,
        );
        let hook = runner.send_hook().expect("on_send hooks the bot's sends");
        // Too small to count, then two steps of selling 4.
        for (s, flow) in [(2, sold(0.2)), (4, sold(4.0)), (6, sold(4.0))] {
            runner.flow(&flow).await;
            runner.after_blocks(&at(s, 100.0)).await;
        }
        // The bot sends again while it holds the pattern.
        hook.before().await;
        hook.after().await;
        for s in [8, 10] {
            runner.flow(&StepFlow::default()).await;
            runner.after_blocks(&at(s, 100.0)).await;
        }
        assert_eq!(
            *log.lock().unwrap(),
            [
                "f take Ask 2.000 at 99.50",
                "f take Ask 2.000 at 99.50",
                "f take Bid 2.000 at 100.50",
                "f take Bid 2.000 at 100.50",
            ]
        );
        let tally = &ledger.report().actors[0];
        assert_eq!(tally.counts["front_runs"], 1);
        assert_eq!(tally.counts["send_front_runs"], 1);
        assert_eq!((tally.sold, tally.bought), (4.0, 4.0));
    }
}
