//! A strategy component playing one session: the host's side of its
//! steps. Each turn the desk reads the market, calls the strategy's `step`
//! (`crate::host`), cuts its sheet to the mandate, checks it, and sends
//! each order through the seat on its market (`crate::venue`): a line
//! alone, or an atomic group's lines joined into one transaction
//! ([`atomic`]), which its first line's seat signs and sends.
//!
//! The desk names nothing of where it runs. A grade hands it seats on a
//! surfnet and, under the transaction layer, the pool its sends wait in
//! ([`Sends`]); the mainnet runtime hands it seats on the chain
//! (`docs/rfd/0004-strategies-on-mainnet.md`).

use std::{io::Write, path::Path};

use async_trait::async_trait;
pub use mtx_core::session::{Account, Fate, StepTrades, VenueReading, Wallet};
use mtx_kit::{Planned, SendSource, SendState, SendStatus};
use solana_keypair::Keypair;

pub mod atomic;

use crate::{
    host::{FUEL_PER_STEP, NoRawSends, Outcome, RawSend, Reads, WasmStrategy},
    taking::{TakingGuard, Touch},
    venue::Seat,
};

/// Where a session's sends land later than they are sent: what takes each
/// line's transaction options, sends the bundles a sheet gathered, and says
/// what became of each send.
#[async_trait]
pub trait Sends: Send + Sync {
    /// The next sends are a line's, with its options; `None` after the
    /// sheet.
    fn set_options(&self, options: Option<mtx_kit::TxOptions>);
    /// Send the bundles the sheet gathered. For each, its transactions'
    /// signatures and why it failed, if it did.
    async fn flush_bundles(&self) -> Vec<(Vec<String>, Option<String>)>;
    /// What became of the send with `signature` (or a bundle's id).
    fn fate(&self, signature: &str) -> Option<Fate>;
    /// Recent fees, the tip floor and the next leaders, as a strategy's
    /// step shows them.
    fn fee_view(&self) -> mtx_kit::FeeView;
}

/// The requests for quotes open to a strategy, and its fills on them.
pub trait Rfqs: Send + Sync {
    /// The requests open now, as the strategy sees them.
    fn open(&self) -> Vec<mtx_kit::Rfq>;
    /// The strategy's fills since the last call: it is the maker of each.
    fn take_fills(&self) -> Vec<mtx_kit::OwnFill>;
}

/// What takes a strategy's raw sends over a session: [`RawSend`] during
/// the step, and what the desk does around it.
pub trait RawDesk: RawSend {
    /// Step `k` begins: sends are taken until [`RawDesk::close`].
    /// `account` is the account the session measured for it.
    fn begin(&mut self, k: u64, account: Option<Account>) -> impl Future<Output = ()> + Send;
    /// The step returned: no more sends are taken.
    fn close(&mut self);
    /// The step failed: what it took is dropped, and said so.
    fn discard(&mut self, why: &str) -> Vec<SendStatus>;
    /// Send what the step took, signed by `signer`, after the sheet.
    fn flush(&mut self, signer: &Keypair) -> impl Future<Output = Vec<SendStatus>> + Send;
    /// The sends that went into a pool since the last call: each one's id
    /// and signature.
    fn take_pooled(&mut self) -> Vec<(u32, String)>;
}

impl RawDesk for NoRawSends {
    async fn begin(&mut self, _k: u64, _account: Option<Account>) {
        match *self {}
    }

    fn close(&mut self) {
        match *self {}
    }

    fn discard(&mut self, _why: &str) -> Vec<SendStatus> {
        match *self {}
    }

    async fn flush(&mut self, _signer: &Keypair) -> Vec<SendStatus> {
        match *self {}
    }

    fn take_pooled(&mut self) -> Vec<(u32, String)> {
        match *self {}
    }
}

/// One step of a component's session, as `mtx bot run --debug` shows it:
/// what the strategy was handed, what it printed and asked for, and what the
/// grader sent.
#[derive(Debug, Clone)]
pub struct StepTrace {
    pub steps: u64,
    pub market: mtx_kit::Market,
    pub printed: Vec<String>,
    /// The strategy's error or trap, if `step` didn't return a sheet.
    pub failed: Option<String>,
    pub intents: Vec<mtx_kit::Intent>,
    /// Each line's options, beside `intents`.
    pub options: Vec<mtx_kit::LineOptions>,
    /// Where `plan_sheet()` cut the sheet to the cap.
    pub notes: Vec<String>,
    /// Each order planned, and what became of it; the grader stops at the
    /// first that fails.
    pub sent: Vec<(mtx_kit::Action, SendState)>,
    pub fuel: u64,
    pub rpc_calls: u32,
    /// The account as the session measured it at this step.
    pub account: Option<Account>,
}

impl StepTrace {
    /// A step's trace as its call left it: what it printed and spent, before
    /// its sheet is planned and sent.
    fn of_call(
        market: &mtx_kit::Market,
        printed: &[String],
        fuel: u64,
        rpc_calls: u32,
        steps: u64,
        account: Option<Account>,
    ) -> Self {
        Self {
            steps,
            market: market.clone(),
            printed: printed.to_vec(),
            failed: None,
            intents: Vec::new(),
            options: Vec::new(),
            notes: Vec::new(),
            sent: Vec::new(),
            fuel,
            rpc_calls,
            account,
        }
    }
}

