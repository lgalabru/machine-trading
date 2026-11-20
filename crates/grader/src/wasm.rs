//! A strategy component playing one session
//! (`docs/rfd/0002-wasm-strategies.md`): a bot directory with a `bot.wasm`
//! is a component of the `mtx:strategy` world, which the grader calls
//! instead of starting a process. The host is `mtx_runtime::host`; this is
//! the session's side: reading the market, planning the sheet and sending
//! it through the session's seats.

use std::{io::Write, path::Path};

use mtx_kit::{Planned, SendSource, SendState, SendStatus};
pub use mtx_runtime::host::{
    BOT_WASM, FUEL_PER_CALL, FUEL_PER_STEP, HttpReads, MEMORY_BYTES, Outcome, component_in,
};

use crate::txlayer::engine::Fate;

/// A strategy component whose raw sends the session's sender takes.
pub type WasmStrategy = mtx_runtime::host::WasmStrategy<crate::raw::RawSender>;

/// The account at a step, as the session measures it (not the strategy's
/// estimate): taken before the strategy's turn, so the position change is
/// what filled during the previous step.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Account {
    pub value_usd: f64,
    pub pnl_usd: f64,
    /// Below the session's highest value so far; in a net-scored session,
    /// net of what holding the start would have gained.
    pub drawdown_usd: f64,
    pub position: f64,
    /// Since the step before: the net of what filled meanwhile.
    pub position_change: f64,
    /// The scenario's planned level at the step.
    pub level_usd: f64,
    /// How far trades have the book from the scenario's level, in basis
    /// points, where the scenario has an `impact`.
    pub impact_bps: Option<f64>,
    /// What traded on the market since the step before, between anyone,
    /// where the session reads it.
    pub trades: Option<crate::session::StepTrades>,
    /// The price the session values the position at, where it isn't
    /// Phoenix's mark: market making's oracle level, or the book's level
    /// where the book trades at a premium.
    pub valued_at_usd: Option<f64>,
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
    /// Before the bot starts: the session's own RPC (its secret path
    /// included), for a tool that shows its transactions in an explorer.
    fn rpc(&self, _url: &str) {}
    /// When the session is over: its chain. Keep it to keep the RPC up (an
    /// explorer still reading the session's transactions); drop it to stop it.
    fn keep(&self, _world: Box<dyn std::any::Any + Send>) {}
}

/// A session's clock, as a component's `start` sees it, and its scenario.
pub struct Clock {
    /// The seed the strategy's randomness starts from (see
    /// [`crate::keys::SessionKeyring::strategy_seed`]).
    pub seed: u64,
    pub steps: u64,
    pub slots_per_step: u64,
    pub start_slot: u64,
    /// The task's variables for the session ([`mtx_kit::SESSION_VARS`]).
    pub vars: Vec<(String, String)>,
    /// What the session's steps report beyond prices.
    pub reports: mtx_kit::Reports,
}

/// The mandate a component's session runs under, as its `start` sees it.
pub struct Terms<'a> {
    pub end_slot: u64,
    pub mandate: &'a mtx_core::Mandate,
    /// The position cap on its market, and its drawdown limit.
    pub cap: f64,
    pub max_drawdown: f64,
}

/// What the grader trades and reads for a component: a seat on each market
/// the session trades, with the bot's key, through the bot's guarded
/// endpoint, and how its market is read.
pub struct Desk {
    /// The session's markets, the bot's own first, as `start` lists them
    /// (the interface's `venues`).
    pub markets: Vec<mtx_kit::VenueInfo>,
    /// A seat on each market the session trades. A line for a market
    /// without one comes back not sent.
    pub seats: Vec<Box<dyn crate::venue::Seat>>,
    /// The market traded, as the venue names it, and the venue's metadata,
    /// as `start` gets them.
    pub symbol: String,
    pub venue: String,
    /// The bot's key, base58.
    pub authority: String,
    pub reader: Reader,
    /// Under the transaction layer: the chain the seats send through, which
    /// takes each line's transaction options, and the layer itself.
    pub pool: Option<std::sync::Arc<crate::txlayer::client::PoolClient>>,
    pub tx: Option<std::sync::Arc<crate::txlayer::TxEngine>>,
    /// The requests for quotes open to the bot, where clients send any
    /// (see [`crate::actors::library::rfq`]).
    pub rfqs: Option<crate::actors::library::RfqBoard>,
}

