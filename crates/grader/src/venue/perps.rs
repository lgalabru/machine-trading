//! Phoenix Eternal's perpetuals as [`Market`]s: one perp of the snapshot's
//! exchange (`specs/instruments/perp.md`), read through Phoenix's Hawkeye
//! views, and seats trading it as the kit's [`Trader`] does. Phoenix offers
//! them ([`super::phoenix`]).

use std::sync::{
    Arc,
    atomic::{AtomicU32, Ordering},
};

use async_trait::async_trait;
use mtx_core::{
    AccountMeter, Holding,
    mandate::{InstrumentRef, Kind},
};
use mtx_kit::{
    Action, BotEnv, Decimal, Planned, Sent, Side, Trader,
    chain::{ExecutionPlan, RpcChainClient},
    core::{Rounding, Side as CoreSide},
    phoenix_rise::types::exchange::ExchangeResponse,
    protocols::phoenix::eternal::{PhoenixEternalMeter, PhoenixOrder},
};
use solana_keypair::Keypair;
use solana_pubkey::Pubkey;

use super::{Market, Seat, SeatResult};
use crate::{
    Error, Result,
    phoenix::Phoenix,
    session::{INTERNAL_RPC_TIMEOUT, patiently},
};

/// One perpetual of a Phoenix Eternal exchange.
pub struct PerpMarket {
    instrument: InstrumentRef,
    phoenix: Phoenix,
    exchange_path: std::path::PathBuf,
    rpc_url: String,
    market: Pubkey,
    /// The fee payer of its Hawkeye reads: an account that exists.
    reader: Pubkey,
}

impl PerpMarket {
    /// `symbol`'s perpetual on the exchange the snapshot holds (its
    /// metadata, and the file it was read from), read through `rpc_url`
    /// with `reader`, an account that exists, as Hawkeye's fee payer.
    pub fn new(
        exchange: ExchangeResponse,
        exchange_path: std::path::PathBuf,
        symbol: &str,
        rpc_url: &str,
        reader: Pubkey,
    ) -> Result<Self> {
        let address = mtx_kit::venue::key(
            &exchange
                .markets
                .iter()
                .find(|m| m.symbol == symbol)
                .ok_or_else(|| Error::UnknownMarket(symbol.into()))?
                .market_pubkey,
        )
        .map_err(Error::InvalidExchangeKey)?;
        Ok(Self {
            instrument: InstrumentRef {
                venue: crate::scenario::VENUE.into(),
                kind: Kind::Perp,
                symbol: symbol.into(),
            },
            phoenix: Phoenix::new(exchange, rpc_url),
            exchange_path,
            rpc_url: rpc_url.to_string(),
            market: address,
            reader,
        })
    }

    /// The kit's trader for `signer` on this market, through `rpc_url`, as
    /// a bot's run loop would make it.
    pub fn trader(&self, signer: Keypair, rpc_url: &str) -> Result<Trader> {
        Ok(Trader::with_keypair(
            &BotEnv {
                rpc_url: rpc_url.to_string(),
                keypair_path: std::path::PathBuf::new(),
                exchange_path: self.exchange_path.clone(),
                symbol: self.instrument.symbol.clone(),
                end_slot: u64::MAX,
                max_position: f64::MAX,
                max_drawdown_usd: f64::MAX,
                mandate_path: None,
            },
            signer,
        )?)
    }
}

#[async_trait]
impl Market for PerpMarket {
    fn instrument(&self) -> &InstrumentRef {
        &self.instrument
    }

    fn address(&self) -> Pubkey {
        self.market
    }

    async fn top(&self) -> Result<(Option<f64>, Option<f64>)> {
        let symbol = &self.instrument.symbol;
        let market = self.phoenix.market(symbol)?;
        let hawkeye = self.phoenix.hawkeye(self.reader);
        let bbo = patiently("the book", || hawkeye.view_bbo(symbol))
            .await?
            .value;
        let usd = |ticks| self.phoenix.ticks_to_usd(market, ticks);
        Ok((bbo.best_bid_ticks().map(usd), bbo.best_ask_ticks().map(usd)))
    }

    /// The account's position, as Phoenix's meter reads it.
    async fn holdings(&self, owner: &Pubkey) -> Result<Vec<Holding>> {
        let meter = PhoenixEternalMeter::new(
            self.phoenix.exchange().clone(),
            &self.rpc_url,
            Arc::new(RpcChainClient::with_timeout(
                &self.rpc_url,
                INTERNAL_RPC_TIMEOUT,
            )),
            *owner,
            &self.instrument.symbol,
        )
        .map_err(mtx_kit::Error::from)?;
        let id = self.instrument.id();
        let measured = patiently("the meter", || meter.measure(std::slice::from_ref(&id))).await?;
        Ok(measured
            .holdings
            .into_iter()
            .filter(|(held, _)| *held == id)
            .map(|(_, holding)| holding)
            .collect())
    }