/// Watches a component's session step by step: `mtx bot run --debug`. It
/// may block (to wait for a key): the world waits for the strategy.
pub trait StepObserver: Send + Sync {
    /// After `start`: the session, and what `start` printed or failed with.
    fn started(&self, session: &mtx_kit::Session, printed: &[String], failed: Option<&str>);
    /// After each step's orders are sent.
    fn step(&self, trace: &StepTrace);
    /// Before the session locks its chain's cheatcodes, which is before the
    /// bot starts: the chain's own RPC (its secret path included) and the
    /// bot's key, base58. The last moment a tool can load a geyser plugin
    /// into a session's surfnet.
    fn before_lock(&self, _url: &str, _authority: &str) {}
    /// Before the bot starts: the session's own RPC (its secret path
    /// included), for a tool that shows its transactions in an explorer.
    fn rpc(&self, _url: &str) {}
    /// When the session is over: its chain. Keep it to keep the RPC up (an
    /// explorer still reading the session's transactions); drop it to stop it.
    fn keep(&self, _world: Box<dyn std::any::Any + Send>) {}
}

/// A session's clock, as a component's `start` sees it, and its scenario.
pub struct Clock {
    /// The seed the strategy's randomness starts from.
    pub seed: u64,
    pub steps: u64,
    pub slots_per_step: u64,
    pub start_slot: u64,
    /// The task's variables for the session ([`mtx_kit::SESSION_VARS`]).
    pub vars: Vec<(String, String)>,
    /// What the session's steps report beyond prices.
    pub reports: mtx_kit::Reports,
    /// A wall deadline for each of the strategy's calls: past it the call
    /// is interrupted, as a trap. A grade has none (fuel alone bounds a
    /// step there); the live runtime does, since a slow step costs the
    /// market.
    pub step_deadline: Option<std::time::Duration>,
}

/// The mandate a component's session runs under, as its `start` sees it.
pub struct Terms<'a> {
    pub end_slot: u64,
    pub mandate: &'a mtx_core::Mandate,
    /// The position cap on its market, and its drawdown limit.
    pub cap: f64,
    pub max_drawdown: f64,
}

/// What a host trades and reads for a component: a seat on each market
/// the session trades, with the strategy's key, and how its market is
/// read.
pub struct Desk {
    /// The session's markets, the bot's own first, as `start` lists them
    /// (the interface's `venues`).
    pub markets: Vec<mtx_kit::VenueInfo>,
    /// A seat on each market the session trades. A line for a market
    /// without one comes back not sent.
    pub seats: Vec<Box<dyn Seat>>,
    /// The market traded, as the venue names it, and the venue's metadata,
    /// as `start` gets them.
    pub symbol: String,
    pub venue: String,
    /// The bot's key, base58.
    pub authority: String,
    pub reader: Reader,
    /// Where sends land later than they are sent (a transaction layer, a
    /// real chain): what takes each line's transaction options, and says
    /// what became of each send.
    pub sends: Option<std::sync::Arc<dyn Sends>>,
    /// The requests for quotes open to the strategy, where clients send
    /// any.
    pub rfqs: Option<std::sync::Arc<dyn Rfqs>>,
    /// Prices from elsewhere (an exchange's book) the strategy reads as
    /// venues of their own, read-only: the live runtime's references.
    pub references: Option<std::sync::Arc<dyn References>>,
}

/// Prices from elsewhere, as read-only venues in `market.venues`: none
/// traded, no position, only a best bid and ask and their age.
pub trait References: Send + Sync {
    fn views(&self) -> Vec<mtx_kit::VenueView>;
}

/// How a component's market is read at each turn.
pub enum Reader {
    /// A Phoenix perp account, as a process bot's run loop reads it: the
    /// venue's mark, oracle and index, the position there, and a PnL
    /// estimated from the mark's moves.
    Perp(std::sync::Arc<mtx_kit::Trader>),
    /// A Phoenix perp account read some other way than through Hawkeye's
    /// views (the live runtime's account cache): the same numbers, the
    /// same PnL estimate.
    Live(std::sync::Arc<dyn PerpReads>),
    /// A wallet the session measures (a spot session): its reading at each
    /// turn ([`Wallet`]), valued at the oracle, and each market's top as
    /// its seat reads it.
    Wallet {
        /// The account when the session starts.
        start: Wallet,
    },
    /// A lending account the session measures: its value, and each
    /// reserve's supply, debt and numbers ([`Wallet::venues`]). Reserves
    /// have no book.
    Lending {
        /// The account when the session starts.
        start: Wallet,
    },
}

/// A perp market and an account's position on it, as [`Reader::Live`]
/// reads them.
#[async_trait]
pub trait PerpReads: Send + Sync {
    /// The mark, the index and the book's best bid and ask.
    async fn top(&self) -> Result<mtx_kit::Top, String>;
    async fn oracle_usd(&self) -> Result<f64, String>;
    async fn position(&self) -> Result<mtx_kit::Position, String>;
}

/// Why an order after a failed one in the same step wasn't sent: the
/// step's first failure says why the step's orders didn't go out.
pub const EARLIER_ORDER_FAILED: &str = "an earlier order of the step failed";

/// The seat among `seats` for a planned line's venue: the session's own
/// market's (the first) for none.
fn seat_in<'a>(seats: &'a [Box<dyn Seat>], venue: Option<&str>) -> Option<&'a dyn Seat> {
    match venue {
        None => seats.first(),
        Some(id) => seats.iter().find(|s| s.market() == id),
    }
    .map(|seat| seat.as_ref())
}

