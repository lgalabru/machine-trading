//! [`PhoenixEternalMeter`]: what an enforcer measures of a cross-margin
//! trader account on Phoenix Eternal, as the Phoenix venue profile
//! (Perpetual markets) defines it.
//!
//! Everything is read from Phoenix through Hawkeye's views, and the wallet
//! from the chain, through the meter's own clients, never the bot's.

use std::sync::Arc;

use phoenix_rise::{core::PhoenixHawkeyeClient, ix::hawkeye::ViewAssetReturn};
use rust_decimal::Decimal;
use solana_pubkey::Pubkey;

use super::{PhoenixError, PhoenixExchange, PhoenixMarket};
use crate::{
    chain::{ChainClient, token},
    core::{AccountMeter, Holding, InstrumentId, Measurement, MeterError, RecordedFill},
    protocols::USDC_MINT,
};

const USDC_DECIMALS: u32 = 6;

/// One owner's cross-margin trader account, measured on one perp market.
pub struct PhoenixEternalMeter {
    exchange: Arc<PhoenixExchange>,
    rpc_url: String,
    chain: Arc<dyn ChainClient>,
    owner: Pubkey,
    trader: Pubkey,
    market: PhoenixMarket,
    unsettled_funding: bool,
}

impl PhoenixEternalMeter {
    /// Measures `owner`'s trader account on `symbol`: Hawkeye's views at
    /// `rpc_url`, the wallet through `chain`.
    pub fn new(
        exchange: Arc<PhoenixExchange>,
        rpc_url: impl Into<String>,
        chain: Arc<dyn ChainClient>,
        owner: Pubkey,
        symbol: &str,
    ) -> Result<Self, PhoenixError> {
        Ok(Self {
            market: exchange.market(symbol)?,
            trader: exchange.trader_account(&owner),
            exchange,
            rpc_url: rpc_url.into(),
            chain,
            owner,
            unsettled_funding: false,
        })
    }

    /// Whether the account's value counts the funding it has accrued and not
    /// settled yet. Phoenix settles funding into the collateral only when the
    /// account next trades, so a position held through funding intervals is
    /// worth its collateral, its unrealized PnL and that funding. Off by
    /// default: the account is valued as the venue profile defines it.
    pub fn with_unsettled_funding(mut self, unsettled_funding: bool) -> Self {
        self.unsettled_funding = unsettled_funding;
        self
    }

    /// The trader account it measures: the owner's cross-margin account.
    pub fn trader(&self) -> Pubkey {
        self.trader
    }

    /// The instrument this meter measures, as the mandate names it.
    pub fn instrument(&self) -> &InstrumentId {
        &self.market.instrument.id
    }

    fn hawkeye(&self) -> PhoenixHawkeyeClient<'_> {
        self.exchange.hawkeye(&self.rpc_url, self.owner)
    }

    async fn asset(&self) -> Result<ViewAssetReturn, MeterError> {
        Ok(self
            .hawkeye()
            .view_margin_for_asset(self.owner, 0, 0, self.market.asset_id)
            .await
            .map_err(read)?
            .value)
    }

    /// The trader account from one margin view, with the wallet's
    /// collateral: what Phoenix holds (collateral plus unrealized PnL at
    /// the mark, plus, when the meter counts it, funding accrued and not
    /// settled yet) and USDC withdrawn back to the wallet, which keeps its
    /// value so a withdrawal can't hide a drawdown.
    async fn account(&self) -> Result<Account, MeterError> {
        let margin = self
            .hawkeye()
            .view_margin_for_trader(self.trader)
            .await
            .map_err(read)?
            .value;
        let mut quote_lots = margin.collateral_quote_lots + margin.unrealized_pnl_quote_lots;
        if self.unsettled_funding {
            quote_lots += margin.unsettled_funding_quote_lots;
        }
        // Real USDC, which deposits take from: the canonical mint
        // (`collateral_mint`) lives only inside Phoenix.
        let wallet = token::owned_amount(self.chain.as_ref(), &self.owner, &USDC_MINT)
            .await
            .map_err(read)?;
        Ok(Account {
            value_usd: Decimal::new(quote_lots, u32::from(self.market.quote_lot_decimals))
                + Decimal::from_i128_with_scale(i128::from(wallet), USDC_DECIMALS),
            position_count: margin.position_count,
        })
    }

    /// Markets held other than the universe's, from one pair of reads.
    fn outside(
        &self,
        account: &Account,
        asset: &ViewAssetReturn,
        universe: &[InstrumentId],
    ) -> u32 {
        let held = asset.has_position_or_orders != 0 && universe.contains(self.instrument());
        u32::from(account.position_count.saturating_sub(u16::from(held)))
    }
}

#[async_trait::async_trait]
impl AccountMeter for PhoenixEternalMeter {
    /// The market's position, read first, then the account. A count of
    /// markets held outside the universe is only kept if a second pair of
    /// reads agrees: the market's view and the account's are two reads, and
    /// an order the bot places or cancels between them shows up in one and
    /// not the other, which looks like a position elsewhere. A position that
    /// really is elsewhere shows up on every read.
    async fn measure(&self, universe: &[InstrumentId]) -> Result<Measurement, MeterError> {
        let asset = self.asset().await?;
        let account = self.account().await?;
        let outside_universe = match self.outside(&account, &asset, universe) {
            0 => 0,
            seen => {
                let again = self.asset().await?;
                seen.min(self.outside(&self.account().await?, &again, universe))
            }
        };
        let position = self
            .market
            .position(asset.base_lots, asset.entry_price_quote_lots_per_base_lot);
        Ok(Measurement {
            holdings: vec![(self.instrument().clone(), Holding::Perp(position))],
            value_usd: account.value_usd,
            outside_universe,
        })
    }

    async fn fills(&self, _after_slot: u64) -> Result<Vec<RecordedFill>, MeterError> {
        Err(MeterError::Unsupported(
            "Phoenix Eternal fills: no task's mandate has a fill rule yet".into(),
        ))
    }

    async fn liquidatable(&self) -> Result<bool, MeterError> {
        let margin = self
            .hawkeye()
            .view_margin(self.owner, 0, 0)
            .await
            .map_err(read)?
            .value;
        Ok(margin.is_liquidatable != 0)
    }
}

struct Account {
    value_usd: Decimal,
    /// Markets the trader holds a position or orders in.
    position_count: u16,
}

fn read(error: impl std::fmt::Display) -> MeterError {
    MeterError::Read(error.to_string())
}
