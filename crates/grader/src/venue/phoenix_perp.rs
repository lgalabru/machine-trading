//! Phoenix Eternal's perpetuals as a [`Venue`]: one market of the snapshot's
//! exchange, read through Phoenix's Hawkeye views, and seats trading it as
//! the kit's [`Trader`] does.

use std::sync::Arc;

use async_trait::async_trait;
use mtx_core::mandate::{InstrumentRef, Kind};
use mtx_kit::{
    Action, BotEnv, Planned, Sent, Side, Trader, phoenix_rise::types::exchange::ExchangeResponse,
};
use solana_keypair::Keypair;
use solana_pubkey::Pubkey;

use super::{MarketKind, Seat, SeatResult, Venue};
use crate::{
    Error, Result,
    phoenix::Phoenix,
    session::{INTERNAL_RPC_TIMEOUT, patiently},
};

/// One perpetual of a Phoenix Eternal exchange.
pub struct PhoenixPerp {
    instrument: InstrumentRef,
    phoenix: Phoenix,
    exchange_path: std::path::PathBuf,
    market: Pubkey,
    /// The fee payer of its Hawkeye reads: an account that exists.
    reader: Pubkey,
}

impl PhoenixPerp {
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
impl Venue for PhoenixPerp {
    fn instrument(&self) -> &InstrumentRef {
        &self.instrument
    }

    fn kind(&self) -> MarketKind {
        MarketKind::Perp
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

/// An account on a Phoenix perpetual: the kit's [`Trader`], its orders
/// placed as a bot's run loop places them.
pub struct PerpSeat {
    market: String,
    trader: Arc<Trader>,
}

impl PerpSeat {
    pub fn new(market: String, trader: Arc<Trader>) -> Self {
        Self { market, trader }
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
        self.trader
            .place_market(side, size, limit)
            .await
            .map(|_| ())
            .map_err(|e| e.to_string())
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
