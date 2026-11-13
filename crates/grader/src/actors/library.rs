//! The actor library: the market's other participants, as a scenario lists
//! them in its `actors` section ([`Library`]).
//!
//! - **Noise traders** ([`Noise`]) take at random, at the market's own pace.
//! - **Informed traders** ([`Informed`]) trade ahead of the scenario's path:
//!   they see the level a horizon ahead and take when it is far enough off.
//! - **Chasers** ([`Chaser`]) take the way the book has moved, and
//!   **decoys** ([`Decoy`]) trade as the informed do for a while, then
//!   against the move: directional flow that knows nothing, which only a
//!   reader of each wallet's record tells from the informed.
//! - **Spoofers** ([`Spoofer`]) rest walls near the touch and pull them as
//!   the price comes near, always from the same wallet.
//! - **Stop hunters** ([`StopHunter`]) push the price through an obvious
//!   level and then trade back.
//! - **Front-runners** ([`FrontRunner`]) watch the bot's trading and, once
//!   it shows a pattern, trade the same way ahead of it.
//! - **An arbitrageur** ([`Arbitrageur`]) trades the market back to the
//!   scenario's reference when it strays past an edge.
//! - **RFQ clients** ([`RfqClients`]) ask the bot for quotes, some of them
//!   informed, and trade with it when a quote suits them.
//! - **A competing liquidator** ([`Liquidator`]) races the bot for
//!   liquidations on a lending venue.
//!
//! Every one of them trades from wallets of its own, through a
//! [`Seat`](crate::venue::Seat) on the session's market, with ordinary
//! transactions anyone reading the chain sees. Their keys come from the
//! session's keyring and their randomness from a seed it derives
//! ([`crate::keys::SessionKeyring::seed`]), so a session replays the same.
//! Sizes and rates are multiples of the market's own ([`Scale`]), as its
//! calibration measures them.
//!
//! Every order, cancel, take and swap counts as an action ([`Actions`]). A
//! session where more than [`MAX_FAILED_SHARE`] of them failed didn't see the
//! market its scenario describes, and the grade fails as infrastructure. The
//! actors that act on the bot's own trading (front-runners, RFQ clients) are
//! left out of that count: the bot can make their actions fail (a swap it
//! can't pay for), and a bot must never void its own session.

mod arbitrageur;
mod front_runner;
mod liquidator;
pub mod rfq;
mod spoofer;
mod stop_hunter;
mod traders;

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use mtx_kit::Side;
use serde::{Deserialize, Serialize};
use solana_pubkey::Pubkey;
use solana_transaction::Signature;

pub use self::{
    arbitrageur::Arbitrageur,
    front_runner::FrontRunner,
    liquidator::{Candidate, Liquidations, Liquidator},
    rfq::{RfqBoard, RfqClients, RfqSeat, Settlement, Swaps, Token},
    spoofer::Spoofer,
    stop_hunter::StopHunter,
    traders::{Chaser, Decoy, Informed, Noise},
};
use super::{Actor, ActorsReport, Moment, SendReader};
use crate::venue::Seat;

/// The share of the library's actions that may fail before the session
/// isn't scored.
pub const MAX_FAILED_SHARE: f64 = 0.1;

/// Seconds a slot lasts: a calibration's per-minute rates, per slot.
pub const SLOT_SECONDS: f64 = 0.4;

/// A scenario's library actors. Each list may name several of a kind.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Library {
    /// The market's own sizes and pace: what the actors' multiples count.
    pub scale: Scale,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub noise: Vec<Noise>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub informed: Vec<Informed>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub chasers: Vec<Chaser>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub decoys: Vec<Decoy>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub spoofers: Vec<Spoofer>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub stop_hunters: Vec<StopHunter>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub front_runners: Vec<FrontRunner>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arbitrageur: Option<Arbitrageur>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rfq_clients: Option<RfqClients>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub liquidator: Option<Liquidator>,
    /// Keep every action in the report's tape (checks and studies: it
    /// grows with the session).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub record: bool,
}

/// A market's taker flow, as its calibration measures it
/// (`venues.<market>.trades_per_minute` and `trade_size`).
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Scale {
    pub trades_per_minute: f64,
    /// The median trade, in base units, and the sigma of its lognormal.
    pub trade_size: f64,
    pub trade_size_sigma: f64,
}

