//! [`ExecutionDriver`] over OpenBook v2.

use super::{OpenBookV2Error, OpenBookV2Order, OpenBookV2Trader};
use crate::{
    chain::{ChainError, TransactionSigner},
    core::{
        ExecutionDriver, ExecutionError, ExecutionReport, InstrumentId, Order, OrderBook, OrderId,
        OrderStatus, OrderType, TimeInForce,
    },
};

/// An [`OpenBookV2Trader`] and the signer it sends with, as an
/// [`ExecutionDriver`] for its one market.
///
/// | Order | OpenBook v2 |
/// |-------|-------------|
/// | limit, GTC | limit order on the trader's open orders account |
/// | limit or market, IOC | take order bounded by the price |
/// | limit or market, FOK | take order bounded by the price, all or none |
///
/// OpenBook needs a price on every order, so a market order without one is
/// rejected. Fills against a take order settle in the transaction; fills
/// against a resting order reach the open orders account when the event
/// heap is consumed. Either way a sent order is reported
/// [`OrderStatus::Pending`] with its signature. Orders are cancelled
/// together ([`cancel_all`](ExecutionDriver::cancel_all)).
pub struct OpenBookV2Driver<S> {
    trader: OpenBookV2Trader,
    signer: S,
}

impl<S: TransactionSigner> OpenBookV2Driver<S> {
    /// Drives `trader`'s market, signing with `signer`.
    pub fn new(trader: OpenBookV2Trader, signer: S) -> Self {
        Self { trader, signer }
    }

    /// The trader underneath.
    pub fn trader(&self) -> &OpenBookV2Trader {
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

impl OpenBookV2Order {
    /// How a core [`Order`] maps onto OpenBook v2 (see [`OpenBookV2Driver`]).
    pub fn from_order(order: &Order) -> Result<Self, OpenBookV2Error> {
        let price = order.price.ok_or_else(|| {
            OpenBookV2Error::InvalidOrder("OpenBook v2 orders need a price".into())
        })?;
        Ok(match (order.order_type, order.time_in_force) {
            (OrderType::Limit, TimeInForce::Gtc) => Self::Limit {
                side: order.side,
                price,
                quantity: order.quantity,
            },
            (_, tif) => Self::Take {
                side: order.side,
                quantity: order.quantity,
                worst_price: price,
                all_or_none: tif == TimeInForce::Fok,
            },
        })
    }
}

impl From<OpenBookV2Error> for ExecutionError {
    fn from(error: OpenBookV2Error) -> Self {
        match error {
            OpenBookV2Error::Instrument(_) | OpenBookV2Error::InvalidOrder(_) => Self::Rejected {
                reason: error.to_string(),
            },
            OpenBookV2Error::Chain(chain @ ChainError::Simulation { .. }) => Self::Rejected {
                reason: chain.to_string(),
            },
            OpenBookV2Error::Chain(ChainError::Rpc(detail)) => Self::Connection(detail),
            other => Self::Venue(other.to_string()),
        }
    }
}

#[async_trait::async_trait]
impl<S: TransactionSigner> ExecutionDriver for OpenBookV2Driver<S> {
    async fn submit_order(&self, order: &Order) -> Result<ExecutionReport, ExecutionError> {
        self.check_instrument(&order.instrument)?;
        let signature = self
            .trader
            .submit(&OpenBookV2Order::from_order(order)?, &self.signer)
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
            "OpenBook v2 orders are addressed by their order ids on chain; use cancel_all".into(),
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
        Ok(self.trader.order_book(0).await?)
    }
}

#[cfg(test)]
mod tests {
    use rust_decimal_macros::dec;

    use super::*;
    use crate::core::{Side, VenueId};

    #[test]
    fn core_orders_map_onto_openbook() {
        let mut o = Order::limit(
            InstrumentId::new("openbook:spot:SOL-USDC"),
            VenueId::new("openbook"),
            Side::Sell,
            dec!(1),
            dec!(150),
        );
        assert!(matches!(
            OpenBookV2Order::from_order(&o).unwrap(),
            OpenBookV2Order::Limit { .. }
        ));
        o.time_in_force = TimeInForce::Fok;
        assert_eq!(
            OpenBookV2Order::from_order(&o).unwrap(),
            OpenBookV2Order::Take {
                side: Side::Sell,
                quantity: dec!(1),
                worst_price: dec!(150),
                all_or_none: true
            }
        );
        o.price = None;
        assert!(OpenBookV2Order::from_order(&o).is_err());
    }
}
