//! A strategy component playing a spot session ([`crate::spot`]): the
//! grader reads the market for it and trades its sheet on a Phoenix v1 or
//! OpenBook v2 book with the bot's key, through the bot's guarded endpoint,
//! as [`WasmBot`](super::WasmBot) does on Phoenix's perpetuals.
//!
//! A component trades on one venue: the first of the scenario's venues, its
//! session's own market. A line of the sheet that names another market
//! comes back as not sent. It reads the others, and their books, through
//! `rpc` with the kit's `phoenix-v1-venue` and `openbook-v2-venue`
//! decoders, and `start`'s `venue` lists them all.
// TODO: route each line to the venue it names, and hand the component the
// step's turn data (tape, depth, clock) as a process bot gets it.
//
// The market a component sees:
//
// | Field | Spot |
// |-------|------|
// | `mark`, `oracle`, `index` | the Pyth SOL/USD price the session values the account at |
// | `best-bid`, `best-ask` | the traded venue's book |
// | `position` | SOL held, wallet and venues, at the session's start price |
// | `pnl` | the account's value in USD since the start, as the session measures it |

use std::{io::Write, path::Path, sync::Arc};

use mtx_core::{
    Decimal, Side as CoreSide,
    mandate::{InstrumentRef, Kind},
};
use mtx_kit::{
    chain::{ChainClient, FireAndForgetOptions, RpcChainClient, SendMode},
    protocols::{
        NamedMarket,
        openbook::v2::{self as openbook_v2, OpenBookV2Order, OpenBookV2Trader},
        phoenix::v1::{self as phoenix_v1, PhoenixV1Order, PhoenixV1Trader},
    },
};
use solana_keypair::Keypair;
use solana_signer::Signer;

use super::{Clock, FUEL_PER_STEP, Outcome, WasmStrategy};
use crate::spot::Wallet;

/// The book a component's orders go to, and the grader's trader on it.
enum Book {
    Phoenix(Box<PhoenixV1Trader>),
    OpenBook(Box<OpenBookV2Trader>),
}

impl Book {
    async fn connect(
        venue: &str,
        chain: Arc<dyn ChainClient>,
        owner: solana_pubkey::Pubkey,
    ) -> crate::Result<Self> {
        // A stepped world lands a send in its next block: don't wait, and
        // don't judge it by the state before the block.
        let mode = SendMode::FireAndForget(FireAndForgetOptions { simulate: false });
        let error = |e: &dyn std::fmt::Display| crate::Error::Transaction {
            label: format!("{venue} market"),
            reason: e.to_string(),
        };
        Ok(match venue {
            "phoenix" => {
                let named = phoenix_v1::MARKETS[0];
                Self::Phoenix(Box::new(
                    PhoenixV1Trader::connect(chain, named.address, "SOL", "USDC", owner)
                        .await
                        .map_err(|e| error(&e))?
                        .with_send_mode(mode),
                ))
            }
            "openbook" => {
                let named = openbook_v2::MARKETS[0];
                Self::OpenBook(Box::new(
                    OpenBookV2Trader::connect(chain, named.address, "SOL", "USDC", owner)
                        .await
                        .map_err(|e| error(&e))?
                        .with_send_mode(mode),
                ))
            }
            other => {
                return Err(crate::Error::Transaction {
                    label: "a spot component's venue".into(),
                    reason: format!("{other} is not a spot venue: {:?}", crate::spot::VENUES),
                });
            }
        })
    }

    fn grid(&self) -> (Decimal, Decimal) {
        let instrument = match self {
            Self::Phoenix(t) => &t.market().instrument,
            Self::OpenBook(t) => &t.market().instrument,
        };
        (instrument.tick_size, instrument.lot_size)
    }

    /// The best bid and ask.
    async fn top(&self, unix_timestamp: u64) -> Option<(Option<f64>, Option<f64>)> {
        let book = match self {
            Self::Phoenix(t) => t.order_book().await.ok()?,
            Self::OpenBook(t) => t.order_book(unix_timestamp).await.ok()?,
        };
        let price = |levels: &[mtx_kit::core::PriceLevel]| {
            levels.first().map(|l| crate::session::grader_f64(l.price))
        };
        Some((price(&book.bids), price(&book.asks)))
    }