/// What a session reads of its own market at a step, before the views.
struct OwnRead {
    mark: f64,
    oracle: f64,
    index: f64,
    best_bid: Option<f64>,
    best_ask: Option<f64>,
    position: mtx_kit::Position,
}

impl OwnRead {
    /// A measured wallet's market, at the wallet's price: its position the
    /// base it holds, entered at the session's starting price.
    fn at_price(
        wallet: &Wallet,
        start: &Wallet,
        best_bid: Option<f64>,
        best_ask: Option<f64>,
    ) -> Self {
        let price = wallet.price_usd;
        Self {
            mark: price,
            oracle: price,
            index: price,
            best_bid,
            best_ask,
            position: mtx_kit::Position {
                size: wallet.base,
                entry_usd: start.price_usd,
            },
        }
    }
}

/// The signature a status of the step's sends is pooled under, if any: a
/// line's landed signature, or a raw send's from the raw desk's pool.
fn pooled_signature(status: &SendStatus, raw_pooled: &[(u32, String)]) -> Option<String> {
    match (&status.state, status.source) {
        (SendState::Landed(signature), _) => Some(signature.clone()),
        (SendState::Delayed, SendSource::Raw(id)) => raw_pooled
            .iter()
            .find(|(raw, _)| *raw == id)
            .map(|(_, signature)| signature.to_string()),
        _ => None,
    }
}

/// The atomic group a planned order is in, if any.
fn atomic_id(planned: &Planned) -> Option<u32> {
    planned.tx.and_then(|tx| tx.atomic)
}

/// A component playing one session: the strategy, and the markets the
/// host trades for it ([`Desk`]). Reads and sends as `mtx_kit::run` does
/// for a process bot.
pub struct WasmBot<R: RawDesk = NoRawSends> {
    /// `None` if the component didn't load: the session plays without it.
    strategy: Option<WasmStrategy<R>>,
    desk: Desk,
    session: mtx_kit::Session,
    pnl: f64,
    peak_pnl: f64,
    last_mark: Option<f64>,
    resting: (f64, f64),
    last: Option<mtx_kit::Market>,
    /// What became of the last sheet, and the planner's notes on it: the
    /// next step's market carries them.
    sends: Vec<SendStatus>,
    notes: Vec<String>,
    /// Under the transaction layer: sends still in its pool, with the
    /// signature each is known by. A later step's market says what became
    /// of each, once something did.
    pending: Vec<(SendStatus, String)>,
    /// Lines that carried transaction options, which no session applies
    /// yet.
    pub tx_options: u64,
    /// Why the strategy plays no more of the session, if it doesn't: it
    /// didn't load, or it trapped (Wasmtime won't enter its instance again).
    pub stopped: Option<String>,
    observer: Option<std::sync::Arc<dyn StepObserver>>,
    /// The account the session measured for the coming turn.
    account: Option<Account>,
    /// Where the mandate allows taking on the bot's market only to reduce:
    /// the check its orders there pass before they go out.
    taking: Option<TakingGuard>,
    /// The orders its last turn sent to the chain, the one that failed
    /// included.
    last_orders: u32,
    /// Where its last turn's time went.
    last_timings: TurnTimings,
}

/// Where a turn's time went, in microseconds: reading the market, the
/// strategy's `step`, and planning and sending its sheet.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TurnTimings {
    pub read_us: u64,
    pub call_us: u64,
    pub send_us: u64,
}

fn micros(since: std::time::Instant) -> u64 {
    u64::try_from(since.elapsed().as_micros()).unwrap_or(u64::MAX)
}

