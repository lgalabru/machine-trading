//! Phoenix Eternal's perpetuals as [`Market`]s: one perp of the snapshot's
//! exchange (`specs/instruments/perp.md`), read through Phoenix's Hawkeye
//! views, and seats trading it as the kit's [`Trader`] does. Phoenix offers
//! them ([`super::phoenix`]).

use std::sync::Arc;

use async_trait::async_trait;
use mtx_core::{
    AccountMeter, Holding,
    mandate::{InstrumentRef, Kind},
};
use mtx_kit::{
    Action, BotEnv, Planned, Sent, Side, Trader, chain::RpcChainClient,
    phoenix_rise::types::exchange::ExchangeResponse,
    protocols::phoenix::eternal::PhoenixEternalMeter,
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

/// An account on a Phoenix perpetual: the kit's [`Trader`], its orders
/// placed as a bot's run loop places them.
pub struct PerpSeat {
    market: String,
    trader: Arc<Trader>,
    /// Its trader sends through the transaction layer's pool: every send
    /// must go through its chain client, which returns without waiting.
    pooled: bool,
}

impl PerpSeat {
    pub fn new(market: String, trader: Arc<Trader>) -> Self {
        Self {
            market,
            trader,
            pooled: false,
        }
    }

    /// This seat, its sends through the transaction layer's pool if
    /// `pooled`.
    pub fn pooled(self, pooled: bool) -> Self {
        Self { pooled, ..self }
    }

    /// Cancel every resting order through the trader's chain client, not
    /// its blocking RPC client, which would wait for a block the pool has
    /// yet to build.
    async fn cancel_all_pooled(&self) -> Sent {
        let trader = &self.trader;
        let instructions = match trader.phoenix.tx().build_cancel_all_orders(
            trader.authority(),
            trader.account,
            &trader.env.symbol,
        ) {
            Ok(instructions) => instructions,
            Err(e) => {
                return Sent {
                    state: mtx_kit::SendState::failed(e.to_string()),
                    sent: false,
                };
            }
        };
        let plan = mtx_kit::chain::ExecutionPlan {
            payer: trader.authority(),
            steps: vec![mtx_kit::chain::TransactionPlan::from_group(
                &mtx_kit::chain::InstructionGroup::new("cancel all", instructions),
                mtx_kit::protocols::phoenix::eternal::COMPUTE_UNIT_LIMIT,
            )],
        };
        match trader.inner.execute(&plan, &trader.keypair).await {
            Ok(signature) => Sent {
                state: mtx_kit::SendState::Landed(signature.to_string()),
                sent: true,
            },
            Err(e) => Sent {
                state: mtx_kit::SendState::failed(e.to_string()),
                sent: true,
            },
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
        if self.pooled && matches!(planned.action, Action::CancelAll) {
            return self.cancel_all_pooled().await;
        }
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