impl Scale {
    /// Trades a slot.
    pub fn per_slot(&self) -> f64 {
        self.trades_per_minute * SLOT_SECONDS / 60.0
    }

    /// One trade's size, drawn at `mult` times the market's.
    pub fn draw_size(&self, rng: &mut Rng, mult: f64) -> f64 {
        rng.lognormal(self.trade_size * mult, self.trade_size_sigma)
    }

    fn validate(&self) -> Result<(), String> {
        let positive = |v: f64| v.is_finite() && v > 0.0;
        if !positive(self.trades_per_minute) || !positive(self.trade_size) {
            return Err("actors.scale: trades_per_minute and trade_size must be positive".into());
        }
        if !(self.trade_size_sigma.is_finite() && self.trade_size_sigma >= 0.0) {
            return Err("actors.scale: trade_size_sigma must be at least 0".into());
        }
        Ok(())
    }
}

/// A window of the session an actor acts in, in slots into it: from `from`
/// until before `until` (the end, without one).
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Window {
    #[serde(default)]
    pub from: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub until: Option<u64>,
}

impl Window {
    pub fn holds(window: &Option<Window>, at: u64) -> bool {
        window.is_none_or(|w| at >= w.from && w.until.is_none_or(|until| at < until))
    }
}

/// The side of a book an actor works.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum BookSide {
    #[default]
    Bid,
    Ask,
}

impl From<BookSide> for Side {
    fn from(side: BookSide) -> Self {
        match side {
            BookSide::Bid => Side::Bid,
            BookSide::Ask => Side::Ask,
        }
    }
}

pub(crate) fn finite_positive(what: &str, v: f64) -> Result<(), String> {
    if v.is_finite() && v > 0.0 {
        Ok(())
    } else {
        Err(format!("{what} must be positive"))
    }
}

pub(crate) fn finite_at_least_zero(what: &str, v: f64) -> Result<(), String> {
    if v.is_finite() && v >= 0.0 {
        Ok(())
    } else {
        Err(format!("{what} must be at least 0"))
    }
}

impl Library {
    pub fn validate(&self) -> Result<(), String> {
        self.scale.validate()?;
        let each = |kind: &str, checks: Vec<Result<(), String>>| {
            checks
                .into_iter()
                .enumerate()
                .try_for_each(|(i, c)| c.map_err(|e| format!("actors.{kind}[{i}]: {e}")))
        };
        each("noise", self.noise.iter().map(Noise::validate).collect())?;
        each(
            "informed",
            self.informed.iter().map(Informed::validate).collect(),
        )?;
        each(
            "chasers",
            self.chasers.iter().map(Chaser::validate).collect(),
        )?;
        each("decoys", self.decoys.iter().map(Decoy::validate).collect())?;
        each(
            "spoofers",
            self.spoofers.iter().map(Spoofer::validate).collect(),
        )?;
        each(
            "stop_hunters",
            self.stop_hunters.iter().map(StopHunter::validate).collect(),
        )?;
        each(
            "front_runners",
            self.front_runners
                .iter()
                .map(FrontRunner::validate)
                .collect(),
        )?;
        if let Some(arb) = &self.arbitrageur {
            arb.validate()
                .map_err(|e| format!("actors.arbitrageur: {e}"))?;
        }
        if let Some(rfq) = &self.rfq_clients {
            rfq.validate()
                .map_err(|e| format!("actors.rfq_clients: {e}"))?;
        }
        if let Some(liquidator) = &self.liquidator {
            liquidator
                .validate()
                .map_err(|e| format!("actors.liquidator: {e}"))?;
        }
        Ok(())
    }