impl<R: RawDesk> WasmBot<R> {
    /// Loads the component and calls its `start`. `reads` answers its own
    /// `rpc` reads, and `raw` takes its raw sends, if the session has any.
    #[allow(clippy::too_many_arguments)]
    pub async fn start(
        path: &Path,
        reads: Box<dyn Reads>,
        clock: Clock,
        terms: Terms<'_>,
        desk: Desk,
        log: Box<dyn Write + Send>,
        observer: Option<std::sync::Arc<dyn StepObserver>>,
        raw: Option<R>,
    ) -> crate::Result<Self> {
        let Clock {
            seed,
            steps,
            slots_per_step,
            start_slot,
            vars,
            reports,
            step_deadline,
        } = clock;
        let (start_mark, start_oracle) = match &desk.reader {
            Reader::Perp(trader) => (trader.mark_usd().await?, trader.oracle_usd().await?),
            Reader::Live(reads) => {
                let read =
                    async { Ok::<_, String>((reads.top().await?.mark, reads.oracle_usd().await?)) };
                read.await.map_err(crate::Error::Live)?
            }
            Reader::Wallet { start } | Reader::Lending { start } => {
                (start.price_usd, start.price_usd)
            }
        };
        let session_fuel = (steps + 2).saturating_mul(FUEL_PER_STEP);
        let mut stopped = None;
        let mut strategy = match WasmStrategy::load(
            path,
            reads,
            log,
            seed,
            session_fuel,
            step_deadline,
        )
        .await
        {
            Ok(strategy) => {
                tracing::info!(interface = strategy.interface(), "strategy component");
                Some(strategy)
            }
            Err(error) => {
                tracing::warn!(%error, "the component didn't load; the session plays without it");
                stopped = Some(format!("didn't load: {error}"));
                None
            }
        };
        let session = mtx_kit::Session {
            steps,
            slots_per_step,
            start_slot,
            end_slot: terms.end_slot,
            symbol: desk.symbol.clone(),
            start_mark,
            start_oracle,
            cap: terms.cap,
            max_drawdown: terms.max_drawdown,
            mandate: terms.mandate.clone(),
            authority: desk.authority.clone(),
            vars,
            seed,
            venues: desk.markets.clone(),
            venue: desk.venue.clone(),
            reports: mtx_kit::Reports {
                fees: reports.fees || desk.sends.is_some(),
                ..reports
            },
            applies_tx_options: desk.sends.is_some(),
        };
        if let (Some(strategy), Some(raw)) = (strategy.as_mut(), raw) {
            strategy.enable_raw_sends(raw);
        }
        if let Some(strategy) = strategy.as_mut() {
            let failed = match strategy.start(&session).await {
                Outcome::Done(()) => None,
                Outcome::Failed(e) => {
                    tracing::warn!(error = %e, "the strategy's start failed; it plays on");
                    Some(e)
                }
                Outcome::Trapped(e) => {
                    tracing::warn!(error = %e, "the strategy's start trapped; it plays no more of the session");
                    stopped = Some(format!("trapped in start: {e}"));
                    Some(format!("trapped: {e}"))
                }
            };
            if let Some(observer) = &observer {
                let printed = strategy.last_call().0.to_vec();
                tokio::task::block_in_place(|| {
                    observer.started(&session, &printed, failed.as_deref())
                });
            }
        } else if let (Some(observer), Some(reason)) = (&observer, &stopped) {
            tokio::task::block_in_place(|| observer.started(&session, &[], Some(reason)));
        }
        let taking = desk
            .markets
            .first()
            .and_then(|market| market.id.parse().ok())
            .and_then(|instrument| terms.mandate.allowance(&instrument))
            .and_then(TakingGuard::new);
        Ok(Self {
            strategy,
            desk,
            session,
            pnl: 0.0,
            peak_pnl: 0.0,
            last_mark: None,
            resting: (0.0, 0.0),
            last: None,
            sends: Vec::new(),
            notes: Vec::new(),
            pending: Vec::new(),
            tx_options: 0,
            stopped,
            observer,
            account: None,
            taking,
            last_orders: 0,
            last_timings: TurnTimings::default(),
        })
    }

    /// The touch the session read for the coming turn, net of the bot's own
    /// orders, for the taking check.
    pub fn touched(&mut self, touch: Option<Touch>) {
        if let Some(guard) = self.taking.as_mut() {
            guard.touch(touch);
        }
    }

    /// The orders the taking check refused, where the mandate has one.
    pub fn refused_takes(&self) -> Option<u64> {
        self.taking.as_ref().map(|guard| guard.refused)
    }

    /// The market at `step`, as the desk reads it: `None` if it couldn't.
    /// `wallet` is the session's reading of a measured wallet.
    async fn market(
        &mut self,
        step: &mtx_core::Step,
        wallet: Option<&Wallet>,
    ) -> Option<mtx_kit::Market> {
        let (k, slot, last) = (step.k, step.slot, step.last);
        self.resolve();
        let read = self.read_own(k, wallet).await?;
        self.peak_pnl = self.peak_pnl.max(self.pnl);
        let (mut venues, mut fills) = mtx_kit::venue_views(
            &self.session.venues,
            self.session.reports,
            read.best_bid,
            read.best_ask,
            read.position,
            &step.tape,
            &step.depth,
        );
        // Its RFQ fills, which settle off the book.
        if let Some(board) = &self.desk.rfqs {
            fills.extend(board.take_fills());
        }
        match (&self.desk.reader, wallet) {
            (Reader::Perp(_) | Reader::Live(_), _) => {
                Self::perp_views(&self.desk.seats, &mut venues).await
            }
            (Reader::Lending { .. }, Some(wallet)) => self.lending_views(&mut venues, wallet),
            (Reader::Lending { .. }, None) | (Reader::Wallet { .. }, _) => {}
        }
        if let Some(references) = &self.desk.references {
            venues.extend(references.views());
        }
        if matches!(self.desk.reader, Reader::Wallet { .. }) {
            Self::wallet_views(&self.desk.seats, &mut venues, read.position).await;
        }
        Some(mtx_kit::Market {
            k,
            slot,
            last,
            steps_left: self.session.steps.saturating_sub(k + 1),
            slots_left: self.session.end_slot.saturating_sub(slot),
            mark: read.mark,
            oracle: read.oracle,
            index: read.index,
            best_bid: read.best_bid,
            best_ask: read.best_ask,
            position: read.position,
            cap: self.session.cap,
            max_drawdown: self.session.max_drawdown,
            resting_bids: self.resting.0,
            resting_asks: self.resting.1,
            pnl: self.pnl,
            peak_pnl: self.peak_pnl,
            drawdown: self.peak_pnl - self.pnl,
            tape: step.tape.clone(),
            depth: step.depth.clone(),
            unix_timestamp: step.unix_timestamp,
            venues,
            sends: std::mem::take(&mut self.sends),
            fills,
            rfqs: self
                .desk
                .rfqs
                .as_ref()
                .map(|board| board.open())
                .unwrap_or_default(),
            notes: std::mem::take(&mut self.notes),
            fees: self.desk.sends.as_ref().map(|sends| sends.fee_view()),
            trigger: step.trigger.clone(),
        })
    }

