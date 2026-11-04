//! A trader's handle on one Phoenix market.
//!
//! [`PhoenixTrader`] reads (mark price, top of book, position) through
//! Hawkeye, Phoenix's read-only view program, and turns orders into
//! [`ExecutionPlan`]s. It never holds a key: the methods that send take a
//! [`TransactionSigner`], and every transaction is simulated right before it
//! is signed ([`chain::execute`](crate::chain::execute)).

use std::sync::Arc;

use phoenix_rise::{
    core::{LimitOrderTicket, MarketOrderTicket},
    ix::hawkeye::{ViewAssetReturn, ViewBboReturn},
    math::Side as PhoenixSide,
};
use rust_decimal::{Decimal, prelude::ToPrimitive};
use solana_pubkey::Pubkey;
use solana_transaction::Signature;

use super::{COMPUTE_UNIT_LIMIT, PhoenixError, PhoenixExchange, PhoenixMarket};
use crate::{
    chain::{
        self, ChainClient, ExecuteOptions, ExecutionPlan, InstructionGroup, RpcChainClient,
        TransactionSigner,
    },
    core::{Order, OrderBook, OrderType, Position, Side, TimeInForce},
};

/// An order in the form Phoenix takes it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PhoenixOrder {
    /// Rests on the book at `price`, filling first against anything it
    /// crosses.
    Limit {
        /// Buy or sell.
        side: Side,
        /// Limit price, USDC per base unit, on the tick grid.
        price: Decimal,
        /// Base units, on the lot grid.
        quantity: Decimal,
    },
    /// Fills against the book now; whatever does not fill is cancelled.
    Market {
        /// Buy or sell.
        side: Side,
        /// Base units, on the lot grid.
        quantity: Decimal,
        /// Fill no worse than this price, if given (on the tick grid).
        worst_price: Option<Decimal>,
        /// Fill all of `quantity` or nothing.
        all_or_none: bool,
    },
}

impl PhoenixOrder {
    /// How a core [`Order`] maps onto Phoenix.
    ///
    /// | Order | Phoenix |
    /// |-------|---------|
    /// | limit, GTC | limit order |
    /// | limit, IOC | market order bounded by the limit price |
    /// | limit, FOK | market order bounded by the limit price, all or none |
    /// | market | market order, bounded by `price` if set; all or none for FOK |
    pub fn from_order(order: &Order) -> Result<Self, PhoenixError> {
        let fok = order.time_in_force == TimeInForce::Fok;
        match order.order_type {
            OrderType::Limit => {
                let price = order.price.ok_or_else(|| {
                    PhoenixError::InvalidOrder("a limit order needs a price".into())
                })?;
                Ok(match order.time_in_force {
                    TimeInForce::Gtc => Self::Limit {
                        side: order.side,
                        price,
                        quantity: order.quantity,
                    },
                    _ => Self::Market {
                        side: order.side,
                        quantity: order.quantity,
                        worst_price: Some(price),
                        all_or_none: fok,
                    },
                })
            }
            OrderType::Market => Ok(Self::Market {
                side: order.side,
                quantity: order.quantity,
                worst_price: order.price,
                all_or_none: fok,
            }),
        }
    }

    fn label(&self) -> &'static str {
        match self {
            Self::Limit { .. } => "limit order",
            Self::Market { .. } => "market order",
        }
    }
}

fn phoenix_side(side: Side) -> PhoenixSide {
    match side {
        Side::Buy => PhoenixSide::Bid,
        Side::Sell => PhoenixSide::Ask,
    }
}

/// One trader account in one Phoenix market.
pub struct PhoenixTrader {
    exchange: Arc<PhoenixExchange>,
    market: PhoenixMarket,
    authority: Pubkey,
    account: Pubkey,
    rpc_url: String,
    chain: Arc<dyn ChainClient>,
    options: ExecuteOptions,
    /// The next order's client order id. Without one, two identical orders
    /// on the same blockhash are the same transaction, and the chain runs it
    /// once: a bot slicing a position into equal orders within a slot gets
    /// one slice. Counting from 1 keeps a replayed session the same.
    next_order_id: std::sync::atomic::AtomicU64,
}

impl PhoenixTrader {
    /// A trader for `authority`'s cross-margin account in `symbol`, talking
    /// to `rpc_url`.
    ///
    /// Receipts are not fetched after sending ([`ExecuteOptions::without_receipts`]):
    /// Phoenix fills surface in the position, which [`position`](Self::position)
    /// reads directly. Override with [`with_execute_options`](Self::with_execute_options).
    pub fn new(
        exchange: Arc<PhoenixExchange>,
        symbol: &str,
        authority: Pubkey,
        rpc_url: impl Into<String>,
    ) -> Result<Self, PhoenixError> {
        let rpc_url = rpc_url.into();
        Ok(Self {
            market: exchange.market(symbol)?,
            account: exchange.trader_account(&authority),
            chain: Arc::new(RpcChainClient::new(rpc_url.clone())),
            exchange,
            authority,
            rpc_url,
            options: ExecuteOptions::without_receipts(),
            next_order_id: std::sync::atomic::AtomicU64::new(1),
        })
    }