    /// The wallets the library trades from, in the order [`Library::build`]
    /// takes their seats: each `(role, index)`, numbered within its role.
    pub fn wallets(&self) -> Vec<(&'static str, u32)> {
        let mut wallets = Vec::new();
        let mut add = |role: &'static str, n: u32| {
            let from = wallets.iter().filter(|(r, _)| *r == role).count() as u32;
            wallets.extend((from..from + n).map(|i| (role, i)));
        };
        for noise in &self.noise {
            add("noise", noise.wallets);
        }
        for informed in &self.informed {
            add("informed", informed.wallets);
        }
        for chaser in &self.chasers {
            add("chaser", chaser.wallets);
        }
        for decoy in &self.decoys {
            add("decoy", decoy.wallets);
        }
        for _ in &self.spoofers {
            add("spoofer", 1);
        }
        for _ in &self.stop_hunters {
            add("stop-hunter", 1);
        }
        for _ in &self.front_runners {
            add("front-runner", 1);
        }
        if self.arbitrageur.is_some() {
            add("arbitrageur", 1);
        }
        if let Some(rfq) = &self.rfq_clients {
            add("rfq-client", rfq.clients);
        }
        if self.liquidator.is_some() {
            add("liquidator", 1);
        }
        wallets
    }

    /// The library's actors, trading from `seats` (one per
    /// [`Library::wallets`], in that order) in a session whose scenario is
    /// `setting`. Each draws from its own seed, from `seed(role, n)`.
    pub fn build(
        &self,
        seats: Vec<WalletSeat>,
        setting: Setting,
        seed: &dyn Fn(&str, u32) -> [u8; 32],
    ) -> Built {
        let ledger = Ledger::new(self.record);
        let mut seats = seats.into_iter();
        let mut take = |n: u32| -> Vec<WalletSeat> { seats.by_ref().take(n as usize).collect() };
        let mut built = Built {
            actors: Vec::new(),
            rfq: None,
        };
        let mut nth = std::collections::BTreeMap::<&str, u32>::new();
        let mut rng = |role: &'static str| {
            let n = nth.entry(role).or_default();
            *n += 1;
            Rng::new(seed(role, *n - 1))
        };
        let scale = self.scale;
        for spec in &self.noise {
            let wallets = ledger.wallets("noise", take(spec.wallets));
            built.actors.push(Box::new(traders::NoiseTrader::new(
                spec.clone(),
                scale,
                wallets,
                rng("noise"),
            )));
        }
        for spec in &self.informed {
            let wallets = ledger.wallets("informed", take(spec.wallets));
            built.actors.push(Box::new(traders::InformedTrader::new(
                spec.clone(),
                scale,
                wallets,
                setting.path.clone(),
                rng("informed"),
            )));
        }
        for spec in &self.chasers {
            let wallets = ledger.wallets("chaser", take(spec.wallets));
            built.actors.push(Box::new(traders::ChaserTrader::new(
                spec.clone(),
                scale,
                wallets,
                setting.path.clone(),
                rng("chaser"),
            )));
        }
        for spec in &self.decoys {
            let wallets = ledger.wallets("decoy", take(spec.wallets));
            built.actors.push(Box::new(traders::DecoyTrader::new(
                spec.clone(),
                scale,
                wallets,
                setting.path.clone(),
                rng("decoy"),
            )));
        }
        for spec in &self.spoofers {
            let wallet = ledger.wallets("spoofer", take(1)).remove(0);
            built.actors.push(Box::new(spoofer::SpooferActor::new(
                spec.clone(),
                scale,
                wallet,
                setting.path.clone(),
            )));
        }
        for spec in &self.stop_hunters {
            let wallet = ledger.wallets("stop-hunter", take(1)).remove(0);
            built.actors.push(Box::new(stop_hunter::Hunter::new(
                spec.clone(),
                scale,
                wallet,
            )));
        }
        for spec in &self.front_runners {
            let wallet = ledger.reacting("front-runner", take(1)).remove(0);
            built.actors.push(Box::new(front_runner::Runner::new(
                spec.clone(),
                scale,
                wallet,
                setting.send_reader.clone(),
            )));
        }
        if let Some(spec) = &self.arbitrageur {
            let wallet = ledger.wallets("arbitrageur", take(1)).remove(0);
            built
                .actors
                .push(Box::new(arbitrageur::Arb::new(spec.clone(), scale, wallet)));
        }
        if let Some(spec) = &self.rfq_clients {
            let clients = ledger.reacting("rfq-client", take(spec.clients));
            let board = RfqBoard::new(format!("rfq:{}", setting.symbol));
            built.actors.push(Box::new(
                rfq::Clients::new(
                    spec.clone(),
                    scale,
                    clients,
                    board.clone(),
                    setting.settlement.clone(),
                    setting.path.clone(),
                    rng("rfq-client"),
                )
                .starting_at(setting.base_slot),
            ));
            built.rfq = Some(board);
        }
        if let Some(spec) = &self.liquidator {
            let wallet = ledger.wallets("liquidator", take(1)).remove(0);
            built.actors.push(Box::new(liquidator::Competing::new(
                spec.clone(),
                wallet,
                setting.liquidations,
            )));
        }
        built.actors.push(Box::new(Reporter {
            ledger,
            rpc_url: setting.rpc_url.filter(|_| self.record),
        }));
        built
    }
}

