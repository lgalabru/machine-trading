//! [`ExecutionDriver`] over Phoenix v1.

use super::{PhoenixV1Error, PhoenixV1Order, PhoenixV1Trader};
use crate::{
    chain::{ChainError, TransactionSigner},
    core::{
        ExecutionDriver, ExecutionError, ExecutionReport, InstrumentId, Order, OrderBook, OrderId,
        OrderStatus, OrderType, TimeInForce,
    },
};

/// A [`PhoenixV1Trader`] and the signer it sends with, as an
/// [`ExecutionDriver`] for its one market.
///
/// | Order | Phoenix v1 |
/// |-------|------------|
/// | limit, GTC | limit order (needs a seat) |
/// | limit, IOC | swap bounded by the limit price |
/// | limit, FOK | swap bounded by the limit price, all or none |
/// | market | swap, bounded by `price` if set; all or none for FOK |
///
/// Fills settle inside the transaction and surface as program events,
/// which this driver does not decode: a sent order is reported
/// [`OrderStatus::Pending`] with the signature as its
/// [`venue_reference`](ExecutionReport::venue_reference). Read
/// [`PhoenixV1Trader::holdings`] to see what filled. Individual orders are
/// addressed by Phoenix sequence numbers, not this crate's [`OrderId`]:
/// [`cancel_all`](ExecutionDriver::cancel_all) is supported, and
/// [`cancel_order`](ExecutionDriver::cancel_order) is not.
pub struct PhoenixV1Driver<S> {
    trader: PhoenixV1Trader,
    signer: S,
}

impl<S: TransactionSigner> PhoenixV1Driver<S> {
    /// Drives `trader`'s market, signing with `signer`.
    pub fn new(trader: PhoenixV1Trader, signer: S) -> Self {
        Self { trader, signer }
    }

    /// The trader underneath.
    pub fn trader(&self) -> &PhoenixV1Trader {
        &self.trader
    }

    fn check_instrument(&self, instrument: &InstrumentId) -> Result<(), ExecutionError> {
        if instrument == self.trader.market().id() {
            Ok(())
        } else {
            Err(ExecutionError::InstrumentNotFound(instrument.clone()))
        }
    }
}

impl PhoenixV1Order {
    /// How a core [`Order`] maps onto Phoenix v1 (see [`PhoenixV1Driver`]).
    pub fn from_order(order: &Order) -> Result<Self, PhoenixV1Error> {
        let fok = order.time_in_force == TimeInForce::Fok;
        match (order.order_type, order.time_in_force) {
            (OrderType::Limit, TimeInForce::Gtc) => Ok(Self::Limit {
                side: order.side,
                price: order.price.ok_or_else(|| {
                    PhoenixV1Error::InvalidOrder("a limit order needs a price".into())
                })?,
                quantity: order.quantity,
            }),
            (OrderType::Limit, _) if order.price.is_none() => Err(PhoenixV1Error::InvalidOrder(
                "a limit order needs a price".into(),
            )),
            _ => Ok(Self::ImmediateOrCancel {
                side: order.side,
                quantity: order.quantity,
                worst_price: order.price,
                all_or_none: fok,
            }),
        }
    }
}

impl From<PhoenixV1Error> for ExecutionError {
    fn from(error: PhoenixV1Error) -> Self {
        match error {
            PhoenixV1Error::Instrument(_) | PhoenixV1Error::InvalidOrder(_) => Self::Rejected {
                reason: error.to_string(),
            },
            PhoenixV1Error::Chain(chain @ ChainError::Simulation { .. }) => Self::Rejected {
                reason: chain.to_string(),
            },
            PhoenixV1Error::Chain(ChainError::Rpc(detail)) => Self::Connection(detail),
            other => Self::Venue(other.to_string()),
        }
    }
}

#[async_trait::async_trait]
impl<S: TransactionSigner> ExecutionDriver for PhoenixV1Driver<S> {
    async fn submit_order(&self, order: &Order) -> Result<ExecutionReport, ExecutionError> {
        self.check_instrument(&order.instrument)?;
        let signature = self
            .trader
            .submit(&PhoenixV1Order::from_order(order)?, &self.signer)
            .await?;
        Ok(ExecutionReport {
            order_id: order.id.clone(),
            status: OrderStatus::Pending,
            fills: Vec::new(),
            venue_reference: Some(signature.to_string()),
        })
    }

    async fn cancel_order(&self, _order_id: &OrderId) -> Result<OrderStatus, ExecutionError> {
        Err(ExecutionError::Unsupported(
            "Phoenix v1 orders are addressed by Phoenix sequence numbers; use cancel_all".into(),
        ))
    }

    async fn cancel_all(&self, instrument: &InstrumentId) -> Result<(), ExecutionError> {
        self.check_instrument(instrument)?;
        let plan = self.trader.plan_cancel_all()?;
        self.trader.send(&plan, &self.signer).await?;
        Ok(())
    }

    async fn order_book(&self, instrument: &InstrumentId) -> Result<OrderBook, ExecutionError> {
        self.check_instrument(instrument)?;
        Ok(self.trader.order_book().await?)
    }
}

#[cfg(test)]
mod tests {
    use rust_decimal_macros::dec;

    use super::*;
    use crate::core::{Side, VenueId};

    fn order(
        order_type: OrderType,
        tif: TimeInForce,
        price: Option<rust_decimal::Decimal>,
    ) -> Order {
        let mut o = Order::market(
            InstrumentId::new("phoenix:spot:SOL-USDC"),
            VenueId::new("phoenix"),
            Side::Sell,
            dec!(2),
            price,
        );
        o.order_type = order_type;
        o.time_in_force = tif;
        o
    }

    #[test]
    fn core_orders_map_onto_phoenix_v1() {
        assert_eq!(
            PhoenixV1Order::from_order(&order(OrderType::Limit, TimeInForce::Gtc, Some(dec!(150))))
                .unwrap(),
            PhoenixV1Order::Limit {
                side: Side::Sell,
                price: dec!(150),
                quantity: dec!(2)
            }
        );
        assert_eq!(
            PhoenixV1Order::from_order(&order(OrderType::Limit, TimeInForce::Fok, Some(dec!(150))))
                .unwrap(),
            PhoenixV1Order::ImmediateOrCancel {
                side: Side::Sell,
                quantity: dec!(2),
                worst_price: Some(dec!(150)),
                all_or_none: true
            }
        );
        assert_eq!(
            PhoenixV1Order::from_order(&order(OrderType::Market, TimeInForce::Ioc, None)).unwrap(),
            PhoenixV1Order::ImmediateOrCancel {
                side: Side::Sell,
                quantity: dec!(2),
                worst_price: None,
                all_or_none: false
            }
        );
        assert!(
            PhoenixV1Order::from_order(&order(OrderType::Limit, TimeInForce::Ioc, None)).is_err()
        );
    }

    #[test]
    fn errors_map_to_execution_vocabulary() {
        let e: ExecutionError = PhoenixV1Error::InvalidOrder("zero lots".into()).into();
        assert!(matches!(e, ExecutionError::Rejected { .. }));
        let e: ExecutionError = PhoenixV1Error::Chain(ChainError::Rpc("timeout".into())).into();
        assert!(matches!(e, ExecutionError::Connection(_)));
    }
}