    fn order_id(&self) -> u128 {
        u128::from(
            self.next_order_id
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed),
        )
    }

    /// Sends through `chain` instead of a fresh RPC client.
    pub fn with_chain(mut self, chain: Arc<dyn ChainClient>) -> Self {
        self.chain = chain;
        self
    }

    /// Execution options for every plan this trader sends.
    pub fn with_execute_options(mut self, options: ExecuteOptions) -> Self {
        self.options = options;
        self
    }

    /// The exchange metadata.
    pub fn exchange(&self) -> &PhoenixExchange {
        &self.exchange
    }

    /// The market traded.
    pub fn market(&self) -> &PhoenixMarket {
        &self.market
    }

    /// The wallet that owns the trader account and signs.
    pub fn authority(&self) -> Pubkey {
        self.authority
    }

    /// The cross-margin trader account (PDA) holding the collateral.
    pub fn account(&self) -> Pubkey {
        self.account
    }

    /// The chain client plans are executed through.
    pub fn chain(&self) -> &dyn ChainClient {
        &*self.chain
    }

    /// The current slot.
    pub async fn slot(&self) -> Result<u64, PhoenixError> {
        Ok(self.chain.get_slot().await?)
    }

    /// Hawkeye's best bid and offer, mark and index prices, in ticks.
    pub async fn bbo(&self) -> Result<ViewBboReturn, PhoenixError> {
        Ok(self
            .exchange
            .hawkeye(&self.rpc_url, self.authority)
            .view_bbo(self.market.symbol())
            .await?
            .value)
    }

    /// The mark price, USDC per base unit.
    pub async fn mark_price(&self) -> Result<Decimal, PhoenixError> {
        Ok(self
            .market
            .ticks_to_price(self.bbo().await?.mark_price_ticks))
    }

    /// The top of the book. Sizes are not reported (see
    /// [`PhoenixMarket::book_from_bbo`]).
    pub async fn order_book(&self) -> Result<OrderBook, PhoenixError> {
        let bbo = self.bbo().await?;
        Ok(self
            .market
            .book_from_bbo(&bbo, Some(bbo.mark_price_last_updated_slot)))
    }

    /// Hawkeye's margin view of the account in this market, in Phoenix units
    /// (base lots, quote lots, ticks).
    pub async fn margin(&self) -> Result<ViewAssetReturn, PhoenixError> {
        Ok(self
            .exchange
            .hawkeye(&self.rpc_url, self.authority)
            .view_margin_for_asset(self.authority, 0, 0, self.market.asset_id)
            .await?
            .value)
    }

    /// The account's position in the market, as Phoenix reports it: signed
    /// size and average entry. Realized PnL is not part of the view.
    pub async fn position(&self) -> Result<Position, PhoenixError> {
        let asset = self.margin().await?;
        Ok(self
            .market
            .position(asset.base_lots, asset.entry_price_quote_lots_per_base_lot))
    }

    fn lots(&self, quantity: Decimal) -> Result<u64, PhoenixError> {
        let lots = self.market.quantity_to_base_lots(quantity)?;
        if lots == 0 {
            return Err(PhoenixError::InvalidOrder(format!(
                "quantity {quantity} is zero lots"
            )));
        }
        Ok(lots)
    }

    /// Validates a price on the tick grid and hands it to Phoenix's ticket,
    /// which takes a float. An on-grid decimal survives the trip: the ticket
    /// re-parses the float's shortest representation, which is the decimal.
    fn ticket_price(&self, price: Decimal) -> Result<f64, PhoenixError> {
        if self.market.price_to_ticks(price)? == 0 {
            return Err(PhoenixError::InvalidOrder(format!(
                "price {price} is zero ticks"
            )));
        }
        price
            .to_f64()
            .ok_or_else(|| PhoenixError::InvalidOrder(format!("price {price} is out of range")))
    }

    /// The plan for one order: validated against the grid, built by
    /// Phoenix's SDK, packed into one transaction.
    pub async fn plan(&self, order: &PhoenixOrder) -> Result<ExecutionPlan, PhoenixError> {
        let symbol = self.market.symbol();
        let instructions = match *order {
            PhoenixOrder::Limit {
                side,
                price,
                quantity,
            } => {
                let ticket = LimitOrderTicket::builder()
                    .authority(self.authority)
                    .trader_account(self.account)
                    .symbol(symbol)
                    .side(phoenix_side(side))
                    .price(self.ticket_price(price)?)
                    .num_base_lots(self.lots(quantity)?)
                    .client_order_id(self.order_id())
                    .build()?;
                self.exchange.tx().place_limit_order(ticket).await?
            }
            PhoenixOrder::Market {
                side,
                quantity,
                worst_price,
                all_or_none,
            } => {
                let lots = self.lots(quantity)?;
                let mut ticket = MarketOrderTicket::builder()
                    .authority(self.authority)
                    .trader_account(self.account)
                    .symbol(symbol)
                    .side(phoenix_side(side))
                    .num_base_lots(lots)
                    .client_order_id(self.order_id());
                if let Some(price) = worst_price {
                    ticket = ticket.price(self.ticket_price(price)?);
                }
                if all_or_none {
                    ticket = ticket.min_base_lots_to_fill(lots);
                }
                self.exchange
                    .tx()
                    .place_market_order(ticket.build()?)
                    .await?
            }
        };
        self.pack(order.label(), instructions)
    }

    /// The plan to cancel every open order of the account in the market.
    pub fn plan_cancel_all(&self) -> Result<ExecutionPlan, PhoenixError> {
        let instructions = self.exchange.tx().build_cancel_all_orders(
            self.authority,
            self.account,
            self.market.symbol(),
        )?;
        self.pack("cancel all", instructions)
    }

    fn pack(
        &self,
        label: &str,
        instructions: Vec<solana_transaction::Instruction>,
    ) -> Result<ExecutionPlan, PhoenixError> {
        Ok(chain::pack(
            &self.authority,
            &[InstructionGroup::new(label, instructions)],
            COMPUTE_UNIT_LIMIT,
        )?)
    }

    /// Executes a plan: simulate, sign, send, confirm, step by step.
    /// Returns the last step's signature.
    pub async fn execute(
        &self,
        plan: &ExecutionPlan,
        signer: &dyn TransactionSigner,
    ) -> Result<Signature, PhoenixError> {
        let receipts = chain::execute(plan, signer, &*self.chain, &[], self.options).await?;
        Ok(receipts
            .last()
            .map(|r| r.signature)
            .expect("execute returns one receipt per step of a non-empty plan"))
    }

    /// Plans and executes one order.
    pub async fn submit(
        &self,
        order: &PhoenixOrder,
        signer: &dyn TransactionSigner,
    ) -> Result<Signature, PhoenixError> {
        let plan = self.plan(order).await?;
        self.execute(&plan, signer).await
    }

    /// Rests a limit order at `price` (or fills against the book if it
    /// crosses).
    pub async fn place_limit(
        &self,
        signer: &dyn TransactionSigner,
        side: Side,
        price: Decimal,
        quantity: Decimal,
    ) -> Result<Signature, PhoenixError> {
        self.submit(
            &PhoenixOrder::Limit {
                side,
                price,
                quantity,
            },
            signer,
        )
        .await
    }

    /// Fills against the book now, at no worse than `worst_price` if given.
    pub async fn place_market(
        &self,
        signer: &dyn TransactionSigner,
        side: Side,
        quantity: Decimal,
        worst_price: Option<Decimal>,
    ) -> Result<Signature, PhoenixError> {
        self.submit(
            &PhoenixOrder::Market {
                side,
                quantity,
                worst_price,
                all_or_none: false,
            },
            signer,
        )
        .await
    }

    /// Cancels every open order of the account in the market.
    pub async fn cancel_all(
        &self,
        signer: &dyn TransactionSigner,
    ) -> Result<Signature, PhoenixError> {
        let plan = self.plan_cancel_all()?;
        self.execute(&plan, signer).await
    }
}

