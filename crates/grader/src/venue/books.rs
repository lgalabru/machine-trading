//! Spot order books as [`Market`]s (`specs/instruments/spot.md`): the
//! markets the venue profiles name on Phoenix v1 and OpenBook v2
//! (`specs/venues/`), which [`super::phoenix`] and [`super::openbook`]
//! offer.
//!
//! A book's seat trades from a wallet: a take needs no other account, a
//! resting order needs the venue's own (a Phoenix seat, an OpenBook open
//! orders account), which the seat opens with its first resting order.
//! Orders go on the market's grid: sizes rounded down to whole lots, prices
//! away from a worse fill (a bid's down, an ask's up). Sends are fire and
//! forget, as a stepped world needs: a send lands in the world's next block.

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use async_trait::async_trait;
use mtx_core::{
    Decimal, Holding, Side as CoreSide,
    mandate::{InstrumentRef, Kind},
    turns::{BookLevels, Level},
};
use mtx_kit::{
    Action, Planned, SendState, Sent, Side,
    chain::{ChainClient, FireAndForgetOptions, RpcChainClient, SendMode},
    core::{OrderBook, PriceLevel},
    protocols::{
        NamedMarket,
        openbook::v2::{
            self as openbook_v2, OpenBookV2Market, OpenBookV2Meter, OpenBookV2Order,
            OpenBookV2Trader,
        },
        phoenix::v1::{
            self as phoenix_v1, MarketState, PhoenixV1Market, PhoenixV1Meter, PhoenixV1Order,
            PhoenixV1Trader,
        },
        spot::MarketFill,
    },
};
use solana_keypair::Keypair;
use solana_pubkey::Pubkey;
use solana_signer::Signer;

use super::{Market, Seat, SeatResult};
/// The book venues a spot session can trade on.
pub use crate::spot::VENUES;
use crate::{Error, Result, session::grader_f64};

/// One spot book's market, decoded once.
enum Decoded {
    Phoenix(Box<PhoenixV1Market>),
    OpenBook(Box<OpenBookV2Market>),
}

/// A spot book a venue profile names.
pub struct BookMarket {
    instrument: InstrumentRef,
    named: NamedMarket,
    chain: Arc<dyn ChainClient>,
    market: Decoded,
}

/// The markets `venue`'s profile names (`phoenix` or `openbook`).
pub fn named_markets(venue: &str) -> Result<&'static [NamedMarket]> {
    match venue {
        "phoenix" => Ok(phoenix_v1::MARKETS),
        "openbook" => Ok(openbook_v2::MARKETS),
        other => Err(not_a_book(other)),
    }
}

/// The base and quote asset symbols of a `BASE-QUOTE` market symbol.
fn assets(symbol: &str) -> Result<(&str, &str)> {
    symbol
        .split_once('-')
        .ok_or_else(|| Error::UnknownMarket(format!("{symbol} is not BASE-QUOTE")))
}

impl BookMarket {
    /// The `symbol` market the profile of `venue` (`phoenix` or `openbook`)
    /// names, read through `chain`.
    pub async fn connect(venue: &str, symbol: &str, chain: Arc<dyn ChainClient>) -> Result<Self> {
        let named = *named_markets(venue)?
            .iter()
            .find(|m| m.symbol == symbol)
            .ok_or_else(|| Error::UnknownMarket(format!("{venue}:spot:{symbol}")))?;
        let (base, quote) = assets(symbol)?;
        let account = market_account(chain.as_ref(), &named.address).await?;
        let market = match venue {
            "phoenix" => Decoded::Phoenix(Box::new(
                PhoenixV1Market::decode(named.address, &account, base, quote)
                    .map_err(venue_error("phoenix v1 market"))?,
            )),
            _ => Decoded::OpenBook(Box::new(
                OpenBookV2Market::decode(named.address, &account, base, quote)
                    .map_err(venue_error("openbook v2 market"))?,
            )),
        };
        Ok(Self {
            instrument: InstrumentRef {
                venue: venue.into(),
                kind: Kind::Spot,
                symbol: symbol.into(),
            },
            named,
            chain,
            market,
        })
    }

    /// The program the market runs on.
    pub fn program(&self) -> Pubkey {
        match self.market {
            Decoded::Phoenix(_) => phoenix_v1::PROGRAM_ID,
            Decoded::OpenBook(_) => openbook_v2::PROGRAM_ID,
        }
    }

    /// The market as the venue profile names it.
    pub fn named(&self) -> &NamedMarket {
        &self.named
    }