    /// The session's own market at step `k`, and its PnL brought up to date:
    /// `None`, said why, if it couldn't be read.
    async fn read_own(&mut self, k: u64, wallet: Option<&Wallet>) -> Option<OwnRead> {
        match &self.desk.reader {
            Reader::Perp(_) | Reader::Live(_) => self.read_perp(k).await,
            Reader::Lending { start } => {
                let Some(wallet) = wallet else {
                    tracing::warn!(step = k, "no reading of the account; skipping the step");
                    return None;
                };
                self.pnl = wallet.value_usd - start.value_usd;
                Some(OwnRead::at_price(wallet, start, None, None))
            }
            Reader::Wallet { start } => {
                let Some(wallet) = wallet else {
                    tracing::warn!(step = k, "no reading of the wallet; skipping the step");
                    return None;
                };
                let Some(Ok((best_bid, best_ask))) = (match self.desk.seats.first() {
                    Some(seat) => Some(seat.top().await),
                    None => None,
                }) else {
                    tracing::warn!(step = k, "couldn't read the book; skipping the step");
                    return None;
                };
                self.pnl = wallet.value_usd - start.value_usd;
                Some(OwnRead::at_price(wallet, start, best_bid, best_ask))
            }
        }
    }

    /// A perp session's market: its top, oracle and position, the PnL marked
    /// to the move since the last read.
    async fn read_perp(&mut self, k: u64) -> Option<OwnRead> {
        let read = match &self.desk.reader {
            Reader::Perp(trader) => (
                trader.top().await.map_err(|e| e.to_string()),
                trader.oracle_usd().await.map_err(|e| e.to_string()),
                trader.position().await.map_err(|e| e.to_string()),
            ),
            Reader::Live(reads) => (
                reads.top().await,
                reads.oracle_usd().await,
                reads.position().await,
            ),
            Reader::Lending { .. } | Reader::Wallet { .. } => {
                unreachable!("read_own reads a perp only with a perp reader")
            }
        };
        let (top, oracle, position) = match read {
            (Ok(top), Ok(oracle), Ok(position)) => (top, oracle, position),
            (top, oracle, position) => {
                tracing::warn!(step = k, mark = ?top.err(), oracle = ?oracle.err(), position = ?position.err(), "couldn't read the market; skipping the step");
                return None;
            }
        };
        if let Some(previous) = self.last_mark {
            self.pnl += position.size * (top.mark - previous);
        }
        self.last_mark = Some(top.mark);
        Some(OwnRead {
            mark: top.mark,
            oracle,
            index: top.index,
            best_bid: top.best_bid,
            best_ask: top.best_ask,
            position,
        })
    }

    /// A perp account's other markets (a session on several): each seat's
    /// own position and top. A seat that can't say leaves its view as the
    /// step's tape and depth made it: the strategy sees no position there.
    /// Takes the seats, not the bot: a future that holds `&self` across its
    /// reads would need the bot shared between threads.
    async fn perp_views(seats: &[Box<dyn Seat>], venues: &mut [mtx_kit::VenueView]) {
        for view in venues.iter_mut().skip(1) {
            let Some(seat) = seats.iter().skip(1).find(|s| s.market() == view.id) else {
                continue;
            };
            if let Ok(size) = seat.position().await {
                view.position = Some(mtx_kit::Position {
                    size,
                    ..Default::default()
                });
            }
            if view.best_bid.is_none()
                && view.best_ask.is_none()
                && let Ok((bid, ask)) = seat.top().await
            {
                (view.best_bid, view.best_ask) = (bid, ask);
            }
        }
    }

    /// A measured wallet's other markets: the same SOL held, and each
    /// reading's lending position, or a pool's prices.
    fn lending_views(&self, venues: &mut [mtx_kit::VenueView], wallet: &Wallet) {
        for view in venues {
            let Some(reading) = wallet.venues.iter().find(|r| r.id == view.id) else {
                continue;
            };
            view.position = None;
            view.stats = reading.stats.clone();
            // A pool of the account's (an amm session's) has prices
            // and no lending position.
            let pool = self
                .session
                .venues
                .iter()
                .any(|v| v.id == view.id && v.kind == mtx_kit::MarketKind::Amm);
            if pool {
                (view.best_bid, view.best_ask) = (reading.best_bid, reading.best_ask);
                continue;
            }
            view.lending = Some(mtx_kit::LendingPosition {
                supplied: reading.supplied,
                borrowed: reading.borrowed,
                health: reading.health,
            });
        }
    }

    /// A wallet session's other markets: the wallet's position, and each
    /// seat's own top where the step's depth didn't give one (a seat that
    /// can't say leaves it empty).
    async fn wallet_views(
        seats: &[Box<dyn Seat>],
        venues: &mut [mtx_kit::VenueView],
        position: mtx_kit::Position,
    ) {
        for view in venues.iter_mut().skip(1) {
            view.position = Some(position);
            if view.best_bid.is_some() || view.best_ask.is_some() {
                continue;
            }
            if let Some(seat) = seats.iter().find(|s| s.market() == view.id)
                && let Ok((bid, ask)) = seat.top().await
            {
                (view.best_bid, view.best_ask) = (bid, ask);
            }
        }
    }

