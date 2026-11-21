//! A strategy component playing one session: the host's side of its
//! steps. Each turn the desk reads the market, calls the strategy's `step`
//! (`crate::host`), cuts its sheet to the mandate, checks it, and sends
//! each order through the seat on its market (`crate::venue`).
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
        let (mark, oracle, index, best_bid, best_ask, position) = match &self.desk.reader {
            Reader::Perp(_) | Reader::Live(_) => {
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
                    _ => unreachable!("the arm matched a perp reader"),
                };
                let (top, oracle, position) = match read {
                    (Ok(top), Ok(oracle), Ok(position)) => (top, oracle, position),
                    (top, oracle, position) => {
                        tracing::warn!(step = k, mark = ?top.err(), oracle = ?oracle.err(), position = ?position.err(), "couldn't read the market; skipping the step");
                        return None;
                    }
                };
                let mark = top.mark;
                if let Some(previous) = self.last_mark {
                    self.pnl += position.size * (mark - previous);
                }
                self.last_mark = Some(mark);
                (
                    mark,
                    oracle,
                    top.index,
                    top.best_bid,
                    top.best_ask,
                    position,
                )
            }
            Reader::Lending { start } => {
                let Some(wallet) = wallet else {
                    tracing::warn!(step = k, "no reading of the account; skipping the step");
                    return None;
                };
                self.pnl = wallet.value_usd - start.value_usd;
                let position = mtx_kit::Position {
                    size: wallet.base,
                    entry_usd: start.price_usd,
                };
                let price = wallet.price_usd;
                (price, price, price, None, None, position)
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
                let position = mtx_kit::Position {
                    size: wallet.base,
                    entry_usd: start.price_usd,
                };
                let price = wallet.price_usd;
                (price, price, price, best_bid, best_ask, position)
            }
        };
        self.peak_pnl = self.peak_pnl.max(self.pnl);
        let (mut venues, mut fills) = mtx_kit::venue_views(
            &self.session.venues,
            self.session.reports,
            best_bid,
            best_ask,
            position,
            &step.tape,
            &step.depth,
        );
        // Its RFQ fills, which settle off the book.
        if let Some(board) = &self.desk.rfqs {
            fills.extend(board.take_fills());
        }
        // A perp account's other markets (a session on several): each
        // seat's own position and top.
        if matches!(self.desk.reader, Reader::Perp(_) | Reader::Live(_)) {
            for view in venues.iter_mut().skip(1) {
                let Some(seat) = self
                    .desk
                    .seats
                    .iter()
                    .skip(1)
                    .find(|s| s.market() == view.id)
                else {
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
        // A measured wallet's other markets: the same SOL held, and each
        // seat's own top, where the step's depth didn't give one.
        if let (Reader::Lending { .. }, Some(wallet)) = (&self.desk.reader, wallet) {
            for view in &mut venues {
                let Some(reading) = wallet.venues.iter().find(|r| r.id == view.id) else {
                    continue;
                };
                view.position = None;
                view.lending = Some(mtx_kit::LendingPosition {
                    supplied: reading.supplied,
                    borrowed: reading.borrowed,
                    health: reading.health,
                });
                view.stats = reading.stats.clone();
            }
        }
        if matches!(self.desk.reader, Reader::Wallet { .. }) {
            for view in venues.iter_mut().skip(1) {
                view.position = Some(position);
                if view.best_bid.is_some() || view.best_ask.is_some() {
                    continue;
                }
                if let Some(seat) = self.desk.seats.iter().find(|s| s.market() == view.id)
                    && let Ok((bid, ask)) = seat.top().await
                {
                    (view.best_bid, view.best_ask) = (bid, ask);
                }
            }
        }
        Some(mtx_kit::Market {
            k,
            slot,
            last,
            steps_left: self.session.steps.saturating_sub(k + 1),
            slots_left: self.session.end_slot.saturating_sub(slot),
            mark,
            oracle,
            index,
            best_bid,
            best_ask,
            position,
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
        })
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
        match venue {
            None => self.desk.seats.first(),
            Some(id) => self.desk.seats.iter().find(|s| s.market() == id),
        }
        .map(|seat| seat.as_ref())
    }

    /// The strategy's turn at `step`: read the market, call `step`, cut the
    /// sheet to the mandate's cap and send each line to its market, in
    /// order, stopping at the first order that fails. The step's turn data,
    /// if the scenario reports any, goes into the market, keyed by market;
    /// `wallet` is the session's reading of a measured wallet. Returns
    /// whether the strategy answered.
    pub async fn turn(&mut self, step: &mtx_core::Step, wallet: Option<&Wallet>) -> bool {
        let k = step.k;
        if self.stopped.is_some() {
            return false;
        }
        let read = tracing::info_span!("strategy.market", k);
        let Some(market) = tracing::Instrument::instrument(self.market(step, wallet), read).await
        else {
            return false;
        };
        let Some(strategy) = self.strategy.as_mut() else {
            self.last = Some(market);
            return false;
        };
        if let Some(raw) = strategy.raw_sender() {
            raw.begin(k, self.account).await;
        }
        let call = tracing::info_span!("strategy.call", k);
        let outcome = tracing::Instrument::instrument(strategy.step(&market), call).await;
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
            StepTrace {
                steps: self.session.steps,
                market: market.clone(),
                printed: printed.to_vec(),
                failed: None,
                intents: Vec::new(),
                options: Vec::new(),
                notes: Vec::new(),
                sent: Vec::new(),
                fuel,
                rpc_calls,
                account: self.account,
            }
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
                tracing::warn!(step = k, error = %e, "the strategy trapped; it plays no more of the session");
                self.stopped = Some(format!("trapped at step {k}: {e}"));
                self.last = Some(market);
                self.observe(trace.map(|t| StepTrace {
                    failed: Some(format!("trapped: {e}; it plays no more of the session")),
                    ..t
                }));
                return false;
            }
        };
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
            mtx_kit::plan_sheet_under(&orders, &market, &self.session.mandate, |venue, action| {
                self.seat(venue).is_some_and(|seat| seat.executes(action))
            });
        for note in &notes {
            tracing::warn!(step = k, "{note}");
        }
        if let Some(trace) = trace.as_mut() {
            (trace.intents, trace.options) = orders.into_lines().into_iter().unzip();
            trace.notes = notes.clone();
        }
        self.notes = notes;
        if let Some(guard) = self.taking.as_mut() {
            guard.begin(&market);
        }
        self.last = Some(market);
        let own = self.session.venues[0].id.clone();
        // The orders that reached the chain, the one that failed included:
        // the ledger of what rests only over-counts.
        let mut tried = Vec::new();
        let mut failed = false;
        let planned_count = planned.len();
        for planned in planned {
            let on_own = planned.venue.as_ref().is_none_or(|venue| *venue == own);
            let refused = match self.taking.as_mut().filter(|_| on_own && !failed) {
                Some(guard) => guard.check(&planned.action, planned.reduce_only).err(),
                None => None,
            };
            let state = if failed {
                SendState::NotSent(EARLIER_ORDER_FAILED.into())
            } else if let Some(reason) = refused {
                tracing::info!(step = k, action = ?planned.action, "{reason}");
                SendState::NotSent(reason)
            } else if let Some(seat) = self.seat(planned.venue.as_deref()) {
                if let Some(pool) = &self.desk.sends {
                    pool.set_options(planned.tx);
                }
                let order = tracing::info_span!(
                    "strategy.order",
                    k,
                    venue = seat.market(),
                    action = ?planned.action,
                );
                let sent = tracing::Instrument::instrument(seat.send(&planned), order).await;
                if sent.sent {
                    tried.push(planned.action.clone());
                    if let SendState::Failed { reason, .. } = &sent.state {
                        tracing::warn!(step = k, action = ?planned.action, error = %reason, "order failed; the step's other orders aren't sent");
                        failed = true;
                    }
                }
                sent.state
            } else {
                SendState::NotSent(format!(
                    "{} isn't traded in this session",
                    planned.venue.as_deref().unwrap_or(&own)
                ))
            };
            let Planned {
                line,
                venue,
                client_id,
                action,
                expires_slot,
                ..
            } = planned;
            if let Some(trace) = trace.as_mut() {
                trace.sent.push((action.clone(), state.clone()));
            }
            self.sends.push(SendStatus {
                step: k,
                source: SendSource::Line(line as u32),
                venue: venue.unwrap_or_else(|| own.clone()),
                client_id,
                order: Some(action),
                expires_slot,
                state,
                slot: None,
                fee: None,
                tip: None,
            });
        }
        self.last_orders = u32::try_from(tried.len()).unwrap_or(u32::MAX);
        self.resting = mtx_kit::resting_after(self.resting.0, self.resting.1, &tried);
        let from = self.sends.len() - planned_count;
        // Raw sends go after the sheet, whatever became of it.
        let mut raw_pooled = Vec::new();
        if let (Some(raw), Reader::Perp(trader)) = (
            self.strategy.as_mut().and_then(WasmStrategy::raw_sender),
            &self.desk.reader,
        ) {
            self.sends.extend(raw.flush(&trader.keypair).await);
            raw_pooled = raw.take_pooled();
        }
        // The step's bundles go once the sheet and the raw sends are all in
        // them.
        if let Some(pool) = &self.desk.sends {
            pool.set_options(None);
            for (signatures, failed) in pool.flush_bundles().await {
                let Some(reason) = failed else {
                    continue;
                };
                for status in &mut self.sends[from..] {
                    let signature = match (&status.state, status.source) {
                        (SendState::Landed(signature), _) => Some(signature.clone()),
                        (SendState::Delayed, SendSource::Raw(id)) => raw_pooled
                            .iter()
                            .find(|(raw, _)| *raw == id)
                            .map(|(_, signature)| signature.to_string()),
                        _ => None,
                    };
                    if signature.is_some_and(|s| signatures.contains(&s)) {
                        status.state = SendState::Failed {
                            reason: reason.clone(),
                            signature: None,
                        };
                    }
                }
            }
        }
        // Under the transaction layer, what the step sent is in the pool:
        // delayed, until a later step says what became of it.
        if self.desk.sends.is_some() {
            for status in &mut self.sends[from..] {
                let signature = match (&status.state, status.source) {
                    (SendState::Landed(signature), _) => Some(signature.clone()),
                    (SendState::Delayed, SendSource::Raw(id)) => raw_pooled
                        .iter()
                        .find(|(raw, _)| *raw == id)
                        .map(|(_, signature)| signature.to_string()),
                    _ => None,
                };
                if let Some(signature) = signature {
                    status.state = SendState::Delayed;
                    self.pending.push((status.clone(), signature));
                }
            }
        }
        self.observe(trace);
        true
    }

    /// The orders its last turn sent to the chain.
    pub fn last_orders(&self) -> u32 {
        self.last_orders
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