    /// The book by price level: as of `slot` and `unix_timestamp`, or as
    /// a trader reads it now (Phoenix at the current slot, OpenBook at the
    /// world's clock).
    async fn book(&self, at: Option<(u64, i64)>) -> Result<OrderBook> {
        let chain = self.chain.as_ref();
        Ok(match &self.market {
            Decoded::Phoenix(market) => {
                let account = market_account(chain, &market.keys.market).await?;
                let state =
                    MarketState::decode(&account).map_err(venue_error("phoenix v1 market"))?;
                let (slot, unix) = match at {
                    Some((slot, unix)) => (slot, unix.max(0) as u64),
                    None => (chain.get_slot().await.map_err(venue_error("the slot"))?, 0),
                };
                market.order_book(&state, slot, unix)
            }
            Decoded::OpenBook(market) => {
                let book = openbook_v2::read_book(chain, market)
                    .await
                    .map_err(venue_error("openbook v2 book"))?;
                let (slot, unix) = match at {
                    Some((slot, unix)) => (Some(slot), unix),
                    None => (None, unix_timestamp(chain).await?),
                };
                market.order_book(&book.bids, &book.asks, unix.max(0) as u64, slot)
            }
        })
    }
}

#[async_trait]
impl Market for BookMarket {
    fn instrument(&self) -> &InstrumentRef {
        &self.instrument
    }

    fn address(&self) -> Pubkey {
        self.named.address
    }

    async fn top(&self) -> Result<(Option<f64>, Option<f64>)> {
        let book = self.book(None).await?;
        let best = |levels: &[PriceLevel]| levels.first().map(|l| grader_f64(l.price));
        Ok((best(&book.bids), best(&book.asks)))
    }

    async fn depth(
        &self,
        levels: usize,
        slot: u64,
        unix_timestamp: i64,
    ) -> Result<Option<BookLevels>> {
        let book = self.book(Some((slot, unix_timestamp))).await?;
        let take = |side: Vec<PriceLevel>| -> Vec<Level> {
            side.into_iter()
                .take(levels)
                .map(|l| Level {
                    price: grader_f64(l.price),
                    base: l.quantity.map_or(0.0, grader_f64),
                })
                .collect()
        };
        Ok(Some(BookLevels {
            venue: self.instrument.venue.clone(),
            market: Some(self.instrument.to_string()),
            bids: take(book.bids),
            asks: take(book.asks),
        }))
    }

    /// The base asset, then the quote, each as the spot profile counts it.
    async fn holdings(&self, owner: &Pubkey) -> Result<Vec<Holding>> {
        let chain = self.chain.as_ref();
        let balances = match &self.market {
            Decoded::Phoenix(market) => {
                let account = market_account(chain, &market.keys.market).await?;
                let state =
                    MarketState::decode(&account).map_err(venue_error("phoenix v1 market"))?;
                phoenix_v1::balances(chain, market, &state, owner)
                    .await
                    .map_err(venue_error("phoenix v1 balances"))?
            }
            Decoded::OpenBook(market) => openbook_v2::balances(chain, market, owner)
                .await
                .map_err(venue_error("openbook v2 balances"))?,
        };
        Ok(vec![
            Holding::Spot(balances.base),
            Holding::Spot(balances.quote),
        ])
    }

    async fn fills(
        &self,
        after_slot: u64,
        owner: &Pubkey,
    ) -> Result<Vec<(MarketFill, Vec<Pubkey>)>> {
        let chain = self.chain.clone();
        Ok(match self.market {
            Decoded::Phoenix(_) => PhoenixV1Meter::new(chain, *owner, &[self.named])
                .market_fills(after_slot)
                .await?
                .into_iter()
                .map(|fill| (fill, vec![*owner]))
                .collect(),
            Decoded::OpenBook(_) => {
                let meter = OpenBookV2Meter::new(chain, *owner, &[self.named]);
                let ids = meter.fill_ids().await?;
                meter
                    .market_fills(after_slot)
                    .await?
                    .into_iter()
                    .map(|fill| (fill, ids.clone()))
                    .collect()
            }
        })
    }

    async fn seat(&self, signer: Keypair, rpc_url: &str) -> Result<Box<dyn Seat>> {
        Ok(Box::new(
            BookSeat::connect(
                &self.instrument,
                &self.named,
                Arc::new(RpcChainClient::new(rpc_url)),
                signer,
            )
            .await?,
        ))
    }
}