/// The scenario a session's library acts in.
pub struct Setting {
    /// The market's symbol.
    pub symbol: String,
    /// The chain's slot the session's steps count from.
    pub base_slot: u64,
    /// The scenario's moment at any slot into the session: what the
    /// informed see ahead.
    pub path: Arc<dyn Fn(u64) -> Moment + Send + Sync>,
    /// How RFQ fills settle between a client and the bot.
    pub settlement: Option<Arc<dyn Settlement>>,
    /// What the bot's sends carry, for the front-runners that read them.
    pub send_reader: Option<SendReader>,
    /// The session's lending venue, for the liquidator: none yet.
    pub liquidations: Option<Box<dyn Liquidations>>,
    /// The grader's own endpoint: with [`Library::record`], the report
    /// counts each actor's transactions there, as the chain lists them.
    pub rpc_url: Option<String>,
}

/// One of the library's wallets, with its seat on the session's market.
pub struct WalletSeat {
    pub pubkey: Pubkey,
    pub seat: Box<dyn Seat>,
}

/// The library's actors, ready for the cast, and the RFQ board the bot's
/// desk reads, if there are RFQ clients.
pub struct Built {
    pub actors: Vec<Box<dyn Actor>>,
    pub rfq: Option<RfqBoard>,
}

/// Counts of actions: those that landed, by kind, and those that failed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize)]
pub struct Actions {
    pub placed: usize,
    pub cancelled: usize,
    pub took: usize,
    pub failed: usize,
}

impl Actions {
    /// Every action tried, failed ones included.
    pub fn total(&self) -> usize {
        self.placed + self.cancelled + self.took + self.failed
    }
}

/// What one of the library's actors did.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct ActorTally {
    /// Its role (`noise`, `spoofer`, …) and its wallets.
    pub kind: String,
    pub wallets: Vec<String>,
    /// It acts on the bot's own trading, so its failures don't count
    /// toward [`LibraryReport::degraded`].
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub reacts_to_bot: bool,
    pub actions: Actions,
    /// Base units it bought and sold by taking.
    pub bought: f64,
    pub sold: f64,
    /// What its kind counts beyond that (walls pulled, requests sent, …).
    #[serde(skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub counts: std::collections::BTreeMap<String, u64>,
    /// With [`Library::record`]: how many of its actions' transactions the
    /// chain has, executed without error.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub on_chain: Option<usize>,
    /// Its actions' transactions, for that count.
    #[serde(skip)]
    pub signatures: Vec<String>,
}

/// One action, as the chain shows it: who, what, at which slot into the
/// session.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TapeEntry {
    pub at: u64,
    pub wallet: String,
    /// `take`, `rest`, `cancel` or `swap` (an RFQ fill, with the bot).
    pub action: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub side: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub price_usd: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<f64>,
    pub ok: bool,
    /// Its transaction, where the seat keeps it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,
}

/// What the library's actors did in a session.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct LibraryReport {
    /// Every actor's actions, summed.
    pub actions: Actions,
    pub actors: Vec<ActorTally>,
    /// Every action in order, with [`Library::record`].
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tape: Vec<TapeEntry>,
}

impl LibraryReport {
    /// The actions of the actors that act on their own, not on the bot's
    /// trading: those that say whether the session saw its market.
    pub fn independent(&self) -> Actions {
        let mut actions = Actions::default();
        for tally in self.actors.iter().filter(|t| !t.reacts_to_bot) {
            actions.placed += tally.actions.placed;
            actions.cancelled += tally.actions.cancelled;
            actions.took += tally.actions.took;
            actions.failed += tally.actions.failed;
        }
        actions
    }

    /// More of the independent actions failed than a session may lose.
    pub fn degraded(&self) -> bool {
        let actions = self.independent();
        actions.failed as f64 > MAX_FAILED_SHARE * actions.total() as f64
    }
}