    /// Whether the account resting orders need exists: a Phoenix seat, an
    /// OpenBook open orders account.
    async fn has_account(&self) -> bool {
        let (chain, address) = match self {
            Self::Phoenix(t) => (t.chain(), t.seat_address()),
            Self::OpenBook(t) => (t.chain(), t.open_orders_account()),
        };
        matches!(chain.get_account(&address).await, Ok(Some(_)))
    }

    async fn open_account(&self, signer: &Keypair) -> Result<(), String> {
        match self {
            Self::Phoenix(t) => {
                let plan = t.plan_claim_seat().map_err(|e| e.to_string())?;
                t.send(&plan, signer).await.map_err(|e| e.to_string())?;
            }
            Self::OpenBook(t) => {
                let plan = t.plan_open_account().map_err(|e| e.to_string())?;
                t.send(&plan, signer).await.map_err(|e| e.to_string())?;
            }
        }
        Ok(())
    }

    /// A market order: take up to `size` at no worse than `limit`.
    async fn take(
        &self,
        side: CoreSide,
        size: Decimal,
        limit: Decimal,
        signer: &Keypair,
    ) -> Result<String, String> {
        match self {
            Self::Phoenix(t) => t
                .submit(
                    &PhoenixV1Order::ImmediateOrCancel {
                        side,
                        quantity: size,
                        worst_price: Some(limit),
                        all_or_none: false,
                    },
                    signer,
                )
                .await
                .map(|signature| signature.to_string())
                .map_err(|e| e.to_string()),
            Self::OpenBook(t) => t
                .submit(
                    &OpenBookV2Order::Take {
                        side,
                        quantity: size,
                        worst_price: limit,
                    },
                    signer,
                )
                .await
                .map(|signature| signature.to_string())
                .map_err(|e| e.to_string()),
        }
    }

    /// A resting order of `size` at `price`.
    async fn rest(
        &self,
        side: CoreSide,
        price: Decimal,
        size: Decimal,
        signer: &Keypair,
    ) -> Result<String, String> {
        match self {
            Self::Phoenix(t) => t
                .submit(
                    &PhoenixV1Order::Limit {
                        side,
                        price,
                        quantity: size,
                    },
                    signer,
                )
                .await
                .map(|signature| signature.to_string())
                .map_err(|e| e.to_string()),
            Self::OpenBook(t) => t
                .submit(
                    &OpenBookV2Order::Limit {
                        side,
                        price,
                        quantity: size,
                    },
                    signer,
                )
                .await
                .map(|signature| signature.to_string())
                .map_err(|e| e.to_string()),
        }
    }

    async fn cancel_all(&self, signer: &Keypair) -> Result<String, String> {
        match self {
            Self::Phoenix(t) => {
                let plan = t.plan_cancel_all().map_err(|e| e.to_string())?;
                t.send(&plan, signer)
                    .await
                    .map(|signature| signature.to_string())
                    .map_err(|e| e.to_string())
            }
            Self::OpenBook(t) => {
                let plan = t.plan_cancel_all().map_err(|e| e.to_string())?;
                t.send(&plan, signer)
                    .await
                    .map(|signature| signature.to_string())
                    .map_err(|e| e.to_string())
            }
        }
    }
}

/// What a spot component's `start` gets as `venue`: every market the
/// session may trade, and the venue its orders go to.
#[derive(serde::Serialize)]
struct SpotVenues {
    orders: String,
    markets: Vec<SpotMarket>,
}

#[derive(serde::Serialize)]
struct SpotMarket {
    venue: String,
    symbol: String,
    program: String,
    market: String,
    base_mint: String,
    quote_mint: String,
}

