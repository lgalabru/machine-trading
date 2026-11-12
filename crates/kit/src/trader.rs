use std::str::FromStr;

use solana_keypair::Keypair;
use solana_pubkey::Pubkey;
use solana_rpc_client::rpc_client::RpcClient;
use solana_signer::Signer;
use solana_transaction::Signature;

use crate::{
    Action, BotEnv, Decimal, Error, Phoenix, Planned, Position, Result, SendState, Side,
    core::{Rounding, Side as CoreSide},
    protocols::phoenix::eternal::{PhoenixOrder, PhoenixOrderOptions, PhoenixTrader},
    rpc_client,
};

/// The top of a market, in USD: [`Trader::top`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Top {
    pub mark: f64,
    pub index: f64,
    pub best_bid: Option<f64>,
    pub best_ask: Option<f64>,
}

/// A bot's handle on its Phoenix account in one market.
///
/// The `f64` methods are the original bot interface. Underneath, orders
/// go through [`PhoenixTrader`]: validated against the market's tick and lot grid,
/// planned, simulated, and only then signed and sent. For exact `Decimal`
/// prices and quantities, core types, and an
/// [`ExecutionDriver`](crate::core::ExecutionDriver), use [`Trader::inner`] (or
/// [`Trader::driver`]).
pub struct Trader {
    pub env: BotEnv,
    pub keypair: Keypair,
    /// The cross-margin trader account (PDA) holding the collateral.
    pub account: Pubkey,
    pub phoenix: Phoenix,
    pub rpc: RpcClient,
    /// The exact-decimal Phoenix trader underneath, for this account and
    /// market.
    pub inner: PhoenixTrader,
}