/// Where the library's actors note what they do.
#[derive(Clone)]
pub struct Ledger(Arc<Mutex<LedgerState>>);

struct LedgerState {
    record: bool,
    actors: Vec<ActorTally>,
    tape: Vec<TapeEntry>,
}

impl Ledger {
    fn new(record: bool) -> Self {
        Self(Arc::new(Mutex::new(LedgerState {
            record,
            actors: Vec::new(),
            tape: Vec::new(),
        })))
    }

    fn state(&self) -> std::sync::MutexGuard<'_, LedgerState> {
        self.0.lock().expect("ledger lock")
    }

    /// A new actor of `kind` trading from `seats`: its wallets.
    fn wallets(&self, kind: &str, seats: Vec<WalletSeat>) -> Vec<Wallet> {
        self.add(kind, seats, false)
    }

    /// As [`Ledger::wallets`], for an actor that acts on the bot's trading.
    fn reacting(&self, kind: &str, seats: Vec<WalletSeat>) -> Vec<Wallet> {
        self.add(kind, seats, true)
    }

    fn add(&self, kind: &str, seats: Vec<WalletSeat>, reacts_to_bot: bool) -> Vec<Wallet> {
        let mut state = self.state();
        state.actors.push(ActorTally {
            kind: kind.to_string(),
            wallets: seats.iter().map(|s| s.pubkey.to_string()).collect(),
            reacts_to_bot,
            ..ActorTally::default()
        });
        let actor = state.actors.len() - 1;
        seats
            .into_iter()
            .map(|s| Wallet {
                seat: s.seat,
                pubkey: s.pubkey,
                actor,
                ledger: self.clone(),
            })
            .collect()
    }

    fn note(&self, actor: usize, entry: TapeEntry, kind: Done) {
        let mut state = self.state();
        let tally = &mut state.actors[actor];
        match (kind, entry.ok) {
            (_, false) => tally.actions.failed += 1,
            (Done::Placed, true) => tally.actions.placed += 1,
            (Done::Cancelled, true) => tally.actions.cancelled += 1,
            (Done::Took(side), true) => {
                tally.actions.took += 1;
                let size = entry.size.unwrap_or(0.0);
                match side {
                    Side::Bid => tally.bought += size,
                    Side::Ask => tally.sold += size,
                }
            }
        }
        if state.record {
            if let Some(signature) = &entry.signature {
                state.actors[actor].signatures.push(signature.clone());
            }
            state.tape.push(entry);
        }
    }

    /// Add `n` to `actor`'s count of `what`.
    fn count(&self, actor: usize, what: &str, n: u64) {
        *self.state().actors[actor]
            .counts
            .entry(what.to_string())
            .or_default() += n;
    }

    fn report(&self) -> LibraryReport {
        let state = self.state();
        let mut actions = Actions::default();
        for tally in &state.actors {
            actions.placed += tally.actions.placed;
            actions.cancelled += tally.actions.cancelled;
            actions.took += tally.actions.took;
            actions.failed += tally.actions.failed;
        }
        LibraryReport {
            actions,
            actors: state.actors.clone(),
            tape: state.tape.clone(),
        }
    }
}

#[derive(Clone, Copy)]
enum Done {
    Placed,
    Cancelled,
    Took(Side),
}

fn side_name(side: Side) -> String {
    match side {
        Side::Bid => "bid",
        Side::Ask => "ask",
    }
    .into()
}

/// One of an actor's wallets: its seat, with every action noted.
pub struct Wallet {
    seat: Box<dyn Seat>,
    pubkey: Pubkey,
    actor: usize,
    ledger: Ledger,
}

impl Wallet {
    pub fn pubkey(&self) -> Pubkey {
        self.pubkey
    }

    pub fn seat(&self) -> &dyn Seat {
        self.seat.as_ref()
    }

    fn entry(&self, at: u64, action: &str, ok: bool) -> TapeEntry {
        TapeEntry {
            at,
            wallet: self.pubkey.to_string(),
            action: action.into(),
            side: None,
            price_usd: None,
            size: None,
            ok,
            signature: ok.then(|| self.seat.last_signature()).flatten(),
        }
    }