fn spot_venues(venues: &[String]) -> crate::Result<SpotVenues> {
    let market = |venue: &str, program: &solana_pubkey::Pubkey, named: &NamedMarket| SpotMarket {
        venue: venue.into(),
        symbol: named.symbol.into(),
        program: program.to_string(),
        market: named.address.to_string(),
        base_mint: named.base_mint.to_string(),
        quote_mint: named.quote_mint.to_string(),
    };
    Ok(SpotVenues {
        orders: venues.first().cloned().unwrap_or_default(),
        markets: venues
            .iter()
            .map(|venue| match venue.as_str() {
                "phoenix" => Ok(market(
                    venue,
                    &phoenix_v1::PROGRAM_ID,
                    &phoenix_v1::MARKETS[0],
                )),
                "openbook" => Ok(market(
                    venue,
                    &openbook_v2::PROGRAM_ID,
                    &openbook_v2::MARKETS[0],
                )),
                other => Err(crate::Error::Transaction {
                    label: "a spot component's venues".into(),
                    reason: format!("{other} is not a spot venue: {:?}", crate::spot::VENUES),
                }),
            })
            .collect::<crate::Result<_>>()?,
    })
}

/// `x` on a grid of `step`, rounded down or up; `None` for a value no grid
/// can hold (not a number, or overflowing a `Decimal`): the order is
/// refused. The value is first snapped to nine
/// decimals, so float noise (`0.7 - 0.4` is 0.29999999999999993) doesn't
/// cost a step.
fn on_grid(x: f64, step: Decimal, up: bool) -> Option<Decimal> {
    let x = Decimal::try_from(x).ok()?.round_dp(9);
    let steps = x.checked_div(step)?;
    let steps = if up { steps.ceil() } else { steps.floor() };
    steps.checked_mul(step)
}

fn core_side(side: mtx_kit::Side) -> CoreSide {
    match side {
        mtx_kit::Side::Bid => CoreSide::Buy,
        mtx_kit::Side::Ask => CoreSide::Sell,
    }
}

/// A component playing one spot session.
pub struct SpotWasmBot {
    /// `None` if the component didn't load: the session plays without it.
    strategy: Option<WasmStrategy>,
    book: Book,
    signer: Keypair,
    session: mtx_kit::Session,
    start_value_usd: f64,
    start_price_usd: f64,
    peak_pnl: f64,
    resting: (f64, f64),
    /// Whether the account resting orders need is known to exist.
    account_open: bool,
    last: Option<mtx_kit::Market>,
    /// What became of the last sheet, and the planner's notes on it: the
    /// next step's market carries them.
    sends: Vec<mtx_kit::SendStatus>,
    notes: Vec<String>,
    /// Why the strategy plays no more of the session, if it doesn't.
    pub stopped: Option<String>,
}

/// The session a spot component starts in, besides its clock.
pub struct SpotStart<'a> {
    /// The scenario's venues: orders go to the first.
    pub venues: &'a [String],
    /// The bot's guarded endpoint, and its key.
    pub rpc_url: &'a str,
    pub signer: Keypair,
    pub end_slot: u64,
    pub mandate: &'a mtx_core::Mandate,
    pub max_position: f64,
    pub max_drawdown_usd: f64,
    /// The account when the session starts.
    pub wallet: &'a Wallet,
}

/// What a spot session executes of a sheet: orders on its own market.
fn executes(venue: Option<&str>, action: &mtx_kit::Action) -> bool {
    venue.is_none()
        && matches!(
            action,
            mtx_kit::Action::Market { .. }
                | mtx_kit::Action::Limit { .. }
                | mtx_kit::Action::CancelAll
        )
}