/// The book an account trades on, and the kit's trader on it.
enum Trader {
    Phoenix(Box<PhoenixV1Trader>),
    OpenBook(Box<OpenBookV2Trader>),
}

/// An account on a spot book.
pub struct BookSeat {
    market: String,
    trader: Trader,
    signer: Keypair,
    /// Whether the account resting orders need is known to exist.
    account_open: AtomicBool,
}

impl BookSeat {
    async fn connect(
        instrument: &InstrumentRef,
        named: &NamedMarket,
        chain: Arc<dyn ChainClient>,
        signer: Keypair,
    ) -> Result<Self> {
        let (base, quote) = assets(&instrument.symbol)?;
        let mode = SendMode::FireAndForget(FireAndForgetOptions { simulate: false });
        let owner = signer.pubkey();
        let venue = instrument.venue.as_str();
        let trader = match venue {
            "phoenix" => Trader::Phoenix(Box::new(
                PhoenixV1Trader::connect(chain, named.address, base, quote, owner)
                    .await
                    .map_err(venue_error("phoenix v1 market"))?
                    .with_send_mode(mode),
            )),
            "openbook" => Trader::OpenBook(Box::new(
                OpenBookV2Trader::connect(chain, named.address, base, quote, owner)
                    .await
                    .map_err(venue_error("openbook v2 market"))?
                    .with_send_mode(mode),
            )),
            other => return Err(not_a_book(other)),
        };
        Ok(Self {
            market: instrument.to_string(),
            trader,
            signer,
            account_open: AtomicBool::new(false),
        })
    }

    fn chain(&self) -> &dyn ChainClient {
        match &self.trader {
            Trader::Phoenix(t) => t.chain(),
            Trader::OpenBook(t) => t.chain(),
        }
    }

    /// The market's tick and lot.
    fn grid(&self) -> (Decimal, Decimal) {
        let instrument = match &self.trader {
            Trader::Phoenix(t) => &t.market().instrument,
            Trader::OpenBook(t) => &t.market().instrument,
        };
        (instrument.tick_size, instrument.lot_size)
    }

    /// Whether the account resting orders need exists, once it is seen:
    /// its setup is sent unconfirmed, and a fault may drop it.
    async fn account_exists(&self) -> bool {
        if self.account_open.load(Ordering::SeqCst) {
            return true;
        }
        let address = match &self.trader {
            Trader::Phoenix(t) => t.seat_address(),
            Trader::OpenBook(t) => t.open_orders_account(),
        };
        let exists = matches!(self.chain().get_account(&address).await, Ok(Some(_)));
        self.account_open.store(exists, Ordering::SeqCst);
        exists
    }

    /// Send the account's setup again, until it is seen.
    async fn open_account(&self) -> SeatResult {
        if self.account_exists().await {
            return Ok(());
        }
        match &self.trader {
            Trader::Phoenix(t) => {
                let plan = t.plan_claim_seat().map_err(|e| e.to_string())?;
                t.send(&plan, &self.signer)
                    .await
                    .map_err(|e| e.to_string())?;
            }
            Trader::OpenBook(t) => {
                let plan = t.plan_open_account().map_err(|e| e.to_string())?;
                t.send(&plan, &self.signer)
                    .await
                    .map_err(|e| e.to_string())?;
            }
        }
        Ok(())
    }

    /// A market order: take up to `size` at no worse than `limit`.
    async fn take_exact(
        &self,
        side: CoreSide,
        size: Decimal,
        limit: Decimal,
    ) -> SeatResult<mtx_kit::Signature> {
        match &self.trader {
            Trader::Phoenix(t) => t
                .submit(
                    &PhoenixV1Order::ImmediateOrCancel {
                        side,
                        quantity: size,
                        worst_price: Some(limit),
                        all_or_none: false,
                    },
                    &self.signer,
                )
                .await
                .map_err(|e| e.to_string()),
            Trader::OpenBook(t) => t
                .submit(
                    &OpenBookV2Order::Take {
                        side,
                        quantity: size,
                        worst_price: limit,
                    },
                    &self.signer,
                )
                .await
                .map_err(|e| e.to_string()),
        }
    }

