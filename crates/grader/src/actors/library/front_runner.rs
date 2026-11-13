//! A front-runner: it watches a large trader, the bot, through what the
//! chain shows of its trading (its position, step by step), and once the
//! bot has traded the same way for `pattern_steps` steps in a row, each
//! more than `min_mult` market trade sizes, it trades that way ahead of the
//! bot's next turn: `share` of the bot's last step. It holds for
//! `hold_steps` and trades back out.
//!
//! With `on_send`, it front-runs the bot's transactions instead, as a
//! searcher reading the mempool would (a [`SendHook`]): while the pattern
//! holds, it reads each transaction the bot sends and, if it carries a
//! trade the pattern's way on the session's market (an order that takes at
//! once, or a limit through the touch) that the chain would run, takes
//! `share` of that order just before it lands.
//!
//! With `on_pool`, under a transaction layer, it reads the bot's sends in
//! the layer's pool as a searcher would, where it can see them (a leaky
//! route, a sandwiching leader), and sends its own take to the same pool
//! paying more per compute unit than the bot's, so the block runs it first.
//!
//! Either way it front-runs at most once a turn, and never holds more than
//! `max_inventory_mult` market trade sizes: what it does follows the bot,
//! so how often and how much is bounded by the market, not by how much the
//! bot sends.

use std::sync::Arc;

use async_trait::async_trait;
use futures::future::BoxFuture;
use mtx_kit::Side;
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

use super::{Scale, Wallet, finite_positive};
use crate::{
    actors::{Actor, ActorsReport, Moment, SendReader, SentOrder, StepFlow},
    guard::SendHook,
    txlayer::{TxEngine, client::ActorChain, engine::PoolTurn},
};

/// What it pays over the bot's send per compute unit, in micro-lamports, to
/// go first in the block.
const OUTBID: u64 = 1_000;

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

fn twenty() -> f64 {
    20.0
}

/// A front-runner's settings.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct FrontRunner {
    /// The bot's trading a step must pass to count, in market trade sizes;
    /// with `on_send`, the least order it front-runs.
    #[serde(default = "half")]
    pub min_mult: f64,
    #[serde(default = "two")]
    pub pattern_steps: u64,
    /// Its trade, as a share of the bot's last step (or, with `on_send`, of
    /// the order it front-runs).
    #[serde(default = "half")]
    pub share: f64,
    #[serde(default = "three")]
    pub hold_steps: u64,
    #[serde(default = "fifty")]
    pub limit_bps: f64,
    /// The most it holds from front-running, in market trade sizes.
    #[serde(default = "twenty")]
    pub max_inventory_mult: f64,
    #[serde(default)]
    pub on_send: bool,
    /// Read the bot's pending sends in the transaction layer's pool, and
    /// land ahead of one in its block.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub on_pool: bool,
}

impl Default for FrontRunner {
    fn default() -> Self {
        Self {
            min_mult: half(),
            pattern_steps: two(),
            share: half(),
            hold_steps: three(),
            limit_bps: fifty(),
            max_inventory_mult: twenty(),
            on_send: false,
            on_pool: false,
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
        finite_positive("limit_bps", self.limit_bps)?;
        if self.on_send && self.on_pool {
            return Err("on_send and on_pool are two ways to read the bot's sends: one".into());
        }
        finite_positive("max_inventory_mult", self.max_inventory_mult)
    }
}

struct Inner {
    spec: FrontRunner,
    scale: Scale,
    wallet: Wallet,
    /// What the bot's sends carry, for `on_send`.
    reader: Option<SendReader>,
    /// The bot's way and how many steps in a row it has traded it, and its
    /// last step's size.
    run: Option<(Side, u64, f64)>,
    /// What it holds from front-running: each trade's way, size and the
    /// step it was made at.
    held: Vec<(Side, f64, u64)>,
    steps: u64,
    /// Whether it has front-run in the turn since the last step.
    fronted: bool,
    /// Where the last step left the price.
    last: Option<Moment>,
    /// With `on_pool`: its seat's chain, sending to the pool, and the
    /// bot's sends it has read there.
    chain: Option<Arc<ActorChain>>,
    read: std::collections::BTreeSet<u64>,
}

impl Inner {
    fn limit(&self, side: Side, price: f64) -> f64 {
        let reach = self.spec.limit_bps / 10_000.0;
        match side {
            Side::Bid => price * (1.0 + reach),
            Side::Ask => price * (1.0 - reach),
        }
    }

    /// The bot's way and its last step's size, if its pattern holds and the
    /// turn has no front-run yet.
    fn armed(&self) -> Option<(Side, f64)> {
        if self.fronted {
            return None;
        }
        self.run
            .filter(|(_, n, _)| *n >= self.spec.pattern_steps)
            .map(|(side, _, size)| (side, size))
    }

    /// The most it may still take: what it holds stays under its cap.
    fn room(&self) -> f64 {
        let held: f64 = self.held.iter().map(|(_, size, _)| size).sum();
        (self.spec.max_inventory_mult * self.scale.trade_size - held).max(0.0)
    }