impl SpotWasmBot {
    /// Loads the component and calls its `start`.
    pub async fn start(
        path: &Path,
        start: SpotStart<'_>,
        clock: Clock<'_>,
        log: Box<dyn Write + Send>,
    ) -> crate::Result<Self> {
        let Clock {
            steps,
            slots_per_step,
            start_slot,
            scenario,
            vars,
            reports,
        } = clock;
        let chain: Arc<dyn ChainClient> = Arc::new(RpcChainClient::new(start.rpc_url));
        let owner = start.signer.pubkey();
        let venue = start.venues.first().map_or("phoenix", String::as_str);
        let book = Book::connect(venue, chain, owner).await?;
        let seed = mtx_core::turns::scenario_seed(scenario);
        let session_fuel = (steps + 2).saturating_mul(FUEL_PER_STEP);
        let mut stopped = None;
        let mut strategy = match WasmStrategy::load(path, start.rpc_url, log, seed, session_fuel)
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
        let own = InstrumentRef {
            venue: venue.into(),
            kind: Kind::Spot,
            symbol: crate::spot::SYMBOL.into(),
        };
        let session = mtx_kit::Session {
            steps,
            slots_per_step,
            start_slot,
            end_slot: start.end_slot,
            symbol: crate::spot::SYMBOL.into(),
            start_mark: start.wallet.price_usd,
            start_oracle: start.wallet.price_usd,
            cap: start.max_position,
            max_drawdown: start.max_drawdown_usd,
            mandate: start.mandate.clone(),
            venue: serde_json::to_string(&spot_venues(start.venues)?)?,
            authority: owner.to_string(),
            vars,
            seed,
            venues: mtx_kit::VenueInfo::of_mandate(start.mandate, &own, |i| *i == own),
            reports,
            applies_tx_options: false,
        };
        if let Some(strategy) = strategy.as_mut() {
            match strategy.start(&session).await {
                Outcome::Done(()) => {}
                Outcome::Failed(e) => {
                    tracing::warn!(error = %e, "the strategy's start failed; it plays on");
                }
                Outcome::Trapped(e) => {
                    tracing::warn!(error = %e, "the strategy's start trapped; it plays no more of the session");
                    stopped = Some(format!("trapped in start: {e}"));
                }
            }
        }
        Ok(Self {
            strategy,
            book,
            signer: start.signer,
            session,
            start_value_usd: start.wallet.value_usd,
            start_price_usd: start.wallet.price_usd,
            peak_pnl: 0.0,
            resting: (0.0, 0.0),
            account_open: false,
            last: None,
            sends: Vec::new(),
            notes: Vec::new(),
            stopped,
        })
    }