    /// What became of the sends that hadn't landed yet: each
    /// that landed, failed or expired since goes into the next market's
    /// sends, with its slot, fee and tip.
    fn resolve(&mut self) {
        let Some(tx) = &self.desk.sends else {
            return;
        };
        let mut still = Vec::new();
        for (mut status, signature) in std::mem::take(&mut self.pending) {
            status.state = match tx.fate(&signature) {
                Some(Fate::Landed {
                    slot,
                    ok,
                    fee,
                    tip,
                    error,
                    signature,
                }) => {
                    status.slot = Some(slot);
                    status.fee = Some(fee);
                    status.tip = Some(tip);
                    if ok {
                        SendState::Landed(signature)
                    } else {
                        SendState::Failed {
                            reason: error.unwrap_or_default(),
                            signature: Some(signature),
                        }
                    }
                }
                Some(Fate::Failed(reason)) => SendState::Failed {
                    reason,
                    signature: None,
                },
                Some(Fate::Expired) => SendState::Expired,
                Some(Fate::Pending) | None => {
                    still.push((status, signature));
                    continue;
                }
            };
            self.sends.push(status);
        }
        self.pending = still;
    }

    /// The seat for a planned line's venue: the session's own market's for
    /// none.
    fn seat(&self, venue: Option<&str>) -> Option<&dyn Seat> {
        seat_in(&self.desk.seats, venue)
    }

    /// The strategy's turn at `step`: read the market, call `step`, cut the
    /// sheet to the mandate's cap and send each line to its market, in
    /// order, stopping at the first order that fails. An atomic group goes
    /// as one transaction, with its first line's options; each of its lines
    /// gets the group's status, and a failed group stops the step as a
    /// failed line does. The step's turn data,
    /// if the scenario reports any, goes into the market, keyed by market;
    /// `wallet` is the session's reading of a measured wallet. Returns
    /// whether the strategy answered.
    pub async fn turn(&mut self, step: &mtx_core::Step, wallet: Option<&Wallet>) -> bool {
        let k = step.k;
        if self.stopped.is_some() {
            return false;
        }
        self.last_timings = TurnTimings::default();
        let started = std::time::Instant::now();
        let read = tracing::info_span!("strategy.market", k);
        let Some(market) = tracing::Instrument::instrument(self.market(step, wallet), read).await
        else {
            return false;
        };
        self.last_timings.read_us = micros(started);
        let called = std::time::Instant::now();
        let Some(strategy) = self.strategy.as_mut() else {
            self.last = Some(market);
            return false;
        };
        if let Some(raw) = strategy.raw_sender() {
            raw.begin(k, self.account).await;
        }
        let call = tracing::info_span!("strategy.call", k);
        let outcome = tracing::Instrument::instrument(strategy.step(&market), call).await;
        self.last_timings.call_us = micros(called);
        let sending = std::time::Instant::now();
        let raw = strategy.raw_sender();
        let dropped = match &outcome {
            Outcome::Done(_) => raw.map(|raw| {
                raw.close();
                Vec::new()
            }),
            Outcome::Failed(_) => raw.map(|raw| raw.discard("the step failed")),
            Outcome::Trapped(_) => raw.map(|raw| raw.discard("the strategy trapped")),
        };
        self.sends.extend(dropped.into_iter().flatten());
        let mut trace = self.observer.as_ref().map(|_| {
            let (printed, fuel, rpc_calls) = strategy.last_call();
            StepTrace::of_call(
                &market,
                printed,
                fuel,
                rpc_calls,
                self.session.steps,
                self.account,
            )
        });
        let lines = match outcome {
            Outcome::Done(lines) => lines,
            Outcome::Failed(e) => {
                tracing::warn!(step = k, error = %e, "the strategy failed; no orders this step");
                self.last = Some(market);
                self.observe(trace.map(|t| StepTrace {
                    failed: Some(e),
                    ..t
                }));
                return true;
            }
            Outcome::Trapped(e) => {
                self.stop_trapped(k, market, trace, &e);
                return false;
            }
        };
        let planned = self.plan(k, lines, &market, trace.as_mut());
        self.last = Some(market);
        let planned_count = planned.len();
        self.send_sheet(k, planned, trace.as_mut()).await;
        assert!(
            self.sends.len() >= planned_count,
            "every planned line has a status"
        );
        let from = self.sends.len() - planned_count;
        self.flush_after_sheet(from).await;
        self.last_timings.send_us = micros(sending);
        self.observe(trace);
        true
    }

    /// The strategy trapped at step `k`: it plays no more of the session.
    fn stop_trapped(&mut self, k: u64, market: mtx_kit::Market, trace: Option<StepTrace>, e: &str) {
        tracing::warn!(step = k, error = %e, "the strategy trapped; it plays no more of the session");
        self.stopped = Some(format!("trapped at step {k}: {e}"));
        self.last = Some(market);
        self.observe(trace.map(|t| StepTrace {
            failed: Some(format!("trapped: {e}; it plays no more of the session")),
            ..t
        }));
    }