    /// Take `share` of `size` the bot's way, within its cap: once a turn.
    async fn front_run(&mut self, at: u64, price: f64, side: Side, size: f64, what: &str) {
        self.fronted = true;
        let size = (size * self.spec.share).min(self.room());
        if size <= 0.0 {
            self.wallet.count("capped", 1);
            return;
        }
        if self
            .wallet
            .take(at, side, size, self.limit(side, price))
            .await
        {
            self.held.push((side, size, self.steps));
            self.wallet.count(what, 1);
        }
    }

    /// Whether `order` is a trade the bot's way that it front-runs: big
    /// enough, and one that takes (at once, or through the touch).
    async fn worth_running(&self, side: Side, order: &SentOrder) -> bool {
        if order.side != side || order.size < self.spec.min_mult * self.scale.trade_size {
            return false;
        }
        if order.immediate {
            return true;
        }
        let (Some(price), Ok((bid, ask))) = (order.price, self.wallet.seat().top().await) else {
            return false;
        };
        match side {
            Side::Bid => ask.is_some_and(|ask| price >= ask),
            Side::Ask => bid.is_some_and(|bid| price <= bid),
        }
    }

    /// Front-run the first of the bot's `sends` worth it, if the pattern
    /// holds.
    async fn on_sends(&mut self, sends: &[serde_json::Value]) {
        let (Some((side, _)), Some(last), Some(reader)) =
            (self.armed(), self.last, self.reader.clone())
        else {
            return;
        };
        for send in sends {
            for order in reader(send.clone()).await {
                if self.worth_running(side, &order).await {
                    self.front_run(last.at, last.book_usd, side, order.size, "send_front_runs")
                        .await;
                    return;
                }
            }
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
    pub fn new(
        spec: FrontRunner,
        scale: Scale,
        wallet: Wallet,
        reader: Option<SendReader>,
        chain: Option<Arc<ActorChain>>,
    ) -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inner {
                spec,
                scale,
                wallet,
                reader,
                run: None,
                held: Vec::new(),
                steps: 0,
                fronted: false,
                last: None,
                chain,
                read: std::collections::BTreeSet::new(),
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
        inner.fronted = false;
        inner.unwind(now.at, now.book_usd, false).await;
        if inner.spec.on_send || inner.spec.on_pool {
            return;
        }
        if let Some((side, size)) = inner.armed() {
            inner
                .front_run(now.at, now.book_usd, side, size, "front_runs")
                .await;
        }
    }

    async fn end(&mut self, last: &Moment) {
        self.inner
            .lock()
            .await
            .unwind(last.at, last.book_usd, true)
            .await;
    }

    fn bind_tx(&mut self, engine: &Arc<TxEngine>) {
        if let Ok(inner) = self.inner.try_lock()
            && let Some(chain) = &inner.chain
        {
            chain.bind(engine);
        }
    }

    fn wants_pool(&self) -> bool {
        self.inner
            .try_lock()
            .is_ok_and(|inner| inner.spec.on_pool && inner.chain.is_some())
    }

    /// Front-run the first of the bot's sends it can see in the pool that
    /// is worth it, if the pattern holds: paying more than it, to land first.
    async fn on_pool(&mut self, turn: &PoolTurn) {
        let mut inner = self.inner.lock().await;
        let (Some((side, _)), Some(last), Some(reader), Some(chain)) = (
            inner.armed(),
            inner.last,
            inner.reader.clone(),
            inner.chain.clone(),
        ) else {
            return;
        };
        if turn.skipped {
            return;
        }
        for pending in &turn.visible {
            if !inner.read.insert(pending.id) {
                continue;
            }
            inner.wallet.count("pool_reads", 1);
            use base64::Engine as _;
            let call = serde_json::json!({
                "method": "sendTransaction",
                "params": [
                    base64::engine::general_purpose::STANDARD.encode(&pending.tx.wire),
                    { "encoding": "base64" }
                ]
            });
            for order in reader(call).await {
                if inner.worth_running(side, &order).await {
                    chain.set_price(
                        pending
                            .tx
                            .effective_price()
                            .max(turn.price_floor)
                            .saturating_add(OUTBID),
                    );
                    inner
                        .front_run(last.at, last.book_usd, side, order.size, "pool_front_runs")
                        .await;
                    return;
                }
            }
        }
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

/// Front-runs the bot's sends while the pattern holds.
struct Hook(Arc<Mutex<Inner>>);

impl SendHook for Hook {
    fn before<'a>(&'a self, sends: &'a [serde_json::Value]) -> BoxFuture<'a, ()> {
        Box::pin(async move { self.0.lock().await.on_sends(sends).await })
    }

    fn after(&self) -> BoxFuture<'_, ()> {
        Box::pin(async {})
    }
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

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

    /// A reader of pretend sends: each names the orders it carries, and
    /// one that wouldn't run carries none.
    fn reader() -> SendReader {
        Arc::new(|call: Value| {
            Box::pin(async move {
                if call["fails"].as_bool() == Some(true) {
                    return Vec::new();
                }
                call["orders"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|o| SentOrder {
                        side: if o["side"] == "bid" {
                            Side::Bid
                        } else {
                            Side::Ask
                        },
                        size: o["size"].as_f64().unwrap(),
                        price: o["price"].as_f64(),
                        immediate: o["immediate"].as_bool().unwrap_or(false),
                    })
                    .collect()
            })
        })
    }

    fn send(orders: Value) -> Value {
        json!({ "method": "sendTransaction", "orders": orders })
    }

    #[tokio::test]
    async fn after_a_run_of_selling_it_sells_ahead_then_buys_back() {
        let (book, log) = Book::new("f", 99.0, 101.0);
        let (wallet, ledger) = wallet(book);
        let mut runner = Runner::new(
            FrontRunner {
                hold_steps: 2,
                ..FrontRunner::default()
            },
            scale(),
            wallet,
            None,
            None,
        );
        assert!(runner.send_hook().is_none());
        // Too small to count, then two steps of selling 4, then none.
        for (s, flow) in [
            (2, sold(0.2)),
            (4, sold(4.0)),
            (6, sold(4.0)),
            (8, StepFlow::default()),
            (10, StepFlow::default()),
        ] {
            runner.flow(&flow).await;
            runner.after_blocks(&at(s, 100.0)).await;
        }
        assert_eq!(
            *log.lock().unwrap(),
            ["f take Ask 2.000 at 99.50", "f take Bid 2.000 at 100.50"]
        );
        let tally = &ledger.report().actors[0];
        assert_eq!(tally.counts["front_runs"], 1);
        assert_eq!((tally.sold, tally.bought), (2.0, 2.0));
    }

    #[tokio::test]
    async fn a_bot_spamming_sends_gets_one_front_run_a_turn_within_the_cap() {
        let (book, log) = Book::new("f", 99.0, 101.0);
        let position = book.position.clone();
        let (wallet, ledger) = wallet(book);
        let mut runner = Runner::new(
            FrontRunner {
                on_send: true,
                hold_steps: 1_000,
                max_inventory_mult: 5.0,
                ..FrontRunner::default()
            },
            scale(),
            wallet,
            Some(reader()),
            None,
        );
        let hook = runner.send_hook().expect("on_send hooks the bot's sends");
        let sell = |size: f64| json!([{ "side": "ask", "size": size, "immediate": true }]);
        // Before the pattern holds, nothing.
        hook.before(&[send(sell(4.0))]).await;
        assert!(log.lock().unwrap().is_empty());
        for s in [2, 4] {
            runner.flow(&sold(4.0)).await;
            runner.after_blocks(&at(s, 100.0)).await;
        }
        // Armed: what isn't a sale that takes is left alone, whatever the
        // bot sends: a cancel, buys, a resting ask behind the touch, a
        // sale that wouldn't run, one too small to count.
        let ignored = [
            send(json!([])),
            send(json!([{ "side": "bid", "size": 4.0, "immediate": true }])),
            send(json!([{ "side": "ask", "size": 4.0, "price": 102.0 }])),
            json!({ "method": "sendTransaction", "orders": sell(4.0), "fails": true }),
            send(sell(0.1)),
        ];
        for call in &ignored {
            hook.before(std::slice::from_ref(call)).await;
        }
        assert!(log.lock().unwrap().is_empty(), "{:?}", log.lock().unwrap());
        // Then it spams sales, one call at a time and in a batch: one
        // front-run a turn, of the order's share.
        for _ in 0..20 {
            hook.before(&[send(sell(4.0))]).await;
        }
        hook.before(&vec![send(sell(4.0)); 10]).await;
        assert_eq!(*log.lock().unwrap(), ["f take Ask 2.000 at 99.50"]);
        // Turn after turn, it never holds more than its cap: 5 trade sizes.
        for s in (6..40).step_by(2) {
            runner.flow(&sold(4.0)).await;
            runner.after_blocks(&at(s, 100.0)).await;
            for _ in 0..5 {
                hook.before(&[send(sell(40.0))]).await;
            }
            assert!(*position.lock().unwrap() >= -5.0 - 1e-9);
        }
        assert!((*position.lock().unwrap() + 5.0).abs() < 1e-9);
        let tally = &ledger.report().actors[0];
        assert_eq!(
            tally.counts["send_front_runs"], 2,
            "2, then the 3 left under the cap"
        );
        assert!(tally.counts["capped"] > 0);
        // A limit through the touch takes: it counts.
        runner.end(&at(40, 100.0)).await;
        runner.flow(&sold(4.0)).await;
        runner.after_blocks(&at(42, 100.0)).await;
        hook.before(&[send(json!([{ "side": "ask", "size": 4.0, "price": 98.0 }]))])
            .await;
        assert_eq!(
            log.lock().unwrap().last().unwrap(),
            "f take Ask 2.000 at 99.50"
        );
    }
}