    /// The strategy's turn at step `k`, with the account as the session
    /// read it at this step: call `step`, cut the sheet to the mandate's cap
    /// and send it, in order, stopping at the first order that fails.
    /// Returns whether the strategy answered.
    pub async fn turn(
        &mut self,
        k: u64,
        slot: u64,
        last: bool,
        wallet: &Wallet,
        unix_timestamp: i64,
    ) -> bool {
        if self.stopped.is_some() {
            return false;
        }
        let Some((best_bid, best_ask)) = self.book.top(unix_timestamp.max(0) as u64).await else {
            tracing::warn!(step = k, "couldn't read the book; skipping the step");
            return false;
        };
        let pnl = wallet.value_usd - self.start_value_usd;
        self.peak_pnl = self.peak_pnl.max(pnl);
        let price = wallet.price_usd;
        let position = mtx_kit::Position {
            size: wallet.base,
            entry_usd: self.start_price_usd,
        };
        let (venues, fills) = mtx_kit::venue_views(
            &self.session.venues,
            self.session.reports,
            best_bid,
            best_ask,
            position,
            &[],
            &[],
        );
        let market = mtx_kit::Market {
            k,
            slot,
            last,
            steps_left: self.session.steps.saturating_sub(k + 1),
            slots_left: self.session.end_slot.saturating_sub(slot),
            mark: price,
            oracle: price,
            index: price,
            best_bid,
            best_ask,
            position,
            cap: self.session.cap,
            max_drawdown: self.session.max_drawdown,
            resting_bids: self.resting.0,
            resting_asks: self.resting.1,
            pnl,
            peak_pnl: self.peak_pnl,
            drawdown: self.peak_pnl - pnl,
            venues,
            sends: std::mem::take(&mut self.sends),
            fills,
            notes: std::mem::take(&mut self.notes),
            ..mtx_kit::Market::default()
        };
        let Some(strategy) = self.strategy.as_mut() else {
            self.last = Some(market);
            return false;
        };
        let lines = match strategy.step(&market).await {
            Outcome::Done(lines) => lines,
            Outcome::Failed(e) => {
                tracing::warn!(step = k, error = %e, "the strategy failed; no orders this step");
                self.last = Some(market);
                return true;
            }
            Outcome::Trapped(e) => {
                tracing::warn!(step = k, error = %e, "the strategy trapped; it plays no more of the session");
                self.stopped = Some(format!("trapped at step {k}: {e}"));
                self.last = Some(market);
                return false;
            }
        };
        let mut orders = mtx_kit::Orders::new();
        for (intent, options) in lines {
            orders.push(intent, options);
        }
        // Only what the session executes counts toward the cap.
        let (planned, notes) = mtx_kit::plan_sheet_with(&orders, &market, executes);
        for note in &notes {
            tracing::warn!(step = k, "{note}");
        }
        self.notes = notes;
        self.last = Some(market);
        let own = self.session.venues[0].id.clone();
        // The orders that reached the venue, the one that failed included:
        // the ledger of what rests only over-counts.
        let mut tried = Vec::new();
        let mut failed = false;
        for planned in planned {
            let state = if failed {
                mtx_kit::SendState::NotSent("an earlier order of the step failed".into())
            } else if let Some(venue) = &planned.venue {
                mtx_kit::SendState::NotSent(format!("{venue} isn't traded in this session"))
            } else if !executes(None, &planned.action) {
                mtx_kit::SendState::NotSent("a spot session doesn't execute this action".into())
            } else {
                tried.push(planned.action.clone());
                match self.send(&planned.action).await {
                    Ok(Some(signature)) => mtx_kit::SendState::Landed(signature),
                    Ok(None) => {
                        mtx_kit::SendState::NotSent("nothing to send on this market's grid".into())
                    }
                    Err(error) => {
                        tracing::warn!(step = k, action = ?planned.action, %error, "order failed; the step's other orders aren't sent");
                        failed = true;
                        mtx_kit::SendState::failed(error)
                    }
                }
            };
            let mtx_kit::Planned {
                line,
                venue,
                client_id,
                action,
                expires_slot,
                ..
            } = planned;
            self.sends.push(mtx_kit::SendStatus {
                step: k,
                source: mtx_kit::SendSource::Line(line as u32),
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
        true
    }

    /// One order of the sheet on the book's grid: sizes rounded down to
    /// whole lots, prices rounded away from a worse fill (a bid's down, an
    /// ask's up). An order of no lots is skipped: `None`.
    async fn send(&mut self, action: &mtx_kit::Action) -> Result<Option<String>, String> {
        let (tick, lot) = self.book.grid();
        let off_grid = || "a price or size no market grid can hold".to_string();
        match *action {
            mtx_kit::Action::Market { side, size, limit } => {
                let size = on_grid(size, lot, false).ok_or_else(off_grid)?;
                let limit =
                    on_grid(limit, tick, side == mtx_kit::Side::Ask).ok_or_else(off_grid)?;
                if size.is_zero() {
                    return Ok(None);
                }
                self.book
                    .take(core_side(side), size, limit, &self.signer)
                    .await
                    .map(Some)
            }
            mtx_kit::Action::Limit { side, price, size } => {
                let size = on_grid(size, lot, false).ok_or_else(off_grid)?;
                let price =
                    on_grid(price, tick, side == mtx_kit::Side::Ask).ok_or_else(off_grid)?;
                if size.is_zero() || price.is_zero() {
                    return Ok(None);
                }
                // The setup is sent with the order, unconfirmed: count the
                // account open only once it is seen, and send the setup again
                // until then (a fault may drop it).
                if !self.account_open {
                    if self.book.has_account().await {
                        self.account_open = true;
                    } else {
                        self.book.open_account(&self.signer).await?;
                    }
                }
                self.book
                    .rest(core_side(side), price, size, &self.signer)
                    .await
                    .map(Some)
            }
            mtx_kit::Action::CancelAll => {
                if !self.account_open {
                    if !self.book.has_account().await {
                        return Ok(None);
                    }
                    self.account_open = true;
                }
                self.book.cancel_all(&self.signer).await.map(Some)
            }
            _ => Err("a spot session doesn't execute this action".into()),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn orders_land_on_the_grid_away_from_a_worse_fill() {
        let tick = Decimal::new(1, 3);
        assert_eq!(
            on_grid(150.12345, tick, false),
            Some(Decimal::new(150_123, 3))
        );
        assert_eq!(
            on_grid(150.12345, tick, true),
            Some(Decimal::new(150_124, 3))
        );
        assert_eq!(
            on_grid(0.0049, Decimal::new(1, 2), false),
            Some(Decimal::ZERO)
        );
        assert_eq!(on_grid(f64::NAN, tick, false), None);
        // Float noise doesn't cost a step.
        assert_eq!(on_grid(0.7 - 0.4, tick, false), Some(Decimal::new(300, 3)));
        assert_eq!(on_grid(0.1 + 0.2, tick, true), Some(Decimal::new(300, 3)));
        // A price past what a Decimal holds is refused, not a panic.
        assert_eq!(on_grid(1e27, tick, true), None);
        assert_eq!(on_grid(1e27, tick, false), None);
        assert_eq!(on_grid(1e30, tick, false), None);
        assert_eq!(on_grid(-1e27, tick, false), None);
    }

    #[test]
    fn a_component_is_told_every_market_and_where_its_orders_go() {
        let venues = spot_venues(&["openbook".into(), "phoenix".into()]).unwrap();
        assert!(spot_venues(&["raydium".into()]).is_err());
        let json = serde_json::to_value(&venues).unwrap();
        assert_eq!(json["orders"], "openbook");
        assert_eq!(
            json["markets"][0]["program"],
            openbook_v2::PROGRAM_ID.to_string()
        );
        assert_eq!(
            json["markets"][1]["market"],
            phoenix_v1::MARKETS[0].address.to_string()
        );
        assert_eq!(json["markets"][1]["symbol"], "SOL-USDC");
    }

    /// The spot smoke as a component: set `MTX_TEST_SPOT_WASM` to
    /// `bots/spot-smoke`'s buyer build (`just build` there).
    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "needs a built component"]
    async fn the_spot_smoke_component_sells_and_buys_once() {
        let path = std::path::PathBuf::from(std::env::var("MTX_TEST_SPOT_WASM").unwrap());
        let mut strategy = WasmStrategy::load(
            &path,
            "http://127.0.0.1:1",
            Box::new(std::io::sink()),
            7,
            100 * crate::wasm::FUEL_PER_CALL,
        )
        .await
        .unwrap();
        let mandate = mtx_core::Mandate::spot(&crate::spot::VENUES, crate::spot::SYMBOL, 10.0);
        let own = InstrumentRef {
            venue: "phoenix".into(),
            kind: Kind::Spot,
            symbol: crate::spot::SYMBOL.into(),
        };
        let session = mtx_kit::Session {
            steps: 20,
            slots_per_step: 2,
            start_slot: 100,
            end_slot: 140,
            symbol: crate::spot::SYMBOL.into(),
            start_mark: 150.0,
            start_oracle: 150.0,
            cap: 10.0,
            max_drawdown: 1_500.0,
            venues: mtx_kit::VenueInfo::of_mandate(&mandate, &own, |i| *i == own),
            mandate,
            venue: serde_json::to_string(
                &spot_venues(&["phoenix".into(), "openbook".into()]).unwrap(),
            )
            .unwrap(),
            authority: String::new(),
            vars: Vec::new(),
            seed: 7,
            reports: mtx_kit::Reports::default(),
            applies_tx_options: false,
        };
        assert!(matches!(strategy.start(&session).await, Outcome::Done(())));
        let market = |k| mtx_kit::Market {
            k,
            slot: 100 + 2 * k,
            last: false,
            steps_left: 19 - k,
            slots_left: 40 - 2 * k,
            mark: 150.0,
            oracle: 150.0,
            index: 150.0,
            best_bid: Some(149.9),
            best_ask: Some(150.1),
            position: mtx_kit::Position {
                size: 10.0,
                entry_usd: 150.0,
            },
            cap: 10.0,
            max_drawdown: 1_500.0,
            ..mtx_kit::Market::default()
        };
        // It reads the books through rpc, which this test doesn't answer,
        // and trades anyway: a sell, and the buyer's buy.
        let Outcome::Done(first) = strategy.step(&market(0)).await else {
            panic!()
        };
        let sides: Vec<mtx_kit::Side> = first
            .iter()
            .map(|(intent, _)| match intent {
                mtx_kit::Intent::Market { side, .. } => *side,
                other => panic!("{other:?}"),
            })
            .collect();
        assert_eq!(sides, [mtx_kit::Side::Ask, mtx_kit::Side::Bid]);
        let Outcome::Done(second) = strategy.step(&market(1)).await else {
            panic!()
        };
        assert!(second.is_empty());
    }
}