#[cfg(test)]
mod tests {
    use rust_decimal_macros::dec;

    use super::*;
    use crate::core::{InstrumentId, VenueId};

    fn order(order_type: OrderType, tif: TimeInForce, price: Option<Decimal>) -> Order {
        let mut o = Order::market(
            InstrumentId::new("phoenix:SOL"),
            VenueId::new("phoenix"),
            Side::Buy,
            dec!(1.5),
            price,
        );
        o.order_type = order_type;
        o.time_in_force = tif;
        o
    }

    #[test]
    fn maps_core_orders_onto_phoenix() {
        assert_eq!(
            PhoenixOrder::from_order(&order(OrderType::Limit, TimeInForce::Gtc, Some(dec!(150))))
                .unwrap(),
            PhoenixOrder::Limit {
                side: Side::Buy,
                price: dec!(150),
                quantity: dec!(1.5)
            }
        );
        assert_eq!(
            PhoenixOrder::from_order(&order(OrderType::Limit, TimeInForce::Ioc, Some(dec!(150))))
                .unwrap(),
            PhoenixOrder::Market {
                side: Side::Buy,
                quantity: dec!(1.5),
                worst_price: Some(dec!(150)),
                all_or_none: false
            }
        );
        assert_eq!(
            PhoenixOrder::from_order(&order(OrderType::Limit, TimeInForce::Fok, Some(dec!(150))))
                .unwrap(),
            PhoenixOrder::Market {
                side: Side::Buy,
                quantity: dec!(1.5),
                worst_price: Some(dec!(150)),
                all_or_none: true
            }
        );
        assert_eq!(
            PhoenixOrder::from_order(&order(OrderType::Market, TimeInForce::Ioc, None)).unwrap(),
            PhoenixOrder::Market {
                side: Side::Buy,
                quantity: dec!(1.5),
                worst_price: None,
                all_or_none: false
            }
        );
        assert!(matches!(
            PhoenixOrder::from_order(&order(OrderType::Limit, TimeInForce::Gtc, None)),
            Err(PhoenixError::InvalidOrder(_))
        ));
    }
}