    /// A resting order of `size` at `price`; post-only, it takes nothing
    /// and isn't placed if it would cross.
    async fn rest_exact(
        &self,
        side: CoreSide,
        price: Decimal,
        size: Decimal,
        post_only: bool,
        expires_slot: Option<u64>,
    ) -> SeatResult<mtx_kit::Signature> {
        self.open_account().await?;
        match &self.trader {
            Trader::Phoenix(t) => {
                let order = if post_only {
                    PhoenixV1Order::PostOnly {
                        side,
                        price,
                        quantity: size,
                        amend_if_crossing: false,
                        last_valid_slot: expires_slot,
                    }
                } else {
                    PhoenixV1Order::Limit {
                        side,
                        price,
                        quantity: size,
                    }
                };
                t.submit(&order, &self.signer)
                    .await
                    .map_err(|e| e.to_string())
            }
            Trader::OpenBook(t) => {
                let order = if post_only {
                    OpenBookV2Order::PostOnly {
                        side,
                        price,
                        quantity: size,
                        expiry_timestamp: 0,
                    }
                } else {
                    OpenBookV2Order::Limit {
                        side,
                        price,
                        quantity: size,
                    }
                };
                t.submit(&order, &self.signer)
                    .await
                    .map_err(|e| e.to_string())
            }
        }
    }

    async fn cancel_exact(&self) -> SeatResult<Option<mtx_kit::Signature>> {
        // Nothing rests without the account.
        if !self.account_exists().await {
            return Ok(None);
        }
        let sent = match &self.trader {
            Trader::Phoenix(t) => {
                let plan = t.plan_cancel_all().map_err(|e| e.to_string())?;
                t.send(&plan, &self.signer).await.map_err(|e| e.to_string())
            }
            Trader::OpenBook(t) => {
                let plan = t.plan_cancel_all().map_err(|e| e.to_string())?;
                t.send(&plan, &self.signer).await.map_err(|e| e.to_string())
            }
        };
        sent.map(Some)
    }

    /// What became of a send the world will land in its next block: what
    /// the chain says of it now.
    async fn outcome(&self, signature: mtx_kit::Signature) -> SendState {
        match self.chain().get_transaction(&signature).await {
            Ok(Some(summary)) => match summary.error {
                None => SendState::Landed(signature.to_string()),
                Some(error) => SendState::Failed {
                    reason: error.to_string(),
                    signature: Some(signature.to_string()),
                },
            },
            Ok(None) | Err(_) => SendState::Delayed,
        }
    }
}

#[async_trait]
impl Seat for BookSeat {
    fn market(&self) -> &str {
        &self.market
    }

    fn executes(&self, action: &Action) -> bool {
        matches!(
            action,
            Action::Market { .. }
                | Action::Limit { .. }
                | Action::PostOnly { .. }
                | Action::CancelAll
        )
    }

    async fn send(&self, planned: &Planned) -> Sent {
        let (tick, lot) = self.grid();
        let off_grid = || Sent {
            state: SendState::failed("a price or size no market grid can hold"),
            sent: false,
        };
        let no_lots = || Sent {
            state: SendState::NotSent("rounds to nothing on the market's grid".into()),
            sent: false,
        };
        let sent = match planned.action {
            Action::Market { side, size, limit } => {
                let (Some(size), Some(limit)) = (
                    on_grid(size, lot, false),
                    on_grid(limit, tick, side == Side::Ask),
                ) else {
                    return off_grid();
                };
                if size.is_zero() {
                    return no_lots();
                }
                self.take_exact(core_side(side), size, limit).await
            }
            Action::Limit { side, price, size } | Action::PostOnly { side, price, size } => {
                let post_only = matches!(planned.action, Action::PostOnly { .. });
                if post_only
                    && planned.expires_slot.is_some()
                    && matches!(self.trader, Trader::OpenBook(_))
                {
                    return Sent {
                        state: SendState::NotSent(
                            "OpenBook orders expire by the clock, not by slot".into(),
                        ),
                        sent: false,
                    };
                }
                let (Some(size), Some(price)) = (
                    on_grid(size, lot, false),
                    on_grid(price, tick, side == Side::Ask),
                ) else {
                    return off_grid();
                };
                if size.is_zero() || price.is_zero() {
                    return no_lots();
                }
                self.rest_exact(
                    core_side(side),
                    price,
                    size,
                    post_only,
                    planned.expires_slot,
                )
                .await
            }
            Action::CancelAll => match self.cancel_exact().await {
                Ok(Some(signature)) => Ok(signature),
                Ok(None) => {
                    return Sent {
                        state: SendState::NotSent("nothing rests: no account yet".into()),
                        sent: false,
                    };
                }
                Err(error) => Err(error),
            },
            _ => {
                return Sent {
                    state: SendState::NotSent("unsupported in this session".into()),
                    sent: false,
                };
            }
        };
        Sent {
            state: match sent {
                Ok(signature) => self.outcome(signature).await,
                Err(reason) => SendState::failed(reason),
            },
            sent: true,
        }
    }