    /// The strategy's `lines` as orders, cut to the mandate's caps on what
    /// the session executes: the planned sheet, its notes kept for the next
    /// market and the trace, and the taking guard started on `market`.
    fn plan(
        &mut self,
        k: u64,
        lines: Vec<(mtx_kit::Intent, mtx_kit::LineOptions)>,
        market: &mtx_kit::Market,
        trace: Option<&mut StepTrace>,
    ) -> Vec<Planned> {
        assert!(
            lines.len() <= crate::host::LINES_PER_STEP_MAX,
            "the host refuses a step past its lines' bound"
        );
        let mut orders = mtx_kit::Orders::new();
        for (intent, options) in lines {
            if options.tx.is_some() {
                self.tx_options += 1;
            }
            orders.push(intent, options);
        }
        // Only what the session executes counts toward the cap.
        // Each venue's lines are cut to its own cap in the mandate.
        let (planned, notes) =
            mtx_kit::plan_sheet_under(&orders, market, &self.session.mandate, |venue, action| {
                self.seat(venue).is_some_and(|seat| seat.executes(action))
            });
        for note in &notes {
            tracing::warn!(step = k, "{note}");
        }
        if let Some(trace) = trace {
            (trace.intents, trace.options) = orders.into_lines().into_iter().unzip();
            trace.notes = notes.clone();
        }
        self.notes = notes;
        if let Some(guard) = self.taking.as_mut() {
            guard.begin(market);
        }
        planned
    }

    /// Send the planned sheet, in order: each line alone or each atomic
    /// group as one transaction, none after the first that fails. Every line
    /// gets a status, a group's lines the group's; what reached the chain
    /// updates the ledger of resting orders.
    async fn send_sheet(
        &mut self,
        k: u64,
        planned: Vec<Planned>,
        mut trace: Option<&mut StepTrace>,
    ) {
        let own = self.session.venues[0].id.clone();
        // The orders that reached the chain, the one that failed included:
        // the ledger of what rests only over-counts.
        let mut tried = Vec::new();
        let mut failed = false;
        let mut planned = planned.into_iter().peekable();
        while let Some(first) = planned.next() {
            // A line alone, or an atomic group: the planner keeps a group's
            // lines next to each other.
            let mut unit = vec![first];
            if let Some(id) = atomic_id(&unit[0]) {
                while let Some(next) = planned.next_if(|p| atomic_id(p) == Some(id)) {
                    unit.push(next);
                }
            }
            let (state, reached) = if failed {
                (SendState::NotSent(EARLIER_ORDER_FAILED.into()), false)
            } else if unit.len() == 1 && atomic_id(&unit[0]).is_none() {
                self.send_line(k, &unit[0], &own).await
            } else {
                self.send_group(k, &unit, &own).await
            };
            if reached {
                tried.extend(unit.iter().map(|p| p.action.clone()));
                if let SendState::Failed { reason, .. } = &state {
                    tracing::warn!(step = k, lines = unit.len(), error = %reason, "order failed; the step's other orders aren't sent");
                    failed = true;
                }
            }
            // Every line of a group gets the group's status.
            for planned in unit {
                let Planned {
                    line,
                    venue,
                    client_id,
                    action,
                    expires_slot,
                    ..
                } = planned;
                if let Some(trace) = trace.as_deref_mut() {
                    trace.sent.push((action.clone(), state.clone()));
                }
                self.sends.push(SendStatus {
                    step: k,
                    source: SendSource::Line(
                        u32::try_from(line)
                            .expect("a step's lines are bounded by LINES_PER_STEP_MAX"),
                    ),
                    venue: venue.unwrap_or_else(|| own.clone()),
                    client_id,
                    order: Some(action),
                    expires_slot,
                    state: state.clone(),
                    slot: None,
                    fee: None,
                    tip: None,
                });
            }
        }
        self.last_orders = u32::try_from(tried.len()).unwrap_or(u32::MAX);
        self.resting = mtx_kit::resting_after(self.resting.0, self.resting.1, &tried);
    }

    /// After the sheet, whatever became of it: the raw sends, then the
    /// step's bundles, a failed bundle failing what it carried. Under the
    /// transaction layer, what the step sent from `from` on is in the pool:
    /// delayed, until a later step says what became of it.
    async fn flush_after_sheet(&mut self, from: usize) {
        let mut raw_pooled = Vec::new();
        if let (Some(raw), Reader::Perp(trader)) = (
            self.strategy.as_mut().and_then(WasmStrategy::raw_sender),
            &self.desk.reader,
        ) {
            self.sends.extend(raw.flush(&trader.keypair).await);
            raw_pooled = raw.take_pooled();
        }
        let Some(pool) = &self.desk.sends else {
            return;
        };
        pool.set_options(None);
        for (signatures, failed) in pool.flush_bundles().await {
            let Some(reason) = failed else {
                continue;
            };
            for status in &mut self.sends[from..] {
                if pooled_signature(status, &raw_pooled).is_some_and(|s| signatures.contains(&s)) {
                    status.state = SendState::Failed {
                        reason: reason.clone(),
                        signature: None,
                    };
                }
            }
        }
        for status in &mut self.sends[from..] {
            if let Some(signature) = pooled_signature(status, &raw_pooled) {
                status.state = SendState::Delayed;
                self.pending.push((status.clone(), signature));
            }
        }
    }

    /// Whether `planned`, on `own` market, is refused by the taking guard,
    /// and why.
    fn refused_take(&mut self, planned: &Planned, own: &str) -> Option<String> {
        let on_own = planned.venue.as_ref().is_none_or(|venue| venue == own);
        match self.taking.as_mut().filter(|_| on_own) {
            Some(guard) => guard.check(&planned.action, planned.reduce_only).err(),
            None => None,
        }
    }

