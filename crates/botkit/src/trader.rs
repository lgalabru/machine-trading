use std::str::FromStr;

use luce::{
    Decimal,
    core::{Rounding, Side as LuceSide},
    protocols::phoenix::{PhoenixTrader, phoenix_rise::math::Side},
};
use solana_keypair::Keypair;
use solana_pubkey::Pubkey;
use solana_rpc_client::rpc_client::RpcClient;
use solana_signer::Signer;
use solana_transaction::Signature;

use crate::{BotEnv, Error, Phoenix, Result, rpc_client};

/// A bot's handle on its Phoenix account in one market.
///
/// The `f64` methods are the original botkit interface. Underneath, orders
/// go through [`luce`]: validated against the market's tick and lot grid,
/// planned, simulated, and only then signed and sent. For exact `Decimal`
/// prices and quantities, core types, and an
/// [`ExecutionDriver`](luce::core::ExecutionDriver), use [`Trader::luce`] (or
/// [`Trader::driver`]).
pub struct Trader {
    pub env: BotEnv,
    pub keypair: Keypair,
    /// The cross-margin trader account (PDA) holding the collateral.
    pub account: Pubkey,
    pub phoenix: Phoenix,
    pub rpc: RpcClient,
    /// The luce trader for this account and market.
    pub luce: PhoenixTrader,
}

/// A position in base units (positive is long) and its average entry in USD.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Position {
    pub size: f64,
    pub entry_usd: f64,
}

fn luce_side(side: Side) -> LuceSide {
    match side {
        Side::Bid => LuceSide::Buy,
        Side::Ask => LuceSide::Sell,
    }
}

/// An `f64` price as the decimal it prints as: the conversion Phoenix's own
/// order tickets use.
fn decimal(value: f64, what: &str) -> Result<Decimal> {
    Decimal::from_str(&format!("{value}"))
        .or_else(|_| Decimal::from_scientific(&format!("{value:e}")))
        .map_err(|_| Error::InvalidOrder(format!("{what} {value} is not a finite decimal")))
}

impl Trader {
    pub fn connect(env: &BotEnv) -> Result<Self> {
        let keypair = env.keypair()?;
        let phoenix = Phoenix::new(env.exchange()?, &env.rpc_url);
        let luce = PhoenixTrader::new(
            phoenix.exchange().clone(),
            &env.symbol,
            keypair.pubkey(),
            &env.rpc_url,
        )?;
        Ok(Self {
            account: phoenix.trader_account(&keypair.pubkey()),
            rpc: rpc_client(&env.rpc_url),
            env: env.clone(),
            keypair,
            phoenix,
            luce,
        })
    }

    pub fn authority(&self) -> Pubkey {
        self.keypair.pubkey()
    }

    pub fn slot(&self) -> Result<u64> {
        Ok(self.rpc.get_slot()?)
    }

    pub fn lots(&self, base_units: f64) -> Result<u64> {
        let market = self.phoenix.market(&self.env.symbol)?;
        Ok(self.phoenix.base_units_to_lots(market, base_units))
    }

    pub async fn mark_usd(&self) -> Result<f64> {
        let market = self.phoenix.market(&self.env.symbol)?;
        let bbo = self.luce.bbo().await?;
        Ok(self.phoenix.ticks_to_usd(market, bbo.mark_price_ticks))
    }

    pub async fn position(&self) -> Result<Position> {
        let market = self.phoenix.market(&self.env.symbol)?;
        let asset = self.luce.margin().await?;
        Ok(Position {
            size: self.phoenix.lots_to_base_units(market, asset.base_lots),
            entry_usd: self
                .phoenix
                .entry_price_to_usd(market, asset.entry_price_quote_lots_per_base_lot),
        })
    }

    /// `base_units` rounded to whole lots, as an exact quantity.
    fn quantity(&self, base_units: f64) -> Result<Decimal> {
        Ok(self
            .luce
            .market()
            .instrument
            .lots_to_quantity(self.lots(base_units)?))
    }

    /// `price_usd` moved onto the tick grid the way Phoenix's order tickets
    /// move it: bids down, asks up.
    fn price(&self, side: Side, price_usd: f64) -> Result<Decimal> {
        let rounding = match side {
            Side::Bid => Rounding::Down,
            Side::Ask => Rounding::Up,
        };
        Ok(self
            .luce
            .market()
            .instrument
            .round_price(decimal(price_usd, "price")?, rounding))
    }

    /// Rest an order on the book at `price_usd`, or fill against it if the
    /// price crosses.
    pub async fn place_limit(
        &self,
        side: Side,
        price_usd: f64,
        base_units: f64,
    ) -> Result<Signature> {
        Ok(self
            .luce
            .place_limit(
                &self.keypair,
                luce_side(side),
                self.price(side, price_usd)?,
                self.quantity(base_units)?,
            )
            .await?)
    }

    /// Fill against the book, at no worse than `limit_usd`.
    pub async fn place_market(
        &self,
        side: Side,
        base_units: f64,
        limit_usd: f64,
    ) -> Result<Signature> {
        Ok(self
            .luce
            .place_market(
                &self.keypair,
                luce_side(side),
                self.quantity(base_units)?,
                Some(self.price(side, limit_usd)?),
            )
            .await?)
    }

    pub fn cancel_all(&self) -> Result<Signature> {
        let ixs = self.phoenix.tx().build_cancel_all_orders(
            self.authority(),
            self.account,
            &self.env.symbol,
        )?;
        crate::send(&self.rpc, &self.keypair, ixs, "cancel all")
    }

    /// This trader as a luce [`ExecutionDriver`](luce::core::ExecutionDriver),
    /// signing with a copy of the bot's keypair.
    pub fn driver(&self) -> Result<luce::protocols::phoenix::PhoenixDriver<Keypair>> {
        let trader = PhoenixTrader::new(
            self.phoenix.exchange().clone(),
            &self.env.symbol,
            self.authority(),
            &self.env.rpc_url,
        )?;
        Ok(luce::protocols::phoenix::PhoenixDriver::new(
            trader,
            self.keypair.insecure_clone(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn float_prices_convert_as_phoenix_parses_them() {
        assert_eq!(decimal(150.12, "price").unwrap().to_string(), "150.12");
        assert_eq!(
            decimal(0.1 + 0.2, "price").unwrap().to_string(),
            "0.30000000000000004"
        );
        assert_eq!(decimal(1e-7, "price").unwrap().to_string(), "0.0000001");
        assert!(decimal(f64::NAN, "price").is_err());
        assert!(decimal(f64::INFINITY, "price").is_err());
    }
}