    /// Take up to `size` on `side` at no worse than `limit`, `at` slots in.
    pub async fn take(&self, at: u64, side: Side, size: f64, limit: f64) -> bool {
        let sent = self.seat.take(side, size, limit).await;
        if let Err(error) = &sent {
            tracing::warn!(wallet = %self.pubkey, %error, "an actor's take failed");
        }
        let entry = TapeEntry {
            side: Some(side_name(side)),
            price_usd: Some(limit),
            size: Some(size),
            ..self.entry(at, "take", sent.is_ok())
        };
        self.ledger.note(self.actor, entry, Done::Took(side));
        sent.is_ok()
    }

    /// Rest `size` on `side` at `price`, `at` slots in.
    pub async fn rest(&self, at: u64, side: Side, price: f64, size: f64) -> bool {
        let sent = self.seat.rest(side, price, size).await;
        if let Err(error) = &sent {
            tracing::warn!(wallet = %self.pubkey, %error, "an actor's order failed");
        }
        let entry = TapeEntry {
            side: Some(side_name(side)),
            price_usd: Some(price),
            size: Some(size),
            ..self.entry(at, "rest", sent.is_ok())
        };
        self.ledger.note(self.actor, entry, Done::Placed);
        sent.is_ok()
    }

    /// Cancel everything the wallet rests, `at` slots in.
    pub async fn cancel_all(&self, at: u64) -> bool {
        let sent = self.seat.cancel_all().await;
        if let Err(error) = &sent {
            tracing::warn!(wallet = %self.pubkey, %error, "an actor's cancel failed");
        }
        self.ledger.note(
            self.actor,
            self.entry(at, "cancel", sent.is_ok()),
            Done::Cancelled,
        );
        sent.is_ok()
    }

    /// Note an RFQ fill it settled with the bot (or failed to), `at` slots
    /// in: it bought (`Bid`) or sold `size` at `price`. Whether it settled.
    pub fn swap(
        &self,
        at: u64,
        side: Side,
        price: f64,
        size: f64,
        settled: Result<String, String>,
    ) -> bool {
        let entry = TapeEntry {
            side: Some(side_name(side)),
            price_usd: Some(price),
            size: Some(size),
            signature: settled.as_ref().ok().cloned(),
            ..self.entry(at, "swap", settled.is_ok())
        };
        self.ledger.note(self.actor, entry, Done::Took(side));
        settled.is_ok()
    }

    /// Add `n` to its actor's count of `what`.
    pub fn count(&self, what: &str, n: u64) {
        self.ledger.count(self.actor, what, n);
    }
}

/// Puts the ledger in the session's report: the cast's last actor.
struct Reporter {
    ledger: Ledger,
    rpc_url: Option<String>,
}

#[async_trait]
impl Actor for Reporter {
    async fn report(&mut self, report: &mut ActorsReport) {
        let mut library = self.ledger.report();
        if let Some(url) = &self.rpc_url {
            let rpc =
                mtx_kit::rpc_client_with_timeout(url.clone(), crate::session::INTERNAL_RPC_TIMEOUT);
            for tally in &mut library.actors {
                let signatures: Vec<Signature> = tally
                    .signatures
                    .iter()
                    .filter_map(|s| s.parse().ok())
                    .collect();
                let mut landed = 0;
                for chunk in signatures.chunks(256) {
                    match rpc.get_signature_statuses(chunk) {
                        Ok(statuses) => {
                            landed += statuses
                                .value
                                .iter()
                                .filter(|s| s.as_ref().is_some_and(|s| s.err.is_none()))
                                .count();
                        }
                        Err(error) => {
                            tracing::warn!(kind = tally.kind, %error, "reading an actor's transactions")
                        }
                    }
                }
                tally.on_chain = Some(landed);
            }
        }
        report.library = Some(library);
    }
}

/// The library's randomness: xoshiro256** from a 32-byte seed. Its own,
/// so the draws stay the same whatever the dependencies do.
pub struct Rng {
    s: [u64; 4],
}

impl Rng {
    pub fn new(seed: [u8; 32]) -> Self {
        let mut s = [0u64; 4];
        for (i, word) in s.iter_mut().enumerate() {
            *word = u64::from_le_bytes(seed[i * 8..i * 8 + 8].try_into().expect("8 bytes"));
        }
        if s == [0; 4] {
            s[0] = 1;
        }
        Self { s }
    }