    async fn take(&self, side: Side, size: f64, limit: f64) -> SeatResult {
        let (tick, lot) = self.grid();
        let (Some(size), Some(limit)) = (
            on_grid(size, lot, false),
            on_grid(limit, tick, side == Side::Ask),
        ) else {
            return Err("a price or size no market grid can hold".into());
        };
        if size.is_zero() {
            return Ok(());
        }
        self.take_exact(core_side(side), size, limit)
            .await
            .map(|_| ())
    }

    async fn rest(&self, side: Side, price: f64, size: f64) -> SeatResult {
        let (tick, lot) = self.grid();
        let (Some(size), Some(price)) = (
            on_grid(size, lot, false),
            on_grid(price, tick, side == Side::Ask),
        ) else {
            return Err("a price or size no market grid can hold".into());
        };
        if size.is_zero() || price.is_zero() {
            return Ok(());
        }
        self.rest_exact(core_side(side), price, size, false, None)
            .await
            .map(|_| ())
    }

    async fn cancel_all(&self) -> SeatResult {
        self.cancel_exact().await.map(|_| ())
    }

    async fn top(&self) -> SeatResult<(Option<f64>, Option<f64>)> {
        let book = match &self.trader {
            Trader::Phoenix(t) => t.order_book().await.map_err(|e| e.to_string())?,
            Trader::OpenBook(t) => {
                let unix = unix_timestamp(t.chain()).await.map_err(|e| e.to_string())?;
                t.order_book(unix.max(0) as u64)
                    .await
                    .map_err(|e| e.to_string())?
            }
        };
        let best = |levels: &[PriceLevel]| levels.first().map(|l| grader_f64(l.price));
        Ok((best(&book.bids), best(&book.asks)))
    }

    /// The base asset the account holds, in its wallet and on the book.
    async fn position(&self) -> SeatResult<f64> {
        let held = match &self.trader {
            Trader::Phoenix(t) => t.holdings().await.map_err(|e| e.to_string())?,
            Trader::OpenBook(t) => {
                openbook_v2::balances(t.chain(), t.market(), &self.signer.pubkey())
                    .await
                    .map_err(|e| e.to_string())?
            }
        };
        Ok(grader_f64(
            held.base.wallet + held.base.free + held.base.locked,
        ))
    }

    fn tick(&self) -> f64 {
        self.grid().0.to_string().parse().unwrap_or(f64::NAN)
    }
}

/// What a spot component's `start` gets as `venue`: every market the
/// session may trade, and the one a line that names none goes to.
#[derive(serde::Serialize)]
pub struct SessionVenues {
    orders: String,
    markets: Vec<SessionMarket>,
}

#[derive(serde::Serialize)]
struct SessionMarket {
    venue: String,
    symbol: String,
    program: String,
    market: String,
    base_mint: String,
    quote_mint: String,
}

impl SessionVenues {
    /// The `symbol` markets the profiles name on `venues`, lines naming
    /// none going to the first.
    pub fn of(venues: &[String], symbol: &str) -> Result<Self> {
        Ok(Self {
            orders: venues.first().cloned().unwrap_or_default(),
            markets: venues
                .iter()
                .map(|venue| {
                    let program = match venue.as_str() {
                        "phoenix" => phoenix_v1::PROGRAM_ID,
                        "openbook" => openbook_v2::PROGRAM_ID,
                        other => return Err(not_a_book(other)),
                    };
                    let named = named_markets(venue)?
                        .iter()
                        .find(|m| m.symbol == symbol)
                        .ok_or_else(|| Error::UnknownMarket(format!("{venue}:spot:{symbol}")))?;
                    Ok(SessionMarket {
                        venue: venue.clone(),
                        symbol: named.symbol.into(),
                        program: program.to_string(),
                        market: named.address.to_string(),
                        base_mint: named.base_mint.to_string(),
                        quote_mint: named.quote_mint.to_string(),
                    })
                })
                .collect::<Result<_>>()?,
        })
    }
}

/// `x` on a grid of `step`, rounded down or up; `None` for a value no grid
/// can hold (not a number, or overflowing a `Decimal`): the order is
/// refused. The value is first snapped to nine decimals, so float noise
/// (`0.7 - 0.4` is 0.29999999999999993) doesn't cost a step.
fn on_grid(x: f64, step: Decimal, up: bool) -> Option<Decimal> {
    let x = Decimal::try_from(x).ok()?.round_dp(9);
    let steps = x.checked_div(step)?;
    let steps = if up { steps.ceil() } else { steps.floor() };
    steps.checked_mul(step)
}

