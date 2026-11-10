//! [`ExecutionDriver`] over Phoenix.

use super::{PhoenixError, PhoenixOrder, PhoenixTrader};
use crate::{
    chain::{ChainError, TransactionSigner},
    core::{
        ExecutionDriver, ExecutionError, ExecutionReport, InstrumentId, Order, OrderBook, OrderId,
        OrderStatus,
    },
};

/// A [`PhoenixTrader`] and the signer it sends with, as an
/// [`ExecutionDriver`] for its one market.
///
/// # What the reports say
///
/// Phoenix settles fills inside the transaction and reports them as program
/// events, which this driver does not decode. A confirmed submission is
/// therefore reported as [`OrderStatus::Pending`] with no fills and the
/// transaction signature as its
/// [`venue_reference`](ExecutionReport::venue_reference). Read
/// [`PhoenixTrader::position`] to see what filled.
///
/// Individual orders cannot be cancelled by this crate's [`OrderId`] (Phoenix
/// addresses resting orders by its own sequence numbers);
/// [`cancel_all`](ExecutionDriver::cancel_all) is supported.
pub struct PhoenixDriver<S> {
    trader: PhoenixTrader,
    signer: S,
}

impl<S: TransactionSigner> PhoenixDriver<S> {
    /// Drives `trader`'s market, signing with `signer`.
    pub fn new(trader: PhoenixTrader, signer: S) -> Self {
        Self { trader, signer }
    }

    /// The trader underneath.
    pub fn trader(&self) -> &PhoenixTrader {
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

/// Maps adapter errors onto the execution vocabulary: what the order got
/// wrong is a rejection, what the network got wrong a connection error.
impl From<PhoenixError> for ExecutionError {
    fn from(error: PhoenixError) -> Self {
        match error {
            PhoenixError::Instrument(_) | PhoenixError::InvalidOrder(_) | PhoenixError::Ix(_) => {
                Self::Rejected {
                    reason: error.to_string(),
                }
            }
            PhoenixError::Chain(chain @ ChainError::Simulation { .. }) => {
                let reason = match chain.custom_error_code() {
                    Some((ix, code)) => {
                        format!("{chain}\n(instruction {ix} failed with Phoenix error {code})")
                    }
                    None => chain.to_string(),
                };
                Self::Rejected { reason }
            }
            PhoenixError::Chain(ChainError::Rpc(detail)) => Self::Connection(detail),
            other => Self::Venue(other.to_string()),
        }
    }
}

#[async_trait::async_trait]
impl<S: TransactionSigner> ExecutionDriver for PhoenixDriver<S> {
    async fn submit_order(&self, order: &Order) -> Result<ExecutionReport, ExecutionError> {
        self.check_instrument(&order.instrument)?;
        let phoenix_order = PhoenixOrder::from_order(order)?;
        let signature = self.trader.submit(&phoenix_order, &self.signer).await?;
        Ok(ExecutionReport {
            order_id: order.id.clone(),
            status: OrderStatus::Pending,
            fills: Vec::new(),
            venue_reference: Some(signature.to_string()),
        })
    }

    async fn cancel_order(&self, _order_id: &OrderId) -> Result<OrderStatus, ExecutionError> {
        Err(ExecutionError::Unsupported(
            "Phoenix orders are addressed by Phoenix sequence numbers; use cancel_all".into(),
        ))
    }

    async fn cancel_all(&self, instrument: &InstrumentId) -> Result<(), ExecutionError> {
        self.check_instrument(instrument)?;
        self.trader.cancel_all(&self.signer).await?;
        Ok(())
    }

    async fn order_book(&self, instrument: &InstrumentId) -> Result<OrderBook, ExecutionError> {
        self.check_instrument(instrument)?;
        Ok(self.trader.order_book().await?)
    }
}

#[cfg(test)]
mod tests {
    use solana_transaction::{InstructionError, TransactionError};

    use super::*;

    #[test]
    fn errors_map_to_execution_vocabulary() {
        let rejected: ExecutionError = PhoenixError::InvalidOrder("zero lots".into()).into();
        assert!(matches!(rejected, ExecutionError::Rejected { .. }));

        let simulated: ExecutionError = PhoenixError::Chain(ChainError::Simulation {
            label: "limit order".into(),
            error: TransactionError::InstructionError(1, InstructionError::Custom(42)),
            logs: vec![],
        })
        .into();
        match simulated {
            ExecutionError::Rejected { reason } => assert!(reason.contains("Phoenix error 42")),
            other => panic!("unexpected {other}"),
        }

        let network: ExecutionError = PhoenixError::Chain(ChainError::Rpc("timeout".into())).into();
        assert!(matches!(network, ExecutionError::Connection(d) if d == "timeout"));

        let other: ExecutionError = PhoenixError::UnknownMarket("X".into()).into();
        assert!(matches!(other, ExecutionError::Venue(_)));
    }
}