    /// Send one line alone through its market's seat, after the taking
    /// check: what became of it, and whether it reached the chain.
    async fn send_line(&mut self, k: u64, planned: &Planned, own: &str) -> (SendState, bool) {
        if let Some(reason) = self.refused_take(planned, own) {
            tracing::info!(step = k, action = ?planned.action, "{reason}");
            return (SendState::NotSent(reason), false);
        }
        let Some(seat) = self.seat(planned.venue.as_deref()) else {
            let venue = planned.venue.as_deref().unwrap_or(own);
            return (
                SendState::NotSent(format!("{venue} isn't traded in this session")),
                false,
            );
        };
        if let Some(pool) = &self.desk.sends {
            pool.set_options(planned.tx);
        }
        let order = tracing::info_span!(
            "strategy.order",
            k,
            venue = seat.market(),
            action = ?planned.action,
        );
        let sent = tracing::Instrument::instrument(seat.send(planned), order).await;
        (sent.state, sent.sent)
    }

    /// Send an atomic group as one transaction ([`atomic::build`]), with its
    /// first line's options, after every line's taking check: what became
    /// of it, and whether it reached the chain. A group that can't be built
    /// isn't sent.
    async fn send_group(&mut self, k: u64, group: &[Planned], own: &str) -> (SendState, bool) {
        assert!(!group.is_empty(), "a group has lines");
        for planned in group {
            if let Some(reason) = self.refused_take(planned, own) {
                tracing::info!(step = k, line = planned.line, "{reason}");
                return (
                    SendState::NotSent(format!("line {}: {reason}", planned.line)),
                    false,
                );
            }
        }
        let applies = self.desk.sends.is_some();
        let options = group[0].tx.filter(|_| applies);
        let seats = &self.desk.seats;
        let built = atomic::build(group, |venue| seat_in(seats, venue), options).await;
        let (plan, seat) = match built {
            Ok(built) => built,
            Err(reason) => {
                tracing::info!(
                    step = k,
                    lines = group.len(),
                    "atomic group not sent: {reason}"
                );
                return (SendState::NotSent(reason), false);
            }
        };
        if let Some(pool) = &self.desk.sends {
            pool.set_options(group[0].tx);
        }
        let span =
            tracing::info_span!("strategy.group", k, lines = group.len(), label = %plan.label());
        let sent = tracing::Instrument::instrument(seat.send_plan(&plan), span).await;
        (sent.state, sent.sent)
    }

    /// The orders its last turn sent to the chain.
    pub fn last_orders(&self) -> u32 {
        self.last_orders
    }

    /// Where its last turn's time went.
    pub fn last_timings(&self) -> TurnTimings {
        self.last_timings
    }

    /// The account as the session measured it, for the next turn's trace.
    pub fn measured(&mut self, account: Account) {
        self.account = Some(account);
    }

    /// Hand a step's trace to the observer, if there is one.
    fn observe(&self, trace: Option<StepTrace>) {
        if let (Some(observer), Some(trace)) = (&self.observer, trace) {
            tokio::task::block_in_place(|| observer.step(&trace));
        }
    }

    /// The session is over: the strategy's `end`.
    pub async fn end(&mut self) {
        let last = self.last.take();
        if self.stopped.is_some() {
            return;
        }
        if let Some(strategy) = self.strategy.as_mut()
            && let Outcome::Trapped(e) = strategy.end(last.as_ref()).await
        {
            tracing::warn!(error = %e, "the strategy's end trapped");
        }
    }

    /// The market the last turn handed the strategy, if it had one.
    pub fn last_market(&self) -> Option<&mtx_kit::Market> {
        self.last.as_ref()
    }

    /// Cancel every order the strategy's seats have resting: what a host
    /// does when it stops stepping it. Each seat that fails says why.
    pub async fn cancel_all(&mut self) -> Vec<String> {
        let mut failed = Vec::new();
        for seat in &self.desk.seats {
            if let Err(error) = seat.cancel_all().await {
                failed.push(format!("{}: {error}", seat.market()));
            }
        }
        failed
    }

    /// Close the position on the strategy's own market with a market
    /// order, no worse than `slippage` (a fraction) past the touch:
    /// what a host does when a strategy is paused flat. Why it couldn't,
    /// if it couldn't.
    pub async fn flatten(&mut self, slippage: f64) -> Result<(), String> {
        assert!((0.0..1.0).contains(&slippage), "a slippage under 100%");
        let Some(seat) = self.desk.seats.first() else {
            return Err("no seat".into());
        };
        let position = seat.position().await.map_err(|e| e.to_string())?;
        if position == 0.0 {
            return Ok(());
        }
        let (bid, ask) = seat.top().await.map_err(|e| e.to_string())?;
        let (side, limit) = if position > 0.0 {
            let bid = bid.ok_or("no bid to sell into")?;
            (mtx_kit::Side::Ask, bid * (1.0 - slippage))
        } else {
            let ask = ask.ok_or("no ask to buy from")?;
            (mtx_kit::Side::Bid, ask * (1.0 + slippage))
        };
        seat.take(side, position.abs(), limit)
            .await
            .map(|_| ())
            .map_err(|e| e.to_string())
    }

    /// The fuel the session spent, and the most one call spent.
    pub fn fuel(&self) -> (u64, u64) {
        self.strategy
            .as_ref()
            .map_or((0, 0), |s| (s.fuel_used, s.fuel_max_call))
    }
}