/// How a component's market is read at each turn.
pub enum Reader {
    /// A Phoenix perp account, as a process bot's run loop reads it: the
    /// venue's mark, oracle and index, the position there, and a PnL
    /// estimated from the mark's moves.
    Perp(std::sync::Arc<mtx_kit::Trader>),
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

/// A wallet as the session measures it, at the session's oracle price.
pub use crate::spot::Wallet;

/// A component playing one session: the strategy, and the markets the
/// grader trades for it ([`Desk`]). Reads and sends as `mtx_kit::run` does
/// for a process bot.
pub struct WasmBot {
    /// `None` if the component didn't load: the session plays without it.
    strategy: Option<WasmStrategy>,
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
    taking: Option<crate::taking::TakingGuard>,
}

impl WasmBot {
    /// Loads the component and calls its `start`. Its own `rpc` reads go to
    /// `rpc_url`, the bot's guarded endpoint.
    #[allow(clippy::too_many_arguments)]
    pub async fn start(
        path: &Path,
        rpc_url: &str,
        clock: Clock,
        terms: Terms<'_>,
        desk: Desk,
        log: Box<dyn Write + Send>,
        observer: Option<std::sync::Arc<dyn StepObserver>>,
        raw: Option<crate::raw::RawConfig>,
    ) -> crate::Result<Self> {
        let Clock {
            seed,
            steps,
            slots_per_step,
            start_slot,
            vars,
            reports,
        } = clock;
        let (start_mark, start_oracle) = match &desk.reader {
            Reader::Perp(trader) => (trader.mark_usd().await?, trader.oracle_usd().await?),
            Reader::Wallet { start } | Reader::Lending { start } => {
                (start.price_usd, start.price_usd)
            }
        };
        let session_fuel = (steps + 2).saturating_mul(FUEL_PER_STEP);
        let mut stopped = None;
        let mut strategy = match WasmStrategy::load(
            path,
            HttpReads::boxed(rpc_url),
            log,
            seed,
            session_fuel,
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
                fees: reports.fees || desk.tx.is_some(),
                ..reports
            },
            applies_tx_options: desk.tx.is_some(),
        };
        // Raw sends are a perp session's: a scenario enables them nowhere
        // else. They go out as the sheet's do, through the desk's trader.
        if let (Some(strategy), Some(raw), Reader::Perp(trader)) =
            (strategy.as_mut(), raw, &desk.reader)
        {
            let sender = crate::raw::RawSender::new(
                raw,
                trader.authority(),
                trader.env.exchange()?,
                &trader.env.symbol,
                &trader.env.rpc_url,
            );
            strategy.enable_raw_sends(match &desk.pool {
                Some(pool) => sender.with_pool(pool.clone()),
                None => sender,
            });
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
            .and_then(crate::taking::TakingGuard::new);
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
        })
    }

    /// The touch the session read for the coming turn, net of the bot's own
    /// orders, for the taking check.
    pub fn touched(&mut self, touch: Option<crate::taking::Touch>) {
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
            Reader::Perp(trader) => {
                let (top, oracle, position) = match (
                    trader.top().await,
                    trader.oracle_usd().await,
                    trader.position().await,
                ) {
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
        if matches!(self.desk.reader, Reader::Perp(_)) {
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
                .map(crate::actors::library::RfqBoard::open)
                .unwrap_or_default(),
            notes: std::mem::take(&mut self.notes),
            fees: self.desk.tx.as_ref().map(|tx| tx.fee_view()),
        })
    }

    /// What became of the sends still in the transaction layer's pool: each
    /// that landed, failed or expired since goes into the next market's
    /// sends, with its slot, fee and tip.
    fn resolve(&mut self) {
        let Some(tx) = &self.desk.tx else {
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
    fn seat(&self, venue: Option<&str>) -> Option<&dyn crate::venue::Seat> {
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
        let Some(market) = self.market(step, wallet).await else {
            return false;
        };
        let Some(strategy) = self.strategy.as_mut() else {
            self.last = Some(market);
            return false;
        };
        if let Some(raw) = strategy.raw_sender() {
            raw.begin(k, self.account).await;
        }
        let outcome = strategy.step(&market).await;
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
                SendState::NotSent("an earlier order of the step failed".into())
            } else if let Some(reason) = refused {
                tracing::info!(step = k, action = ?planned.action, "{reason}");
                SendState::NotSent(reason)
            } else if let Some(seat) = self.seat(planned.venue.as_deref()) {
                if let Some(pool) = &self.desk.pool {
                    pool.set_options(planned.tx);
                }
                let sent = seat.send(&planned).await;
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
        if let Some(pool) = &self.desk.pool {
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
        if self.desk.tx.is_some() {
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

    /// The fuel the session spent, and the most one call spent.
    pub fn fuel(&self) -> (u64, u64) {
        self.strategy
            .as_ref()
            .map_or((0, 0), |s| (s.fuel_used, s.fuel_max_call))
    }
}