fn core_side(side: Side) -> CoreSide {
    match side {
        Side::Bid => CoreSide::Buy,
        Side::Ask => CoreSide::Sell,
    }
}

/// The world's clock, in Unix seconds.
pub async fn unix_timestamp(chain: &dyn ChainClient) -> Result<i64> {
    let clock = solana_pubkey::pubkey!("SysvarC1ock11111111111111111111111111111111");
    let account = market_account(chain, &clock).await?;
    // The clock sysvar: slot, epoch start, epoch, leader schedule epoch,
    // then the Unix timestamp.
    account
        .get(32..40)
        .and_then(|b| b.try_into().ok())
        .map(i64::from_le_bytes)
        .ok_or_else(|| Error::UnknownMarket("the clock sysvar".into()))
}

async fn market_account(chain: &dyn ChainClient, address: &Pubkey) -> Result<Vec<u8>> {
    Ok(chain
        .get_account(address)
        .await
        .map_err(venue_error("market"))?
        .ok_or_else(|| Error::UnknownMarket(address.to_string()))?
        .data)
}

fn venue_error<E: std::fmt::Display>(what: &'static str) -> impl Fn(E) -> Error {
    move |error| Error::Transaction {
        label: what.into(),
        reason: error.to_string(),
    }
}

fn not_a_book(venue: &str) -> Error {
    Error::Transaction {
        label: "a spot venue".into(),
        reason: format!("{venue} is not a spot venue: {VENUES:?}"),
    }
}

#[cfg(test)]
mod tests {
    use mtx_kit::{Intent, Market, Position, Session, VenueInfo};

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
        let venues = SessionVenues::of(&["openbook".into(), "phoenix".into()], "SOL-USDC").unwrap();
        assert!(SessionVenues::of(&["raydium".into()], "SOL-USDC").is_err());
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
        let mut strategy = crate::wasm::WasmStrategy::load(
            &path,
            "http://127.0.0.1:1",
            Box::new(std::io::sink()),
            7,
            100 * crate::wasm::FUEL_PER_CALL,
        )
        .await
        .unwrap();
        let venues: Vec<String> = vec!["phoenix".into(), "openbook".into()];
        let mandate = mtx_core::Mandate::spot(&VENUES, crate::spot::SYMBOL, 10.0);
        let own = mandate.universe[0].instrument.clone();
        let session = Session {
            steps: 20,
            slots_per_step: 2,
            start_slot: 100,
            end_slot: 140,
            symbol: crate::spot::SYMBOL.into(),
            start_mark: 150.0,
            start_oracle: 150.0,
            cap: 10.0,
            max_drawdown: 1_500.0,
            venues: VenueInfo::of_mandate(&mandate, &own, |_| true),
            mandate,
            venue: serde_json::to_string(&SessionVenues::of(&venues, crate::spot::SYMBOL).unwrap())
                .unwrap(),
            authority: String::new(),
            vars: Vec::new(),
            seed: 7,
            reports: mtx_kit::Reports::default(),
            applies_tx_options: false,
        };
        assert!(matches!(
            strategy.start(&session).await,
            crate::wasm::Outcome::Done(())
        ));
        let market = |k| Market {
            best_bid: Some(149.9),
            best_ask: Some(150.1),
            last: false,
            k,
            slot: 100 + 2 * k,
            mark: 150.0,
            oracle: 150.0,
            index: 150.0,
            position: Position {
                size: 10.0,
                entry_usd: 150.0,
            },
            cap: 10.0,
            max_drawdown: 1_500.0,
            ..Market::default()
        };
        // It reads the books through rpc, which this test doesn't answer,
        // and trades anyway: a sell, and the buyer's buy.
        let crate::wasm::Outcome::Done(first) = strategy.step(&market(0)).await else {
            panic!()
        };
        let sides: Vec<Side> = first
            .iter()
            .map(|(intent, _)| match intent {
                Intent::Market { side, .. } => *side,
                other => panic!("{other:?}"),
            })
            .collect();
        assert_eq!(sides, [Side::Ask, Side::Bid]);
        let crate::wasm::Outcome::Done(second) = strategy.step(&market(1)).await else {
            panic!()
        };
        assert!(second.is_empty());
    }
}