    pub fn next_u64(&mut self) -> u64 {
        let result = self.s[1].wrapping_mul(5).rotate_left(7).wrapping_mul(9);
        let t = self.s[1] << 17;
        self.s[2] ^= self.s[0];
        self.s[3] ^= self.s[1];
        self.s[1] ^= self.s[2];
        self.s[0] ^= self.s[3];
        self.s[2] ^= t;
        self.s[3] = self.s[3].rotate_left(45);
        result
    }

    /// Uniform in [0, 1).
    pub fn uniform(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }

    pub fn chance(&mut self, p: f64) -> bool {
        self.uniform() < p
    }

    /// Uniform in 0..n (n > 0).
    pub fn below(&mut self, n: usize) -> usize {
        ((self.uniform() * n as f64) as usize).min(n.saturating_sub(1))
    }

    /// Standard normal (Box-Muller).
    pub fn normal(&mut self) -> f64 {
        let u = 1.0 - self.uniform();
        let v = self.uniform();
        (-2.0 * u.ln()).sqrt() * (std::f64::consts::TAU * v).cos()
    }

    pub fn lognormal(&mut self, median: f64, sigma: f64) -> f64 {
        median * (sigma * self.normal()).exp()
    }

    /// A Poisson count with mean `mean`.
    pub fn poisson(&mut self, mean: f64) -> u32 {
        if mean.is_nan() || mean <= 0.0 {
            return 0;
        }
        if mean > 30.0 {
            return (mean + mean.sqrt() * self.normal()).round().max(0.0) as u32;
        }
        let limit = (-mean).exp();
        let mut product = self.uniform();
        let mut n = 0;
        while product > limit {
            n += 1;
            product *= self.uniform();
        }
        n
    }
}

/// `usd` rounded down to the tick grid.
pub(crate) fn round_down(usd: f64, tick: f64) -> f64 {
    (usd / tick + 1e-9).floor() * tick
}

/// `usd` rounded up to the tick grid.
pub(crate) fn round_up(usd: f64, tick: f64) -> f64 {
    (usd / tick - 1e-9).ceil() * tick
}