    async fn seat(&self, signer: Keypair, rpc_url: &str) -> Result<Box<dyn Seat>> {
        Ok(Box::new(PerpSeat::new(
            self.instrument.to_string(),
            Arc::new(
                self.trader(signer, rpc_url)?
                    .with_timeout(INTERNAL_RPC_TIMEOUT),
            ),
        )))
    }
}

impl PerpMarket {
    /// A seat for `signer` whose every transaction is unique among its own
    /// ([`UniquePerpSeat`]).
    pub fn unique_seat(&self, signer: Keypair, rpc_url: &str) -> Result<Box<dyn Seat>> {
        let trader = self
            .trader(signer.insecure_clone(), rpc_url)?
            .with_timeout(INTERNAL_RPC_TIMEOUT);
        Ok(Box::new(UniquePerpSeat {
            seat: PerpSeat::new(self.instrument.to_string(), Arc::new(trader)),
            signer,
            sends: AtomicU32::new(0),
            last: std::sync::Mutex::default(),
        }))
    }
}

/// An account on a Phoenix perpetual: the kit's [`Trader`], its orders
/// placed as a bot's run loop places them.
pub struct PerpSeat {
    market: String,
    trader: Arc<Trader>,
    /// The signature of its last market order that landed.
    last: std::sync::Mutex<Option<String>>,
}

impl PerpSeat {
    pub fn new(market: String, trader: Arc<Trader>) -> Self {
        Self {
            market,
            trader,
            last: std::sync::Mutex::default(),
        }
    }
}

#[async_trait]
impl Seat for PerpSeat {
    fn market(&self) -> &str {
        &self.market
    }

    fn executes(&self, action: &Action) -> bool {
        Trader::executes(action)
    }

    async fn send(&self, planned: &Planned) -> Sent {
        self.trader.send_planned(planned).await
    }

    async fn take(&self, side: Side, size: f64, limit: f64) -> SeatResult {
        let signature = self
            .trader
            .place_market(side, size, limit)
            .await
            .map_err(|e| e.to_string())?;
        *self.last.lock().expect("seat lock") = Some(signature.to_string());
        Ok(())
    }

    /// The signature of the last market order [`Seat::take`] landed.
    fn last_signature(&self) -> Option<String> {
        self.last.lock().expect("seat lock").clone()
    }

    async fn rest(&self, side: Side, price: f64, size: f64) -> SeatResult {
        self.trader
            .place_limit(side, price, size)
            .await
            .map(|_| ())
            .map_err(|e| e.to_string())
    }

    async fn cancel_all(&self) -> SeatResult {
        self.trader
            .cancel_all()
            .map(|_| ())
            .map_err(|e| e.to_string())
    }

    /// The book's best bid and ask at the market's exact tick prices.
    async fn top(&self) -> SeatResult<(Option<f64>, Option<f64>)> {
        let trader = &self.trader;
        let bbo = patiently("the seat's book", || trader.inner.bbo())
            .await
            .map_err(|e| e.to_string())?;
        let market = trader.inner.market();
        let usd = |ticks: Option<u64>| {
            ticks.map(|t| {
                market
                    .ticks_to_price(t)
                    .to_string()
                    .parse::<f64>()
                    .unwrap_or(0.0)
            })
        };
        Ok((usd(bbo.best_bid_ticks()), usd(bbo.best_ask_ticks())))
    }

    /// The best resting limit orders of at least `min_size` at their price,
    /// read from the orderbook account. The splines the snapshot's makers
    /// quote through aren't in it, and are never odd lots.
    async fn top_sized(&self, min_size: f64) -> SeatResult<(Option<f64>, Option<f64>)> {
        let inner = &self.trader.inner;
        let market = inner.market();
        let key = inner
            .exchange()
            .metadata()
            .get_market(market.symbol())
            .and_then(|m| m.market_pubkey.parse::<Pubkey>().ok())
            .ok_or_else(|| format!("no orderbook account for {}", market.symbol()))?;
        let account = patiently("the orderbook account", || inner.chain().get_account(&key))
            .await
            .map_err(|e| e.to_string())?
            .ok_or_else(|| "the orderbook account is missing".to_string())?;
        let book = phoenix_rise::accounts::owned::Orderbook::try_from_account_bytes(&account.data)
            .map_err(|e| e.to_string())?;
        let sized = |entries: &[phoenix_rise::accounts::owned::OrderbookEntry],
                     best: fn(u64, u64) -> u64| {
            let mut levels: std::collections::BTreeMap<u64, u64> = Default::default();
            for entry in entries {
                *levels
                    .entry(entry.order_id.price_in_ticks.as_inner())
                    .or_default() += entry.order.num_base_lots_remaining;
            }
            levels
                .into_iter()
                .filter(|(_, lots)| {
                    market
                        .base_lots_to_quantity(*lots as i64)
                        .to_string()
                        .parse::<f64>()
                        .is_ok_and(|size| size >= min_size)
                })
                .map(|(ticks, _)| ticks)
                .reduce(best)
                .map(|ticks| {
                    market
                        .ticks_to_price(ticks)
                        .to_string()
                        .parse::<f64>()
                        .unwrap_or(0.0)
                })
        };
        Ok((sized(&book.bids, u64::max), sized(&book.asks, u64::min)))
    }