fn core_side(side: Side) -> CoreSide {
    match side {
        Side::Bid => CoreSide::Buy,
        Side::Ask => CoreSide::Sell,
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
        let inner = PhoenixTrader::new(
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
            inner,
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
        Ok(self.top().await?.mark)
    }

    /// The mark, the index and the best bid and ask, in USD, from one read
    /// of Phoenix's view of the market.
    pub async fn top(&self) -> Result<Top> {
        let market = self.phoenix.market(&self.env.symbol)?;
        let bbo = self.inner.bbo().await?;
        let usd = |ticks| self.phoenix.ticks_to_usd(market, ticks);
        Ok(Top {
            mark: usd(bbo.mark_price_ticks),
            index: usd(bbo.index_price_ticks),
            best_bid: bbo.best_bid_ticks().map(usd),
            best_ask: bbo.best_ask_ticks().map(usd),
        })
    }

    /// The market's oracle price, in USD: the median of the latest prices
    /// its oracles reported to Phoenix. Phoenix's mark blends the oracles
    /// with the order book, so quotes, including the bot's own, can push
    /// the mark when the book is thin. They can't push this.
    pub async fn oracle_usd(&self) -> Result<f64> {
        use crate::protocols::phoenix::eternal::phoenix_rise::accounts::perp_asset_map::PerpAssetMap;
        let address = self.phoenix.perp_asset_map()?;
        let account = self.rpc.get_account(&address)?;
        let invalid = |reason: String| Error::Account {
            account: address.to_string(),
            reason,
        };
        let map = PerpAssetMap::try_from_account_bytes(&account.data)
            .map_err(|e| invalid(e.to_string()))?;
        let entry = map
            .find_by_symbol(&self.env.symbol)
            .map_err(|e| invalid(e.to_string()))?
            .ok_or_else(|| Error::UnknownMarket(self.env.symbol.clone()))?;
        let price = &entry.metadata.oracle_price().mark_price;
        let reported = |ticks: u64| ticks > 0 && ticks != u64::MAX;
        let mut ticks: Vec<u64> = price
            .spot_price_component
            .last_exchange_spot_price
            .iter()
            .map(|p| p.ticks.as_inner())
            .filter(|&t| reported(t))
            .collect();
        if ticks.is_empty() {
            ticks = price
                .perp_price_component
                .last_exchange_perp_price
                .iter()
                .map(|p| p.ticks.as_inner())
                .filter(|&t| reported(t))
                .collect();
        }
        if ticks.is_empty() {
            return Err(invalid(format!(
                "no oracle reports for {}",
                self.env.symbol
            )));
        }
        ticks.sort_unstable();
        let median = ticks[ticks.len() / 2];
        let market = self.phoenix.market(&self.env.symbol)?;
        Ok(self.phoenix.ticks_to_usd(market, median))
    }

    pub async fn position(&self) -> Result<Position> {
        let market = self.phoenix.market(&self.env.symbol)?;
        let asset = self.inner.margin().await?;
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
            .inner
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
            .inner
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
            .inner
            .place_limit(
                &self.keypair,
                core_side(side),
                self.price(side, price_usd)?,
                self.quantity(base_units)?,
            )
            .await?)
    }

    /// Rest an order on the book at `price_usd` that takes nothing: not
    /// placed if the price crosses.
    pub async fn place_post_only(
        &self,
        side: Side,
        price_usd: f64,
        base_units: f64,
    ) -> Result<Signature> {
        Ok(self
            .inner
            .place_post_only(
                &self.keypair,
                core_side(side),
                self.price(side, price_usd)?,
                self.quantity(base_units)?,
                PhoenixOrderOptions::default(),
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
            .inner
            .place_market(
                &self.keypair,
                core_side(side),
                self.quantity(base_units)?,
                Some(self.price(side, limit_usd)?),
            )
            .await?)
    }

    /// Whether [`Trader::send_planned`] executes `action`: market, limit
    /// and post-only orders, and cancelling everything. A session plans its
    /// sheets with this (`mtx_kit::plan_sheet_with`), so a line it won't
    /// execute counts for nothing.
    pub fn executes(action: &Action) -> bool {
        matches!(
            action,
            Action::Market { .. }
                | Action::Limit { .. }
                | Action::PostOnly { .. }
                | Action::CancelAll
        )
    }

    /// Send one order a sheet planned for this market (`mtx_kit::plan_sheet`),
    /// and say what became of it. Its line's reduce-only flag and expiry go
    /// on the order. A post-only order that would cross the book as it is
    /// now (the world is paused) isn't placed, and fails without reaching
    /// the chain. Actions this trader doesn't execute aren't sent.
    pub async fn send_planned(&self, planned: &Planned) -> Sent {
        let options = PhoenixOrderOptions {
            reduce_only: planned.reduce_only,
            last_valid_slot: planned.expires_slot,
        };
        let order = match planned.action {
            Action::Market { side, size, limit } => self.order(size, |quantity| {
                Ok(PhoenixOrder::Market {
                    side: core_side(side),
                    quantity,
                    worst_price: Some(self.price(side, limit)?),
                    all_or_none: false,
                })
            }),
            Action::Limit { side, price, size } => self.order(size, |quantity| {
                Ok(PhoenixOrder::Limit {
                    side: core_side(side),
                    price: self.price(side, price)?,
                    quantity,
                })
            }),
            Action::PostOnly { side, price, size } => {
                if planned.expires_slot.is_some() {
                    return Sent::not_sent(
                        "an expiry on a post-only order is unsupported in this session",
                    );
                }
                let crosses = |top: Top| {
                    let price = self.price(side, price)?.to_string();
                    let price: f64 = price.parse().unwrap_or(f64::NAN);
                    Ok::<_, Error>(match side {
                        Side::Bid => top.best_ask.is_some_and(|ask| price >= ask),
                        Side::Ask => top.best_bid.is_some_and(|bid| price <= bid),
                    })
                };
                match self.top().await.and_then(crosses) {
                    Ok(false) => self.order(size, |quantity| {
                        Ok(PhoenixOrder::PostOnly {
                            side: core_side(side),
                            price: self.price(side, price)?,
                            quantity,
                        })
                    }),
                    Ok(true) => return Sent::checked("post-only would cross: not placed"),
                    Err(error) => {
                        return Sent::checked(&format!(
                            "post-only: couldn't read the book to check it: {error}"
                        ));
                    }
                }
            }
            Action::CancelAll => {
                return Sent::reached(self.cancel_all());
            }
            Action::Respond { .. } => {
                return Sent::not_sent("no requests for quotes in this session");
            }
            _ => return Sent::not_sent("unsupported in this session"),
        };
        let result = match order {
            Ok(order) => self
                .inner
                .submit_with(&order, options, &self.keypair)
                .await
                .map_err(Error::from),
            Err(error) => Err(error),
        };
        Sent::reached(result)
    }

    /// A Phoenix order of `size` base units, built by `order` from its
    /// quantity on the lot grid.
    fn order(
        &self,
        size: f64,
        order: impl FnOnce(Decimal) -> Result<PhoenixOrder>,
    ) -> Result<PhoenixOrder> {
        order(self.quantity(size)?)
    }

    pub fn cancel_all(&self) -> Result<Signature> {
        let ixs = self.phoenix.tx().build_cancel_all_orders(
            self.authority(),
            self.account,
            &self.env.symbol,
        )?;
        crate::send(&self.rpc, &self.keypair, ixs, "cancel all")
    }

    /// This trader as an [`ExecutionDriver`](crate::core::ExecutionDriver),
    /// signing with a copy of the bot's keypair.
    pub fn driver(&self) -> Result<crate::protocols::phoenix::eternal::PhoenixDriver<Keypair>> {
        let trader = PhoenixTrader::new(
            self.phoenix.exchange().clone(),
            &self.env.symbol,
            self.authority(),
            &self.env.rpc_url,
        )?;
        Ok(crate::protocols::phoenix::eternal::PhoenixDriver::new(
            trader,
            self.keypair.insecure_clone(),
        ))
    }
}

/// The order sheet's side, for Phoenix's own builders (`.side(side.into())`).
impl From<Side> for crate::phoenix_rise::math::Side {
    fn from(side: Side) -> Self {
        match side {
            Side::Bid => Self::Bid,
            Side::Ask => Self::Ask,
        }
    }
}

impl From<crate::phoenix_rise::math::Side> for Side {
    fn from(side: crate::phoenix_rise::math::Side) -> Self {
        match side {
            crate::phoenix_rise::math::Side::Bid => Self::Bid,
            crate::phoenix_rise::math::Side::Ask => Self::Ask,
        }
    }
}

/// What became of an order [`Trader::send_planned`] was given.
#[derive(Debug, Clone, PartialEq)]
pub struct Sent {
    pub state: SendState,
    /// Whether it reached the chain: a resting order that did counts toward
    /// what rests, even if it failed (the ledger only over-counts), and a
    /// failure stops a component's step.
    pub sent: bool,
}

impl Sent {
    /// What became of an order that was sent.
    fn reached(result: Result<Signature>) -> Self {
        Sent {
            state: match result {
                Ok(signature) => SendState::Landed(signature.to_string()),
                Err(error) => SendState::failed(error.to_string()),
            },
            sent: true,
        }
    }

    fn checked(why: &str) -> Self {
        Sent {
            state: SendState::failed(why),
            sent: false,
        }
    }

    fn not_sent(why: &str) -> Self {
        Sent {
            state: SendState::NotSent(why.to_string()),
            sent: false,
        }
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