#[cfg(test)]
pub(crate) mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::venue::SeatResult;

    /// A seat on a pretend book: a fixed touch the test sets, and every
    /// action noted.
    pub(crate) struct Book {
        pub top: Arc<Mutex<(Option<f64>, Option<f64>)>>,
        pub log: Arc<Mutex<Vec<String>>>,
        pub position: Arc<Mutex<f64>>,
        pub name: String,
    }

    impl Book {
        pub fn new(name: &str, bid: f64, ask: f64) -> (Self, Arc<Mutex<Vec<String>>>) {
            let log = Arc::new(Mutex::new(Vec::new()));
            (
                Self {
                    top: Arc::new(Mutex::new((Some(bid), Some(ask)))),
                    log: log.clone(),
                    position: Arc::new(Mutex::new(0.0)),
                    name: name.into(),
                },
                log,
            )
        }
    }

    #[async_trait]
    impl Seat for Book {
        fn market(&self) -> &str {
            "test"
        }
        fn executes(&self, _: &mtx_kit::Action) -> bool {
            true
        }
        async fn send(&self, _: &mtx_kit::Planned) -> mtx_kit::Sent {
            unimplemented!()
        }
        async fn take(&self, side: Side, size: f64, limit: f64) -> SeatResult {
            self.log.lock().unwrap().push(format!(
                "{} take {side:?} {size:.3} at {limit:.2}",
                self.name
            ));
            *self.position.lock().unwrap() += match side {
                Side::Bid => size,
                Side::Ask => -size,
            };
            Ok(())
        }
        async fn rest(&self, side: Side, price: f64, size: f64) -> SeatResult {
            self.log.lock().unwrap().push(format!(
                "{} rest {side:?} {size:.3} at {price:.2}",
                self.name
            ));
            Ok(())
        }
        async fn cancel_all(&self) -> SeatResult {
            self.log
                .lock()
                .unwrap()
                .push(format!("{} cancel", self.name));
            Ok(())
        }
        async fn top(&self) -> SeatResult<(Option<f64>, Option<f64>)> {
            Ok(*self.top.lock().unwrap())
        }
        async fn position(&self) -> SeatResult<f64> {
            Ok(*self.position.lock().unwrap())
        }
        fn tick(&self) -> f64 {
            0.01
        }
    }

    pub(crate) fn scale() -> Scale {
        Scale {
            trades_per_minute: 150.0,
            trade_size: 1.0,
            trade_size_sigma: 0.0,
        }
    }

    pub(crate) fn wallet(seat: Book) -> (Wallet, Ledger) {
        let ledger = Ledger::new(true);
        let wallet = ledger
            .wallets(
                "test",
                vec![WalletSeat {
                    pubkey: Pubkey::new_unique(),
                    seat: Box::new(seat),
                }],
            )
            .remove(0);
        (wallet, ledger)
    }

    pub(crate) fn at(at: u64, level_usd: f64) -> Moment {
        Moment {
            at,
            level_usd,
            book_usd: level_usd,
        }
    }

    #[test]
    fn draws_replay_from_their_seed() {
        let draws = |seed: u8| {
            let mut rng = Rng::new([seed; 32]);
            (0..5).map(|_| rng.next_u64()).collect::<Vec<_>>()
        };
        assert_eq!(draws(1), draws(1));
        assert_ne!(draws(1), draws(2));
        let mut rng = Rng::new([7; 32]);
        let n = 20_000;
        let mean = (0..n).map(|_| rng.poisson(2.5) as f64).sum::<f64>() / n as f64;
        assert!((mean - 2.5).abs() < 0.05, "{mean}");
        let mean = (0..n).map(|_| rng.normal()).sum::<f64>() / n as f64;
        assert!(mean.abs() < 0.03, "{mean}");
        assert!((0..n).all(|_| rng.below(3) < 3));
    }

    #[test]
    fn wallets_number_within_their_role_in_build_order() {
        let library: Library = serde_json::from_value(serde_json::json!({
            "scale": { "trades_per_minute": 1.5, "trade_size": 0.02, "trade_size_sigma": 1.0 },
            "noise": [{ "wallets": 2 }, { "wallets": 1 }],
            "spoofers": [{}, {}],
            "arbitrageur": {},
            "rfq_clients": { "clients": 3 }
        }))
        .unwrap();
        assert_eq!(library.validate(), Ok(()));
        assert_eq!(
            library.wallets(),
            [
                ("noise", 0),
                ("noise", 1),
                ("noise", 2),
                ("spoofer", 0),
                ("spoofer", 1),
                ("arbitrageur", 0),
                ("rfq-client", 0),
                ("rfq-client", 1),
                ("rfq-client", 2),
            ]
        );
        let json = serde_json::to_value(&library).unwrap();
        assert_eq!(serde_json::from_value::<Library>(json).unwrap(), library);
        let bad: Result<Library, _> =
            serde_json::from_value(serde_json::json!({ "scale": {}, "nope": [] }));
        assert!(bad.is_err(), "unknown fields are refused");
        let unscaled = Library::default();
        assert!(unscaled.validate().unwrap_err().contains("scale"));
    }

    #[tokio::test]
    async fn a_ledger_counts_each_action_and_a_session_that_lost_many_is_degraded() {
        let (book, _) = Book::new("w", 99.0, 101.0);
        let (wallet, ledger) = wallet(book);
        assert!(wallet.take(1, Side::Bid, 2.0, 101.0).await);
        assert!(wallet.rest(2, Side::Ask, 102.0, 1.0).await);
        assert!(wallet.cancel_all(3).await);
        wallet.count("walls", 2);
        let report = ledger.report();
        assert_eq!(
            report.actions,
            Actions {
                placed: 1,
                cancelled: 1,
                took: 1,
                failed: 0
            }
        );
        assert_eq!(report.actors[0].bought, 2.0);
        assert_eq!(report.actors[0].counts["walls"], 2);
        assert_eq!(report.tape.len(), 3);
        assert!(!report.degraded());
        let mut lost = report.clone();
        lost.actors[0].actions.failed = 1;
        assert!(lost.degraded(), "1 of 4 failed");
        lost.actors[0].actions = Actions {
            placed: 90,
            cancelled: 10,
            took: 0,
            failed: 10,
        };
        assert!(!lost.degraded());
        // An actor that acts on the bot's trading may fail all it likes.
        lost.actors[0].actions.failed = 1_000;
        lost.actors[0].reacts_to_bot = true;
        assert!(!lost.degraded());
    }
}