    /// Phoenix's index price.
    async fn index(&self) -> SeatResult<Option<f64>> {
        let trader = &self.trader;
        patiently("the seat's index", || trader.top())
            .await
            .map(|top| Some(top.index))
            .map_err(|e| e.to_string())
    }

    async fn position(&self) -> SeatResult<f64> {
        let trader = &self.trader;
        patiently("the seat's position", || trader.position())
            .await
            .map(|p| p.size)
            .map_err(|e| e.to_string())
    }

    fn tick(&self) -> f64 {
        self.trader
            .inner
            .market()
            .instrument
            .tick_size
            .to_string()
            .parse()
            .unwrap_or(f64::NAN)
    }
}

/// A [`PerpSeat`] whose transactions are each unique among its own: a
/// library actor's (see [`crate::actors::library`]), which may send the
/// same order twice in a slot. Two such would otherwise be the same
/// transaction, signed alike, and the second would never land. Its `n`th
/// send asks for `n % 1024 + 1` fewer compute units than Phoenix's plan.
pub struct UniquePerpSeat {
    seat: PerpSeat,
    signer: Keypair,
    sends: AtomicU32,
    last: std::sync::Mutex<Option<String>>,
}

impl UniquePerpSeat {
    async fn execute<E: std::fmt::Display>(
        &self,
        plan: std::result::Result<ExecutionPlan, E>,
    ) -> SeatResult {
        let mut plan = plan.map_err(|e| e.to_string())?;
        let n = self.sends.fetch_add(1, Ordering::Relaxed);
        for step in &mut plan.steps {
            step.compute_unit_limit = step.compute_unit_limit.saturating_sub(1 + n % 1024);
        }
        let signature = self
            .seat
            .trader
            .inner
            .execute(&plan, &self.signer)
            .await
            .map_err(|e| e.to_string())?;
        *self.last.lock().expect("seat lock") = Some(signature.to_string());
        Ok(())
    }

    /// `size` base units on the lot grid, and `price` on the tick grid the
    /// way Phoenix's tickets round it: bids down, asks up.
    fn order_terms(
        &self,
        side: Side,
        size: f64,
        price: f64,
    ) -> Result<(CoreSide, Decimal, Decimal)> {
        let trader = &self.seat.trader;
        let instrument = &trader.inner.market().instrument;
        let invalid = |what: String| Error::Transaction {
            label: "an actor's order".into(),
            reason: what,
        };
        let quantity = instrument.lots_to_quantity(trader.lots(size)?);
        let (core, rounding) = match side {
            Side::Bid => (CoreSide::Buy, Rounding::Down),
            Side::Ask => (CoreSide::Sell, Rounding::Up),
        };
        let price: Decimal = price
            .to_string()
            .parse()
            .map_err(|_| invalid(format!("price {price} isn't a number")))?;
        let price = instrument
            .checked_round_price(price, rounding)
            .ok_or_else(|| invalid(format!("price {price} is past the tick grid")))?;
        Ok((core, quantity, price))
    }
}

#[async_trait]
impl Seat for UniquePerpSeat {
    fn market(&self) -> &str {
        self.seat.market()
    }

    fn executes(&self, action: &Action) -> bool {
        self.seat.executes(action)
    }

    async fn send(&self, planned: &Planned) -> Sent {
        self.seat.send(planned).await
    }

    async fn take(&self, side: Side, size: f64, limit: f64) -> SeatResult {
        let (side, quantity, worst) = self
            .order_terms(side, size, limit)
            .map_err(|e| e.to_string())?;
        let order = PhoenixOrder::Market {
            side,
            quantity,
            worst_price: Some(worst),
            all_or_none: false,
        };
        let plan = self.seat.trader.inner.plan(&order).await;
        self.execute(plan).await
    }

    async fn rest(&self, side: Side, price: f64, size: f64) -> SeatResult {
        let (side, quantity, price) = self
            .order_terms(side, size, price)
            .map_err(|e| e.to_string())?;
        let order = PhoenixOrder::Limit {
            side,
            price,
            quantity,
        };
        let plan = self.seat.trader.inner.plan(&order).await;
        self.execute(plan).await
    }

    async fn cancel_all(&self) -> SeatResult {
        let plan = self.seat.trader.inner.plan_cancel_all();
        self.execute(plan).await
    }

    async fn top(&self) -> SeatResult<(Option<f64>, Option<f64>)> {
        self.seat.top().await
    }

    async fn top_sized(&self, min_size: f64) -> SeatResult<(Option<f64>, Option<f64>)> {
        self.seat.top_sized(min_size).await
    }

    async fn position(&self) -> SeatResult<f64> {
        self.seat.position().await
    }

    fn tick(&self) -> f64 {
        self.seat.tick()
    }

    async fn index(&self) -> SeatResult<Option<f64>> {
        self.seat.index().await
    }

    fn last_signature(&self) -> Option<String> {
        self.last.lock().expect("seat lock").clone()
    }
}
